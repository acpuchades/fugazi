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
//! * [`ExprSpec`] (see [`expr`]) → [`crate::dyn_indicator::DynValue`] — a
//!   value-producing expression (nominally `Output = Real`, but polymorphic
//!   over the runtime [`DynType`](crate::dyn_indicator::DynType) — some
//!   variants yield `Atom` / `Candle` / `Str` / `Time`).
//! * [`SignalSpec`] (see [`signal`]) → boolean condition (a `Signal`).
//! * [`SingleStrategySpec`] (see [`strategy`]) → [`fugazi::strategies::SingleAssetStrategy`] —
//!   the decision layer.
//!
//! The enums are *externally tagged* (serde's default), so an indicator reads as
//! a single-key map — `{ema: {source: close, period: 20}}` — and a parameterless
//! leaf or bar indicator reads as a bare string — `close`, `obv`.

mod basket;
mod expr;
mod multi_asset;
mod pairs;
mod portfolio;
mod preset;
mod signal;
mod strategy;
mod template;
mod trailing;

/// The load-time passes every strategy document goes through before typed
/// deserialization, in order: parse the YAML into an untyped tree, splice in
/// every `!import`ed document, then resolve every `!param` placeholder against
/// `params`. (`!arg` is deliberately *not* resolved here — it belongs to a
/// [`SpecTemplate`] and resolves per-instance at build time.)
///
/// Imports run before params so an imported fragment is a first-class part of
/// the document: it may carry its own `!param` placeholders, resolved from the
/// same table as the importer. `base` is the directory relative import paths
/// resolve against — the importing document's own directory (see
/// [`crate::imports`] and [`crate::input::Source::base_dir`]).
///
/// `label` is a short origin string (a file path, `(inline)`, …) folded into
/// the parse-error prefix so a user reading the error sees which document
/// failed. Import splices carry their own file label; the passed `label` names
/// only the *importing* document.
fn load_value(
    text: &str,
    params: &std::collections::HashMap<String, serde_json::Value>,
    base: &std::path::Path,
    label: &str,
) -> anyhow::Result<serde_json::Value> {
    let value = crate::input::parse_value_at(text, label)?;
    let value = crate::imports::resolve(value, base)?;
    crate::params::substitute(value, params)
}

#[allow(unused_imports)]
pub use basket::{BasketStrategySpec, SelectionRuleSpec};
pub use expr::ExprSpec;
#[allow(unused_imports)]
pub use expr::ValueLit;
#[allow(unused_imports)]
pub use multi_asset::MultiAssetStrategySpec;
#[allow(unused_imports)]
pub use pairs::PairsStrategySpec;
#[allow(unused_imports)]
pub use portfolio::{PortfolioSpec, PortfolioChildSpec, PortfolioChildStrategy};
#[allow(unused_imports)]
pub use preset::{StrategyPreset, StrategyRef};
#[allow(unused_imports)]
pub use signal::SignalSpec;
#[allow(unused_imports)]
pub use signal::StrOperand;
pub use strategy::SingleStrategySpec;
#[allow(unused_imports)]
pub use template::SpecTemplate;
#[allow(unused_imports)]
pub(crate) use multi_asset::DynMultiAssetStrategy;
#[allow(unused_imports)]
pub(crate) use pairs::DynPairsStrategy;
#[allow(unused_imports)]
pub(crate) use portfolio::DynPortfolio;
#[allow(unused_imports)]
pub(crate) use strategy::{DynSingleStrategy, SideSpec};

#[cfg(test)]
mod tests {
    use super::*;
    use crate::dyn_indicator::{DynIndicator, DynValue as Payload};
    use fugazi::indicators::{
        Book, Correlation, Current, Ema, GarmanKlass, Kurtosis, Parkinson, Position, RogersSatchell,
        Skewness, VarianceRatio, ZScore,
    };
    use fugazi::prelude::*;
    use fugazi::types::Snapshot;

    fn bar(close: Real) -> Candle {
        Candle::new(close, close, close, close, 0.0)
    }

    /// Wrap a candle into the single-entry `Snapshot<String>` the CLI's
    /// built DynIndicators consume — the same shape the CLI driver feeds
    /// end-to-end via `!pick`-rooted leaves.
    fn snap(c: Candle) -> Snapshot<String> {
        Snapshot::<String>::of_atom(c.into())
    }

    /// A multi-asset snapshot tagging each `(symbol, close)` — the shape a
    /// pairs / basket strategy reads (and the shape the widened trailing tags
    /// forward whole to their embedded strategy).
    fn multi_snap(entries: &[(&str, Real)]) -> Snapshot<String> {
        let mut s = Snapshot::<String>::new();
        for (sym, px) in entries {
            s.push(Some((*sym).to_string()), None, bar(*px).into());
        }
        s
    }

    /// Feed a `Box<dyn DynIndicator>` a candle and unwrap the payload as `Real`.
    fn feed_real(source: &mut Box<dyn DynIndicator>, c: Candle) -> Option<Real> {
        match source.update(Payload::Snapshot(snap(c)))? {
            Payload::Real(x) => Some(x),
            other => panic!("expected Real payload, got {other:?}"),
        }
    }

    /// Feed and unwrap as `bool` — for signal-side tests.
    fn feed_bool(source: &mut Box<dyn DynIndicator>, c: Candle) -> Option<bool> {
        match source.update(Payload::Snapshot(snap(c)))? {
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
        let mut sig = spec.build(&Position::new(), &Book::new(1.0), None, &Schema::empty());
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
        let spec: SingleStrategySpec = serde_json::from_value(json).unwrap();
        assert_eq!(spec.symbol, "BTC");
        assert!(spec.long.is_some());
        let _ = spec.build(1_000.0, &Schema::empty());
    }

    #[test]
    fn default_source_is_close() {
        let spec: ExprSpec = serde_norway::from_str("!ema { period: 3 }").unwrap();
        let mut ema = spec.build(&Position::new(), &Book::new(1.0), None, &Schema::empty());
        let mut reference = Ema::new(Current::close(), 3);
        for p in [1.0, 2.0, 3.0, 4.0, 5.0] {
            assert_eq!(feed_real(&mut ema, bar(p)), reference.update(bar(p).into()));
        }
    }

    #[test]
    fn distribution_shape_tags_match_reference() {
        // `!skewness` / `!kurtosis` / `!zscore` default their source to close and
        // build to the library indicator, matching a hand-wired reference.
        let closes = [10.0, 12.0, 9.0, 14.0, 8.0, 15.0, 11.0];

        let sk: ExprSpec = serde_norway::from_str("!skewness { period: 4 }").unwrap();
        let mut sk = sk.build(&Position::new(), &Book::new(1.0), None, &Schema::empty());
        let mut sk_ref = Skewness::new(Current::close(), 4);

        let ku: ExprSpec = serde_norway::from_str("!kurtosis { period: 4 }").unwrap();
        let mut ku = ku.build(&Position::new(), &Book::new(1.0), None, &Schema::empty());
        let mut ku_ref = Kurtosis::new(Current::close(), 4);

        let z: ExprSpec = serde_norway::from_str("!zscore { period: 4 }").unwrap();
        let mut z = z.build(&Position::new(), &Book::new(1.0), None, &Schema::empty());
        let mut z_ref = ZScore::new(Current::close(), 4);

        for p in closes {
            assert_eq!(feed_real(&mut sk, bar(p)), sk_ref.update(bar(p).into()));
            assert_eq!(feed_real(&mut ku, bar(p)), ku_ref.update(bar(p).into()));
            assert_eq!(feed_real(&mut z, bar(p)), z_ref.update(bar(p).into()));
        }
    }

    #[test]
    fn correlation_tag_matches_reference() {
        // Lag-1 autocorrelation: lhs close vs. its own previous value.
        let spec: ExprSpec =
            serde_norway::from_str("!correlation { lhs: close, rhs: !lag { periods: 1 }, period: 3 }")
                .unwrap();
        let mut built = spec.build(&Position::new(), &Book::new(1.0), None, &Schema::empty());
        let mut reference = Correlation::new(Current::close(), Current::close().lag(1), 3);
        for p in [10.0, 12.0, 9.0, 14.0, 8.0, 15.0] {
            assert_eq!(feed_real(&mut built, bar(p)), reference.update(bar(p).into()));
        }
    }

    #[test]
    fn variance_ratio_tag_matches_reference() {
        // `!variance_ratio` defaults its source to close and builds to the
        // library indicator (`> 1` trending, `< 1` mean-reverting).
        let spec: ExprSpec =
            serde_norway::from_str("!variance_ratio { period: 5, lag: 2 }").unwrap();
        let mut built = spec.build(&Position::new(), &Book::new(1.0), None, &Schema::empty());
        let mut reference = VarianceRatio::new(Current::close(), 5, 2);
        for p in [10.0, 12.0, 9.0, 14.0, 8.0, 15.0, 11.0] {
            assert_eq!(feed_real(&mut built, bar(p)), reference.update(bar(p).into()));
        }
    }

    #[test]
    fn log_defaults_to_natural_and_accepts_explicit_base() {
        // Default base: natural log (`e`).
        let bare: ExprSpec = serde_norway::from_str("!log").unwrap();
        let mut ln = bare.build(&Position::new(), &Book::new(1.0), None, &Schema::empty());
        for p in [1.0, std::f64::consts::E, 10.0, 100.0] {
            let got = feed_real(&mut ln, bar(p)).unwrap();
            assert!((got - p.ln()).abs() < 1e-12, "ln({p})");
        }

        // Explicit base: 10.
        let spec: ExprSpec = serde_norway::from_str("!log { base: 10.0 }").unwrap();
        let mut log10 = spec.build(&Position::new(), &Book::new(1.0), None, &Schema::empty());
        for p in [1.0, 10.0, 1000.0] {
            let got = feed_real(&mut log10, bar(p)).unwrap();
            assert!((got - p.log10()).abs() < 1e-12, "log10({p})");
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
        let spec = SingleStrategySpec::from_text_with_params(yaml, &std::collections::HashMap::new())
            .unwrap();
        assert_eq!(spec.symbol, "BTC");
        let _strat = spec.build(1_000.0, &Schema::empty());
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
            SingleStrategySpec::from_text_with_params(yaml, &std::collections::HashMap::new()).unwrap();
        let mut strat = spec.build(1_000.0, &Schema::empty());
        let mut w = PaperWallet::new(1_000.0);
        for c in [
            Candle::new(100.0, 100.0, 100.0, 100.0, 0.0),
            Candle::new(100.0, 100.0, 100.0, 100.0, 0.0),
            Candle::new(95.0, 96.0, 88.0, 89.0, 0.0),
        ] {
            for fill in w.update("BTC".to_string(), c) {
                strat.on_fill(&fill);
            }
            strat.update(snap(c));
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
        let wrapped = spec.build(&Position::new(), &Book::new(1.0), None, &Schema::empty());
        let inner_raw = Ema::new(Current::close(), 3).above(0.0);
        assert_eq!(wrapped.warm_up_period(), inner_raw.warm_up_period());
        assert_eq!(wrapped.unstable_period(), 0);
        assert_eq!(wrapped.stable_period(), inner_raw.warm_up_period());
        assert!(inner_raw.stable_period() > inner_raw.warm_up_period());
    }

    #[test]
    fn unstable_source_zeroes_unstable_period_but_forwards_output() {
        let yaml = r#"!unstable { source: !ema { period: 5 } }"#;
        let spec: ExprSpec = serde_norway::from_str(yaml).unwrap();
        let wrapped = spec.build(&Position::new(), &Book::new(1.0), None, &Schema::empty());
        let inner_raw = Ema::new(Current::close(), 5);
        assert_eq!(wrapped.warm_up_period(), inner_raw.warm_up_period());
        assert_eq!(wrapped.unstable_period(), 0);
        assert_eq!(wrapped.stable_period(), inner_raw.warm_up_period());
    }

    #[test]
    fn inline_yaml_anchors_reused_across_sides() {
        // Anchors defined inline at their first use site are inlined by the
        // YAML parser at each `*name` alias, so a shared signal can be defined
        // once on `long` and reused on `short` without repeating the tree.
        let yaml = r#"
            symbol: BTC
            long:
              enter: &cross_up !crosses_above { lhs: !sma { period: 3 }, rhs: !sma { period: 8 } }
              exit:  &cross_dn !crosses_below { lhs: !sma { period: 3 }, rhs: !sma { period: 8 } }
            short:
              enter: *cross_dn
              exit:  *cross_up
        "#;
        let spec = SingleStrategySpec::from_text_with_params(yaml, &std::collections::HashMap::new())
            .unwrap();
        assert_eq!(spec.symbol, "BTC");
        assert!(spec.long.is_some() && spec.short.is_some());
        let _ = spec.build(1_000.0, &Schema::empty());
    }

    #[test]
    fn parses_strategy_with_vol_target_sizing() {
        // The `sizing:` field wires a real-valued source into the
        // strategy's position_sizing slot. Verify parse + build for the
        // vol-target helper.
        let yaml = r#"
            symbol: BTC
            long:
              enter: !value true
            sizing: !vol_target { target: 0.20, window: 20, bars_per_year: 252 }
        "#;
        let spec = SingleStrategySpec::from_text_with_params(yaml, &std::collections::HashMap::new())
            .unwrap();
        assert!(spec.sizing.is_some());
        let _built = spec.build(1_000.0, &Schema::empty());
    }

    #[test]
    fn parses_strategy_with_atr_risk_sizing() {
        let yaml = r#"
            symbol: BTC
            long:
              enter: !value true
            sizing: !atr_risk { risk_frac: 0.01, period: 14, atr_multiple: 2.0 }
        "#;
        let spec = SingleStrategySpec::from_text_with_params(yaml, &std::collections::HashMap::new())
            .unwrap();
        assert!(spec.sizing.is_some());
        let _built = spec.build(1_000.0, &Schema::empty());
    }

    #[test]
    fn parses_strategy_with_drawdown_throttle_sizing() {
        let yaml = r#"
            symbol: BTC
            long:
              enter: !value true
            sizing: !drawdown_throttle { max_drawdown: 0.20 }
        "#;
        let spec = SingleStrategySpec::from_text_with_params(yaml, &std::collections::HashMap::new())
            .unwrap();
        assert!(spec.sizing.is_some());
        let _built = spec.build(1_000.0, &Schema::empty());
    }

    #[test]
    fn parses_strategy_with_equity_vol_target_sizing() {
        let yaml = r#"
            symbol: BTC
            long:
              enter: !value true
            sizing: !equity_vol_target { target: 0.15, window: 60, bars_per_year: 252 }
        "#;
        let spec = SingleStrategySpec::from_text_with_params(yaml, &std::collections::HashMap::new())
            .unwrap();
        assert!(spec.sizing.is_some());
        let _built = spec.build(1_000.0, &Schema::empty());
    }

    #[test]
    fn parses_strategy_with_fractional_kelly_sizing() {
        let yaml = r#"
            symbol: BTC
            long:
              enter: !value true
            sizing: !fractional_kelly { kelly_fraction: 0.5, window: 30 }
        "#;
        let spec = SingleStrategySpec::from_text_with_params(yaml, &std::collections::HashMap::new())
            .unwrap();
        assert!(spec.sizing.is_some());
        let _built = spec.build(1_000.0, &Schema::empty());
    }

    #[test]
    fn parses_an_inline_flow_map_strategy() {
        let doc = r#"{"symbol":"ETH","long":{"enter":{"crosses_above":
            {"lhs":{"sma":{"period":5}},"rhs":{"sma":{"period":20}}}}}}"#;
        let spec = SingleStrategySpec::from_text_with_params(doc, &std::collections::HashMap::new())
            .unwrap();
        assert_eq!(spec.symbol, "ETH");
        let _strat = spec.build(1_000.0, &Schema::empty());
    }

    #[test]
    fn resample_tag_projects_the_field() {
        // `!resample { every: N, inner: close }` emits the resampled close on
        // the Nth base tick, None between.
        let spec: ExprSpec =
            serde_norway::from_str("!resample { every: 4, inner: close }").unwrap();
        let mut built = spec.build(&Position::new(), &Book::new(1.0), None, &Schema::empty());
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
        let spec: ExprSpec = serde_norway::from_str(
            "!latch { source: !resample { every: 3, inner: close } }",
        )
        .unwrap();
        let mut built = spec.build(&Position::new(), &Book::new(1.0), None, &Schema::empty());
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
        let obv: ExprSpec = serde_norway::from_str("!obv").unwrap();
        let mut built = obv.build(&Position::new(), &Book::new(1.0), None, &Schema::empty());
        // OBV seeds at first bar's volume.
        assert_eq!(
            feed_real(&mut built, Candle::new(1.0, 1.0, 1.0, 1.0, 100.0)),
            Some(100.0)
        );

        // And still parses with an explicit source override.
        let obv_htf: ExprSpec =
            serde_norway::from_str("!obv { source: !resample { every: 2, inner: current } }")
                .unwrap();
        let _ = obv_htf.build(&Position::new(), &Book::new(1.0), None, &Schema::empty());
    }

    #[test]
    fn atr_tag_parses_with_default_current_source() {
        // `!atr { period: 3 }` without a source keeps its historical form.
        let spec: ExprSpec = serde_norway::from_str("!atr { period: 3 }").unwrap();
        let _ = spec.build(&Position::new(), &Book::new(1.0), None, &Schema::empty());
    }

    #[test]
    fn range_volatility_tags_match_reference() {
        // `!parkinson` / `!garman_klass` / `!rogers_satchell` default their
        // candle source to `current` and build to the library indicator,
        // matching a hand-wired reference over varied OHLC bars.
        let ohlc = [
            Candle::new(10.0, 11.0, 9.0, 10.5, 1.0),
            Candle::new(10.5, 12.0, 8.0, 11.0, 1.0),
            Candle::new(11.0, 13.0, 10.0, 12.0, 1.0),
            Candle::new(12.0, 12.5, 11.0, 11.5, 1.0),
            Candle::new(11.5, 14.0, 11.0, 13.0, 1.0),
        ];

        let pk: ExprSpec = serde_norway::from_str("!parkinson { period: 3 }").unwrap();
        let mut pk = pk.build(&Position::new(), &Book::new(1.0), None, &Schema::empty());
        let mut pk_ref = Parkinson::new(Current::candle(), 3);

        let gk: ExprSpec = serde_norway::from_str("!garman_klass { period: 3 }").unwrap();
        let mut gk = gk.build(&Position::new(), &Book::new(1.0), None, &Schema::empty());
        let mut gk_ref = GarmanKlass::new(Current::candle(), 3);

        let rs: ExprSpec = serde_norway::from_str("!rogers_satchell { period: 3 }").unwrap();
        let mut rs = rs.build(&Position::new(), &Book::new(1.0), None, &Schema::empty());
        let mut rs_ref = RogersSatchell::new(Current::candle(), 3);

        for c in ohlc {
            assert_eq!(feed_real(&mut pk, c), pk_ref.update(c.into()));
            assert_eq!(feed_real(&mut gk, c), gk_ref.update(c.into()));
            assert_eq!(feed_real(&mut rs, c), rs_ref.update(c.into()));
        }
    }

    #[test]
    fn keltner_tag_parses_with_default_sources() {
        // Keltner's price source defaults to `close`, its candle source to
        // `current` — so a bare `!keltner_upper { ema_period, atr_period,
        // multiplier }` still parses.
        let spec: ExprSpec = serde_norway::from_str(
            "!keltner_upper { ema_period: 3, atr_period: 3, multiplier: 2.0 }",
        )
        .unwrap();
        let _ = spec.build(&Position::new(), &Book::new(1.0), None, &Schema::empty());
    }

    #[test]
    fn sharpe_tag_builds_and_reads_over_rising_equity() {
        // `!sharpe` embeds a whole single-asset strategy (here an always-long
        // buy-and-hold on X), drives it against a private wallet, and reads a
        // rolling Sharpe over the resulting equity curve. A strictly rising
        // price → a fully-invested rising equity → a positive trailing Sharpe.
        let spec: ExprSpec = serde_norway::from_str(
            "!sharpe { strategy: { symbol: X, long: { enter: !gt { lhs: !close, rhs: !value 0.0 } } }, \
             period: 4, bars_per_year: 252 }",
        )
        .unwrap();
        let mut built = spec.build(&Position::new(), &Book::new(1.0), None, &Schema::empty());
        assert_eq!(built.output_type(), crate::dyn_indicator::DynType::Real);

        let mut last = None;
        for p in [100.0, 102.0, 105.0, 108.0, 112.0, 116.0, 121.0, 127.0] {
            last = built.update(Payload::Snapshot(snap(bar(p))));
        }
        match last {
            Some(Payload::Real(s)) => {
                assert!(s > 0.0, "rising equity should give a positive Sharpe, got {s}")
            }
            other => panic!("expected Some(Real), got {other:?}"),
        }
    }

    #[test]
    fn sharpe_accepts_a_preset_strategy() {
        // The `strategy:` field takes a catalogue preset tag as well as a full
        // spec — `!ma_crossover { … }` builds the same strategy the Rust
        // `trend::ma_crossover` recipe does.
        let spec: ExprSpec = serde_norway::from_str(
            "!sharpe { strategy: !ma_crossover { symbol: X, fast: 2, slow: 4 }, \
             period: 4, bars_per_year: 252 }",
        )
        .unwrap();
        let mut built = spec.build(&Position::new(), &Book::new(1.0), None, &Schema::empty());
        assert_eq!(built.output_type(), crate::dyn_indicator::DynType::Real);
        // Drives a golden-then-death cross without panicking; reads Some once warm.
        let mut last = None;
        for p in [14.0, 13.0, 12.0, 11.0, 12.0, 14.0, 16.0, 18.0, 15.0, 12.0] {
            last = built.update(Payload::Snapshot(snap(bar(p))));
        }
        assert!(last.is_some(), "trailing Sharpe over a preset should read once warm");
    }

    #[test]
    fn sharpe_accepts_a_pairs_strategy() {
        // The widened `strategy:` field routes a `left`/`right` map to a pairs
        // strategy. Fed tagged 2-entry snapshots (the shape the old single-asset
        // engine would panic on), the embedded pair prices both legs and the
        // trailing Sharpe reads over its aggregate equity curve.
        use super::trailing::AnyStrategyRef;
        let yaml = r#"
            !sharpe
            strategy:
              left: BTC
              right: ETH
              enter: !value true
            period: 4
            bars_per_year: 252
        "#;
        let spec: ExprSpec = serde_norway::from_str(yaml).unwrap();
        // The `strategy:` field routed to the pairs arm.
        match &spec {
            ExprSpec::Sharpe { strategy, .. } => {
                assert!(matches!(**strategy, AnyStrategyRef::Pairs(_)))
            }
            other => panic!("expected a Sharpe spec, got {other:?}"),
        }

        let mut built = spec.build(&Position::new(), &Book::new(1.0), None, &Schema::empty());
        assert_eq!(built.output_type(), crate::dyn_indicator::DynType::Real);

        // BTC drifts up, ETH drifts down: long-BTC / short-ETH earns on both
        // legs → a rising, variable equity curve → a positive trailing Sharpe.
        let btc = [100.0, 102.0, 101.0, 104.0, 106.0, 108.0, 110.0, 113.0];
        let eth = [100.0, 99.0, 100.0, 97.0, 96.0, 95.0, 94.0, 92.0];
        let mut last = None;
        for i in 0..btc.len() {
            last = built.update(Payload::Snapshot(multi_snap(&[
                ("BTC", btc[i]),
                ("ETH", eth[i]),
            ])));
        }
        match last {
            Some(Payload::Real(s)) => {
                assert!(s > 0.0, "net-profitable pair should give a positive Sharpe, got {s}")
            }
            other => panic!("expected Some(Real), got {other:?}"),
        }
    }

    #[test]
    fn sharpe_accepts_a_basket_strategy() {
        // A `selection` map routes to a basket strategy. Basket score/sizing are
        // `SpecTemplate`s that capture normalised JSON, so this must go through
        // the CLI's `parse_value` (YAML→JSON tag normalisation) path — the raw
        // `serde_norway::from_str` path can't capture `!tag` into a template.
        use super::trailing::AnyStrategyRef;
        let yaml = r#"
            !sharpe
            strategy:
              selection: !top_bottom { longs: 1, shorts: 1 }
              score: !roc { source: !close { source: !pick { symbol: !arg SYM } }, periods: 2 }
              sizing: !equal_weight 2
            period: 3
            bars_per_year: 252
        "#;
        let json = crate::input::parse_value(yaml).unwrap();
        let spec: ExprSpec = serde_json::from_value(json).unwrap();
        match &spec {
            ExprSpec::Sharpe { strategy, .. } => {
                assert!(matches!(**strategy, AnyStrategyRef::Basket(_)))
            }
            other => panic!("expected a Sharpe spec, got {other:?}"),
        }

        // Builds and drives a 3-symbol universe without panicking (the embedded
        // basket ranks per-symbol ROC, longs the top / shorts the bottom).
        let mut built = spec.build(&Position::new(), &Book::new(1.0), None, &Schema::empty());
        assert_eq!(built.output_type(), crate::dyn_indicator::DynType::Real);
        for i in 0..8 {
            let f = i as Real;
            let _ = built.update(Payload::Snapshot(multi_snap(&[
                ("A", 100.0 + f * 2.0),
                ("B", 100.0 - f),
                ("C", 100.0 + f * 0.5),
            ])));
        }
    }

    #[test]
    fn sharpe_accepts_a_multi_asset_strategy() {
        // A bare mapping without `symbol` / pairs / basket keys routes to
        // multi-asset. The embedded multi runs independent per-symbol
        // decisions; the trailing Sharpe reads over its aggregate equity.
        use super::trailing::AnyStrategyRef;
        let yaml = r#"
            !sharpe
            strategy:
              long:
                enter: !gt { lhs: !close { source: !pick { symbol: !arg SYM } }, rhs: !value 0.0 }
              sizing: !equal_weight 2
            period: 3
            bars_per_year: 252
        "#;
        let json = crate::input::parse_value(yaml).unwrap();
        let spec: ExprSpec = serde_json::from_value(json).unwrap();
        match &spec {
            ExprSpec::Sharpe { strategy, .. } => {
                assert!(matches!(**strategy, AnyStrategyRef::Multi(_)))
            }
            other => panic!("expected a Sharpe spec, got {other:?}"),
        }
        // Builds without panicking; drives on a small 2-symbol path.
        let mut built = spec.build(&Position::new(), &Book::new(1.0), None, &Schema::empty());
        assert_eq!(built.output_type(), crate::dyn_indicator::DynType::Real);
        for i in 0..6 {
            let f = i as Real;
            let _ = built.update(Payload::Snapshot(multi_snap(&[
                ("A", 100.0 + f),
                ("B", 100.0 + f * 0.5),
            ])));
        }
    }

    #[test]
    fn max_drawdown_tag_defaults_and_builds() {
        // `!max_drawdown` needs no rf/bpy. Over a rise-then-dip path the
        // trailing drawdown is a defined non-negative fraction.
        let spec: ExprSpec = serde_norway::from_str(
            "!max_drawdown { strategy: { symbol: X, long: { enter: !gt { lhs: !close, rhs: !value 0.0 } } }, \
             period: 3 }",
        )
        .unwrap();
        let mut built = spec.build(&Position::new(), &Book::new(1.0), None, &Schema::empty());
        let mut last = None;
        for p in [100.0, 110.0, 120.0, 108.0, 96.0] {
            last = built.update(Payload::Snapshot(snap(bar(p))));
        }
        match last {
            Some(Payload::Real(dd)) => assert!(dd >= 0.0, "drawdown is a non-negative fraction"),
            other => panic!("expected Some(Real), got {other:?}"),
        }
    }

    #[test]
    fn sortino_tag_defaults_risk_free_rate_to_zero() {
        // `risk_free_rate` is optional (defaults to 0), so a bare
        // `!sortino { strategy, period, bars_per_year }` parses and builds.
        let spec: ExprSpec = serde_norway::from_str(
            "!sortino { strategy: { symbol: X, long: { enter: !gt { lhs: !close, rhs: !value 0.0 } } }, \
             period: 4, bars_per_year: 365 }",
        )
        .unwrap();
        assert!(matches!(spec, ExprSpec::Sortino { risk_free_rate, .. } if risk_free_rate == 0.0));
        let _ = spec.build(&Position::new(), &Book::new(1.0), None, &Schema::empty());
    }

    #[test]
    fn get_real_dispatches_to_get_real_leaf() {
        // `!get { key: vol_20 }` in a Real position reads the numeric column.
        let mut b = Schema::builder();
        b.add_real("vol_20");
        let schema = b.finish();

        let spec: ExprSpec = serde_norway::from_str("!get { key: vol_20 }").unwrap();
        let mut built = spec.build(&Position::new(), &Book::new(1.0), None, &schema);
        assert_eq!(built.output_type(), crate::dyn_indicator::DynType::Real);

        let ov = OverlayInfo::new(schema.clone(), vec![OverlayValue::Real(0.42)]);
        let atom = Atom::with_overlays(bar(100.0), ov);
        assert_eq!(built.update(Payload::Snapshot(Snapshot::of_atom(atom.clone()))), Some(Payload::Real(0.42)));
    }

    #[test]
    fn get_bool_dispatches_to_get_bool_signal() {
        // `!get { key: risk_on }` in a signal position reads the Bool column
        // directly — no comparison needed.
        let mut b = Schema::builder();
        b.add_bool("risk_on");
        let schema = b.finish();

        let spec: SignalSpec = serde_norway::from_str("!get { key: risk_on }").unwrap();
        let mut built = spec.build(&Position::new(), &Book::new(1.0), None, &schema);
        assert_eq!(built.output_type(), crate::dyn_indicator::DynType::Bool);

        let ov = OverlayInfo::new(schema.clone(), vec![OverlayValue::Bool(true)]);
        let atom = Atom::with_overlays(bar(100.0), ov);
        assert_eq!(built.update(Payload::Snapshot(Snapshot::of_atom(atom.clone()))), Some(Payload::Bool(true)));
    }

    #[test]
    fn bare_number_auto_wraps_as_value_in_expr_position() {
        // `rhs: 100` (bare number) is auto-wrapped as `!value 100` — no
        // more `!value` boilerplate needed in the common comparison
        // shape. Same result as writing `rhs: !value 100` explicitly.
        let spec_bare: SignalSpec =
            serde_norway::from_str("!gt { lhs: close, rhs: 100 }").unwrap();
        let spec_explicit: SignalSpec =
            serde_norway::from_str("!gt { lhs: close, rhs: !value 100 }").unwrap();
        let mut b1 = spec_bare.build(&Position::new(), &Book::new(1.0), None, &Schema::empty());
        let mut b2 = spec_explicit.build(&Position::new(), &Book::new(1.0), None, &Schema::empty());
        for px in [99.0, 100.0, 101.0] {
            assert_eq!(
                feed_bool(&mut b1, bar(px)),
                feed_bool(&mut b2, bar(px)),
                "bare-number and !value-wrapped forms must agree at px={px}",
            );
        }
    }

    #[test]
    fn bare_bool_auto_wraps_as_value_in_signal_position() {
        // `enter: true` / `exit: false` (bare bools) are auto-wrapped as
        // `!value true` / `!value false` in signal positions. Removes
        // the boilerplate from constant signal slots.
        let spec_bare: SignalSpec = serde_norway::from_str("true").unwrap();
        let spec_explicit: SignalSpec = serde_norway::from_str("!value true").unwrap();
        let mut b1 = spec_bare.build(&Position::new(), &Book::new(1.0), None, &Schema::empty());
        let mut b2 = spec_explicit.build(&Position::new(), &Book::new(1.0), None, &Schema::empty());
        assert_eq!(feed_bool(&mut b1, bar(1.0)), feed_bool(&mut b2, bar(1.0)));
    }

    #[test]
    fn bare_number_list_auto_wraps_as_value_in_expr_position() {
        // A bare `[0.5, 0.5]` in an ExprSpec position auto-wraps to
        // `!value [0.5, 0.5]` — the common case for portfolio weights
        // being cleaner without the `!value` prefix.
        let spec_bare: ExprSpec = serde_norway::from_str("[0.5, 0.5]").unwrap();
        let spec_explicit: ExprSpec = serde_norway::from_str("!value [0.5, 0.5]").unwrap();
        assert!(matches!(spec_bare, ExprSpec::Value(_)));
        assert!(matches!(spec_explicit, ExprSpec::Value(_)));
    }

    #[test]
    fn polymorphic_eq_dispatches_by_lhs_output_type() {
        // `!eq` inspects `lhs`'s built output type and picks compare::Eq
        // (Real) or compare::StrEq (Str). Same YAML shape covers both.
        let mut b = Schema::builder();
        b.add_str("regime");
        let schema = b.finish();
        // Str path: lhs is a Str column, rhs is a !value Str literal.
        let spec: SignalSpec = serde_norway::from_str(
            "!eq { lhs: !get { key: regime }, rhs: !value bull }",
        )
        .unwrap();
        let mut built = spec.build(&Position::new(), &Book::new(1.0), None, &schema);
        let bull = OverlayInfo::new(
            schema.clone(),
            vec![OverlayValue::Str(std::sync::Arc::from("bull"))],
        );
        let bear = OverlayInfo::new(
            schema.clone(),
            vec![OverlayValue::Str(std::sync::Arc::from("bear"))],
        );
        assert_eq!(
            built.update(Payload::Snapshot(Snapshot::of_atom(Atom::with_overlays(bar(100.0), bull)))),
            Some(Payload::Bool(true)),
        );
        assert_eq!(
            built.update(Payload::Snapshot(Snapshot::of_atom(Atom::with_overlays(bar(100.0), bear)))),
            Some(Payload::Bool(false)),
        );
        // Real path: lhs is close, rhs is a !value number. Same tag, no
        // change in shape needed.
        let spec: SignalSpec =
            serde_norway::from_str("!eq { lhs: close, rhs: !value 100.0 }").unwrap();
        let mut built = spec.build(&Position::new(), &Book::new(1.0), None, &Schema::empty());
        assert_eq!(feed_bool(&mut built, bar(100.0)), Some(true));
        assert_eq!(feed_bool(&mut built, bar(99.9)), Some(false));
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
        let mut built = spec.build(&Position::new(), &Book::new(1.0), None, &schema);
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
            built.update(Payload::Snapshot(Snapshot::of_atom(Atom::with_overlays(bar(100.0), bull)))),
            Some(Payload::Bool(true)),
        );
        assert_eq!(
            built.update(Payload::Snapshot(Snapshot::of_atom(Atom::with_overlays(bar(100.0), bear)))),
            Some(Payload::Bool(false)),
        );
    }

    #[test]
    fn import_splices_a_fragment_that_still_sees_the_param_table() {
        // The load order is `parse -> !import -> !param -> typed parse`, so an
        // imported fragment is a first-class part of the document: its own
        // `!param` placeholders resolve from the importing run's --params table.
        let dir = std::env::temp_dir().join("fugazi_spec_import");
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(
            dir.join("enter.yml"),
            "!crosses_above { lhs: !sma { period: !param FAST }, rhs: !sma { period: 8 } }\n",
        )
        .unwrap();

        let params = std::collections::HashMap::from([(
            "FAST".to_string(),
            serde_json::Value::from(3),
        )]);
        let spec = SingleStrategySpec::from_text_with_params_in(
            "symbol: BTC\nlong:\n  enter: !import enter.yml\n  exit: !value false\n",
            &params,
            &dir,
            "(inline)",
        )
        .unwrap();

        assert_eq!(spec.symbol, "BTC");
        // The spliced signal fires like a hand-written one: a fast SMA(3)
        // crossing up through a slow SMA(8).
        let mut strat = spec.build(1_000.0, &Schema::empty());
        let mut fired = false;
        for p in [10.0, 9.0, 8.0, 7.0, 6.0, 7.0, 9.0, 12.0, 15.0, 18.0, 21.0] {
            strat.update(snap(bar(p)));
            fired |= strat.is_ready();
        }
        assert!(fired, "expected the imported crossover signal to build and warm up");
    }

    #[test]
    fn value_builds_a_real_or_a_str_constant_from_the_literal_type() {
        // `!value 70` is the scalar constant; `!value bull` the string one.
        // Quoting decides when the two would collide: `!value "70"` is a string.
        let cases = [
            ("!value 70", crate::dyn_indicator::DynType::Real),
            ("!value bull", crate::dyn_indicator::DynType::Str),
            ("!value \"70\"", crate::dyn_indicator::DynType::Str),
        ];
        for (yaml, want) in cases {
            let spec: ExprSpec = serde_norway::from_str(yaml).unwrap();
            let built = spec.build(&Position::new(), &Book::new(1.0), None, &Schema::empty());
            assert_eq!(built.output_type(), want, "{yaml}");
        }

        let spec: ExprSpec = serde_norway::from_str("!value bull").unwrap();
        let mut built = spec.build(&Position::new(), &Book::new(1.0), None, &Schema::empty());
        assert_eq!(
            built.update(Payload::Snapshot(snap(bar(100.0)))),
            Some(Payload::Str(std::sync::Arc::from("bull"))),
        );
    }

    #[test]
    fn value_rejects_a_literal_that_is_neither_number_nor_string() {
        // A bool in a source position isn't a `!value` — it's the signal-side
        // `!value <bool>` leaf, a different (SignalSpec) tag.
        let err = serde_norway::from_str::<ExprSpec>("!value true")
            .unwrap_err()
            .to_string();
        assert!(err.contains("!value takes a number"), "{err}");
    }

    #[test]
    fn str_eq_takes_a_literal_a_value_string_or_a_second_column_as_rhs() {
        let mut b = Schema::builder();
        b.add_str("regime");
        b.add_str("prev_regime");
        let schema = b.finish();

        let build = |yaml: &str| {
            let spec: SignalSpec = serde_norway::from_str(yaml).unwrap();
            spec.build(&Position::new(), &Book::new(1.0), None, &schema)
        };
        // The bare literal (the original shape), the same constant written the
        // long way, and a second Str column — "the regime is unchanged".
        let mut literal = build("!str_eq { lhs: !get { key: regime }, rhs: bull }");
        let mut constant = build("!str_eq { lhs: !get { key: regime }, rhs: !value bull }");
        let mut cross = build("!str_eq { lhs: !get { key: regime }, rhs: !get { key: prev_regime } }");

        let row = |regime: &str, prev: &str| {
            let ov = OverlayInfo::new(
                schema.clone(),
                vec![
                    OverlayValue::Str(std::sync::Arc::from(regime)),
                    OverlayValue::Str(std::sync::Arc::from(prev)),
                ],
            );
            Payload::Snapshot(Snapshot::of_atom(Atom::with_overlays(bar(100.0), ov)))
        };

        assert_eq!(literal.update(row("bull", "bear")), Some(Payload::Bool(true)));
        assert_eq!(literal.update(row("bear", "bear")), Some(Payload::Bool(false)));

        assert_eq!(constant.update(row("bull", "bear")), Some(Payload::Bool(true)));
        assert_eq!(constant.update(row("bear", "bear")), Some(Payload::Bool(false)));

        assert_eq!(cross.update(row("bull", "bull")), Some(Payload::Bool(true)));
        assert_eq!(cross.update(row("bull", "bear")), Some(Payload::Bool(false)));
    }

    #[test]
    fn value_string_survives_the_yaml_to_json_path() {
        // The CLI deserializes through `yaml_to_json` (for `!param`
        // substitution), not straight from YAML — so the string literal and
        // the `!str_eq` rhs operand have to normalise on that path too.
        let yaml = r#"
            symbol: BTC
            long:
              enter: !str_eq { lhs: !get { key: regime }, rhs: !value bull }
              exit: !str_ne { lhs: !get { key: regime }, rhs: bull }
        "#;
        let value: serde_norway::Value = serde_norway::from_str(yaml).unwrap();
        let json = crate::convert::yaml_to_json(value).unwrap();
        let spec: SingleStrategySpec = serde_json::from_value(json).unwrap();

        let mut b = Schema::builder();
        b.add_str("regime");
        let schema = b.finish();
        let _ = spec.build(1.0, &schema);
    }

    #[test]
    #[should_panic(expected = "overlay column not registered")]
    fn get_panics_on_unknown_key_with_registered_list() {
        let mut b = Schema::builder();
        b.add_real("vol_20");
        let schema = b.finish();
        let spec: ExprSpec = serde_norway::from_str("!get { key: missing }").unwrap();
        let _ = spec.build(&Position::new(), &Book::new(1.0), None, &schema);
    }

    #[test]
    #[should_panic(expected = "no overlay side channel is bound")]
    fn get_panics_on_empty_schema_with_hint() {
        let spec: ExprSpec = serde_norway::from_str("!get { key: anything }").unwrap();
        let _ = spec.build(&Position::new(), &Book::new(1.0), None, &Schema::empty());
    }

    #[test]
    #[should_panic(expected = "must be Bool")]
    fn signal_get_on_real_column_hints_at_comparison() {
        let mut b = Schema::builder();
        b.add_real("vol_20");
        let schema = b.finish();
        let spec: SignalSpec = serde_norway::from_str("!get { key: vol_20 }").unwrap();
        let _ = spec.build(&Position::new(), &Book::new(1.0), None, &schema);
    }

    #[test]
    #[should_panic(expected = "!str_eq")]
    fn signal_get_on_str_column_hints_at_str_eq() {
        let mut b = Schema::builder();
        b.add_str("regime");
        let schema = b.finish();
        let spec: SignalSpec = serde_norway::from_str("!get { key: regime }").unwrap();
        let _ = spec.build(&Position::new(), &Book::new(1.0), None, &schema);
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
            let spec: ExprSpec = serde_norway::from_str(yaml).unwrap();
            let mut built = spec.build(&Position::new(), &Book::new(1.0), None, &Schema::empty());
            assert_eq!(built.output_type(), DynType::Real, "{yaml}: output type");
            assert_eq!(
                built.update(Payload::Snapshot(Snapshot::of_atom(atom.clone()))),
                Some(Payload::Real(want)),
                "{yaml}: value on 2024-03-15 12:34:56 UTC",
            );
        }

        // `!time` is the raw Timestamp payload, not a scalar.
        let spec: ExprSpec = serde_norway::from_str("time").unwrap();
        let mut built = spec.build(&Position::new(), &Book::new(1.0), None, &Schema::empty());
        assert_eq!(built.output_type(), DynType::Time);
        assert_eq!(
            built.update(Payload::Snapshot(Snapshot::of_atom(atom.clone()))),
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
            .build(&Position::new(), &Book::new(1.0), None, &Schema::empty());
        assert_eq!(
            wd.update(Payload::Snapshot(Snapshot::of_atom(fri.clone()))),
            Some(Payload::Bool(true)),
        );
        assert_eq!(
            wd.update(Payload::Snapshot(Snapshot::of_atom(sat.clone()))),
            Some(Payload::Bool(false)),
        );

        let mut we = serde_norway::from_str::<SignalSpec>("is_weekend")
            .unwrap()
            .build(&Position::new(), &Book::new(1.0), None, &Schema::empty());
        assert_eq!(we.update(Payload::Snapshot(Snapshot::of_atom(fri.clone()))), Some(Payload::Bool(false)));
        assert_eq!(we.update(Payload::Snapshot(Snapshot::of_atom(sat.clone()))), Some(Payload::Bool(true)));
    }

    #[test]
    fn calendar_source_tags_read_first_atom_on_multi_symbol_snapshot() {
        // Regression: bare calendar tags used to root through Pick::new(),
        // which panics on 2+ entries. The whole point of a calendar
        // accessor is that atom.time is symbol-agnostic (every entry in a
        // bar's snapshot shares the same wall-clock time), so picking any
        // one is stable. PickAny is now the default source — the same
        // spec should parse, build, and read on a multi-symbol snapshot
        // without touching the panic path.
        use fugazi::types::Timestamp;

        let ts = Timestamp(1_710_506_096_000); // 2024-03-15 12:34:56 UTC
        let atom_btc = Atom::with_time(bar(1.0), ts);
        let atom_eth = Atom::with_time(bar(2.0), ts);
        let mut multi = Snapshot::<String>::new();
        multi.push(Some("BTC".to_string()), None, atom_btc);
        multi.push(Some("ETH".to_string()), None, atom_eth);

        for (yaml, want) in [
            ("month", 3.0),
            ("day", 15.0),
            ("hour", 12.0),
            ("day_of_week", 5.0),
            ("quarter", 1.0),
        ] {
            let spec: ExprSpec = serde_norway::from_str(yaml).unwrap();
            let mut built = spec.build(&Position::new(), &Book::new(1.0), None, &Schema::empty());
            assert_eq!(
                built.update(Payload::Snapshot(multi.clone())),
                Some(Payload::Real(want)),
                "{yaml}: value on 2-symbol snapshot",
            );
        }
    }

    #[test]
    fn cadence_sugar_tags_fire_on_multi_symbol_snapshot() {
        // Regression: `!daily` / `!monthly` etc. desugar to
        // `!changed { source: !<accessor> {} }` — the calendar accessor's
        // empty-map source used to root on Pick::new(), which panicked on
        // 2+ entries. With PickAny as the calendar default, the cadence
        // sugar composes cleanly on a multi-symbol snapshot — the exact
        // shape a portfolio `rebalance_on:` sees.
        use fugazi::types::Timestamp;

        // Two consecutive days in the same month — `!daily` fires on the
        // second bar (day rolls over); `!monthly` stays false.
        let day1 = Timestamp(1_710_506_096_000); // 2024-03-15
        let day2 = Timestamp(1_710_506_096_000 + 86_400_000); // 2024-03-16
        let mk = |ts: Timestamp| {
            let a = Atom::with_time(bar(1.0), ts);
            let b = Atom::with_time(bar(2.0), ts);
            let mut s = Snapshot::<String>::new();
            s.push(Some("BTC".to_string()), None, a);
            s.push(Some("ETH".to_string()), None, b);
            s
        };

        // !changed is None on the warm-up bar (it needs a prior value to
        // compare against), so we only assert on the second bar's edge.
        let spec: SignalSpec = serde_norway::from_str("daily").unwrap();
        let mut built = spec.build(&Position::new(), &Book::new(1.0), None, &Schema::empty());
        let _ = built.update(Payload::Snapshot(mk(day1)));
        assert_eq!(
            built.update(Payload::Snapshot(mk(day2))),
            Some(Payload::Bool(true)),
            "!daily should fire on the day rollover",
        );

        let spec: SignalSpec = serde_norway::from_str("monthly").unwrap();
        let mut built = spec.build(&Position::new(), &Book::new(1.0), None, &Schema::empty());
        let _ = built.update(Payload::Snapshot(mk(day1)));
        assert_eq!(
            built.update(Payload::Snapshot(mk(day2))),
            Some(Payload::Bool(false)),
            "!monthly should not fire on a same-month rollover",
        );
    }

    #[test]
    fn is_weekday_reads_multi_symbol_snapshot_without_panic() {
        // Regression: `!is_weekday` used to root on Pick::new() and would
        // panic on 2+ entries. Now uses PickAny — reads the first atom's
        // time, which is stable because all entries share the timestamp.
        use fugazi::types::Timestamp;

        let fri = Timestamp(1_710_506_096_000); // 2024-03-15 Friday
        let sat = Timestamp(fri.0 + 86_400_000); // 2024-03-16 Saturday
        let mk = |ts: Timestamp| {
            let a = Atom::with_time(bar(1.0), ts);
            let b = Atom::with_time(bar(2.0), ts);
            let mut s = Snapshot::<String>::new();
            s.push(Some("BTC".to_string()), None, a);
            s.push(Some("ETH".to_string()), None, b);
            s
        };

        let spec: SignalSpec = serde_norway::from_str("is_weekday").unwrap();
        let mut built = spec.build(&Position::new(), &Book::new(1.0), None, &Schema::empty());
        assert_eq!(built.update(Payload::Snapshot(mk(fri))), Some(Payload::Bool(true)));
        assert_eq!(built.update(Payload::Snapshot(mk(sat))), Some(Payload::Bool(false)));
    }

    #[test]
    fn calendar_source_none_on_untimed_atom() {
        // A calendar accessor over a bare Atom (time=None) yields None — same
        // shape as a not-yet-warm indicator.
        let spec: ExprSpec = serde_norway::from_str("year").unwrap();
        let mut built = spec.build(&Position::new(), &Book::new(1.0), None, &Schema::empty());
        assert_eq!(built.update(Payload::Snapshot(Snapshot::of_atom(bar(1.0).into()))), None);
    }

    #[test]
    fn if_else_survives_nested_signals_via_json_bridge() {
        // Regression: nested `SignalSpec` inside `ExprSpec::IfElse`'s cond
        // used to fail when the outer document went through the CLI's
        // serde_json → serde_norway::Value bridge (because SignalSpec had
        // no matching `try_from` normaliser). This test hits that path
        // explicitly: build a spec through `SingleStrategySpec::from_text_with_params`
        // (which routes via serde_json) and assert that a nested
        // `!if_else { cond: !and { ... } }` reaches build without erroring.
        let yaml = r#"
            symbol: BTC
            long:
              enter: !value true
            sizing:
              !if_else
                cond:
                  !and
                    lhs: !above { source: close, level: 0.0 }
                    rhs: !below { source: close, level: 1000000.0 }
                if_true: !value 0.5
                if_false: !value 0.0
        "#;
        let spec = SingleStrategySpec::from_text_with_params(
            yaml,
            &std::collections::HashMap::new(),
        )
        .expect("nested !and inside !if_else cond must parse via the JSON bridge");
        let _ = spec.build(1_000.0, &Schema::empty());
    }

    #[test]
    fn equal_weight_yields_the_constant_reciprocal() {
        // `!equal_weight 4` is the sugar for the 1/4 = 0.25 constant per
        // leg — the common basket case for a 4-leg balanced strategy.
        let spec: ExprSpec = serde_norway::from_str("!equal_weight 4").unwrap();
        let mut built = spec.build(&Position::new(), &Book::new(1.0), None, &Schema::empty());
        assert_eq!(feed_real(&mut built, bar(100.0)), Some(0.25));
        assert_eq!(feed_real(&mut built, bar(50.0)), Some(0.25));
    }

    #[test]
    fn if_else_selects_by_condition() {
        // `!if_else { cond, if_true, if_false }`: an ADX-gated momentum
        // score shape without the ADX (which would need many bars to
        // warm) — use a level comparison as the condition so we can
        // trigger both branches on adjacent bars.
        let yaml = r#"
            !if_else
            cond: !above { source: close, level: 100.0 }
            if_true: !value 1.0
            if_false: !value -1.0
        "#;
        let spec: ExprSpec = serde_norway::from_str(yaml).unwrap();
        let mut built = spec.build(&Position::new(), &Book::new(1.0), None, &Schema::empty());
        // close = 99 → cond false → -1; close = 101 → cond true → 1.
        assert_eq!(feed_real(&mut built, bar(99.0)), Some(-1.0));
        assert_eq!(feed_real(&mut built, bar(101.0)), Some(1.0));
        assert_eq!(feed_real(&mut built, bar(100.5)), Some(1.0));
    }

    #[test]
    fn if_else_holds_none_while_selected_branch_warms() {
        // The condition is always true (close > 0) and picks `if_true`
        // (SMA-5, warm-up 5). The ternary reads None for the first four
        // bars while the SELECTED branch is still warming; on bar 5 it
        // reports the SMA's first value.
        let yaml = r#"
            !if_else
            cond: !above { source: close, level: 0.0 }
            if_true: !sma { source: close, period: 5 }
            if_false: !value 99.0
        "#;
        let spec: ExprSpec = serde_norway::from_str(yaml).unwrap();
        let mut built = spec.build(&Position::new(), &Book::new(1.0), None, &Schema::empty());
        for _ in 0..4 {
            assert_eq!(feed_real(&mut built, bar(100.0)), None);
        }
        // Fifth bar: SMA-5 has warmed and the condition is true.
        assert_eq!(feed_real(&mut built, bar(100.0)), Some(100.0));
    }

    #[test]
    fn if_else_publishes_early_when_selected_branch_warms_fast() {
        // Same shape but with the condition inverted: close < 0 is always
        // false, so the ternary picks `if_false` — a Value with warm-up 0,
        // so a `Some` shows up on the very first bar even though the
        // *unselected* branch (SMA-5) has a warm-up of 5. Reported
        // `stable_period()` is still the max, so a downstream consumer
        // that waits on it still waits long enough.
        let yaml = r#"
            !if_else
            cond: !below { source: close, level: 0.0 }
            if_true: !sma { source: close, period: 5 }
            if_false: !value -1.0
        "#;
        let spec: ExprSpec = serde_norway::from_str(yaml).unwrap();
        let mut built = spec.build(&Position::new(), &Book::new(1.0), None, &Schema::empty());
        // First bar: cond is Some(false), if_false is Some(-1.0).
        assert_eq!(feed_real(&mut built, bar(100.0)), Some(-1.0));
        // The reported stability window still covers the slowest source
        // (SMA-5 → warm-up 5), so callers waiting on `stable_period()`
        // don't act on this early Some until the whole tree could
        // theoretically be ready.
        assert!(built.stable_period() >= 5);
    }

    #[test]
    fn match_numeric_dispatches_by_value_equality() {
        // `!match` on a numeric `on` — each case fires when `on == value`.
        // Here `on` is `!current_bar { close }` (effectively the close),
        // and cases dispatch by the actual close value.
        let yaml = r#"
            !match
            on: close
            cases:
              - when: 100.0
                value: !value 1.0
              - when: 200.0
                value: !value 2.0
            default: !value -1.0
        "#;
        let spec: ExprSpec = serde_norway::from_str(yaml).unwrap();
        let mut built = spec.build(&Position::new(), &Book::new(1.0), None, &Schema::empty());
        assert_eq!(feed_real(&mut built, bar(100.0)), Some(1.0));
        assert_eq!(feed_real(&mut built, bar(200.0)), Some(2.0));
        assert_eq!(feed_real(&mut built, bar(150.0)), Some(-1.0));
    }

    #[test]
    fn match_lowers_to_nested_if_else_for_a_single_case() {
        // A one-case `!match` is equivalent to a single `!if_else`:
        // cond fires on equality, if_true is the case's result, if_false
        // is the default.
        let yaml_match = r#"
            !match
            on: close
            cases:
              - when: 42.0
                value: !value 1.0
            default: !value 0.0
        "#;
        let yaml_if_else = r#"
            !if_else
            cond: !eq { lhs: close, rhs: !value 42.0 }
            if_true: !value 1.0
            if_false: !value 0.0
        "#;
        let spec_match: ExprSpec = serde_norway::from_str(yaml_match).unwrap();
        let spec_if_else: ExprSpec = serde_norway::from_str(yaml_if_else).unwrap();
        let mut m = spec_match.build(&Position::new(), &Book::new(1.0), None, &Schema::empty());
        let mut e = spec_if_else.build(&Position::new(), &Book::new(1.0), None, &Schema::empty());
        for px in [41.0, 42.0, 43.0, 42.0, 100.0] {
            assert_eq!(
                feed_real(&mut m, bar(px)),
                feed_real(&mut e, bar(px)),
                "!match one-case must match !if_else at px={px}",
            );
        }
    }

    #[test]
    #[should_panic(expected = "cases")]
    fn match_empty_cases_rejected_at_build() {
        // A zero-case `!match` isn't a legal shape — it collapses to
        // just the default, at which point `!if_else` isn't needed.
        // The check lives on the build side (not the load side) because
        // the typed enum accepts an empty `Vec<MatchCase>` — we can't
        // encode a non-empty vec constraint at serde level without
        // custom deserialize.
        let yaml = r#"
            !match
            on: close
            cases: []
            default: !value 0.0
        "#;
        let spec: ExprSpec = serde_norway::from_str(yaml).unwrap();
        let _ = spec.build(&Position::new(), &Book::new(1.0), None, &Schema::empty());
    }

    #[test]
    #[should_panic(expected = "same type")]
    fn match_mixed_pattern_types_rejected_at_build() {
        // A `!match` with one numeric and one string case can't map to
        // a single `K` on the library-level `Match<S, T, K>` — rejected
        // at build with a clear message.
        let yaml = r#"
            !match
            on: close
            cases:
              - when: 42.0
                value: !value 1.0
              - when: mixed_string
                value: !value 2.0
            default: !value 0.0
        "#;
        let spec: ExprSpec = serde_norway::from_str(yaml).unwrap();
        let _ = spec.build(&Position::new(), &Book::new(1.0), None, &Schema::empty());
    }

    #[test]
    fn match_first_matching_case_wins() {
        // Ordering matters — cases are checked in list order; a later
        // case that also matches doesn't fire.
        let yaml = r#"
            !match
            on: close
            cases:
              - when: 100.0
                value: !value 1.0
              - when: 100.0
                value: !value 999.0
            default: !value 0.0
        "#;
        let spec: ExprSpec = serde_norway::from_str(yaml).unwrap();
        let mut built = spec.build(&Position::new(), &Book::new(1.0), None, &Schema::empty());
        assert_eq!(feed_real(&mut built, bar(100.0)), Some(1.0));
    }

    #[test]
    fn latch_ema_of_resample_matches_reference_htf_ema() {
        // The composition-order regression at the YAML surface: an EMA-3
        // running inside !resample, wrapped in !latch, agrees numerically
        // with Ema(Resample.close, 3) at every boundary.
        let spec: ExprSpec = serde_norway::from_str(
            "!latch { source: !resample { every: 4, inner: !ema { period: 3, source: close } } }",
        )
        .unwrap();
        let mut built = spec.build(&Position::new(), &Book::new(1.0), None, &Schema::empty());
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
