//! `--params` substitution for a strategy spec.
//!
//! The strategy spec ([`crate::spec`]) deserializes into strongly-typed serde
//! enums, where a `period` is a `usize`, a `k` is a `Real`, and so on — there is
//! no room to drop a `param` placeholder where a number is expected during typed
//! parsing. So substitution happens in a **first pass over the untyped value
//! tree**: the document is normalized to a [`serde_json::Value`] (see
//! [`crate::convert`]), every placeholder node is rewritten to its resolved value
//! here, and only then is the result deserialized into the typed spec.
//!
//! A placeholder is a singleton object keyed `param` — written `!param { … }` in
//! YAML (the tag becomes that object via [`crate::convert`]) or, in flow/map form,
//! `{ param: { … } }`:
//!
//! ```yaml
//! period: !param { key: FAST }                # required — must be passed
//! period: !param { key: SLOW, default: 8 }    # optional — falls back to 8
//! symbol: !param SYM                           # bare-string shorthand for { key: SYM }
//! ```

use std::collections::HashMap;
use std::str::FromStr;

use anyhow::{Context, Result, anyhow, bail};
use serde_json::{Map, Value};

use crate::input::{self, Source};

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
                    let value = input::parse_value(&text)
                        .with_context(|| format!("parsing params {}", src.label()))?;
                    match value {
                        Value::Object(map) => table.extend(map),
                        _ => bail!("params file {} must be a mapping of NAME: value", src.label()),
                    }
                }
            }
        }
    }
    Ok(table)
}

/// Rewrite every `param` placeholder in `value` to its resolved value, recursing
/// through objects and arrays.
pub fn substitute(value: Value, params: &HashMap<String, Value>) -> Result<Value> {
    match value {
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

/// Resolve a single placeholder body (its `{ key, default }` object or bare key
/// name) against the supplied params.
fn resolve(body: &Value, params: &HashMap<String, Value>) -> Result<Value> {
    let (key, default) = match body {
        Value::String(name) => (name.as_str(), None),
        Value::Object(o) => {
            let key = o
                .get("key")
                .and_then(Value::as_str)
                .ok_or_else(|| anyhow!("`param` needs a string `key`"))?;
            (key, o.get("default"))
        }
        _ => bail!("`param` expects a key name or a `{{ key: NAME }}` object"),
    };

    if let Some(value) = params.get(key) {
        Ok(value.clone())
    } else if let Some(default) = default {
        Ok(default.clone())
    } else {
        bail!("parameter `{key}` is not set (pass `--params {key}=…` or add a `default`)")
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::convert::yaml_to_json;
    use crate::spec::StrategySpec;

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
        let out = sub("symbol: !param SYM", &["SYM=ETH"]).unwrap();
        assert_eq!(out.get("symbol"), Some(&Value::from("ETH")));
    }

    #[test]
    fn json_param_placeholder_resolves() {
        // The `{"param": …}` form straight from JSON (no YAML tag involved).
        let value: Value = serde_json::from_str(r#"{"period": {"param": {"key": "FAST"}}}"#).unwrap();
        let out = substitute(value, &table_of(&["FAST=5"])).unwrap();
        assert_eq!(out.get("period"), Some(&Value::from(5)));
    }

    #[test]
    fn round_trips_into_a_strategy() {
        // After substitution, the surviving `!sma`/`!crosses_above` tags (now
        // singleton objects) must still resolve to their enum variants.
        let yaml = r#"
            symbol: !param { key: SYM, default: BTC }
            long:
              enter: !crosses_above
                lhs: !sma { source: close, period: !param { key: FAST } }
                rhs: !sma { source: close, period: !param { key: SLOW, default: 8 } }
        "#;
        let value = yaml_to_json(serde_norway::from_str(yaml).unwrap()).unwrap();
        let value = substitute(value, &table_of(&["FAST=3"])).unwrap();
        let spec: StrategySpec = serde_json::from_value(value).unwrap();
        assert_eq!(spec.symbol, "BTC");
        assert!(spec.long.is_some());
        let _strat = spec.build();
    }
}
