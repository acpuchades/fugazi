//! `--params` substitution for a strategy spec.
//!
//! The strategy spec ([`crate::spec`]) deserializes into strongly-typed serde
//! enums, where a `period` is a `usize`, a `k` is a `Real`, and so on — there is
//! no room to drop a `param` placeholder where a number is expected during typed
//! parsing. So substitution happens in a **first pass over the untyped value
//! tree**: the document is normalized to a [`serde_json::Value`] (see
//! [`crate::spec::convert`]), every placeholder node is rewritten to its resolved value
//! here, and only then is the result deserialized into the typed spec.
//!
//! A placeholder is a singleton object keyed `param` — written `!param { … }` in
//! YAML (the tag becomes that object via [`crate::spec::convert`]) or, in flow/map form,
//! `{ param: { … } }`:
//!
//! ```yaml
//! period: !param { key: FAST }                # required — must be passed
//! period: !param { key: SLOW, default: 8 }    # optional — falls back to 8
//! root: !param SYM                             # bare-string shorthand for { key: SYM }
//! period: !param { key: FAST, type: integer }  # declared — coerced and checked here
//! ```
//!
//! The optional `type:` ([`ParamType`]) is what turns "whatever the value
//! happened to parse as" into something the load pass can check and correct;
//! omitted or `null`, the heuristics stand. See [`crate::spec::param_type`].

use std::collections::HashMap;
use std::str::FromStr;

use anyhow::{Context, Result, anyhow, bail};
use serde_json::{Map, Value};

use crate::spec::input::{self, Source};
use crate::spec::param_type::{ParamType, parse_declaration};

/// One term of a `--params` spec: set a single value, or load a mapping file.
#[derive(Debug, Clone)]
enum ParamTerm {
    /// `NAME=value` — the value parsed leniently as a JSON scalar (so `FAST=3` is a
    /// number and `SYM=BTC` a string).
    Set { name: String, value: Value },
    /// `@file.yml` — a whole `NAME: value` mapping.
    Load(Source),
}

/// One `--params` argument: a `,`-separated list of terms, exactly like
/// `--series` (e.g. `@base.yml,FAST=3,SLOW=8`). Terms apply left-to-right, and the
/// flag is itself repeatable, so a later term/flag overrides an earlier one.
#[derive(Debug, Clone)]
pub struct ParamSpec(Vec<ParamTerm>);

impl FromStr for ParamSpec {
    type Err = String;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        let mut terms = Vec::new();
        for term in split_terms(s) {
            let term = term.trim();
            if term.is_empty() {
                continue;
            }
            terms.push(parse_term(term)?);
        }
        Ok(ParamSpec(terms))
    }
}

/// Split a `--params` spec by top-level `,` — commas inside `[...]` / `{...}`
/// brackets or `"..."` quotes are kept, so a term like `FAST=[3,5,8]` (an
/// `optimize` sweep list, JSON-shaped) stays one term rather than splitting into
/// `FAST=[3`, `5`, `8]`.
fn split_terms(s: &str) -> Vec<String> {
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
                '[' | '{' => {
                    depth += 1;
                    buf.push(c);
                }
                ']' | '}' => {
                    depth = depth.saturating_sub(1);
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
    if !buf.is_empty() {
        out.push(buf);
    }
    out
}

fn parse_term(term: &str) -> Result<ParamTerm, String> {
    if term.starts_with('@') {
        // `Source::from_str` is infallible; `@path` yields a `File`.
        Ok(ParamTerm::Load(term.parse().expect("infallible")))
    } else if let Some((name, raw)) = term.split_once('=') {
        Ok(ParamTerm::Set {
            name: name.trim().to_string(),
            value: scalar(raw),
        })
    } else {
        Err(format!(
            "invalid --params term `{term}`: expected NAME=value or @file"
        ))
    }
}

/// Parse a `NAME=value` param value: JSON if it parses (`3` → number, `true` →
/// bool, `"x"` → string), otherwise a bare string (so `BTC` works without quotes).
fn scalar(raw: &str) -> Value {
    serde_json::from_str(raw).unwrap_or_else(|_| Value::String(raw.to_string()))
}

/// Fold all `--params` specs into a name → value table, applying every term
/// left-to-right (so a later term wins).
pub fn table(specs: &[ParamSpec]) -> Result<HashMap<String, Value>> {
    let mut table = HashMap::new();
    for spec in specs {
        for term in &spec.0 {
            match term {
                ParamTerm::Set { name, value } => {
                    table.insert(name.clone(), value.clone());
                }
                ParamTerm::Load(src) => {
                    let text = src.read().context("reading params file")?;
                    let value = input::parse_value_at(&text, &src.label())?;
                    match value {
                        Value::Object(map) => table.extend(map),
                        _ => bail!(
                            "params file {} must be a mapping of NAME: value",
                            src.label()
                        ),
                    }
                }
            }
        }
    }
    Ok(table)
}

/// Rewrite every `param` placeholder in `value` to its resolved value, recursing
/// through objects and arrays.
/// The tag an author writes for a deliberate hole: `period: !undefined`.
///
/// Converted to a singleton object by `yaml_to_json` like every other tag, so
/// it is recognised the same way `!param` is. No spec enum has an `undefined`
/// variant, which is what makes the match unambiguous.
const UNDEFINED_KEY: &str = "undefined";

pub fn substitute(value: Value, params: &HashMap<String, Value>) -> Result<Value> {
    match value {
        // `!undefined` validates under `fugazi check` and nowhere else: it
        // stands for a decision not yet made, so a spec still carrying one
        // cannot be run. Rejecting it here — rather than letting it reach the
        // typed parse as an "invalid type" — is what makes that message
        // actionable.
        Value::Object(map) if map.len() == 1 && map.contains_key(UNDEFINED_KEY) => {
            bail!(
                "`!undefined` is a check-time placeholder — `fugazi check` accepts it so the \
                 rest of the document can be validated, but a value has to be supplied before \
                 the spec can run"
            )
        }
        // A `{param: …}` singleton object is a placeholder (no spec enum has a
        // `param` variant, so this is unambiguous).
        Value::Object(map) if map.len() == 1 && map.contains_key("param") => {
            resolve(&map["param"], params)
        }
        Value::Object(map) => {
            let mut out = Map::new();
            for (k, v) in map {
                out.insert(k, substitute(v, params)?);
            }
            Ok(Value::Object(out))
        }
        Value::Array(seq) => seq
            .into_iter()
            .map(|v| substitute(v, params))
            .collect::<Result<Vec<_>>>()
            .map(Value::Array),
        other => Ok(other),
    }
}

/// Rewrite every `param` placeholder in `value` **only when its key
/// appears in `params`** — every other placeholder (including one with a
/// `default`) is left in place for the outer [`substitute`] pass to
/// resolve later.
///
/// Used by [`crate::spec::imports`] when an `!import` node carries inline
/// `params: { … }`: the imported subtree is *partially* resolved with
/// those inline values, and any placeholder whose key isn't listed
/// inline falls through unchanged. That gives callers scope-limited
/// overrides — one shared strategy fragment can be imported N times with
/// N distinct parameterizations — without eagerly applying `default`s or
/// erroring on the outer document's own `--params` keys.
pub fn substitute_partial(value: Value, params: &HashMap<String, Value>) -> Result<Value> {
    match value {
        Value::Object(map) if map.len() == 1 && map.contains_key("param") => {
            // Extract the key if the placeholder is well-formed; if the
            // inline table has a value for it, substitute that value.
            // Otherwise leave the whole `{param: …}` node in place —
            // the outer pass gets to decide (via `--params`, a
            // `default`, or an error).
            //
            // A malformed body is *also* left in place: this pass has no
            // mandate to report one, and the outer pass — which sees every
            // placeholder, not just the inline-overridden ones — does.
            let Ok(p) = placeholder(&map["param"]) else {
                return Ok(Value::Object(map));
            };
            if let Some(value) = params.get(p.key) {
                return p.apply(value.clone());
            }
            Ok(Value::Object(map))
        }
        Value::Object(map) => {
            let mut out = Map::new();
            for (k, v) in map {
                out.insert(k, substitute_partial(v, params)?);
            }
            Ok(Value::Object(out))
        }
        Value::Array(seq) => seq
            .into_iter()
            .map(|v| substitute_partial(v, params))
            .collect::<Result<Vec<_>>>()
            .map(Value::Array),
        other => Ok(other),
    }
}

/// Resolve a single placeholder body (its `{ key, default, type }` object or
/// bare key name) against the supplied params.
fn resolve(body: &Value, params: &HashMap<String, Value>) -> Result<Value> {
    let p = placeholder(body)?;
    if let Some(value) = params.get(p.key) {
        p.apply(value.clone())
    } else if let Some(default) = p.default {
        // The `default:` is checked against the declaration too. An author who
        // writes `{ type: integer, default: 3.5 }` has contradicted themselves,
        // and the run that never passes the param is exactly the one where
        // nothing else would catch it.
        p.apply(default.clone())
    } else {
        bail!(
            "parameter `{key}` is not set (pass `--params {key}=…` or add a `default`)",
            key = p.key
        )
    }
}

/// A parsed placeholder body — the shared read of the `{ key, default, type }`
/// object and the bare-string form.
///
/// One type for both tags: `spec/args.rs` says the `!arg` grammar mirrors
/// `!param`, and this is where that claim is *enforced* rather than restated.
/// The only difference is which word the messages use, which
/// [`tag`](Self::tag) carries.
///
/// Errors on a malformed body (no string `key`, an unrecognized `type:`, a key
/// that isn't one of the three); an unset-but-well-formed key is not an error
/// here.
pub(crate) struct Placeholder<'a> {
    /// `"param"` or `"arg"` — the tag this body was written under, so a message
    /// names the thing the author actually typed.
    pub tag: &'static str,
    /// The `--params` name (or driver binding) this stands for.
    pub key: &'a str,
    /// What an unset name falls back to, when the body carries one.
    pub default: Option<&'a Value>,
    /// The declared type, when the body carries one. `None` is *apply the
    /// heuristics* — see [`crate::spec::param_type`].
    pub ty: Option<ParamType>,
}

impl Placeholder<'_> {
    /// Put a resolved value through the declaration, if there is one.
    pub fn apply(&self, value: Value) -> Result<Value> {
        match self.ty {
            None => Ok(value),
            Some(ty) => ty
                .coerce(value)
                .map_err(|why| anyhow!("{} `{}` {why}", self.noun(), self.key)),
        }
    }

    /// What the user calls this thing: a `--params` value is a *parameter*, a
    /// driver binding an *argument*.
    fn noun(&self) -> &'static str {
        if self.tag == "arg" {
            "argument"
        } else {
            "parameter"
        }
    }
}

/// The keys a `{ … }` placeholder body may carry. Closed, because the whole
/// value of a `type:` declaration is that it does something — a body that
/// tolerated `typ:` would silently ignore the very thing it was written for.
/// The same guard `deny_unknown_fields` gives every typed spec node; this body
/// is hand-parsed, so it has to state it.
const BODY_KEYS: [&str; 3] = ["key", "default", "type"];

/// Parse a `!param` body — [`placeholder_of`] with the tag this module owns.
pub(crate) fn placeholder(body: &Value) -> Result<Placeholder<'_>> {
    placeholder_of("param", body)
}

/// Parse a placeholder body written under `tag` (`"param"` or `"arg"`), which
/// is used only to word the messages. `spec::args` calls this for `!arg`, so
/// the two tags cannot grow different key sets or different type vocabularies.
pub(crate) fn placeholder_of<'a>(tag: &'static str, body: &'a Value) -> Result<Placeholder<'a>> {
    match body {
        Value::String(name) => Ok(Placeholder {
            tag,
            key: name.as_str(),
            default: None,
            ty: None,
        }),
        Value::Object(o) => {
            let key = o
                .get("key")
                .and_then(Value::as_str)
                .ok_or_else(|| anyhow!("`{tag}` needs a string `key`"))?;
            if let Some(unknown) = o.keys().find(|k| !BODY_KEYS.contains(&k.as_str())) {
                bail!(
                    "`{tag}` `{key}` has an unknown key `{unknown}` — a placeholder body takes \
                     `key`, `default` and `type`"
                );
            }
            let ty =
                parse_declaration(o.get("type")).map_err(|why| anyhow!("`{tag}` `{key}` {why}"))?;
            Ok(Placeholder {
                tag,
                key,
                default: o.get("default"),
                ty,
            })
        }
        _ => bail!("`{tag}` expects a key name or a `{{ key: NAME }}` object"),
    }
}

/// [`substitute`] for `fugazi check`: a required placeholder with no `--params`
/// value and no `default` becomes an [undefined](crate::spec::undefined) sentinel to
/// validate *around*, rather than the hard error [`substitute`] raises. A
/// malformed placeholder (no string `key`) still errors — that's a genuine
/// format mistake `check` should catch. Returns the rewritten tree and the
/// number of undefined placeholders introduced.
pub fn substitute_for_check(
    value: Value,
    params: &HashMap<String, Value>,
) -> Result<(Value, usize)> {
    let mut holes = 0;
    let value = substitute_for_check_inner(value, params, &mut holes, &mut Vec::new())?;
    Ok((value, holes))
}

fn substitute_for_check_inner(
    value: Value,
    params: &HashMap<String, Value>,
    holes: &mut usize,
    path: &mut Vec<String>,
) -> Result<Value> {
    match value {
        // `!undefined` — an author-written hole. Same machinery as an unset
        // `!param`, but with no name to key on. This pass owns the traversal,
        // so it knows exactly where in the document it is: name the sentinel
        // by that path. Two `!undefined`s therefore never collapse into one
        // entry, and never look like one placeholder demanded at two types.
        Value::Object(map) if map.len() == 1 && map.contains_key(UNDEFINED_KEY) => {
            *holes += 1;
            Ok(crate::spec::undefined::undefined_sentinel(&path.join(".")))
        }
        Value::Object(map) if map.len() == 1 && map.contains_key("param") => {
            let p = placeholder(&map["param"])?;
            if params.contains_key(p.key) || p.default.is_some() {
                resolve(&map["param"], params)
            } else {
                *holes += 1;
                // A hole has no value to coerce, so a declaration on it is a
                // *claim* instead: log it, so the report can say what the
                // missing `--params` value has to be and a position demanding
                // something else can be refused.
                if let Some(ty) = p.ty {
                    crate::spec::undefined::declare(
                        crate::spec::undefined::UndefinedOrigin::Param,
                        p.key,
                        ty,
                    );
                }
                Ok(crate::spec::undefined::sentinel(p.key))
            }
        }
        Value::Object(map) => {
            let mut out = Map::new();
            for (k, v) in map {
                path.push(k.clone());
                let v = substitute_for_check_inner(v, params, holes, path)?;
                path.pop();
                out.insert(k, v);
            }
            Ok(Value::Object(out))
        }
        Value::Array(seq) => {
            let mut out = Vec::with_capacity(seq.len());
            for (i, v) in seq.into_iter().enumerate() {
                path.push(format!("[{i}]"));
                out.push(substitute_for_check_inner(v, params, holes, path)?);
                path.pop();
            }
            Ok(Value::Array(out))
        }
        other => Ok(other),
    }
}

#[cfg(test)]
mod tests {

    /// The traded symbol of a single-asset spec, via the root analyser.
    fn sole(spec: &crate::spec::SingleStrategySpec) -> String {
        spec.root
            .sole_symbol("single-asset")
            .expect("root names one symbol")
    }
    use super::*;
    use crate::spec::SingleStrategySpec;
    use crate::spec::convert::yaml_to_json;
    use serde_json::json;

    fn table_of(specs: &[&str]) -> HashMap<String, Value> {
        let specs: Vec<ParamSpec> = specs.iter().map(|s| s.parse().unwrap()).collect();
        table(&specs).unwrap()
    }

    #[test]
    fn param_values_parse_as_json_scalars() {
        let map = table_of(&["FAST=3", "K=2.0", "SYM=BTC"]);
        assert_eq!(map["FAST"], Value::from(3));
        assert_eq!(map["K"], Value::from(2.0));
        assert_eq!(map["SYM"], Value::from("BTC"));
    }

    #[test]
    fn one_spec_holds_comma_separated_terms() {
        let map = table_of(&["FAST=3,SLOW=8,SYM=BTC"]);
        assert_eq!(map["FAST"], Value::from(3));
        assert_eq!(map["SLOW"], Value::from(8));
        assert_eq!(map["SYM"], Value::from("BTC"));
    }

    #[test]
    fn later_terms_win() {
        // Within one spec and across specs.
        assert_eq!(table_of(&["FAST=3,FAST=9"])["FAST"], Value::from(9));
        assert_eq!(table_of(&["FAST=3", "FAST=9"])["FAST"], Value::from(9));
    }

    #[test]
    fn param_rejects_bare_token() {
        assert!("FAST".parse::<ParamSpec>().is_err());
    }

    #[test]
    fn splitter_respects_brackets_and_ranges() {
        // `[3,5,8]` — inner commas belong to the array, not to term splitting.
        let map = table_of(&["FAST=[3,5,8],SLOW=13"]);
        assert_eq!(map["FAST"], serde_json::json!([3, 5, 8]));
        assert_eq!(map["SLOW"], Value::from(13));
        // Ranges have no commas, but coexisting with a list must still split cleanly.
        let map = table_of(&["FAST=3..10:1,SLOW=[13,21]"]);
        assert_eq!(map["FAST"], Value::from("3..10:1"));
        assert_eq!(map["SLOW"], serde_json::json!([13, 21]));
    }

    /// Substitute over a YAML doc (converted to JSON first, as the CLI does).
    fn sub(yaml: &str, pairs: &[&str]) -> Result<Value> {
        let value = yaml_to_json(serde_norway::from_str(yaml).unwrap()).unwrap();
        substitute(value, &table_of(pairs))
    }

    #[test]
    fn provided_value_wins_over_default() {
        let out = sub("period: !param { key: FAST, default: 8 }", &["FAST=3"]).unwrap();
        assert_eq!(out.get("period"), Some(&Value::from(3)));
    }

    #[test]
    fn falls_back_to_default_when_unset() {
        let out = sub("period: !param { key: FAST, default: 8 }", &[]).unwrap();
        assert_eq!(out.get("period"), Some(&Value::from(8)));
    }

    #[test]
    fn errors_when_unset_and_no_default() {
        let err = sub("period: !param { key: FAST }", &[]).unwrap_err();
        assert!(err.to_string().contains("FAST"));
    }

    #[test]
    fn bare_string_shorthand() {
        let out = sub("root: !param SYM", &["SYM=ETH"]).unwrap();
        assert_eq!(out.get("root"), Some(&Value::from("ETH")));
    }

    #[test]
    fn json_param_placeholder_resolves() {
        // The `{"param": …}` form straight from JSON (no YAML tag involved).
        let value: Value =
            serde_json::from_str(r#"{"period": {"param": {"key": "FAST"}}}"#).unwrap();
        let out = substitute(value, &table_of(&["FAST=5"])).unwrap();
        assert_eq!(out.get("period"), Some(&Value::from(5)));
    }

    #[test]
    fn round_trips_into_a_strategy() {
        // After substitution, the surviving `!sma`/`!crosses_above` tags (now
        // singleton objects) must still resolve to their enum variants.
        let yaml = r#"
            root: !param { key: SYM, default: BTC }
            long:
              enter: !crosses_above
                lhs: !sma { source: close, period: !param { key: FAST } }
                rhs: !sma { source: close, period: !param { key: SLOW, default: 8 } }
        "#;
        let value = yaml_to_json(serde_norway::from_str(yaml).unwrap()).unwrap();
        let value = substitute(value, &table_of(&["FAST=3"])).unwrap();
        let spec: SingleStrategySpec = serde_json::from_value(value).unwrap();
        assert_eq!(sole(&spec), "BTC");
        assert!(spec.long.is_some());
        let _strat = spec.build(1_000.0, &crate::Schema::empty());
    }

    /// End-to-end `check` path: substitute-for-check over a YAML doc, then
    /// parse the hole-marked tree through the hole-aware deserializer, exactly
    /// as `fugazi check` does.
    fn check(yaml: &str, pairs: &[&str]) -> Result<(SingleStrategySpec, usize)> {
        let value = yaml_to_json(serde_norway::from_str(yaml).unwrap()).unwrap();
        let (value, holes) = substitute_for_check(value, &table_of(pairs))?;
        let _guard = crate::spec::undefined::check_mode();
        let spec = crate::spec::undefined::from_json_value(value).map_err(anyhow::Error::new)?;
        Ok((spec, holes))
    }

    #[test]
    fn check_validates_around_an_unset_numeric_param() {
        // `period` has no default and no value — `check` fills it with a
        // number (the type serde asks for), so the spec still parses.
        let yaml = r#"
            root: BTC
            long:
              enter: !crosses_above
                lhs: !sma { source: close, period: !param { key: FAST } }
                rhs: !sma { source: close, period: 20 }
        "#;
        let (spec, holes) = check(yaml, &[]).unwrap();
        assert_eq!(sole(&spec), "BTC");
        assert_eq!(holes, 1);
    }

    #[test]
    fn check_validates_around_an_unset_string_param() {
        // `root` bottoms out in a `String` symbol — the hole must satisfy it as
        // not a number. This is the case a single fixed placeholder can't cover.
        let (spec, holes) = check("root: !param SYM\nlong: { enter: !value true }", &[]).unwrap();
        assert_eq!(holes, 1);
        // An unset root names nothing, and says so rather than guessing. That
        // is the whole point of `check` reporting holes: the document is valid
        // *around* the placeholder, and the instrument is simply not known yet.
        assert!(
            spec.root
                .sole_symbol("single-asset")
                .unwrap_err()
                .contains("names no symbol")
        );
    }

    /// A placeholder standing in for a whole *expression* is the one position
    /// serde never asks a type of — the node parse is hand-rolled. It used to
    /// become a `!value 0.0` constant, so a Bool slot rejected it on type and a
    /// Real slot accepted it while recording nothing.
    #[test]
    fn check_validates_around_a_param_standing_for_an_expression() {
        let _ = crate::spec::undefined::take_observations();
        // `enter:` demands a Bool; the hole must not claim to be a Real.
        let (_spec, holes) = check("root: BTC\nlong: { enter: !param SIGNAL }", &[]).unwrap();
        assert_eq!(holes, 1);
        // And the report has to know the placeholder exists at all.
        let seen = crate::spec::undefined::take_observations();
        assert_eq!(
            seen,
            vec![crate::spec::undefined::HoleTypes {
                origin: crate::spec::undefined::UndefinedOrigin::Param,
                name: "SIGNAL".to_string(),
                declared: None,
                used: vec![crate::spec::undefined::RequiredType::Expr],
            }]
        );
    }

    /// The same for a `!value` literal slot (`!match`'s `when:`), the other
    /// hand-rolled parse — which reported the internal sentinel key at the user.
    #[test]
    fn check_validates_around_a_param_in_a_literal_slot() {
        let _ = crate::spec::undefined::take_observations();
        let yaml = "\
root: BTC
long:
  enter: !gt
    lhs: !match
      on: !close
      cases:
        - when: !param LEVEL
          value: !value 1
      default: !value 0
    rhs: !value 0
";
        let (_spec, holes) = check(yaml, &[]).unwrap();
        assert_eq!(holes, 1);
        let seen = crate::spec::undefined::take_observations();
        assert_eq!(seen.len(), 1, "{seen:?}");
        assert_eq!(seen[0].name, "LEVEL");
    }

    #[test]
    fn check_still_prefers_a_supplied_value_or_default() {
        let (spec, holes) = check(
            "root: !param { key: SYM, default: BTC }\nlong: { enter: !value true }",
            &[],
        )
        .unwrap();
        assert_eq!(sole(&spec), "BTC");
        assert_eq!(holes, 0);

        let (spec, holes) = check(
            "root: !param SYM\nlong: { enter: !value true }",
            &["SYM=ETH"],
        )
        .unwrap();
        assert_eq!(sole(&spec), "ETH");
        assert_eq!(holes, 0);
    }

    #[test]
    fn check_still_rejects_a_malformed_placeholder() {
        // No string `key` is a genuine format error, not a hole to fill.
        let err = check(
            "root: !param { default: BTC }\nlong: { enter: !value true }",
            &[],
        )
        .unwrap_err();
        assert!(err.to_string().contains("needs a string"), "{err}");
    }

    // --- explicit `type:` declarations --------------------------------

    #[test]
    fn a_declared_string_keeps_a_numeric_ticker_a_string() {
        // The heuristic reads `SYM=123` as a number, and `symbol:` is a
        // `String` — so without the declaration this document does not parse.
        let out = sub("root: !param { key: SYM, type: string }", &["SYM=123"]).unwrap();
        assert_eq!(out.get("root"), Some(&Value::from("123")));
    }

    #[test]
    fn a_declared_integer_refuses_a_fraction_naming_the_parameter() {
        let err = sub("period: !param { key: FAST, type: integer }", &["FAST=3.5"]).unwrap_err();
        let err = err.to_string();
        assert!(err.contains("parameter `FAST`"), "{err}");
        assert!(err.contains("not a whole number"), "{err}");
    }

    #[test]
    fn a_declaration_checks_the_default_too() {
        // Nothing else would: the run that never passes `FAST` is exactly the
        // one where the fallback is used.
        let err = sub(
            "period: !param { key: FAST, type: integer, default: 3.5 }",
            &[],
        )
        .unwrap_err();
        assert!(err.to_string().contains("not a whole number"), "{err}");
    }

    #[test]
    fn an_absent_or_null_type_leaves_the_heuristics_alone() {
        // The whole compatibility claim, pinned: two spellings of "no
        // declaration", and a value that a declaration would have changed.
        for doc in [
            "root: !param { key: SYM }",
            "root: !param { key: SYM, type: null }",
        ] {
            let out = sub(doc, &["SYM=123"]).unwrap();
            assert_eq!(out.get("root"), Some(&Value::from(123)), "{doc}");
        }
    }

    #[test]
    fn an_unknown_body_key_is_an_error_not_a_shrug() {
        // Without this, `typ:` means "untyped" — the exact opposite of what was
        // written, and silently.
        let err = sub("period: !param { key: FAST, typ: integer }", &["FAST=3"]).unwrap_err();
        let err = err.to_string();
        assert!(err.contains("unknown key `typ`"), "{err}");
        assert!(err.contains("`key`, `default` and `type`"), "{err}");
    }

    #[test]
    fn an_unknown_type_name_names_the_placeholder() {
        let err = sub("period: !param { key: FAST, type: int }", &["FAST=3"]).unwrap_err();
        let err = err.to_string();
        assert!(err.contains("`param` `FAST`"), "{err}");
        assert!(err.contains("`integer`"), "{err}");
    }

    #[test]
    fn check_reports_the_declared_type_of_an_unset_placeholder() {
        let _ = crate::spec::undefined::take_observations();
        // `period:` demands a number either way; the *declaration* is what says
        // it has to be a whole one, and that is what the report should print.
        let (_spec, holes) = check(
            "root: BTC
long:
  enter: !gt
    lhs: !sma { period: !param { key: FAST, type: integer } }
    rhs: !value 0
",
            &[],
        )
        .unwrap();
        assert_eq!(holes, 1);
        let seen = crate::spec::undefined::take_observations();
        assert_eq!(seen.len(), 1, "{seen:?}");
        assert_eq!(seen[0].name, "FAST");
        assert_eq!(seen[0].declared, Some(crate::spec::ParamType::Integer));
        assert_eq!(
            seen[0].used,
            vec![crate::spec::undefined::RequiredType::Number]
        );
    }

    #[test]
    fn check_records_no_declaration_for_a_placeholder_it_resolved() {
        // A resolved placeholder has a real value and its declaration has
        // already done its work by coercing it — there is nothing left to
        // report, and reporting it would count a hole that isn't one.
        let _ = crate::spec::undefined::take_observations();
        let (_spec, holes) = check(
            "root: !param { key: SYM, type: string }
long: { enter: !value true }",
            &["SYM=123"],
        )
        .unwrap();
        assert_eq!(holes, 0);
        assert!(crate::spec::undefined::take_observations().is_empty());
    }

    #[test]
    fn substitute_partial_applies_the_declaration_it_resolves() {
        // `!import`'s inline `params:` path — the coercion has to happen there
        // too, or an imported fragment types differently than a top-level one.
        let input = json!({"root": {"param": {"key": "SYM", "type": "string"}}});
        let table = HashMap::from([("SYM".to_string(), json!(123))]);
        let out = substitute_partial(input, &table).unwrap();
        assert_eq!(out["root"], json!("123"));
    }

    #[test]
    fn substitute_partial_leaves_an_unresolved_declared_placeholder_intact() {
        // Including its `type:` — the outer pass is what resolves it, and it
        // needs the declaration to still be there.
        let input = json!({"root": {"param": {"key": "SYM", "type": "string"}}});
        let out = substitute_partial(input.clone(), &HashMap::new()).unwrap();
        assert_eq!(out, input);
    }

    #[test]
    fn undefined_is_rejected_outside_check_mode() {
        // A spec still carrying `!undefined` cannot run, and the message has to
        // say so rather than surfacing as an "invalid type" from the typed parse.
        let input = json!({"period": {"undefined": null}});
        let err = substitute(input, &HashMap::new()).expect_err("run must refuse");
        assert!(
            format!("{err:#}").contains("check-time placeholder"),
            "{err:#}"
        );
    }

    #[test]
    fn undefined_becomes_a_hole_named_by_its_document_path() {
        // The path is what locates it — there is no name to report, and a
        // positional counter would make the reader count occurrences.
        let input = json!({"long": {"enter": {"sma": {"period": {"undefined": null}}}}});
        let (out, holes) = substitute_for_check(input, &HashMap::new()).unwrap();
        assert_eq!(holes, 1);
        let hole = &out["long"]["enter"]["sma"]["period"];
        assert_eq!(
            hole[crate::spec::undefined::UNDEFINED_KEY],
            json!("long.enter.sma.period")
        );
    }

    #[test]
    fn two_undefined_holes_get_distinct_paths() {
        // Distinct keys are what stop them collapsing into one report entry —
        // and stop two positions of different types looking like one
        // contradictory placeholder.
        let input = json!({
            "a": {"undefined": null},
            "b": {"c": {"undefined": null}},
        });
        let (out, holes) = substitute_for_check(input, &HashMap::new()).unwrap();
        assert_eq!(holes, 2);
        let key = crate::spec::undefined::UNDEFINED_KEY;
        assert_eq!(out["a"][key], json!("a"));
        assert_eq!(out["b"]["c"][key], json!("b.c"));
    }

    #[test]
    fn undefined_inside_a_sequence_records_its_index() {
        let input = json!({"cases": [{"when": {"undefined": null}}]});
        let (out, _) = substitute_for_check(input, &HashMap::new()).unwrap();
        assert_eq!(
            out["cases"][0]["when"][crate::spec::undefined::UNDEFINED_KEY],
            json!("cases.[0].when")
        );
    }
}
