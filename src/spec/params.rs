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
//! ```

use std::collections::HashMap;
use std::str::FromStr;

use anyhow::{Context, Result, anyhow, bail};
use serde_json::{Map, Value};

use crate::spec::input::{self, Source};

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
            let key: Option<String> = match &map["param"] {
                Value::String(name) => Some(name.clone()),
                Value::Object(o) => o.get("key").and_then(Value::as_str).map(str::to_string),
                _ => None,
            };
            if let Some(key) = key
                && let Some(value) = params.get(&key)
            {
                return Ok(value.clone());
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

/// Resolve a single placeholder body (its `{ key, default }` object or bare key
/// name) against the supplied params.
fn resolve(body: &Value, params: &HashMap<String, Value>) -> Result<Value> {
    let (key, default) = placeholder_parts(body)?;
    if let Some(value) = params.get(key) {
        Ok(value.clone())
    } else if let Some(default) = default {
        Ok(default.clone())
    } else {
        bail!("parameter `{key}` is not set (pass `--params {key}=…` or add a `default`)")
    }
}

/// Extract a placeholder body's `(key, default)` — the shared parse of the
/// `{ key, default }` object and bare-string forms. Errors on a malformed body
/// (no string `key`); an unset-but-well-formed key is not an error here.
fn placeholder_parts(body: &Value) -> Result<(&str, Option<&Value>)> {
    match body {
        Value::String(name) => Ok((name.as_str(), None)),
        Value::Object(o) => {
            let key = o
                .get("key")
                .and_then(Value::as_str)
                .ok_or_else(|| anyhow!("`param` needs a string `key`"))?;
            Ok((key, o.get("default")))
        }
        _ => bail!("`param` expects a key name or a `{{ key: NAME }}` object"),
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
            let (key, default) = placeholder_parts(&map["param"])?;
            if params.contains_key(key) || default.is_some() {
                resolve(&map["param"], params)
            } else {
                *holes += 1;
                Ok(crate::spec::undefined::sentinel(key))
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
            vec![(
                crate::spec::undefined::UndefinedOrigin::Param,
                "SIGNAL".to_string(),
                vec![crate::spec::undefined::RequiredType::Expr],
            )]
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
        assert_eq!(seen[0].1, "LEVEL");
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
