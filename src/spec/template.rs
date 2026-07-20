//! [`SpecTemplate<T>`]: a raw YAML/JSON tree deferred for later
//! typed-deserialize into `T`.
//!
//! First-class YAML type alongside [`ExprSpec`](super::ExprSpec) and
//! [`SignalSpec`](super::SignalSpec). Where those two produce a concrete
//! indicator eagerly at `spec.build(...)` time, `SpecTemplate<T>` holds a
//! raw `serde_json::Value` tree that may still contain `!arg NAME`
//! placeholder leaves, and produces a concrete `T` only when the caller
//! supplies the missing arguments via [`build(&args)`](SpecTemplate::build).
//!
//! # Substitution model
//!
//! Two-pass, with a clear division of labour:
//!
//! 1. **Load-time** — the user's `!param` substitutions (from `--params`
//!    CLI args) are applied to the whole document once, via
//!    [`crate::spec::params::substitute`]. Those values are baked into the
//!    stored tree; every subsequent `.build()` sees them already-resolved.
//! 2. **Build-time** — a driver (e.g. `BasketStrategySpec`'s per-symbol
//!    factory) supplies `!arg NAME` values via
//!    [`crate::spec::args::substitute`]. This runs on every `.build()` call,
//!    so one template can produce many concrete `T` values (one per
//!    set of driver-supplied args).
//!
//! `!param` and `!arg` never collide because they're keyed on distinct
//! singleton-object keys (`param` vs. `arg`), so a leftover `!arg` after
//! the load-time pass survives untouched.
//!
//! # YAML surface
//!
//! **Untagged** — a `SpecTemplate<T>` field just captures its subtree
//! raw; no `!template` wrapper on the YAML. The template-ness is a schema
//! fact of the containing struct's field type. Concretely:
//!
//! ```yaml
//! score:
//!   !mul
//!     lhs: !roc { source: !close { source: !pick { symbol: !arg SYM } }, periods: 20 }
//!     rhs: !adx { source: !current_bar { source: !pick { symbol: !arg SYM } }, period: 14 }
//! ```
//!
//! deserializes as a `SpecTemplate<ExprSpec>` because that's the type of
//! the `score:` field on its containing spec (e.g.
//! `BasketStrategySpec`). The same YAML tree under a field typed as
//! `ExprSpec` would deserialize eagerly and fail on the `!arg` leaves.

use std::collections::HashMap;
use std::marker::PhantomData;

use anyhow::Result;
use serde::{Deserialize, Deserializer};
use serde_json::Value;

use crate::spec::args;

/// A deferred spec: an untyped `serde_json::Value` tree with `!arg`
/// placeholder leaves, resolved into a concrete `T` at build time. See
/// the module docs for the load-time (`!param`) vs build-time (`!arg`)
/// substitution model.
#[derive(Debug, Clone)]
pub struct SpecTemplate<T> {
    tree: Value,
    // `fn() -> T` so the phantom is `Send`/`Sync` regardless of `T`, and
    // doesn't induce a drop check on `T`.
    _t: PhantomData<fn() -> T>,
}

impl<T> SpecTemplate<T> {
    /// Wrap a raw JSON tree as a template. Any load-time `!param`
    /// substitutions should already be applied to `tree` (the standard
    /// path is via the [`Deserialize`] impl below, after a caller runs
    /// [`params::substitute`](crate::spec::params::substitute) on the whole
    /// document first).
    pub fn from_tree(tree: Value) -> Self {
        Self {
            tree,
            _t: PhantomData,
        }
    }

    /// Access the raw tree — useful for diagnostics or non-`T` consumers
    /// (e.g. a config dump), and by unit tests that want to inspect
    /// substitution state.
    #[allow(dead_code)]
    pub fn tree(&self) -> &Value {
        &self.tree
    }
}

impl<T: for<'de> Deserialize<'de>> SpecTemplate<T> {
    /// Resolve `!arg` placeholders against `args` and deserialize into
    /// `T`. Errors if an `!arg` references a name that isn't in `args`
    /// and has no `default`, or if the resulting tree doesn't
    /// deserialize into `T`.
    pub fn build(&self, args: &HashMap<String, Value>) -> Result<T> {
        let resolved = args::substitute(self.tree.clone(), args)?;
        Ok(serde_json::from_value(resolved)?)
    }
}

/// Deserialization captures the raw tree without trying to typed-parse it
/// into `T` — that's deferred to [`build`](SpecTemplate::build), so any
/// `!arg` placeholders inside the tree survive the load pass.
impl<'de, T> Deserialize<'de> for SpecTemplate<T> {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        let tree = Value::deserialize(deserializer)?;
        Ok(Self::from_tree(tree))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde::Deserialize;
    use serde_json::json;

    #[derive(Debug, Deserialize, PartialEq)]
    struct Toy {
        symbol: String,
        period: usize,
    }

    fn args(pairs: &[(&str, Value)]) -> HashMap<String, Value> {
        pairs
            .iter()
            .map(|(k, v)| ((*k).to_string(), v.clone()))
            .collect()
    }

    #[test]
    fn deserialize_captures_raw_tree() {
        let value = json!({"symbol": {"arg": "SYM"}, "period": 20});
        let template: SpecTemplate<Toy> = serde_json::from_value(value.clone()).unwrap();
        assert_eq!(template.tree(), &value);
    }

    #[test]
    fn build_resolves_args_and_typed_parses() {
        let value = json!({"symbol": {"arg": "SYM"}, "period": 20});
        let template: SpecTemplate<Toy> = serde_json::from_value(value).unwrap();
        let concrete = template.build(&args(&[("SYM", json!("BTC"))])).unwrap();
        assert_eq!(
            concrete,
            Toy {
                symbol: "BTC".to_string(),
                period: 20,
            }
        );
    }

    #[test]
    fn build_errors_on_missing_arg() {
        let value = json!({"symbol": {"arg": "SYM"}, "period": 20});
        let template: SpecTemplate<Toy> = serde_json::from_value(value).unwrap();
        assert!(template.build(&HashMap::new()).is_err());
    }

    #[test]
    fn build_uses_arg_default_when_missing() {
        let value = json!({"symbol": {"arg": {"key": "SYM", "default": "BTC"}}, "period": 20});
        let template: SpecTemplate<Toy> = serde_json::from_value(value).unwrap();
        let concrete = template.build(&HashMap::new()).unwrap();
        assert_eq!(concrete.symbol, "BTC");
    }

    #[test]
    fn build_errors_on_typed_deserialize_failure_after_substitution() {
        // `period` is a number in `Toy`; if we substitute a string via
        // `!arg`, the typed parse should fail.
        let value = json!({"symbol": "BTC", "period": {"arg": "P"}});
        let template: SpecTemplate<Toy> = serde_json::from_value(value).unwrap();
        assert!(
            template
                .build(&args(&[("P", json!("not a number"))]))
                .is_err()
        );
    }

    #[test]
    fn one_template_produces_multiple_concrete_specs() {
        let value = json!({"symbol": {"arg": "SYM"}, "period": 10});
        let template: SpecTemplate<Toy> = serde_json::from_value(value).unwrap();
        let btc = template.build(&args(&[("SYM", json!("BTC"))])).unwrap();
        let eth = template.build(&args(&[("SYM", json!("ETH"))])).unwrap();
        assert_eq!(btc.symbol, "BTC");
        assert_eq!(eth.symbol, "ETH");
    }

    #[test]
    fn template_from_yaml_via_serde_norway() {
        // End-to-end: parse from YAML text (through the normal CLI pipeline
        // via serde_norway), build once with args resolved.
        let yaml = r#"
            symbol: !arg SYM
            period: 30
        "#;
        let value = crate::spec::input::parse_value(yaml).unwrap();
        let template: SpecTemplate<Toy> = serde_json::from_value(value).unwrap();
        let concrete = template.build(&args(&[("SYM", json!("SOL"))])).unwrap();
        assert_eq!(concrete.symbol, "SOL");
        assert_eq!(concrete.period, 30);
    }
}
