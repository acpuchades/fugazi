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
