//! End-to-end: a signal built on top of overlays of each of the three types
//! (`Real`, `Bool`, `Str`) fires exactly on the bars it should. Uses the
//! library layer directly (not the YAML surface) so this test is independent
//! of the CLI parser.

use std::sync::Arc;

use fugazi::indicators::{Combine, GetBool, GetReal, GetStr, StrEqOp, Value, ValueStr};
use fugazi::prelude::*;
use fugazi::Snapshot;

/// Build the shared schema: one column of each type.
fn schema() -> Arc<Schema> {
    let mut b = Schema::builder();
    b.add_real("vol_20");
    b.add_bool("risk_on");
    b.add_str("regime");
    b.finish()
}

/// One test bar: a candle plus the three overlay values that make the row
/// interesting.
struct Fixture {
    close: Real,
    vol: Real,
    risk_on: bool,
    regime: &'static str,
}

fn atom(schema: &Arc<Schema>, f: &Fixture) -> Atom {
    let candle = Candle::new(f.close, f.close, f.close, f.close, 0.0);
    let overlays = OverlayInfo::new(
        schema.clone(),
        vec![
            OverlayValue::Real(f.vol),
            OverlayValue::Bool(f.risk_on),
            OverlayValue::Str(Arc::from(f.regime)),
        ],
    );
    Atom::with_overlays(candle, overlays)
}

#[test]
fn get_real_composes_into_a_numeric_signal() {
    // "vol_20 > 0.15" fires only when the Real overlay column exceeds the
    // threshold — the same shape a strategy would use for a regime filter.
    let schema = schema();
    let mut sig = GetReal::new(&schema, "vol_20").gt(Value::new(0.15));

    let atoms = [
        (Fixture { close: 100.0, vol: 0.10, risk_on: false, regime: "bull" }, false),
        (Fixture { close: 100.0, vol: 0.15, risk_on: false, regime: "bull" }, false), // == not >
        (Fixture { close: 100.0, vol: 0.20, risk_on: false, regime: "bull" }, true),
        (Fixture { close: 100.0, vol: 0.05, risk_on: false, regime: "bear" }, false),
    ];
    for (f, expected) in &atoms {
        assert_eq!(sig.update(atom(&schema, f)), Some(*expected), "vol={}", f.vol);
    }
}

#[test]
fn get_bool_is_a_direct_signal() {
    // A Bool overlay column reads as a signal directly — no `!str_eq true`
    // gymnastics.
    let schema = schema();
    let mut sig = GetBool::new(&schema, "risk_on");
    for &b in &[true, false, true, true, false] {
        let f = Fixture { close: 100.0, vol: 0.0, risk_on: b, regime: "bull" };
        assert_eq!(sig.update(atom(&schema, &f)), Some(b));
    }
}

#[test]
fn get_str_composes_with_str_eq_into_a_regime_signal() {
    // `regime == "bull"` — the canonical Str overlay pattern: a categorical
    // column consumed via string equality.
    let schema = schema();
    let mut sig = Combine::<GetStr, ValueStr<Atom>, StrEqOp>::new(
        GetStr::new(&schema, "regime"),
        ValueStr::new("bull"),
    );
    let cases = [("bull", true), ("bear", false), ("bull", true), ("crab", false)];
    for (regime, expected) in cases {
        let f = Fixture { close: 100.0, vol: 0.0, risk_on: false, regime };
        assert_eq!(sig.update(atom(&schema, &f)), Some(expected), "regime={regime}");
    }
}

#[test]
fn strategy_style_and_of_three_types_fires_only_on_full_agreement() {
    // The whole payoff: a signal that reads one overlay of each type and
    // fires when *all three* line up — the shape a strategy would compose
    // for an entry gate. Emulates
    //   !all [ !get { key: risk_on },
    //          !str_eq { lhs: !get { key: regime }, rhs: "bull" },
    //          !gt   { lhs: !get { key: vol_20 }, rhs: !value { 0.15 } } ]
    // at the library level.
    let schema = schema();
    let regime_bull = Combine::<GetStr, ValueStr<Atom>, StrEqOp>::new(
        GetStr::new(&schema, "regime"),
        ValueStr::new("bull"),
    );
    let vol_high = GetReal::new(&schema, "vol_20").gt(Value::new(0.15));
    let mut sig = GetBool::new(&schema, "risk_on")
        .and(regime_bull)
        .and(vol_high);

    let bars = [
        (Fixture { close: 100.0, vol: 0.20, risk_on: true, regime: "bull" }, true),
        (Fixture { close: 100.0, vol: 0.20, risk_on: false, regime: "bull" }, false),
        (Fixture { close: 100.0, vol: 0.20, risk_on: true, regime: "bear" }, false),
        (Fixture { close: 100.0, vol: 0.10, risk_on: true, regime: "bull" }, false),
        (Fixture { close: 100.0, vol: 0.16, risk_on: true, regime: "bull" }, true),
    ];
    for (f, expected) in &bars {
        assert_eq!(
            sig.update(atom(&schema, f)),
            Some(*expected),
            "risk_on={} regime={} vol={}",
            f.risk_on,
            f.regime,
            f.vol,
        );
    }
}

#[test]
fn signal_is_none_before_overlays_arrive() {
    // An atom without an OverlayInfo (or bound to a foreign schema) yields
    // None from every typed Get, which composes into a false-reading signal
    // via `is_true`. Confirms the safe-by-default readiness the library
    // documents.
    let schema = schema();
    let mut sig = GetReal::new(&schema, "vol_20").gt(Value::new(0.0));
    // Bare Candle → no overlays → None.
    let bare = Atom::new(Candle::new(100.0, 100.0, 100.0, 100.0, 0.0));
    assert_eq!(sig.update(bare), None);
    assert!(!sig.is_true());
}

/// End-to-end at the driver layer: hand the AND-of-three-types signal to
/// `fugazi::backtest::run` inside a `SingleAssetStrategy` and confirm the
/// fills book on exactly the bars the signal fires on. Proves the signal
/// composes through the strategy layer as expected — the trip that the
/// standalone signal tests don't cover.
///
/// The signal chain is snapshot-rooted through `Pick::<String>::new()`
/// (the empty-selector single-entry unpack), so the strategy's own atom is
/// projected out of every incoming size-1 snapshot before the overlay
/// readers see it.
#[test]
fn overlay_signal_drives_a_backtest_end_to_end() {
    use fugazi::indicators::Pick;
    use fugazi::strategies::SingleAssetStrategy;

    let schema = schema();
    // Every overlay leaf sits on top of `Pick::<String>::new()` — the same
    // single-entry unpack the source-generic Field/Calendar leaves use, so
    // GetReal/GetBool/GetStr become `Input = Snapshot<String>` instead of
    // `Input = Atom` and the whole signal chain composes into the
    // Snapshot-input strategy.
    let make_enter = || {
        let regime_bull = Combine::<_, _, StrEqOp>::new(
            GetStr::of(&schema, "regime", Pick::<String>::new()),
            ValueStr::<Snapshot<String>>::new("bull"),
        );
        let vol_high = GetReal::of(&schema, "vol_20", Pick::<String>::new())
            .gt(Value::new(0.15));
        GetBool::of(&schema, "risk_on", Pick::<String>::new())
            .and(regime_bull)
            .and(vol_high)
    };
    let make_exit = || {
        Combine::<_, _, StrEqOp>::new(
            GetStr::of(&schema, "regime", Pick::<String>::new()),
            ValueStr::<Snapshot<String>>::new("bear"),
        )
    };

    let symbol = "TEST".to_string();
    let mut strategy = SingleAssetStrategy::new(symbol.clone())
        .long_on(make_enter(), make_exit());

    let bars: Vec<Fixture> = vec![
        Fixture { close: 100.0, vol: 0.10, risk_on: true, regime: "bull" },
        Fixture { close: 105.0, vol: 0.20, risk_on: true, regime: "bull" },
        Fixture { close: 108.0, vol: 0.20, risk_on: true, regime: "bull" },
        Fixture { close: 112.0, vol: 0.20, risk_on: true, regime: "bear" },
        Fixture { close: 110.0, vol: 0.20, risk_on: true, regime: "bear" },
    ];

    // The strategy consumes `Snapshot<String>`; wrap each atom in a
    // symbol-tagged size-1 snapshot so `fugazi::backtest::run` prices the
    // wallet each bar.
    let snapshots: Vec<Snapshot<String>> = bars
        .iter()
        .map(|f| Snapshot::<String>::single(symbol.clone(), atom(&schema, f)))
        .collect();
    let mut wallet: PaperWallet<String> = PaperWallet::new(10_000.0);
    let report = fugazi::backtest::run(&mut strategy, &mut wallet, snapshots);

    // Expect two fills: a Buy (the entry) then a Sell (the regime-flip exit).
    assert_eq!(
        report.fills.len(),
        2,
        "expected one enter + one exit fill, got {:#?}",
        report.fills,
    );
    assert_eq!(report.fills[0].order.side, Side::Buy);
    assert_eq!(report.fills[1].order.side, Side::Sell);
    // Same absolute size — the entry is round-tripped exactly.
    let bought = report.fills[0].order.units;
    let sold = report.fills[1].order.units;
    assert!(
        (bought - sold).abs() < 1e-9,
        "buy ({bought}) and sell ({sold}) sizes should match",
    );
}
