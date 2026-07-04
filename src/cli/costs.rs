//! `--costs` spec parsing for the `run` and `optimize` subcommands.
//!
//! Same shape as `--params`/`--overlay`: a `,`-separated list of terms, each of
//! which is either a whole-file loader (`@file.yml`), an explicit-none literal
//! (`none`), or a `key=value` setter — optionally prefixed with a `SYMBOL[FREQ]:`
//! scope (a subset of the [`crate::overlay`] grammar). Multiple `--costs` flags
//! are folded left-to-right; later terms override earlier ones at the same
//! specificity, and more-specific scopes win over less-specific ones at
//! resolution time.
//!
//! ```text
//! --costs @binance.yml
//! --costs @binance.yml,commission.rate=0.0004
//! --costs 'commission=!percentage { rate: 0.001 },spread=!bps { bps: 5 }'
//! --costs 'BTCUSDT[1m]:slippage=!volume_participation { coefficient: 0.3 }'
//! --costs none
//! ```
//!
//! The intermediate representation is a `serde_json::Value` tree whose top-level
//! keys are `commission`, `spread`, `slippage`; each leg carries a `default`
//! plus optional `by_symbol` / `by_interval` maps and a `scoped` list. Terms
//! deep-merge into it, and the final tree is deserialized to a typed
//! [`CostConfig`] via serde. [`CostConfig::resolve`] then picks the winning
//! model per leg for a given `(symbol, frequency)` and returns a live
//! [`TradingCosts`] the wallet consumes.

use std::collections::HashMap;
use std::str::FromStr;

use anyhow::{Context, Result, anyhow, bail};
use fugazi::costs::{
    CommissionModel, CompositeCommission, FixedAbsoluteSpread, FixedBpsSlippage, FixedBpsSpread,
    FixedCommission, MaxCommission, NoCommission, NoSlippage, NoSpread, PercentageCommission,
    PerUnitCommission, SlippageModel, SpreadModel, TradingCosts, VolumeParticipationSlippage,
};
use fugazi::types::Real;
use serde::Deserialize;
use serde_json::{Map, Value};

use crate::calendar::Frequency;
use crate::input::{self, Source};

// ---------------------------------------------------------------------------
// Parsing: one `--costs` argument
// ---------------------------------------------------------------------------

/// One `--costs` argument as parsed off the command line: a sequence of terms
/// (`@file`, `[SCOPE:]key=value`, or the literal `none`) applied in order.
#[derive(Debug, Clone)]
pub struct CostSpec(Vec<CostTerm>);

/// One term of a `--costs` spec.
#[derive(Debug, Clone)]
enum CostTerm {
    /// `@file.yml` — a whole cost-model preset.
    Load(Source),
    /// `[SCOPE:]key=value` — a single leaf-or-model setter.
    Set {
        scope: Scope,
        key: Vec<String>,
        value: Value,
    },
    /// The literal `none` — reset every leg to its no-op default. Doubles as the
    /// user's way of silencing the "no cost model set" warning banner without
    /// activating a model.
    None,
}

impl FromStr for CostSpec {
    type Err = String;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        let mut terms = Vec::new();
        for chunk in split_top_commas(s).map_err(|e| e.to_string())? {
            let chunk = chunk.trim();
            if chunk.is_empty() {
                continue;
            }
            terms.push(parse_term(chunk).map_err(|e| e.to_string())?);
        }
        Ok(CostSpec(terms))
    }
}

/// Parse one term: `@file`, `none`, or `[SCOPE:]key=value`.
fn parse_term(text: &str) -> Result<CostTerm> {
    if text == "none" {
        return Ok(CostTerm::None);
    }
    if let Some(path) = text.strip_prefix('@') {
        let path = path.trim();
        if path.is_empty() {
            bail!("`@` prefix is missing a file path");
        }
        // Source::from_str strips '@' — we've already stripped it, so build the
        // File variant directly to avoid a round-trip that would classify it
        // as inline text.
        return Ok(CostTerm::Load(Source::File(path.into())));
    }
    let (scope, body) = split_scope(text)?;
    let body = body.trim();
    if body.is_empty() {
        bail!("cost term has an empty body");
    }
    let (key_str, value_str) = body
        .split_once('=')
        .ok_or_else(|| anyhow!("cost term {body:?} is missing `=`"))?;
    let key: Vec<String> = key_str.trim().split('.').map(String::from).collect();
    if key.iter().any(String::is_empty) {
        bail!("cost term key {key_str:?}: empty segment");
    }
    if !matches!(key[0].as_str(), "commission" | "spread" | "slippage") {
        bail!(
            "cost term key {key_str:?}: first segment must be one of \
             `commission`, `spread`, `slippage` (got `{}`)",
            key[0]
        );
    }
    let value = input::parse_value(value_str.trim())
        .with_context(|| format!("parsing cost value for `{key_str}`"))?;
    Ok(CostTerm::Set { scope, key, value })
}

// ---------------------------------------------------------------------------
// Scope
// ---------------------------------------------------------------------------

/// The `SYMBOL[FREQ]:` prefix (either half optional; both empty = the default
/// scope). Kept private to the folding path — public callers see the resolved
/// [`TradingCosts`].
#[derive(Debug, Clone, Default, PartialEq)]
struct Scope {
    symbol: Option<String>,
    freq: Option<Frequency>,
}

impl Scope {
    fn is_default(&self) -> bool {
        self.symbol.is_none() && self.freq.is_none()
    }
}

/// Split off a leading `SYMBOL[FREQ]:` scope prefix. Same rules as the
/// [`crate::overlay`] parser: a `:` at bracket depth zero is the separator; the
/// first `=` at depth zero without a preceding `:` means "no scope, start of an
/// inline pair".
fn split_scope(text: &str) -> Result<(Scope, &str)> {
    let mut depth: i32 = 0;
    for (i, ch) in text.char_indices() {
        match ch {
            '{' | '[' => depth += 1,
            '}' | ']' => depth -= 1,
            ':' if depth == 0 => {
                let scope_text = text[..i].trim();
                let body = &text[i + 1..];
                return Ok((parse_scope(scope_text)?, body));
            }
            '=' if depth == 0 => break,
            _ => {}
        }
    }
    Ok((Scope::default(), text))
}

fn parse_scope(text: &str) -> Result<Scope> {
    let text = text.trim();
    if text.is_empty() {
        return Ok(Scope::default());
    }
    let (symbol_part, freq_part) = match text.find('[') {
        Some(open) => {
            if !text.ends_with(']') {
                bail!("cost scope {text:?}: `[freq]` bracket must close at the end");
            }
            let symbol = text[..open].trim();
            let freq = &text[open + 1..text.len() - 1];
            (symbol, Some(freq))
        }
        None => (text, None),
    };
    let symbol = if symbol_part.is_empty() {
        None
    } else {
        Some(symbol_part.to_string())
    };
    let freq = match freq_part {
        Some(f) => {
            let f = f.trim();
            if f.is_empty() {
                bail!("cost scope {text:?}: empty `[freq]` bracket");
            }
            Some(
                Frequency::from_str(f)
                    .map_err(|e| anyhow!("cost scope {text:?}: {e}"))?,
            )
        }
        None => None,
    };
    if symbol.is_none() && freq.is_none() {
        bail!("cost scope {text:?}: neither symbol nor freq present");
    }
    Ok(Scope { symbol, freq })
}

// ---------------------------------------------------------------------------
// Top-level `,`-splitter — respects `{...}` / `[...]` / `"..."` grouping so a
// term like `commission=!percentage { rate: 0.001 }` stays a single term.
// ---------------------------------------------------------------------------

fn split_top_commas(s: &str) -> Result<Vec<String>> {
    let mut out = Vec::new();
    let mut buf = String::new();
    let mut depth: i32 = 0;
    let mut in_str = false;
    let mut prev = '\0';
    for c in s.chars() {
        if in_str {
            buf.push(c);
            if c == '"' && prev != '\\' {
                in_str = false;
            }
        } else {
            match c {
                '"' => {
                    in_str = true;
                    buf.push(c);
                }
                '{' | '[' => {
                    depth += 1;
                    buf.push(c);
                }
                '}' | ']' => {
                    depth -= 1;
                    if depth < 0 {
                        bail!("unexpected `{c}` in cost spec");
                    }
                    buf.push(c);
                }
                ',' if depth == 0 => {
                    out.push(std::mem::take(&mut buf));
                }
                _ => buf.push(c),
            }
        }
        prev = c;
    }
    if depth != 0 {
        bail!("unclosed bracket in cost spec");
    }
    if !buf.is_empty() {
        out.push(buf);
    }
    Ok(out)
}

// ---------------------------------------------------------------------------
// Folding: multiple CostSpec → one Value tree → typed CostConfig
// ---------------------------------------------------------------------------

/// Fold all `--costs` specs left-to-right into a single [`CostConfig`]. Deep
/// merge for objects (later keys win, missing keys inherit), full replacement
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
    commission: LegConfig<CommissionSpec>,
    #[serde(default)]
    spread: LegConfig<SpreadSpec>,
    #[serde(default)]
    slippage: LegConfig<SlippageSpec>,
}

/// One leg's configuration: a default and any per-scope overrides.
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields, bound(deserialize = "T: Deserialize<'de>"))]
struct LegConfig<T> {
    #[serde(default = "none_option")]
    default: Option<T>,
    #[serde(default = "HashMap::new")]
    by_symbol: HashMap<String, T>,
    #[serde(default = "HashMap::new")]
    by_interval: HashMap<String, T>,
    #[serde(default = "Vec::new")]
    scoped: Vec<ScopedEntry<T>>,
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
struct ScopedEntry<T> {
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
enum CommissionSpec {
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
enum SpreadSpec {
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
enum SlippageSpec {
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

#[cfg(test)]
mod tests {
    use super::*;

    fn parse(spec: &str) -> CostSpec {
        spec.parse().unwrap()
    }

    fn config_of(specs: &[&str]) -> CostConfig {
        let specs: Vec<CostSpec> = specs.iter().map(|s| parse(s)).collect();
        config(&specs).unwrap()
    }

    #[test]
    fn empty_specs_produce_empty_config() {
        let cfg = config(&[]).unwrap();
        assert!(cfg.is_none());
    }

    #[test]
    fn none_literal_resets_prior_layers() {
        let cfg = config_of(&["commission=!percentage { rate: 0.001 }", "none"]);
        assert!(cfg.is_none());
    }

    #[test]
    fn inline_commission_sets_default() {
        let cfg = config_of(&["commission=!percentage { rate: 0.001 }"]);
        assert!(matches!(
            cfg.commission.default,
            Some(CommissionSpec::Percentage { rate }) if (rate - 0.001).abs() < 1e-12
        ));
    }

    #[test]
    fn dotted_key_targets_default_leaf() {
        // The first term establishes the default; the second nudges its `rate`.
        let cfg = config_of(&[
            "commission=!percentage { rate: 0.001 }",
            "commission.rate=0.0004",
        ]);
        assert!(matches!(
            cfg.commission.default,
            Some(CommissionSpec::Percentage { rate }) if (rate - 0.0004).abs() < 1e-12
        ));
    }

    #[test]
    fn symbol_scoped_overrides_default_at_resolution() {
        let cfg = config_of(&[
            "spread=!bps { bps: 10 }",
            "BTC:spread=!bps { bps: 3 }",
        ]);
        // Global default is 10 bps; BTC gets its own 3 bps.
        let btc = cfg.resolve("BTC", None);
        let eth = cfg.resolve("ETH", None);
        // A 100-price probe: BTC's half-spread is 0.015; ETH's is 0.05.
        let b = fugazi::types::Candle::new(100.0, 100.0, 100.0, 100.0, 0.0);
        assert!((btc.spread.half_spread(100.0, &b) - 0.015).abs() < 1e-9);
        assert!((eth.spread.half_spread(100.0, &b) - 0.05).abs() < 1e-9);
    }

    #[test]
    fn symbol_plus_freq_wins_over_symbol_only() {
        let cfg = config_of(&[
            "BTC:spread=!bps { bps: 10 }",
            "BTC[1d]:spread=!bps { bps: 2 }",
        ]);
        let b = fugazi::types::Candle::new(100.0, 100.0, 100.0, 100.0, 0.0);
        let daily = cfg.resolve("BTC", Some(Frequency::Day(1)));
        let hourly = cfg.resolve("BTC", Some(Frequency::Hour(1)));
        // Daily gets the more-specific 2 bps; hourly falls back to the 10-bps
        // symbol-only entry.
        assert!((daily.spread.half_spread(100.0, &b) - 0.01).abs() < 1e-9);
        assert!((hourly.spread.half_spread(100.0, &b) - 0.05).abs() < 1e-9);
    }

    #[test]
    fn later_scoped_entry_wins_at_same_specificity() {
        // Two same-scope entries; later wins.
        let cfg = config_of(&[
            "BTC[1d]:spread=!bps { bps: 5 }",
            "BTC[1d]:spread=!bps { bps: 2 }",
        ]);
        let b = fugazi::types::Candle::new(100.0, 100.0, 100.0, 100.0, 0.0);
        let daily = cfg.resolve("BTC", Some(Frequency::Day(1)));
        assert!((daily.spread.half_spread(100.0, &b) - 0.01).abs() < 1e-9);
    }

    #[test]
    fn preset_flat_leg_normalizes_to_default() {
        let yaml = r#"
            commission:
              kind: percentage
              rate: 0.001
            spread:
              kind: bps
              bps: 5
        "#;
        let cfg = config(&[CostSpec(vec![CostTerm::Load(Source::Inline(yaml.to_string()))])])
            .unwrap();
        assert!(matches!(
            cfg.commission.default,
            Some(CommissionSpec::Percentage { .. })
        ));
        assert!(matches!(cfg.spread.default, Some(SpreadSpec::Bps { .. })));
    }

    #[test]
    fn preset_structured_by_symbol_populates_map() {
        let yaml = r#"
            spread:
              default: { kind: bps, bps: 2 }
              by_symbol:
                BTC: { kind: bps, bps: 1 }
                ETH: { kind: bps, bps: 1.5 }
        "#;
        let cfg = config(&[CostSpec(vec![CostTerm::Load(Source::Inline(yaml.to_string()))])])
            .unwrap();
        let b = fugazi::types::Candle::new(100.0, 100.0, 100.0, 100.0, 0.0);
        let btc = cfg.resolve("BTC", None);
        let eth = cfg.resolve("ETH", None);
        let other = cfg.resolve("XRP", None);
        assert!((btc.spread.half_spread(100.0, &b) - 0.005).abs() < 1e-9);
        assert!((eth.spread.half_spread(100.0, &b) - 0.0075).abs() < 1e-9);
        assert!((other.spread.half_spread(100.0, &b) - 0.01).abs() < 1e-9);
    }

    #[test]
    fn rejects_unknown_leg() {
        let err = CostSpec::from_str("wallet=!percentage { rate: 0.001 }").unwrap_err();
        assert!(err.contains("commission"));
    }

    #[test]
    fn rejects_bad_scope_prefix() {
        let err = CostSpec::from_str("BTC[NOPE]:spread=!bps { bps: 1 }").unwrap_err();
        assert!(err.contains("scope"));
    }

    #[test]
    fn rejects_unknown_model_kind() {
        // The build path is where the typed deserialize runs; check we hit it.
        let err = config(&[parse("commission=!martian { rate: 0.001 }")]).unwrap_err();
        let msg = format!("{err:#}");
        assert!(
            msg.contains("kind") || msg.contains("commission") || msg.contains("martian"),
            "unexpected error: {msg}"
        );
    }
}
