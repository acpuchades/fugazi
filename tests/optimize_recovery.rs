//! Does `optimize` actually find the parameters the data was built around?
//!
//! Every other sweep test in the suite checks *shape* — a row per grid point,
//! the right columns, the right sort direction. None of them checks that the
//! answer is **right**, because on real bars nobody knows what right is. Here
//! the data is synthesised from a known parameter set, so the sweep has a
//! ground truth to be graded against.
//!
//! # The generating process
//!
//! Each bar carries an overlay column `edge` holding an integer **level**
//! `0..=6`, held for [`BLOCK`] bars at a time and stepping through the seven
//! levels in ascending order — one [`CYCLE`] of 42 bars, repeated [`CYCLES`]
//! times. The bar's return is a straight line in that level:
//!
//! ```text
//! return(level) = STEP · (level − pivot) · wobble
//! ```
//!
//! so every level **above** the pivot pays and every level **below** it costs,
//! by an amount proportional to the distance. `wobble` is a fixed ±15% ripple
//! that never changes a return's sign; it exists only so that windows and
//! members are not bit-identical, which is what `-w` and `--shrink` need in
//! order to have any dispersion to measure.
//!
//! The strategy is the obvious reader of that column — long at `edge >= HIGH`,
//! short at `edge <= LOW` — so with `pivot = 3.5` the parameters that take
//! exactly the paying bars and short exactly the costing ones are
//!
//! ```text
//! HIGH = 4,  LOW = 3
//! ```
//!
//! and that is the whole ground truth. It is an argmax by construction, not by
//! luck: raising `HIGH` to 5 gives up the positive level-4 bars, lowering it to
//! 3 takes on the negative level-3 ones, and the same argument mirrored holds
//! for `LOW`. Ranking is on `returns.total_pct` for the same reason — total
//! return is a sum over independently-signed bar contributions, so its
//! maximiser is exactly the parameter set that takes the positive bars and no
//! others. (A ratio like Sharpe recovers the truth here too, but only as an
//! observation; it is not forced to.)
//!
//! `LAG` is the one bit of engine convention baked in: an order submitted on
//! bar *t* fills at bar *t+1*'s open, and with flat OHLC that is bar *t+1*'s
//! close, so the decision taken on bar *t* earns bar *t+2*'s return. The
//! fixture does not lean on it —
//! [`the_recovery_does_not_depend_on_the_fill_lag`] rebuilds the same series
//! for every lag in `0..=4` and demands the same answer, so a change to the
//! fill convention cannot silently re-baseline this file.
//!
//! # What each mode is graded on
//!
//! Recovering the truth on clean data is table stakes — every mode does that,
//! and a file that only asserted it would pass on an implementation where the
//! flags did nothing. So each mode is instead given the failure it exists to
//! fix, and graded on whether it fixes it:
//!
//! | Mode | Exists to | Negative half | Positive half |
//! |---|---|---|---|
//! | `-w` | keep one unrepeatable disaster from deciding the sweep | a single −30% bar flips the whole-run answer to the wrong parameter | windowing confines it to one window of eight and the truth comes back |
//! | `--pooled` | average out what is idiosyncratic to one member | each member's own noise drags its private answer off the truth, in a different direction | the pooled mean has neither distortion and lands on the truth |
//! | `--shrink` | report a *distribution* of parameters, not a point | complete pooling collapses a two-answer panel onto one, wrong for half of it | partial pooling recovers each member's own truth — and stays a point mass when the panel really does agree |
//! | `--walkforward` | follow a process whose answer moves | a pivot that changes halfway leaves the whole-run fit on a compromise right for neither half | each fold re-fits on its own window and returns the regime it sits in |
//!
//! # The ablation is inside each test, not between them
//!
//! Every positive test above runs its own fixture **twice**, with and without
//! the flag, and asserts that dropping it gives the wrong answer. That is
//! deliberate: an assertion living in a sibling test proves nothing unless the
//! reader checks that both used the same series, the same grid and the same
//! ranking metric, and nothing enforces that they do. Keeping both invocations
//! in one test body makes "the flag is what moved the answer" a thing the suite
//! checks rather than a thing the comments claim.
//!
//! Each was then confirmed against a deliberately broken build — `-w` reduced
//! to a no-op, `pool_metric` averaging one member instead of all of them,
//! `opts.shrink` forced false, `walkforward_layout` handing every fold the
//! whole run. Each mutation fails that mode's tests and only that mode's (plus,
//! for `-w`, the two shrinkage tests, which need it for replication — `λ` is
//! not estimable without replicates, and that dependency is real).
//!
//! The one test that does **not** discriminate is
//! [`every_combination_of_the_reductions_lands_on_the_same_truth`], and its doc
//! comment says so: it runs on clean agreeing data where every mode works, and
//! exists to catch compositions that break, not to prove any flag is
//! load-bearing.

mod common;

use std::path::{Path, PathBuf};

use common::cli::{Cmd, Outcome, scratch_file, unique_path};

// ---------------------------------------------------------------- the process

/// Distinct `edge` levels, `0..=6`.
const LEVELS: usize = 7;
/// Bars each level is held for. Wide enough that a few bars of drift in the
/// fill convention cannot shift a whole block onto the neighbouring level —
/// which is what buys the tolerance
/// [`the_recovery_does_not_depend_on_the_fill_lag`] asserts.
const BLOCK: usize = 6;
/// One sweep through every level, ascending: 42 bars.
const CYCLE: usize = BLOCK * LEVELS;
/// Cycles per member — 336 bars, eight windows of one cycle each.
const CYCLES: usize = 8;
/// Per-bar return per unit of `edge` away from the pivot.
const STEP: f64 = 0.01;
/// Bars between the decision and the return it earns: submitted on `t`, filled
/// at `t+1`'s open, so `t+2`'s return is the one it takes home.
const LAG: usize = 2;
/// A sign-preserving ripple, applied per cycle. Its only job is to stop the
/// windows and the members from being identical — with zero dispersion there is
/// nothing for `-w` to replicate over and no residual for `λ` to divide by.
const WOBBLE: [f64; 5] = [1.0, 0.85, 1.15, 0.95, 1.05];

/// The pivot most members are built around, and the parameters it implies.
const PIVOT: f64 = 3.5;
const TRUE_HIGH: i64 = 4;
const TRUE_LOW: i64 = 3;

/// A second pivot, for the panel that genuinely has two answers.
const ALT_PIVOT: f64 = 1.5;
const ALT_HIGH: i64 = 2;
const ALT_LOW: i64 = 1;

/// A grid straddling both truths on both axes.
const GRID: &str = "HIGH=[2,3,4,5],LOW=[1,2,3,4]";
/// The narrower grid for the single-pivot fixtures.
const NARROW_GRID: &str = "HIGH=[3,4,5],LOW=[2,3,4]";
/// One window per cycle — eight replicates per member.
const WINDOW: &str = "42";

/// The `edge` level bar `i` carries.
fn level(i: usize) -> i64 {
    ((i / BLOCK) % LEVELS) as i64
}

/// One synthetic series.
struct Member {
    symbol: &'static str,
    /// Where this member's returns cross zero — the parameter to be recovered.
    pivot: f64,
    /// Offset into [`WOBBLE`], so members are not carbon copies.
    phase: usize,
    /// `(level, delta)` — an extra per-bar return on one level only. The
    /// idiosyncratic component `--pooled` exists to average away.
    distort: Option<(i64, f64)>,
    /// `(signal bar, return)` — one bar's return replaced outright. The
    /// unrepeatable disaster `-w` exists to quarantine.
    swan: Option<(usize, f64)>,
    /// `(bar, pivot)` — the process changes its answer partway through. The
    /// drift `--walkforward` exists to follow.
    shift: Option<(usize, f64)>,
    /// Bars between decision and return; [`LAG`] unless a test is varying it.
    lag: usize,
}

fn member(symbol: &'static str, pivot: f64, phase: usize) -> Member {
    Member {
        symbol,
        pivot,
        phase,
        distort: None,
        swan: None,
        shift: None,
        lag: LAG,
    }
}

impl Member {
    fn distort(mut self, level: i64, delta: f64) -> Self {
        self.distort = Some((level, delta));
        self
    }

    fn swan(mut self, at: usize, ret: f64) -> Self {
        self.swan = Some((at, ret));
        self
    }

    fn shift(mut self, at: usize, pivot: f64) -> Self {
        self.shift = Some((at, pivot));
        self
    }

    fn lag(mut self, lag: usize) -> Self {
        self.lag = lag;
        self
    }

    /// The return the decision taken on bar `j` earns.
    fn ret(&self, j: usize) -> f64 {
        if let Some((at, r)) = self.swan
            && at == j
        {
            return r;
        }
        let wobble = WOBBLE[(j / CYCLE + self.phase) % WOBBLE.len()];
        let level = level(j);
        let pivot = match self.shift {
            Some((at, pivot)) if j >= at => pivot,
            _ => self.pivot,
        };
        let extra = match self.distort {
            Some((l, d)) if l == level => d,
            _ => 0.0,
        };
        STEP * (level as f64 - pivot) * wobble + extra
    }

    /// `symbol,freq,time,open,high,low,close,volume,edge` rows. Flat OHLC, so a
    /// market order fills at the next bar's close and the arithmetic above is
    /// the arithmetic the engine performs.
    fn rows(&self) -> String {
        let bars = CYCLE * CYCLES;
        let mut close = 100.0_f64;
        let mut out = String::new();
        for i in 0..bars {
            if i >= self.lag {
                close *= 1.0 + self.ret(i - self.lag);
            }
            let t = 1_704_067_200_000_i64 + i as i64 * 86_400_000;
            let (s, e) = (self.symbol, level(i));
            out += &format!("{s},1d,{t},{close},{close},{close},{close},1000,{e}\n");
        }
        out
    }
}

fn frame(members: Vec<Member>) -> String {
    let mut out = String::from("symbol,freq,time,open,high,low,close,volume,edge\n");
    for m in &members {
        out += &m.rows();
    }
    out
}

/// The reader of the `edge` column: long above `HIGH`, short at or below `LOW`.
/// Deliberately the simplest thing that can express the ground truth — this
/// file is about the sweep, not about the strategy.
const DOC: &str = "\
root: !pick { symbol: !param SYM }
long:
  enter: !ge { lhs: !get { key: edge }, rhs: !param HIGH }
  exit:  !lt { lhs: !get { key: edge }, rhs: !param HIGH }
short:
  enter: !le { lhs: !get { key: edge }, rhs: !param LOW }
  exit:  !gt { lhs: !get { key: edge }, rhs: !param LOW }
sizing: !value 1.0
";

// ---------------------------------------------------------------- the harness

/// Run `optimize` over `csv`, ranking on `metric`, and return `(outcome, the
/// CSV it wrote)`. No cost model: frictionless keeps the argmax the one the
/// arithmetic above predicts.
fn sweep_by(name: &str, csv: &str, grid: &str, metric: &str, extra: &[&str]) -> (Outcome, PathBuf) {
    let (_frame, series) = scratch_file(&format!("{name}_series.csv"), csv);
    let (_doc, doc) = scratch_file(&format!("{name}_doc.yml"), DOC);
    let out = unique_path(name).with_extension("csv");
    let _ = std::fs::remove_file(&out);

    let outcome = Cmd::new("optimize")
        .arg(&doc)
        .series(&series)
        .args(&["--grid", grid])
        .args(&["--best-by", metric])
        .args(&["-m", metric])
        .args(&["--crypto"])
        .args(&["--output", &out.to_string_lossy()])
        .args(extra)
        .ok();
    (outcome, out)
}

/// [`sweep_by`] on `returns.total_pct` — the ranking the module doc justifies,
/// and what every test that is not specifically about a ratio uses.
fn sweep(name: &str, csv: &str, grid: &str, extra: &[&str]) -> (Outcome, PathBuf) {
    sweep_by(name, csv, grid, "returns.total_pct", extra)
}

fn read(path: &Path) -> (Vec<String>, Vec<Vec<String>>) {
    let text = std::fs::read_to_string(path)
        .unwrap_or_else(|e| panic!("optimize did not write {}: {e}", path.display()));
    let cells = |l: &str| l.split(',').map(str::to_string).collect::<Vec<_>>();
    let mut lines = text.lines().filter(|l| !l.is_empty());
    let header = cells(lines.next().unwrap_or_default());
    (header, lines.map(cells).collect())
}

/// The value each named axis takes in the sweep's **answer**. `--best-by` sorts
/// the file, so row 0 is the winner.
fn winner_cells(path: &Path, axes: &[&str]) -> Vec<String> {
    let (header, rows) = read(path);
    let row = rows
        .first()
        .unwrap_or_else(|| panic!("{} has no rows", path.display()));
    axes.iter()
        .map(|axis| {
            let at = header
                .iter()
                .position(|c| c == axis)
                .unwrap_or_else(|| panic!("no `{axis}` column in {header:?}"));
            row[at].clone()
        })
        .collect()
}

fn winner(path: &Path, axes: &[&str]) -> Vec<i64> {
    winner_cells(path, axes)
        .iter()
        .map(|cell| {
            cell.parse()
                .unwrap_or_else(|e| panic!("axis cell `{cell}` is not an integer: {e}"))
        })
        .collect()
}

/// `(HIGH, LOW) -> the ranked metric`, over every row of the sweep.
fn surface(path: &Path) -> std::collections::HashMap<(i64, i64), f64> {
    let (header, rows) = read(path);
    let at = |name: &str| header.iter().position(|c| c == name).expect("column");
    let (h, l, m) = (at("HIGH"), at("LOW"), at("returns.total_pct"));
    rows.iter()
        .map(|r| {
            (
                (r[h].parse().unwrap(), r[l].parse().unwrap()),
                r[m].parse().unwrap(),
            )
        })
        .collect()
}

/// The per-member picks `--shrink` writes when the panel splits, as
/// `(member, HIGH, LOW)`. `None` when no member departed — the file is only
/// written when there is a departure to report.
fn member_winners(path: &Path) -> Option<Vec<(String, i64, i64)>> {
    let stem = path.file_stem().unwrap().to_string_lossy().into_owned();
    let file = path.with_file_name(format!("{stem}.member_winners.csv"));
    if !file.exists() {
        return None;
    }
    let (header, rows) = read(&file);
    assert_eq!(header[0], "member", "per-member file is keyed by member");
    let at = |name: &str| header.iter().position(|c| c == name).expect("axis column");
    let (h, l) = (at("HIGH"), at("LOW"));
    Some(
        rows.iter()
            .map(|r| (r[0].clone(), r[h].parse().unwrap(), r[l].parse().unwrap()))
            .collect(),
    )
}

/// One clean member built around [`PIVOT`].
fn clean() -> String {
    frame(vec![member("AAA", PIVOT, 0)])
}

/// Three clean members that share [`PIVOT`], differing only in ripple phase.
fn agreeing_panel() -> String {
    frame(vec![
        member("AAA", PIVOT, 0),
        member("BBB", PIVOT, 2),
        member("CCC", PIVOT, 4),
    ])
}

const PANEL: &str = "SYM=[\"AAA\",\"BBB\",\"CCC\"]";
const WIDE_PANEL: &str = "SYM=[\"AAA\",\"BBB\",\"CCC\",\"DDD\"]";

// ------------------------------------------------------- the truth is findable

/// The baseline everything else is measured against: on clean data the plain
/// sweep returns the parameters the series was generated from.
///
/// The neighbour check is the part that matters. An argmax alone would also be
/// produced by a flat surface with a rounding accident at the top; requiring
/// each of the four adjacent grid points to score *strictly* less says the peak
/// is where the arithmetic put it and the surface falls away from it on every
/// side.
#[test]
fn a_plain_sweep_finds_the_parameters_the_data_was_built_around() {
    let (_out, csv) = sweep(
        "recover_plain",
        &clean(),
        GRID,
        &["--params", "SYM=\"AAA\""],
    );

    assert_eq!(
        winner(&csv, &["HIGH", "LOW"]),
        vec![TRUE_HIGH, TRUE_LOW],
        "the sweep must return the parameters the series was synthesised from"
    );

    let surface = surface(&csv);
    let peak = surface[&(TRUE_HIGH, TRUE_LOW)];
    for (dh, dl) in [(-1, 0), (1, 0), (0, -1), (0, 1)] {
        let neighbour = (TRUE_HIGH + dh, TRUE_LOW + dl);
        let score = surface[&neighbour];
        assert!(
            score < peak,
            "one grid step to {neighbour:?} must score strictly worse than the truth \
             ({score} vs {peak}) — a peak with a flat shoulder is not a recovered parameter"
        );
    }
}

/// The fixture must not be a hostage to the fill convention.
///
/// The generator has to decide which bar's return a decision earns, and that is
/// engine convention (`docs/TRADING.md`: nothing fills on the bar that caused
/// it). Rebuilding the same series under every lag in `0..=4` and demanding the
/// same answer says the recovery survives the convention moving — so a future
/// change to it fails on its own tests rather than quietly turning this file
/// into a golden master of the old behaviour.
#[test]
fn the_recovery_does_not_depend_on_the_fill_lag() {
    for lag in 0..=4 {
        let csv = frame(vec![member("AAA", PIVOT, 0).lag(lag)]);
        let (_out, out) = sweep(
            &format!("recover_lag{lag}"),
            &csv,
            NARROW_GRID,
            &["--params", "SYM=\"AAA\""],
        );
        assert_eq!(
            winner(&out, &["HIGH", "LOW"]),
            vec![TRUE_HIGH, TRUE_LOW],
            "the recovery must survive a {lag}-bar decision-to-return lag"
        );
    }
}

// ------------------------------------------------- `-w`: quarantine the outlier

/// The `edge` level the swan is buried in, and where in the run it lands.
///
/// Level 4 is the lowest level `HIGH = 4` is long through and the highest one
/// `HIGH = 5` sits out, so the disaster falls on the true parameter and spares
/// the one just above it. `LOW` is pinned at its own truth so the short side
/// stays out of the story.
const SWAN_AT: usize = 3 * CYCLE + 4 * BLOCK + 2;
const SWAN_RETURN: f64 = -0.30;
const SWAN_GRID: &str = "HIGH=[3,4,5,6]";
const SWAN_PARAMS: &str = "SYM=\"AAA\",LOW=3";

fn swan_frame() -> String {
    frame(vec![member("AAA", PIVOT, 0).swan(SWAN_AT, SWAN_RETURN)])
}

/// **The failure `-w` exists for.** One −30% bar, on 336, is enough to hand the
/// whole-run sweep to the wrong parameter.
///
/// This is the negative half and it has to be asserted, not assumed: if the
/// undisturbed fixture and the disturbed one gave the same answer there would
/// be nothing for windowing to rescue, and the test below would pass on an
/// implementation where `-w` was a no-op. So both halves run here — the same
/// series without the swan still recovers the truth, and adding the single bar
/// is what moves it.
#[test]
fn a_black_swan_captures_the_whole_run_sweep() {
    let extra = &["--params", SWAN_PARAMS];

    let (_out, clean_csv) = sweep_by("recover_swan_control", &clean(), SWAN_GRID, "sharpe", extra);
    assert_eq!(
        winner(&clean_csv, &["HIGH"]),
        vec![TRUE_HIGH],
        "without the swan the whole-run sweep finds the truth — the fixture is sound"
    );

    let (_out, swan_csv) = sweep_by(
        "recover_swan_whole",
        &swan_frame(),
        SWAN_GRID,
        "sharpe",
        extra,
    );
    let captured = winner(&swan_csv, &["HIGH"])[0];
    assert_ne!(
        captured, TRUE_HIGH,
        "one catastrophic bar must be enough to move a whole-run ranking — otherwise \
         there is nothing here for `-w` to fix"
    );
    assert_eq!(
        captured,
        TRUE_HIGH + 1,
        "and it must move it to the parameter that happened to sit the disaster out"
    );
}

/// **What `-w` buys.** The same fixture, reduced window by window, comes back
/// to the truth.
///
/// A whole-run ratio lets one bar into every other bar's denominator: the
/// swan's contribution to the run's stddev is paid by all 336 bars, so the
/// parameter that was long through it is punished everywhere. Cutting the run
/// into eight windows confines the damage to the one window it happened in, and
/// the other seven — where the true parameter is simply better — carry the
/// mean. That is the entire argument for windowed reduction, and this is the
/// test of it.
#[test]
fn windowing_quarantines_the_black_swan_to_one_window() {
    let frame = swan_frame();

    // The ablation, inline: the same fixture and the same grid, `-w` the only
    // difference between the two invocations. Asserted here rather than left to
    // the sibling test above so that this test alone establishes that `-w` is
    // what moved the answer — not the fixture, not the ranking metric.
    let (_out, without) = sweep_by(
        "recover_swan_ablate_off",
        &frame,
        SWAN_GRID,
        "sharpe",
        &["--params", SWAN_PARAMS],
    );
    let (_out, with) = sweep_by(
        "recover_swan_ablate_on",
        &frame,
        SWAN_GRID,
        "sharpe",
        &["--params", SWAN_PARAMS, "-w", WINDOW],
    );

    assert_ne!(
        winner(&without, &["HIGH"]),
        vec![TRUE_HIGH],
        "drop `-w` and the swan must take the sweep with it — otherwise this fixture \
         proves nothing about windowing"
    );
    assert_eq!(
        winner(&with, &["HIGH"]),
        vec![TRUE_HIGH],
        "windowing must keep a one-window disaster from deciding the whole sweep"
    );
}

/// `-k` charges the swan back against the true parameter — by design, and worth
/// pinning so nobody reads it as a bug.
///
/// `--risk-aversion` ranks on `mean − k·std` across windows, and a black swan is
/// *precisely* cross-window dispersion. So the flag undoes some of what `-w`
/// just bought: it cannot tell "one unrepeatable disaster" from "this parameter
/// is erratic", and it is not supposed to — the two are the same measurement.
/// The lesson the test encodes is that `-k` is a statement about which risk you
/// are willing to hold, not a free improvement, and that a recovery run wants
/// `k = 0`.
#[test]
fn risk_aversion_charges_the_swan_back_against_the_true_parameter() {
    let (_out, csv) = sweep_by(
        "recover_swan_k",
        &swan_frame(),
        SWAN_GRID,
        "sharpe",
        &["--params", SWAN_PARAMS, "-w", WINDOW, "-k", "1"],
    );
    assert_eq!(
        winner(&csv, &["HIGH"]),
        vec![TRUE_HIGH + 1],
        "penalising dispersion must penalise the parameter that took the swan — `-k` \
         re-prices the outlier as risk rather than quarantining it"
    );
}

// ------------------------------------------- `--pooled`: average the members

/// An extra return on one level only, sized to sit in the gap that makes this
/// fixture work: big enough (`> STEP/2`) to flip the sign of the level it lands
/// on and drag that member's private answer one grid step, small enough
/// (`< STEP`) that halving it across the two members leaves both signs as the
/// generator set them.
const DISTORTION: f64 = 0.0075;

/// Two members with the same pivot and opposite idiosyncrasies: `AAA` is paid
/// for level 3, which its own sweep reads as "go one step lower", and `BBB` is
/// taxed on level 4, which its own sweep reads as "go one step higher".
fn distorted_panel() -> String {
    frame(vec![
        member("AAA", PIVOT, 0).distort(TRUE_LOW, DISTORTION),
        member("BBB", PIVOT, 2).distort(TRUE_HIGH, -DISTORTION),
    ])
}

/// **The failure `--pooled` exists for.** Fit each member on its own evidence
/// and each one confidently returns a different wrong answer.
///
/// Both members were generated around the same pivot; the only thing separating
/// their answers is noise local to one of them. This is what a plain `SYM=[...]`
/// grid axis gives you — `N` members, `N` answers, each fit on its share of the
/// evidence and each overfit to whatever that share happened to contain.
#[test]
fn no_member_of_the_pool_finds_the_parameters_on_its_own() {
    let panel = distorted_panel();
    for (symbol, expected) in [
        ("AAA", vec![TRUE_HIGH - 1, TRUE_LOW - 1]),
        ("BBB", vec![TRUE_HIGH + 1, TRUE_LOW + 1]),
    ] {
        let (_out, csv) = sweep(
            &format!("recover_solo_{symbol}"),
            &panel,
            GRID,
            &["--params", &format!("SYM=\"{symbol}\"")],
        );
        let alone = winner(&csv, &["HIGH", "LOW"]);
        assert_ne!(
            alone,
            vec![TRUE_HIGH, TRUE_LOW],
            "{symbol} fit alone must miss the truth — otherwise pooling has nothing to fix"
        );
        assert_eq!(
            alone, expected,
            "{symbol}'s own noise must drag its answer one step in its own direction"
        );
    }
}

/// **What `--pooled` buys.** One parameter set fit across the panel: the
/// distortions are equal and opposite, so the pooled mean has neither, and the
/// answer is the pivot both members were actually generated from.
///
/// This is the claim pooling rests on — that the member-specific part of a
/// backtest score averages toward nothing while the part driven by the shared
/// process does not — stated as a fixture where "averages toward nothing" is
/// arithmetic rather than hope.
#[test]
fn pooling_averages_the_members_idiosyncrasies_away() {
    let panel = distorted_panel();

    // The ablation. Dropping `--pooled` does not mean "sweep one member" — it
    // means the member axis stays an ordinary grid axis, so the sweep ranks
    // `(parameters, member)` cells against each other and its answer names a
    // *series*. That is the failure in its most honest form: two members and a
    // 16-point grid is 32 hypotheses dressed up as 16, and the cell that wins
    // is the one whose noise was kindest.
    let (unpooled_out, unpooled) = sweep(
        "recover_unpooled",
        &panel,
        &format!("{GRID},SYM=[\"AAA\",\"BBB\"]"),
        &[],
    );
    assert_eq!(
        winner_cells(&unpooled, &["HIGH", "LOW", "SYM"]),
        vec!["3", "2", "AAA"],
        "without `--pooled` the winner is a (parameters, series) cell chosen on one \
         member's idiosyncrasy — the wrong parameters, and a member picked as if that \
         were an answer"
    );
    assert!(
        unpooled_out.stderr.contains("--pooled"),
        "and the run must say so — a member axis left in the grid makes the rows \
         incomparable:\n{}",
        unpooled_out.stderr
    );

    let (out, csv) = sweep(
        "recover_pooled",
        &panel,
        GRID,
        &["--pooled", "SYM=[\"AAA\",\"BBB\"]"],
    );
    assert_eq!(
        winner(&csv, &["HIGH", "LOW"]),
        vec![TRUE_HIGH, TRUE_LOW],
        "the pooled sweep must recover the pivot both members share, which neither of \
         them recovers alone"
    );
    assert!(
        !read(&csv).0.iter().any(|c| c == "SYM"),
        "and the member axis must be reduced over, not ranked on — no `SYM` column"
    );
    assert!(
        out.stdout.contains("2 of 2") || out.stdout.contains("(2/2)"),
        "and it must say it pooled over both members, not silently drop one:\n{}",
        out.stdout
    );
}

// --------------------------------------- `--shrink`: a distribution, not a point

/// Two members around [`PIVOT`] and two around [`ALT_PIVOT`] — a panel with two
/// genuinely different right answers, `(4, 3)` and `(2, 1)`.
fn split_panel() -> String {
    frame(vec![
        member("AAA", PIVOT, 0),
        member("BBB", PIVOT, 2),
        member("CCC", ALT_PIVOT, 1),
        member("DDD", ALT_PIVOT, 3),
    ])
}

/// Four members around [`PIVOT`] — a panel whose answer really is a single
/// point.
fn unsplit_panel() -> String {
    frame(vec![
        member("AAA", PIVOT, 0),
        member("BBB", PIVOT, 2),
        member("CCC", PIVOT, 1),
        member("DDD", PIVOT, 3),
    ])
}

/// **The failure `--shrink` exists for.** Complete pooling has one slot to put
/// an answer in, so a panel holding two of them loses one.
///
/// Each half of this panel recovers its own pivot when fit alone — asserted
/// here, so the disagreement is established as real rather than assumed. Pooled
/// completely, the sweep still emits a single parameter set, and being a single
/// number it cannot be right for both halves: whichever way it lands, half the
/// panel is being handed parameters generated from the other half's process.
#[test]
fn complete_pooling_collapses_a_panel_that_has_two_answers() {
    let panel = split_panel();
    for (symbol, expected) in [
        ("AAA", vec![TRUE_HIGH, TRUE_LOW]),
        ("CCC", vec![ALT_HIGH, ALT_LOW]),
    ] {
        let (_out, csv) = sweep(
            &format!("recover_split_solo_{symbol}"),
            &panel,
            GRID,
            &["--params", &format!("SYM=\"{symbol}\"")],
        );
        assert_eq!(
            winner(&csv, &["HIGH", "LOW"]),
            expected,
            "{symbol} alone must recover its own pivot — the halves really do disagree"
        );
    }

    let (_out, csv) = sweep(
        "recover_split_pooled",
        &panel,
        GRID,
        &["--pooled", WIDE_PANEL],
    );
    let (_header, rows) = read(&csv);
    let pooled = winner(&csv, &["HIGH", "LOW"]);
    assert_eq!(
        rows.len(),
        16,
        "one row per parameter set, the member axis reduced over"
    );
    let wrong_for = [
        (vec![TRUE_HIGH, TRUE_LOW], "AAA/BBB"),
        (vec![ALT_HIGH, ALT_LOW], "CCC/DDD"),
    ]
    .into_iter()
    .filter(|(truth, _)| *truth != pooled)
    .count();
    assert!(
        wrong_for > 0,
        "complete pooling returned {pooled:?}, which cannot be the right answer for \
         both halves of a panel built around two different pivots"
    );
}

/// **What `--shrink` buys.** Not one parameter set but the panel's *distribution*
/// of them — and on this fixture the distribution it reports is exactly the one
/// the members were generated from.
///
/// Two modes, at the two pivots, with the right members at each. That is the
/// strongest form of this file's claim: complete pooling recovers one truth,
/// no pooling recovers four noisy ones, and partial pooling recovers the two
/// that are actually there.
#[test]
fn shrinkage_recovers_the_panels_parameter_distribution() {
    let panel = split_panel();

    // The ablation: the identical run with `--shrink` removed. It still pools,
    // still replicates, still ranks the same grid — and it still hands back one
    // parameter set and no distribution, because a single answer is all
    // complete pooling has to give. Everything the test below asserts is
    // therefore attributable to `--shrink` and to nothing else in the stack.
    let (_out, unshrunk) = sweep(
        "recover_shrink_ablate",
        &panel,
        GRID,
        &["--pooled", WIDE_PANEL, "-w", WINDOW],
    );
    assert!(
        member_winners(&unshrunk).is_none(),
        "without `--shrink` there is no per-member distribution at all — one answer for \
         the panel, and half the panel was not generated from it"
    );

    let (out, csv) = sweep(
        "recover_shrink_split",
        &panel,
        GRID,
        &["--pooled", WIDE_PANEL, "-w", WINDOW, "--shrink"],
    );

    assert!(
        out.stdout.contains("the members are separate problems"),
        "a panel with two pivots must read as disagreement, not as noise:\n{}",
        out.stdout
    );

    let mut winners = member_winners(&csv)
        .expect("a split panel must write its per-member picks — that file is the distribution");
    winners.sort();
    assert_eq!(
        winners,
        vec![
            ("SYM=AAA".into(), TRUE_HIGH, TRUE_LOW),
            ("SYM=BBB".into(), TRUE_HIGH, TRUE_LOW),
            ("SYM=CCC".into(), ALT_HIGH, ALT_LOW),
            ("SYM=DDD".into(), ALT_HIGH, ALT_LOW),
        ],
        "each member must be given back the parameters its own series was built from"
    );
}

/// The other half of "a distribution": when the panel really does agree, the
/// distribution is a point mass and `--shrink` says so.
///
/// A shrinkage implementation that always split the panel would pass the test
/// above and be useless — it would manufacture per-member parameters out of
/// backtest noise, which is the failure mode partial pooling exists to avoid.
/// Four members, one pivot: `λ` must read as agreement, no per-member file may
/// be written, and the single answer must be the truth.
#[test]
fn shrinkage_does_not_manufacture_a_spread_that_is_not_there() {
    let (out, csv) = sweep(
        "recover_shrink_unsplit",
        &unsplit_panel(),
        GRID,
        &["--pooled", WIDE_PANEL, "-w", WINDOW, "--shrink"],
    );

    assert!(
        out.stdout.contains("the members agree"),
        "a panel built around one pivot must read as agreement:\n{}",
        out.stdout
    );
    assert!(
        member_winners(&csv).is_none(),
        "no member departed, so there is no per-member distribution to write — a file \
         of four identical rows reads like a finding when it is the absence of one"
    );
    assert_eq!(
        winner(&csv, &["HIGH", "LOW"]),
        vec![TRUE_HIGH, TRUE_LOW],
        "and the one answer it does report is the pivot every member shares"
    );
}

// ------------------------------------------------------------ the combinations

/// Every valid stack of the reductions, on clean agreeing data, lands on the
/// same truth.
///
/// **This one does not discriminate, and is not meant to.** Its fixture is clean
/// and agreeing, so every combination reaches the truth and it would pass on an
/// implementation where all three flags were no-ops. What establishes that each
/// flag is load-bearing is the ablation inside each mode's own test, where the
/// same fixture is run with and without it. What *this* catches is the thing
/// those cannot: a composition that breaks an answer neither flag breaks alone
/// — a windowed reduction that stops being carried beside the pooled document,
/// a shrink that re-ranks on a demeaned score whose sign convention drifted.
///
/// `--shrink` requires a panel and a ranking key, so the two combinations that
/// omit them are not reachable; the other six are all here.
#[test]
fn every_combination_of_the_reductions_lands_on_the_same_truth() {
    let solo: &[&str] = &["--params", "SYM=\"AAA\""];
    let combinations: [(&str, Vec<&str>); 6] = [
        ("direct", solo.to_vec()),
        ("windowed", [solo, &["-w", WINDOW]].concat()),
        ("pooled", vec!["--pooled", PANEL]),
        ("pooled_windowed", vec!["--pooled", PANEL, "-w", WINDOW]),
        ("pooled_shrunk", vec!["--pooled", PANEL, "--shrink"]),
        (
            "pooled_windowed_shrunk",
            vec!["--pooled", PANEL, "-w", WINDOW, "--shrink"],
        ),
    ];

    let panel = agreeing_panel();
    for (name, extra) in combinations {
        let (_out, csv) = sweep(
            &format!("recover_combo_{name}"),
            &panel,
            NARROW_GRID,
            &extra,
        );
        assert_eq!(
            winner(&csv, &["HIGH", "LOW"]),
            vec![TRUE_HIGH, TRUE_LOW],
            "the `{name}` reduction must reach the same truth as every other"
        );
    }
}

// ------------------------------- `--walkforward`: follow the process as it moves

/// Where the generating process changes its mind — halfway, at bar 168.
const SHIFT: usize = 4 * CYCLE;

/// One member whose pivot moves from [`PIVOT`] to [`ALT_PIVOT`] at [`SHIFT`]:
/// the right parameters are `(4, 3)` for the first half of the run and
/// `(2, 1)` for the second, and there is no single answer for the whole of it.
fn regime_shift() -> String {
    frame(vec![member("AAA", PIVOT, 0).shift(SHIFT, ALT_PIVOT)])
}

/// **The failure `--walkforward` exists for.** A process that changes its
/// parameters cannot be fit by one number, and a whole-run sweep has only one
/// number to offer.
///
/// The sweep does not fail loudly here — that is the point. It returns
/// `(3, 2)`, an average of two regimes that is the right answer to neither, and
/// it returns it with exactly the same confidence it would give a stationary
/// series. Established as its own test because the fold assertions below are
/// worth nothing unless the single-shot answer is genuinely wrong.
#[test]
fn a_whole_run_sweep_cannot_follow_a_regime_change() {
    let (_out, csv) = sweep(
        "recover_regime_whole",
        &regime_shift(),
        GRID,
        &["--params", "SYM=\"AAA\""],
    );
    let single = winner(&csv, &["HIGH", "LOW"]);
    assert_ne!(
        single,
        vec![TRUE_HIGH, TRUE_LOW],
        "a whole-run fit must not land on the first regime's answer"
    );
    assert_ne!(
        single,
        vec![ALT_HIGH, ALT_LOW],
        "nor on the second's — one fit over two regimes is a compromise between them"
    );
}

/// **What `--walkforward` buys.** Each fold re-fits on its own in-sample
/// window, so a fold that sits inside one regime returns that regime's
/// parameters instead of the run-wide average.
///
/// Folds are graded by where their *in-sample* window falls, not by index: one
/// entirely before [`SHIFT`] must return the early truth, one entirely after it
/// the late truth. A fold straddling the change is not graded — it is fitting
/// two processes at once and the compromise it returns is the correct answer to
/// the question it was asked. Both classes have to be non-empty, or the layout
/// drifted and the test stopped testing anything.
#[test]
fn walkforward_refits_each_fold_to_the_regime_it_sits_in() {
    let (_out, csv) = sweep(
        "recover_regime_folds",
        &regime_shift(),
        GRID,
        &["--params", "SYM=\"AAA\"", "--walkforward", "84,42"],
    );

    let (header, rows) = read(&csv);
    let at = |name: &str| header.iter().position(|c| c == name).expect("column");
    let (start, end, h, l) = (at("is_start"), at("is_end"), at("HIGH"), at("LOW"));
    let num = |row: &Vec<String>, i: usize| row[i].parse::<usize>().unwrap();

    let (mut early, mut late) = (0, 0);
    for row in &rows {
        let picked = (
            row[h].parse::<i64>().unwrap(),
            row[l].parse::<i64>().unwrap(),
        );
        if num(row, end) <= SHIFT {
            early += 1;
            assert_eq!(
                picked,
                (TRUE_HIGH, TRUE_LOW),
                "fold {} fits entirely inside the first regime and must return its \
                 parameters",
                row[0]
            );
        } else if num(row, start) >= SHIFT {
            late += 1;
            assert_eq!(
                picked,
                (ALT_HIGH, ALT_LOW),
                "fold {} fits entirely inside the second regime and must return its \
                 parameters",
                row[0]
            );
        }
    }
    assert!(
        early > 0 && late > 0,
        "the layout must put at least one fold wholly inside each regime \
         (got {early} early, {late} late) — otherwise nothing above was graded"
    );
}

/// The fifth reduction under the full stack: walk-forward, which re-picks per
/// fold rather than once.
///
/// The discrimination for `--walkforward` lives in the regime-change pair
/// above; on a stationary panel every fold has the same right answer, so what
/// this adds is that pooling and shrinking *underneath* the fold decision does
/// not disturb it.
///
/// Every fold sees a different slice of the same process, so every fold has the
/// same right answer — and the fold CSV, not the console, is where that is
/// checked, because the fold's chosen parameters are what an out-of-sample
/// composite is actually built from. Pooled and shrunk on top, so the fold
/// decision is the full stack rather than a plain sweep in disguise.
#[test]
fn a_pooled_walkforward_picks_the_true_parameters_in_every_fold() {
    let (_out, csv) = sweep(
        "recover_walkforward",
        &agreeing_panel(),
        NARROW_GRID,
        &["--pooled", PANEL, "--walkforward", "126,42", "--shrink"],
    );

    let (header, rows) = read(&csv);
    let at = |name: &str| header.iter().position(|c| c == name).expect("axis column");
    let (h, l) = (at("HIGH"), at("LOW"));
    assert!(
        rows.len() >= 2,
        "the walk-forward produced too few folds to be a test"
    );
    for row in &rows {
        assert_eq!(
            (
                row[h].parse::<i64>().unwrap(),
                row[l].parse::<i64>().unwrap()
            ),
            (TRUE_HIGH, TRUE_LOW),
            "fold {} chose parameters the process never had",
            row[0]
        );
    }
}
