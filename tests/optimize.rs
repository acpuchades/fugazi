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
    assert_eq!(
        lines.len(),
        7,
        "header + 6 grid points, got:\n{}",
        lines.join("\n")
    );

    let header = &lines[0];
    for axis in ["FAST", "SLOW"] {
        assert!(
            header.contains(axis),
            "axis `{axis}` missing from header: {header}"
        );
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
        &[
            "--params",
            "SLOW=9",
            "--grid",
            "FAST=[2,3]",
            "--metrics",
            "sharpe",
        ],
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
    assert!(
        values.len() >= 2,
        "need at least two comparable rows: {values:?}"
    );
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

// ---------------------------------------------------------------------------
// Neighbourhood smoothing (`--smooth`)
// ---------------------------------------------------------------------------
//
// The numeric truth — kernel weights, edge renormalization, subgrid and
// categorical partitioning, `None` handling — is pinned by unit tests on
// `smooth_keys` in `src/spec/optimize.rs`, where a failure names the function.
// `examples/candles.csv` is 30 bars, so a spike-vs-plateau *surface* is not
// constructible out of a real backtest here and pretending otherwise would pin
// the price fixture rather than the kernel. What belongs at this layer is
// wiring: flag parsing, column emission, ordering, and the axis→lattice
// mapping surviving the sort.

/// Split a CSV line on `,`.
fn cells(line: &str) -> Vec<&str> {
    line.split(',').collect()
}

fn column(header: &str, name: &str) -> usize {
    cells(header)
        .iter()
        .position(|c| *c == name)
        .unwrap_or_else(|| panic!("no `{name}` column in {header}"))
}

#[test]
fn without_smooth_the_csv_shape_is_untouched() {
    // The regression guard: `--smooth` is opt-in, so a sweep that doesn't ask
    // for it must emit exactly the columns it always did — including across a
    // stacked multi-subgrid sweep with a categorical axis, which is where the
    // sparse union-column projection is most likely to shift under a change to
    // the row struct.
    let (_, csv) = sweep(
        "fugazi_opt_nosmooth_shape",
        &[
            "--grid",
            "FAST=[2,3],SLOW=[6,8]",
            "--grid",
            // A categorical axis needs JSON-quoted values — a bare `[a,b]`
            // parses as the scalar string it looks like, not as a list.
            r#"FAST=[4],SLOW=[10],MODE=["a","b"]"#,
            "-m",
            "total_pct",
            "--best-by",
            "total_pct",
        ],
    );
    let lines = read_csv(&csv);
    let header = &lines[0];
    assert!(
        !header.contains("_smoothed") && !header.contains("_support"),
        "smoothing columns leaked into a sweep that never asked for them: {header}"
    );
    // Axis columns name-sorted, then the metric, then the DSR cell.
    assert!(
        header.starts_with("FAST,MODE,SLOW,returns.total_pct"),
        "column order changed: {header}"
    );
    assert_eq!(lines.len(), 1 + 4 + 2, "header + (2x2) + (1x1x2) points");
}

#[test]
fn smooth_appends_two_columns_without_reordering_the_others() {
    let (out, csv) = sweep(
        "fugazi_opt_smooth_columns",
        &[
            "--grid",
            "FAST=[2,3,4],SLOW=[6,8,10]",
            "-m",
            "total_pct",
            "--best-by",
            "total_pct",
            "--smooth",
        ],
    );
    let lines = read_csv(&csv);
    let header = &lines[0];
    assert!(
        header.starts_with("FAST,SLOW,returns.total_pct"),
        "existing columns were reordered: {header}"
    );
    assert!(
        header.ends_with("returns.total_pct_smoothed,returns.total_pct_support"),
        "the smoothing columns are not appended last: {header}"
    );
    // Bare `--smooth` is the Moore neighbourhood, and the console says so.
    assert!(
        out.stdout.contains("box:1"),
        "the inputs block should echo the default kernel:\n{}",
        out.stdout
    );
    // Support is a fraction of a fully-interior neighbourhood. The ceiling of
    // 1.0 holds because both axes here are evenly spaced — on an irregular axis
    // a denser-than-median stretch reads above it, deliberately.
    let sup = column(header, "returns.total_pct_support");
    let supports: Vec<f64> = lines[1..]
        .iter()
        .map(|l| {
            cells(l)[sup]
                .parse::<f64>()
                .expect("support is always defined")
        })
        .collect();
    assert_eq!(supports.len(), 9);
    assert!(
        supports.iter().all(|s| *s > 0.0 && *s <= 1.0 + 1e-12),
        "{supports:?}"
    );
    // A 3x3 box:1 lattice has exactly one fully interior cell.
    assert_eq!(
        supports.iter().filter(|s| (**s - 1.0).abs() < 1e-9).count(),
        1,
        "expected exactly one interior cell in a 3x3 grid: {supports:?}"
    );
}

#[test]
fn the_smoothed_column_is_the_neighbourhood_mean_of_the_raw_column() {
    // The centrepiece. Rather than engineer a surface, recompute the kernel
    // from the CSV's own raw column and demand agreement. Rows are indexed by
    // their axis *cells*, not by row order — which simultaneously proves the
    // axis→lattice mapping survived the sort by the smoothed key.
    //
    // `total_pct` rather than `sharpe`: it is defined on every point of a
    // 30-bar run, so the assertion tests the kernel and not the `None` path
    // (which the unit tests cover directly).
    let fasts = [2.0, 3.0, 4.0];
    let slows = [6.0, 8.0, 10.0];
    let (_, csv) = sweep(
        "fugazi_opt_smooth_recompute",
        &[
            "--grid",
            "FAST=[2,3,4],SLOW=[6,8,10]",
            "-m",
            "total_pct",
            "--best-by",
            "total_pct",
            "--smooth=box:1",
            "--smooth-min-support",
            "0",
        ],
    );
    let lines = read_csv(&csv);
    let header = &lines[0];
    let (cf, cs) = (column(header, "FAST"), column(header, "SLOW"));
    let craw = column(header, "returns.total_pct");
    let csm = column(header, "returns.total_pct_smoothed");
    let csup = column(header, "returns.total_pct_support");

    // raw[i][j] for FAST=fasts[i], SLOW=slows[j].
    let mut raw = [[f64::NAN; 3]; 3];
    let mut got = [[f64::NAN; 3]; 3];
    let mut support = [[f64::NAN; 3]; 3];
    for line in &lines[1..] {
        let c = cells(line);
        let i = fasts
            .iter()
            .position(|v| *v == c[cf].parse::<f64>().unwrap())
            .unwrap();
        let j = slows
            .iter()
            .position(|v| *v == c[cs].parse::<f64>().unwrap())
            .unwrap();
        raw[i][j] = c[craw].parse().unwrap();
        got[i][j] = c[csm].parse().unwrap();
        support[i][j] = c[csup].parse().unwrap();
    }

    for i in 0..3 {
        for j in 0..3 {
            // box:1 over the Chebyshev ball, renormalized over the cells that
            // exist — no padding, no reflection.
            let mut sum = 0.0;
            let mut n = 0.0;
            for di in -1i32..=1 {
                for dj in -1i32..=1 {
                    let (ni, nj) = (i as i32 + di, j as i32 + dj);
                    if (0..3).contains(&ni) && (0..3).contains(&nj) {
                        sum += raw[ni as usize][nj as usize];
                        n += 1.0;
                    }
                }
            }
            assert!(
                (got[i][j] - sum / n).abs() < 1e-9,
                "FAST={} SLOW={}: smoothed {} != neighbourhood mean {}",
                fasts[i],
                slows[j],
                got[i][j],
                sum / n
            );
            assert!(
                (support[i][j] - n / 9.0).abs() < 1e-9,
                "FAST={} SLOW={}: support {} != {n}/9",
                fasts[i],
                slows[j],
                support[i][j]
            );
        }
    }

    // And the CSV really is ordered by the smoothed key, not the raw one.
    let smoothed_in_order: Vec<f64> = lines[1..]
        .iter()
        .map(|l| cells(l)[csm].parse().unwrap())
        .collect();
    assert!(
        smoothed_in_order.windows(2).all(|w| w[0] >= w[1]),
        "rows are not sorted best-first by the smoothed key: {smoothed_in_order:?}"
    );
}

#[test]
fn min_support_empties_the_smoothed_cell_but_keeps_the_support_cell() {
    let (_, csv) = sweep(
        "fugazi_opt_smooth_minsupport",
        &[
            "--grid",
            "FAST=[2,3,4],SLOW=[6,8,10]",
            "-m",
            "total_pct",
            "--best-by",
            "total_pct",
            "--smooth=box:1",
            "--smooth-min-support",
            "1.0",
        ],
    );
    let lines = read_csv(&csv);
    let header = &lines[0];
    let csm = column(header, "returns.total_pct_smoothed");
    let csup = column(header, "returns.total_pct_support");
    let kept = lines[1..]
        .iter()
        .filter(|l| !cells(l)[csm].is_empty())
        .count();
    assert_eq!(
        kept, 1,
        "only the interior cell of a 3x3 clears full support"
    );
    assert!(
        lines[1..].iter().all(|l| !cells(l)[csup].is_empty()),
        "support must be reported even for the rows it rejected — that is the diagnostic"
    );
}

#[test]
fn smooth_scale_pins_the_distance_scale_and_index_restores_the_old_measure() {
    // Seven values, one surface, two typings. In value space the neighbourhood
    // cannot depend on declaration order; `--smooth-scale=index` is the
    // documented way back to the measure that did.
    let smoothed_by_fast = |name: &str, axis: &str, extra: &[&str]| {
        let mut args = vec![
            "--grid",
            axis,
            "-m",
            "total_pct",
            "--best-by",
            "total_pct",
            "--smooth=box:1",
        ];
        args.extend_from_slice(extra);
        let (out, csv) = sweep(name, &args);
        let lines = read_csv(&csv);
        let header = &lines[0];
        let (cf, csm) = (
            column(header, "FAST"),
            column(header, "returns.total_pct_smoothed"),
        );
        let mut got: Vec<(String, String)> = lines[1..]
            .iter()
            .map(|l| (cells(l)[cf].to_string(), cells(l)[csm].to_string()))
            .collect();
        got.sort();
        (out.stdout, got)
    };

    let scrambled = "FAST=[3,9,4,8,5,7,6],SLOW=[9]";
    let sorted = "FAST=[3,4,5,6,7,8,9],SLOW=[9]";
    let (stdout, a) = smoothed_by_fast("fugazi_opt_smooth_scale_scrambled", scrambled, &[]);
    let (_, b) = smoothed_by_fast("fugazi_opt_smooth_scale_sorted", sorted, &[]);
    assert_eq!(a, b, "declaration order changed the smoothed column");
    // The resolved scale is echoed, never left implicit.
    assert!(
        stdout.contains("scale FAST linear"),
        "the inputs block should name the scale each axis resolved to:\n{stdout}"
    );

    // Bare `index` restores it wholesale — and with it the order dependence.
    let (stdout, a) = smoothed_by_fast(
        "fugazi_opt_smooth_scale_index",
        scrambled,
        &["--smooth-scale=index"],
    );
    assert!(stdout.contains("scale FAST index"), "{stdout}");
    assert_ne!(
        a, b,
        "`--smooth-scale=index` did not restore the index-space measure"
    );

    // Per-axis pins compose with a bare default, and a geometric axis is
    // detected as log without being asked.
    let (stdout, _) = smoothed_by_fast(
        "fugazi_opt_smooth_scale_per_axis",
        "FAST=[2,4,8,16],SLOW=[6,9]",
        &["--smooth-scale=SLOW:index"],
    );
    assert!(
        stdout.contains("scale FAST log, SLOW index"),
        "per-axis pins and the automatic choice should both show:\n{stdout}"
    );
}

#[test]
fn smooth_scale_rejects_an_unknown_scale_and_needs_smooth() {
    let (path, _) = scratch_file("fugazi_opt_smooth_badscale_strategy.yml", SWEEPABLE);
    let out = Cmd::new("optimize")
        .arg(&format!("@{}", path.display()))
        .series(&at("examples/candles.csv"))
        .args(&["--grid", "FAST=[2,3],SLOW=[9]"])
        .args(&[
            "--best-by",
            "total_pct",
            "--smooth=box:1",
            "--smooth-scale=quadratic",
        ])
        .args(&["--output", "/dev/null"])
        .fails();
    assert!(
        out.stderr.contains("linear") && out.stderr.contains("index"),
        "the refusal should name the accepted scales, got: {}",
        out.stderr
    );

    // Like `--smooth-min-support`, it tunes a pass that has to be turned on.
    let out = Cmd::new("optimize")
        .arg(&format!("@{}", path.display()))
        .series(&at("examples/candles.csv"))
        .args(&["--grid", "FAST=[2,3],SLOW=[9]"])
        .args(&["--best-by", "total_pct", "--smooth-scale=index"])
        .args(&["--output", "/dev/null"])
        .fails();
    assert!(out.stderr.contains("--smooth"), "{}", out.stderr);
}

/// A numeric axis with one value is not a swept dimension — it carries no
/// neighbourhood information in either direction. Multiplying its share into
/// the support denominator made the same sweep score 1.000 written `SLOW=9`
/// and 0.333 written `SLOW=[9]`, and turned `--smooth-min-support 0.5` into a
/// hard error on a grid where every point had a complete `FAST` neighbourhood.
#[test]
fn a_one_value_axis_does_not_dilute_support() {
    let supports = |name: &str, grid: &str, params: &[&str]| {
        let mut args = vec![
            "--grid",
            grid,
            "-m",
            "total_pct",
            "--best-by",
            "total_pct",
            "--smooth=box:1",
        ];
        args.extend_from_slice(params);
        let (_, csv) = sweep(name, &args);
        let lines = read_csv(&csv);
        let header = &lines[0];
        let (cf, cs) = (
            column(header, "FAST"),
            column(header, "returns.total_pct_support"),
        );
        let mut got: Vec<(String, String)> = lines[1..]
            .iter()
            .map(|l| (cells(l)[cf].to_string(), cells(l)[cs].to_string()))
            .collect();
        got.sort();
        got
    };
    let listed = supports(
        "fugazi_opt_smooth_pin_listed",
        "FAST=[2,3,4,5,6],SLOW=[9]",
        &[],
    );
    let scalar = supports(
        "fugazi_opt_smooth_pin_scalar",
        "FAST=[2,3,4,5,6],SLOW=9",
        &[],
    );
    assert_eq!(
        listed, scalar,
        "the two spellings of a pinned axis must score the same"
    );
    assert!(
        listed
            .iter()
            .any(|(_, s)| (s.parse::<f64>().unwrap() - 1.0).abs() < 1e-12),
        "interior points should reach full support: {listed:?}"
    );

    // The user-visible half: a floor that used to reject the whole grid.
    let (path, _) = scratch_file("fugazi_opt_smooth_pin_minsup_strategy.yml", SWEEPABLE);
    let out_csv = common::cli::unique_path("fugazi_opt_smooth_pin_minsup").with_extension("csv");
    Cmd::new("optimize")
        .arg(&format!("@{}", path.display()))
        .series(&at("examples/candles.csv"))
        .args(&["--grid", "FAST=[2,3,4,5,6],SLOW=[9]"])
        .args(&["-m", "total_pct", "--best-by", "total_pct"])
        .args(&["--smooth=box:1", "--smooth-min-support", "1.0"])
        .args(&["--output", &out_csv.to_string_lossy()])
        .ok();
    let lines = read_csv(&out_csv.to_string_lossy());
    let csm = column(&lines[0], "returns.total_pct_smoothed");
    let kept = lines[1..]
        .iter()
        .filter(|l| !cells(l)[csm].is_empty())
        .count();
    assert_eq!(kept, 3, "the three interior FAST points clear full support");
}

/// A `--smooth-scale` pin is looked up by axis name, so a name that matches
/// nothing is never consulted: the user asks for `linear` and silently gets
/// whatever the heuristic picked. `--best-by` and `-m` both refuse an
/// unresolvable name; this one used not to.
#[test]
fn a_smooth_scale_pin_for_an_unknown_axis_is_refused() {
    let (path, _) = scratch_file("fugazi_opt_smooth_pin_unknown_strategy.yml", SWEEPABLE);
    let pinned = |scale: &str| {
        Cmd::new("optimize")
            .arg(&format!("@{}", path.display()))
            .series(&at("examples/candles.csv"))
            .args(&["--grid", "FAST=[2,4,8,16],SLOW=20,SYM=BTC"])
            .args(&[
                "-m",
                "total_pct",
                "--best-by",
                "total_pct",
                "--smooth=box:1",
            ])
            .args(&[&format!("--smooth-scale={scale}"), "--output", "/dev/null"])
    };

    let out = pinned("FASTT:linear").fails();
    assert!(
        out.stderr.contains("FASTT"),
        "the refusal should name the typo, got: {}",
        out.stderr
    );
    assert!(
        out.stderr.contains("FAST,") || out.stderr.contains("axes are: FAST"),
        "the refusal should list the axes that are available, got: {}",
        out.stderr
    );

    // The guard against over-rejecting: the correctly spelled pin still works,
    // and still takes effect.
    let out = pinned("FAST:linear").ok();
    assert!(
        out.stdout.contains("scale FAST linear"),
        "a valid pin must still be honoured:\n{}",
        out.stdout
    );
}

/// `SLOW=20` and `SYM=BTC` are scalars, not axes — a pin naming one is the same
/// silent no-op as a typo, and reads as one to the user.
#[test]
fn a_smooth_scale_pin_naming_a_scalar_is_refused() {
    let (path, _) = scratch_file("fugazi_opt_smooth_pin_scalar_strategy.yml", SWEEPABLE);
    for name in ["SYM", "SLOW"] {
        let out = Cmd::new("optimize")
            .arg(&format!("@{}", path.display()))
            .series(&at("examples/candles.csv"))
            .args(&["--grid", "FAST=[2,4,8,16],SLOW=20,SYM=BTC"])
            .args(&[
                "-m",
                "total_pct",
                "--best-by",
                "total_pct",
                "--smooth=box:1",
            ])
            .args(&[
                &format!("--smooth-scale={name}:log"),
                "--output",
                "/dev/null",
            ])
            .fails();
        assert!(
            out.stderr.contains(name),
            "pinning the scalar `{name}` should be refused by name, got: {}",
            out.stderr
        );
    }
}

/// Stacked subgrids legitimately have a name that is an axis in one and a
/// scalar in another, so the test is "matches somewhere", never "matches
/// everywhere" — the same asymmetry `compute_union_columns` reasons about.
#[test]
fn a_smooth_scale_pin_is_accepted_when_any_subgrid_sweeps_it() {
    let (path, _) = scratch_file("fugazi_opt_smooth_pin_stacked_strategy.yml", SWEEPABLE);
    let out = Cmd::new("optimize")
        .arg(&format!("@{}", path.display()))
        .series(&at("examples/candles.csv"))
        // `SLOW` sweeps in the second subgrid and is pinned flat in the first.
        .args(&["--grid", "FAST=[2,4,8,16],SLOW=9"])
        .args(&["--grid", "FAST=3,SLOW=[6,8,10]"])
        .args(&[
            "-m",
            "total_pct",
            "--best-by",
            "total_pct",
            "--smooth=box:1",
        ])
        .args(&["--smooth-scale=SLOW:index", "--output", "/dev/null"])
        .ok();
    assert!(
        out.stdout.contains("scale FAST log, SLOW index"),
        "a pin swept by one subgrid should be honoured there:\n{}",
        out.stdout
    );
}

/// Two equal values sit at distance 0 in value space, so each becomes a
/// full-weight neighbour of the other and the point double-counts itself —
/// which inflates `support` and lets a duplicate defeat the
/// `--smooth-min-support` floor that exists to reject edge points.
#[test]
fn a_repeated_axis_value_is_refused() {
    let (path, _) = scratch_file("fugazi_opt_dup_axis_strategy.yml", SWEEPABLE);
    let swept = |grid: &str| {
        Cmd::new("optimize")
            .arg(&format!("@{}", path.display()))
            .series(&at("examples/candles.csv"))
            .args(&["--grid", grid])
            .args(&[
                "-m",
                "total_pct",
                "--best-by",
                "total_pct",
                "--smooth=box:1",
            ])
            .args(&["--output", "/dev/null"])
    };

    let out = swept("FAST=[4,5,5,6],SLOW=[9]").fails();
    assert!(
        out.stderr.contains("FAST") && out.stderr.contains('5'),
        "the refusal should name the axis and the repeated value, got: {}",
        out.stderr
    );

    // The same grid without the typo is untouched.
    swept("FAST=[4,5,6],SLOW=[9]").ok();

    // Categorical too: a repeat there wastes exactly the same evaluations.
    let out = swept(r#"FAST=[2,3],MODE=["a","a","b"]"#).fails();
    assert!(
        out.stderr.contains("MODE"),
        "a repeated categorical value should be refused, got: {}",
        out.stderr
    );

    // `20` and `20.0` substitute identically into the strategy, so they name
    // one point under two spellings — and the error says both.
    let out = swept("FAST=[20,20.0],SLOW=[9]").fails();
    assert!(
        out.stderr.contains("20") && out.stderr.contains("20.0"),
        "the refusal should quote both spellings that collided, got: {}",
        out.stderr
    );
}

#[test]
fn smooth_without_best_by_is_refused() {
    // There is no ranking key to average over the neighbourhood. Clap enforces
    // it, the same way it enforces `-k`'s dependencies.
    let (path, _) = scratch_file("fugazi_opt_smooth_nobestby_strategy.yml", SWEEPABLE);
    let out = Cmd::new("optimize")
        .arg(&format!("@{}", path.display()))
        .series(&at("examples/candles.csv"))
        .args(&["--grid", "FAST=[2,3],SLOW=[9]"])
        .args(&["--smooth", "--output", "/dev/null"])
        .fails();
    assert!(
        out.stderr.contains("--best-by"),
        "the refusal should name the missing flag, got: {}",
        out.stderr
    );
}

#[test]
fn an_unknown_smoothing_kernel_names_the_forms_it_accepts() {
    let (path, _) = scratch_file("fugazi_opt_smooth_badkernel_strategy.yml", SWEEPABLE);
    let out = Cmd::new("optimize")
        .arg(&format!("@{}", path.display()))
        .series(&at("examples/candles.csv"))
        .args(&["--grid", "FAST=[2,3],SLOW=[9]"])
        .args(&["--best-by", "total_pct", "--smooth=parabola:2"])
        .args(&["--output", "/dev/null"])
        .fails();
    assert!(
        out.stderr.contains("box:R") && out.stderr.contains("gaussian:S"),
        "the refusal should name the accepted forms, got: {}",
        out.stderr
    );
}

#[test]
fn smoothing_composes_with_windowing_and_risk_aversion() {
    // `-k` shifts each point's cross-window mean against it *before* ranking;
    // `--smooth` then averages that shifted key over the neighbourhood. The two
    // penalties are orthogonal — dispersion across time, dispersion across the
    // parameter neighbourhood — and they have to compose.
    //
    // `-w 10` rather than the documentation's `-w 252`: the example series is
    // 30 bars, so 252-bar windows would leave a single degenerate window.
    let (out, csv) = sweep(
        "fugazi_opt_smooth_with_k",
        &[
            "--grid",
            "FAST=[2,3,4],SLOW=[6,8,10]",
            "-m",
            "total_pct",
            "--best-by",
            "total_pct",
            "-w",
            "10",
            "-k",
            "1.0",
            "--smooth=box:1",
        ],
    );
    let lines = read_csv(&csv);
    let header = &lines[0];
    // Windowed sweeps emit `_mean`/`_std` pairs; the smoothed column sits after.
    assert!(header.contains("returns.total_pct_mean"), "{header}");
    let cmean = column(header, "returns.total_pct_mean");
    let cstd = column(header, "returns.total_pct_std");
    let csm = column(header, "returns.total_pct_smoothed");

    // What is smoothed is `mean − k·std`, not `mean`. Rebuild both candidate
    // neighbourhood averages for the winning row and demand the shifted one.
    let win = cells(&lines[1]);
    let (mean, std) = (
        win[cmean].parse::<f64>().unwrap(),
        win[cstd].parse::<f64>().unwrap(),
    );
    let smoothed: f64 = win[csm].parse().unwrap();
    assert!(
        std > 0.0,
        "this fixture needs a metric with cross-window spread for the test to bite"
    );
    // The smoothed key averages `mean − 1.0·std` over the neighbourhood, so it
    // must land below the raw windowed mean of the same neighbourhood.
    assert!(
        smoothed < mean,
        "the risk-aversion shift was not folded in before smoothing: smoothed {smoothed} vs mean {mean}"
    );
    assert!(
        out.stdout.contains("risk-aversion") && out.stdout.contains("box:1"),
        "the inputs block should echo both knobs:\n{}",
        out.stdout
    );
}

#[test]
fn an_ascending_best_by_sorts_the_smoothed_column_ascending() {
    let (_, csv) = sweep(
        "fugazi_opt_smooth_ascending",
        &[
            "--grid",
            "FAST=[2,3,4],SLOW=[6,8,10]",
            "-m",
            "max_pct",
            "--best-by",
            "max_pct",
            "--smooth=box:1",
        ],
    );
    let lines = read_csv(&csv);
    let csm = column(&lines[0], "drawdown.max_pct_smoothed");
    let values: Vec<f64> = lines[1..]
        .iter()
        .filter_map(|l| cells(l)[csm].parse::<f64>().ok())
        .collect();
    assert!(values.len() >= 2, "{values:?}");
    assert!(
        values.windows(2).all(|w| w[0] <= w[1]),
        "a minimize metric must sort smallest-first on the smoothed key: {values:?}"
    );
}

#[test]
fn walkforward_folds_carry_the_smoothed_is_key() {
    let (path, _) = scratch_file("fugazi_opt_smooth_wf_strategy.yml", SWEEPABLE);
    let out_csv = common::cli::unique_path("fugazi_opt_smooth_wf").with_extension("csv");
    let out_str = out_csv.to_string_lossy().into_owned();
    Cmd::new("optimize")
        .arg(&format!("@{}", path.display()))
        .series(&at("examples/candles.csv"))
        .args(&["--grid", "FAST=[2,3,4],SLOW=[6,8,10]"])
        .args(&["-m", "total_pct", "--best-by", "total_pct"])
        .args(&["--walkforward", "12,6", "--smooth=box:1"])
        .args(&["--output", &out_str])
        .ok();
    let lines = read_csv(&out_str);
    let header = &lines[0];
    assert!(
        header.ends_with("returns.total_pct_is_smoothed,returns.total_pct_is_support"),
        "fold rows should carry the key each fold was actually selected on: {header}"
    );
    let csm = column(header, "returns.total_pct_is_smoothed");
    assert!(
        lines[1..]
            .iter()
            .all(|l| cells(l)[csm].parse::<f64>().is_ok()),
        "every fold should report its smoothed IS key:\n{lines:#?}"
    );
}
