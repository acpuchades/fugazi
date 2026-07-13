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
//! --costs @binance.yml,commission.percentage.rate=0.0004
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
//!
//! Models are **externally tagged** — a variant is spelled `!percentage { rate:
//! 0.001 }`, the same YAML tag vocabulary the strategy spec uses. A dotted
//! setter is therefore a literal address into that tree, variant level included:
//! `commission.percentage.rate=0.0004` nudges one field of a loaded preset,
//! while `commission=!percentage { rate: 0.0004 }` replaces the whole model.
//!
//! Split across two submodules:
//!
//! * [`spec`] — `--costs` argument parsing into a [`CostSpec`] term list.
//! * [`config`] — folding into [`CostConfig`] and resolving to a runtime
//!   [`fugazi::costs::TradingCosts`].

mod config;
mod spec;

pub use config::{CostConfig, config};
pub use spec::CostSpec;

#[cfg(test)]
mod tests {
    use super::*;
    use super::config::{CommissionSpec, SpreadSpec};
    use super::spec::CostTerm;
    use crate::calendar::Frequency;
    use crate::input::Source;
    use std::str::FromStr;

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
        // The dotted path names the variant — it addresses the tree literally.
        let cfg = config_of(&[
            "commission=!percentage { rate: 0.001 }",
            "commission.percentage.rate=0.0004",
        ]);
        assert!(matches!(
            cfg.commission.default,
            Some(CommissionSpec::Percentage { rate }) if (rate - 0.0004).abs() < 1e-12
        ));
    }

    #[test]
    fn dotted_key_naming_the_wrong_variant_is_an_error() {
        // Nudging `percentage.rate` on a `!fixed` model would plant a second key
        // at the model position; the externally-tagged enum rejects that rather
        // than half-applying the setter.
        let err = config(&[
            parse("commission=!fixed { amount: 1.0 }"),
            parse("commission.percentage.rate=0.0004"),
        ])
        .unwrap_err();
        let msg = format!("{err:#}");
        assert!(
            msg.contains("commission"),
            "expected a typed error naming the leg, got: {msg}"
        );
    }

    #[test]
    fn dotted_key_builds_a_model_from_scratch() {
        // With no preset loaded the same path is just a longhand spelling of
        // `commission=!percentage { rate: … }`.
        let cfg = config_of(&["commission.percentage.rate=0.001"]);
        assert!(matches!(
            cfg.commission.default,
            Some(CommissionSpec::Percentage { rate }) if (rate - 0.001).abs() < 1e-12
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
            commission: !percentage
              rate: 0.001
            spread: !bps
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
              default: !bps { bps: 2 }
              by_symbol:
                BTC: !bps { bps: 1 }
                ETH: !bps { bps: 1.5 }
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
    fn rejects_unknown_model_variant() {
        // The build path is where the typed deserialize runs; check we hit it.
        let err = config(&[parse("commission=!martian { rate: 0.001 }")]).unwrap_err();
        let msg = format!("{err:#}");
        assert!(
            msg.contains("commission") || msg.contains("martian"),
            "unexpected error: {msg}"
        );
    }

    #[test]
    fn later_model_replaces_earlier_wholesale() {
        // A `!max` layered over a `!percentage` must replace it, not merge into
        // a two-variant object no externally-tagged enum can read.
        let cfg = config_of(&[
            "commission=!percentage { rate: 0.001 }",
            "commission=!max { lhs: !per_unit { rate: 0.0035 }, rhs: !fixed { amount: 1.0 } }",
        ]);
        assert!(matches!(
            cfg.commission.default,
            Some(CommissionSpec::Max { .. })
        ));
    }

    #[test]
    fn leading_scope_distributes_over_later_unscoped_terms() {
        // `--costs 'BTC:commission=…,spread=…'` should scope BOTH terms to BTC.
        let cfg = config_of(&[
            "BTC:commission=!percentage { rate: 0.001 },spread=!bps { bps: 3 }",
        ]);
        // Both live under by_symbol["BTC"], not the default leg.
        assert!(cfg.commission.default.is_none());
        assert!(cfg.spread.default.is_none());
        let b = fugazi::types::Candle::new(100.0, 100.0, 100.0, 100.0, 0.0);
        let btc = cfg.resolve("BTC", None);
        let eth = cfg.resolve("ETH", None);
        assert!((btc.commission.commission(1_000.0, 10.0) - 1.0).abs() < 1e-9);
        assert!((btc.spread.half_spread(100.0, &b) - 0.015).abs() < 1e-9);
        // ETH falls back to no-op on both legs.
        assert_eq!(eth.commission.commission(1_000.0, 10.0), 0.0);
        assert_eq!(eth.spread.half_spread(100.0, &b), 0.0);
    }

    #[test]
    fn per_term_scope_overrides_leading_distributive_scope() {
        // `BTC:X=…,ETH:Y=…` — the per-term ETH scope wins over the outer BTC.
        let cfg = config_of(&[
            "BTC:commission=!percentage { rate: 0.001 },ETH:spread=!bps { bps: 3 }",
        ]);
        let b = fugazi::types::Candle::new(100.0, 100.0, 100.0, 100.0, 0.0);
        // BTC has the commission, no spread.
        let btc = cfg.resolve("BTC", None);
        assert!((btc.commission.commission(1_000.0, 10.0) - 1.0).abs() < 1e-9);
        assert_eq!(btc.spread.half_spread(100.0, &b), 0.0);
        // ETH has the spread, no commission.
        let eth = cfg.resolve("ETH", None);
        assert_eq!(eth.commission.commission(1_000.0, 10.0), 0.0);
        assert!((eth.spread.half_spread(100.0, &b) - 0.015).abs() < 1e-9);
    }

    #[test]
    fn separate_flags_do_not_inherit_scope_across_boundaries() {
        // Splitting into two flags is the escape hatch when a caller wants
        // some terms scoped and others on the default leg.
        let cfg = config_of(&[
            "BTC:commission=!percentage { rate: 0.001 }",
            "spread=!bps { bps: 3 }",
        ]);
        // The spread is on the DEFAULT leg — a fresh flag = a fresh scope context.
        assert!(cfg.spread.default.is_some());
        assert!(cfg.commission.default.is_none());
        let b = fugazi::types::Candle::new(100.0, 100.0, 100.0, 100.0, 0.0);
        let eth = cfg.resolve("ETH", None);
        assert!((eth.spread.half_spread(100.0, &b) - 0.015).abs() < 1e-9);
    }
}
