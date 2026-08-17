//! [`SpecTemplate<T>`]: a raw YAML/JSON tree deferred for later
//! typed-deserialize into `T`.
//!
//! First-class YAML type alongside [`NodeSpec`](super::NodeSpec) and
//! `NodeSpec`. Where those two produce a concrete
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
//! # Deferred value, eager shape
//!
//! Only the *value* is deferred. A template's shape — which tags, which
//! fields, which types — does not depend on the symbol (or child index, or
//! group) the driver eventually binds, so the [`Deserialize`] impl below
//! typed-parses a probe copy of the body at load, with every `!arg` held as a
//! hole. A misspelled tag inside a basket's `score:` is therefore a parse error
//! at load, exactly like one inside a single-asset `enter:` — not a surprise on
//! the first bar that quotes the symbol which instantiates it.
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
//!     lhs: !roc { source: !close { source: !pick { symbol: !arg SYM } }, period: 20 }
//!     rhs: !adx { source: !current_bar { source: !pick { symbol: !arg SYM } }, period: 14 }
//! ```
//!
//! deserializes as a `SpecTemplate<NodeSpec>` because that's the type of
//! the `score:` field on its containing spec (e.g.
//! `BasketStrategySpec`). The same YAML tree under a field typed as
//! `NodeSpec` would deserialize eagerly and fail on the `!arg` leaves.

use std::collections::HashMap;
use std::marker::PhantomData;

use anyhow::Result;
use serde::de::DeserializeOwned;
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
    /// Wrap a raw JSON tree as a template **without validating its shape**.
    ///
    /// Any load-time `!param` substitutions should already be applied to `tree`
    /// (the standard path is via the [`Deserialize`] impl below, after a caller
    /// runs [`params::substitute`](crate::spec::params::substitute) on the whole
    /// document first).
    ///
    /// Prefer [`checked`](Self::checked) when the tree comes from a document:
    /// this one defers every shape error to the eventual
    /// [`build`](Self::build), which for the per-symbol slots is inside the
    /// driver. It is the right call only when the tree is derived from an
    /// already-validated template (the per-child `!value <list>` rewrite in
    /// `PortfolioSpec::build` is the one such caller).
    pub fn from_tree(tree: Value) -> Self {
        Self {
            tree,
            _t: PhantomData,
        }
    }

    /// Access the raw tree — useful for diagnostics or non-`T` consumers
    /// (e.g. a config dump), and by unit tests that want to inspect
    /// substitution state.
    pub fn tree(&self) -> &Value {
        &self.tree
    }
}

impl<T: DeserializeOwned> SpecTemplate<T> {
    /// Wrap a raw JSON tree as a template, **typed-parsing a probe copy first**
    /// so a shape error in the deferred body is reported here.
    ///
    /// What the [`Deserialize`] impl below does, exposed for the callers that
    /// preprocess a tree before wrapping it (the `weights:` sugar rewrite) and
    /// for API consumers assembling a template programmatically. See that impl
    /// for what the probe can and can't decide.
    pub fn checked(tree: Value) -> std::result::Result<Self, String> {
        let probe = args::substitute_for_check(tree.clone());
        crate::spec::undefined::parse_probe::<T>(probe)?;
        Ok(Self::from_tree(tree))
    }

    /// Resolve `!arg` placeholders against `args` and deserialize into
    /// `T`. Errors if an `!arg` references a name that isn't in `args`
    /// and has no `default`, or if the resulting tree doesn't
    /// deserialize into `T`.
    pub fn build(&self, args: &HashMap<String, Value>) -> Result<T> {
        let resolved = args::substitute(self.tree.clone(), args)?;
        Ok(serde_json::from_value(resolved)?)
    }
}

/// Deserialization stores the raw tree — the `!arg` placeholders inside it have
/// to survive the load pass, so what [`build`](SpecTemplate::build) resolves
/// later is the original — but it **typed-parses a probe copy first**, so a
/// template body is validated as eagerly as an ordinary slot.
///
/// Deferring the typed parse entirely would mean a template body is never
/// validated by *anything* until a run reaches the symbol that instantiates it:
/// an unknown tag or a misspelled field inside a basket's `score:`, a
/// multi-asset side's `enter:`, or a portfolio's `weights:` would load clean,
/// pass `fugazi check`, and then abort a run mid-flight. A template's *shape*
/// doesn't depend on which symbol (or child index, or group) the driver
/// eventually binds, so it can be decided here: this parses a copy of the tree
/// with every `!arg` marked as a hole sentinel
/// ([`args::substitute_for_check`]) through
/// [`undefined::parse_probe`](crate::spec::undefined::parse_probe), which
/// answers each hole with a value of whatever type its position demands.
///
/// The effect is that a typo in a deferred body is a parse error like every
/// other, for every consumer of the loader — `run`, `optimize`, `check`, the
/// Rust `*Spec::from_text_*` constructors, and Python's `load_spec`.
impl<'de, T: DeserializeOwned> Deserialize<'de> for SpecTemplate<T> {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        let tree = Value::deserialize(deserializer)?;
        Self::checked(tree).map_err(serde::de::Error::custom)
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
        // Captured verbatim: the probe parse runs on a *copy*, so the `!arg`
        // leaf is still there for `build` to resolve.
        assert_eq!(template.tree(), &value);
    }

    #[test]
    fn deserialize_rejects_a_body_that_cannot_typed_parse() {
        // `period` is missing. Deferring the typed parse used to make this a
        // build-time failure — inside the driver, for the per-symbol slots.
        let value = json!({"symbol": {"arg": "SYM"}});
        let err = serde_json::from_value::<SpecTemplate<Toy>>(value)
            .expect_err("a body that can't parse must not load");
        assert!(err.to_string().contains("period"), "{err}");
    }

    #[test]
    fn deserialize_answers_an_arg_hole_with_the_type_its_position_demands() {
        // The probe knows nothing about the values a driver will bind, so each
        // `!arg` answers as whatever its position needs: a string for
        // `symbol:`, a number for `period:`. Neither is a parse failure.
        let value = json!({"symbol": {"arg": "SYM"}, "period": {"arg": "CHILD_INDEX"}});
        serde_json::from_value::<SpecTemplate<Toy>>(value).expect("both holes are answerable");
    }

    #[test]
    fn a_load_time_probe_leaves_no_check_state_behind() {
        // The probe borrows `check`'s hole machinery, which records what type
        // each placeholder was required to have. Outside `check` nobody drains
        // that ledger, so a long-lived API consumer loading specs in a loop
        // would grow it without bound — and the next `check` in the process
        // would report placeholders from a document it never read.
        let value = json!({"symbol": {"arg": "SYM"}, "period": 20});
        serde_json::from_value::<SpecTemplate<Toy>>(value).unwrap();
        assert!(!crate::spec::undefined::in_check_mode());
        assert!(
            crate::spec::undefined::take_observations().is_empty(),
            "a load-time probe must not leave observations for `check` to report",
        );
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
