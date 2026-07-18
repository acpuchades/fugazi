//! CLI builder for the trailing risk indicators (`!sharpe` / `!sortino` /
//! `!volatility` / `!max_drawdown` / `!calmar`).
//!
//! The library indicators ([`fugazi::indicators::Sharpe`] and friends) each own
//! a [`Strategy`](fugazi::Strategy) and drive it internally. Since the embedded
//! engine forwards the whole snapshot to its strategy, that strategy can be a
//! single-asset, **pairs**, or **basket** one — the [`AnyStrategyRef`] the
//! `strategy:` field deserializes to picks which.
//!
//! The runtime type-erasure layer ([`DynIndicator`]) requires the wrapped
//! indicator to be [`Clone`], but a built strategy
//! ([`DynSingleStrategy`](super::strategy::DynSingleStrategy) and its pairs /
//! basket twins) is **not** `Clone` (it holds `Box<dyn Signal>` slots and
//! `Rc`-shared `Position`/`Book` state). So this module wraps the trailing
//! indicator in a [`RebuildIndicator`] that carries the strategy *spec* plus a
//! rebuild closure and mints a **fresh** indicator instance on every clone —
//! matching the "clone = an independently-advanced instance" convention the
//! component accessors already use.
//!
//! The wallet seed is a fixed [`SEED`]: every metric here is a ratio of
//! equity-curve returns, and the returns are scale-invariant in the seed, so
//! exposing it as a knob would add surface with no effect on the reading. The
//! embedded strategy's [`Book`](fugazi::indicators::Book) is seeded to the same
//! value so its book-anchored sizing recipes stay meaningful.

use std::sync::Arc;

use serde::Deserialize;

use fugazi::indicators::{Calmar, MaxDrawdown, Sharpe, Sortino, Volatility};
use fugazi::prelude::*;
use fugazi::types::{Real, Snapshot};

use super::basket::BasketStrategySpec;
use super::multi_asset::MultiAssetStrategySpec;
use super::pairs::PairsStrategySpec;
use super::preset::StrategyRef;
use crate::dyn_indicator::{self, DynIndicator};

/// The wallet / book seed for every embedded strategy. Arbitrary and positive
/// — the ratio metrics are scale-invariant in it (see the module docs).
const SEED: Real = 1_000.0;

/// Which trailing metric a [`build`] call constructs.
#[derive(Debug, Clone, Copy)]
pub(super) enum TrailingMetric {
    Sharpe,
    Sortino,
    Volatility,
    MaxDrawdown,
    Calmar,
}

/// A strategy reference the trailing risk tags accept — widened beyond the
/// single-asset [`StrategyRef`] to also name a **pairs** or **basket** strategy.
///
/// The embedded engine forwards the whole snapshot to its strategy, so any
/// [`Strategy`](fugazi::Strategy) over a `Snapshot<String>` drives it:
/// `!sharpe { strategy: <single | pairs | basket> }` reads the trailing risk of
/// whichever one. (A pairs / basket strategy only produces meaningful numbers
/// when the surrounding run feeds it a tagged multi-asset snapshot each bar —
/// inside a pairs / basket run or a multi-symbol `--series` frame — since a
/// single-asset run feeds one leg per bar.)
///
/// Deserialized through the same [`serde_norway::Value`] bridge as
/// [`StrategyRef`], routing by a distinctive top-level key: `left` + `right` →
/// pairs, `selection` → basket, otherwise a single-asset spec map or a preset
/// tag (delegated to [`StrategyRef`]).
#[derive(Debug, Clone, Deserialize)]
#[serde(try_from = "serde_norway::Value")]
pub enum AnyStrategyRef {
    Single(StrategyRef),
    Pairs(Box<PairsStrategySpec>),
    Basket(Box<BasketStrategySpec>),
    Multi(Box<MultiAssetStrategySpec>),
}

impl AnyStrategyRef {
    /// The tag applied to *untagged* snapshot entries the embedded engine prices
    /// (see the [engine docs](fugazi::indicators::Sharpe)). For a single asset
    /// it's the traded symbol; for a pair, the left leg. A basket / multi
    /// names no symbol upfront (its universe floats), so it has none — but
    /// they're only ever fed tagged multi-asset snapshots, where the fallback
    /// is never consulted.
    fn fallback_symbol(&self) -> String {
        match self {
            AnyStrategyRef::Single(s) => s.symbol().to_string(),
            AnyStrategyRef::Pairs(p) => p.left.clone(),
            AnyStrategyRef::Basket(_) | AnyStrategyRef::Multi(_) => String::new(),
        }
    }
}

impl TryFrom<serde_norway::Value> for AnyStrategyRef {
    type Error = String;

    fn try_from(v: serde_norway::Value) -> Result<Self, Self::Error> {
        use serde_norway::Value;

        // Detect pairs / basket / multi by distinctive top-level keys.
        // Multi has no unique key — its shape is "a bare mapping with no
        // symbol, no left+right, no selection" (mirrors how
        // `PortfolioChildStrategy` distinguishes multi from single).
        let (is_pairs, is_basket, has_symbol) = match &v {
            Value::Mapping(m) => {
                let has = |key: &str| {
                    m.iter()
                        .any(|(k, _)| matches!(k, Value::String(s) if s == key))
                };
                (has("left") && has("right"), has("selection"), has("symbol"))
            }
            _ => (false, false, false),
        };

        // Deserialize pairs / basket through the *serde_json* path (normalising
        // `!tag`s to `{tag: value}` maps first): it's the same path their
        // `from_text_with_params_in` loaders use, and it's required for two
        // reasons the serde_norway `Value` path can't satisfy — a basket's
        // `SpecTemplate` score/sizing capture `serde_json::Value`, and its
        // `SelectionRuleSpec` is a bare externally-tagged enum serde_norway
        // reads only from a `Value::Tagged`, not a plain single-key map.
        if is_pairs || is_basket {
            let json = crate::convert::yaml_to_json(v).map_err(|e| e.to_string())?;
            return if is_pairs {
                serde_json::from_value::<PairsStrategySpec>(json)
                    .map(|p| AnyStrategyRef::Pairs(Box::new(p)))
                    .map_err(|e| e.to_string())
            } else {
                serde_json::from_value::<BasketStrategySpec>(json)
                    .map(|b| AnyStrategyRef::Basket(Box::new(b)))
                    .map_err(|e| e.to_string())
            };
        }

        // Multi: bare mapping without symbol / pairs / basket keys.
        if matches!(&v, Value::Mapping(_)) && !has_symbol {
            let json = crate::convert::yaml_to_json(v).map_err(|e| e.to_string())?;
            return serde_json::from_value::<MultiAssetStrategySpec>(json)
                .map(|m| AnyStrategyRef::Multi(Box::new(m)))
                .map_err(|e| e.to_string());
        }

        StrategyRef::try_from(v).map(AnyStrategyRef::Single)
    }
}

/// A boxed real-valued source over the single-asset snapshot stream — the
/// erased form every trailing indicator collapses to.
type BoxedReal = Box<dyn Indicator<Input = Snapshot<String>, Output = Real> + Send + Sync>;

/// A `Clone`-able wrapper around a non-`Clone` trailing indicator: it holds the
/// closure that builds a fresh instance (rebuilding the embedded strategy from
/// its spec) and rebuilds on every clone. See the module docs.
struct RebuildIndicator {
    build: Arc<dyn Fn() -> BoxedReal + Send + Sync>,
    inner: BoxedReal,
}

impl Clone for RebuildIndicator {
    fn clone(&self) -> Self {
        let inner = (self.build)();
        Self {
            build: Arc::clone(&self.build),
            inner,
        }
    }
}

impl Indicator for RebuildIndicator {
    type Input = Snapshot<String>;
    type Output = Real;

    fn update(&mut self, input: Snapshot<String>) -> Option<Real> {
        self.inner.update(input)
    }

    fn value(&self) -> Option<Real> {
        self.inner.value()
    }

    fn warm_up_period(&self) -> usize {
        self.inner.warm_up_period()
    }

    fn unstable_period(&self) -> usize {
        self.inner.unstable_period()
    }

    fn reset(&mut self) {
        self.inner.reset();
    }
}

/// Wrap a freshly-built strategy in the trailing indicator `metric` selects,
/// erased to [`BoxedReal`]. Generic over the strategy type so the single / pairs
/// / basket arms share one body. `fallback_symbol` is the tag the embedded
/// engine applies to untagged snapshot entries.
fn make<S>(
    metric: TrailingMetric,
    strat: S,
    fallback_symbol: String,
    period: usize,
    risk_free_rate: Real,
    bars_per_year: Real,
) -> BoxedReal
where
    S: fugazi::Strategy<Symbol = String, Input = Snapshot<String>> + Send + Sync + 'static,
{
    match metric {
        TrailingMetric::Sharpe => Box::new(Sharpe::new(
            strat,
            fallback_symbol,
            SEED,
            period,
            risk_free_rate,
            bars_per_year,
        )),
        TrailingMetric::Sortino => Box::new(Sortino::new(
            strat,
            fallback_symbol,
            SEED,
            period,
            risk_free_rate,
            bars_per_year,
        )),
        TrailingMetric::Volatility => Box::new(Volatility::new(
            strat,
            fallback_symbol,
            SEED,
            period,
            bars_per_year,
        )),
        TrailingMetric::MaxDrawdown => {
            Box::new(MaxDrawdown::new(strat, fallback_symbol, SEED, period))
        }
        TrailingMetric::Calmar => Box::new(Calmar::new(
            strat,
            fallback_symbol,
            SEED,
            period,
            bars_per_year,
        )),
    }
}

/// Build the runtime-typed trailing indicator `metric` over the strategy
/// `strategy` describes (single, pairs, or basket), reading a rolling
/// `period`-bar window.
///
/// `risk_free_rate` (annualized fraction) is consumed only by
/// [`TrailingMetric::Sharpe`] / [`TrailingMetric::Sortino`]; `bars_per_year`
/// annualizes every metric except [`TrailingMetric::MaxDrawdown`]. `schema` is
/// the overlay schema the embedded strategy's `!get` leaves resolve against.
pub(super) fn build(
    metric: TrailingMetric,
    strategy: &AnyStrategyRef,
    period: usize,
    risk_free_rate: Real,
    bars_per_year: Real,
    schema: &Arc<Schema>,
) -> Box<dyn DynIndicator> {
    let spec = Arc::new(strategy.clone());
    let schema = Arc::clone(schema);
    let fallback = strategy.fallback_symbol();

    let build_fn: Arc<dyn Fn() -> BoxedReal + Send + Sync> = Arc::new(move || {
        let sym = fallback.clone();
        match &*spec {
            AnyStrategyRef::Single(s) => make(
                metric,
                s.build(SEED, &schema),
                sym,
                period,
                risk_free_rate,
                bars_per_year,
            ),
            AnyStrategyRef::Pairs(p) => make(
                metric,
                p.build(SEED, &schema),
                sym,
                period,
                risk_free_rate,
                bars_per_year,
            ),
            AnyStrategyRef::Basket(b) => make(
                metric,
                b.build(SEED, &schema),
                sym,
                period,
                risk_free_rate,
                bars_per_year,
            ),
            AnyStrategyRef::Multi(m) => make(
                metric,
                m.build(SEED, &schema),
                sym,
                period,
                risk_free_rate,
                bars_per_year,
            ),
        }
    });

    let inner = build_fn();
    dyn_indicator::wrap(RebuildIndicator {
        build: build_fn,
        inner,
    })
}
