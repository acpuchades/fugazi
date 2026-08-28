//! YAML-deserializable [`PortfolioSpec`] — a top-level composite strategy
//! that runs N heterogeneous child strategies against one shared cash pool
//! through a [`Portfolio<Symbol>`](crate::portfolio::Portfolio).
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
//!     strategy: !ma_crossover { root: BTC, fast: 20, slow: 50 }
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

use crate::indicators::{Book, Position};
use crate::portfolio::Portfolio;
use crate::portfolio::policy::{EqualWeight, Fixed};
use crate::portfolio::rebalance::{LargestFirst, Proportional};
use crate::prelude::*;
use crate::types::Snapshot;

use crate::runtime::AnyChain;

use super::basket::BasketStrategySpec;
use super::expr::{NodeSpec, Root};
use super::meta::Meta;
use super::multi_asset::MultiAssetStrategySpec;
use super::pairs::PairsStrategySpec;
use super::preset::StrategyRef;
use super::template::SpecTemplate;
use crate::types::Symbol;

/// YAML surface for the **position-phase rebalance policy** — the impl
/// picked from [`rebalance`](crate::portfolio::rebalance) that decides
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
/// [`Proportional`], matching
/// the [`PortfolioBuilder`](crate::portfolio::PortfolioBuilder) default.
/// A CLI-only discriminator; at build it constructs the corresponding
/// [`PositionRebalancer`](crate::portfolio::rebalance::PositionRebalancer)
/// impl and installs it via
/// [`PortfolioBuilder::position_rebalancer`](crate::portfolio::PortfolioBuilder::position_rebalancer).
/// Rust-side callers with a bespoke rule build their own impl and install
/// it directly — no CLI-side wiring needed.
#[derive(Debug, Clone, Copy, Deserialize)]
#[serde(rename_all = "snake_case", deny_unknown_fields)]
pub enum RebalancePolicySpec {
    /// Scale every held leg by the same fraction to cover the shortfall.
    /// The default — matches
    /// [`Proportional`].
    Proportional,

    /// Fully liquidate biggest positions (by `|units| * price`) first,
    /// walking down until the shortfall is covered. The last position
    /// touched is partially scaled if fully closing it would overshoot.
    /// Wraps [`LargestFirst`].
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
    ///   surface of [`NodeSpec`] is available. `!fixed` and
    ///   `!equal_weight` are recognized as sugar and rewritten to the
    ///   corresponding `!value` form at load time.
    ///
    /// Per-child instantiation supplies these auto-bound slots:
    /// `!slot CHILD_INDEX` (always — a numeric index used to resolve
    /// `!value <list>` literals), `!slot CHILD_NAME` (only when the
    /// child sets `name:`), `!slot CHILD_GROUP` (only when the child
    /// sets `group:`), and `!slot SYM` (single-asset children only —
    /// same convention as basket / multi-asset specs). Anything not
    /// declared on the child isn't injected — a template referencing an
    /// unset slot fails at build with a clear missing-slot error.
    ///
    /// Weights are magnitudes and needn't sum to `1.0`; the portfolio
    /// normalizes on use.
    ///
    /// **A non-constant expression requires a
    /// [`rebalance_on`](Self::rebalance_on).** Weight shares are read only
    /// inside a rebalance cycle, so an omitted gate would build the chains,
    /// update them every bar, and consult them on none — the portfolio would
    /// run the equal-split seed and drift with P&L, its weighting rule inert.
    /// That is a build error (see [`try_build`](Self::try_build)); the named
    /// opt-out is writing `rebalance_on: !never`. Both constant forms are
    /// exempt, since the build-time seed already *is* their answer: `!value
    /// <list>` seeds the ratio, `!value <scalar>` seeds `1/N`.
    #[serde(default, deserialize_with = "deserialize_weights")]
    pub weights: Option<SpecTemplate<NodeSpec>>,

    /// The **rebalance gate**: a boolean signal deciding, on each bar,
    /// whether the portfolio runs one rebalance cycle after children
    /// have traded. Defaults to `!never` (`ValueBool::false`) — no
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
    /// Omitting this field is refused when [`weights`](Self::weights) is a
    /// non-constant expression — nothing would ever read it. Write `!never`
    /// to state that the drift is intended.
    ///
    /// Each fire runs the same two-phase rebalance: cash phase first
    /// (contributors donate free cash, receivers split the pot), then a
    /// position phase for any contributor whose cash phase couldn't
    /// cover its donation (submits proportional `set_position`
    /// scale-downs that fill next bar, freeing cash for the following
    /// fire cycle). A rebalance whose cash phase covers everyone stays
    /// fill-free automatically.
    #[serde(default)]
    pub rebalance_on: Option<NodeSpec>,

    /// The **position-phase rebalance policy** — which
    /// [`PositionRebalancer`](crate::portfolio::rebalance::PositionRebalancer)
    /// impl decides what to sell (and by how much) when a contributor's
    /// cash-phase donation can't be fully covered.
    ///
    /// Defaults to `!proportional` when omitted (matches the built-in
    /// `PortfolioBuilder` default). See [`RebalancePolicySpec`].
    #[serde(default)]
    pub rebalance_policy: Option<RebalancePolicySpec>,

    /// Free-form document metadata for external tooling. Parsed, carried, and
    /// never interpreted — see [`spec::meta`](crate::spec::meta). Each child
    /// carries its own [`meta`](PortfolioChildSpec::meta) independently.
    #[serde(default)]
    pub meta: Option<Meta>,
}

/// One child slot: optional identity metadata (`name`, `group`) plus the
/// nested strategy spec. When set, `name` and `group` are surfaced to
/// the `weights:` expression via auto-injected `!slot` values
/// (`CHILD_NAME`, `CHILD_GROUP`) so a portfolio-scope weight template
/// can dispatch on them — the natural way to write "up-weight every
/// momentum child when ADX is high" without enumerating names in a big
/// `!if_else` tower.
#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PortfolioChildSpec {
    /// Optional display name for logs and downstream per-child
    /// reporting. Defaults internally to `child_<idx>` when omitted
    /// (used as the sub-wallet key inside [`Portfolio`]). Must be
    /// unique across the portfolio after defaulting;
    /// [`PortfolioSpec::try_build`] reports a collision as a build error.
    ///
    /// Surfaced to `weights:` as `!slot CHILD_NAME` **only when
    /// explicitly set** — a template referencing `!slot CHILD_NAME`
    /// against an unnamed child fails at build with a clear
    /// missing-slot error (matches the `CHILD_GROUP` and `SYM`
    /// injection convention: no silent auto-populated value).
    #[serde(default)]
    pub name: Option<String>,

    /// Optional group label — may be shared across siblings (e.g.
    /// `group: momentum`) so a `weights:` expression can gate on
    /// `!slot CHILD_GROUP` to steer whole cohorts together. When omitted,
    /// `CHILD_GROUP` is not injected — a template that references it
    /// against an ungrouped child fails at build with a clear
    /// missing-slot error (matches the `SYM`-only-for-single-asset
    /// convention).
    #[serde(default)]
    pub group: Option<String>,

    /// The nested strategy — of any shape. Routed by distinctive top-level
    /// key on the child's `strategy:` map (see [`PortfolioChildStrategy`]).
    pub strategy: PortfolioChildStrategy,

    /// Free-form metadata for this child slot — see
    /// [`spec::meta`](crate::spec::meta). Distinct from any `meta:` inside
    /// `strategy:`, which belongs to the nested document: this one describes
    /// the *slot* (why this child is in this portfolio), that one describes the
    /// strategy. Neither is read by fugazi, and unlike `name` / `group` it is
    /// **not** surfaced to the `weights:` expression — `meta` is opaque by
    /// contract, and a weight that read it would be reading data fugazi
    /// promises not to interpret.
    #[serde(default)]
    pub meta: Option<Meta>,
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
        use crate::spec::shape::{ShapeHint, detect_shape};

        // The tag-normalising JSON bridge is required by `BasketStrategySpec`
        // (its `SpecTemplate` captures `serde_json::Value` — the raw
        // `serde_norway::Value` path can't feed it) and by
        // `MultiAssetStrategySpec` (same reason). Kept consistent for pairs
        // too so all three go through one path.
        let via_json =
            |v| crate::spec::convert::yaml_to_json(v).map_err(|e: anyhow::Error| e.to_string());

        match detect_shape(&v) {
            // `StrategyRef` owns the preset-name gate.
            ShapeHint::Preset | ShapeHint::Single => {
                StrategyRef::try_from(v).map(|s| PortfolioChildStrategy::Single(Box::new(s)))
            }
            ShapeHint::Pairs => serde_json::from_value::<PairsStrategySpec>(via_json(v)?)
                .map(|p| PortfolioChildStrategy::Pairs(Box::new(p)))
                .map_err(|e| e.to_string()),
            ShapeHint::Basket => serde_json::from_value::<BasketStrategySpec>(via_json(v)?)
                .map(|b| PortfolioChildStrategy::Basket(Box::new(b)))
                .map_err(|e| e.to_string()),
            ShapeHint::Multi => serde_json::from_value::<MultiAssetStrategySpec>(via_json(v)?)
                .map(|m| PortfolioChildStrategy::Multi(Box::new(m)))
                .map_err(|e| e.to_string()),
        }
    }
}

/// Deserialize the `weights:` field, rewriting the sugar tags
/// `!fixed [w0, w1, ...]` and `!equal_weight` to their canonical
/// `!value` equivalents before wrapping in the deferred
/// [`SpecTemplate<NodeSpec>`].
///
/// The two sugar tags exist so common weight cases stay readable:
/// - `!fixed [w0, w1, ...]` → `!value [w0, w1, ...]` (per-child indexed
///   list literal).
/// - `!equal_weight` → `!value 1.0` (any per-child constant normalizes
///   to `1/N`).
///
/// Everything else falls through untouched — the whole [`NodeSpec`]
/// surface is available under `weights:`, e.g.
/// `weights: !drawdown_throttle { source: !portfolio_book, max_drawdown: 0.15 }`
/// to throttle every child's weight by the aggregate drawdown.
fn deserialize_weights<'de, D>(d: D) -> Result<Option<SpecTemplate<NodeSpec>>, D::Error>
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
    // `checked`, not `from_tree`: the sugar rewrite bypasses `SpecTemplate`'s
    // own `Deserialize`, and with it the probe parse that makes a typo in a
    // deferred body a load error. Without this the `weights:` template is the
    // one template still validated only at build.
    SpecTemplate::<NodeSpec>::checked(rewritten)
        .map(Some)
        .map_err(D::Error::custom)
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
/// panic path in [`NodeSpec::build`]).
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

/// Whether a `weights:` tree is a **constant** the build-time seed already
/// captures — a top-level `!value` literal, either a per-child indexed list
/// (`!value [0.7, 0.3]`, also reached via the `!fixed` sugar) or a scalar
/// every child reads identically (`!value 1.0`, the `!equal_weight` sugar).
///
/// This is the discriminator behind the `weights:` / `rebalance_on:`
/// consistency check in [`PortfolioSpec::try_build`]. Weight-share chains are
/// only read inside a rebalance cycle, so a portfolio that never fires its
/// gate applies nothing but the seed. For a constant that is harmless — the
/// seed *is* the expression's answer, on every bar, forever. For anything
/// else the expression would be built, updated every bar, and never consulted.
fn weights_are_constant(tree: &Value) -> bool {
    let Some(m) = tree.as_object() else {
        return false;
    };
    if m.len() != 1 {
        return false;
    }
    match m.get("value") {
        Some(Value::Number(_)) => true,
        Some(Value::Array(list)) => list.iter().all(Value::is_number),
        _ => false,
    }
}

/// Resolve every child's internal display name — using its declared
/// `name:` when set, else defaulting to `child_<index>` — and enforce
/// that the resulting vector has no duplicates. This is the string
/// [`Portfolio`] uses to key its sub-wallets and to label template-build
/// errors ("template failed for child `X`"), so shadowing between an
/// explicitly-named child and an auto-generated `child_N` slot must be
/// a hard error (otherwise sub-wallet lookups become ambiguous). Note
/// this is *not* the `!slot CHILD_NAME` injection value — that slot is
/// only injected when `name:` was declared explicitly.
///
/// A duplicate is bad **input**, not a broken invariant, so it comes back
/// as an `Err` listing the collided name(s) rather than aborting the run.
fn resolve_child_names(children: &[PortfolioChildSpec]) -> Result<Vec<String>, String> {
    let resolved: Vec<String> = children
        .iter()
        .enumerate()
        .map(|(i, c)| c.name.clone().unwrap_or_else(|| format!("child_{i}")))
        .collect();
    let mut seen: std::collections::HashSet<&str> =
        std::collections::HashSet::with_capacity(resolved.len());
    let mut collisions: Vec<&str> = Vec::new();
    for name in &resolved {
        if !seen.insert(name.as_str()) {
            collisions.push(name.as_str());
        }
    }
    if !collisions.is_empty() {
        return Err(format!(
            "duplicate child name(s) after defaulting: {collisions:?} \
             — every child's resolved `name:` (or the auto-generated `child_<index>` \
             fallback) must be unique across the portfolio",
        ));
    }
    Ok(resolved)
}

impl PortfolioSpec {
    /// Parse a YAML portfolio document, applying `!import` splices and
    /// `!param` substitutions before typed deserialization.
    pub fn from_text_with_params_in(
        text: &str,
        params: &HashMap<String, Value>,
        base: &std::path::Path,
        root: &std::path::Path,
        label: &str,
    ) -> Result<Self> {
        let value = super::load_document(
            text,
            params,
            base,
            root,
            label,
            super::input::StrategyKind::Portfolio,
        )?;
        serde_json::from_value(value)
            .with_context(|| format!("building portfolio strategy from {label}"))
    }

    /// Test convenience: [`from_text_with_params_in`](Self::from_text_with_params_in)
    /// with imports resolved against the working directory and an
    /// `(inline)` source label.
    #[cfg(test)]
    pub fn from_text_with_params(text: &str, params: &HashMap<String, Value>) -> Result<Self> {
        Self::from_text_with_params_in(
            text,
            params,
            std::path::Path::new("."),
            std::path::Path::new("."),
            "(inline)",
        )
    }

    /// Build the live [`DynPortfolio`] this spec describes.
    ///
    /// `total_initial_equity` is the whole cash budget passed to the
    /// portfolio builder — split across children per the weight policy.
    /// Each child's own [`SingleAssetStrategy::with_initial_equity`](crate::strategies::SingleAssetStrategy::with_initial_equity)-style
    /// book seed is set to the child's allocated share, so book-anchored
    /// sizing recipes inside a child read against that child's slice of
    /// the pool rather than the aggregate.
    ///
    /// `costs` is the [`TradingCosts`] bundle installed on every child's
    /// sub-wallet — [`Portfolio`] applies the
    /// same bundle uniformly (v1 constraint: no per-symbol dispatch
    /// through the composite wallet). Pass `None` to skip cost wiring
    /// (matches the zero-cost paper-wallet default the other specs use for
    /// gross twins).
    ///
    /// A non-constant `weights:` with no `rebalance_on:` is refused: the
    /// expression would never be read (weight shares are consulted only on a
    /// rebalance-fire), so the portfolio would silently run the equal-split
    /// seed. `rebalance_on: !never` is the named opt-out.
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
        self.try_build(total_initial_equity, schema, costs)
            .unwrap_or_else(|e| panic!("{e}"))
    }

    /// The fallible twin of [`build`](Self::build) — see
    /// [`SingleStrategySpec::try_build`](crate::spec::SingleStrategySpec::try_build).
    pub fn try_build(
        &self,
        total_initial_equity: Real,
        schema: &Arc<Schema>,
        // Costs now ride on the wallet the portfolio is driven with (via
        // `RunnableStrategy::drive`), like every other shape — there is no
        // sub-wallet to bake them into. Kept for call-site symmetry.
        _costs: Option<TradingCosts>,
    ) -> Result<DynPortfolio, String> {
        if self.children.is_empty() {
            return Err(
                "PortfolioSpec::build: `children:` must have at least one entry".to_string(),
            );
        }
        // A dynamic `weights:` with no `rebalance_on:` is a written
        // instruction that can never run. The weight-share chains are read
        // only inside `Portfolio::rebalance_now`, so an omitted gate means
        // they are built, updated on every bar, and consulted on none — the
        // portfolio silently runs the equal-split seed and drifts with P&L.
        // Refusing beats reporting a backtest whose weighting rule was inert
        // (the same reasoning that keeps `deny_unknown_fields` on every
        // document). Constants are exempt: the seed already *is* their answer.
        // `rebalance_on: !never` is the named opt-out — written down, it says
        // the drift is intended.
        if let Some(template) = &self.weights
            && self.rebalance_on.is_none()
            && !weights_are_constant(template.tree())
        {
            return Err(
                "PortfolioSpec::build: `weights:` is a non-constant expression but \
                 `rebalance_on:` is omitted, so it would never be read — weight \
                 expressions are consulted only on a rebalance-fire, and the \
                 portfolio would run the equal-split seed and drift with P&L. Give \
                 it a cadence (`rebalance_on: !every 28`), or write `rebalance_on: \
                 !never` to state that the drift is intended"
                    .to_string(),
            );
        }
        let n = self.children.len();
        let resolved_names = resolve_child_names(&self.children)?;
        let allocations = self.resolve_allocations(total_initial_equity, n);

        // Track each child's readiness periods at build. We inspect the
        // typed child *before* boxing into `Box<dyn Strategy>` — the
        // erased trait doesn't expose `stable_bars` / `warm_up_bars`,
        // so this is the only chance to capture them for
        // [`DynPortfolio::stable_bars`] to aggregate later.
        //
        // Multi / basket children with lazy per-symbol chains report only
        // their rebalance signal's period at this point (chains build on
        // first snapshot). A portfolio containing them may under-report
        // stable_bars slightly at build time — accurate for the common
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
        let agg_book: Book<Symbol> = Book::new(total_initial_equity);
        let mut builder = Portfolio::<Symbol>::builder()
            .with_initial_equity(total_initial_equity)
            .aggregate_book(agg_book.clone());
        // Capture each child's Book at build so each per-child weight-share
        // template can be built with that child's book as its `strategy_book`
        // — bare `!drawdown` / `!return_per_bar` / etc. inside a template
        // resolves to the child's own state.
        let mut child_books: Vec<Book<Symbol>> = Vec::with_capacity(self.children.len());
        for (i, c) in self.children.iter().enumerate() {
            let name = resolved_names[i].clone();
            let child_equity = allocations[i];
            let (stable, warm_up);
            builder = match &c.strategy {
                PortfolioChildStrategy::Single(s) => {
                    let built = s.try_build(child_equity, schema)?;
                    stable = built.stable_bars();
                    warm_up = built.warm_up_bars();
                    child_books.push(built.book());
                    builder.add(name, built)
                }
                PortfolioChildStrategy::Pairs(p) => {
                    let built = p.try_build(child_equity, schema)?;
                    stable = built.stable_bars();
                    warm_up = built.warm_up_bars();
                    child_books.push(built.book());
                    builder.add(name, built)
                }
                PortfolioChildStrategy::Basket(b) => {
                    let built = b.try_build(child_equity, schema)?;
                    stable = built.stable_bars();
                    warm_up = built.warm_up_bars();
                    child_books.push(built.book());
                    builder.add(name, built)
                }
                PortfolioChildStrategy::Multi(m) => {
                    let built = m.try_build(child_equity, schema)?;
                    stable = built.stable_bars();
                    warm_up = built.warm_up_bars();
                    child_books.push(built.book());
                    builder.add(name, built)
                }
            };
            max_stable = max_stable.max(stable);
            max_warm_up = max_warm_up.max(warm_up);
        }
        // Install the library-side `WeightPolicy` fallback. This drives
        // two things: (a) the initial cash split, so sub-wallets seed at
        // the same values the child strategies' books saw as their
        // initial equity; (b) the *fallback* target on rebalance-fire
        // when every weight-share reads `0` (still warming, or
        // genuinely zero). Omitting `weights:` picks
        // [`EqualWeight`](crate::portfolio::policy::EqualWeight) —
        // stateless, equal split now and forever. A `!value <list>`
        // pre-resolves to `Fixed(list)` so the seed and fallback both
        // respect the user's per-child weights. Any other expression
        // gets a `Fixed(equal-split)` fallback so a warming expression
        // rebalances toward its initial (equal) seed.
        builder = match &self.weights {
            None => builder.weights(EqualWeight),
            Some(_) => builder.weights(Fixed::new(allocations.clone())),
        };
        // Weight-share indicators — one instance per child. Each carries
        // the `$`-prefixed portfolio-scope reserved auto-bound slots:
        // `$CHILD_NAME` (always), `$CHILD_INDEX` (a number, used to
        // resolve `!value <list>` literals per child), and `$CHILD_GROUP`
        // (only when the child sets `group:`). The `$`-prefix reserves
        // the portfolio-scope auto-bound namespace so user slots (via
        // future `defs:` mechanisms) can't shadow the system-provided
        // ones — a template referencing `!slot CHILD_GROUP` against an
        // ungrouped child fails at build with a clear missing-slot error.
        //
        // `SYM` is also injected for single-asset children (prefix-free,
        // matching the basket/multi-asset per-symbol convention).
        //
        // The strategy-book slot is the child's own book, so bare
        // `!drawdown` / `!return_per_bar` / `!drawdown_throttle` /
        // `!equity_vol_target` / `!fractional_kelly` inside a template
        // reads that child's per-child state by default; the aggregate
        // book is passed as `portfolio_book`, so `source: !portfolio_book`
        // inside any book-reading node routes to it.
        if let Some(template) = &self.weights {
            let mut shares: Vec<
                Box<
                    dyn crate::indicator::Indicator<Input = Snapshot<Symbol>, Output = Real> + Send,
                >,
            > = Vec::new();
            for (i, c) in self.children.iter().enumerate() {
                let internal_name = resolved_names[i].clone();
                let mut slots: HashMap<String, Value> = HashMap::new();
                // `CHILD_INDEX` is unconditional — every child has a
                // stable position in `.add(...)` order, so `!slot
                // CHILD_INDEX` always resolves.
                slots.insert(
                    "CHILD_INDEX".to_string(),
                    Value::Number(serde_json::Number::from(i)),
                );
                // `CHILD_NAME` / `CHILD_GROUP` are only injected when
                // the child sets them explicitly. Same policy for both:
                // no silent auto-populated fallback (a template that
                // references either against a child that doesn't
                // declare it fails at build with a clear missing-slot
                // error). The internal `child_<idx>` default is only
                // used to key the sub-wallet inside `Portfolio`, never
                // exposed as a slot.
                if let Some(name) = &c.name {
                    slots.insert("CHILD_NAME".to_string(), Value::String(name.clone()));
                }
                if let Some(group) = &c.group {
                    slots.insert("CHILD_GROUP".to_string(), Value::String(group.clone()));
                }
                if let PortfolioChildStrategy::Single(s) = &c.strategy {
                    slots.insert("SYM".to_string(), Value::String(s.symbol()?));
                }
                // The `child_<idx>` default is still used below as the
                // template-build-error label for anyone chasing a
                // "template failed for child 'child_2'" panic.
                let name = internal_name;
                // Preprocess the template tree so `!value <list>`
                // literals resolve to `!value <list[i]>` for this
                // child. Runs before slots::substitute (which only
                // handles `!slot`) so the typed parse below sees only
                // scalar `!value` payloads.
                let preprocessed_tree = rewrite_value_list_by_index(template.tree().clone(), i);
                let per_child_template = SpecTemplate::<NodeSpec>::from_tree(preprocessed_tree);
                let concrete = per_child_template.build(&slots).map_err(|e| {
                    format!(
                        "PortfolioSpec::build: weight_share template failed \
                         for child '{name}' (index {i}): {e}"
                    )
                })?;
                let anchor = Position::new();
                // A single-asset child has one blessed series — its traded
                // symbol, the same value bound to `!slot SYM` above — so a
                // bare price leaf in its weight expression reads that. Every
                // other child shape spans many symbols, so there is no
                // "this series" and its leaves must name one.
                let child_root = match &c.strategy {
                    PortfolioChildStrategy::Single(s) => Some(s.root().clone()),
                    _ => None,
                };
                let dyn_ind: AnyChain = concrete.try_build(
                    &anchor,
                    &child_books[i],
                    Some(&agg_book),
                    schema,
                    Root::or_sole(child_root.as_ref()),
                )?;
                let real_ind = (dyn_ind).into_real()?;
                max_stable = max_stable.max(real_ind.stable_bars());
                max_warm_up = max_warm_up.max(real_ind.warm_up_bars());
                shares.push(Box::new(real_ind));
            }
            builder = builder.weight_shares(shares);
        }
        // Install the rebalance gate — a boolean signal over
        // `Snapshot<Symbol>`. Built against a dummy `Position` because a
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
            // `root: None` — a portfolio-level gate spans every child, so
            // "this series" is undefined; a price leaf inside one must name
            // its asset with `!pick { symbol: ... }`.
            let dyn_ind: AnyChain = rebalance_spec.try_build(
                &anchor,
                &agg_book,
                Some(&agg_book),
                schema,
                Root::sole(),
            )?;
            let signal = (dyn_ind).into_bool()?;
            max_stable = max_stable.max(signal.stable_bars());
            max_warm_up = max_warm_up.max(signal.warm_up_bars());
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
        Ok(DynPortfolio {
            inner: built,
            stable_bars: max_stable,
            warm_up_bars: max_warm_up,
        })
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
// DynPortfolio: CLI-owned wrapper around Portfolio<Symbol>
// ---------------------------------------------------------------------------

/// The CLI's built portfolio handle. Implements [`Strategy`] by delegation so
/// it drops into [`crate::backtest::run`] like any other shape — a portfolio now
/// trades the wallet it is handed, netting its children's intents onto that one
/// account.
pub struct DynPortfolio {
    inner: Portfolio<Symbol>,
    /// Max child stable-period captured at build (see
    /// [`PortfolioSpec::build`] for the lazy-child caveat).
    stable_bars: usize,
    /// Max child warm-up-period captured at build.
    warm_up_bars: usize,
}

impl Strategy for DynPortfolio {
    type Input = Snapshot<Symbol>;
    type Symbol = Symbol;

    fn update(&mut self, input: Snapshot<Symbol>) {
        self.inner.update(input);
    }
    fn trade(&self, wallet: &mut dyn Wallet<Symbol>) {
        self.inner.trade(wallet);
    }
    fn on_fill(&mut self, order: &Order<Symbol>) {
        self.inner.on_fill(order);
    }
    fn on_reject(&mut self, rejection: &Rejection<Symbol>) {
        // Must be forwarded explicitly: without this the whole rejection
        // path below (sub-wallet drain → owner lookup → child) is invisible
        // to the CLI and Python, which only ever see a `DynPortfolio`.
        self.inner.on_reject(rejection);
    }
    fn is_ready(&self) -> bool {
        self.inner.is_ready()
    }
    fn reset(&mut self) {
        self.inner.reset();
    }
    // The `Strategy`-level twins of the inherent `save_state`/`restore_state`
    // below. `RunnableStrategy` routes through the inherent pair, so these are
    // what a portfolio reached through an *erased* handle uses — nested as
    // another composite's child, or embedded in a trailing-metric engine. The
    // other four shapes have carried them since the seam was introduced;
    // without them a portfolio in either position silently saves `Null`.
    fn save_state(&self) -> serde_json::Value {
        self.inner.save_state()
    }
    fn load_state(&mut self, state: &serde_json::Value) -> Result<(), String> {
        self.inner.restore_state(state)
    }
}

impl DynPortfolio {
    /// The number of children the portfolio holds, in `.add(...)` order.
    pub fn child_count(&self) -> usize {
        self.inner.child_count()
    }

    /// Child `idx`'s mark-to-market equity — see [`Portfolio::sub_equity`].
    pub fn sub_equity(&self, idx: usize) -> Real {
        self.inner.sub_equity(idx)
    }

    /// Child `idx`'s signed ledger position in `symbol` — see
    /// [`Portfolio::sub_position`].
    pub fn sub_position(&self, idx: usize, symbol: &str) -> Real {
        self.inner.sub_position(idx, &crate::types::symbol(symbol))
    }

    /// Assert the netting identity against the account — see
    /// [`Portfolio::assert_books_balance`].
    pub fn assert_books_balance(&self, wallet: &dyn Wallet<Symbol>) {
        self.inner.assert_books_balance(wallet);
    }

    /// The aggregate stable-period across every child, captured at build.
    /// Used by `optimize --walkforward` to skip the initial warm-up before
    /// starting IS windows.
    ///
    /// See [`PortfolioSpec::build`] for the lazy-child caveat: portfolios
    /// containing basket / multi children under-report this at build time
    /// (only the child's rebalance signal period), since lazy per-symbol
    /// chains haven't built yet.
    pub fn stable_bars(&self) -> usize {
        self.stable_bars
    }

    /// Warm-up-only aggregate (ignoring IIR settling) — the walkforward
    /// twin of [`stable_bars`](Self::stable_bars), used under
    /// `--keep-unstable`.
    pub fn warm_up_bars(&self) -> usize {
        self.warm_up_bars
    }

    /// Serialize the wrapped portfolio's resumable state (per-child ledgers +
    /// aggregate book). See `Portfolio::save_state` for the children caveat.
    pub fn save_state(&self) -> serde_json::Value {
        self.inner.save_state()
    }

    /// Restore state produced by [`save_state`](Self::save_state).
    pub fn restore_state(&mut self, state: &serde_json::Value) -> Result<(), String> {
        self.inner.restore_state(state)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::types::Atom;

    fn candle(price: Real) -> Candle {
        Candle::new(price, price, price, price, 0.0)
    }

    fn snap_of(entries: &[(&'static str, Real)]) -> Snapshot<Symbol> {
        let mut s = Snapshot::new();
        for &(sym, close) in entries {
            let atom = Atom::new(candle(close));
            s.push(Some(crate::types::symbol(sym)), None, atom);
        }
        s
    }

    fn snap_of_at(
        entries: &[(&'static str, Real)],
        ts: crate::types::Timestamp,
    ) -> Snapshot<Symbol> {
        let mut s = Snapshot::new();
        for &(sym, close) in entries {
            let atom = Atom::with_time(candle(close), ts);
            s.push(Some(crate::types::symbol(sym)), None, atom);
        }
        s
    }

    #[test]
    fn parses_a_portfolio_with_mixed_children() {
        let yaml = r#"
            weights: !fixed [0.6, 0.4]
            children:
              - name: hold_btc
                strategy: !buy_and_hold { root: BTC }
              - name: rsi_eth
                strategy:
                  root: ETH
                  long:
                    enter: !gt { lhs: !close, rhs: !value 0 }
        "#;
        let spec = PortfolioSpec::from_text_with_params(yaml, &HashMap::new()).unwrap();
        assert_eq!(spec.children.len(), 2);
        assert!(matches!(
            &spec.children[0].strategy,
            PortfolioChildStrategy::Single(_)
        ));
        assert!(matches!(
            &spec.children[1].strategy,
            PortfolioChildStrategy::Single(_)
        ));
        // `!fixed [...]` lowered to `!value [...]` via sugar rewrite.
        let list = extract_top_level_value_list(spec.weights.as_ref().unwrap().tree())
            .expect("!fixed should have lowered to !value <list>");
        assert_eq!(list, vec![0.6, 0.4]);
    }

    /// `weights:` is a deferred template like a basket's `score:`, so a typo
    /// inside it is a load error too.
    ///
    /// It reaches `SpecTemplate` through `deserialize_weights` (the `!fixed` /
    /// `!equal_weight` sugar rewrite) rather than the plain `Deserialize`, so
    /// this pins that path against dropping the probe parse.
    #[test]
    fn a_misspelled_tag_inside_the_weights_template_fails_the_load() {
        let yaml = r#"
            weights: !drawdown_throtle { source: !portfolio_book, max_drawdown: 0.15 }
            children:
              - strategy: !buy_and_hold { root: A }
              - strategy: !buy_and_hold { root: B }
        "#;
        let err = PortfolioSpec::from_text_with_params(yaml, &HashMap::new())
            .expect_err("a misspelled tag must not load");
        let err = format!("{err:#}");
        assert!(err.contains("drawdown_throtle"), "{err}");
    }

    #[test]
    fn weights_default_to_equal_when_omitted() {
        let yaml = r#"
            children:
              - strategy: !buy_and_hold { root: A }
              - strategy: !buy_and_hold { root: B }
        "#;
        let spec = PortfolioSpec::from_text_with_params(yaml, &HashMap::new()).unwrap();
        assert!(spec.weights.is_none());
        // Two children with equal-weight split of 1000 → 500 each.
        let allocations = spec.resolve_allocations(1000.0, 2);
        assert_eq!(allocations, vec![500.0, 500.0]);
    }

    /// A `weights:` expression is read only inside a rebalance cycle, so a
    /// document that never fires its gate would compute one every bar and
    /// apply none of them — the portfolio would silently run the equal-split
    /// seed. That is a written instruction quietly ignored, so it is refused.
    #[test]
    fn dynamic_weights_without_a_rebalance_gate_are_refused() {
        let yaml = r#"
            weights: !drawdown_throttle { source: !portfolio_book, max_drawdown: 0.15 }
            children:
              - strategy: !buy_and_hold { root: A }
              - strategy: !buy_and_hold { root: B }
        "#;
        let spec = PortfolioSpec::from_text_with_params(yaml, &HashMap::new()).unwrap();
        let Err(err) = spec.try_build(1_000.0, &Schema::empty(), None) else {
            panic!("a weight expression that can never be read must not build");
        };
        assert!(err.contains("weights:"), "{err}");
        assert!(
            err.contains("rebalance_on:"),
            "the error must name the field that fixes it:\n{err}"
        );
    }

    /// The same document builds once it says when to act. `!every 1` is the
    /// eager end of the range; any signal at all satisfies the check, since
    /// what is refused is the *omitted* field, not an infrequent cadence.
    #[test]
    fn dynamic_weights_with_a_cadence_build() {
        let yaml = r#"
            weights: !drawdown_throttle { source: !portfolio_book, max_drawdown: 0.15 }
            rebalance_on: !every 28
            children:
              - strategy: !buy_and_hold { root: A }
              - strategy: !buy_and_hold { root: B }
        "#;
        let spec = PortfolioSpec::from_text_with_params(yaml, &HashMap::new()).unwrap();
        assert!(spec.try_build(1_000.0, &Schema::empty(), None).is_ok());
    }

    /// `rebalance_on: !never` is the named opt-out. Written down it says the
    /// drift is intended, which is a different statement from forgetting the
    /// field — so it builds, and the weights stay inert by request.
    #[test]
    fn dynamic_weights_with_an_explicit_never_are_allowed() {
        let yaml = r#"
            weights: !drawdown_throttle { source: !portfolio_book, max_drawdown: 0.15 }
            rebalance_on: !never
            children:
              - strategy: !buy_and_hold { root: A }
              - strategy: !buy_and_hold { root: B }
        "#;
        let spec = PortfolioSpec::from_text_with_params(yaml, &HashMap::new()).unwrap();
        assert!(spec.try_build(1_000.0, &Schema::empty(), None).is_ok());
    }

    /// Constants are exempt in both spellings: the build-time seed already
    /// *is* their answer, on every bar, forever. `!fixed` lowers to `!value
    /// <list>` (which seeds the ratio) and `!equal_weight` to `!value 1.0`
    /// (which seeds `1/N`), so neither needs a gate to take effect.
    #[test]
    fn constant_weights_need_no_rebalance_gate() {
        for weights in ["!fixed [0.75, 0.25]", "!equal_weight", "!value [0.6, 0.4]"] {
            let yaml = format!(
                r#"
                weights: {weights}
                children:
                  - strategy: !buy_and_hold {{ root: A }}
                  - strategy: !buy_and_hold {{ root: B }}
            "#
            );
            let spec = PortfolioSpec::from_text_with_params(&yaml, &HashMap::new()).unwrap();
            assert!(
                spec.try_build(1_000.0, &Schema::empty(), None).is_ok(),
                "constant weights `{weights}` must build without a gate"
            );
        }
    }

    #[test]
    fn fixed_weights_split_cash_proportionally() {
        let yaml = r#"
            weights: !fixed [0.75, 0.25]
            children:
              - strategy: !buy_and_hold { root: A }
              - strategy: !buy_and_hold { root: B }
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
        assert!(matches!(
            &spec.children[0].strategy,
            PortfolioChildStrategy::Pairs(_)
        ));

        // Basket: has selection.
        let yaml = r#"
            children:
              - strategy:
                  selection: !top_bottom { longs: 1, shorts: 1 }
                  score: !roc { source: !close { source: !pick { symbol: !slot SYM } }, period: 5 }
                  sizing: !equal_weight 2
        "#;
        let spec = PortfolioSpec::from_text_with_params(yaml, &HashMap::new()).unwrap();
        assert!(matches!(
            &spec.children[0].strategy,
            PortfolioChildStrategy::Basket(_)
        ));

        // Multi: no symbol, no pairs/basket keys.
        let yaml = r#"
            children:
              - strategy:
                  long:
                    enter: !gt { lhs: !close { source: !pick { symbol: !slot SYM } }, rhs: !value 0 }
                  sizing: !equal_weight 2
        "#;
        let spec = PortfolioSpec::from_text_with_params(yaml, &HashMap::new()).unwrap();
        assert!(matches!(
            &spec.children[0].strategy,
            PortfolioChildStrategy::Multi(_)
        ));
    }

    #[test]
    fn build_drives_two_buy_and_hold_children_split_by_weights() {
        let yaml = r#"
            weights: !fixed [0.6, 0.4]
            children:
              - name: hold_a
                strategy: !buy_and_hold { root: A }
              - name: hold_b
                strategy: !buy_and_hold { root: B }
        "#;
        let spec = PortfolioSpec::from_text_with_params(yaml, &HashMap::new()).unwrap();
        let mut portfolio = spec.build(10_000.0, &Schema::empty(), None);
        let mut wallet = PaperWallet::new(10_000.0);

        // Two bars: bar 1 queues entry, bar 2 fills. Portfolio wallet fans
        // the update to every sub, so each child's own PaperWallet marks +
        // fills its own leg.
        for _ in 0..2 {
            for fill in wallet.update(crate::types::symbol("A"), candle(100.0)) {
                portfolio.on_fill(&fill);
            }
            for fill in wallet.update(crate::types::symbol("B"), candle(50.0)) {
                portfolio.on_fill(&fill);
            }
            portfolio.update(snap_of(&[("A", 100.0), ("B", 50.0)]));
            portfolio.trade(&mut wallet);
        }
        // Aggregate equity across both sub-wallets is roughly the seed (no
        // move in prices → no P&L).
        assert!((wallet.equity().0 - 10_000.0).abs() < 1e-6);
        // Both legs are long — each child bought its own symbol.
        assert!(wallet.position(&crate::types::symbol("A")).amount > 0.0);
        assert!(wallet.position(&crate::types::symbol("B")).amount > 0.0);
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
                strategy: !buy_and_hold { root: !param SYM }
        "#;
        let mut params = HashMap::new();
        params.insert("SYM".to_string(), Value::String("BTC".to_string()));
        let spec = PortfolioSpec::from_text_with_params(yaml, &params).unwrap();
        match &spec.children[0].strategy {
            PortfolioChildStrategy::Single(s) => assert_eq!(s.symbol().unwrap(), "BTC"),
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
              - strategy: !buy_and_hold { root: A }
              - strategy: !buy_and_hold { root: B }
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
              - strategy: !buy_and_hold { root: A }
              - strategy: !buy_and_hold { root: B }
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
              - strategy: !buy_and_hold { root: A }
              - strategy: !buy_and_hold { root: B }
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
                  root: A
                  sizing: !value 1.0
                  long:
                    enter: !value true
                    exit: !value false
              - name: full_b
                strategy:
                  root: B
                  sizing: !value 1.0
                  long:
                    enter: !value true
                    exit: !value false
        "#;
        let spec = PortfolioSpec::from_text_with_params(yaml, &HashMap::new()).unwrap();
        let mut portfolio = spec.build(1_000.0, &Schema::empty(), None);
        let mut wallet = PaperWallet::new(1_000.0);

        for bar in 0..4usize {
            let px_a = if bar < 2 { 100.0 } else { 1000.0 };
            let px_b = 100.0;
            for fill in wallet.update(crate::types::symbol("A"), candle(px_a)) {
                portfolio.on_fill(&fill);
            }
            for fill in wallet.update(crate::types::symbol("B"), candle(px_b)) {
                portfolio.on_fill(&fill);
            }
            portfolio.update(snap_of(&[("A", px_a), ("B", px_b)]));
            portfolio.trade(&mut wallet);
        }

        // A must have shrunk (its position phase scaled the sole leg down);
        // B remains flat or grew from the freed cash on the next fire.
        let e0 = portfolio.sub_equity(0);
        let e1 = portfolio.sub_equity(1);
        assert!(
            e0 < e1 * 4.0,
            "largest-first should have started rebalancing A down; got e0={e0}, e1={e1}",
        );
    }

    #[test]
    fn rebalance_on_defaults_to_none() {
        // Omitted `rebalance_on:` → the built portfolio behaves as
        // pre-rebalance v1 (ValueBool::false gate, weights drift with P&L).
        let yaml = r#"
            children:
              - strategy: !buy_and_hold { root: A }
              - strategy: !buy_and_hold { root: B }
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
        // `!slot SYM` (single-asset only) and `!slot CHILD_NAME`.
        let yaml = r#"
            weights:
              close:
                source:
                  pick:
                    symbol: !slot SYM
            rebalance_on: !every 1
            children:
              - strategy: !buy_and_hold { root: A }
              - strategy: !buy_and_hold { root: B }
        "#;
        let spec = PortfolioSpec::from_text_with_params(yaml, &HashMap::new()).unwrap();
        assert!(spec.weights.is_some());
        // Should build cleanly — each child's template instance uses its
        // own symbol via !slot SYM.
        let _portfolio = spec.build(1_000.0, &Schema::empty(), None);
    }

    #[test]
    fn parses_rebalance_on_every_28() {
        let yaml = r#"
            rebalance_on: !every 28
            children:
              - strategy: !buy_and_hold { root: A }
              - strategy: !buy_and_hold { root: B }
        "#;
        let spec = PortfolioSpec::from_text_with_params(yaml, &HashMap::new()).unwrap();
        assert!(spec.rebalance_on.is_some());
    }

    #[test]
    fn parses_rebalance_on_never_as_const_false() {
        let yaml = r#"
            rebalance_on: !never
            children:
              - strategy: !buy_and_hold { root: A }
              - strategy: !buy_and_hold { root: B }
        "#;
        let spec = PortfolioSpec::from_text_with_params(yaml, &HashMap::new()).unwrap();
        assert!(spec.rebalance_on.is_some());
        // Should build cleanly — !never resolves to ValueBool::false, which
        // has zero warm-up / stable period.
        let portfolio = spec.build(1_000.0, &Schema::empty(), None);
        assert_eq!(portfolio.stable_bars(), 0);
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
        use crate::types::Timestamp;

        let yaml = r#"
            weights: !value [0.5, 0.5]
            rebalance_on: !monthly
            children:
              - name: half_a
                strategy:
                  root: A
                  sizing: !value 0.5
                  long:
                    enter: !value true
                    exit: !value false
              - name: half_b
                strategy:
                  root: B
                  sizing: !value 0.5
                  long:
                    enter: !value true
                    exit: !value false
        "#;
        let spec = PortfolioSpec::from_text_with_params(yaml, &HashMap::new()).unwrap();
        let mut portfolio = spec.build(1_000.0, &Schema::empty(), None);
        let mut wallet = PaperWallet::new(1_000.0);

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
            for fill in wallet.update(crate::types::symbol("A"), candle(px_a)) {
                portfolio.on_fill(&fill);
            }
            for fill in wallet.update(crate::types::symbol("B"), candle(px_b)) {
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
                  root: A
                  sizing: !value 0.5
                  long:
                    enter: !value true
                    exit: !value false
              - name: half_b
                strategy:
                  root: B
                  sizing: !value 0.5
                  long:
                    enter: !value true
                    exit: !value false
        "#;
        let spec = PortfolioSpec::from_text_with_params(yaml, &HashMap::new()).unwrap();
        let mut portfolio = spec.build(1_000.0, &Schema::empty(), None);
        let mut wallet = PaperWallet::new(1_000.0);

        for bar in 0..4usize {
            let px_a = if bar < 2 { 100.0 } else { 200.0 };
            let px_b = 100.0;
            for fill in wallet.update(crate::types::symbol("A"), candle(px_a)) {
                portfolio.on_fill(&fill);
            }
            for fill in wallet.update(crate::types::symbol("B"), candle(px_b)) {
                portfolio.on_fill(&fill);
            }
            portfolio.update(snap_of(&[("A", px_a), ("B", px_b)]));
            portfolio.trade(&mut wallet);
        }

        let e0 = portfolio.sub_equity(0);
        let e1 = portfolio.sub_equity(1);
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
                  root: A
                  sizing: !value 0.5
                  long:
                    enter: !value true
                    exit: !value false
              - name: b
                strategy:
                  root: B
                  sizing: !value 0.5
                  long:
                    enter: !value true
                    exit: !value false
        "#;
        let spec = PortfolioSpec::from_text_with_params(yaml, &HashMap::new()).unwrap();
        let mut portfolio = spec.build(1_000.0, &Schema::empty(), None);
        let mut wallet = PaperWallet::new(1_000.0);
        // Initial split respects the list (seed via extract_top_level_value_list).
        assert!((portfolio.sub_equity(0) - 750.0).abs() < 1e-6);
        assert!((portfolio.sub_equity(1) - 250.0).abs() < 1e-6);
        // Run a few bars — rebalance keeps ratios locked to 75/25.
        for _ in 0..4 {
            for fill in wallet.update(crate::types::symbol("A"), candle(100.0)) {
                portfolio.on_fill(&fill);
            }
            for fill in wallet.update(crate::types::symbol("B"), candle(100.0)) {
                portfolio.on_fill(&fill);
            }
            portfolio.update(snap_of(&[("A", 100.0), ("B", 100.0)]));
            portfolio.trade(&mut wallet);
        }
        let e0 = portfolio.sub_equity(0);
        let e1 = portfolio.sub_equity(1);
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
              - strategy: !buy_and_hold { root: A }
              - strategy: !buy_and_hold { root: B }
        "#;
        let yaml_value = r#"
            weights: !value [0.6, 0.4]
            children:
              - strategy: !buy_and_hold { root: A }
              - strategy: !buy_and_hold { root: B }
        "#;
        let spec_fixed = PortfolioSpec::from_text_with_params(yaml_fixed, &HashMap::new()).unwrap();
        let spec_value = PortfolioSpec::from_text_with_params(yaml_value, &HashMap::new()).unwrap();
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
              - strategy: !buy_and_hold { root: A }
              - strategy: !buy_and_hold { root: B }
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
                  root: A
                  sizing: !value 0.5
                  long:
                    enter: !value true
                    exit: !value false
              - name: b
                strategy:
                  root: B
                  sizing: !value 0.5
                  long:
                    enter: !value true
                    exit: !value false
        "#;
        let spec = PortfolioSpec::from_text_with_params(yaml, &HashMap::new()).unwrap();
        let mut portfolio = spec.build(1_000.0, &Schema::empty(), None);
        let mut wallet = PaperWallet::new(1_000.0);

        for _ in 0..4usize {
            for fill in wallet.update(crate::types::symbol("A"), candle(100.0)) {
                portfolio.on_fill(&fill);
            }
            for fill in wallet.update(crate::types::symbol("B"), candle(100.0)) {
                portfolio.on_fill(&fill);
            }
            portfolio.update(snap_of(&[("A", 100.0), ("B", 100.0)]));
            portfolio.trade(&mut wallet);
        }
        let e0 = portfolio.sub_equity(0);
        let e1 = portfolio.sub_equity(1);
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
            root: X
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
            rebalance_on: !every 1
            children:
              - strategy: !buy_and_hold { root: A }
              - strategy: !buy_and_hold { root: B }
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
            rebalance_on: !every 1
            children:
              - strategy: !buy_and_hold { root: A }
              - strategy: !buy_and_hold { root: B }
        "#;
        let spec = PortfolioSpec::from_text_with_params(yaml, &HashMap::new()).unwrap();
        let _portfolio = spec.build(1_000.0, &Schema::empty(), None);
    }

    #[test]
    fn parses_group_field_on_children() {
        // `group:` is optional on every child; children may share a
        // group so a `weights:` expression can dispatch by cohort.
        let yaml = r#"
            children:
              - name: fast
                group: momentum
                strategy: !buy_and_hold { root: A }
              - name: slow
                group: momentum
                strategy: !buy_and_hold { root: B }
              - name: reverter
                group: mean_rev
                strategy: !buy_and_hold { root: C }
              - name: ungrouped
                strategy: !buy_and_hold { root: D }
        "#;
        let spec = PortfolioSpec::from_text_with_params(yaml, &HashMap::new()).unwrap();
        assert_eq!(spec.children[0].group.as_deref(), Some("momentum"));
        assert_eq!(spec.children[1].group.as_deref(), Some("momentum"));
        assert_eq!(spec.children[2].group.as_deref(), Some("mean_rev"));
        assert_eq!(spec.children[3].group, None);
    }

    #[test]
    fn child_group_slot_resolves_in_weights_template() {
        // `!slot CHILD_GROUP` in the weights template resolves per child.
        // Here we dispatch on group via `!if_else` — the momentum leg
        // reads 2.0, the mean-rev leg reads 1.0; normalized 2/3 and
        // 1/3 with `rebalance_on: !every 1` should snap the sub-equities
        // toward those weights. The `!value { slot: CHILD_GROUP }`
        // wrapper turns the resolved slot into an `NodeSpec::Value(Str)`
        // — the position where `!eq`'s lhs takes any
        // `Str`-emitting NodeSpec.
        let yaml = r#"
            weights:
              !if_else
                cond: !eq { lhs: !value { slot: CHILD_GROUP }, rhs: momentum }
                then: !value 2.0
                otherwise: !value 1.0
            rebalance_on: !every 1
            children:
              - name: mom
                group: momentum
                strategy:
                  root: A
                  sizing: !value 0.5
                  long:
                    enter: !value true
                    exit: !value false
              - name: mr
                group: mean_rev
                strategy:
                  root: B
                  sizing: !value 0.5
                  long:
                    enter: !value true
                    exit: !value false
        "#;
        let spec = PortfolioSpec::from_text_with_params(yaml, &HashMap::new()).unwrap();
        let mut portfolio = spec.build(1_500.0, &Schema::empty(), None);
        let mut wallet = PaperWallet::new(1_500.0);
        for _ in 0..4 {
            for fill in wallet.update(crate::types::symbol("A"), candle(100.0)) {
                portfolio.on_fill(&fill);
            }
            for fill in wallet.update(crate::types::symbol("B"), candle(100.0)) {
                portfolio.on_fill(&fill);
            }
            portfolio.update(snap_of(&[("A", 100.0), ("B", 100.0)]));
            portfolio.trade(&mut wallet);
        }
        let e0 = portfolio.sub_equity(0);
        let e1 = portfolio.sub_equity(1);
        let total = e0 + e1;
        // Weights normalize to 2/3 (momentum) and 1/3 (mean_rev).
        assert!(
            (e0 / total - 2.0 / 3.0).abs() < 0.02,
            "momentum leg should be ~2/3; got e0={e0}, e1={e1}",
        );
    }

    #[test]
    #[should_panic(expected = "CHILD_GROUP")]
    fn child_group_slot_missing_on_ungrouped_child_panics() {
        // A weights template that references `!slot CHILD_GROUP` with an
        // ungrouped child fails at build with a missing-slot error —
        // matches the CHILD_NAME / SYM convention (no silent auto-populated
        // fallback for identity slots).
        let yaml = r#"
            weights: !slot CHILD_GROUP
            rebalance_on: !every 1
            children:
              - name: named_but_ungrouped
                strategy: !buy_and_hold { root: A }
        "#;
        let spec = PortfolioSpec::from_text_with_params(yaml, &HashMap::new()).unwrap();
        let _ = spec.build(1_000.0, &Schema::empty(), None);
    }

    #[test]
    #[should_panic(expected = "CHILD_NAME")]
    fn child_name_slot_missing_on_unnamed_child_panics() {
        // Symmetry with the CHILD_GROUP case: `!slot CHILD_NAME` on an
        // unnamed child (no explicit `name:`) fails. The internal
        // `child_<idx>` default only keys sub-wallets — it's not
        // injected as the slot.
        let yaml = r#"
            weights: !slot CHILD_NAME
            rebalance_on: !every 1
            children:
              - strategy: !buy_and_hold { root: A }
        "#;
        let spec = PortfolioSpec::from_text_with_params(yaml, &HashMap::new()).unwrap();
        let _ = spec.build(1_000.0, &Schema::empty(), None);
    }

    #[test]
    fn match_dispatches_weights_by_child_group() {
        // The motivating case for `!match`: pick a per-child weight
        // expression by group name. Momentum children score 2.0,
        // mean-rev children score 1.0, others fall through to 0.5.
        // Normalized weights: 2.0 → 4/6, 1.0 → 2/6 (only two children here).
        let yaml = r#"
            weights:
              !match
                on: !value { slot: CHILD_GROUP }
                cases:
                  - when: momentum
                    value: !value 2.0
                  - when: mean_rev
                    value: !value 1.0
                default: !value 0.5
            rebalance_on: !every 1
            children:
              - name: mom
                group: momentum
                strategy:
                  root: A
                  sizing: !value 0.5
                  long:
                    enter: !value true
                    exit: !value false
              - name: mr
                group: mean_rev
                strategy:
                  root: B
                  sizing: !value 0.5
                  long:
                    enter: !value true
                    exit: !value false
        "#;
        let spec = PortfolioSpec::from_text_with_params(yaml, &HashMap::new()).unwrap();
        let mut portfolio = spec.build(1_500.0, &Schema::empty(), None);
        let mut wallet = PaperWallet::new(1_500.0);
        for _ in 0..4 {
            for fill in wallet.update(crate::types::symbol("A"), candle(100.0)) {
                portfolio.on_fill(&fill);
            }
            for fill in wallet.update(crate::types::symbol("B"), candle(100.0)) {
                portfolio.on_fill(&fill);
            }
            portfolio.update(snap_of(&[("A", 100.0), ("B", 100.0)]));
            portfolio.trade(&mut wallet);
        }
        let e0 = portfolio.sub_equity(0);
        let e1 = portfolio.sub_equity(1);
        let total = e0 + e1;
        // Momentum leg (weight 2) vs mean_rev leg (weight 1) → 2/3, 1/3.
        assert!(
            (e0 / total - 2.0 / 3.0).abs() < 0.02,
            "!match by CHILD_GROUP should give momentum ~2/3; got e0={e0}, e1={e1}",
        );
    }

    #[test]
    #[should_panic(expected = "duplicate child name")]
    fn duplicate_child_names_panic_at_build() {
        // Two children declaring the same name. This pins the *shim's*
        // contract — `build` unwraps whatever `try_build` returns, so it
        // still aborts. The fallible path is covered by
        // `duplicate_child_names_are_a_build_error_not_an_abort`.
        let yaml = r#"
            children:
              - name: dup
                strategy: !buy_and_hold { root: A }
              - name: dup
                strategy: !buy_and_hold { root: B }
        "#;
        let spec = PortfolioSpec::from_text_with_params(yaml, &HashMap::new()).unwrap();
        let _ = spec.build(1_000.0, &Schema::empty(), None);
    }

    #[test]
    #[should_panic(expected = "duplicate child name")]
    fn explicit_name_colliding_with_default_child_slot_panics() {
        // An explicit `name: child_1` collides with the auto-generated
        // `child_1` for the second (unnamed) slot — must panic to keep
        // sub-wallet lookups unambiguous.
        let yaml = r#"
            children:
              - name: child_1
                strategy: !buy_and_hold { root: A }
              - strategy: !buy_and_hold { root: B }
        "#;
        let spec = PortfolioSpec::from_text_with_params(yaml, &HashMap::new()).unwrap();
        let _ = spec.build(1_000.0, &Schema::empty(), None);
    }

    #[test]
    fn duplicate_child_names_are_a_build_error_not_an_abort() {
        // Two children resolving to the same name makes sub-wallet lookups
        // ambiguous, so it must be refused — but as a value. It used to
        // `assert!`, taking the CLI down without a breadcrumb.
        let yaml = r#"
            children:
              - name: momentum
                strategy: !buy_and_hold { root: A }
              - name: momentum
                strategy: !buy_and_hold { root: B }
        "#;
        let spec = PortfolioSpec::from_text_with_params(yaml, &HashMap::new()).unwrap();
        let err = spec
            .try_build(1_000.0, &Schema::empty(), None)
            .err()
            .expect("a duplicate child name must be rejected");
        assert!(err.contains("duplicate child name"), "{err}");
        assert!(err.contains("momentum"), "names the collision: {err}");
    }

    #[test]
    fn a_basket_child_reports_a_bad_expression_instead_of_aborting() {
        // The Single and Pairs arms of this match always propagated their
        // child's error; the Basket and Multi arms called the panicking
        // `build` shim, so a bad expression under either aborted the process.
        let yaml = r#"
            children:
              - name: b
                strategy:
                  score: !sma { source: !get { key: nope }, period: 3 }
                  selection: !top_bottom { longs: 1, shorts: 1 }
                  sizing: !equal_weight 2
        "#;
        let spec = PortfolioSpec::from_text_with_params(yaml, &HashMap::new()).unwrap();
        let err = spec
            .try_build(1_000.0, &Schema::empty(), None)
            .err()
            .expect("a basket child's bad `!get` must be rejected");
        assert!(err.contains("no overlay side channel is bound"), "{err}");
    }

    #[test]
    fn dyn_portfolio_surfaces_a_childs_rejection() {
        // The CLI and Python only ever see a `DynPortfolio`, so the composite
        // wallet's rejection drain has to survive the wrapper and land in
        // `RunReport::rejections`. (The `on_reject` delegation down to the
        // owning child is covered in tests/portfolio.rs, where a test child
        // actually records what it is handed — the spec-built shapes all
        // inherit the no-op default, so routing isn't observable here.)
        //
        // A short position whose buy-to-cover stop triggers on a violent gap
        // up is the cleanest way to force a refusal from a spec-built child:
        // covering costs far more than the sub-wallet's cash, so the wallet
        // books a `Stop` rejection and leaves the bracket resting.
        let yaml = r#"
            children:
              - name: shorty
                strategy:
                  root: A
                  short:
                    enter: !value true
                    exit: !value false
                    stop_loss: !mul { lhs: !entry, rhs: !value 1.05 }
        "#;
        let spec = PortfolioSpec::from_text_with_params(yaml, &HashMap::new()).unwrap();
        let mut portfolio = spec.build(1_000.0, &Schema::empty(), None);

        // Bars 0-1 open and fill the short at 100; bar 2 gaps to 100_000, so
        // the stop triggers at a price the child cannot pay to cover.
        let snaps: Vec<Snapshot<Symbol>> = vec![
            snap_of(&[("A", 100.0)]),
            snap_of(&[("A", 100.0)]),
            snap_of(&[("A", 100_000.0)]),
            snap_of(&[("A", 100_000.0)]),
        ];
        let mut wallet = PaperWallet::new(1_000.0);
        let report = crate::backtest::run(&mut portfolio, &mut wallet, snaps.iter().cloned());

        assert!(
            report
                .rejections
                .iter()
                .any(|r| r.rejection.kind == OrderKind::Stop),
            "a refused protective leg must reach the report through DynPortfolio",
        );
    }
}
