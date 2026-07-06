//! Cost configuration: fold the parsed `CostSpec` layers into a resolved
//! `CostConfig`, plus the runtime resolution of `(symbol, freq) → TradingCosts`.
//!
//! Split out of `costs/mod.rs`; kept in `crate::costs::config` so
//! `crate::costs::CostConfig` still resolves via the `pub use` in `mod.rs`.

use std::collections::HashMap;

use anyhow::{Context, Result, anyhow, bail};
use fugazi::costs::{
    CommissionModel, CompositeCommission, FixedAbsoluteSpread, FixedBpsSlippage, FixedBpsSpread,
    FixedCommission, MaxCommission, NoCommission, NoSlippage, NoSpread, PercentageCommission,
    PerUnitCommission, SlippageModel, SpreadModel, TradingCosts, VolumeParticipationSlippage,
};
use fugazi::types::Real;
use serde::Deserialize;
use serde_json::{Map, Value};

use crate::calendar::{Frequency, Scope};
use crate::input;

use super::spec::{CostSpec, CostTerm};

/// for scalars and arrays; the `none` literal resets the accumulator.
pub fn config(specs: &[CostSpec]) -> Result<CostConfig> {
    let mut tree = empty_tree();
    for spec in specs {
        for term in &spec.0 {
            apply_term(&mut tree, term)?;
        }
    }
    // The typed enums are internally-tagged (`kind: percentage`), but the
    // inline YAML form uses external tagging (`!percentage {…}` → singleton
    // object). Normalize every model-position node in the tree before typed
    // deserialization so both shapes reach the same variant.
    let normalized = Value::Object(normalize_tree_to_internal_tags(tree));
    let config: CostConfig = serde_json::from_value(normalized)
        .context("interpreting the resolved --costs tree")?;
    Ok(config)
}

/// Names the model enums recognize as `kind:` values — for detecting when a
/// singleton-object node is an external-tag form we should rewrite.
const KNOWN_KINDS: &[&str] = &[
    // Commission
    "none",
    "fixed",
    "percentage",
    "per_unit",
    "composite",
    "max",
    // Spread
    "bps",
    "absolute",
    // Slippage
    "volume_participation",
];

/// Rewrite every model-position node inside `tree` from external-tag form
/// (`{percentage: {rate: X}}`) to internal-tag form (`{kind: percentage, rate:
/// X}`), so serde's internally-tagged Deserialize accepts both shapes.
fn normalize_tree_to_internal_tags(mut tree: Map<String, Value>) -> Map<String, Value> {
    for leg in ["commission", "spread", "slippage"] {
        if let Some(Value::Object(leg_map)) = tree.get_mut(leg) {
            if let Some(v) = leg_map.get_mut("default") {
                *v = normalize_model_value(v.take());
            }
            for key in ["by_symbol", "by_interval"] {
                if let Some(Value::Object(map)) = leg_map.get_mut(key) {
                    for v in map.values_mut() {
                        *v = normalize_model_value(v.take());
                    }
                }
            }
            if let Some(Value::Array(entries)) = leg_map.get_mut("scoped") {
                for e in entries {
                    if let Value::Object(obj) = e
                        && let Some(m) = obj.get_mut("model")
                    {
                        *m = normalize_model_value(m.take());
                    }
                }
            }
        }
    }
    tree
}

/// Normalize a single model node. External-tag form (`{percentage: {rate: X}}`,
/// or the parameterless `"percentage"` string) becomes `{kind: percentage, ...}`;
/// an already-internally-tagged object passes through; anything else is left
/// alone (typed deserialization will emit a clear error).
fn normalize_model_value(value: Value) -> Value {
    match value {
        Value::String(ref name) if KNOWN_KINDS.contains(&name.as_str()) => {
            let mut m = Map::new();
            m.insert("kind".to_string(), Value::String(name.clone()));
            Value::Object(m)
        }
        Value::Object(obj) => {
            // Already internally tagged.
            if obj.contains_key("kind") {
                // Recursively normalize nested model fields (composite parts,
                // max legs) so a nested `!fixed { amount: 1.0 }` also normalizes.
                let mut obj = obj;
                if let Some(parts) = obj.get_mut("parts")
                    && let Value::Array(entries) = parts
                {
                    for v in entries.iter_mut() {
                        *v = normalize_model_value(v.take());
                    }
                }
                for k in ["lhs", "rhs"] {
                    if let Some(v) = obj.get_mut(k) {
                        *v = normalize_model_value(v.take());
                    }
                }
                return Value::Object(obj);
            }
            // Single-key form: rewrite `{name: {...}}` → `{kind: name, ...}`.
            if obj.len() == 1 {
                let (name, inner) = obj.into_iter().next().unwrap();
                if KNOWN_KINDS.contains(&name.as_str()) {
                    let mut m = Map::new();
                    m.insert("kind".to_string(), Value::String(name));
                    match inner {
                        Value::Object(fields) => {
                            for (k, v) in fields {
                                m.insert(k, normalize_model_value(v));
                            }
                        }
                        Value::Null => {}
                        other => {
                            // A scalar body for an external tag (e.g. `!fixed 1.0`)
                            // would collapse the single field — we don't support
                            // this for models. Rebuild as-was so the typed error
                            // is clear.
                            m.insert("value".to_string(), other);
                        }
                    }
                    // Also normalize nested composite/max fields.
                    if let Some(parts) = m.get_mut("parts")
                        && let Value::Array(entries) = parts
                    {
                        for v in entries.iter_mut() {
                            *v = normalize_model_value(v.take());
                        }
                    }
                    for k in ["lhs", "rhs"] {
                        if let Some(v) = m.get_mut(k) {
                            *v = normalize_model_value(v.take());
                        }
                    }
                    return Value::Object(m);
                }
                // Restore the map.
                let mut restored = Map::new();
                restored.insert(name, inner);
                return Value::Object(restored);
            }
            Value::Object(obj)
        }
        other => other,
    }
}

/// The empty accumulator: `{commission: {}, spread: {}, slippage: {}}`.
fn empty_tree() -> Map<String, Value> {
    let mut m = Map::new();
    m.insert("commission".to_string(), Value::Object(Map::new()));
    m.insert("spread".to_string(), Value::Object(Map::new()));
    m.insert("slippage".to_string(), Value::Object(Map::new()));
    m
}

fn apply_term(tree: &mut Map<String, Value>, term: &CostTerm) -> Result<()> {
    match term {
        CostTerm::None => {
            // Reset every leg to the empty object (which deserializes to an
            // all-no-op LegConfig).
            *tree = empty_tree();
        }
        CostTerm::Load(source) => {
            let text = source.read().context("reading --costs file")?;
            let value = input::parse_value(&text)
                .with_context(|| format!("parsing --costs file {}", source.label()))?;
            let normalized = normalize_preset(value)
                .with_context(|| format!("normalizing --costs preset {}", source.label()))?;
            deep_merge_into(tree, normalized);
        }
        CostTerm::Set { scope, key, value } => {
            set_scoped(tree, scope, key, value.clone())?;
        }
    }
    // Fold external-tag models into internal-tag form after every term so a
    // later dotted-key setter (`commission.rate=…`) targets a `kind:`-shaped
    // parent rather than an untouched `{percentage: {…}}` singleton.
    let taken = std::mem::replace(tree, Map::new());
    *tree = normalize_tree_to_internal_tags(taken);
    Ok(())
}

/// Turn a user-facing preset into the canonical structured form. A leg whose
/// value carries a `kind:` discriminator is a flat model — hoist it into
/// `default:`. A leg whose value already has `default:`/`by_symbol:`/`by_interval:`
/// stays as-is.
fn normalize_preset(value: Value) -> Result<Map<String, Value>> {
    let Value::Object(map) = value else {
        bail!("cost preset must be a mapping (got {})", value_kind(&value));
    };
    let mut out = Map::new();
    for (leg, node) in map {
        if !matches!(leg.as_str(), "commission" | "spread" | "slippage") {
            bail!(
                "cost preset has unknown leg `{leg}` (expected commission/spread/slippage)"
            );
        }
        out.insert(leg, normalize_leg(node)?);
    }
    Ok(out)
}

/// A leg node is one of:
/// * a flat model — `{kind: ...}` (internal-tag form) or `{percentage: {...}}`
///   (external-tag / YAML-`!tag` form) — becomes `{default: <it>}`.
/// * a `{default: ..., by_symbol: {...}, by_interval: {...}}` structured shape —
///   pass through.
fn normalize_leg(node: Value) -> Result<Value> {
    let Value::Object(obj) = node else {
        return Ok(node);
    };
    let has_kind = obj.contains_key("kind");
    let has_structured = obj.contains_key("default")
        || obj.contains_key("by_symbol")
        || obj.contains_key("by_interval")
        || obj.contains_key("scoped");
    let is_external_tag = obj.len() == 1
        && obj
            .keys()
            .next()
            .is_some_and(|k| KNOWN_KINDS.contains(&k.as_str()));

    if has_kind && has_structured {
        bail!(
            "cost leg cannot mix flat (`kind: …`) and structured (`default`/`by_symbol`/…) shapes"
        );
    }
    if has_kind || is_external_tag {
        let mut wrapper = Map::new();
        wrapper.insert("default".to_string(), Value::Object(obj));
        return Ok(Value::Object(wrapper));
    }
    if !has_structured {
        // Neither flat nor structured — refuse rather than silently accept an
        // empty leg the user thought they were configuring.
        bail!(
            "cost leg must be either a flat model (`kind: …` or `!variant {{…}}`) \
             or a structured `{{default, by_symbol, by_interval}}` mapping"
        );
    }
    Ok(Value::Object(obj))
}

/// Deep-merge `src` into `dest`: object keys recurse (later overrides), scalars
/// and arrays are replaced wholesale. A `{kind: ...}` value on either side
/// short-circuits recursion — a full model replaces the previous one wholesale
/// so a `commission: {kind: percentage}` layer isn't left with an orphan
/// `rate` field when a later `commission: {kind: max, lhs, rhs}` overwrites it.
fn deep_merge_into(dest: &mut Map<String, Value>, src: Map<String, Value>) {
    for (k, v) in src {
        match (dest.get_mut(&k), v) {
            (Some(Value::Object(inner_dest)), Value::Object(inner_src)) => {
                // If either side already looks like a fully-formed model
                // (`kind:` present), replacing the whole subtree is the right
                // semantic — deep-merging risks leaving orphan fields from the
                // previous variant.
                if inner_dest.contains_key("kind") || inner_src.contains_key("kind") {
                    *inner_dest = inner_src;
                } else {
                    deep_merge_into(inner_dest, inner_src);
                }
            }
            (_, other) => {
                dest.insert(k, other);
            }
        }
    }
}

/// Apply a `Set` term with the given `scope` and dotted `key` to `tree`.
///
/// A scope-less `commission=EXPR` is sugar for `commission.default=EXPR`; a
/// scope-less `commission.rate=…` writes into `commission.default.rate` when the
/// first segment after the leg is a leaf name (i.e. not one of `default`,
/// `by_symbol`, `by_interval`, `scoped`).
///
/// A symbol-scoped `S:commission=EXPR` writes to `commission.by_symbol.S`; a
/// freq-scoped `[F]:commission=EXPR` writes to `commission.by_interval.<F>`; a
/// full-scoped `S[F]:commission=EXPR` appends to `commission.scoped` as
/// `{symbol, freq, model}` (in insertion order, so later-declared entries win at
/// resolution time).
fn set_scoped(
    tree: &mut Map<String, Value>,
    scope: &Scope,
    key: &[String],
    value: Value,
) -> Result<()> {
    let leg = &key[0];
    let rest = &key[1..];

    // Split into "path segments inside the leg" once so scope-less and scoped
    // paths compute the same target address.
    let target_leg = tree
        .get_mut(leg)
        .expect("empty_tree seeded every leg")
        .as_object_mut()
        .ok_or_else(|| anyhow!("leg `{leg}` is not an object"))?;

    if scope.is_default() {
        // Rewrite `commission.X…` to `commission.default.X…` if X is not one of
        // the top-level structured keys.
        let path: Vec<String> = if let Some(first) = rest.first() {
            if matches!(
                first.as_str(),
                "default" | "by_symbol" | "by_interval" | "scoped"
            ) {
                rest.to_vec()
            } else {
                std::iter::once("default".to_string())
                    .chain(rest.iter().cloned())
                    .collect()
            }
        } else {
            vec!["default".to_string()]
        };
        assign_path(target_leg, &path, value);
        return Ok(());
    }

    match (scope.symbol.as_deref(), scope.freq) {
        (Some(symbol), None) => {
            let map = ensure_object(target_leg, "by_symbol");
            let existing = map.entry(symbol.to_string()).or_insert(Value::Null);
            if rest.is_empty() {
                *existing = value;
            } else {
                if !existing.is_object() {
                    *existing = Value::Object(Map::new());
                }
                let obj = existing.as_object_mut().expect("just made it an object");
                assign_path(obj, rest, value);
            }
        }
        (None, Some(freq)) => {
            let map = ensure_object(target_leg, "by_interval");
            let key = format_freq(freq);
            let existing = map.entry(key).or_insert(Value::Null);
            if rest.is_empty() {
                *existing = value;
            } else {
                if !existing.is_object() {
                    *existing = Value::Object(Map::new());
                }
                let obj = existing.as_object_mut().expect("just made it an object");
                assign_path(obj, rest, value);
            }
        }
        (Some(symbol), Some(freq)) => {
            if !rest.is_empty() {
                bail!(
                    "scoped cost setter `{}[{}]:{}` cannot use a dotted subkey; \
                     assign the whole model with a single expression",
                    symbol,
                    format_freq(freq),
                    key.join("."),
                );
            }
            let list = target_leg
                .entry("scoped".to_string())
                .or_insert_with(|| Value::Array(Vec::new()))
                .as_array_mut()
                .ok_or_else(|| anyhow!("`{leg}.scoped` must be an array"))?;
            let mut entry = Map::new();
            entry.insert("symbol".to_string(), Value::String(symbol.to_string()));
            entry.insert("freq".to_string(), Value::String(format_freq(freq)));
            entry.insert("model".to_string(), value);
            list.push(Value::Object(entry));
        }
        (None, None) => unreachable!("scope.is_default() covered above"),
    }
    Ok(())
}

fn assign_path(root: &mut Map<String, Value>, path: &[String], value: Value) {
    let (last, prefix) = path.split_last().expect("assign_path needs at least one segment");
    let mut cur = root;
    for seg in prefix {
        let entry = cur
            .entry(seg.clone())
            .or_insert_with(|| Value::Object(Map::new()));
        if !entry.is_object() {
            *entry = Value::Object(Map::new());
        }
        cur = entry.as_object_mut().expect("just made it an object");
    }
    cur.insert(last.clone(), value);
}

fn ensure_object<'a>(leg: &'a mut Map<String, Value>, key: &str) -> &'a mut Map<String, Value> {
    let slot = leg
        .entry(key.to_string())
        .or_insert_with(|| Value::Object(Map::new()));
    if !slot.is_object() {
        *slot = Value::Object(Map::new());
    }
    slot.as_object_mut().expect("just made it an object")
}

fn format_freq(freq: Frequency) -> String {
    match freq {
        Frequency::Minute(n) => format!("{n}m"),
        Frequency::Hour(n) => format!("{n}h"),
        Frequency::Day(n) => format!("{n}d"),
        Frequency::Week(n) => format!("{n}w"),
        Frequency::Month(n) => format!("{n}M"),
    }
}

fn value_kind(v: &Value) -> &'static str {
    match v {
        Value::Null => "null",
        Value::Bool(_) => "bool",
        Value::Number(_) => "number",
        Value::String(_) => "string",
        Value::Array(_) => "array",
        Value::Object(_) => "object",
    }
}

// ---------------------------------------------------------------------------
// Typed CostConfig — the deserialized form of the folded tree.
// ---------------------------------------------------------------------------

/// The fully-folded cost configuration: three legs, each with a default plus
/// optional per-scope overrides.
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CostConfig {
    #[serde(default)]
    pub(super) commission: LegConfig<CommissionSpec>,
    #[serde(default)]
    pub(super) spread: LegConfig<SpreadSpec>,
    #[serde(default)]
    pub(super) slippage: LegConfig<SlippageSpec>,
}

/// One leg's configuration: a default and any per-scope overrides.
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields, bound(deserialize = "T: Deserialize<'de>"))]
pub(super) struct LegConfig<T> {
    #[serde(default = "none_option")]
    pub(super) default: Option<T>,
    #[serde(default = "HashMap::new")]
    pub(super) by_symbol: HashMap<String, T>,
    #[serde(default = "HashMap::new")]
    pub(super) by_interval: HashMap<String, T>,
    #[serde(default = "Vec::new")]
    pub(super) scoped: Vec<ScopedEntry<T>>,
}

fn none_option<T>() -> Option<T> {
    None
}

// `Default` bound is not on T, so derive doesn't fit — write it by hand.
impl<T> Default for LegConfig<T> {
    fn default() -> Self {
        Self {
            default: None,
            by_symbol: HashMap::new(),
            by_interval: HashMap::new(),
            scoped: Vec::new(),
        }
    }
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields, bound(deserialize = "T: Deserialize<'de>"))]
pub(super) struct ScopedEntry<T> {
    symbol: String,
    freq: String,
    model: T,
}

impl<T> LegConfig<T> {
    /// Resolve to the winning leg model for a `(symbol, freq)` pair, applying
    /// specificity precedence (symbol+freq > symbol > freq > default) and
    /// insertion order as a same-specificity tie-breaker (later wins in the
    /// scoped list; the `by_symbol` / `by_interval` maps naturally overwrite on
    /// later insertions).
    fn resolve(&self, symbol: &str, freq: Option<Frequency>) -> Option<&T> {
        if let Some(freq) = freq {
            let freq_str = format_freq(freq);
            // Full symbol+freq — scan scoped list in reverse so the *last*
            // matching entry wins on same specificity.
            for entry in self.scoped.iter().rev() {
                if entry.symbol == symbol && entry.freq == freq_str {
                    return Some(&entry.model);
                }
            }
            if let Some(m) = self.by_symbol.get(symbol) {
                return Some(m);
            }
            if let Some(m) = self.by_interval.get(&freq_str) {
                return Some(m);
            }
        } else {
            if let Some(m) = self.by_symbol.get(symbol) {
                return Some(m);
            }
        }
        self.default.as_ref()
    }
}

// ---------------------------------------------------------------------------
// Model enums (tagged by `kind:`) with a build → live-model method.
// ---------------------------------------------------------------------------

#[derive(Debug, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
pub(super) enum CommissionSpec {
    None,
    Fixed { amount: Real },
    Percentage { rate: Real },
    PerUnit { rate: Real },
    Composite { parts: Vec<CommissionSpec> },
    Max { lhs: Box<CommissionSpec>, rhs: Box<CommissionSpec> },
}

impl CommissionSpec {
    fn build(&self) -> Box<dyn CommissionModel> {
        match self {
            CommissionSpec::None => Box::new(NoCommission),
            CommissionSpec::Fixed { amount } => Box::new(FixedCommission::new(*amount)),
            CommissionSpec::Percentage { rate } => Box::new(PercentageCommission::new(*rate)),
            CommissionSpec::PerUnit { rate } => Box::new(PerUnitCommission::new(*rate)),
            CommissionSpec::Composite { parts } => {
                Box::new(CompositeCommission::new(parts.iter().map(|p| p.build()).collect()))
            }
            CommissionSpec::Max { lhs, rhs } => {
                Box::new(MaxCommission::new(lhs.build(), rhs.build()))
            }
        }
    }
}

#[derive(Debug, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
pub(super) enum SpreadSpec {
    None,
    Bps { bps: Real },
    Absolute { amount: Real },
}

impl SpreadSpec {
    fn build(&self) -> Box<dyn SpreadModel> {
        match self {
            SpreadSpec::None => Box::new(NoSpread),
            SpreadSpec::Bps { bps } => Box::new(FixedBpsSpread::new(*bps)),
            SpreadSpec::Absolute { amount } => Box::new(FixedAbsoluteSpread::new(*amount)),
        }
    }
}

#[derive(Debug, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
pub(super) enum SlippageSpec {
    None,
    Bps {
        bps: Real,
        #[serde(default)]
        stop_multiplier: Option<Real>,
    },
    VolumeParticipation {
        coefficient: Real,
        #[serde(default)]
        exponent: Option<Real>,
        #[serde(default)]
        stop_multiplier: Option<Real>,
    },
}

impl SlippageSpec {
    fn build(&self) -> Box<dyn SlippageModel> {
        match self {
            SlippageSpec::None => Box::new(NoSlippage),
            SlippageSpec::Bps { bps, stop_multiplier } => {
                let mut s = FixedBpsSlippage::new(*bps);
                if let Some(m) = stop_multiplier {
                    s = s.with_stop_multiplier(*m);
                }
                Box::new(s)
            }
            SlippageSpec::VolumeParticipation {
                coefficient,
                exponent,
                stop_multiplier,
            } => {
                let mut s = VolumeParticipationSlippage::new(*coefficient);
                if let Some(e) = exponent {
                    s = s.with_exponent(*e);
                }
                if let Some(m) = stop_multiplier {
                    s = s.with_stop_multiplier(*m);
                }
                Box::new(s)
            }
        }
    }
}

impl CostConfig {
    /// Whether every leg is empty (no default, no scoped, no by-something) —
    /// what `--costs none` and an absent `--costs` flag both resolve to.
    pub fn is_none(&self) -> bool {
        leg_is_empty(&self.commission)
            && leg_is_empty(&self.spread)
            && leg_is_empty(&self.slippage)
    }

    /// Whether a per-leg `default` exists (used by the check subcommand's
    /// summary line — nothing to build vs. built one).
    pub fn has_any_default(&self) -> bool {
        self.commission.default.is_some()
            || self.spread.default.is_some()
            || self.slippage.default.is_some()
    }

    /// The count of scoped entries across every leg, for diagnostic printing.
    pub fn scoped_count(&self) -> usize {
        fn n<T>(leg: &LegConfig<T>) -> usize {
            leg.by_symbol.len() + leg.by_interval.len() + leg.scoped.len()
        }
        n(&self.commission) + n(&self.spread) + n(&self.slippage)
    }

    /// Build the live [`TradingCosts`] for `(symbol, freq)`. A leg whose
    /// resolution is `None` becomes the [`No*`](fugazi::costs::NoCommission)
    /// zero-cost default.
    pub fn resolve(&self, symbol: &str, freq: Option<Frequency>) -> TradingCosts {
        let commission = self
            .commission
            .resolve(symbol, freq)
            .map(|s| s.build())
            .unwrap_or_else(|| Box::new(NoCommission));
        let spread = self
            .spread
            .resolve(symbol, freq)
            .map(|s| s.build())
            .unwrap_or_else(|| Box::new(NoSpread));
        let slippage = self
            .slippage
            .resolve(symbol, freq)
            .map(|s| s.build())
            .unwrap_or_else(|| Box::new(NoSlippage));
        TradingCosts::new(commission, spread, slippage)
    }
}

fn leg_is_empty<T>(leg: &LegConfig<T>) -> bool {
    leg.default.is_none()
        && leg.by_symbol.is_empty()
        && leg.by_interval.is_empty()
        && leg.scoped.is_empty()
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------
