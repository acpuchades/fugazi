//! End-to-end tests of the `fugazi optimize` binary.
//!
//! `src/cli/optimize.rs` is ~1,400 lines and had **no tests of any kind**. The
//! sweep *kernel* (`src/spec/optimize.rs`) is well covered by unit tests, and
//! `python/tests/test_specs.py` drives `ta.optimize`, so the hole was precisely
//! the CLI layer: grid parsing, subgrid stacking, `--best-by` direction and
//! ranking, metric column selection, and CSV emission.
//!
//! (`tests/common/cli.rs`'s doc comment names `optimize` as an example
//! subcommand its `Cmd` builder supports, which reads like coverage and wasn't.)

mod common;

use common::cli::{Cmd, at, scratch_file};

/// A sweepable always-in crossover: both periods are `!param` placeholders, so
/// `--grid` can drive them. Deliberately inline rather than an `examples/` file
/// — per `docs/TESTING.md`, shapes are shared but series constants are not, and
/// a test that pins grid arithmetic shouldn't break when an example is retuned.
const SWEEPABLE: &str = "\
symbol: BTC
long:
  enter: !crosses_above
    lhs: !sma { source: close, period: !param FAST }
    rhs: !sma { source: close, period: !param SLOW }
  exit: !crosses_below
    lhs: !sma { source: close, period: !param FAST }
    rhs: !sma { source: close, period: !param SLOW }
";

/// `fugazi optimize` over `examples/candles.csv`, writing `grid.csv` into a
/// fresh scratch dir. Returns the outcome plus the CSV path so callers can read
/// it back.
fn sweep(name: &str, extra: &[&str]) -> (common::cli::Outcome, String) {
    let (path, _) = scratch_file(&format!("{name}_strategy.yml"), SWEEPABLE);
    let out_csv = common::cli::unique_path(name).with_extension("csv");
    let out_str = out_csv.to_string_lossy().into_owned();
    let _ = std::fs::remove_file(&out_csv);

    let outcome = Cmd::new("optimize")
        .arg(&format!("@{}", path.display()))
        .series(&at("examples/candles.csv"))
        .args(&["--output", &out_str])
        .args(extra)
        .ok();
    (outcome, out_str)
}

fn read_csv(path: &str) -> Vec<String> {
    std::fs::read_to_string(path)
        .unwrap_or_else(|e| panic!("optimize did not write {path}: {e}"))
        .lines()
        .map(str::to_string)
        .collect()
}

#[test]
fn a_two_axis_grid_emits_one_row_per_combination() {
    // 3 × 2 = 6 points, plus a header.
    let (_, csv) = sweep(
        "fugazi_opt_grid",
        &["--grid", "FAST=[2,3,4],SLOW=[6,8]", "--metrics", "sharpe"],
    );
    let lines = read_csv(&csv);
    assert_eq!(lines.len(), 7, "header + 6 grid points, got:\n{}", lines.join("\n"));

    let header = &lines[0];
    for axis in ["FAST", "SLOW"] {
        assert!(header.contains(axis), "axis `{axis}` missing from header: {header}");
    }
    assert!(
        header.contains("sharpe"),
        "the requested metric column is missing: {header}"
    );
}

#[test]
fn a_range_axis_expands_inclusively() {
    // `2..6:2` is 2, 4, 6 — inclusive of the endpoint, which is the documented
    // behaviour and the easiest thing to get off-by-one.
    let (_, csv) = sweep(
        "fugazi_opt_range",
        &["--grid", "FAST=2..6:2,SLOW=[10]", "--metrics", "sharpe"],
    );
    assert_eq!(read_csv(&csv).len(), 4, "header + 3 points");
}

#[test]
fn stacked_subgrids_are_a_union_not_a_product() {
    // Two `--grid` flags stack as a *union of Cartesian products*: 2 + 3 = 5,
    // not 2 × 3 = 6. This is the whole reason subgrids exist (a parameter that
    // only makes sense conditionally on another), so it's worth pinning.
    let (_, csv) = sweep(
        "fugazi_opt_subgrids",
        &[
            "--grid",
            "FAST=[2,3],SLOW=[9]",
            "--grid",
            "FAST=[4],SLOW=[10,11,12]",
            "--metrics",
            "sharpe",
        ],
    );
    assert_eq!(read_csv(&csv).len(), 6, "header + (2 + 3) points");
}

#[test]
fn params_supply_a_baseline_under_every_subgrid() {
    // `--params` sets scalars shared by every subgrid; only the `--grid` axes
    // vary. One axis of 2 => 2 rows, with SLOW pinned by --params.
    let (_, csv) = sweep(
        "fugazi_opt_params",
        &["--params", "SLOW=9", "--grid", "FAST=[2,3]", "--metrics", "sharpe"],
    );
    let lines = read_csv(&csv);
    assert_eq!(lines.len(), 3, "header + 2 points");
    // A `--params` scalar is not a sweep axis, so it earns no column.
    assert!(
        !lines[0].split(',').any(|c| c == "SLOW"),
        "a fixed --params scalar should not become an axis column: {}",
        lines[0]
    );
}

#[test]
fn a_sweep_axis_in_params_is_refused() {
    // The grammars overlap deliberately, so the CLI has to reject a list in
    // `--params` rather than silently sweeping or silently taking one value.
    let (path, _) = scratch_file("fugazi_opt_reject_strategy.yml", SWEEPABLE);
    let out = Cmd::new("optimize")
        .arg(&format!("@{}", path.display()))
        .series(&at("examples/candles.csv"))
        .args(&["--params", "FAST=[2,3]", "--grid", "SLOW=[9]"])
        .args(&["--output", "/dev/null"])
        .fails();
    assert!(
        out.stderr.contains("FAST") || out.stderr.contains("grid"),
        "the refusal should name the axis or point at --grid, got: {}",
        out.stderr
    );
}

#[test]
fn best_by_sorts_descending_for_a_higher_is_better_metric() {
    // `--best-by` direction is hardcoded per metric. Sharpe is higher-is-better,
    // so row 1 must hold the maximum — a flipped direction would silently
    // report the *worst* combination as the winner.
    let (out, csv) = sweep(
        "fugazi_opt_best",
        &[
            "--grid",
            "FAST=[2,3,4],SLOW=[6,8]",
            "--metrics",
            "sharpe",
            "--best-by",
            "sharpe",
        ],
    );
    let lines = read_csv(&csv);
    let col = lines[0]
        .split(',')
        .position(|c| c.ends_with("sharpe"))
        .unwrap_or_else(|| panic!("no sharpe column in {}", lines[0]));

    let values: Vec<f64> = lines[1..]
        .iter()
        .filter_map(|l| l.split(',').nth(col)?.parse::<f64>().ok())
        .collect();
    assert!(values.len() >= 2, "need at least two comparable rows: {values:?}");
    assert!(
        values.windows(2).all(|w| w[0] >= w[1]),
        "rows are not sorted best-first by sharpe: {values:?}"
    );

    // The winner is also announced on stdout.
    assert!(
        out.stdout.contains("sharpe") || out.stdout.contains("best"),
        "expected a winner block on stdout, got:\n{}",
        out.stdout
    );
}

#[test]
fn an_unrankable_best_by_target_is_refused() {
    // A metric with no defined direction cannot rank a grid. Better to refuse
    // than to pick an arbitrary order and call one row the winner.
    let (path, _) = scratch_file("fugazi_opt_unrankable_strategy.yml", SWEEPABLE);
    let out = Cmd::new("optimize")
        .arg(&format!("@{}", path.display()))
        .series(&at("examples/candles.csv"))
        .args(&["--grid", "FAST=[2,3],SLOW=[9]"])
        .args(&["--best-by", "not_a_metric"])
        .args(&["--output", "/dev/null"])
        .fails();
    assert!(
        out.stderr.contains("not_a_metric"),
        "the refusal should name the unknown metric, got: {}",
        out.stderr
    );
}

#[test]
fn omitting_metrics_emits_the_whole_catalogue() {
    // Documented behaviour: no `--metrics` means every metric gets a column.
    // At least two points: a single-point grid is refused outright ("use `run`
    // for a single combination"), which is the right call and is pinned below.
    let (_, csv) = sweep("fugazi_opt_all_metrics", &["--grid", "FAST=[2,3],SLOW=[9]"]);
    let lines = read_csv(&csv);
    let columns = lines[0].split(',').count();
    assert!(
        columns > 20,
        "expected the full catalogue as columns, got {columns}: {}",
        lines[0]
    );
}

#[test]
fn a_single_point_grid_is_refused_and_points_at_run() {
    // A grid of one is a backtest, not a sweep. Refusing keeps `optimize`'s
    // output contract honest (one row per *combination*) and tells the user
    // which command they actually wanted.
    let (path, _) = scratch_file("fugazi_opt_one_point_strategy.yml", SWEEPABLE);
    let out = Cmd::new("optimize")
        .arg(&format!("@{}", path.display()))
        .series(&at("examples/candles.csv"))
        .args(&["--grid", "FAST=[2],SLOW=[9]"])
        .args(&["--output", "/dev/null"])
        .fails();
    assert!(
        out.stderr.contains("only 1 point"),
        "expected the single-point refusal, got: {}",
        out.stderr
    );
    assert!(
        out.stderr.contains("run"),
        "the refusal should name `run` as the alternative, got: {}",
        out.stderr
    );
}

// ---------------------------------------------------------------------------
// A sweep where nothing traded
//
// `examples/candles.csv` is 30 bars, so a 400-bar SMA never warms up and no
// grid point can fire a signal. Every metric cell is then empty — which used
// to be reported as ``unknown metric `sharpe` ``, blaming a perfectly valid
// name for an empty result, and pointing at the strategy file (`in: loading
// strategy …`) when nothing was wrong with it. The same name resolved fine as
// soon as one grid point traded, which is what made it so confusing to
// diagnose.
//
// The cause was resolving `-m` names against a *serialized sample*: serde drops
// a `None` metric, so a degenerate run's document has no `risk_adjusted.sharpe`
// key at all. Names now resolve against `metrics::flatten`'s static catalogue.
// ---------------------------------------------------------------------------

/// Periods far longer than the series, so no point trades.
const DEGENERATE_GRID: &str = "FAST=[300,320],SLOW=[400,420]";

#[test]
fn a_sweep_where_no_point_traded_still_resolves_its_metric_names() {
    let (out, csv) = sweep(
        "fugazi_opt_all_degenerate",
        &["--grid", DEGENERATE_GRID, "-m", "sharpe"],
    );
    assert!(
        !out.stderr.contains("unknown metric"),
        "a valid metric name was reported as unknown:\n{}",
        out.stderr
    );

    // The sweep must still produce its grid: one header + 2 x 2 points, with
    // the metric column present and every cell empty. That is exactly what a
    // sweep with *one* healthy point already did for its degenerate rows.
    let rows = read_csv(&csv);
    assert_eq!(rows.len(), 5, "expected a header + 4 points:\n{rows:#?}");
    assert!(
        rows[0].ends_with("risk_adjusted.sharpe"),
        "metric column missing from the header: {}",
        rows[0]
    );
    for row in &rows[1..] {
        assert!(
            row.ends_with(','),
            "expected an empty metric cell on a degenerate point: {row}"
        );
    }
}

/// An empty grid is almost always a mistake, and an all-empty CSV does not say
/// so on its own.
#[test]
fn a_sweep_where_no_point_traded_says_so() {
    let (out, _) = sweep(
        "fugazi_opt_degenerate_warns",
        &["--grid", DEGENERATE_GRID, "-m", "sharpe"],
    );
    assert!(
        out.stderr.contains("no grid point produced any trades"),
        "expected the empty-sweep warning:\n{}",
        out.stderr
    );
}

/// ...and must stay quiet when something did trade, or it is just noise.
#[test]
fn a_sweep_that_traded_does_not_warn() {
    let (out, _) = sweep(
        "fugazi_opt_traded_no_warn",
        &["--grid", "FAST=[2,3],SLOW=[5,8]", "-m", "sharpe"],
    );
    assert!(
        !out.stderr.contains("no grid point produced any trades"),
        "warned about an empty sweep that traded:\n{}",
        out.stderr
    );
}

/// A genuinely unknown name must still be rejected — the fix widens the
/// catalogue, it does not stop validating against it.
#[test]
fn a_misspelled_metric_is_still_unknown() {
    let (path, _) = scratch_file("fugazi_opt_bad_metric.yml", SWEEPABLE);
    let out_csv = common::cli::unique_path("fugazi_opt_bad_metric").with_extension("csv");
    let out = Cmd::new("optimize")
        .arg(&format!("@{}", path.display()))
        .series(&at("examples/candles.csv"))
        .args(&["--output", &out_csv.to_string_lossy()])
        .args(&["--grid", "FAST=[2,3],SLOW=[5,8]", "-m", "shrape"])
        .fails();
    assert!(
        out.stderr.contains("unknown metric"),
        "expected a typo to be caught:\n{}",
        out.stderr
    );
}
