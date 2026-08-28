//! Run-resuming: the acceptance gate for the full-state-serialization feature.
//!
//! Each test runs a strategy over `N` bars in one go, then runs it as a
//! sequence of chunks with a serialize / rebuild-from-spec / restore between
//! each pair, and asserts the chunked run is **bit-identical** to the
//! uninterrupted one. That is the whole promise: a resumed run behaves as if it
//! never paused.
//!
//! Two chunks is not enough. A two-way split exercises save → restore, but
//! never restore → *re*-save, so any state that a resumed strategy fails to
//! carry forward into its own next snapshot is invisible. Every test here
//! therefore cuts at three or more chunks; [`assert_chunked_resume_matches`]
//! takes an arbitrary list of split points.
//!
//! The series generators stay local (see `tests/common/bars.rs`'s module doc):
//! these assertions depend on exactly which crossovers the price path fires.
//! Only the *bar shape* is shared, so the streams carry timestamps and
//! `RunState::last_bar` is on the critical path.

mod common;

use common::bars;
use fugazi::backtest::Fill;
use fugazi::market::{Real, Schema};
use fugazi::spec::{
    BasketStrategySpec, MultiAssetStrategySpec, PairsStrategySpec, PortfolioSpec, RunState,
    RunnableStrategy, SingleStrategySpec,
};
use fugazi::types::{Atom, Snapshot};
use fugazi::types::{Symbol, symbol as intern};

/// A price series with enough swings to trigger crossovers in every chunk.
fn prices(n: usize) -> Vec<Real> {
    (0..n)
        .map(|i| {
            let t = i as Real;
            100.0 + 10.0 * (t * 0.35).sin() + 4.0 * (t * 0.11).cos() + 0.05 * t
        })
        .collect()
}

/// The B-leg: a slower, out-of-phase path so the two symbols disagree about
/// direction (which is what makes a basket's cross-sectional pick non-trivial).
fn prices_b(n: usize) -> Vec<Real> {
    (0..n)
        .map(|i| 100.0 + 8.0 * ((i as Real) * 0.27).cos() + 0.03 * i as Real)
        .collect()
}

fn single_snaps(n: usize) -> Vec<Snapshot<Symbol>> {
    bars::daily_series(&[("X", &prices(n))], bars::banded)
}

/// Two-symbol snapshots for the pairs / basket / multi / portfolio shapes.
fn multi_snaps(n: usize) -> Vec<Snapshot<Symbol>> {
    bars::daily_series(&[("A", &prices(n)), ("B", &prices_b(n))], bars::banded)
}

/// [`multi_snaps`] with `B` absent for `gap` — a listing gap, a delisting, a
/// feed hiccup, or simply a name that doesn't quote every bar.
///
/// Hand-built rather than via `bars::daily_series`, which panics on ragged
/// columns on purpose: *which* bar is missing is the thing being asserted.
fn multi_snaps_with_gap(n: usize, gap: std::ops::Range<usize>) -> Vec<Snapshot<Symbol>> {
    let (a, b) = (prices(n), prices_b(n));
    (0..n)
        .map(|i| {
            let t = fugazi::types::Timestamp(i as i64 * bars::DAY_MS);
            let mut snap = Snapshot::<Symbol>::new();
            snap.push(
                Some(intern("A")),
                None,
                Atom::with_time(bars::banded(a[i]), t),
            );
            if !gap.contains(&i) {
                snap.push(
                    Some(intern("B")),
                    None,
                    Atom::with_time(bars::banded(b[i]), t),
                );
            }
            snap
        })
        .collect()
}

const CASH: Real = 10_000.0;

fn schema() -> std::sync::Arc<Schema> {
    Schema::empty()
}

// ---------------------------------------------------------------------------
// The chunked-resume harness
// ---------------------------------------------------------------------------

/// What one chunked run produced, alongside the uninterrupted run it must
/// match. Built once by [`chunked_run`] and consumed by the two assertions
/// below, which check different properties of the same evidence.
struct Chunked {
    whole_equity: Vec<Real>,
    whole_fills: Vec<Fill<Symbol>>,
    whole_state: RunState,
    /// Every chunk's equity points, concatenated in bar order.
    chunk_equity: Vec<Real>,
    /// Every chunk's fills, rebased onto whole-run bar indices.
    chunk_fills: Vec<Fill<Symbol>>,
    /// The state captured after the final chunk.
    final_state: RunState,
}

/// Drive `build()` over all `snaps` in one run, then over the chunks cut at
/// `splits`, serializing → JSON → rebuilding from spec → restoring between each
/// pair. A real resume goes through a file, so the JSON round-trip is part of
/// the path under test, not a convenience.
fn chunked_run<S, B>(build: B, snaps: &[Snapshot<Symbol>], splits: &[usize]) -> Chunked
where
    S: RunnableStrategy,
    B: Fn() -> S,
{
    assert!(
        splits.windows(2).all(|w| w[0] < w[1])
            && splits.first().is_some_and(|&s| s > 0)
            && splits.last().is_some_and(|&s| s < snaps.len()),
        "splits must be strictly increasing within 1..{}: {splits:?}",
        snaps.len()
    );

    let mut whole = build();
    let (whole_report, whole_state) = whole
        .drive_resumable(snaps, CASH, &[], None, false)
        .expect("uninterrupted run");

    let bounds: Vec<usize> = std::iter::once(0)
        .chain(splits.iter().copied())
        .chain(std::iter::once(snaps.len()))
        .collect();

    let mut carried: Option<RunState> = None;
    let mut chunk_equity = Vec::new();
    let mut chunk_fills = Vec::new();
    for (chunk, window) in bounds.windows(2).enumerate() {
        let (start, end) = (window[0], window[1]);
        // Rebuild from the spec every chunk: a resume never inherits a live
        // object, only a document plus a state blob.
        let mut strat = build();
        let (report, state) = strat
            .drive_resumable(&snaps[start..end], CASH, &[], carried.as_ref(), false)
            .unwrap_or_else(|e| panic!("chunk {chunk} ({start}..{end}) failed: {e}"));

        chunk_equity.extend_from_slice(&report.equity_curve);
        chunk_fills.extend(report.fills.into_iter().map(|f| Fill {
            bar: f.bar + start,
            order: f.order,
        }));

        let json = serde_json::to_string(&state).expect("serialize RunState");
        carried = Some(serde_json::from_str(&json).expect("deserialize RunState"));
    }

    Chunked {
        whole_equity: whole_report.equity_curve,
        whole_fills: whole_report.fills,
        whole_state,
        chunk_equity,
        chunk_fills,
        final_state: carried.expect("at least one chunk"),
    }
}

/// A fill's identity for comparison purposes — everything except `OrderId`.
///
/// Ids are deliberately excluded: `BasketStrategy::trade` iterates a `HashMap`,
/// so the order in which a bar's submissions mint ids varies between two map
/// instances even within one process. That reorders id *assignment* without
/// changing a single fill's economics, which is exactly what this key captures.
fn fill_key(f: &Fill<Symbol>) -> (usize, String, String, u64, u64, String, u64) {
    (
        f.bar,
        f.order.symbol.to_string(),
        format!("{:?}", f.order.side),
        f.order.units.to_bits(),
        f.order.price.to_bits(),
        format!("{:?}", f.order.kind),
        f.order.commission.to_bits(),
    )
}

/// The headline property: a run cut into chunks, with a serialize/restore at
/// every seam, is indistinguishable from the uninterrupted run.
///
/// `case` names the shape so a failure identifies which one diverged without
/// the reader having to map a line number back to a spec.
#[track_caller]
fn assert_chunked_resume_matches<S, B>(
    case: &str,
    build: B,
    snaps: &[Snapshot<Symbol>],
    splits: &[usize],
) where
    S: RunnableStrategy,
    B: Fn() -> S,
{
    let run = chunked_run(build, snaps, splits);

    assert_eq!(
        run.chunk_equity.len(),
        run.whole_equity.len(),
        "{case}: chunked curve length"
    );
    for (i, (got, want)) in run.chunk_equity.iter().zip(&run.whole_equity).enumerate() {
        assert_eq!(
            got.to_bits(),
            want.to_bits(),
            "{case}: equity diverged at bar {i} (splits {splits:?}): {got} vs {want}"
        );
    }

    let got: Vec<_> = run.chunk_fills.iter().map(fill_key).collect();
    let want: Vec<_> = run.whole_fills.iter().map(fill_key).collect();
    assert_eq!(
        got,
        want,
        "{case}: fills diverged (splits {splits:?}); chunked {} vs whole {}",
        got.len(),
        want.len()
    );

    assert_eq!(
        run.final_state.bars_seen,
        snaps.len(),
        "{case}: bars_seen must accumulate across resumes"
    );
    assert_eq!(
        run.final_state.bars_seen, run.whole_state.bars_seen,
        "{case}: bars_seen vs uninterrupted run"
    );
    assert_eq!(
        run.final_state.last_bar, run.whole_state.last_bar,
        "{case}: last_bar vs uninterrupted run"
    );
    assert_eq!(run.final_state.kind, run.whole_state.kind, "{case}: kind");
}

/// The diagnostic twin: the state a chunked run *ends up holding* must equal
/// the state the uninterrupted run holds.
///
/// Stronger than [`assert_chunked_resume_matches`] and deliberately separate.
/// State that is silently dropped on the way through a resume often doesn't
/// move the curve until some later bar — or at all, on this price path — so a
/// curve assertion alone reports "fine" for a strategy that has quietly lost a
/// symbol's chains or a child's indicators. This one fails at the drop.
///
/// Compares `strategy` only, never `wallet`: the wallet blob carries `next_id`
/// and a blotter whose ordering is subject to the same `HashMap` caveat as
/// [`fill_key`].
#[track_caller]
fn assert_chunked_state_matches<S, B>(
    case: &str,
    build: B,
    snaps: &[Snapshot<Symbol>],
    splits: &[usize],
) where
    S: RunnableStrategy,
    B: Fn() -> S,
{
    let run = chunked_run(build, snaps, splits);
    assert_eq!(
        run.final_state.strategy, run.whole_state.strategy,
        "{case}: resumed strategy state differs from the uninterrupted run's (splits {splits:?})"
    );
}

// ---------------------------------------------------------------------------
// Spec fixtures
// ---------------------------------------------------------------------------

macro_rules! parse {
    ($ty:ty, $yaml:expr) => {
        <$ty>::from_text_with_params_in(
            $yaml,
            &Default::default(),
            std::path::Path::new("."),
            std::path::Path::new("."),
            "(resume)",
        )
        .expect(concat!("parse ", stringify!($ty)))
    };
}

fn single_ema_spec() -> SingleStrategySpec {
    parse!(
        SingleStrategySpec,
        r#"
        root: X
        long:
          enter: !crosses_above
            lhs: !ema { period: 3, source: !close }
            rhs: !ema { period: 8, source: !close }
          exit: !crosses_below
            lhs: !ema { period: 3, source: !close }
            rhs: !ema { period: 8, source: !close }
    "#
    )
}

fn pairs_spec() -> PairsStrategySpec {
    parse!(
        PairsStrategySpec,
        r#"
        left: A
        right: B
        long_spread:
          enter: !lt
            lhs: !sub { lhs: !close { source: !pick { symbol: A } }, rhs: !close { source: !pick { symbol: B } } }
            rhs: !value -2.0
          exit: !gt
            lhs: !sub { lhs: !close { source: !pick { symbol: A } }, rhs: !close { source: !pick { symbol: B } } }
            rhs: !value 0.0
    "#
    )
}

fn multi_spec() -> MultiAssetStrategySpec {
    parse!(
        MultiAssetStrategySpec,
        r#"
        long:
          enter: !crosses_above
            lhs: !ema { period: 3, source: !close }
            rhs: !ema { period: 8, source: !close }
          exit: !crosses_below
            lhs: !ema { period: 3, source: !close }
            rhs: !ema { period: 8, source: !close }
        sizing: !value 0.5
    "#
    )
}

/// [`multi_spec`] with an explicit rebalance cadence.
///
/// `Every` carries a bar counter, so its *phase* is state. Cut points at 20/40
/// are deliberately not multiples of 5: a gate that restarts its count each
/// chunk fires on different bars than one that carries.
fn multi_rebalancing_spec() -> MultiAssetStrategySpec {
    parse!(
        MultiAssetStrategySpec,
        r#"
        long:
          enter: !crosses_above
            lhs: !ema { period: 3, source: !close }
            rhs: !ema { period: 8, source: !close }
          exit: !crosses_below
            lhs: !ema { period: 3, source: !close }
            rhs: !ema { period: 8, source: !close }
        sizing: !value 0.5
        rebalance_on: !every 7
    "#
    )
}

fn basket_spec() -> BasketStrategySpec {
    parse!(
        BasketStrategySpec,
        r#"
        selection: !top_bottom { longs: 1, shorts: 1 }
        score: !rsi { period: 5, source: !close }
        sizing: !value 0.5
    "#
    )
}

/// Two children over the two symbols, with *different* indicators per child so
/// a child-state restore that mixes up which state belongs to whom produces a
/// wrong answer rather than a coincidentally-right one. `rebalance_on: !every 7`
/// puts the gate's phase on the critical path.
fn portfolio_spec() -> PortfolioSpec {
    parse!(
        PortfolioSpec,
        r#"
        weights: !value [0.6, 0.4]
        rebalance_on: !every 7
        children:
          - name: fast_a
            strategy:
              root: A
              long:
                enter: !crosses_above
                  lhs: !ema { period: 3, source: !close }
                  rhs: !ema { period: 8, source: !close }
                exit: !crosses_below
                  lhs: !ema { period: 3, source: !close }
                  rhs: !ema { period: 8, source: !close }
          - name: slow_b
            strategy:
              root: B
              long:
                enter: !lt { lhs: !rsi { period: 5, source: !close }, rhs: !value 35.0 }
                exit: !gt { lhs: !rsi { period: 5, source: !close }, rhs: !value 65.0 }
    "#
    )
}

/// [`portfolio_spec`] with a per-child weight *expression* rather than a
/// constant list, so the `share_indicators` chains carry state that must
/// survive a resume.
fn portfolio_weight_shares_spec() -> PortfolioSpec {
    parse!(
        PortfolioSpec,
        r#"
        weights: !drawdown_throttle { source: !portfolio_book, max_drawdown: 0.15 }
        rebalance_on: !every 7
        children:
          - name: fast_a
            strategy:
              root: A
              long:
                enter: !crosses_above
                  lhs: !ema { period: 3, source: !close }
                  rhs: !ema { period: 8, source: !close }
                exit: !crosses_below
                  lhs: !ema { period: 3, source: !close }
                  rhs: !ema { period: 8, source: !close }
          - name: slow_b
            strategy:
              root: B
              long:
                enter: !lt { lhs: !rsi { period: 5, source: !close }, rhs: !value 35.0 }
                exit: !gt { lhs: !rsi { period: 5, source: !close }, rhs: !value 65.0 }
    "#
    )
}

/// An always-long strategy that never exits, so a position is open at run end.
fn buy_and_hold_spec() -> SingleStrategySpec {
    parse!(
        SingleStrategySpec,
        r#"
        root: X
        long:
          enter: !gt { lhs: !close, rhs: !value 0.0 }
    "#
    )
}

/// The split points every shape is held to. Three chunks, so the middle one
/// both restores and re-saves.
const SPLITS: &[usize] = &[20, 40];

// ---------------------------------------------------------------------------
// Per shape: chunked resume == one shot
// ---------------------------------------------------------------------------

#[test]
fn single_asset_ema_crossover_resumes_across_three_chunks() {
    let spec = single_ema_spec();
    let sch = schema();
    let snaps = single_snaps(60);
    assert_chunked_resume_matches("single/ema", || spec.build(CASH, &sch), &snaps, SPLITS);

    // Cut *during* warm-up (before the EMA-8 seed has settled): proves the IIR
    // seed itself is serialized and restored exactly — the whole reason for
    // serde_json's `float_roundtrip`. A replay-based scheme couldn't do this
    // without re-feeding the pre-split bars.
    assert_chunked_resume_matches(
        "single/ema mid-warm-up",
        || spec.build(CASH, &sch),
        &snaps,
        &[2, 4, 7],
    );
}

#[test]
fn single_asset_rsi_reversal_with_atr_stop_resumes_across_three_chunks() {
    // Exercises IIR RSI + ATR (Wilder state) + a position-anchored trailing
    // stop, so the Position/Book restore is on the critical path.
    let spec = parse!(
        SingleStrategySpec,
        r#"
        root: X
        long:
          enter: !lt { lhs: !rsi { period: 5, source: !close }, rhs: !value 35.0 }
          exit: !gt { lhs: !rsi { period: 5, source: !close }, rhs: !value 65.0 }
          stop_loss: !sub
            lhs: !entry
            rhs: !mul { lhs: !atr { period: 5 }, rhs: !value 2.0 }
    "#
    );
    let sch = schema();
    assert_chunked_resume_matches(
        "single/rsi+atr-stop",
        || spec.build(CASH, &sch),
        &single_snaps(60),
        SPLITS,
    );
}

#[test]
fn single_asset_gated_on_trailing_sharpe_resumes_across_three_chunks() {
    // A `!sharpe` gate embeds a whole sub-strategy + its own wallet inside the
    // indicator — the deepest state in the crate. Resuming must restore that
    // embedded engine exactly, not re-warm it.
    let spec = parse!(
        SingleStrategySpec,
        r#"
        root: X
        long:
          enter: !gt
            lhs: !sharpe
              period: 5
              bars_per_year: 252.0
              strategy:
                root: X
                long:
                  enter: !gt { lhs: !close, rhs: !value 0.0 }
            rhs: !value -1000.0
          exit: !lt
            lhs: !sharpe
              period: 5
              bars_per_year: 252.0
              strategy:
                root: X
                long:
                  enter: !gt { lhs: !close, rhs: !value 0.0 }
            rhs: !value -1000.0
    "#
    );
    let sch = schema();
    assert_chunked_resume_matches(
        "single/trailing-sharpe",
        || spec.build(CASH, &sch),
        &single_snaps(60),
        SPLITS,
    );
}

#[test]
fn pairs_spread_resumes_across_three_chunks() {
    let spec = pairs_spec();
    let sch = schema();
    assert_chunked_resume_matches(
        "pairs/spread",
        || spec.build(CASH, &sch),
        &multi_snaps(60),
        SPLITS,
    );
}

#[test]
fn multi_asset_resumes_across_three_chunks() {
    let spec = multi_spec();
    let sch = schema();
    assert_chunked_resume_matches(
        "multi/ema",
        || spec.build(CASH, &sch),
        &multi_snaps(60),
        SPLITS,
    );
}

#[test]
fn multi_asset_with_a_rebalance_cadence_resumes_across_three_chunks() {
    // The default gate (`!never`) is stateless, so the plain multi test above
    // cannot see a dropped gate. `rebalance_on:` is on the multi spec surface,
    // and `Every`'s bar counter is state like any other.
    let spec = multi_rebalancing_spec();
    let sch = schema();
    assert_chunked_resume_matches(
        "multi/rebalancing",
        || spec.build(CASH, &sch),
        &multi_snaps(60),
        SPLITS,
    );
}

#[test]
fn multi_asset_resumes_a_symbol_absent_from_a_middle_chunk() {
    // B doesn't quote for the whole middle chunk. Its state must survive that
    // chunk's save — a resumed strategy only rediscovers the symbols it
    // actually sees, so anything it doesn't see it has to carry forward
    // untouched rather than drop.
    let spec = multi_spec();
    let sch = schema();
    assert_chunked_resume_matches(
        "multi/listing-gap",
        || spec.build(CASH, &sch),
        &multi_snaps_with_gap(60, 20..40),
        SPLITS,
    );
}

#[test]
fn basket_resumes_across_three_chunks() {
    let spec = basket_spec();
    let sch = schema();
    assert_chunked_resume_matches(
        "basket/top-bottom",
        || spec.build(CASH, &sch),
        &multi_snaps(60),
        SPLITS,
    );
}

#[test]
fn basket_resumes_a_symbol_absent_from_a_middle_chunk() {
    let spec = basket_spec();
    let sch = schema();
    assert_chunked_resume_matches(
        "basket/listing-gap",
        || spec.build(CASH, &sch),
        &multi_snaps_with_gap(60, 20..40),
        SPLITS,
    );
}

#[test]
fn portfolio_resumes_across_three_chunks() {
    let spec = portfolio_spec();
    let sch = schema();
    assert_chunked_resume_matches(
        "portfolio/fixed-weights",
        || spec.build(CASH, &sch, None),
        &multi_snaps(60),
        SPLITS,
    );
}

#[test]
fn portfolio_with_weight_shares_resumes_across_three_chunks() {
    let spec = portfolio_weight_shares_spec();
    let sch = schema();
    assert_chunked_resume_matches(
        "portfolio/weight-shares",
        || spec.build(CASH, &sch, None),
        &multi_snaps(60),
        SPLITS,
    );
}

// ---------------------------------------------------------------------------
// Per shape: the state itself round-trips, not just the numbers it produced
// ---------------------------------------------------------------------------

#[test]
fn single_asset_carries_every_state_field_across_chunks() {
    let spec = single_ema_spec();
    let sch = schema();
    assert_chunked_state_matches(
        "single/ema",
        || spec.build(CASH, &sch),
        &single_snaps(60),
        SPLITS,
    );
}

#[test]
fn pairs_carries_every_state_field_across_chunks() {
    let spec = pairs_spec();
    let sch = schema();
    assert_chunked_state_matches(
        "pairs/spread",
        || spec.build(CASH, &sch),
        &multi_snaps(60),
        SPLITS,
    );
}

#[test]
fn multi_asset_carries_every_state_field_across_chunks() {
    let spec = multi_rebalancing_spec();
    let sch = schema();
    assert_chunked_state_matches(
        "multi/rebalancing",
        || spec.build(CASH, &sch),
        &multi_snaps(60),
        SPLITS,
    );
}

#[test]
fn basket_carries_every_state_field_across_chunks() {
    let spec = basket_spec();
    let sch = schema();
    assert_chunked_state_matches(
        "basket/top-bottom",
        || spec.build(CASH, &sch),
        &multi_snaps(60),
        SPLITS,
    );
}

#[test]
fn portfolio_carries_every_state_field_across_chunks() {
    let spec = portfolio_spec();
    let sch = schema();
    assert_chunked_state_matches(
        "portfolio/fixed-weights",
        || spec.build(CASH, &sch, None),
        &multi_snaps(60),
        SPLITS,
    );
}

// ---------------------------------------------------------------------------
// Flatten, and the two rejection paths
// ---------------------------------------------------------------------------

#[test]
fn flatten_closes_the_position_in_the_wallet_not_just_the_report() {
    let sch = schema();
    let snaps = single_snaps(40);

    // Without --flatten: an open position at the end is unrealized (no
    // closing fill for it in the blotter).
    let mut carried = buy_and_hold_spec().build(CASH, &sch);
    let (carried_report, carried_state) = carried
        .drive_resumable(&snaps, CASH, &[], None, false)
        .expect("carried run");

    // With --flatten: the open position is closed at the last bar, so the
    // blotter gains exactly one more fill...
    let mut flattened = buy_and_hold_spec().build(CASH, &sch);
    let (flattened_report, flattened_state) = flattened
        .drive_resumable(&snaps, CASH, &[], None, true)
        .expect("flattened run");

    assert_eq!(
        flattened_report.fills.len(),
        carried_report.fills.len() + 1,
        "flatten should book exactly one closing fill for the open long"
    );

    // ...and, unlike the carried run, the wallet it leaves behind is flat.
    // This is the property the whole feature turns on: a paused deployment
    // that flattens must not resume holding the position it just closed.
    let carried_positions = wallet_positions(&carried_state);
    assert!(
        carried_positions
            .iter()
            .any(|(_, units)| units.abs() > 1e-12),
        "the carried run should still hold its long: {carried_positions:?}"
    );
    let flattened_positions = wallet_positions(&flattened_state);
    assert!(
        flattened_positions
            .iter()
            .all(|(_, units)| units.abs() <= 1e-12),
        "a flattened run's state must hold no position: {flattened_positions:?}"
    );

    // The curve agrees everywhere but the final bar, which absorbs the
    // closing leg's realized cost. (With a zero-cost wallet the two are equal;
    // the invariant that matters here is the length, which every report
    // consumer assumes is one point per bar.)
    assert_eq!(
        flattened_report.equity_curve.len(),
        snaps.len(),
        "flatten must not change the number of equity points"
    );
    assert_eq!(
        carried_report.equity_curve[..snaps.len() - 1],
        flattened_report.equity_curve[..snaps.len() - 1],
        "flatten must only touch the final equity point"
    );
}

#[test]
fn resuming_a_flattened_run_continues_from_a_flat_book() {
    let sch = schema();
    let snaps = single_snaps(40);

    let mut flattened = buy_and_hold_spec().build(CASH, &sch);
    let (_, state) = flattened
        .drive_resumable(&snaps[..20], CASH, &[], None, true)
        .expect("flattened first chunk");

    // A resume from a flattened state starts flat, so it must re-enter — i.e.
    // book an opening fill — rather than silently continuing to hold.
    let mut resumed = buy_and_hold_spec().build(CASH, &sch);
    let (report, _) = resumed
        .drive_resumable(&snaps[20..], CASH, &[], Some(&state), false)
        .expect("resumed chunk");
    assert!(
        !report.fills.is_empty(),
        "resuming from a flattened state should re-enter, but booked no fills"
    );
}

/// The `positions` map out of a serialized `PaperWallet` snapshot.
fn wallet_positions(state: &RunState) -> Vec<(String, Real)> {
    state
        .wallet
        .get("positions")
        .and_then(|p| p.as_object())
        .map(|m| {
            m.iter()
                .map(|(k, v)| (k.clone(), v.as_f64().unwrap_or(0.0)))
                .collect()
        })
        .unwrap_or_default()
}

#[test]
fn resuming_a_mismatched_shape_is_rejected() {
    let sch = schema();

    // Capture a single-asset state...
    let mut single = buy_and_hold_spec().build(CASH, &sch);
    let (_, state) = single
        .drive_resumable(&single_snaps(20), CASH, &[], None, false)
        .expect("single run");
    assert_eq!(state.kind, "single");

    // ...then try to resume it into a pairs strategy — rejected with a clear
    // `!resume >` message, not a silent mis-parse.
    let mut pair_strat = pairs_spec().build(CASH, &sch);
    let err = pair_strat
        .drive_resumable(&multi_snaps(20), CASH, &[], Some(&state), false)
        .expect_err("cross-shape resume must fail");
    assert!(err.contains("!resume"), "unexpected error: {err}");
    assert!(
        err.contains("single") && err.contains("pairs"),
        "error: {err}"
    );
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

/// `warm_up_over` restores a state too, and carries its **own copy** of both
/// resume guards — `warm_up_over_wallet` in `src/spec/runnable.rs` repeats the
/// format-version and shape checks that `drive_over` makes.
///
/// The two tests above only reach the `drive_over` copy. A guard that exists
/// twice and is tested once is a guard that can be deleted from one of its
/// homes without anything going red, and the pause-gap path is the one where a
/// mis-restore is least visible: it books no trades, so a silently wrong
/// restore surfaces only in the run that follows it.
#[test]
fn warming_up_over_a_mismatched_or_stale_state_is_rejected_too() {
    use fugazi::spec::RunnableStrategyExt;

    let sch = schema();
    let mut single = buy_and_hold_spec().build(CASH, &sch);
    let (_, state) = single
        .drive_resumable(&single_snaps(20), CASH, &[], None, false)
        .expect("single run");
    assert_eq!(state.kind, "single");

    // Wrong shape.
    let mut pair_strat = pairs_spec().build(CASH, &sch);
    let mut wallet = fugazi::PaperWallet::new(CASH);
    let err = pair_strat
        .warm_up_over(&multi_snaps(20), &mut wallet, Some(&state))
        .expect_err("cross-shape warm-up must fail");
    assert!(
        err.contains("!resume") && err.contains("single") && err.contains("pairs"),
        "unexpected error: {err}"
    );

    // Stale format.
    let mut stale = state.clone();
    stale.format_version += 1;
    let mut fresh = buy_and_hold_spec().build(CASH, &sch);
    let mut wallet = fugazi::PaperWallet::new(CASH);
    let err = fresh
        .warm_up_over(&single_snaps(20), &mut wallet, Some(&stale))
        .expect_err("stale version must fail");
    assert!(err.contains("format version"), "unexpected error: {err}");

    // …and the matching state is accepted, so neither assertion above passes
    // because `warm_up_over` refuses everything.
    let mut fresh = buy_and_hold_spec().build(CASH, &sch);
    let mut wallet = fugazi::PaperWallet::new(CASH);
    fresh
        .warm_up_over(&single_snaps(20), &mut wallet, Some(&state))
        .expect("a matching state warms up");
}

// ---------------------------------------------------------------------------
// The resume file holds *state*, not history
// ---------------------------------------------------------------------------
//
// A `RunState` exists to resume a run. Reporting history — the wallet's fill
// blotter and its rejection log — is not state: nothing in the fill, pricing or
// restore path reads either one, and `RunReport::fills` is built from
// `Wallet::update`'s return value rather than from the blotter. Persisting them
// anyway made the file grow linearly in bars forever; on a 1500-bar 8-symbol
// basket they were 98% of it. These tests pin the three properties that keeps.

/// The serialized size of a `RunState` must not scale with how long the run is.
///
/// This is the regression that would otherwise creep back silently: everything
/// still *works* when history rides along in the state, it just costs more every
/// bar. A 4x longer run over the same spec and universe warms the same chains
/// into the same shape, so the state it saves must stay within a constant factor
/// — while the fill count it discards grows with the bars.
#[test]
fn state_size_does_not_grow_with_run_length() {
    let sch = schema();

    let measure = |bars: usize| -> (usize, usize) {
        let mut strat = single_ema_spec().build(CASH, &sch);
        let (report, state) = strat
            .drive_resumable(&single_snaps(bars), CASH, &[], None, false)
            .expect("run");
        let size = serde_json::to_string(&state)
            .expect("serialize state")
            .len();
        (size, report.fills.len())
    };

    let (short_size, short_fills) = measure(100);
    let (long_size, long_fills) = measure(400);

    // The premise: the longer run really does book more fills, so a
    // history-carrying state would have grown.
    assert!(
        long_fills > short_fills,
        "test is vacuous unless the longer run trades more: {short_fills} -> {long_fills}"
    );

    assert!(
        long_size < short_size * 2,
        "state grew with run length: {short_size} bytes over 100 bars -> {long_size} over 400 \
         ({short_fills} -> {long_fills} fills). The resume file is carrying history again."
    );
}

/// A resumed wallet's blotter covers the resumed chunk, not the whole run.
///
/// `orders()` is an observability accessor, and this is the semantic that pins
/// it: history does not survive a restore. It already matched `RunReport`, which
/// has always been per-chunk.
#[test]
fn a_resumed_wallet_reports_only_its_own_fills() {
    use fugazi::spec::RunnableStrategyExt;

    let sch = schema();
    // Long enough that each half clears the EMA(8) warm-up and trades — the
    // assertions below are vacuous otherwise, so both halves are guarded.
    let snaps = single_snaps(160);
    let cut = 80;

    let mut first = single_ema_spec().build(CASH, &sch);
    let mut wallet = fugazi::PaperWallet::new(CASH);
    let (first_report, state) = first
        .drive_resumable_with(&snaps[..cut], &mut wallet, None, false)
        .expect("first chunk");
    assert_eq!(
        wallet.orders().len(),
        first_report.fills.len(),
        "a cold wallet's blotter is its own fills"
    );
    assert!(
        !first_report.fills.is_empty(),
        "test is vacuous unless the first chunk trades"
    );

    let mut second = single_ema_spec().build(CASH, &sch);
    let mut resumed = fugazi::PaperWallet::new(CASH);
    let (second_report, _) = second
        .drive_resumable_with(&snaps[cut..], &mut resumed, Some(&state), false)
        .expect("resumed chunk");
    assert!(
        !second_report.fills.is_empty(),
        "test is vacuous unless the resumed chunk trades"
    );

    assert_eq!(
        resumed.orders().len(),
        second_report.fills.len(),
        "a resumed wallet's blotter is the resumed chunk's fills, not the run's"
    );
    assert!(
        resumed.orders().len() < first_report.fills.len() + second_report.fills.len(),
        "the resumed blotter must not carry the first chunk's fills forward"
    );
}

/// A state written before history was dropped still resumes, unchanged.
///
/// `WalletSnapshot` simply stopped naming those keys, and serde ignores unknown
/// fields — which is why `RUN_STATE_FORMAT_VERSION` did not have to move. This
/// test is what makes that claim safe to rely on.
#[test]
fn a_state_carrying_legacy_history_keys_still_resumes() {
    let sch = schema();
    let snaps = single_snaps(60);

    let mut cold = single_ema_spec().build(CASH, &sch);
    let (_, mut state) = cold
        .drive_resumable(&snaps[..30], CASH, &[], None, false)
        .expect("first chunk");

    // Re-attach the keys a pre-change build wrote.
    let wallet = state.wallet.as_object_mut().expect("wallet object");
    wallet.insert(
        "blotter".into(),
        serde_json::json!([{
            "id": 0, "symbol": "X", "side": "Buy", "kind": "Market",
            "units": 1.0, "price": 100.0, "commission": 0.0
        }]),
    );
    wallet.insert("rejections".into(), serde_json::json!([]));
    wallet.insert("rejections_drained".into(), serde_json::json!(0));

    let mut legacy = single_ema_spec().build(CASH, &sch);
    let (legacy_report, _) = legacy
        .drive_resumable(&snaps[30..], CASH, &[], Some(&state), false)
        .expect("a legacy state must still resume");

    // And it resumes to the same place a current state does.
    let mut current = single_ema_spec().build(CASH, &sch);
    let (current_report, _) = current
        .drive_resumable(&snaps[30..], CASH, &[], Some(&state.clone()), false)
        .expect("current state");
    assert_eq!(
        legacy_report.equity_curve.len(),
        current_report.equity_curve.len(),
        "legacy state resumed to a different curve length"
    );
    for (i, (a, b)) in legacy_report
        .equity_curve
        .iter()
        .zip(&current_report.equity_curve)
        .enumerate()
    {
        assert_eq!(a.to_bits(), b.to_bits(), "legacy state diverged at bar {i}");
    }
}

/// **A resume into an edited document is refused.**
///
/// Nothing stops `--resume` being pointed at a state file written by a
/// different spec, and replaying configuration in place made that silently
/// wrong in the worst possible way: a `Diff` of period 4 restored from a
/// period-2 blob took the blob's `period` field and kept the destination's
/// four-slot buffer, so it reported a warm-up of 3 while differencing over 4
/// bars; a `Percentile` built for the 90th became the 10th; an `Sma(5)` became
/// a self-consistent `Sma(3)` that contradicted the document it came from.
///
/// None of those is a resumable run. The mismatch is now an error carrying the
/// breadcrumb, so the operator sees *which* knob moved.
#[test]
fn resuming_into_a_changed_document_is_refused_rather_than_silently_hybridised() {
    let sch = schema();
    let snaps = single_snaps(60);

    let mut original = single_ema_spec().build(CASH, &sch);
    let (_, state) = original
        .drive_resumable(&snaps[..30], CASH, &[], None, false)
        .expect("first chunk");

    // The same document with one period changed — a plausible edit between two
    // halves of a run.
    let edited = parse!(
        SingleStrategySpec,
        r#"
        root: X
        long:
          enter: !crosses_above
            lhs: !ema { period: 4, source: !close }
            rhs: !ema { period: 8, source: !close }
          exit: !crosses_below
            lhs: !ema { period: 4, source: !close }
            rhs: !ema { period: 8, source: !close }
    "#
    );
    let err = edited
        .build(CASH, &sch)
        .drive_resumable(&snaps[30..], CASH, &[], Some(&state), false)
        .expect_err("a state from a different document must not resume");
    let msg = err.to_string();
    assert!(
        msg.contains("smoothing factor"),
        "the error should name the knob that moved, got: {msg}"
    );

    // The unchanged document still resumes, so the check is not blanket.
    single_ema_spec()
        .build(CASH, &sch)
        .drive_resumable(&snaps[30..], CASH, &[], Some(&state), false)
        .expect("the original document must still resume");
}

/// The same guard one layer down, on the indicators whose configuration is a
/// plain field rather than an embedded core's. Each of these silently adopted
/// the blob's value before.
#[test]
fn a_config_field_from_a_different_build_is_refused() {
    use fugazi::Indicator;
    use fugazi::indicators::{Diff, Identity, Percentile, Sma, Vwap};

    // `Lookback::period`: took the blob's period while keeping the
    // destination's buffer — a `Diff(4)` that reported the warm-up of a
    // `Diff(2)`.
    let mut short: Diff<Identity<Real>> = Diff::new(Identity::new(), 2);
    for x in [1.0, 2.0, 3.0, 4.0, 5.0] {
        short.update(x);
    }
    let blob = short.save_state();
    let mut long: Diff<Identity<Real>> = Diff::new(Identity::new(), 4);
    assert!(
        long.load_state(&blob).is_err(),
        "Diff accepted a foreign period"
    );

    // `Percentile::pct`: the 90th silently became the 10th.
    let mut p90 = Percentile::new(Identity::<Real>::new(), 3, 0.9);
    for x in [1.0, 2.0, 3.0] {
        p90.update(x);
    }
    let blob = p90.save_state();
    let mut p10 = Percentile::new(Identity::<Real>::new(), 3, 0.1);
    assert!(
        p10.load_state(&blob).is_err(),
        "Percentile accepted a foreign pct"
    );

    // An embedded core: `Sma(5)` rebuilt itself at the blob's period 3.
    let mut sma3 = Sma::new(Identity::<Real>::new(), 3);
    for x in [1.0, 2.0, 3.0] {
        sma3.update(x);
    }
    let blob = sma3.save_state();
    let mut sma5 = Sma::new(Identity::<Real>::new(), 5);
    assert!(
        sma5.load_state(&blob).is_err(),
        "Sma accepted a foreign period"
    );

    // …and a matching destination still round-trips, so the check is not a
    // blanket refusal.
    let mut same = Sma::new(Identity::<Real>::new(), 3);
    same.load_state(&blob)
        .expect("same period must still resume");
    assert_eq!(same.value(), sma3.value());

    // `Vwap::period` is a plain config field over a hand-written window.
    let mut v10: Vwap<Identity<fugazi::types::Candle>> = Vwap::new(Identity::new(), 10);
    v10.update(fugazi::types::Candle::new(1.0, 2.0, 0.5, 1.5, 10.0));
    let blob = v10.save_state();
    let mut v20: Vwap<Identity<fugazi::types::Candle>> = Vwap::new(Identity::new(), 20);
    assert!(
        v20.load_state(&blob).is_err(),
        "Vwap accepted a foreign period"
    );
}
