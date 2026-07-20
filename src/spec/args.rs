//! Build-time `!arg` substitution for a [`SpecTemplate`](crate::spec::SpecTemplate).
//!
//! Twin of [`crate::spec::params`]. Where `!param` is resolved at **load time**
//! from the user's `--params` CLI args (same values for every build of the
//! spec), `!arg` is resolved at **build time** by whatever driver
//! constructs the concrete spec — for a
//! [`BasketStrategySpec`](crate::spec::BasketStrategySpec) that's
//! per-symbol (the driver declares `SYM` when it discovers a new symbol
//! in the snapshot and hands the fresh tree to the score/sizing factory,
//! so a `!pick { symbol: !arg SYM }` inside a deferred score template
//! becomes `!pick { symbol: BTC }` on the fresh chain built for BTC).
//!
//! The `!arg` grammar mirrors `!param`:
//!
//! ```yaml
//! !pick { symbol: !arg SYM }                        # bare-string shorthand
//! !pick { symbol: !arg { key: SYM } }               # required — driver must supply
//! !pick { symbol: !arg { key: SYM, default: BTC } } # optional with fallback
//! ```

use std::collections::HashMap;

use anyhow::{Result, anyhow, bail};
use serde_json::{Map, Value};

/// Rewrite every `arg` placeholder in `value` to its resolved literal from
/// `args`, recursing through objects and arrays. Non-placeholder scalars
/// pass through untouched. `param` placeholders (which are `!param`'s
/// singleton form) are treated as ordinary objects and left alone — those
/// are `crate::spec::params::substitute`'s responsibility.
pub fn substitute(value: Value, args: &HashMap<String, Value>) -> Result<Value> {
    match value {
        Value::Object(map) if map.len() == 1 && map.contains_key("arg") => {
            resolve(&map["arg"], args)
        }
        Value::Object(map) => {
            let mut out = Map::new();
            for (k, v) in map {
                out.insert(k, substitute(v, args)?);
            }
            Ok(Value::Object(out))
        }
        Value::Array(seq) => seq
            .into_iter()
            .map(|v| substitute(v, args))
            .collect::<Result<Vec<_>>>()
            .map(Value::Array),
        other => Ok(other),
    }
}

/// Resolve a single placeholder body — its `{ key, default }` object or
/// bare key name — against the supplied `args`.
fn resolve(body: &Value, args: &HashMap<String, Value>) -> Result<Value> {
    let (key, default) = match body {
        Value::String(name) => (name.as_str(), None),
        Value::Object(o) => {
            let key = o
                .get("key")
                .and_then(Value::as_str)
                .ok_or_else(|| anyhow!("`arg` needs a string `key`"))?;
            (key, o.get("default"))
        }
        _ => bail!("`arg` expects a key name or a `{{ key: NAME }}` object"),
    };

    if let Some(value) = args.get(key) {
        Ok(value.clone())
    } else if let Some(default) = default {
        Ok(default.clone())
    } else {
        bail!(
            "argument `{key}` was not supplied by the driver (add a `default:` to make it optional)"
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn args(pairs: &[(&str, Value)]) -> HashMap<String, Value> {
        pairs
            .iter()
            .map(|(k, v)| ((*k).to_string(), v.clone()))
            .collect()
    }

    #[test]
    fn substitutes_bare_string_form() {
        let input = json!({"pick": {"symbol": {"arg": "SYM"}}});
        let out = substitute(input, &args(&[("SYM", json!("BTC"))])).unwrap();
        assert_eq!(out, json!({"pick": {"symbol": "BTC"}}));
    }

    #[test]
    fn substitutes_object_form_with_key() {
        let input = json!({"arg": {"key": "SYM"}});
        let out = substitute(input, &args(&[("SYM", json!("ETH"))])).unwrap();
        assert_eq!(out, json!("ETH"));
    }

    #[test]
    fn resolves_default_when_arg_missing() {
        let input = json!({"arg": {"key": "MISSING", "default": "fallback"}});
        let out = substitute(input, &HashMap::new()).unwrap();
        assert_eq!(out, json!("fallback"));
    }

    #[test]
    fn errors_when_arg_missing_no_default() {
        let input = json!({"arg": "SYM"});
        let err = substitute(input, &HashMap::new()).unwrap_err();
        assert!(err.to_string().contains("SYM"));
    }

    #[test]
    fn recurses_into_arrays_and_nested_objects() {
        let input = json!({
            "list": [{"arg": "A"}, {"arg": "B"}],
            "nested": {"deep": {"arg": "C"}},
        });
        let out = substitute(
            input,
            &args(&[("A", json!(1)), ("B", json!(2)), ("C", json!(3))]),
        )
        .unwrap();
        assert_eq!(out, json!({"list": [1, 2], "nested": {"deep": 3}}));
    }

    #[test]
    fn leaves_param_placeholders_alone() {
        // A leftover `!param` singleton should pass through — args::substitute
        // is only responsible for `!arg`.
        let input = json!({"param": "FAST"});
        let out = substitute(input, &HashMap::new()).unwrap();
        assert_eq!(out, json!({"param": "FAST"}));
    }

    #[test]
    fn preserves_multi_key_objects_with_arg_key() {
        // An object with `arg` alongside other keys is NOT a placeholder —
        // the singleton-object convention is precise (one key, spelled `arg`).
        let input = json!({"arg": "SYM", "other": 1});
        let out = substitute(input, &args(&[("SYM", json!("BTC"))])).unwrap();
        assert_eq!(out, json!({"arg": "SYM", "other": 1}));
    }

    #[test]
    fn substitutes_a_number_or_bool_literal_from_args() {
        // `args` values are arbitrary JSON, so the placeholder can resolve
        // to a number, bool, string, or even a nested object.
        let input = json!({"period": {"arg": "N"}, "trend": {"arg": "T"}});
        let out = substitute(
            input,
            &args(&[("N", json!(20)), ("T", json!(true))]),
        )
        .unwrap();
        assert_eq!(out, json!({"period": 20, "trend": true}));
    }
}
