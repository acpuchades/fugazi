//! Declarative, serde-deserializable mirror of the fugazi composition API.
//!
//! These spec types are the YAML *surface*: each variant maps to one fugazi
//! constructor, and `build()` turns a spec tree into the corresponding live
//! (type-erased) indicator, signal or strategy. Keeping the serde boilerplate
//! here — on dedicated wrapper enums — means the core crate's data model stays
//! free of serde and of any runtime-dispatch concession.
//!
//! Three layers, mirroring the crate; one per submodule:
//!
//! * [`SourceSpec`] (see [`source`]) → [`crate::dyn_indicator::DynValue`] — a
//!   real-valued source (`Output = Real`).
//! * [`SignalSpec`] (see [`signal`]) → boolean condition (a `Signal`).
//! * [`StrategySpec`] (see [`strategy`]) → [`fugazi::strategies::SingleAssetStrategy`] —
//!   the decision layer.
//!
//! The enums are *externally tagged* (serde's default), so an indicator reads as
//! a single-key map — `{ema: {source: close, period: 20}}` — and a parameterless
//! leaf or bar indicator reads as a bare string — `close`, `obv`.

mod signal;
mod source;
mod strategy;

#[allow(unused_imports)]
pub use signal::SignalSpec;
pub use source::SourceSpec;
pub use strategy::StrategySpec;
#[allow(unused_imports)]
pub(crate) use strategy::{DynSingleStrategy, SideSpec};

#[cfg(test)]
mod tests {
    use super::*;
    use crate::dyn_indicator::{DynIndicator, DynValue as Payload};
    use fugazi::indicators::{Current, Ema, Position};
    use fugazi::prelude::*;

    fn bar(close: Real) -> Candle {
        Candle::new(close, close, close, close, 0.0)
    }

    /// Feed a `Box<dyn DynIndicator>` a candle and unwrap the payload as `Real`.
    fn feed_real(source: &mut Box<dyn DynIndicator>, c: Candle) -> Option<Real> {
        match source.update(Payload::Atom(c.into()))? {
            Payload::Real(x) => Some(x),
            other => panic!("expected Real payload, got {other:?}"),
        }
    }

    /// Feed and unwrap as `bool` — for signal-side tests.
    fn feed_bool(source: &mut Box<dyn DynIndicator>, c: Candle) -> Option<bool> {
        match source.update(Payload::Atom(c.into()))? {
            Payload::Bool(b) => Some(b),
            other => panic!("expected Bool payload, got {other:?}"),
        }
    }

    #[test]
    fn builds_an_sma_crossover_signal_that_fires() {
        let yaml = r#"
            !crosses_above
            lhs: !sma { source: close, period: 2 }
            rhs: !sma { source: close, period: 4 }
        "#;
        let spec: SignalSpec = serde_norway::from_str(yaml).unwrap();
        let mut sig = spec.build(&Position::new());
        let mut fired = false;
        for p in [10.0, 9.0, 8.0, 7.0, 8.0, 10.0, 12.0, 14.0, 16.0] {
            fired |= feed_bool(&mut sig, bar(p)).unwrap_or(false);
        }
        assert!(fired, "expected the fast/slow SMA crossover to fire");
    }

    #[test]
    fn probe_yaml_tags_survive_conversion_to_value() {
        let yaml = r#"
            symbol: BTC
            long:
              enter: !crosses_above { lhs: !sma { source: close, period: 3 }, rhs: !sma { period: 8 } }
        "#;
        let value: serde_norway::Value = serde_norway::from_str(yaml).unwrap();
        let json = crate::convert::yaml_to_json(value).unwrap();
        let spec: StrategySpec = serde_json::from_value(json).unwrap();
        assert_eq!(spec.symbol, "BTC");
        assert!(spec.long.is_some());
        let _ = spec.build();
    }

    #[test]
    fn default_source_is_close() {
        let spec: SourceSpec = serde_norway::from_str("!ema { period: 3 }").unwrap();
        let mut ema = spec.build(&Position::new());
        let mut reference = Ema::new(Current::close(), 3);
        for p in [1.0, 2.0, 3.0, 4.0, 5.0] {
            assert_eq!(feed_real(&mut ema, bar(p)), reference.update(bar(p).into()));
        }
    }

    #[test]
    fn parses_full_strategy_with_long_and_short() {
        let yaml = r#"
            symbol: BTC
            long:
              enter: !crosses_above { lhs: !sma { period: 5 }, rhs: !sma { period: 20 } }
              exit:  !crosses_below { lhs: !sma { period: 5 }, rhs: !sma { period: 20 } }
            short:
              enter: !crosses_below { lhs: !sma { period: 5 }, rhs: !sma { period: 20 } }
              exit:  !crosses_above { lhs: !sma { period: 5 }, rhs: !sma { period: 20 } }
        "#;
        let spec = StrategySpec::from_text_with_params(yaml, &std::collections::HashMap::new())
            .unwrap();
        assert_eq!(spec.symbol, "BTC");
        let _strat = spec.build();
    }

    #[test]
    fn stop_loss_with_entry_source_fires_at_the_level() {
        // Enter on the first bar, with a stop at 90% of entry built from `entry`.
        let yaml = r#"
            symbol: BTC
            long:
              enter: !value true
              stop_loss: !mul { lhs: entry, rhs: !value 0.9 }
        "#;
        let spec =
            StrategySpec::from_text_with_params(yaml, &std::collections::HashMap::new()).unwrap();
        let mut strat = spec.build();
        let mut w = PaperWallet::new(1_000.0);
        for c in [
            Candle::new(100.0, 100.0, 100.0, 100.0, 0.0),
            Candle::new(100.0, 100.0, 100.0, 100.0, 0.0),
            Candle::new(95.0, 96.0, 88.0, 89.0, 0.0),
        ] {
            for fill in w.update("BTC".to_string(), c) {
                strat.on_fill(&fill);
            }
            strat.update(c.into());
            strat.trade(&mut w);
        }
        assert!(w.positions().next().is_none());
        assert_eq!(w.orders().last().unwrap().price, 90.0);
    }

    #[test]
    fn unstable_signal_zeroes_unstable_period_but_forwards_output() {
        // `!unstable { signal }` is a passthrough — same output, same warm-up,
        // but `unstable_period()` reports 0 so a strategy's readiness gate
        // stops waiting for the IIR settling tail underneath.
        let yaml = r#"
            !unstable
            signal: !above { source: !ema { period: 3 }, level: 0 }
        "#;
        let spec: SignalSpec = serde_norway::from_str(yaml).unwrap();
        let wrapped = spec.build(&Position::new());
        let inner_raw = Ema::new(Current::close(), 3).above(0.0);
        assert_eq!(wrapped.warm_up_period(), inner_raw.warm_up_period());
        assert_eq!(wrapped.unstable_period(), 0);
        assert_eq!(wrapped.stable_period(), inner_raw.warm_up_period());
        assert!(inner_raw.stable_period() > inner_raw.warm_up_period());
    }

    #[test]
    fn unstable_source_zeroes_unstable_period_but_forwards_output() {
        let yaml = r#"!unstable { source: !ema { period: 5 } }"#;
        let spec: SourceSpec = serde_norway::from_str(yaml).unwrap();
        let wrapped = spec.build(&Position::new());
        let inner_raw = Ema::new(Current::close(), 5);
        assert_eq!(wrapped.warm_up_period(), inner_raw.warm_up_period());
        assert_eq!(wrapped.unstable_period(), 0);
        assert_eq!(wrapped.stable_period(), inner_raw.warm_up_period());
    }

    #[test]
    fn defs_block_parks_yaml_anchors_reused_across_sides() {
        // Anchors defined in an ignored `defs:` block are inlined by the YAML
        // parser at each `*name` site, so a shared signal can be defined once
        // and reused from both sides without repeating the tree.
        let yaml = r#"
            defs:
              - &cross_up !crosses_above { lhs: !sma { period: 3 }, rhs: !sma { period: 8 } }
              - &cross_dn !crosses_below { lhs: !sma { period: 3 }, rhs: !sma { period: 8 } }
            symbol: BTC
            long:  { enter: *cross_up, exit: *cross_dn }
            short: { enter: *cross_dn, exit: *cross_up }
        "#;
        let spec = StrategySpec::from_text_with_params(yaml, &std::collections::HashMap::new())
            .unwrap();
        assert_eq!(spec.symbol, "BTC");
        assert!(spec.long.is_some() && spec.short.is_some());
        let _ = spec.build();
    }

    #[test]
    fn parses_an_inline_flow_map_strategy() {
        let doc = r#"{"symbol":"ETH","long":{"enter":{"crosses_above":
            {"lhs":{"sma":{"period":5}},"rhs":{"sma":{"period":20}}}}}}"#;
        let spec = StrategySpec::from_text_with_params(doc, &std::collections::HashMap::new())
            .unwrap();
        assert_eq!(spec.symbol, "ETH");
        let _strat = spec.build();
    }

    #[test]
    fn resample_tag_projects_the_field() {
        // `!resample { every: N, inner: close }` emits the resampled close on
        // the Nth base tick, None between.
        let spec: SourceSpec =
            serde_norway::from_str("!resample { every: 4, inner: close }").unwrap();
        let mut built = spec.build(&Position::new());
        for i in 1..=8 {
            let out = feed_real(&mut built, bar(i as Real));
            if i % 4 == 0 {
                assert_eq!(out, Some(i as Real));
            } else {
                assert_eq!(out, None);
            }
        }
    }

    #[test]
    fn latch_tag_holds_the_last_value() {
        // `!latch { source: !resample { every: 3, inner: close } }` — Some on
        // the Nth bar, held on the two between.
        let spec: SourceSpec = serde_norway::from_str(
            "!latch { source: !resample { every: 3, inner: close } }",
        )
        .unwrap();
        let mut built = spec.build(&Position::new());
        assert_eq!(feed_real(&mut built, bar(1.0)), None);
        assert_eq!(feed_real(&mut built, bar(2.0)), None);
        assert_eq!(feed_real(&mut built, bar(3.0)), Some(3.0));
        assert_eq!(feed_real(&mut built, bar(4.0)), Some(3.0));
        assert_eq!(feed_real(&mut built, bar(5.0)), Some(3.0));
        assert_eq!(feed_real(&mut built, bar(6.0)), Some(6.0));
    }

    #[test]
    fn bar_indicator_tags_parse_bare_with_default_source() {
        // Every bar-indicator variant carries a defaulted `source` field
        // pointing to `!current`, so a bare `!obv` / `!vwap` / … tag with no
        // map still deserializes and drives the base bar stream.
        let obv: SourceSpec = serde_norway::from_str("!obv").unwrap();
        let mut built = obv.build(&Position::new());
        // OBV seeds at first bar's volume.
        assert_eq!(
            feed_real(&mut built, Candle::new(1.0, 1.0, 1.0, 1.0, 100.0)),
            Some(100.0)
        );

        // And still parses with an explicit source override.
        let obv_htf: SourceSpec =
            serde_norway::from_str("!obv { source: !resample { every: 2, inner: current } }")
                .unwrap();
        let _ = obv_htf.build(&Position::new());
    }

    #[test]
    fn atr_tag_parses_with_default_current_source() {
        // `!atr { period: 3 }` without a source keeps its historical form.
        let spec: SourceSpec = serde_norway::from_str("!atr { period: 3 }").unwrap();
        let _ = spec.build(&Position::new());
    }

    #[test]
    fn keltner_tag_parses_with_default_sources() {
        // Keltner's price source defaults to `close`, its candle source to
        // `current` — so a bare `!keltner_upper { ema_period, atr_period,
        // multiplier }` still parses.
        let spec: SourceSpec = serde_norway::from_str(
            "!keltner_upper { ema_period: 3, atr_period: 3, multiplier: 2.0 }",
        )
        .unwrap();
        let _ = spec.build(&Position::new());
    }

    #[test]
    fn latch_ema_of_resample_matches_reference_htf_ema() {
        // The composition-order regression at the YAML surface: an EMA-3
        // running inside !resample, wrapped in !latch, agrees numerically
        // with Ema(Resample.close, 3) at every boundary.
        let spec: SourceSpec = serde_norway::from_str(
            "!latch { source: !resample { every: 4, inner: !ema { period: 3, source: close } } }",
        )
        .unwrap();
        let mut built = spec.build(&Position::new());
        let mut reference = fugazi::indicators::Latch::new(Ema::new(
            fugazi::indicators::Resample::new(fugazi::indicators::CurrentBar::new(), 4).close(),
            3,
        ));
        for i in 1..=24 {
            let c = bar(100.0 + i as Real * 0.5);
            assert_eq!(feed_real(&mut built, c), reference.update(c.into()));
        }
    }
}
