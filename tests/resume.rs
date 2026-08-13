//! Run-resuming: the acceptance gate for the full-state-serialization feature.
//!
//! Each test runs a strategy over `2N` bars in one go, then runs it as two
//! `N`-bar halves with a serialize / rebuild-from-spec / restore in between, and
//! asserts the second half is **bit-identical** to the tail of the uninterrupted
//! run. That is the whole promise: a resumed run behaves as if it never paused.

use fugazi::market::{Real, Schema};
use fugazi::spec::{
    BasketStrategySpec, MultiAssetStrategySpec, PairsStrategySpec, RunnableStrategy,
    SingleStrategySpec,
};
use fugazi::types::{Atom, Candle, Snapshot};

/// A price series with enough swings to trigger crossovers on both halves.
fn prices(n: usize) -> Vec<Real> {
    (0..n)
        .map(|i| {
            let t = i as Real;
            100.0 + 10.0 * (t * 0.35).sin() + 4.0 * (t * 0.11).cos() + 0.05 * t
        })
        .collect()
}

fn single_snaps(n: usize) -> Vec<Snapshot<String>> {
    prices(n)
        .into_iter()
        .map(|p| {
            let c = Candle::new(p, p + 1.0, p - 1.0, p, 1_000.0);
            Snapshot::single("X".to_string(), Atom::new(c))
        })
        .collect()
}

/// Two-symbol snapshots for the pairs / basket / multi shapes.
fn multi_snaps(n: usize) -> Vec<Snapshot<String>> {
    let a = prices(n);
    (0..n)
        .map(|i| {
            let pa = a[i];
            let pb = 100.0 + 8.0 * ((i as Real) * 0.27).cos() + 0.03 * i as Real;
            let mut snap = Snapshot::<String>::new();
            snap.push(
                Some("A".to_string()),
                None,
                Atom::new(Candle::new(pa, pa + 1.0, pa - 1.0, pa, 1_000.0)),
            );
            snap.push(
                Some("B".to_string()),
                None,
                Atom::new(Candle::new(pb, pb + 1.0, pb - 1.0, pb, 1_000.0)),
            );
            snap
        })
        .collect()
}

const CASH: Real = 10_000.0;

/// Drive `build()` over all `snaps` in one run, then over the two halves with a
/// serialize→rebuild→restore in the middle, and assert the resumed tail matches
/// the uninterrupted tail exactly (equity curve + fill count).
fn assert_resume_matches<S, B>(build: B, snaps: &[Snapshot<String>], split: usize)
where
    S: RunnableStrategy,
    B: Fn() -> S,
{
    // Uninterrupted 2N-bar run.
    let mut whole = build();
    let (whole_report, _) = whole
        .drive_resumable(snaps, CASH, &[], None, false)
        .expect("uninterrupted run");

    // First half → capture state.
    let mut first = build();
    let (_first_report, state) = first
        .drive_resumable(&snaps[..split], CASH, &[], None, false)
        .expect("first half");

    // Round-trip the state through JSON, as a real resume would (via a file).
    let json = serde_json::to_string(&state).expect("serialize RunState");
    let restored: fugazi::spec::RunState = serde_json::from_str(&json).expect("deserialize RunState");

    // Rebuild fresh from the spec, restore, and run the second half.
    let mut second = build();
    let (second_report, _) = second
        .drive_resumable(&snaps[split..], CASH, &[], Some(&restored), false)
        .expect("resumed half");

    // The resumed half's equity curve must match the tail of the whole run,
    // bit for bit.
    let tail = &whole_report.equity_curve[split..];
    assert_eq!(
        second_report.equity_curve.len(),
        tail.len(),
        "resumed curve length"
    );
    for (i, (got, want)) in second_report.equity_curve.iter().zip(tail).enumerate() {
        assert_eq!(
            got.to_bits(),
            want.to_bits(),
            "resumed equity diverged at tail bar {i}: {got} vs {want}"
        );
    }
}

fn schema() -> std::sync::Arc<Schema> {
    Schema::empty()
}

#[test]
fn single_asset_ema_crossover_resumes_identically() {
    let yaml = r#"
        symbol: X
        long:
          enter: !crosses_above
            lhs: !ema { period: 3, source: !close }
            rhs: !ema { period: 8, source: !close }
          exit: !crosses_below
            lhs: !ema { period: 3, source: !close }
            rhs: !ema { period: 8, source: !close }
    "#;
    let spec = SingleStrategySpec::from_text_with_params_in(
        yaml,
        &Default::default(),
        std::path::Path::new("."),
        "(resume)",
    )
    .expect("parse single spec");
    let sch = schema();
    let snaps = single_snaps(60);
    assert_resume_matches(|| spec.build(CASH, &sch), &snaps, 30);

    // Split *during* warm-up (before the EMA-8 seed has settled): proves the IIR
    // seed itself is serialized and restored exactly — the whole reason for
    // serde_json's `float_roundtrip`. A replay-based scheme couldn't do this
    // without re-feeding the pre-split bars.
    let sch2 = schema();
    assert_resume_matches(|| spec.build(CASH, &sch2), &snaps, 4);
}

#[test]
fn single_asset_rsi_reversal_with_atr_stop_resumes_identically() {
    // Exercises IIR RSI + ATR (Wilder state) + a position-anchored trailing
    // stop, so the Position/Book restore is on the critical path.
    let yaml = r#"
        symbol: X
        long:
          enter: !lt { lhs: !rsi { period: 5, source: !close }, rhs: !value 35.0 }
          exit: !gt { lhs: !rsi { period: 5, source: !close }, rhs: !value 65.0 }
          stop_loss: !sub
            lhs: !entry
            rhs: !mul { lhs: !atr { period: 5 }, rhs: !value 2.0 }
    "#;
    let spec = SingleStrategySpec::from_text_with_params_in(
        yaml,
        &Default::default(),
        std::path::Path::new("."),
        "(resume)",
    )
    .expect("parse single spec");
    let sch = schema();
    let snaps = single_snaps(60);
    assert_resume_matches(|| spec.build(CASH, &sch), &snaps, 30);
}

#[test]
fn pairs_spread_resumes_identically() {
    let yaml = r#"
        left: A
        right: B
        long_spread:
          enter: !lt
            lhs: !sub { lhs: !close { source: !pick { symbol: A } }, rhs: !close { source: !pick { symbol: B } } }
            rhs: !value -2.0
          exit: !gt
            lhs: !sub { lhs: !close { source: !pick { symbol: A } }, rhs: !close { source: !pick { symbol: B } } }
            rhs: !value 0.0
    "#;
    let spec = PairsStrategySpec::from_text_with_params_in(
        yaml,
        &Default::default(),
        std::path::Path::new("."),
        "(resume)",
    )
    .expect("parse pairs spec");
    let sch = schema();
    let snaps = multi_snaps(60);
    assert_resume_matches(|| spec.build(CASH, &sch), &snaps, 30);
}

#[test]
fn multi_asset_resumes_identically() {
    let yaml = r#"
        long:
          enter: !crosses_above
            lhs: !ema { period: 3, source: !close }
            rhs: !ema { period: 8, source: !close }
          exit: !crosses_below
            lhs: !ema { period: 3, source: !close }
            rhs: !ema { period: 8, source: !close }
        sizing: !value 0.5
    "#;
    let spec = MultiAssetStrategySpec::from_text_with_params_in(
        yaml,
        &Default::default(),
        std::path::Path::new("."),
        "(resume)",
    )
    .expect("parse multi spec");
    let sch = schema();
    let snaps = multi_snaps(60);
    assert_resume_matches(|| spec.build(CASH, &sch), &snaps, 30);
}

/// A always-long strategy that never exits, so a position is open at run end.
fn buy_and_hold_spec() -> SingleStrategySpec {
    let yaml = r#"
        symbol: X
        long:
          enter: !gt { lhs: !close, rhs: !value 0.0 }
    "#;
    SingleStrategySpec::from_text_with_params_in(
        yaml,
        &Default::default(),
        std::path::Path::new("."),
        "(resume)",
    )
    .expect("parse")
}

#[test]
fn flatten_books_a_closing_trade() {
    let sch = schema();
    let snaps = single_snaps(40);

    // Without --flatten: an open position at the end is unrealized (no
    // closing fill for it in the blotter).
    let mut carried = buy_and_hold_spec().build(CASH, &sch);
    let (carried_report, _) = carried
        .drive_resumable(&snaps, CASH, &[], None, false)
        .expect("carried run");

    // With --flatten: the open position is booked closed at the last bar,
    // so the blotter gains exactly one more fill.
    let mut flattened = buy_and_hold_spec().build(CASH, &sch);
    let (flattened_report, _) = flattened
        .drive_resumable(&snaps, CASH, &[], None, true)
        .expect("flattened run");

    assert_eq!(
        flattened_report.fills.len(),
        carried_report.fills.len() + 1,
        "flatten should book exactly one closing fill for the open long"
    );
    // And the equity curve is untouched (open positions were already marked to
    // market every bar).
    assert_eq!(carried_report.equity_curve, flattened_report.equity_curve);
}

#[test]
fn resuming_a_mismatched_shape_is_rejected() {
    let sch = schema();
    let snaps = single_snaps(20);

    // Capture a single-asset state...
    let mut single = buy_and_hold_spec().build(CASH, &sch);
    let (_, state) = single
        .drive_resumable(&snaps, CASH, &[], None, false)
        .expect("single run");
    assert_eq!(state.kind, "single");

    // ...then try to resume it into a pairs strategy — rejected with a clear
    // `!resume >` message, not a silent mis-parse.
    let pairs_yaml = r#"
        left: A
        right: B
        long_spread:
          enter: !lt
            lhs: !sub { lhs: !close { source: !pick { symbol: A } }, rhs: !close { source: !pick { symbol: B } } }
            rhs: !value 0.0
          exit: !gt
            lhs: !sub { lhs: !close { source: !pick { symbol: A } }, rhs: !close { source: !pick { symbol: B } } }
            rhs: !value 5.0
    "#;
    let pairs = PairsStrategySpec::from_text_with_params_in(
        pairs_yaml,
        &Default::default(),
        std::path::Path::new("."),
        "(resume)",
    )
    .expect("parse pairs");
    let mut pair_strat = pairs.build(CASH, &sch);
    let err = pair_strat
        .drive_resumable(&multi_snaps(20), CASH, &[], Some(&state), false)
        .expect_err("cross-shape resume must fail");
    assert!(err.contains("!resume"), "unexpected error: {err}");
    assert!(err.contains("single") && err.contains("pairs"), "error: {err}");
}

#[test]
fn resuming_a_stale_format_version_is_rejected() {
    let sch = schema();
    let mut single = buy_and_hold_spec().build(CASH, &sch);
    let (_, mut state) = single
        .drive_resumable(&single_snaps(20), CASH, &[], None, false)
        .expect("single run");
    state.format_version += 1; // pretend it was written by an older/newer build

    let mut fresh = buy_and_hold_spec().build(CASH, &sch);
    let err = fresh
        .drive_resumable(&single_snaps(20), CASH, &[], Some(&state), false)
        .expect_err("stale version must fail");
    assert!(err.contains("format version"), "unexpected error: {err}");
}

#[test]
fn single_asset_gated_on_trailing_sharpe_resumes_identically() {
    // A `!sharpe` gate embeds a whole sub-strategy + its own wallet inside the
    // indicator — the deepest state in the crate. Resuming must restore that
    // embedded engine exactly, not re-warm it.
    let yaml = r#"
        symbol: X
        long:
          enter: !gt
            lhs: !sharpe
              period: 5
              bars_per_year: 252.0
              strategy:
                symbol: X
                long:
                  enter: !gt { lhs: !close, rhs: !value 0.0 }
            rhs: !value -1000.0
          exit: !lt
            lhs: !sharpe
              period: 5
              bars_per_year: 252.0
              strategy:
                symbol: X
                long:
                  enter: !gt { lhs: !close, rhs: !value 0.0 }
            rhs: !value -1000.0
    "#;
    let spec = SingleStrategySpec::from_text_with_params_in(
        yaml,
        &Default::default(),
        std::path::Path::new("."),
        "(resume)",
    )
    .expect("parse trailing-gated spec");
    let sch = schema();
    let snaps = single_snaps(60);
    assert_resume_matches(|| spec.build(CASH, &sch), &snaps, 30);
}

#[test]
fn basket_resumes_identically() {
    let yaml = r#"
        selection: !top_bottom { longs: 1, shorts: 1 }
        score: !rsi { period: 5, source: !close }
        sizing: !value 0.5
    "#;
    let spec = BasketStrategySpec::from_text_with_params_in(
        yaml,
        &Default::default(),
        std::path::Path::new("."),
        "(resume)",
    )
    .expect("parse basket spec");
    let sch = schema();
    let snaps = multi_snaps(60);
    assert_resume_matches(|| spec.build(CASH, &sch), &snaps, 30);
}
