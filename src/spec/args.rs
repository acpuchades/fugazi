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
//! !pick { symbol: !arg { key: SYM, type: string } } # declared type, checked on bind
//! ```
//!
//! The optional `type:` ([`ParamType`](crate::spec::ParamType)) is read by the
//! same `Placeholder` parse `!param` uses, so the two bodies cannot drift. It bites at *build* time here rather than
//! load time — that is when a driver binds the name — and a `CHILD_INDEX`
//! declared `string` is a portfolio weight template that says what it means
//! instead of indexing by a stringified integer.

use std::collections::HashMap;

use anyhow::{Result, bail};
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

/// Rewrite every `arg` placeholder to an [undefined](crate::spec::undefined) sentinel,
/// so a template body can be typed-parsed without knowing what the driver will
/// eventually supply.
///
/// The `!arg`-side twin of
/// [`params::substitute_for_check`](crate::spec::params::substitute_for_check),
/// and used for the same reason: `fugazi check` validates a document's
/// **shape**, and a template's shape does not depend on which symbol (or child
/// index, or group) the driver will bind. Marking every `!arg` undefined lets
/// the typed parse run anyway, which is what catches an unknown tag or a
/// misspelled field inside a `score:` / `sizing:` / per-side template — errors
/// that otherwise surface only once a run reaches that symbol.
///
/// Unlike the `!param` twin this never fails and takes no table: at check time
/// *every* `!arg` is unknown by construction, so there is nothing to resolve
/// against and no "unset required placeholder" case to report.
pub fn substitute_for_check(value: Value) -> Value {
    match value {
        Value::Object(map) if map.len() == 1 && map.contains_key("arg") => {
            // Keep the key for diagnostics when it is well-formed; a malformed
            // placeholder still becomes undefined here, and `build` is where its
            // shape is enforced (check does not resolve args at all).
            let key = match &map["arg"] {
                Value::String(name) => name.clone(),
                Value::Object(o) => o
                    .get("key")
                    .and_then(Value::as_str)
                    .unwrap_or("arg")
                    .to_string(),
                _ => "arg".to_string(),
            };
            crate::spec::undefined::arg_sentinel(&key)
        }
        Value::Object(map) => Value::Object(
            map.into_iter()
                .map(|(k, v)| (k, substitute_for_check(v)))
                .collect(),
        ),
        Value::Array(seq) => Value::Array(seq.into_iter().map(substitute_for_check).collect()),
        other => other,
    }
}

/// Resolve a single placeholder body — its `{ key, default, type }` object or
/// bare key name — against the supplied `args`.
fn resolve(body: &Value, args: &HashMap<String, Value>) -> Result<Value> {
    let p = crate::spec::params::placeholder_of("arg", body)?;
    if let Some(value) = args.get(p.key) {
        p.apply(value.clone())
    } else if let Some(default) = p.default {
        // Declared types check the `default:` too — an author who contradicts
        // their own declaration should hear it on the build that uses the
        // fallback, not on the one that doesn't.
        p.apply(default.clone())
    } else {
        bail!(
            "argument `{key}` was not supplied by the driver (add a `default:` to make it optional)",
            key = p.key
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
    fn a_declared_type_coerces_what_the_driver_bound() {
        // A portfolio weight template binds `CHILD_INDEX` as a number; a slot
        // that wants the name of the child wants it as text.
        let input = json!({"arg": {"key": "CHILD_INDEX", "type": "string"}});
        let out = substitute(input, &args(&[("CHILD_INDEX", json!(2))])).unwrap();
        assert_eq!(out, json!("2"));
    }

    #[test]
    fn a_declared_type_refuses_a_binding_it_cannot_be_calling_it_an_argument() {
        // The noun matters: a driver supplies this, so telling the user to pass
        // a different `--params` value would send them to the wrong knob.
        let input = json!({"arg": {"key": "N", "type": "integer"}});
        let err = substitute(input, &args(&[("N", json!(2.5))]))
            .unwrap_err()
            .to_string();
        assert!(err.contains("argument `N`"), "{err}");
        assert!(err.contains("not a whole number"), "{err}");
    }

    #[test]
    fn a_declaration_checks_the_default_too() {
        let input = json!({"arg": {"key": "N", "type": "integer", "default": 2.5}});
        let err = substitute(input, &HashMap::new()).unwrap_err().to_string();
        assert!(err.contains("not a whole number"), "{err}");
    }

    #[test]
    fn an_absent_type_leaves_the_binding_exactly_as_the_driver_gave_it() {
        let input = json!({"arg": {"key": "CHILD_INDEX"}});
        let out = substitute(input, &args(&[("CHILD_INDEX", json!(2))])).unwrap();
        assert_eq!(out, json!(2));
    }

    #[test]
    fn the_body_key_set_is_the_one_param_uses() {
        // `!arg` reads its body through `params::placeholder_of`, so the typo
        // guard and the `type:` vocabulary can't drift between the two tags.
        let err = substitute(
            json!({"arg": {"key": "SYM", "typ": "string"}}),
            &HashMap::new(),
        )
        .unwrap_err()
        .to_string();
        assert!(
            err.contains("`arg` `SYM` has an unknown key `typ`"),
            "{err}"
        );
        let err = substitute(
            json!({"arg": {"key": "SYM", "type": "str"}}),
            &HashMap::new(),
        )
        .unwrap_err()
        .to_string();
        assert!(
            err.contains("`arg` `SYM` has an unknown `type: str`"),
            "{err}"
        );
    }

    #[test]
    fn substitutes_a_number_or_bool_literal_from_args() {
        // `args` values are arbitrary JSON, so the placeholder can resolve
        // to a number, bool, string, or even a nested object.
        let input = json!({"period": {"arg": "N"}, "trend": {"arg": "T"}});
        let out = substitute(input, &args(&[("N", json!(20)), ("T", json!(true))])).unwrap();
        assert_eq!(out, json!({"period": 20, "trend": true}));
    }
}
