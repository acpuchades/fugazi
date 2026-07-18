//! YAML-deserializable [`PortfolioSpec`] — a top-level composite strategy
//! that runs N heterogeneous child strategies against one shared cash pool
//! through a [`Portfolio<String>`](fugazi::portfolio::Portfolio).
//!
//! Each child slot names a child (for reporting) and a nested strategy of
//! any shape — single-asset, pairs, basket, or multi-asset — routed by
//! distinctive top-level key on the child's `strategy:` map. The weight
//! policy governs how `--cash` is split across children at build time (v1:
//! init-only, weights don't rebalance).
//!
//! ```yaml
//! weights: !fixed [0.4, 0.6]
//! children:
//!   - name: trend
//!     strategy: !ma_crossover { symbol: BTC, fast: 20, slow: 50 }
//!   - name: mean_reversion
//!     strategy:
//!       symbol: ETH
//!       long:
//!         enter: !crosses_above { lhs: !rsi { period: 14 }, rhs: !value 30 }
//! ```
//!
//! To reuse one child spec N times with different parameters (the natural
//! way to build a multi-strategy portfolio without name-clashing globals),
//! reach for `!import { path, params }`:
//!
//! ```yaml
//! children:
//!   - name: fast_trend
//!     strategy: !import { path: trend.yml, params: { FAST: 10, SLOW: 30 } }
//!   - name: slow_trend
//!     strategy: !import { path: trend.yml, params: { FAST: 50, SLOW: 200 } }
//! ```
//!
//! `weights:` is optional — omitting it (or writing `!equal_weight`) picks
//! [`EqualWeight`](fugazi::portfolio::policy::EqualWeight), splitting cash
//! 1/N across children.

use std::collections::HashMap;
use std::sync::Arc;

use anyhow::{Context, Result};
use serde::Deserialize;
use serde_json::Value;

use fugazi::indicators::{Book, Position};
use fugazi::portfolio::policy::{EqualWeight, Fixed, WeightPolicy};
use fugazi::portfolio::Portfolio;
use fugazi::prelude::*;
use fugazi::types::Snapshot;

use crate::dyn_indicator::{AsBool, DynIndicator};

use super::basket::BasketStrategySpec;
use super::multi_asset::MultiAssetStrategySpec;
use super::pairs::PairsStrategySpec;
use super::preset::StrategyRef;
use super::signal::SignalSpec;

/// A whole `portfolio.yml`: an ordered list of children plus an optional
/// weight policy that governs how the initial cash is split at build.
#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PortfolioSpec {
    /// The child strategies, in insertion order. Weights returned by
    /// [`WeightPolicySpec`] apply in the same order. Must be non-empty.
    pub children: Vec<PortfolioChildSpec>,

    /// How the initial cash is split across children at build. Omitted
    /// means [`WeightPolicySpec::EqualWeight`] (1/N per child).
    ///
    /// Weights are read once at build to seed each child's sub-wallet;
    /// on subsequent [`rebalance_on`](Self::rebalance_on) fire bars the
    /// policy is re-queried to compute new targets.
    #[serde(default)]
    pub weights: Option<WeightPolicySpec>,

    /// The **rebalance gate**: a boolean signal deciding, on each bar,
    /// whether the portfolio runs one rebalance cycle after children
    /// have traded. Defaults to `!never` (`Const::false`) — no
    /// rebalance, weights drift with per-child P&L (v1 behavior).
    ///
    /// Common cadences: `!every 5` for weekly on a daily portfolio,
    /// `!every 28` for approximately monthly, or a compound signal
    /// (`!or [!every 28, !gt { lhs: !drawdown, rhs: !value 0.1 }]`) for
    /// scheduled rebalance with drawdown-triggered overrides.
    ///
    /// A `None` reading (from a still-warming user signal) is treated as
    /// `false` — the safe default; the portfolio sits between rebalances
    /// rather than trading through unsettled data.
    ///
    /// Each fire runs the same two-phase rebalance: cash phase first
    /// (contributors donate free cash, receivers split the pot), then a
    /// position phase for any contributor whose cash phase couldn't
    /// cover its donation (submits proportional `set_position`
    /// scale-downs that fill next bar, freeing cash for the following
    /// fire cycle). A rebalance whose cash phase covers everyone stays
    /// fill-free automatically.
    #[serde(default)]
    pub rebalance_on: Option<SignalSpec>,
}

/// One child slot: an optional display name plus the nested strategy spec.
#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PortfolioChildSpec {
    /// Display name for logs and downstream per-child reporting. Defaults
    /// to `child_<idx>` when omitted.
    #[serde(default)]
    pub name: Option<String>,

    /// The nested strategy — of any shape. Routed by distinctive top-level
    /// key on the child's `strategy:` map (see [`PortfolioChildStrategy`]).
    pub strategy: PortfolioChildStrategy,
}

/// A strategy spec of any of fugazi's four shapes, used as a
/// [`Portfolio`]'s child. Routed by distinctive top-level key on the
/// deserialized value:
///
/// - a tagged value (`!ma_crossover`, `!buy_and_hold`, …) → a preset,
///   dispatched through [`StrategyRef`];
/// - a map with both `left:` and `right:` → [`PairsStrategySpec`];
/// - a map with `selection:` → [`BasketStrategySpec`];
/// - a map with `symbol:` → a single-asset [`StrategyRef`];
/// - any other map → [`MultiAssetStrategySpec`].
///
/// Deserialized through the same [`serde_norway::Value`] bridge as
/// [`AnyStrategyRef`](super::trailing::AnyStrategyRef), widened to include
/// [`MultiAssetStrategySpec`] since a portfolio child may be a per-symbol
/// independent replicated strategy too.
#[derive(Debug, Clone, Deserialize)]
#[serde(try_from = "serde_norway::Value")]
pub enum PortfolioChildStrategy {
    Single(Box<StrategyRef>),
    Pairs(Box<PairsStrategySpec>),
    Basket(Box<BasketStrategySpec>),
    Multi(Box<MultiAssetStrategySpec>),
}

impl TryFrom<serde_norway::Value> for PortfolioChildStrategy {
    type Error = String;

    fn try_from(v: serde_norway::Value) -> Result<Self, Self::Error> {
        use serde_norway::Value as YV;

        // A preset arrives either as a YAML `!tag { … }` (Value::Tagged)
        // or, on the serde_json load path, as a single-key `{ tag: { … } }`
        // mapping. Either shape routes to `StrategyRef` — which owns the
        // preset-name gate — before we consider the other strategy shapes.
        //
        // Ordering matters: `{"buy_and_hold": {...}}` is a bare mapping
        // without `symbol:` at the top, so the multi-asset arm would
        // otherwise swallow it and fail on the unknown `buy_and_hold`
        // field. Routing presets first sidesteps that.
        let is_preset_shape = matches!(&v, YV::Tagged(_))
            || matches!(&v, YV::Mapping(m) if m.len() == 1 && matches!(
                m.iter().next(),
                Some((YV::String(k), _)) if is_preset_tag(k)
            ));
        if is_preset_shape {
            return StrategyRef::try_from(v)
                .map(|s| PortfolioChildStrategy::Single(Box::new(s)));
        }

        // Detect shape by distinctive top-level key. `left`+`right` picks
        // pairs; `selection` picks basket; `symbol:` picks single-asset
        // (spec map); a bare map with none of those goes to multi-asset
        // (`long`/`short`/`sizing` per-symbol templates).
        let (is_pairs, is_basket, has_symbol) = match &v {
            YV::Mapping(m) => {
                let has = |key: &str| {
                    m.iter()
                        .any(|(k, _)| matches!(k, YV::String(s) if s == key))
                };
                (has("left") && has("right"), has("selection"), has("symbol"))
            }
            _ => (false, false, false),
        };

        // The tag-normalising JSON bridge is required by `BasketStrategySpec`
        // (its `SpecTemplate` captures `serde_json::Value` — the raw
        // `serde_norway::Value` path can't feed it) and by
        // `MultiAssetStrategySpec` (same reason). Kept consistent for pairs
        // too so all three go through one path.
        if is_pairs {
            let json = crate::convert::yaml_to_json(v).map_err(|e| e.to_string())?;
            return serde_json::from_value::<PairsStrategySpec>(json)
                .map(|p| PortfolioChildStrategy::Pairs(Box::new(p)))
                .map_err(|e| e.to_string());
        }
        if is_basket {
            let json = crate::convert::yaml_to_json(v).map_err(|e| e.to_string())?;
            return serde_json::from_value::<BasketStrategySpec>(json)
                .map(|b| PortfolioChildStrategy::Basket(Box::new(b)))
                .map_err(|e| e.to_string());
        }
        // A bare mapping without `symbol:` (and without pairs/basket keys)
        // is multi-asset — the shape with no upfront symbol declaration.
        if matches!(&v, YV::Mapping(_)) && !has_symbol {
            let json = crate::convert::yaml_to_json(v).map_err(|e| e.to_string())?;
            return serde_json::from_value::<MultiAssetStrategySpec>(json)
                .map(|m| PortfolioChildStrategy::Multi(Box::new(m)))
                .map_err(|e| e.to_string());
        }
        // Fall through: a `symbol:`-carrying single-asset spec map that
        // `StrategyRef` handles (presets already routed above).
        StrategyRef::try_from(v).map(|s| PortfolioChildStrategy::Single(Box::new(s)))
    }
}

/// Whether `name` is one of [`preset::PRESET_TAGS`]. Kept in sync with
/// that constant by [`preset_tags_match`](tests::preset_tags_match) —
/// duplicating the check here avoids exposing the private constant
/// through the `preset` module.
fn is_preset_tag(name: &str) -> bool {
    matches!(
        name,
        "buy_and_hold"
            | "ma_crossover"
            | "rsi_reversal"
            | "donchian_breakout"
            | "keltner_breakout"
    )
}

/// YAML surface for the [`WeightPolicy`] governing the initial cash split.
///
/// Externally tagged: `!equal_weight` (unit) picks
/// [`EqualWeight`](fugazi::portfolio::policy::EqualWeight); `!fixed [w1, w2, …]`
/// (tuple over a plain list) picks [`Fixed`](fugazi::portfolio::policy::Fixed).
/// Weights are magnitudes and needn't sum to `1.0` — the portfolio
/// normalizes on use.
#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "snake_case", deny_unknown_fields)]
pub enum WeightPolicySpec {
    /// The 1/N uniform policy — stateless.
    EqualWeight,
    /// A fixed weight vector of length `children.len()`. Panics at build
    /// if the vector's length doesn't match the number of children.
    Fixed(Vec<Real>),
}

impl PortfolioSpec {
    /// Parse a YAML portfolio document, applying `!import` splices and
    /// `!param` substitutions before typed deserialization.
    pub fn from_text_with_params_in(
        text: &str,
        params: &HashMap<String, Value>,
        base: &std::path::Path,
        label: &str,
    ) -> Result<Self> {
        let value = super::load_value(text, params, base, label)?;
        serde_json::from_value(value)
            .with_context(|| format!("building portfolio strategy from {label}"))
    }

    /// Test convenience: [`from_text_with_params_in`](Self::from_text_with_params_in)
    /// with imports resolved against the working directory and an
    /// `(inline)` source label.
    #[cfg(test)]
    pub fn from_text_with_params(text: &str, params: &HashMap<String, Value>) -> Result<Self> {
        Self::from_text_with_params_in(text, params, std::path::Path::new("."), "(inline)")
    }

    /// Build the live [`DynPortfolio`] this spec describes.
    ///
    /// `total_initial_equity` is the whole cash budget passed to the
    /// portfolio builder — split across children per the weight policy.
    /// Each child's own [`SingleAssetStrategy::with_initial_equity`](fugazi::strategies::SingleAssetStrategy::with_initial_equity)-style
    /// book seed is set to the child's allocated share, so book-anchored
    /// sizing recipes inside a child read against that child's slice of
    /// the pool rather than the aggregate.
    ///
    /// `costs` is the [`TradingCosts`] bundle installed on every child's
    /// sub-wallet — [`Portfolio`](fugazi::portfolio::Portfolio) applies the
    /// same bundle uniformly (v1 constraint: no per-symbol dispatch
    /// through the composite wallet). Pass `None` to skip cost wiring
    /// (matches the zero-cost paper-wallet default the other specs use for
    /// gross twins).
    ///
    /// # Panics
    /// Panics if the spec declares no children (a zero-child portfolio has
    /// no meaning) or if a [`WeightPolicySpec::Fixed`] weight vector's
    /// length doesn't match the number of children.
    pub fn build(
        &self,
        total_initial_equity: Real,
        schema: &Arc<Schema>,
        costs: Option<TradingCosts>,
    ) -> DynPortfolio {
        assert!(
            !self.children.is_empty(),
            "PortfolioSpec::build: `children:` must have at least one entry"
        );
        let n = self.children.len();
        let allocations = self.resolve_allocations(total_initial_equity, n);

        // Track each child's readiness periods at build. We inspect the
        // typed child *before* boxing into `Box<dyn Strategy>` — the
        // erased trait doesn't expose `stable_period` / `warm_up_period`,
        // so this is the only chance to capture them for
        // [`DynPortfolio::stable_period`] to aggregate later.
        //
        // Multi / basket children with lazy per-symbol chains report only
        // their rebalance signal's period at this point (chains build on
        // first snapshot). A portfolio containing them may under-report
        // stable_period slightly at build time — accurate for the common
        // case of a portfolio of single-asset strategies, understated for
        // portfolios of basket / multi children. The `optimize
        // --walkforward` layout uses this reading to skip the initial
        // warm-up, so an understated portfolio period there means the
        // first IS window may include a few unsettled bars for lazy
        // children (documented v1 limitation).
        let mut max_stable = 0usize;
        let mut max_warm_up = 0usize;
        let mut builder = Portfolio::<String>::builder().with_initial_equity(total_initial_equity);
        for (i, c) in self.children.iter().enumerate() {
            let name = c
                .name
                .clone()
                .unwrap_or_else(|| format!("child_{i}"));
            let child_equity = allocations[i];
            let (stable, warm_up);
            builder = match &c.strategy {
                PortfolioChildStrategy::Single(s) => {
                    let built = s.build(child_equity, schema);
                    stable = built.stable_period();
                    warm_up = built.warm_up_period();
                    builder.add(name, built)
                }
                PortfolioChildStrategy::Pairs(p) => {
                    let built = p.build(child_equity, schema);
                    stable = built.stable_period();
                    warm_up = built.warm_up_period();
                    builder.add(name, built)
                }
                PortfolioChildStrategy::Basket(b) => {
                    let built = b.build(child_equity, schema);
                    stable = built.stable_period();
                    warm_up = built.warm_up_period();
                    builder.add(name, built)
                }
                PortfolioChildStrategy::Multi(m) => {
                    let built = m.build(child_equity, schema);
                    stable = built.stable_period();
                    warm_up = built.warm_up_period();
                    builder.add(name, built)
                }
            };
            max_stable = max_stable.max(stable);
            max_warm_up = max_warm_up.max(warm_up);
        }
        if let Some(c) = costs {
            builder = builder.costs(c);
        }
        // Install the policy on the builder; on rebalance-fire bars
        // `Portfolio` re-queries `WeightPolicy::weights` to compute new
        // target equities from the aggregate.
        builder = match &self.weights {
            Some(WeightPolicySpec::Fixed(w)) => builder.weights(Fixed::new(w.clone())),
            Some(WeightPolicySpec::EqualWeight) | None => builder.weights(EqualWeight),
        };
        // Install the rebalance gate — a boolean signal over
        // `Snapshot<String>`. Built against a dummy `Position` and a
        // fresh `Book` because a portfolio-level rebalance signal has no
        // per-child position or book to anchor to; a signal using
        // `!entry` / `!drawdown` at this level will read the (empty)
        // dummies. Fold the signal's stable / warm-up periods into the
        // aggregate so `optimize --walkforward` sees an accurate head
        // skip.
        if let Some(rebalance_spec) = &self.rebalance_on {
            let anchor = Position::new();
            let book = Book::new(total_initial_equity);
            let dyn_ind: Box<dyn DynIndicator> =
                rebalance_spec.build(&anchor, &book, schema);
            let signal = AsBool::new(dyn_ind);
            max_stable = max_stable.max(signal.stable_period());
            max_warm_up = max_warm_up.max(signal.warm_up_period());
            builder = builder.rebalance_on(signal);
        }
        let built = builder.build();
        DynPortfolio {
            inner: built,
            stable_period: max_stable,
            warm_up_period: max_warm_up,
        }
    }

    /// Pre-compute the per-child cash allocations the built [`Portfolio`]
    /// will seed each sub-wallet with — used to hand each child its own
    /// slice as its book seed. Mirrors the split
    /// [`PortfolioBuilder::build`](fugazi::portfolio::PortfolioBuilder::build)
    /// does internally; kept in sync by construction (both consult the same
    /// policy via [`WeightPolicy::weights`]).
    fn resolve_allocations(&self, total: Real, n: usize) -> Vec<Real> {
        let policy: Box<dyn WeightPolicy> = match &self.weights {
            Some(WeightPolicySpec::Fixed(w)) => Box::new(Fixed::new(w.clone())),
            Some(WeightPolicySpec::EqualWeight) | None => Box::new(EqualWeight),
        };
        let weights = policy.weights(n);
        let sum: Real = weights.iter().sum();
        if sum <= 0.0 {
            // Degenerate policy (all-zero weights): fall through to equal
            // split so the CLI reports something usable rather than 0s.
            return vec![total / n as Real; n];
        }
        weights.iter().map(|w| total * w / sum).collect()
    }
}

// ---------------------------------------------------------------------------
// DynPortfolio: CLI-owned wrapper around Portfolio<String>
// ---------------------------------------------------------------------------

/// The CLI's built portfolio handle. Implements [`Strategy`] by
/// delegation so it drops into [`fugazi::backtest::run`] unchanged — but
/// unlike the other `Dyn*Strategy`s it must be driven through
/// [`wallet_view`](Self::wallet_view) rather than a plain [`PaperWallet`],
/// since portfolio fills route through a composite wallet that owns one
/// sub-wallet per child.
pub struct DynPortfolio {
    inner: Portfolio<String>,
    /// Max child stable-period captured at build (see
    /// [`PortfolioSpec::build`] for the lazy-child caveat).
    stable_period: usize,
    /// Max child warm-up-period captured at build.
    warm_up_period: usize,
}

impl Strategy for DynPortfolio {
    type Input = Snapshot<String>;
    type Symbol = String;

    fn update(&mut self, input: Snapshot<String>) {
        self.inner.update(input);
    }
    fn trade(&self, wallet: &mut dyn Wallet<String>) {
        self.inner.trade(wallet);
    }
    fn on_fill(&mut self, order: &Order<String>) {
        self.inner.on_fill(order);
    }
    fn is_ready(&self) -> bool {
        self.inner.is_ready()
    }
    fn reset(&mut self) {
        self.inner.reset();
    }
}

impl DynPortfolio {
    /// A fresh aggregate [`PortfolioWallet`](fugazi::portfolio::PortfolioWallet)
    /// sharing this portfolio's interior — the wallet the CLI hands to
    /// [`fugazi::backtest::run`]. Multiple views share the same underlying
    /// state, so a second view for side-inspection is cheap.
    pub fn wallet_view(&self) -> fugazi::portfolio::PortfolioWallet<String> {
        self.inner.wallet_view()
    }

    /// The number of children the portfolio holds, in `.add(...)` order.
    #[allow(dead_code)]
    pub fn child_count(&self) -> usize {
        self.inner.child_count()
    }

    /// The aggregate stable-period across every child, captured at build.
    /// Used by `optimize --walkforward` to skip the initial warm-up before
    /// starting IS windows.
    ///
    /// See [`PortfolioSpec::build`] for the lazy-child caveat: portfolios
    /// containing basket / multi children under-report this at build time
    /// (only the child's rebalance signal period), since lazy per-symbol
    /// chains haven't built yet.
    pub fn stable_period(&self) -> usize {
        self.stable_period
    }

    /// Warm-up-only aggregate (ignoring IIR settling) — the walkforward
    /// twin of [`stable_period`](Self::stable_period), used under
    /// `--keep-unstable`.
    pub fn warm_up_period(&self) -> usize {
        self.warm_up_period
    }

    /// Install a per-symbol [`TradingCosts`] bundle on every sub-wallet
    /// inside the composite. Used by CLI runners to thread scoped
    /// `--costs SYM:...` overrides through the portfolio boundary: after
    /// the composite is built with a uniform default (via
    /// [`PortfolioSpec::build`]'s `costs` arg), each symbol's resolved
    /// bundle is installed here on every sub, so whichever child ends up
    /// filling that symbol books at the right rate.
    pub fn install_costs_for(&mut self, symbol: &str, costs: TradingCosts) {
        self.inner.install_costs_for(&symbol.to_string(), costs);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use fugazi::types::Atom;

    fn candle(price: Real) -> Candle {
        Candle::new(price, price, price, price, 0.0)
    }

    fn snap_of(entries: &[(&'static str, Real)]) -> Snapshot<String> {
        let mut s = Snapshot::new();
        for &(sym, close) in entries {
            let atom = Atom::new(candle(close));
            s.push(Some(sym.to_string()), None, atom);
        }
        s
    }

    #[test]
    fn parses_a_portfolio_with_mixed_children() {
        let yaml = r#"
            weights: !fixed [0.6, 0.4]
            children:
              - name: hold_btc
                strategy: !buy_and_hold { symbol: BTC }
              - name: rsi_eth
                strategy:
                  symbol: ETH
                  long:
                    enter: !gt { lhs: !close, rhs: !value 0 }
        "#;
        let spec = PortfolioSpec::from_text_with_params(yaml, &HashMap::new()).unwrap();
        assert_eq!(spec.children.len(), 2);
        assert!(matches!(&spec.children[0].strategy, PortfolioChildStrategy::Single(_)));
        assert!(matches!(&spec.children[1].strategy, PortfolioChildStrategy::Single(_)));
        assert!(matches!(spec.weights, Some(WeightPolicySpec::Fixed(_))));
    }

    #[test]
    fn weights_default_to_equal_when_omitted() {
        let yaml = r#"
            children:
              - strategy: !buy_and_hold { symbol: A }
              - strategy: !buy_and_hold { symbol: B }
        "#;
        let spec = PortfolioSpec::from_text_with_params(yaml, &HashMap::new()).unwrap();
        assert!(spec.weights.is_none());
        // Two children with equal-weight split of 1000 → 500 each.
        let allocations = spec.resolve_allocations(1000.0, 2);
        assert_eq!(allocations, vec![500.0, 500.0]);
    }

    #[test]
    fn fixed_weights_split_cash_proportionally() {
        let yaml = r#"
            weights: !fixed [0.75, 0.25]
            children:
              - strategy: !buy_and_hold { symbol: A }
              - strategy: !buy_and_hold { symbol: B }
        "#;
        let spec = PortfolioSpec::from_text_with_params(yaml, &HashMap::new()).unwrap();
        let allocations = spec.resolve_allocations(1000.0, 2);
        assert_eq!(allocations, vec![750.0, 250.0]);
    }

    #[test]
    fn child_strategy_shape_routing() {
        // Pairs: has left+right.
        let yaml = r#"
            children:
              - strategy: { left: BTC, right: ETH, enter: !value true }
        "#;
        let spec = PortfolioSpec::from_text_with_params(yaml, &HashMap::new()).unwrap();
        assert!(matches!(&spec.children[0].strategy, PortfolioChildStrategy::Pairs(_)));

        // Basket: has selection.
        let yaml = r#"
            children:
              - strategy:
                  selection: !top_bottom { longs: 1, shorts: 1 }
                  score: !roc { source: !close { source: !pick { symbol: !arg SYM } }, periods: 5 }
                  sizing: !equal_weight 2
        "#;
        let spec = PortfolioSpec::from_text_with_params(yaml, &HashMap::new()).unwrap();
        assert!(matches!(&spec.children[0].strategy, PortfolioChildStrategy::Basket(_)));

        // Multi: no symbol, no pairs/basket keys.
        let yaml = r#"
            children:
              - strategy:
                  long:
                    enter: !gt { lhs: !close { source: !pick { symbol: !arg SYM } }, rhs: !value 0 }
                  sizing: !equal_weight 2
        "#;
        let spec = PortfolioSpec::from_text_with_params(yaml, &HashMap::new()).unwrap();
        assert!(matches!(&spec.children[0].strategy, PortfolioChildStrategy::Multi(_)));
    }

    #[test]
    fn build_drives_two_buy_and_hold_children_split_by_weights() {
        let yaml = r#"
            weights: !fixed [0.6, 0.4]
            children:
              - name: hold_a
                strategy: !buy_and_hold { symbol: A }
              - name: hold_b
                strategy: !buy_and_hold { symbol: B }
        "#;
        let spec = PortfolioSpec::from_text_with_params(yaml, &HashMap::new()).unwrap();
        let mut portfolio = spec.build(10_000.0, &Schema::empty(), None);
        let mut wallet = portfolio.wallet_view();

        // Two bars: bar 1 queues entry, bar 2 fills. Portfolio wallet fans
        // the update to every sub, so each child's own PaperWallet marks +
        // fills its own leg.
        for _ in 0..2 {
            for fill in wallet.update("A".to_string(), candle(100.0)) {
                portfolio.on_fill(&fill);
            }
            for fill in wallet.update("B".to_string(), candle(50.0)) {
                portfolio.on_fill(&fill);
            }
            portfolio.update(snap_of(&[("A", 100.0), ("B", 50.0)]));
            portfolio.trade(&mut wallet);
        }
        // Aggregate equity across both sub-wallets is roughly the seed (no
        // move in prices → no P&L).
        assert!((wallet.equity().0 - 10_000.0).abs() < 1e-6);
        // Both legs are long — each child bought its own symbol.
        assert!(wallet.position(&"A".to_string()).amount > 0.0);
        assert!(wallet.position(&"B".to_string()).amount > 0.0);
    }

    #[test]
    #[should_panic(expected = "children")]
    fn build_panics_on_empty_children() {
        // Empty children list — `!` on the deserialize side is caught by the
        // build panic (matches PortfolioBuilder::build's own invariant).
        let yaml = r#"
            children: []
        "#;
        let spec = PortfolioSpec::from_text_with_params(yaml, &HashMap::new()).unwrap();
        let _ = spec.build(10_000.0, &Schema::empty(), None);
    }

    #[test]
    fn params_are_substituted_at_load_time() {
        // A `!param` inside a child's strategy spec is resolved on the way
        // in, exactly like on the other strategy specs.
        let yaml = r#"
            children:
              - name: hold
                strategy: !buy_and_hold { symbol: !param SYM }
        "#;
        let mut params = HashMap::new();
        params.insert("SYM".to_string(), Value::String("BTC".to_string()));
        let spec = PortfolioSpec::from_text_with_params(yaml, &params).unwrap();
        match &spec.children[0].strategy {
            PortfolioChildStrategy::Single(s) => assert_eq!(s.symbol(), "BTC"),
            _ => panic!("expected a single-asset child"),
        }
    }

    #[test]
    fn rebalance_on_defaults_to_none() {
        // Omitted `rebalance_on:` → the built portfolio behaves as
        // pre-rebalance v1 (Const::false gate, weights drift with P&L).
        let yaml = r#"
            children:
              - strategy: !buy_and_hold { symbol: A }
              - strategy: !buy_and_hold { symbol: B }
        "#;
        let spec = PortfolioSpec::from_text_with_params(yaml, &HashMap::new()).unwrap();
        assert!(spec.rebalance_on.is_none());
        // Should build without failure.
        let _portfolio = spec.build(1_000.0, &Schema::empty(), None);
    }

    #[test]
    fn parses_rebalance_on_every_28() {
        let yaml = r#"
            rebalance_on: !every 28
            children:
              - strategy: !buy_and_hold { symbol: A }
              - strategy: !buy_and_hold { symbol: B }
        "#;
        let spec = PortfolioSpec::from_text_with_params(yaml, &HashMap::new()).unwrap();
        assert!(spec.rebalance_on.is_some());
    }

    #[test]
    fn parses_rebalance_on_never_as_const_false() {
        let yaml = r#"
            rebalance_on: !never
            children:
              - strategy: !buy_and_hold { symbol: A }
              - strategy: !buy_and_hold { symbol: B }
        "#;
        let spec = PortfolioSpec::from_text_with_params(yaml, &HashMap::new()).unwrap();
        assert!(spec.rebalance_on.is_some());
        // Should build cleanly — !never resolves to Const::false, which
        // has zero warm-up / stable period.
        let portfolio = spec.build(1_000.0, &Schema::empty(), None);
        assert_eq!(portfolio.stable_period(), 0);
    }

    #[test]
    fn build_drives_rebalance_cycle_snapping_equities_to_fixed_target() {
        // End-to-end: partial-sizing buy-and-hold children with a rebalance
        // gate that fires every bar should snap sub-equities back to the
        // Fixed target after price divergence — cash phase does all the work
        // since contributors have cash headroom (position phase is a no-op).
        //
        // Bar 1: children enter (queue market orders).
        // Bar 2: fills at $100 → each child holds 2.5 units of its symbol
        //        + 250 cash. Equities: 500 each. Rebalance no-op.
        // Bar 3: A jumps to $200. A's position value doubles: 250 cash +
        //        500 in position = 750 equity. B stays at 500. Total 1250.
        //        Rebalance fires: A donates 125 cash to B. Result: 625 each.
        // Bar 4: nothing changes. Rebalance is a no-op (equities at target).
        let yaml = r#"
            weights: !fixed [0.5, 0.5]
            rebalance_on: !every 1
            children:
              - name: half_a
                strategy:
                  symbol: A
                  sizing: !value 0.5
                  long:
                    enter: !value true
                    exit: !value false
              - name: half_b
                strategy:
                  symbol: B
                  sizing: !value 0.5
                  long:
                    enter: !value true
                    exit: !value false
        "#;
        let spec = PortfolioSpec::from_text_with_params(yaml, &HashMap::new()).unwrap();
        let mut portfolio = spec.build(1_000.0, &Schema::empty(), None);
        let mut wallet = portfolio.wallet_view();

        for bar in 0..4usize {
            let px_a = if bar < 2 { 100.0 } else { 200.0 };
            let px_b = 100.0;
            for fill in wallet.update("A".to_string(), candle(px_a)) {
                portfolio.on_fill(&fill);
            }
            for fill in wallet.update("B".to_string(), candle(px_b)) {
                portfolio.on_fill(&fill);
            }
            portfolio.update(snap_of(&[("A", px_a), ("B", px_b)]));
            portfolio.trade(&mut wallet);
        }

        let e0 = wallet.sub_equity(0).0;
        let e1 = wallet.sub_equity(1).0;
        assert!(
            (e0 - e1).abs() < 1.0,
            "cash-mode rebalance should snap sub-equities to 50/50; got e0={e0}, e1={e1}",
        );
    }
}
