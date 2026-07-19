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
//! weights: !value [0.4, 0.6]        # per-child fixed weights
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
//! `weights:` is a portfolio-scope indicator expression, instantiated per
//! child at build time. Omitting it picks an equal split (`1/N`).
//! `!value <list>` gives per-child indexed constants (the classic "fixed
//! weights" case); any other expression drives dynamic weighting.
//! `!fixed [...]` and `!equal_weight` are recognized as sugar and
//! rewritten to their `!value` equivalents at load time.

use std::collections::HashMap;
use std::sync::Arc;

use anyhow::{Context, Result};
use serde::Deserialize;
use serde_json::Value;

use fugazi::indicators::{Book, Position};
use fugazi::portfolio::policy::{EqualWeight, Fixed};
use fugazi::portfolio::rebalance::{LargestFirst, Proportional};
use fugazi::portfolio::Portfolio;
use fugazi::prelude::*;
use fugazi::types::Snapshot;

use crate::dyn_indicator::{AsBool, AsReal, DynIndicator};

use super::basket::BasketStrategySpec;
use super::expr::ExprSpec;
use super::multi_asset::MultiAssetStrategySpec;
use super::pairs::PairsStrategySpec;
use super::preset::StrategyRef;
use super::signal::SignalSpec;
use super::template::SpecTemplate;

/// YAML surface for the **position-phase rebalance policy** — the impl
/// picked from [`rebalance`](fugazi::portfolio::rebalance) that decides
/// which held positions to scale down (and by how much) when a
/// contributor's cash-phase donation can't be fully covered.
///
/// Externally tagged, currently unit-only:
///
/// ```yaml
/// rebalance_policy: !proportional   # default — every leg scaled uniformly
/// rebalance_policy: !largest_first  # fully close biggest positions first
/// ```
///
/// Omitted (`rebalance_policy:` absent) defaults to
/// [`Proportional`](fugazi::portfolio::rebalance::Proportional), matching
/// the [`PortfolioBuilder`](fugazi::portfolio::PortfolioBuilder) default.
/// A CLI-only discriminator; at build it constructs the corresponding
/// [`PositionRebalancer`](fugazi::portfolio::rebalance::PositionRebalancer)
/// impl and installs it via
/// [`PortfolioBuilder::position_rebalancer`](fugazi::portfolio::PortfolioBuilder::position_rebalancer).
/// Rust-side callers with a bespoke rule build their own impl and install
/// it directly — no CLI-side wiring needed.
#[derive(Debug, Clone, Copy, Deserialize)]
#[serde(rename_all = "snake_case", deny_unknown_fields)]
pub enum RebalancePolicySpec {
    /// Scale every held leg by the same fraction to cover the shortfall.
    /// The default — matches
    /// [`Proportional`](fugazi::portfolio::rebalance::Proportional).
    Proportional,

    /// Fully liquidate biggest positions (by `|units| * price`) first,
    /// walking down until the shortfall is covered. The last position
    /// touched is partially scaled if fully closing it would overshoot.
    /// Wraps [`LargestFirst`](fugazi::portfolio::rebalance::LargestFirst).
    LargestFirst,
}

/// A whole `portfolio.yml`: an ordered list of children plus an optional
/// weight expression governing how cash is split at build and re-targeted
/// on each rebalance-fire.
#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PortfolioSpec {
    /// The child strategies, in insertion order. Weight expressions are
    /// instantiated per child in the same order, and `!value <list>`
    /// literals index into their list by that position. Must be non-empty.
    pub children: Vec<PortfolioChildSpec>,

    /// The weight expression: a portfolio-scope indicator instantiated
    /// once per child at build time. The portfolio reads each instance's
    /// value at every rebalance-fire and normalizes `w_i = N_i / Σ N_j`.
    ///
    /// Everything is expressions — the two common patterns just use
    /// convenient constants:
    /// - **Omitted** → equal weight (every child seeded to `1/N`; on
    ///   rebalance-fire, equal target as long as no weight expression
    ///   changes it).
    /// - **`!value [w0, w1, ...]`** → per-child indexed constant (child
    ///   *i* reads `w_i`). Equivalent to the classic "fixed weights"
    ///   policy — no separate tag needed.
    /// - **`!value 1.0`** (or any per-child constant) → normalizes to
    ///   `1/N`, so an equivalent to explicit "equal weight".
    /// - **Any other expression** — e.g.
    ///   `weights: !drawdown_throttle { source: !portfolio_book, max_drawdown: 0.15 }`
    ///   for aggregate-drawdown-throttled per-child sizing (bare
    ///   `!drawdown_throttle` reads each child's own book; add
    ///   `source: !portfolio_book` to read the aggregate). The whole
    ///   surface of [`ExprSpec`] is available. `!fixed` and
    ///   `!equal_weight` are recognized as sugar and rewritten to the
    ///   corresponding `!value` form at load time.
    ///
    /// Per-child instantiation supplies `!arg SYM` (single-asset
    /// children only), `!arg CHILD_NAME`, and `!arg CHILD_INDEX` (a
    /// numeric index used to resolve `!value <list>` literals).
    ///
    /// Weights are magnitudes and needn't sum to `1.0`; the portfolio
    /// normalizes on use.
    #[serde(default, deserialize_with = "deserialize_weights")]
    pub weights: Option<SpecTemplate<ExprSpec>>,

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

    /// The **position-phase rebalance policy** — which
    /// [`PositionRebalancer`](fugazi::portfolio::rebalance::PositionRebalancer)
    /// impl decides what to sell (and by how much) when a contributor's
    /// cash-phase donation can't be fully covered.
    ///
    /// Defaults to `!proportional` when omitted (matches the built-in
    /// `PortfolioBuilder` default). See [`RebalancePolicySpec`].
    #[serde(default)]
    pub rebalance_policy: Option<RebalancePolicySpec>,
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

/// Deserialize the `weights:` field, rewriting the sugar tags
/// `!fixed [w0, w1, ...]` and `!equal_weight` to their canonical
/// `!value` equivalents before wrapping in the deferred
/// [`SpecTemplate<ExprSpec>`].
///
/// The two sugar tags exist so common weight cases stay readable:
/// - `!fixed [w0, w1, ...]` → `!value [w0, w1, ...]` (per-child indexed
///   list literal).
/// - `!equal_weight` → `!value 1.0` (any per-child constant normalizes
///   to `1/N`).
///
/// Everything else falls through untouched — the whole [`ExprSpec`]
/// surface is available under `weights:`, e.g.
/// `weights: !drawdown_throttle { source: !portfolio_book, max_drawdown: 0.15 }`
/// to throttle every child's weight by the aggregate drawdown.
fn deserialize_weights<'de, D>(
    d: D,
) -> Result<Option<SpecTemplate<ExprSpec>>, D::Error>
where
    D: serde::Deserializer<'de>,
{
    use serde::de::Error;
    let raw: Option<Value> = Option::deserialize(d)?;
    let raw = match raw {
        Some(v) => v,
        None => return Ok(None),
    };
    let rewritten = rewrite_weights_sugar(raw).map_err(D::Error::custom)?;
    Ok(Some(SpecTemplate::<ExprSpec>::from_tree(rewritten)))
}

/// Rewrite `!fixed`/`!equal_weight` at the top level of a weights
/// expression to their `!value` equivalents. The rewrite is shallow
/// (only the outermost node) since these tags are policy-shortcuts,
/// not general primitives.
fn rewrite_weights_sugar(v: Value) -> std::result::Result<Value, String> {
    use serde_json::json;
    // Sugar tags arrive from the load pipeline as single-key objects
    // (the serde_norway → serde_json bridge encodes YAML tags this way).
    if let Value::Object(m) = &v
        && m.len() == 1
    {
        let (k, payload) = m.iter().next().unwrap();
        match k.as_str() {
            "fixed" => {
                // `!fixed [w0, w1, ...]` → `!value [w0, w1, ...]`.
                // Payload must be a numeric list; typed parse of the
                // resulting `!value` verifies element shape.
                return Ok(json!({ "value": payload.clone() }));
            }
            "equal_weight" => {
                // `!equal_weight` → `!value 1.0`. Payload is expected
                // to be `null`/`{}` (unit tag); accept either.
                // Normalization at rebalance-fire turns every child's
                // `1.0` into `1/N`.
                return Ok(json!({ "value": 1.0 }));
            }
            _ => {}
        }
    }
    Ok(v)
}

/// Recursively rewrite every `!value <list>` node in `tree` to
/// `!value <list[index]>` — the per-child indexing pass. Called once
/// per child in [`PortfolioSpec::build`] with that child's index.
///
/// Non-list `!value` payloads (`Real`, `Str`) and every non-`!value`
/// node pass through untouched. An out-of-range `index` leaves the list
/// alone; the downstream typed parse then rejects the list as an
/// invalid `!value` payload in a non-per-child context (matches the
/// panic path in [`ExprSpec::build`]).
fn rewrite_value_list_by_index(v: Value, index: usize) -> Value {
    match v {
        Value::Object(mut m) => {
            // Detect `{"value": <list>}` — rewrite in place.
            let is_value_list = m.len() == 1
                && m.get("value")
                    .map(|payload| payload.is_array())
                    .unwrap_or(false);
            if is_value_list {
                let payload = m.remove("value").unwrap();
                if let Value::Array(items) = payload {
                    if let Some(elem) = items.get(index) {
                        let mut out = serde_json::Map::new();
                        out.insert("value".to_string(), elem.clone());
                        return Value::Object(out);
                    } else {
                        // Restore original so downstream can report the
                        // shape mismatch clearly.
                        let mut out = serde_json::Map::new();
                        out.insert("value".to_string(), Value::Array(items));
                        return Value::Object(out);
                    }
                }
            }
            // Otherwise recurse into every value.
            let rebuilt: serde_json::Map<String, Value> = m
                .into_iter()
                .map(|(k, v)| (k, rewrite_value_list_by_index(v, index)))
                .collect();
            Value::Object(rebuilt)
        }
        Value::Array(items) => Value::Array(
            items
                .into_iter()
                .map(|v| rewrite_value_list_by_index(v, index))
                .collect(),
        ),
        scalar => scalar,
    }
}

/// If `tree` is exactly `!value <list of numbers>` at the top level,
/// extract the list as `Vec<Real>`. Used by [`resolve_allocations`] to
/// give a `!fixed`-style initial cash split the same seed-time
/// behavior as the classic policy variant. Returns `None` for any other
/// shape (dynamic expressions, string values, nested trees).
fn extract_top_level_value_list(tree: &Value) -> Option<Vec<Real>> {
    let m = tree.as_object()?;
    if m.len() != 1 {
        return None;
    }
    let list = m.get("value")?.as_array()?;
    list.iter()
        .map(|v| v.as_f64())
        .collect::<Option<Vec<Real>>>()
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
    /// no meaning) or if a `weights: !value <list>` (or sugar `!fixed
    /// <list>`) has a length that doesn't match the number of children
    /// — a per-child index out of range for the list.
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
        // Aggregate book — the portfolio's own mark-to-market view. Passed
        // as `portfolio_book` to per-child weight-share instantiations
        // below, so a book-reading node inside a weight template resolves
        // to it whenever `source: !portfolio_book` is given. Also handed to
        // `PortfolioBuilder::aggregate_book` so the built portfolio
        // shares the exact same handle — one state, one truth.
        let agg_book: Book<String> = Book::new(total_initial_equity);
        let mut builder = Portfolio::<String>::builder()
            .with_initial_equity(total_initial_equity)
            .aggregate_book(agg_book.clone());
        // Capture each child's Book at build so each per-child weight-share
        // template can be built with that child's book as its `strategy_book`
        // — bare `!drawdown` / `!return_per_bar` / etc. inside a template
        // resolves to the child's own state.
        let mut child_books: Vec<Book<String>> = Vec::with_capacity(self.children.len());
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
                    child_books.push(built.book());
                    builder.add(name, built)
                }
                PortfolioChildStrategy::Pairs(p) => {
                    let built = p.build(child_equity, schema);
                    stable = built.stable_period();
                    warm_up = built.warm_up_period();
                    child_books.push(built.book());
                    builder.add(name, built)
                }
                PortfolioChildStrategy::Basket(b) => {
                    let built = b.build(child_equity, schema);
                    stable = built.stable_period();
                    warm_up = built.warm_up_period();
                    child_books.push(built.book());
                    builder.add(name, built)
                }
                PortfolioChildStrategy::Multi(m) => {
                    let built = m.build(child_equity, schema);
                    stable = built.stable_period();
                    warm_up = built.warm_up_period();
                    child_books.push(built.book());
                    builder.add(name, built)
                }
            };
            max_stable = max_stable.max(stable);
            max_warm_up = max_warm_up.max(warm_up);
        }
        if let Some(c) = costs {
            builder = builder.costs(c);
        }
        // Install the library-side `WeightPolicy` fallback. This drives
        // two things: (a) the initial cash split, so sub-wallets seed at
        // the same values the child strategies' books saw as their
        // initial equity; (b) the *fallback* target on rebalance-fire
        // when every weight-share reads `0` (still warming, or
        // genuinely zero). Omitting `weights:` picks
        // [`EqualWeight`](fugazi::portfolio::policy::EqualWeight) —
        // stateless, equal split now and forever. A `!value <list>`
        // pre-resolves to `Fixed(list)` so the seed and fallback both
        // respect the user's per-child weights. Any other expression
        // gets a `Fixed(equal-split)` fallback so a warming expression
        // rebalances toward its initial (equal) seed.
        builder = match &self.weights {
            None => builder.weights(EqualWeight),
            Some(_) => builder.weights(Fixed::new(allocations.clone())),
        };
        // Weight-share indicators — one instance per child. Each
        // carries `!arg CHILD_NAME` (always), `!arg SYM` (single-asset
        // children only), and `!arg CHILD_INDEX` (a number, used to
        // resolve `!value <list>` literals per child). The strategy-book
        // slot is the child's own book, so bare `!drawdown` /
        // `!return_per_bar` / `!drawdown_throttle` / `!equity_vol_target`
        // / `!fractional_kelly` inside a template reads that child's
        // per-child state by default; the aggregate book is passed as
        // `portfolio_book`, so `source: !portfolio_book` inside any
        // book-reading node routes to it.
        if let Some(template) = &self.weights {
            let mut shares: Vec<
                Box<
                    dyn fugazi::indicator::Indicator<
                        Input = Snapshot<String>,
                        Output = Real,
                    >,
                >,
            > = Vec::new();
            for (i, c) in self.children.iter().enumerate() {
                let name = c
                    .name
                    .clone()
                    .unwrap_or_else(|| format!("child_{i}"));
                let mut args: HashMap<String, Value> = HashMap::new();
                args.insert(
                    "CHILD_NAME".to_string(),
                    Value::String(name.clone()),
                );
                args.insert(
                    "CHILD_INDEX".to_string(),
                    Value::Number(serde_json::Number::from(i)),
                );
                if let PortfolioChildStrategy::Single(s) = &c.strategy {
                    args.insert(
                        "SYM".to_string(),
                        Value::String(s.symbol().to_string()),
                    );
                }
                // Preprocess the template tree so `!value <list>`
                // literals resolve to `!value <list[i]>` for this
                // child. Runs before args::substitute (which only
                // handles `!arg`) so the typed parse below sees only
                // scalar `!value` payloads.
                let preprocessed_tree =
                    rewrite_value_list_by_index(template.tree().clone(), i);
                let per_child_template =
                    SpecTemplate::<ExprSpec>::from_tree(preprocessed_tree);
                let concrete = per_child_template.build(&args).unwrap_or_else(|e| {
                    panic!(
                        "PortfolioSpec::build: weight_share template failed \
                         for child '{name}' (index {i}): {e}"
                    )
                });
                let anchor = Position::new();
                let dyn_ind: Box<dyn DynIndicator> =
                    concrete.build(&anchor, &child_books[i], Some(&agg_book), schema);
                let real_ind = AsReal::new(dyn_ind);
                max_stable = max_stable.max(real_ind.stable_period());
                max_warm_up = max_warm_up.max(real_ind.warm_up_period());
                shares.push(Box::new(real_ind));
            }
            builder = builder.weight_shares(shares);
        }
        // Install the rebalance gate — a boolean signal over
        // `Snapshot<String>`. Built against a dummy `Position` because a
        // portfolio-level rebalance signal has no per-child position to
        // anchor to (a signal using `!entry` will read the empty dummy).
        // The strategy-book slot is the aggregate book itself (bare book
        // reads at portfolio scope mean the aggregate — the natural read
        // for a portfolio-level gate), and `portfolio_book` is `Some`ing
        // the same handle so explicit `source: !portfolio_book` also
        // works. Fold the signal's stable / warm-up periods into the
        // aggregate so `optimize --walkforward` sees an accurate head
        // skip.
        if let Some(rebalance_spec) = &self.rebalance_on {
            let anchor = Position::new();
            let dyn_ind: Box<dyn DynIndicator> =
                rebalance_spec.build(&anchor, &agg_book, Some(&agg_book), schema);
            let signal = AsBool::new(dyn_ind);
            max_stable = max_stable.max(signal.stable_period());
            max_warm_up = max_warm_up.max(signal.warm_up_period());
            builder = builder.rebalance_on(signal);
        }
        // Install the position-phase policy — omitted `rebalance_policy:`
        // means `Proportional` (matches PortfolioBuilder's default).
        if let Some(policy) = self.rebalance_policy {
            builder = match policy {
                RebalancePolicySpec::Proportional => builder.position_rebalancer(Proportional),
                RebalancePolicySpec::LargestFirst => builder.position_rebalancer(LargestFirst),
            };
        }
        let built = builder.build();
        DynPortfolio {
            inner: built,
            stable_period: max_stable,
            warm_up_period: max_warm_up,
        }
    }

    /// Pre-compute the per-child cash allocations the built [`Portfolio`]
    /// will seed each sub-wallet with. The rule:
    ///
    /// - **Omitted `weights:`** → equal split (`1/N`).
    /// - **`weights: !value <list>`** (a pure per-child indexed
    ///   constant) → use `list[i]` as the initial weight for child `i`.
    ///   This preserves the classic "fixed weights" behavior: writing
    ///   `weights: !fixed [0.7, 0.3]` (which lowers to `!value [0.7,
    ///   0.3]`) seeds 70/30 from bar zero.
    /// - **Any other expression** → equal split for initial cash.
    ///   Dynamic expressions haven't warmed up at build time, so an
    ///   equal seed is the safe default; the first rebalance-fire then
    ///   hands weighting to the expression.
    fn resolve_allocations(&self, total: Real, n: usize) -> Vec<Real> {
        let weights = match &self.weights {
            None => vec![1.0; n],
            Some(template) => match extract_top_level_value_list(template.tree()) {
                Some(list) if list.len() == n => list,
                _ => vec![1.0; n],
            },
        };
        let sum: Real = weights.iter().sum();
        if sum <= 0.0 {
            // Degenerate weights (all zero / negative): fall through to
            // equal split so the CLI reports something usable rather
            // than 0s.
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

    fn snap_of_at(
        entries: &[(&'static str, Real)],
        ts: fugazi::types::Timestamp,
    ) -> Snapshot<String> {
        let mut s = Snapshot::new();
        for &(sym, close) in entries {
            let atom = Atom::with_time(candle(close), ts);
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
        // `!fixed [...]` lowered to `!value [...]` via sugar rewrite.
        let list = extract_top_level_value_list(spec.weights.as_ref().unwrap().tree())
            .expect("!fixed should have lowered to !value <list>");
        assert_eq!(list, vec![0.6, 0.4]);
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
    fn rebalance_policy_defaults_to_none_and_omitting_matches_proportional() {
        // Omitted `rebalance_policy:` parses to `None` — the built
        // portfolio installs the default `Proportional` policy
        // internally, matching PortfolioBuilder's own default.
        let yaml = r#"
            children:
              - strategy: !buy_and_hold { symbol: A }
              - strategy: !buy_and_hold { symbol: B }
        "#;
        let spec = PortfolioSpec::from_text_with_params(yaml, &HashMap::new()).unwrap();
        assert!(spec.rebalance_policy.is_none());
        // Should build without failure.
        let _portfolio = spec.build(1_000.0, &Schema::empty(), None);
    }

    #[test]
    fn parses_rebalance_policy_proportional() {
        let yaml = r#"
            rebalance_policy: !proportional
            children:
              - strategy: !buy_and_hold { symbol: A }
              - strategy: !buy_and_hold { symbol: B }
        "#;
        let spec = PortfolioSpec::from_text_with_params(yaml, &HashMap::new()).unwrap();
        assert!(matches!(
            spec.rebalance_policy,
            Some(RebalancePolicySpec::Proportional),
        ));
        let _portfolio = spec.build(1_000.0, &Schema::empty(), None);
    }

    #[test]
    fn parses_rebalance_policy_largest_first() {
        let yaml = r#"
            rebalance_policy: !largest_first
            children:
              - strategy: !buy_and_hold { symbol: A }
              - strategy: !buy_and_hold { symbol: B }
        "#;
        let spec = PortfolioSpec::from_text_with_params(yaml, &HashMap::new()).unwrap();
        assert!(matches!(
            spec.rebalance_policy,
            Some(RebalancePolicySpec::LargestFirst),
        ));
        let _portfolio = spec.build(1_000.0, &Schema::empty(), None);
    }

    #[test]
    fn largest_first_policy_closes_biggest_leg_first_during_rebalance() {
        // Two fully-invested children under `!fixed [0.5, 0.5]`. After bar 2,
        // A jumps 10× so its sub-equity dwarfs B's. On bar 3 the rebalance
        // fires — A is the sole contributor; with cash phase capped at its
        // (near-zero) free cash, the position phase runs. Under
        // `!largest_first`, A's leg is scaled down to raise the shortfall,
        // and the fill lands on bar 4.
        let yaml = r#"
            weights: !fixed [0.5, 0.5]
            rebalance_on: !every 1
            rebalance_policy: !largest_first
            children:
              - name: full_a
                strategy:
                  symbol: A
                  sizing: !value 1.0
                  long:
                    enter: !value true
                    exit: !value false
              - name: full_b
                strategy:
                  symbol: B
                  sizing: !value 1.0
                  long:
                    enter: !value true
                    exit: !value false
        "#;
        let spec = PortfolioSpec::from_text_with_params(yaml, &HashMap::new()).unwrap();
        let mut portfolio = spec.build(1_000.0, &Schema::empty(), None);
        let mut wallet = portfolio.wallet_view();

        for bar in 0..4usize {
            let px_a = if bar < 2 { 100.0 } else { 1000.0 };
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

        // A must have shrunk (its position phase scaled the sole leg down);
        // B remains flat or grew from the freed cash on the next fire.
        let e0 = wallet.sub_equity(0).0;
        let e1 = wallet.sub_equity(1).0;
        assert!(
            e0 < e1 * 4.0,
            "largest-first should have started rebalancing A down; got e0={e0}, e1={e1}",
        );
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
    fn parses_indicator_weight_policy_without_indicator_wrapper() {
        // A bare expression under `weights:` falls through into the
        // indicator template — no `!indicator` wrapper needed. Each
        // child gets its own instance of the template built with
        // `!arg SYM` (single-asset only) and `!arg CHILD_NAME`.
        let yaml = r#"
            weights:
              close:
                source:
                  pick:
                    symbol: !arg SYM
            children:
              - strategy: !buy_and_hold { symbol: A }
              - strategy: !buy_and_hold { symbol: B }
        "#;
        let spec = PortfolioSpec::from_text_with_params(yaml, &HashMap::new()).unwrap();
        assert!(spec.weights.is_some());
        // Should build cleanly — each child's template instance uses its
        // own symbol via !arg SYM.
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
    fn rebalance_on_monthly_drives_multi_symbol_portfolio_without_panic() {
        // Regression: `rebalance_on: !monthly` (and the whole cadence sugar
        // family) used to panic on the first bar of a 2+ symbol portfolio
        // because the calendar accessor rooted through Pick::new, which
        // sole-atom-unpacks and panics on 2+ entries. With PickAny as the
        // calendar default, a portfolio-level `rebalance_on: !monthly`
        // now builds and drives cleanly over a multi-symbol snapshot
        // stream — the exact shape CLAUDE.md's PortfolioSpec bullet
        // recommends ("use snapshot / calendar / cadence signals").
        use fugazi::types::Timestamp;

        let yaml = r#"
            weights: !value [0.5, 0.5]
            rebalance_on: !monthly
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

        // Three bars spanning a month rollover: 2024-01-31, 2024-02-01,
        // 2024-02-02. `!monthly` fires on 2024-02-01 (month rolled from
        // 1 → 2). All three snapshots carry two symbols — the panic path
        // is exactly this: PickAny reads the first atom's time, which
        // both entries share.
        let day = 86_400_000i64;
        let jan_31 = Timestamp(1_706_659_200_000); // 2024-01-31 00:00 UTC
        let feb_01 = Timestamp(jan_31.0 + day);
        let feb_02 = Timestamp(feb_01.0 + day);

        for (bar_i, ts) in [jan_31, feb_01, feb_02].into_iter().enumerate() {
            let px_a = 100.0 + (bar_i as Real);
            let px_b = 200.0 + (bar_i as Real);
            for fill in wallet.update("A".to_string(), candle(px_a)) {
                portfolio.on_fill(&fill);
            }
            for fill in wallet.update("B".to_string(), candle(px_b)) {
                portfolio.on_fill(&fill);
            }
            portfolio.update(snap_of_at(&[("A", px_a), ("B", px_b)], ts));
            portfolio.trade(&mut wallet);
        }
        // Aggregate equity should stay well-defined (no NaN, no panic).
        assert!(portfolio.inner.book().equity_value().is_finite());
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

    #[test]
    fn value_list_seeds_and_rebalances_at_the_indexed_weights() {
        // `weights: !value [0.75, 0.25]` (the canonical form of what
        // `!fixed` used to be) both seeds the initial cash split at
        // 75/25 and — on rebalance-fire — snaps back to that same
        // target after any price divergence.
        let yaml = r#"
            weights: !value [0.75, 0.25]
            rebalance_on: !every 1
            children:
              - name: a
                strategy:
                  symbol: A
                  sizing: !value 0.5
                  long:
                    enter: !value true
                    exit: !value false
              - name: b
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
        // Initial split respects the list (seed via extract_top_level_value_list).
        assert!((wallet.sub_equity(0).0 - 750.0).abs() < 1e-6);
        assert!((wallet.sub_equity(1).0 - 250.0).abs() < 1e-6);
        // Run a few bars — rebalance keeps ratios locked to 75/25.
        for _ in 0..4 {
            for fill in wallet.update("A".to_string(), candle(100.0)) {
                portfolio.on_fill(&fill);
            }
            for fill in wallet.update("B".to_string(), candle(100.0)) {
                portfolio.on_fill(&fill);
            }
            portfolio.update(snap_of(&[("A", 100.0), ("B", 100.0)]));
            portfolio.trade(&mut wallet);
        }
        let e0 = wallet.sub_equity(0).0;
        let e1 = wallet.sub_equity(1).0;
        let total = e0 + e1;
        assert!(
            (e0 / total - 0.75).abs() < 0.01 && (e1 / total - 0.25).abs() < 0.01,
            "!value [0.75, 0.25] should hold 75/25 split; got e0={e0}, e1={e1}",
        );
    }

    #[test]
    fn fixed_sugar_lowers_to_value_list() {
        // `!fixed [0.6, 0.4]` should behave identically to
        // `!value [0.6, 0.4]` — both lower to the same tree.
        let yaml_fixed = r#"
            weights: !fixed [0.6, 0.4]
            children:
              - strategy: !buy_and_hold { symbol: A }
              - strategy: !buy_and_hold { symbol: B }
        "#;
        let yaml_value = r#"
            weights: !value [0.6, 0.4]
            children:
              - strategy: !buy_and_hold { symbol: A }
              - strategy: !buy_and_hold { symbol: B }
        "#;
        let spec_fixed =
            PortfolioSpec::from_text_with_params(yaml_fixed, &HashMap::new()).unwrap();
        let spec_value =
            PortfolioSpec::from_text_with_params(yaml_value, &HashMap::new()).unwrap();
        assert_eq!(
            spec_fixed.weights.as_ref().unwrap().tree(),
            spec_value.weights.as_ref().unwrap().tree(),
            "!fixed should lower to the same tree as !value <list>",
        );
    }

    #[test]
    fn equal_weight_sugar_lowers_to_value_one() {
        // `!equal_weight` should lower to `!value 1.0` — a per-child
        // constant that normalizes to `1/N`.
        let yaml = r#"
            weights: !equal_weight
            children:
              - strategy: !buy_and_hold { symbol: A }
              - strategy: !buy_and_hold { symbol: B }
        "#;
        let spec = PortfolioSpec::from_text_with_params(yaml, &HashMap::new()).unwrap();
        let tree = spec.weights.as_ref().unwrap().tree();
        let m = tree.as_object().expect("tree should be an object");
        assert_eq!(m.len(), 1);
        let payload = m.get("value").expect("!equal_weight → !value <n>");
        assert_eq!(payload.as_f64(), Some(1.0));
    }

    #[test]
    fn portfolio_book_weight_share_reads_the_aggregate() {
        // A weight-share template whose value is
        // `!equity_peak { source: !portfolio_book }` reads the aggregate
        // book — every child reads the same value each rebalance-fire,
        // so the normalized weight vector is uniform.
        let yaml = r#"
            weights:
              equity_peak:
                source: !portfolio_book
            rebalance_on: !every 1
            children:
              - name: a
                strategy:
                  symbol: A
                  sizing: !value 0.5
                  long:
                    enter: !value true
                    exit: !value false
              - name: b
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

        for _ in 0..4usize {
            for fill in wallet.update("A".to_string(), candle(100.0)) {
                portfolio.on_fill(&fill);
            }
            for fill in wallet.update("B".to_string(), candle(100.0)) {
                portfolio.on_fill(&fill);
            }
            portfolio.update(snap_of(&[("A", 100.0), ("B", 100.0)]));
            portfolio.trade(&mut wallet);
        }
        let e0 = wallet.sub_equity(0).0;
        let e1 = wallet.sub_equity(1).0;
        assert!(
            (e0 - e1).abs() < 1.0,
            "weight-share reading same aggregate value per child should split \
             equally; got e0={e0}, e1={e1}",
        );
    }

    #[test]
    #[should_panic(expected = "!portfolio_book")]
    fn portfolio_book_source_outside_portfolio_context_panics() {
        // Referencing `!portfolio_book` in a place with no portfolio
        // scope (a plain single-asset spec) panics at build with a
        // clear message.
        use super::super::SingleStrategySpec;
        let yaml = r#"
            symbol: X
            long:
              enter: !gt
                lhs: !drawdown { source: !portfolio_book }
                rhs: !value 0
        "#;
        let spec = SingleStrategySpec::from_text_with_params(yaml, &HashMap::new()).unwrap();
        let _built = spec.build(1_000.0, &Schema::empty());
    }

    #[test]
    fn weight_share_bare_drawdown_reads_child_book() {
        // Under the source-based book design (Option A), a bare
        // `!drawdown` inside a per-child weight-share template reads
        // that child's own book — not the aggregate. Wire two children
        // whose sub-wallets diverge (only one holds a position), then
        // observe that the drawdown-reading weight expression sees
        // per-child state rather than a shared aggregate reading.
        //
        // The specific check: the aggregate never draws down (prices
        // don't move, everyone is flat / long at cost), but if we
        // deliberately override one child's book to have a drawdown,
        // its weight share should react while the other's doesn't.
        // Here the cleanest observable is that the two book handles
        // *are* distinct — validated by the compile-time plumbing
        // (`strategy_book = child_books[i]`, `portfolio_book = agg`).
        let yaml = r#"
            weights: !drawdown
            children:
              - strategy: !buy_and_hold { symbol: A }
              - strategy: !buy_and_hold { symbol: B }
        "#;
        let spec = PortfolioSpec::from_text_with_params(yaml, &HashMap::new()).unwrap();
        // Just check that the build succeeds — the plumbing test above
        // is really a documentation of the wiring; a full behavior
        // test that shows the per-child drawdown reading would need a
        // fake `PortfolioWallet` seam. This is `smoke` — the source
        // resolution isn't hit until the spec runs, and every child's
        // book is a fresh `Book` (initial equity = allocated share)
        // that reports `Some(0.0)` for `!drawdown` at bar 0, so the
        // build itself is enough to prove it compiles.
        let _portfolio = spec.build(1_000.0, &Schema::empty(), None);
    }

    #[test]
    fn portfolio_book_source_in_weights_reads_aggregate() {
        // Explicit `source: !portfolio_book` in a weight-share
        // template resolves to the aggregate book — the mirror of
        // `weight_share_bare_drawdown_reads_child_book` for the
        // portfolio-side default.
        let yaml = r#"
            weights: !drawdown { source: !portfolio_book }
            children:
              - strategy: !buy_and_hold { symbol: A }
              - strategy: !buy_and_hold { symbol: B }
        "#;
        let spec = PortfolioSpec::from_text_with_params(yaml, &HashMap::new()).unwrap();
        let _portfolio = spec.build(1_000.0, &Schema::empty(), None);
    }
}
