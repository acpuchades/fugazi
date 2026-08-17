//! CLI builder for the trailing risk indicators (`!sharpe` / `!sortino` /
//! `!volatility` / `!max_drawdown` / `!calmar`).
//!
//! The library indicators ([`crate::indicators::Sharpe`] and friends) each own
//! a [`Strategy`] and drive it internally. Since the embedded
//! engine forwards the whole snapshot to its strategy, that strategy can be a
//! single-asset, **pairs**, or **basket** one — the [`AnyStrategyRef`] the
//! `strategy:` field deserializes to picks which.
//!
//! The runtime type-erasure layer ([`PayloadIndicator`]) requires the wrapped
//! indicator to be [`Clone`], but a built strategy
//! ([`DynSingleStrategy`](super::strategy::DynSingleStrategy) and its pairs /
//! basket twins) is **not** `Clone` (it holds `Box<dyn Signal>` slots and
//! `Rc`-shared `Position`/`Book` state). So this module wraps the trailing
//! indicator in a `RebuildIndicator` that carries the strategy *spec* plus a
//! rebuild closure and mints a **fresh** indicator instance on every clone —
//! matching the "clone = an independently-advanced instance" convention the
//! component accessors already use.
//!
//! The wallet seed is a fixed `SEED`: every metric here is a ratio of
//! equity-curve returns, and the returns are scale-invariant in the seed, so
//! exposing it as a knob would add surface with no effect on the reading. The
//! embedded strategy's [`Book`](crate::indicators::Book) is seeded to the same
//! value so its book-anchored sizing recipes stay meaningful.

use std::sync::Arc;

use serde::Deserialize;

use crate::indicators::{Calmar, MaxDrawdown, Sharpe, Sortino, Volatility};
use crate::prelude::*;
use crate::types::{Real, Snapshot};

use super::basket::BasketStrategySpec;
use super::multi_asset::MultiAssetStrategySpec;
use super::pairs::PairsStrategySpec;
use super::preset::StrategyRef;
use crate::spec::dyn_indicator::{self, PayloadIndicator};
use crate::types::Symbol;

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
/// [`Strategy`] over a `Snapshot<Symbol>` drives it:
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
    /// (see the [engine docs](crate::indicators::Sharpe)). For a single asset
    /// it's the traded symbol; for a pair, the left leg. A basket / multi
    /// names no symbol upfront (its universe floats), so it has none — but
    /// they're only ever fed tagged multi-asset snapshots, where the fallback
    /// is never consulted.
    fn fallback_symbol(&self) -> Symbol {
        match self {
            AnyStrategyRef::Single(s) => crate::types::symbol(s.symbol()),
            AnyStrategyRef::Pairs(p) => crate::types::symbol(&p.left),
            AnyStrategyRef::Basket(_) | AnyStrategyRef::Multi(_) => crate::types::symbol(""),
        }
    }
}

impl TryFrom<serde_norway::Value> for AnyStrategyRef {
    type Error = String;

    fn try_from(v: serde_norway::Value) -> Result<Self, Self::Error> {
        use crate::spec::shape::{ShapeHint, detect_shape};

        // Deserialize pairs / basket / multi through the *serde_json* path
        // (normalising `!tag`s to `{tag: value}` maps first): it's the same
        // path their `from_text_with_params_in` loaders use, and it's required
        // for two reasons the serde_norway `Value` path can't satisfy — a
        // basket's `SpecTemplate` score/sizing capture `serde_json::Value`,
        // and its `SelectionRuleSpec` is a bare externally-tagged enum
        // serde_norway reads only from a `Value::Tagged`, not a single-key map.
        let via_json =
            |v| crate::spec::convert::yaml_to_json(v).map_err(|e: anyhow::Error| e.to_string());

        match detect_shape(&v) {
            // `StrategyRef` owns the preset-name gate. Routing presets here
            // rather than letting them fall through is the whole reason this
            // decision is shared: post-JSON-bridge a preset is a bare
            // single-key map, which the multi-asset arm below would swallow.
            ShapeHint::Preset | ShapeHint::Single => {
                StrategyRef::try_from(v).map(AnyStrategyRef::Single)
            }
            ShapeHint::Pairs => serde_json::from_value::<PairsStrategySpec>(via_json(v)?)
                .map(|p| AnyStrategyRef::Pairs(Box::new(p)))
                .map_err(|e| e.to_string()),
            ShapeHint::Basket => serde_json::from_value::<BasketStrategySpec>(via_json(v)?)
                .map(|b| AnyStrategyRef::Basket(Box::new(b)))
                .map_err(|e| e.to_string()),
            ShapeHint::Multi => serde_json::from_value::<MultiAssetStrategySpec>(via_json(v)?)
                .map(|m| AnyStrategyRef::Multi(Box::new(m)))
                .map_err(|e| e.to_string()),
        }
    }
}

/// A boxed real-valued source over the single-asset snapshot stream — the
/// erased form every trailing indicator collapses to.
type BoxedReal = Box<dyn Indicator<Input = Snapshot<Symbol>, Output = Real> + Send + Sync>;

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
    type Input = Snapshot<Symbol>;
    type Output = Real;

    fn update(&mut self, input: Snapshot<Symbol>) -> Option<Real> {
        self.inner.update(input)
    }

    fn value(&self) -> Option<Real> {
        self.inner.value()
    }

    fn warm_up_bars(&self) -> usize {
        self.inner.warm_up_bars()
    }

    fn unstable_bars(&self) -> usize {
        self.inner.unstable_bars()
    }

    fn reset(&mut self) {
        self.inner.reset();
    }

    fn save_state(&self) -> serde_json::Value {
        self.inner.save_state()
    }

    fn load_state(&mut self, state: &serde_json::Value) -> Result<(), String> {
        self.inner.load_state(state)
    }
}

/// Wrap a freshly-built strategy in the trailing indicator `metric` selects,
/// erased to [`BoxedReal`]. Generic over the strategy type so the single / pairs
/// / basket arms share one body. `fallback_symbol` is the tag the embedded
/// engine applies to untagged snapshot entries.
fn make<S>(
    metric: TrailingMetric,
    strat: S,
    fallback_symbol: Symbol,
    period: usize,
    risk_free_rate: Real,
    bars_per_year: Real,
) -> BoxedReal
where
    S: crate::Strategy<Symbol = Symbol, Input = Snapshot<Symbol>> + Send + Sync + 'static,
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
) -> Result<Box<dyn PayloadIndicator>, String> {
    let spec = Arc::new(strategy.clone());
    let schema = Arc::clone(schema);
    let fallback = strategy.fallback_symbol();

    let try_build_fn: Arc<dyn Fn() -> Result<BoxedReal, String> + Send + Sync> =
        Arc::new(move || {
            let sym = fallback.clone();
            Ok(match &*spec {
                AnyStrategyRef::Single(s) => make(
                    metric,
                    s.try_build(SEED, &schema)?,
                    sym,
                    period,
                    risk_free_rate,
                    bars_per_year,
                ),
                AnyStrategyRef::Pairs(p) => make(
                    metric,
                    p.try_build(SEED, &schema)?,
                    sym,
                    period,
                    risk_free_rate,
                    bars_per_year,
                ),
                AnyStrategyRef::Basket(b) => make(
                    metric,
                    b.try_build(SEED, &schema)?,
                    sym,
                    period,
                    risk_free_rate,
                    bars_per_year,
                ),
                AnyStrategyRef::Multi(m) => make(
                    metric,
                    m.try_build(SEED, &schema)?,
                    sym,
                    period,
                    risk_free_rate,
                    bars_per_year,
                ),
            })
        });

    // The first construction is fallible: a malformed embedded `strategy:`
    // subtree is bad *input*, and the caller wraps this `Err` with the
    // enclosing `!sharpe` / `!sortino` / … tag to extend the breadcrumb.
    let inner = try_build_fn()?;

    // `RebuildIndicator` needs an infallible factory — it rebuilds on
    // `reset()` and on `Clone`, neither of which has an error path to return
    // through. The build above already succeeded against this exact spec and
    // schema, and nothing about either changes afterwards, so every later
    // rebuild succeeds too. Same argument as the basket/multi per-symbol
    // factories, which are probed once at build time for the same reason.
    let build_fn: Arc<dyn Fn() -> BoxedReal + Send + Sync> = Arc::new(move || {
        try_build_fn().unwrap_or_else(|e| {
            panic!("trailing metric rebuild failed after a successful first build: {e}")
        })
    });

    Ok(dyn_indicator::wrap(RebuildIndicator {
        build: build_fn,
        inner,
    }))
}
