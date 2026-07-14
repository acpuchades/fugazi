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
    let config: CostConfig = serde_json::from_value(Value::Object(tree))
        .context("interpreting the resolved --costs tree")?;
    Ok(config)
}

/// The model enums' variant names. A YAML `!percentage { rate: X }` parses to
/// the singleton object `{percentage: {rate: X}}` — serde's external-tag form,
/// which the enums below deserialize natively. This list is what lets the
/// *untyped* passes (leg normalization, deep merge) recognize such a node as a
/// fully-formed model rather than a bag of fields.
const MODEL_VARIANTS: &[&str] = &[
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

/// Whether `obj` is a model in external-tag form — the `{percentage: {…}}`
/// singleton a `!percentage { … }` tag parses to.
fn is_model(obj: &Map<String, Value>) -> bool {
    obj.len() == 1
        && obj
            .keys()
            .next()
            .is_some_and(|k| MODEL_VARIANTS.contains(&k.as_str()))
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
            let value = input::parse_value_at(&text, &source.label())?;
            let normalized = normalize_preset(value)
                .with_context(|| format!("normalizing --costs preset {}", source.label()))?;
            deep_merge_into(tree, normalized);
        }
        CostTerm::Set { scope, key, value } => {
            set_scoped(tree, scope, key, value.clone())?;
        }
    }
    Ok(())
}

/// Turn a user-facing preset into the canonical structured form. A leg whose
/// value is a flat model (`!percentage { … }`) is hoisted into `default:`. A
/// leg that already has `default:`/`by_symbol:`/`by_interval:` stays as-is.
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
/// * a flat model — `{percentage: {...}}`, i.e. what `!percentage { ... }`
///   parses to — becomes `{default: <it>}`.
/// * a `{default: ..., by_symbol: {...}, by_interval: {...}}` structured shape —
///   pass through.
fn normalize_leg(node: Value) -> Result<Value> {
    let Value::Object(obj) = node else {
        return Ok(node);
    };
    let has_structured = obj.contains_key("default")
        || obj.contains_key("by_symbol")
        || obj.contains_key("by_interval")
        || obj.contains_key("scoped");

    if is_model(&obj) {
        let mut wrapper = Map::new();
        wrapper.insert("default".to_string(), Value::Object(obj));
        return Ok(Value::Object(wrapper));
    }
    if !has_structured {
        // Neither flat nor structured — refuse rather than silently accept an
        // empty leg the user thought they were configuring.
        bail!(
            "cost leg must be either a flat model (`!variant {{…}}`) \
             or a structured `{{default, by_symbol, by_interval}}` mapping"
        );
    }
    Ok(Value::Object(obj))
}

/// Deep-merge `src` into `dest`: object keys recurse (later overrides), scalars
/// and arrays are replaced wholesale. A fully-formed model on either side
/// short-circuits recursion and replaces the previous one wholesale — otherwise
/// a `commission: !percentage { rate }` layer overwritten by a later
/// `commission: !max { lhs, rhs }` would merge into a two-variant object that no
/// externally-tagged enum can read.
fn deep_merge_into(dest: &mut Map<String, Value>, src: Map<String, Value>) {
    for (k, v) in src {
        match (dest.get_mut(&k), v) {
            (Some(Value::Object(inner_dest)), Value::Object(inner_src)) => {
                if is_model(inner_dest) || is_model(&inner_src) {
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
/// scope-less `commission.percentage.rate=…` writes into
/// `commission.default.percentage.rate` when the first segment after the leg is
/// a leaf name (i.e. not one of `default`, `by_symbol`, `by_interval`,
/// `scoped`). Note that the dotted path is a literal address into the tree —
/// the model's variant is a level of it, so nudging one field of a loaded
/// preset names the variant (`commission.percentage.rate=0.00075`). Naming the
/// wrong variant plants a second key at the model position, which the
/// externally-tagged enum rejects rather than silently half-applying.
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
        if path[0] == "default" {
            check_variant(leg, target_leg.get("default"), &path[1..])?;
        }
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
                check_variant(leg, Some(&*existing), rest)?;
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
                check_variant(leg, Some(&*existing), rest)?;
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

/// A dotted setter that reaches *into* a model — `commission.percentage.rate` —
/// names the variant as a path segment, because the path is a literal address
/// into the spec tree. Check that name against the model already sitting there,
/// so aiming the wrong variant at a loaded preset says so plainly instead of
/// surfacing later as serde's "expected map with a single key".
///
/// `path` is the remainder *inside* the model position (so `["percentage",
/// "rate"]`); an absent or empty `existing` means nothing is loaded yet and the
/// setter is building the model from scratch, which is fine.
fn check_variant(leg: &str, existing: Option<&Value>, path: &[String]) -> Result<()> {
    let Some(named) = path.first() else {
        return Ok(());
    };
    if !MODEL_VARIANTS.contains(&named.as_str()) {
        return Ok(());
    }
    let Some(Value::Object(obj)) = existing else {
        return Ok(());
    };
    if !is_model(obj) {
        return Ok(());
    }
    let current = obj.keys().next().expect("is_model checked len == 1");
    if current != named {
        bail!(
            "cost setter `{leg}.{}`: the {leg} model is `!{current}`, not `!{named}` — \
             nudge a field `!{current}` actually has, or replace the model outright \
             with `{leg}=!{named} {{ … }}`",
            path.join("."),
        );
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
// Model enums with a build → live-model method. Externally tagged, so a variant
// is spelled `!percentage { rate: 0.001 }` — the same YAML tag vocabulary the
// strategy spec (`ExprSpec` / `SignalSpec`) uses.
// ---------------------------------------------------------------------------

#[derive(Debug, Deserialize)]
#[serde(rename_all = "snake_case", deny_unknown_fields)]
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
#[serde(rename_all = "snake_case", deny_unknown_fields)]
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
#[serde(rename_all = "snake_case", deny_unknown_fields)]
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
