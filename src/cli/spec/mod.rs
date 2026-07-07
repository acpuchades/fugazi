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
        let mut sig = spec.build(&Position::new(), &Schema::empty());
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
        let _ = spec.build(&Schema::empty());
    }

    #[test]
    fn default_source_is_close() {
        let spec: SourceSpec = serde_norway::from_str("!ema { period: 3 }").unwrap();
        let mut ema = spec.build(&Position::new(), &Schema::empty());
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
        let _strat = spec.build(&Schema::empty());
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
        let mut strat = spec.build(&Schema::empty());
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
        let wrapped = spec.build(&Position::new(), &Schema::empty());
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
        let wrapped = spec.build(&Position::new(), &Schema::empty());
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
        let _ = spec.build(&Schema::empty());
    }

    #[test]
    fn parses_an_inline_flow_map_strategy() {
        let doc = r#"{"symbol":"ETH","long":{"enter":{"crosses_above":
            {"lhs":{"sma":{"period":5}},"rhs":{"sma":{"period":20}}}}}}"#;
        let spec = StrategySpec::from_text_with_params(doc, &std::collections::HashMap::new())
            .unwrap();
        assert_eq!(spec.symbol, "ETH");
        let _strat = spec.build(&Schema::empty());
    }

    #[test]
    fn resample_tag_projects_the_field() {
        // `!resample { every: N, inner: close }` emits the resampled close on
        // the Nth base tick, None between.
        let spec: SourceSpec =
            serde_norway::from_str("!resample { every: 4, inner: close }").unwrap();
        let mut built = spec.build(&Position::new(), &Schema::empty());
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
        let mut built = spec.build(&Position::new(), &Schema::empty());
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
        let mut built = obv.build(&Position::new(), &Schema::empty());
        // OBV seeds at first bar's volume.
        assert_eq!(
            feed_real(&mut built, Candle::new(1.0, 1.0, 1.0, 1.0, 100.0)),
            Some(100.0)
        );

        // And still parses with an explicit source override.
        let obv_htf: SourceSpec =
            serde_norway::from_str("!obv { source: !resample { every: 2, inner: current } }")
                .unwrap();
        let _ = obv_htf.build(&Position::new(), &Schema::empty());
    }

    #[test]
    fn atr_tag_parses_with_default_current_source() {
        // `!atr { period: 3 }` without a source keeps its historical form.
        let spec: SourceSpec = serde_norway::from_str("!atr { period: 3 }").unwrap();
        let _ = spec.build(&Position::new(), &Schema::empty());
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
        let _ = spec.build(&Position::new(), &Schema::empty());
    }

    #[test]
    fn get_real_dispatches_to_get_real_leaf() {
        // `!get { key: vol_20 }` in a Real position reads the numeric column.
        let mut b = Schema::builder();
        b.add_real("vol_20");
        let schema = b.finish();

        let spec: SourceSpec = serde_norway::from_str("!get { key: vol_20 }").unwrap();
        let mut built = spec.build(&Position::new(), &schema);
        assert_eq!(built.output_type(), crate::dyn_indicator::DynType::Real);

        let ov = OverlayInfo::new(schema.clone(), vec![OverlayValue::Real(0.42)]);
        let atom = Atom::with_overlays(bar(100.0), ov);
        assert_eq!(built.update(Payload::Atom(atom)), Some(Payload::Real(0.42)));
    }

    #[test]
    fn get_bool_dispatches_to_get_bool_signal() {
        // `!get { key: risk_on }` in a signal position reads the Bool column
        // directly — no comparison needed.
        let mut b = Schema::builder();
        b.add_bool("risk_on");
        let schema = b.finish();

        let spec: SignalSpec = serde_norway::from_str("!get { key: risk_on }").unwrap();
        let mut built = spec.build(&Position::new(), &schema);
        assert_eq!(built.output_type(), crate::dyn_indicator::DynType::Bool);

        let ov = OverlayInfo::new(schema.clone(), vec![OverlayValue::Bool(true)]);
        let atom = Atom::with_overlays(bar(100.0), ov);
        assert_eq!(built.update(Payload::Atom(atom)), Some(Payload::Bool(true)));
    }

    #[test]
    fn str_eq_against_a_str_get_column() {
        // `!str_eq { lhs: !get { key: regime }, rhs: bull }` fires only when
        // the current regime cell reads exactly "bull".
        let mut b = Schema::builder();
        b.add_str("regime");
        let schema = b.finish();

        let spec: SignalSpec = serde_norway::from_str(
            "!str_eq { lhs: !get { key: regime }, rhs: bull }",
        )
        .unwrap();
        let mut built = spec.build(&Position::new(), &schema);
        assert_eq!(built.output_type(), crate::dyn_indicator::DynType::Bool);

        let bull = OverlayInfo::new(
            schema.clone(),
            vec![OverlayValue::Str(std::sync::Arc::from("bull"))],
        );
        let bear = OverlayInfo::new(
            schema.clone(),
            vec![OverlayValue::Str(std::sync::Arc::from("bear"))],
        );
        assert_eq!(
            built.update(Payload::Atom(Atom::with_overlays(bar(100.0), bull))),
            Some(Payload::Bool(true)),
        );
        assert_eq!(
            built.update(Payload::Atom(Atom::with_overlays(bar(100.0), bear))),
            Some(Payload::Bool(false)),
        );
    }

    #[test]
    #[should_panic(expected = "overlay column not registered")]
    fn get_panics_on_unknown_key_with_registered_list() {
        let mut b = Schema::builder();
        b.add_real("vol_20");
        let schema = b.finish();
        let spec: SourceSpec = serde_norway::from_str("!get { key: missing }").unwrap();
        let _ = spec.build(&Position::new(), &schema);
    }

    #[test]
    #[should_panic(expected = "no overlay side channel is bound")]
    fn get_panics_on_empty_schema_with_hint() {
        let spec: SourceSpec = serde_norway::from_str("!get { key: anything }").unwrap();
        let _ = spec.build(&Position::new(), &Schema::empty());
    }

    #[test]
    #[should_panic(expected = "must be Bool")]
    fn signal_get_on_real_column_hints_at_comparison() {
        let mut b = Schema::builder();
        b.add_real("vol_20");
        let schema = b.finish();
        let spec: SignalSpec = serde_norway::from_str("!get { key: vol_20 }").unwrap();
        let _ = spec.build(&Position::new(), &schema);
    }

    #[test]
    #[should_panic(expected = "!str_eq")]
    fn signal_get_on_str_column_hints_at_str_eq() {
        let mut b = Schema::builder();
        b.add_str("regime");
        let schema = b.finish();
        let spec: SignalSpec = serde_norway::from_str("!get { key: regime }").unwrap();
        let _ = spec.build(&Position::new(), &schema);
    }

    #[test]
    fn calendar_source_tags_decompose_atom_time() {
        // Each bare calendar tag parses, builds, and emits the expected
        // component on a timed atom.
        use crate::dyn_indicator::DynType;
        use fugazi::types::Timestamp;

        // 2024-03-15 12:34:56 UTC — Friday, Q1, DOY 75.
        let atom = Atom::with_time(bar(1.0), Timestamp(1_710_506_096_000));

        for (yaml, want) in [
            ("year", 2024.0),
            ("month", 3.0),
            ("day", 15.0),
            ("hour", 12.0),
            ("minute", 34.0),
            ("second", 56.0),
            ("day_of_week", 5.0),
            ("day_of_year", 75.0),
            ("quarter", 1.0),
            ("unix_seconds", 1_710_506_096.0),
            ("unix_millis", 1_710_506_096_000.0),
        ] {
            let spec: SourceSpec = serde_norway::from_str(yaml).unwrap();
            let mut built = spec.build(&Position::new(), &Schema::empty());
            assert_eq!(built.output_type(), DynType::Real, "{yaml}: output type");
            assert_eq!(
                built.update(Payload::Atom(atom.clone())),
                Some(Payload::Real(want)),
                "{yaml}: value on 2024-03-15 12:34:56 UTC",
            );
        }

        // `!time` is the raw Timestamp payload, not a scalar.
        let spec: SourceSpec = serde_norway::from_str("time").unwrap();
        let mut built = spec.build(&Position::new(), &Schema::empty());
        assert_eq!(built.output_type(), DynType::Time);
        assert_eq!(
            built.update(Payload::Atom(atom.clone())),
            Some(Payload::Time(Timestamp(1_710_506_096_000))),
        );
    }

    #[test]
    fn calendar_signal_tags_gate_by_weekday() {
        use fugazi::types::Timestamp;

        // 2024-03-15 (Fri) vs 2024-03-16 (Sat).
        let fri = Atom::with_time(bar(1.0), Timestamp(1_710_506_096_000));
        let sat = Atom::with_time(bar(1.0), Timestamp(1_710_506_096_000 + 86_400_000));

        let mut wd = serde_norway::from_str::<SignalSpec>("is_weekday")
            .unwrap()
            .build(&Position::new(), &Schema::empty());
        assert_eq!(
            wd.update(Payload::Atom(fri.clone())),
            Some(Payload::Bool(true)),
        );
        assert_eq!(
            wd.update(Payload::Atom(sat.clone())),
            Some(Payload::Bool(false)),
        );

        let mut we = serde_norway::from_str::<SignalSpec>("is_weekend")
            .unwrap()
            .build(&Position::new(), &Schema::empty());
        assert_eq!(we.update(Payload::Atom(fri)), Some(Payload::Bool(false)));
        assert_eq!(we.update(Payload::Atom(sat)), Some(Payload::Bool(true)));
    }

    #[test]
    fn calendar_source_none_on_untimed_atom() {
        // A calendar accessor over a bare Atom (time=None) yields None — same
        // shape as a not-yet-warm indicator.
        let spec: SourceSpec = serde_norway::from_str("year").unwrap();
        let mut built = spec.build(&Position::new(), &Schema::empty());
        assert_eq!(built.update(Payload::Atom(bar(1.0).into())), None);
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
        let mut built = spec.build(&Position::new(), &Schema::empty());
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
