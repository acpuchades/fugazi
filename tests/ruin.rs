//! Ruin is a terminal run outcome, and every layer above it reads correctly
//! *because* of that rather than by patching itself.
//!
//! The defect these pin: `per_bar_returns` is `(e - prev) / prev`, which
//! **inverts sign** once `prev < 0`. A simulation that kept trading through
//! negative equity therefore turned further losses into positive returns, and a
//! whole region of parameter space acquired a genuinely positive Sharpe — which
//! is exactly what `optimize --best-by sharpe` is built to find. It was not a
//! reporting quirk: on a real strategy set, 14% of grid cells had crossed zero
//! and eight strategies' Sharpe-optimal cells were among them.
//!
//! The fix is one thing in one place — `backtest::run` records
//! [`RunReport::ruin_bar`], liquidates the book there, stops trading, and pins
//! the equity curve at `0.0` for the rest of the run. So the assertions below
//! are mostly *consequences*: bounded drawdown, a defined `-100%` CAGR, no
//! post-ruin fill, a losing rank under any `--best-by`. None of them needed a
//! guard of their own.

mod common;

use common::bars::series;
use common::cli::{Cmd, scratch_file, unique_path};
use fugazi::prelude::*;
use fugazi::spec::metrics;
use fugazi::types::{Snapshot, Symbol};
use fugazi::wallet::{PaperWallet, Side, Size};

/// Enough of a rise to bury the short opened on bar 0: 100 → 600 against a
/// fully-invested short is a >100% loss, so equity crosses zero mid-series and
/// there are bars left afterwards to assert about.
const DOOMED: [Real; 9] = [
    100.0, 100.0, 150.0, 260.0, 320.0, 400.0, 450.0, 500.0, 600.0,
];

/// Shorts the whole account once and never covers.
///
/// The shortest path to insolvency a `PaperWallet` allows: a short's loss is
/// unbounded above, so a rising series takes equity through zero with no
/// leverage knob, no margin model and no cost assumptions involved.
struct NeverCovers;

impl Strategy for NeverCovers {
    type Input = Snapshot<Symbol>;
    type Symbol = Symbol;

    fn update(&mut self, _snap: Snapshot<Symbol>) {}

    fn trade(&self, wallet: &mut dyn Wallet<Symbol>) {
        if wallet.position(&fugazi::types::symbol("X")).amount == 0.0 {
            let _ = wallet.set(fugazi::types::symbol("X"), Side::Sell, Size::value_frac(1.0));
        }
    }

    fn reset(&mut self) {}
}

/// Drive [`NeverCovers`] over `closes` from a 10 000 seed.
fn doomed_run(closes: &[Real]) -> fugazi::RunReport<Symbol> {
    let snaps = series("X", closes, common::bars::flat);
    let mut strategy = NeverCovers;
    let mut wallet: PaperWallet<Symbol> = PaperWallet::new(10_000.0);
    fugazi::backtest::run(&mut strategy, &mut wallet, snaps)
}

// ---------------------------------------------------------------------------
// The driver: ruin is recorded, liquidated and terminal
// ---------------------------------------------------------------------------

#[test]
fn the_curve_is_pinned_at_zero_from_the_ruin_bar_on() {
    let report = doomed_run(&DOOMED);

    let ruin = report.ruin_bar.expect("a fully-invested short into a 6x rise is ruin");
    assert!(ruin < DOOMED.len() - 1, "ruin must leave bars to assert about");

    // The invariant `run` documents at src/backtest.rs — one entry per input
    // snapshot — survives: the tail is pinned, not truncated.
    assert_eq!(
        report.equity_curve.len(),
        DOOMED.len(),
        "one equity point per snapshot, ruined or not"
    );
    assert!(
        report.equity_curve[..ruin].iter().all(|&e| e > 0.0),
        "pre-ruin bars are untouched: {:?}",
        report.equity_curve
    );
    assert!(
        report.equity_curve[ruin..].iter().all(|&e| e == 0.0),
        "every bar from ruin on is pinned at exactly zero: {:?}",
        report.equity_curve
    );
}

#[test]
fn nothing_trades_after_the_ruin_bar() {
    let report = doomed_run(&DOOMED);
    let ruin = report.ruin_bar.expect("ruined");

    assert!(
        report.fills.iter().all(|f| f.bar <= ruin),
        "a blown account kept trading: fills at {:?}, ruin at {ruin}",
        report.fills.iter().map(|f| f.bar).collect::<Vec<_>>()
    );
}

#[test]
fn the_book_is_liquidated_at_ruin_when_the_account_can_afford_it() {
    // Ruin is a liquidation, not just a bookmark: `run` calls `Wallet::flatten`
    // on the ruin bar, so the killer trade closes into the blotter instead of
    // hanging open forever and the wallet a `RunState` is captured from is flat.
    //
    // The caveat is inherent and worth stating: closing a short costs cash the
    // ruined account may no longer have, so the close can be *refused* — see
    // `an_unaffordable_liquidation_is_reported_not_hidden`. Fixing that would
    // take a maintenance-margin model that liquidates before zero, which is
    // deliberately out of scope.
    let report = doomed_run(&[100.0, 100.0, 150.0, 200.0, 300.0, 400.0]);
    let ruin = report.ruin_bar.expect("100 -> 200 against a fully-invested short is exactly ruin");

    assert!(
        report.fills.iter().any(|f| f.bar == ruin && f.order.side == Side::Buy),
        "the short must be covered at ruin: {:?}",
        report.fills
    );
    assert!(report.rejections.is_empty(), "this close was affordable");
}

#[test]
fn an_unaffordable_liquidation_is_reported_not_hidden() {
    // Equity crosses well below zero in one bar, so buying the short back costs
    // more than the account holds and the wallet refuses. The run is still
    // ruined, still terminal, and still pinned — but the refusal is booked like
    // any other rather than swallowed, because "the account could not even
    // close its own position" is a fact about the run.
    let report = doomed_run(&DOOMED);
    let ruin = report.ruin_bar.expect("ruined");
    assert!(
        report.rejections.iter().any(|r| r.bar == ruin),
        "the refused close must reach the report: {:?}",
        report.rejections
    );
    assert!(
        report.rejections.iter().all(|r| r.bar <= ruin),
        "nothing is submitted after ruin, so nothing can be refused after it"
    );
}

#[test]
fn a_solvent_run_is_untouched() {
    // The whole-suite version of this constraint is the rest of `cargo test`;
    // this is the direct statement of it. A falling series suits the short, so
    // equity only ever rises.
    let report = doomed_run(&[100.0, 100.0, 90.0, 80.0, 70.0, 60.0]);
    assert_eq!(report.ruin_bar, None, "this run never crossed zero");
    assert!(
        report.equity_curve.iter().all(|&e| e > 10_000.0 - 1e-9),
        "a profitable short's curve must be reported verbatim: {:?}",
        report.equity_curve
    );
}

#[test]
fn warm_up_never_liquidates() {
    // `warm_up` advances state and submits nothing, so it cannot ruin anything
    // — and must not close a position the caller is about to resume trading.
    let mut strategy = NeverCovers;
    let mut wallet: PaperWallet<Symbol> = PaperWallet::new(10_000.0);
    // Prime a price, then seed a short that the rally below would ruin.
    wallet.update(fugazi::types::symbol("X"), common::bars::flat(100.0));
    wallet
        .set(fugazi::types::symbol("X"), Side::Sell, Size::units(50.0))
        .expect("seeding a short");
    fugazi::backtest::warm_up(
        &mut strategy,
        &mut wallet,
        series("X", &DOOMED, common::bars::flat),
    );
    assert_ne!(
        wallet.position(&fugazi::types::symbol("X")).amount,
        0.0,
        "a warm-up pass must leave the book alone"
    );
}

// ---------------------------------------------------------------------------
// The inversion itself
// ---------------------------------------------------------------------------

#[test]
fn the_return_series_never_turns_a_loss_into_a_gain() {
    // First, the arithmetic that motivates the whole fix, stated directly.
    // `per_bar_returns` is a public function over a caller-supplied series, and
    // on a curve that goes negative it does exactly what the formula says:
    let inverted = fugazi::metrics::per_bar_returns(
        &[100.0, 60.0, 20.0, -20.0, -60.0, -100.0, -140.0],
        100.0,
    );
    // (index 0 is the seed bar, `(100 - 100) / 100`; the crossing is at 3.)
    assert!(
        inverted[4] > 0.0 && inverted[5] > 0.0,
        "the sign inversion below zero is real — this is what the driver must \
         prevent from ever reaching the metrics: {inverted:?}"
    );

    // And now the same shape as it comes out of an actual run: no such curve
    // exists any more, so no such return does either.
    let report = doomed_run(&DOOMED);
    let ruin = report.ruin_bar.expect("ruined");
    let returns = fugazi::metrics::per_bar_returns(&report.equity_curve, report.initial_equity);

    assert_eq!(returns.len(), report.equity_curve.len());
    assert!(
        returns[ruin] < 0.0,
        "the ruin bar is a loss, not a gain: {returns:?}"
    );
    assert!(
        returns[ruin + 1..].iter().all(|&r| r == 0.0),
        "a dead account earns nothing — no return of either sign: {returns:?}"
    );
    assert!(
        returns.iter().all(|&r| r >= -1.0),
        "no bar can lose more than everything: {returns:?}"
    );
}

// ---------------------------------------------------------------------------
// The metrics that used to be nonsense
// ---------------------------------------------------------------------------

/// Reduce a report the way the CLI does, on a daily calendar.
fn reduce(report: &fugazi::RunReport<Symbol>) -> metrics::Metrics {
    metrics::from_report(report, 252.0, 0.0, None)
}

#[test]
fn a_ruined_run_reports_minus_100_percent_and_a_100_percent_drawdown() {
    let report = doomed_run(&DOOMED);
    let m = reduce(&report);

    assert_eq!(m.run.ruin_bar, report.ruin_bar, "the field must survive the reduction");
    assert_eq!(m.run.final_equity, 0.0);
    common::bars::assert_close(m.returns.total_pct, -100.0, 1e-12, "total return");
    common::bars::assert_close(m.drawdown.max_pct, 100.0, 1e-12, "max drawdown");
    // 1780.5% — the number this whole change exists to make unrepresentable.
    assert!(
        m.drawdown.max_pct <= 100.0,
        "a drawdown deeper than the account is arithmetically meaningless"
    );
    assert!(
        m.risk_adjusted.sharpe.is_some_and(|s| s < 0.0),
        "a wiped-out account cannot report a positive Sharpe, got {:?}",
        m.risk_adjusted.sharpe
    );
}

#[test]
fn cagr_distinguishes_ruin_from_too_little_data() {
    // Ruin: a definite -100% per annum, not a blank cell.
    let ruined = reduce(&doomed_run(&DOOMED));
    common::bars::assert_close(
        ruined.returns.cagr_pct.expect("ruin has a defined CAGR: -100%"),
        -100.0,
        1e-12,
        "CAGR on a ruined run",
    );
    assert!(ruined.run.ruin_bar.is_some());

    // Too little data: still `None` — but now distinguishable, because
    // `run.ruin_bar` is absent. Before this the two rendered as the same empty
    // CSV cell and a reader had to guess which they were looking at.
    let degenerate = metrics::from_report(&doomed_run(&[100.0, 90.0]), 0.0, 0.0, None);
    assert_eq!(degenerate.returns.cagr_pct, None, "no bars-per-year to annualize against");
    assert_eq!(
        degenerate.run.ruin_bar, None,
        "absent CAGR here means `undefined`, not `wiped out`"
    );
}

#[test]
fn max_drawdown_never_exceeds_100_percent_over_random_paths() {
    // A property test over runs, not over hand-written curves: the bound is a
    // claim about what `backtest::run` can produce, and `drawdown_segments`
    // stays honest about a series a caller builds by hand.
    //
    // Deterministic LCG rather than `rand` — that dependency is gated behind
    // the `montecarlo` feature, and a seeded sequence makes a failure
    // reproducible from the seed printed in the message.
    let mut state: u64 = 0x5eed_1234_9876_abcd;
    let mut next = move || {
        state = state.wrapping_mul(6_364_136_223_846_793_005).wrapping_add(1_442_695_040_888_963_407);
        ((state >> 33) as Real) / ((1u64 << 31) as Real)
    };

    let mut ruined_paths = 0;
    for path in 0..200 {
        // A random walk with strong upward drift, so a good share of the paths
        // bury the short and the property is tested where it can fail.
        let mut px = 100.0;
        let closes: Vec<Real> = (0..40)
            .map(|_| {
                px *= 1.0 + 0.06 * next();
                px
            })
            .collect();
        let report = doomed_run(&closes);
        if report.ruin_bar.is_some() {
            ruined_paths += 1;
        }
        let m = reduce(&report);
        assert!(
            m.drawdown.max_pct <= 100.0 + 1e-9,
            "path {path}: drawdown {} exceeds the account",
            m.drawdown.max_pct
        );
        assert!(
            report.equity_curve.iter().all(|&e| e >= 0.0),
            "path {path}: equity went negative: {:?}",
            report.equity_curve
        );
    }
    assert!(
        ruined_paths > 0,
        "the generator produced no ruin at all — the property was never exercised"
    );
}

// ---------------------------------------------------------------------------
// Slicing
// ---------------------------------------------------------------------------

#[test]
fn a_slice_can_always_say_whether_it_is_after_ruin() {
    let report = doomed_run(&DOOMED);
    let ruin = report.ruin_bar.expect("ruined");

    // Entirely before: this window describes a live account.
    let before = metrics::report_slice(&report, 0..ruin);
    assert_eq!(before.ruin_bar, None, "ruin lies outside this window");

    // Straddling: reported on the window's own bar axis, like every other
    // index a slice carries.
    let straddle = metrics::report_slice(&report, (ruin - 1)..(ruin + 2));
    assert_eq!(straddle.ruin_bar, Some(1));
    assert_eq!(reduce(&straddle).run.ruin_bar, Some(1));

    // Entirely after: `Some(0)` — dead for the window's whole length, which is
    // the same fact as being ruined on its first bar, and reads that way in the
    // metrics (a flat zero curve). A fold here is pure fiction otherwise.
    let after = metrics::report_slice(&report, (ruin + 1)..DOOMED.len());
    assert_eq!(after.ruin_bar, Some(0), "a post-ruin window must not look merely flat");
    assert!(after.equity_curve.iter().all(|&e| e == 0.0));
    assert!(after.fills.is_empty());
}

#[test]
fn windowed_reductions_carry_ruin_into_the_window_that_owns_it() {
    let report = doomed_run(&DOOMED);
    let ruin = report.ruin_bar.expect("ruined");
    let windows = metrics::windowed_from_report(&report, 2, 252.0, 0.0, None);

    for w in &windows {
        if w.end_bar < ruin {
            assert_eq!(w.metrics.run.ruin_bar, None, "window {w:?} predates ruin");
        } else {
            assert!(
                w.metrics.run.ruin_bar.is_some(),
                "window [{}, {}] is at or past ruin at {ruin} and must say so",
                w.start_bar,
                w.end_bar
            );
        }
    }
}

// ---------------------------------------------------------------------------
// `--flatten` and the portfolio shape
// ---------------------------------------------------------------------------

#[test]
fn flatten_does_not_resurrect_a_ruined_curve() {
    let snaps = series("X", &DOOMED, common::bars::flat);
    let mut strategy = NeverCovers;
    let mut wallet: PaperWallet<Symbol> = PaperWallet::new(10_000.0);
    let mut report = fugazi::backtest::run(&mut strategy, &mut wallet, snaps.clone());
    let before = report.equity_curve.clone();
    let fills_before = report.fills.len();

    fugazi::backtest::flatten_open_positions(&mut strategy, &mut wallet, &snaps, &mut report);

    // The book was already liquidated at ruin, so there is nothing to finalize
    // — and overwriting the final point would replace the pinned `0.0` with the
    // account's true negative balance, un-bounding every metric below it.
    assert_eq!(report.equity_curve, before, "`--flatten` must not un-pin a ruined tail");
    assert_eq!(report.fills.len(), fills_before, "nothing left open to close");
}

#[test]
fn a_portfolio_is_ruined_on_the_accounts_equity_not_a_childs() {
    use fugazi::portfolio::PortfolioBuilder;

    // Two children, one of which shorts into the same doomed series. Ruin is
    // decided on the one real account they net onto — a ledger is notional
    // attribution, not money, so a single child's ledger going negative while
    // its siblings carry the balance is not insolvency.
    let mut portfolio: fugazi::portfolio::Portfolio<Symbol> = PortfolioBuilder::default()
        .with_initial_equity(10_000.0)
        .add("shorts", NeverCovers)
        .add("idle", NeverCovers)
        .build();
    let mut wallet: PaperWallet<Symbol> = PaperWallet::new(10_000.0);
    let snaps = series("X", &DOOMED, common::bars::flat);
    let report = fugazi::backtest::run(&mut portfolio, &mut wallet, snaps);

    // Same driver, same site, no per-shape special case: `backtest.rs` is the
    // sole producer of a backtest equity curve, and a portfolio is an ordinary
    // strategy trading the wallet it was handed.
    let ruin = report.ruin_bar.expect("both children short the same doomed series");
    assert!(report.equity_curve[ruin..].iter().all(|&e| e == 0.0));
    assert!(report.fills.iter().all(|f| f.bar <= ruin));
    assert!(reduce(&report).drawdown.max_pct <= 100.0);
}

// ---------------------------------------------------------------------------
// The CLI: a ruined cell loses on its own, with no filter flag
// ---------------------------------------------------------------------------

/// A short held from the first crossover to the end, at a sweepable leverage.
///
/// Leverage is the knob on purpose: it is the one that manufactures the defect.
/// Every cell here trades *identically* — same entry bar, same exit, same
/// direction — and differs only in size, so the grid isolates "how much of the
/// account was at risk" from every other variable. At 0.2x the short survives the
/// rally and profits on the crash; leveraged up, it is wiped out during it.
const SWEEPABLE_SHORT: &str = "\
symbol: X
sizing: !param LEVERAGE
short:
  enter: !crosses_above
    lhs: !close
    rhs: !sma { source: close, period: 5 }
  exit: !never
";

/// A rally that buries a leveraged short, then a crash that pays an unleveraged
/// one — so the grid has both a ruined cell and a genuinely profitable one to
/// rank against each other.
///
/// Synthetic on purpose: the reproduction that motivated this needed
/// fugazi-labs and fugazi-datasets, and a regression test may not.
fn doomed_csv() -> String {
    let mut out = String::from("time,symbol,open,high,low,close,volume\n");
    let mut closes: Vec<Real> = Vec::new();
    let mut px = 100.0;
    for i in 0..70 {
        if (8..30).contains(&i) {
            px *= 1.05; // the rally that kills the leveraged short
        } else if i >= 30 {
            px *= 0.95; // the crash the surviving short is paid for
        }
        closes.push(px);
    }
    for (i, px) in closes.iter().enumerate() {
        let t = 1_600_000_000_000i64 + i as i64 * 86_400_000;
        out.push_str(&format!("{t},X,{px},{px},{px},{px},1000\n"));
    }
    out
}

fn column(header: &str, row: &str, name: &str) -> String {
    let idx = header
        .split(',')
        .position(|c| c == name)
        .unwrap_or_else(|| panic!("no `{name}` column in: {header}"));
    row.split(',').nth(idx).unwrap_or_default().to_string()
}

#[test]
fn a_ruined_cell_does_not_win_best_by() {
    let (spec, _) = scratch_file("ruin_sweep_strategy.yml", SWEEPABLE_SHORT);
    let (csv_in, _) = scratch_file("ruin_sweep_series.csv", &doomed_csv());
    let out = unique_path("ruin_sweep").with_extension("csv");
    let out_str = out.to_string_lossy().into_owned();
    let _ = std::fs::remove_file(&out);

    // Every ranking direction the report named, each on its own run: the claim
    // is that a wiped-out cell sorts below a solvent one under *any* of them,
    // with no `--exclude-ruined` flag and no drawdown screen.
    for best_by in ["sharpe", "sortino", "cagr_pct"] {
        Cmd::new("optimize")
            .arg(&format!("@{}", spec.display()))
            .series(&format!("@{}", csv_in.display()))
            .args(&["--crypto", "-f", "1d"])
            .args(&["--grid", "LEVERAGE=[0.2,2]"])
            .args(&["--metrics", "sharpe,sortino,cagr_pct,max_pct,run.ruin_bar"])
            .args(&["--best-by", best_by])
            .args(&["--output", &out_str])
            .ok();

        let text = std::fs::read_to_string(&out).expect("optimize wrote no CSV");
        let mut lines = text.lines();
        let header = lines.next().expect("header").to_string();
        let rows: Vec<&str> = lines.collect();
        assert!(rows.len() > 1, "need a solvent cell to out-rank the ruined one");

        // Some cell must actually have been ruined, or the test proves nothing.
        let ruined: Vec<&&str> = rows
            .iter()
            .filter(|r| !column(&header, r, "run.ruin_bar").is_empty())
            .collect();
        assert!(
            !ruined.is_empty(),
            "the fixture no longer ruins anything under --best-by {best_by}:\n{text}"
        );
        assert!(
            ruined.len() < rows.len(),
            "every cell was ruined — nothing solvent left to compare against:\n{text}"
        );

        // `--best-by` sorts the winner to the top. It must not be a dead one.
        let winner = rows[0];
        assert!(
            column(&header, winner, "run.ruin_bar").is_empty(),
            "--best-by {best_by} picked a wiped-out cell as the winner:\n{header}\n{winner}"
        );
        // And each ruined cell reports the bound rather than a fantasy number.
        for r in ruined {
            let dd: Real = column(&header, r, "drawdown.max_pct").parse().unwrap_or(0.0);
            assert!(dd <= 100.0 + 1e-9, "drawdown {dd} exceeds the account:\n{r}");
        }
    }
}

#[test]
fn run_reports_ruin_rather_than_leaving_it_to_be_inferred() {
    let (spec, _) = scratch_file("ruin_run_strategy.yml", &SWEEPABLE_SHORT.replace("!param LEVERAGE", "2"));
    let (csv_in, _) = scratch_file("ruin_run_series.csv", &doomed_csv());

    let out = Cmd::new("run")
        .arg(&format!("@{}", spec.display()))
        .series(&format!("@{}", csv_in.display()))
        .args(&["--crypto", "-f", "1d"])
        .output_dir("ruin_run")
        .ok();

    let console = format!("{}{}", out.stdout, out.stderr);
    assert!(
        console.contains("ruined at bar"),
        "the console must say the account was wiped out, not leave it to be \
         read off a blank CAGR:\n{console}"
    );
    let metrics = out.read("metrics.yml");
    assert!(
        metrics.contains("ruin_bar:"),
        "ruin must be a field, not an inference:\n{metrics}"
    );
}

// ---------------------------------------------------------------------------
// Ranking: a dead account is not a candidate
// ---------------------------------------------------------------------------
//
// `1a253e8` claimed a ruined row "sorts last under any `--best-by` on its own
// arithmetic". That holds for the metrics *anchored to terminal wealth* —
// `cagr_pct` is exactly `-100`, `calmar` and `recovery_factor` exactly `-1`,
// `max_pct` exactly `100`. It does not hold for a bar-return ratio, and
// `sharpe` is one: truncating at ruin contributes **one** `-100%` bar out of
// however many the run had, so a strategy that compounds for years and then
// dies keeps a positive Sharpe, and `--best-by sharpe` finds it.
//
// The fix is one predicate — `optimize::ranking_lookup` — at the one place a
// `Metrics` becomes a *ranking key*. The metric keeps its value everywhere
// else; what a ruined run loses is not its Sharpe, it is its candidacy.
//
// Two alternatives were considered and rejected; `TODO.md` records why, and
// `ruin_is_unrankable_but_its_numbers_are_still_reported` pins the choice.

use fugazi::spec::optimize::{
    Direction, Evaluation, argbest, direction_for, lookup, ranking_lookup, ranking_value,
};
use fugazi::wallet::{OrderId, OrderKind};
use fugazi::{Fill, Order, RunReport};

/// A `Fill` of `units` of `X` at `price` on `bar`.
fn blotter_fill(bar: usize, side: Side, units: Real, price: Real) -> Fill<Symbol> {
    Fill {
        bar,
        order: Order::new(
            fugazi::types::symbol("X"),
            side,
            units,
            price,
            OrderKind::Market,
            OrderId(bar as u64),
        ),
    }
}

/// One closed long: in at `a` on `bar`, out at `b` on the next.
fn round_trip(bar: usize, a: Real, b: Real) -> Vec<Fill<Symbol>> {
    vec![
        blotter_fill(bar, Side::Buy, 1.0, a),
        blotter_fill(bar + 1, Side::Sell, 1.0, b),
    ]
}

/// How long the synthetic curves below run. Long enough that one `-100%` bar
/// is a rounding error in the return moments — which is the whole defect: at
/// 1 858 bars (the length of the daily series that surfaced this) ruin moves
/// Sharpe by less than the difference between two neighbouring grid cells.
const RANKING_BARS: usize = 1858;

/// The curve a wiped-out grid cell actually draws: smooth compounding, shallow
/// drawdowns, mostly-winning trades — and then nothing, because there is no
/// money left. Ruined on `at`, pinned at zero from there on exactly as
/// `backtest::run` pins it.
fn pretty_then_dead(at: usize) -> metrics::Metrics {
    let mut equity = Vec::with_capacity(RANKING_BARS);
    let mut e = 10_000.0;
    for i in 0..at {
        e *= if i % 11 == 0 { 0.999 } else { 1.006 };
        equity.push(e);
    }
    equity.extend(std::iter::repeat_n(0.0, RANKING_BARS - at));

    let mut fills = Vec::new();
    for i in 0..(at / 4).max(1) {
        let bar = i * 4;
        if i % 9 == 0 {
            fills.extend(round_trip(bar, 100.0, 99.0));
        } else {
            fills.extend(round_trip(bar, 100.0, 103.0));
        }
    }
    // The wipeout is a realized trade: `run` liquidates the book at ruin.
    fills.extend(round_trip(at.saturating_sub(2), 100.0, 0.0001));

    reduce_curve(equity, fills, Some(at))
}

/// A modest, solvent, genuinely profitable cell — noisier than the doomed one
/// on purpose, so on every bar-return statistic it is the *less* attractive of
/// the two and only its survival distinguishes it.
fn modest_and_alive() -> metrics::Metrics {
    let mut equity = Vec::with_capacity(RANKING_BARS);
    let mut e = 10_000.0;
    for i in 0..RANKING_BARS {
        e *= 1.0 + 0.0012 + 0.02 * (i as Real * 1.7).sin();
        equity.push(e);
    }
    let mut fills = Vec::new();
    for i in 0..40 {
        let bar = i * 4;
        if i % 2 == 0 {
            fills.extend(round_trip(bar, 100.0, 108.0));
        } else {
            fills.extend(round_trip(bar, 100.0, 94.0));
        }
    }
    reduce_curve(equity, fills, None)
}

fn reduce_curve(
    equity_curve: Vec<Real>,
    fills: Vec<Fill<Symbol>>,
    ruin_bar: Option<usize>,
) -> metrics::Metrics {
    let report = RunReport {
        equity_curve,
        fills,
        rejections: Vec::new(),
        initial_equity: 10_000.0,
        ruin_bar,
    };
    metrics::from_report(&report, 252.0, 0.0, None)
}

fn whole(m: metrics::Metrics) -> Evaluation {
    Evaluation::Whole(Box::new(m))
}

/// **The invariant.** Asserted over the whole of [`direction_for`]'s table, not
/// a sample, so a metric added later is covered the day it is added.
///
/// Before the guard, 17 of the 39 rankable paths preferred one of these dead
/// accounts to the live one — `sharpe`, `sortino` and `omega` among them, plus
/// `mean_bar`, `win_rate_pct`, the VaR pair and most of the `drawdown.*` block.
/// The other 22 were safe only in the sense that *this* pair of curves does not
/// reach them: `best_bar`, `largest_win` and `payoff_ratio` are one lucky
/// pre-ruin trade away, and `stddev_bar` and `ulcer_index` are bounded only by
/// `1/sqrt(bars)`. Enumerating the safe ones was never going to be the fix.
#[test]
fn no_rankable_metric_prefers_a_ruined_run_to_a_solvent_profitable_one() {
    let solvent = modest_and_alive();
    assert!(
        solvent.run.ruin_bar.is_none() && solvent.returns.total_pct > 0.0,
        "the control must be solvent and profitable, or the test proves nothing"
    );

    // Ruin early, in the middle, and on the last bar: the damage one `-100%`
    // bar does to a moment shrinks as it moves later, so the final-bar case is
    // where a pre-ruin ratio is at its most flattering.
    let ruined: Vec<(usize, metrics::Metrics)> = [3, 20, 200, RANKING_BARS / 2, RANKING_BARS - 1]
        .into_iter()
        .map(|at| (at, pretty_then_dead(at)))
        .collect();
    for (at, m) in &ruined {
        assert_eq!(m.run.ruin_bar, Some(*at));
    }

    let mut checked = 0;
    for (path, _) in metrics::flatten(&solvent) {
        let Some(direction) = direction_for(path) else { continue };
        checked += 1;

        let solvent_key = ranking_value(&whole(solvent.clone()), path, direction, 0.0);
        for (at, m) in &ruined {
            let ruined_key = ranking_value(&whole(m.clone()), path, direction, 0.0);
            assert_eq!(
                ruined_key, None,
                "`{path}` is rankable, so a run ruined at bar {at} must have no ranking \
                 value under it — it had {ruined_key:?}"
            );
            // And the ordering that follows from it, through the same
            // comparator `--best-by` sorts with.
            let keys = [ruined_key, solvent_key];
            assert_eq!(
                argbest(&keys, direction),
                Some(1),
                "`{path}` ranked a run ruined at bar {at} above a solvent profitable one"
            );
        }
    }
    assert_eq!(
        checked,
        39,
        "the direction table changed size — re-read the rule in `ranking_lookup` and \
         check the new entries against it rather than adjusting this number"
    );
}

/// **The design decision, pinned.**
///
/// Three ways to stop a dead account winning a sweep were on the table:
///
/// 1. `None` from the metric itself — "a run that ceased to exist has no
///    Sharpe". Rejected: the rule it needs does not exist. To satisfy the
///    invariant it has to null ~30 of the 39, including `stddev_bar`,
///    `var_95`, `drawdown.max_duration_bars` and `largest_loss`, ten of which
///    are non-`Option` fields today — so it is a schema change to `metrics.yml`
///    that leaves a ruined run's document nearly empty, *and* it still covers
///    only the metrics someone remembered to list.
/// 2. Unrankable, but still reported — this. One predicate, total coverage,
///    every number kept.
/// 3. A parallel `pre_ruin.` namespace. Rejected: (1)'s blast radius plus a
///    second catalogue to keep in step — `direction_for` entries, `flatten`,
///    CSV columns, Python — to say what `run.ruin_bar` beside the plain value
///    already says.
///
/// So: the value survives, the candidacy does not. Change this test only
/// together with the decision it records.
#[test]
fn ruin_is_unrankable_but_its_numbers_are_still_reported() {
    let m = pretty_then_dead(RANKING_BARS - 1);

    let sharpe = lookup(&m, "risk_adjusted.sharpe").expect("a pre-ruin Sharpe exists");
    assert!(
        sharpe > 0.0,
        "the fixture must reproduce the defect — a *positive* Sharpe on a wiped-out \
         account — or it pins nothing: got {sharpe}"
    );
    assert_eq!(m.risk_adjusted.sharpe, Some(sharpe), "the document keeps the number");
    assert_eq!(m.run.ruin_bar, Some(RANKING_BARS - 1), "and says it is a dead account");
    assert_eq!(m.returns.cagr_pct, Some(-100.0), "the terminal-wealth metrics still read ruin");

    // Same document, asked as a ranking key rather than as a description.
    assert_eq!(ranking_lookup(&m, "risk_adjusted.sharpe"), None);
    assert_eq!(
        ranking_value(&whole(m), "risk_adjusted.sharpe", Direction::Descending, 0.0),
        None
    );
}

/// The edge case where truncation removes almost nothing and the pre-ruin ratio
/// is at its most nearly legitimate: the account died on the **final** bar, so
/// the run is 1 857 bars of real trading and one bar of insolvency.
///
/// It is still not a candidate. "Nearly the whole run was real" is a matter of
/// degree, and a degree is a threshold; ruin is not.
#[test]
fn a_run_ruined_on_the_final_bar_is_still_unrankable() {
    let last = pretty_then_dead(RANKING_BARS - 1);
    let early = pretty_then_dead(3);

    let key = |m: &metrics::Metrics| ranking_lookup(m, "risk_adjusted.sharpe");
    assert_eq!(key(&last), None);
    assert_eq!(key(&early), None);

    // The two dead accounts differ enormously as *descriptions* — which is the
    // reason the numbers are kept — and not at all as candidates.
    let (a, b) = (
        lookup(&last, "risk_adjusted.sharpe").unwrap(),
        lookup(&early, "risk_adjusted.sharpe").unwrap(),
    );
    assert!(
        a > b + 1.0,
        "a final-bar ruin should describe far better than a bar-3 one: {a} vs {b}"
    );
}

/// Under `-w` a row is ruined if **any** window is. `report_slice` clamps ruin
/// into the window it lands in and reports `Some(0)` for every window after it,
/// so a row whose later folds are flat zeros is not a row that was solvent for
/// its early ones — the account only dies once.
#[test]
fn a_windowed_row_is_unrankable_if_any_window_was_ruined() {
    let solvent = modest_and_alive();
    let dead = pretty_then_dead(RANKING_BARS / 2);
    let window = |start_bar, end_bar, m: &metrics::Metrics| metrics::WindowMetrics {
        start_bar,
        end_bar,
        metrics: m.clone(),
    };

    let all_alive = Evaluation::Windowed(vec![
        window(0, 99, &solvent),
        window(100, 199, &solvent),
        window(200, 299, &solvent),
    ]);
    assert_eq!(all_alive.ruin_bar(), None);
    assert!(
        ranking_value(&all_alive, "risk_adjusted.sharpe", Direction::Descending, 0.0).is_some(),
        "a row that never died must stay rankable under -w"
    );

    // Ruin in the middle fold. The two solvent folds around it must not average
    // it back into contention.
    let died_midway = Evaluation::Windowed(vec![
        window(0, 99, &solvent),
        window(100, 199, &dead),
        window(200, 299, &solvent),
    ]);
    assert_eq!(
        died_midway.ruin_bar(),
        Some(100 + RANKING_BARS / 2),
        "the absolute bar, not the window-relative one"
    );
    assert_eq!(
        ranking_value(&died_midway, "risk_adjusted.sharpe", Direction::Descending, 0.0),
        None
    );
}

// ---------------------------------------------------------------------------
// The CLI: `optimize` neither picks a dead account nor hides one
// ---------------------------------------------------------------------------

/// A trend-following short whose only knob is the moving average it reads.
///
/// The knob matters because it decides *whether the cell is in the market when
/// the roof falls in*, independently of how well it traded before then — which
/// is the shape of the real failure. A fast cell trades the trend beautifully
/// and is still short at the spike; a slow one is flat and survives having
/// traded much worse.
const RUIN_SWEEPABLE_MA: &str = "\
symbol: X
sizing: !value 1.0
short:
  enter: !lt
    lhs: !close
    rhs: !sma { source: close, period: !param PERIOD }
  exit: !gt
    lhs: !close
    rhs: !sma { source: close, period: !param PERIOD }
";

/// 1 858 daily bars in which the Sharpe-optimal cell is a wiped-out account.
///
/// The reproduction that motivated this needed fugazi-labs and
/// fugazi-datasets; a regression test may not, so the path is built here:
///
/// - 1 700 bars of a drifting-down series with **AR(1)** shocks. The
///   autocorrelation is the point: it makes the series trend, so a trend
///   follower actually works on it and the fast cells earn a real Sharpe rather
///   than a whipsawed one.
/// - a 60-bar rally, then a 16-bar slide. Close ends below every moving average
///   up to 60 — those cells have just gone short — and still above the 100- and
///   200-bar ones, which are flat.
/// - one bar at 4×, which is ruin for anyone short.
///
/// The result: `PERIOD=3` posts the highest Sharpe in the grid (**+2.76**) with
/// `cagr_pct = -100`, and `PERIOD=100` is the best cell that still has an
/// account. Before the ranking guard, `--best-by sharpe` returned the former.
/// Bars between the end of the noise stretch and the 4× spike: the 60-bar rally
/// plus the 16-bar slide. So a series built from `n` noise bars is ruined on bar
/// `n + APPROACH`.
const APPROACH: usize = 76;

/// The grid sweep's series: 1 700 noise bars, so ruin lands at bar 1 776 of
/// 1 858 — late, where a bar-return ratio is at its most flattering.
const SWEEP_NOISE: usize = 1700;
const RUIN_BAR: usize = SWEEP_NOISE + APPROACH;

/// The walk-forward series: the same shape with ruin 220 bars earlier, so an
/// in-sample window long enough to dilute one `-100%` bar can still straddle
/// it. Both are 1 858 bars.
const WF_NOISE: usize = 1480;
const WF_RUIN_BAR: usize = WF_NOISE + APPROACH;

fn ma_sweep_csv() -> String {
    ma_series(SWEEP_NOISE)
}

fn ma_series(noise_bars: usize) -> String {
    let mut closes: Vec<Real> = Vec::with_capacity(1858);
    let mut px: Real = 100.0;
    // Deterministic pseudo-noise — an LCG, so the fixture is a constant.
    let mut seed: u64 = 0x2545_F491_4F6C_DD1D;
    let mut noise = || {
        seed = seed.wrapping_mul(6364136223846793005).wrapping_add(1442695040888963407);
        ((seed >> 33) as Real / (1u64 << 31) as Real) * 2.0 - 1.0
    };
    let mut shock: Real = 0.0;
    for _ in 0..noise_bars {
        shock = 0.85 * shock + 0.020 * noise();
        px *= 1.0 - 0.0015 + shock;
        closes.push(px);
    }
    for _ in 0..60 {
        px *= 1.014;
        closes.push(px);
    }
    for _ in 0..16 {
        px *= 0.980;
        closes.push(px);
    }
    px *= 4.0;
    closes.push(px);
    for _ in 0..(1858 - noise_bars - APPROACH - 1) {
        px *= 0.999;
        closes.push(px);
    }
    assert_eq!(closes.len(), 1858, "the fixture is 1 858 bars whatever the ruin bar");

    let mut out = String::from("time,symbol,open,high,low,close,volume\n");
    for (i, px) in closes.iter().enumerate() {
        let t = 1_600_000_000_000i64 + i as i64 * 86_400_000;
        out.push_str(&format!("{t},X,{px},{px},{px},{px},1000\n"));
    }
    out
}

/// Run the fixture sweep and hand back `(console, csv_header, csv_rows)`.
fn ma_sweep(extra: &[&str], tag: &str) -> (String, String, Vec<String>) {
    let (spec, _) = scratch_file(&format!("ruin_ma_{tag}_strategy.yml"), RUIN_SWEEPABLE_MA);
    let (series, _) = scratch_file(&format!("ruin_ma_{tag}_series.csv"), &ma_sweep_csv());
    let out = unique_path(&format!("ruin_ma_{tag}")).with_extension("csv");
    let out_str = out.to_string_lossy().into_owned();
    let _ = std::fs::remove_file(&out);

    let outcome = Cmd::new("optimize")
        .arg(&format!("@{}", spec.display()))
        .series(&format!("@{}", series.display()))
        .args(&["--crypto", "-f", "1d"])
        .args(&["--grid", "PERIOD=[3,5,10,20,40,60,100,200]"])
        .args(&["--metrics", "sharpe,cagr_pct,max_pct,run.ruin_bar"])
        .args(extra)
        .args(&["--output", &out_str])
        .ok();

    let text = std::fs::read_to_string(&out).expect("optimize wrote no CSV");
    let mut lines = text.lines();
    let header = lines.next().expect("header").to_string();
    let rows: Vec<String> = lines.map(str::to_string).collect();
    (format!("{}{}", outcome.stdout, outcome.stderr), header, rows)
}

fn is_ruined(header: &str, row: &str) -> bool {
    !column(header, row, "run.ruin_bar").is_empty()
}

fn sharpe_of(header: &str, row: &str) -> Real {
    column(header, row, "risk_adjusted.sharpe").parse().expect("a Sharpe cell")
}

/// The reproduction, end to end: the grid's Sharpe-*optimal* cell is a wiped-out
/// account, and `--best-by sharpe` does not return it.
///
/// This is the assertion `1a253e8` believed it had already made. It did not:
/// the cell it checked was ruined *and* Sharpe-worst, because the leverage grid
/// it used trades every cell identically and Sharpe is leverage-invariant, so a
/// wipeout could only ever push a cell down. Here the cells trade differently,
/// which is the only way the defect shows.
#[test]
fn the_sharpe_optimal_cell_can_be_a_dead_account_and_still_not_win() {
    let (_console, header, rows) = ma_sweep(&["--best-by", "sharpe"], "best");

    let ruined: Vec<&String> = rows.iter().filter(|r| is_ruined(&header, r)).collect();
    let solvent: Vec<&String> = rows.iter().filter(|r| !is_ruined(&header, r)).collect();
    assert!(!ruined.is_empty() && !solvent.is_empty(), "need both kinds:\n{rows:#?}");
    for row in &ruined {
        assert_eq!(
            column(&header, row, "run.ruin_bar"),
            RUIN_BAR.to_string(),
            "every doomed cell dies on the spike, at the bar `RUIN_BAR` names:\n{row}"
        );
    }

    // The defect, still present in the numbers: the best Sharpe in this grid
    // belongs to an account that reached zero. If this ever stops holding the
    // fixture has drifted and the test below proves nothing.
    let best_sharpe = rows
        .iter()
        .max_by(|a, b| sharpe_of(&header, a).total_cmp(&sharpe_of(&header, b)))
        .expect("a non-empty grid");
    assert!(
        is_ruined(&header, best_sharpe),
        "the fixture must keep a *ruined* cell as the Sharpe argmax:\n{header}\n{best_sharpe}"
    );
    assert!(
        sharpe_of(&header, best_sharpe) > 0.0,
        "and that Sharpe must be positive, or ruin is not what is being hidden"
    );

    // And the fix: the row `--best-by` sorted to the top is a live account.
    let winner = &rows[0];
    assert!(
        !is_ruined(&header, winner),
        "--best-by sharpe picked a wiped-out cell:\n{header}\n{winner}"
    );
    assert!(
        sharpe_of(&header, winner) < sharpe_of(&header, best_sharpe),
        "the winner should be beaten on raw Sharpe by the dead cell it out-ranked — \
         otherwise the guard was never exercised"
    );
    // Every ruined cell sorts below every solvent one, not just the first.
    let last_solvent = rows.iter().rposition(|r| !is_ruined(&header, r)).unwrap();
    let first_ruined = rows.iter().position(|r| is_ruined(&header, r)).unwrap();
    assert!(
        last_solvent < first_ruined,
        "ruined cells must sort as a block below the solvent ones:\n{rows:#?}"
    );
}

/// The value is kept. `--best-by` stops selecting on it; nothing stops
/// reporting it. See `ruin_is_unrankable_but_its_numbers_are_still_reported`
/// for why that is the choice.
#[test]
fn a_ruined_cell_keeps_its_metrics_in_the_csv() {
    let (_console, header, rows) = ma_sweep(&["--best-by", "sharpe"], "csv");
    let ruined: Vec<&String> = rows.iter().filter(|r| is_ruined(&header, r)).collect();
    assert!(!ruined.is_empty());
    for row in ruined {
        let sharpe = column(&header, row, "risk_adjusted.sharpe");
        assert!(
            sharpe.parse::<Real>().is_ok(),
            "a ruined cell keeps its pre-ruin Sharpe rather than blanking it:\n{row}"
        );
        assert_eq!(
            column(&header, row, "returns.cagr_pct"),
            "-100",
            "and its terminal-wealth metrics still read ruin:\n{row}"
        );
    }
}

/// Defect B: the console said `+356.06% ann` for a zeroed account and nothing
/// else. `run` gained a ruin banner in `1a253e8`; `optimize` reports N cells and
/// shows one, so it needs both — a count for the rows in the CSV, and a line on
/// the winner when the winner itself is dead.
#[test]
fn optimize_names_ruin_for_a_ruined_row_and_for_a_ruined_winner() {
    // A ruined *non-winner*: the sweep is correctly ranked and still has to say
    // that seven of its eight rows are dead accounts.
    let (console, header, rows) = ma_sweep(&["--best-by", "sharpe"], "warn");
    let n_ruined = rows.iter().filter(|r| is_ruined(&header, r)).count();
    assert!(n_ruined > 0 && n_ruined < rows.len());
    assert!(
        console.contains(&format!("{n_ruined} of {} grid points ended in ruin", rows.len())),
        "a correctly-ranked sweep must still name its dead rows:\n{console}"
    );
    assert!(
        !console.contains("ruined at bar"),
        "…but must not claim the *winner* is one when it is not:\n{console}"
    );

    // A ruined winner: rank by a metric every cell here is degenerate under, so
    // no cell is rankable and the block falls back to the first row — which is
    // ruined. The headline figures above it describe a run that ended at zero.
    let (spec, _) = scratch_file("ruin_ma_win_strategy.yml", RUIN_SWEEPABLE_MA);
    let (series, _) = scratch_file("ruin_ma_win_series.csv", &ma_sweep_csv());
    let out = unique_path("ruin_ma_win").with_extension("csv");
    let out_str = out.to_string_lossy().into_owned();
    let outcome = Cmd::new("optimize")
        .arg(&format!("@{}", spec.display()))
        .series(&format!("@{}", series.display()))
        .args(&["--crypto", "-f", "1d"])
        // Only cells that die, so every candidate is unrankable and the winner
        // is necessarily a dead one.
        .args(&["--grid", "PERIOD=[3,5,10]"])
        .args(&["--metrics", "sharpe,cagr_pct,run.ruin_bar"])
        .args(&["--best-by", "sharpe"])
        .args(&["--output", &out_str])
        .ok();
    let console = format!("{}{}", outcome.stdout, outcome.stderr);
    assert!(
        console.contains("ruined at bar"),
        "the best block must qualify the headline it prints for a dead winner:\n{console}"
    );
    assert!(
        console.contains("Every cell in this grid ended in ruin"),
        "and say that there was no solvent cell to pick:\n{console}"
    );
}

/// `--smooth` needs no change of its own: a ruined cell has no ranking key, and
/// a missing key already contributes no weight to its neighbours' averages and
/// lowers their `_support`. That is the honest reading — the neighbourhood
/// average rests on fewer cells — and it is what the flag's support column is
/// for. Treating ruin as "very bad" instead would mean inventing a magnitude.
#[test]
fn smoothing_gives_a_ruined_cell_no_weight_and_says_so_in_support() {
    let (spec, _) = scratch_file("ruin_ma_smooth_strategy.yml", RUIN_SWEEPABLE_MA);
    let (series, _) = scratch_file("ruin_ma_smooth_series.csv", &ma_sweep_csv());
    let out = unique_path("ruin_ma_smooth").with_extension("csv");
    let out_str = out.to_string_lossy().into_owned();
    Cmd::new("optimize")
        .arg(&format!("@{}", spec.display()))
        .series(&format!("@{}", series.display()))
        .args(&["--crypto", "-f", "1d"])
        .args(&["--grid", "PERIOD=[3,5,10,20,40,60,100,200]"])
        .args(&["--metrics", "sharpe,run.ruin_bar"])
        .args(&["--best-by", "sharpe", "--smooth=box:1"])
        .args(&["--output", &out_str])
        .ok();

    let text = std::fs::read_to_string(&out).expect("optimize wrote no CSV");
    let mut lines = text.lines();
    let header = lines.next().expect("header").to_string();
    let rows: Vec<String> = lines.map(str::to_string).collect();
    let support = |row: &str| -> Real {
        column(&header, row, "risk_adjusted.sharpe_support").parse().expect("a support cell")
    };

    for row in &rows {
        if !is_ruined(&header, row) {
            continue;
        }
        assert_eq!(
            column(&header, row, "risk_adjusted.sharpe_smoothed"),
            "",
            "a ruined cell contributes no ranking key, so it has no smoothed value \
             of its own either:\n{row}"
        );
    }
    // A cell whose neighbourhood contains a dead one must say its average rests
    // on less: full support on this stencil is 1.0.
    assert!(
        rows.iter().any(|r| !is_ruined(&header, r) && support(r) < 1.0),
        "a solvent cell next to a ruined one should carry reduced support:\n{text}"
    );
}

/// Walk-forward selects a winner **per fold**, from that fold's in-sample
/// slice, through its own call site — so it needs the same rule, and a fold is
/// where getting it wrong is most expensive: the cell picked in-sample is the
/// cell whose out-of-sample slice becomes the composite. Pick a dead one and
/// the composite is stitched from a flat-zero curve.
///
/// Both halves are asserted here.
///
/// - A fold whose in-sample slice **contains** the ruin bar must not crown the
///   cell that died in it. Without the guard this fold picks `PERIOD=3` on an
///   in-sample Sharpe of `+1.84` with `run.ruin_bar_is` set — the account was
///   already at zero for the last 300 bars of the window it was selected on.
/// - A fold whose winner was solvent when it was picked and blew up **out of**
///   sample has to say so, because `sharpe_oos` reads like an ordinary bad fold
///   and `_wfe` like an ordinary bad ratio.
///
/// The in-sample half needs a window long enough to dilute one `-100%` bar,
/// which is why this runs against [`WF_NOISE`]'s earlier ruin rather than the
/// grid sweep's: over a 500-bar slice the wipeout sinks the cell on its own
/// arithmetic and the guard is never reached.
#[test]
fn a_fold_neither_picks_a_cell_that_died_in_sample_nor_hides_one_that_died_out() {
    let (spec, _) = scratch_file("ruin_ma_wf_strategy.yml", RUIN_SWEEPABLE_MA);
    let (series, _) = scratch_file("ruin_ma_wf_series.csv", &ma_series(WF_NOISE));
    let out = unique_path("ruin_ma_wf").with_extension("csv");
    let out_str = out.to_string_lossy().into_owned();
    let outcome = Cmd::new("optimize")
        .arg(&format!("@{}", spec.display()))
        .series(&format!("@{}", series.display()))
        .args(&["--crypto", "-f", "1d"])
        .args(&["--grid", "PERIOD=[3,5,10,20,40,60,100,200]"])
        .args(&["--metrics", "sharpe,run.ruin_bar"])
        .args(&["--best-by", "sharpe"])
        // Long in-sample windows, so one wiped-out bar in 1 300 is exactly as
        // invisible to Sharpe as it is over a whole run.
        .args(&["--walkforward", "1300,100"])
        .args(&["--output", &out_str])
        .ok();

    let text = std::fs::read_to_string(&out).expect("optimize wrote no walk-forward CSV");
    let mut lines = text.lines();
    let header = lines.next().expect("header").to_string();
    let rows: Vec<String> = lines.map(str::to_string).collect();

    // No fold may have selected a cell that was already dead in the slice it
    // was selected on.
    for row in &rows {
        assert_eq!(
            column(&header, row, "run.ruin_bar_is"),
            "",
            "a fold selected a cell that was wiped out inside its own in-sample \
             slice:\n{header}\n{row}"
        );
    }
    // And the fixture must actually have offered one, or the loop above is
    // vacuous: some fold's in-sample window has to contain the ruin bar.
    let straddles = rows.iter().any(|r| {
        let start: usize = column(&header, r, "is_start").parse().unwrap_or(0);
        let end: usize = column(&header, r, "is_end").parse().unwrap_or(0);
        (start..end).contains(&WF_RUIN_BAR)
    });
    assert!(straddles, "no fold's in-sample slice covers bar {WF_RUIN_BAR}:\n{text}");

    // The other half: a fold whose winner died out of sample says so.
    let died_oos = rows.iter().any(|r| !column(&header, r, "run.ruin_bar_oos").is_empty());
    assert!(died_oos, "the fixture should ruin some fold's winner out of sample:\n{text}");
    let console = format!("{}{}", outcome.stdout, outcome.stderr);
    assert!(
        console.contains("ruined oos@"),
        "the folds table must name a winner that blew up out of sample:\n{console}"
    );
}
