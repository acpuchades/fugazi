//! End-to-end tests of the snapshot co-occurrence diagnostic, across the three
//! subcommands that assemble a multi-symbol universe: `get` (which writes one),
//! `run` and `optimize` (which read one back).
//!
//! Bars are grouped into snapshots by **exact** timestamp — the rule that stops
//! a multi-session universe from manufacturing lookahead (a Tokyo close and a
//! New York close on the same date are thirteen hours apart, not
//! contemporaneous). The failure mode it leaves behind is silent: a nine-symbol
//! index universe stamped at five session opens produces snapshots holding at
//! most four of them, and every surface still looks right — each symbol has its
//! full history, the row and bar counts are correct, and only the *joint*
//! occupancy is wrong.
//!
//! These pin the wiring: that the warning reaches stderr where a universe never
//! meets, and stays quiet where it does. The measurement itself is unit-tested
//! in `src/cli/overlap.rs`.
//!
//! `file:` is `get`'s hermetic provider — no network, and the file's own
//! `symbol` / `freq` / `time` columns drive the same assembly path a remote
//! fetch takes; `run` and `optimize` read the same shape through `--series`.

mod common;

use common::cli::{Cmd, at, scratch_file, unique_path};

/// One `symbol,freq,time,...` row at `date` `time` with filler OHLCV.
fn bar(symbol: &str, date: &str, time: &str) -> String {
    format!("{symbol},1d,{date}T{time}Z,1,1,1,1,1\n")
}

const HEADER: &str = "symbol,freq,time,open,high,low,close,volume\n";

/// Run `get file:<file>` over `body` and hand back its stdout + stderr.
fn get_over(name: &str, body: String) -> (String, String) {
    let (path, _) = scratch_file(name, &format!("{HEADER}{body}"));
    let out = unique_path("dataset.csv");
    let outcome = Cmd::new("get")
        .arg(&format!("file:{}", path.display()))
        .args(&["--since", "2024-01-02", "--until", "2024-01-05"])
        .args(&["-o", out.to_str().expect("utf-8 scratch path")])
        .ok();
    (outcome.stdout, outcome.stderr)
}

/// The macro-index case that motivated this: five session opens, so at most
/// two of the five symbols ever land on one stamp.
#[test]
fn a_fragmented_fetch_reports_its_widest_snapshot() {
    let mut body = String::new();
    for date in ["2024-01-02", "2024-01-03"] {
        body += &bar("^N225", date, "00:00:00");
        body += &bar("^HSI", date, "01:30:00");
        body += &bar("^FTSE", date, "07:00:00");
        body += &bar("^GDAXI", date, "07:00:00");
        body += &bar("SPY", date, "13:30:00");
    }
    let (stdout, stderr) = get_over("fragmented.csv", body);

    assert!(
        stderr.contains("at most 2 of 5 symbols ever share a bar"),
        "no fragmentation warning on stderr:\n{stderr}"
    );
    // The widest snapshot is named, with the session stamp the split runs
    // along — that stamp is what points at the cause.
    assert!(
        stderr.contains("widest snapshot: ^FTSE, ^GDAXI (2024-01-02 07:00Z)"),
        "widest snapshot not reported:\n{stderr}"
    );
    assert!(
        stderr.contains("never sharing a bar with any other symbol: SPY, ^HSI, ^N225"),
        "isolated symbols not reported:\n{stderr}"
    );
    // And the summary block carries the same figure, next to rows/series.
    assert!(
        stdout.contains("widest snapshot: 2 of 5 symbols"),
        "result block missing the overlap field:\n{stdout}"
    );
}

/// A universe stamped alike stays quiet — and still gets the positive figure
/// in the result block, which is the confirmation a cross-sectional dataset
/// actually wants.
#[test]
fn a_universe_that_meets_warns_about_nothing() {
    let mut body = String::new();
    for date in ["2024-01-02", "2024-01-03"] {
        for sym in ["SPY", "QQQ", "EEM"] {
            body += &bar(sym, date, "13:30:00");
        }
    }
    let (stdout, stderr) = get_over("aligned.csv", body);

    assert!(
        !stderr.contains("ever share a bar"),
        "unexpected fragmentation warning:\n{stderr}"
    );
    assert!(
        stdout.contains("widest snapshot: 3 of 3 symbols"),
        "result block missing the overlap field:\n{stdout}"
    );
}

/// Daylight saving alone moves a session by an hour, so per-symbol stamp
/// *signatures* differ while the series still share nearly every bar. The
/// guard measures observed co-occurrence, so it does not fire here.
#[test]
fn daylight_saving_alone_does_not_warn() {
    // ^FTSE {07:00, 08:00} vs ^GDAXI {06:00, 07:00, 08:00} — different
    // signatures, two shared bars.
    let body = bar("^GDAXI", "2024-01-02", "06:00:00")
        + &bar("^FTSE", "2024-01-03", "07:00:00")
        + &bar("^GDAXI", "2024-01-03", "07:00:00")
        + &bar("^FTSE", "2024-01-04", "08:00:00")
        + &bar("^GDAXI", "2024-01-04", "08:00:00");
    let (stdout, stderr) = get_over("dst.csv", body);

    assert!(
        !stderr.contains("ever share a bar"),
        "unexpected fragmentation warning:\n{stderr}"
    );
    assert!(
        stdout.contains("widest snapshot: 2 of 2 symbols"),
        "result block missing the overlap field:\n{stdout}"
    );
}

/// A single-symbol fetch has nothing to co-occur with: no warning, and no
/// overlap field cluttering the summary.
#[test]
fn a_single_symbol_fetch_says_nothing_about_overlap() {
    let body = bar("SPY", "2024-01-02", "13:30:00") + &bar("SPY", "2024-01-03", "13:30:00");
    let (stdout, stderr) = get_over("single.csv", body);

    assert!(!stderr.contains("ever share a bar"), "warned:\n{stderr}");
    assert!(
        !stdout.contains("overlap"),
        "overlap field on a one-symbol fetch:\n{stdout}"
    );
}

// ---------------------------------------------------------------------------
// `run` / `optimize` — the same universe, read back through `--series`
// ---------------------------------------------------------------------------

/// A minimal cross-sectional basket: rank on a 2-bar rate of change, one long
/// and one short. Short warm-up so a handful of bars is a real run.
const BASKET: &str = "\
selection: !top_bottom { longs: 1, shorts: 1 }
score: !roc
  source: !close { source: !pick { symbol: !slot SYM } }
  period: 2
sizing: !equal_weight 2
";

/// A `--series` CSV: `symbol;time;…` rows for each `(symbol, session)` pair
/// over `days` days, with a gently drifting price so the ranking has something
/// to sort.
fn series_csv(pairs: &[(&str, &str)], days: usize) -> String {
    let mut out = String::from("symbol;time;open;high;low;close;volume\n");
    for d in 0..days {
        let date = format!("2024-01-{:02}", d + 1);
        for (i, (sym, session)) in pairs.iter().enumerate() {
            let p = 100.0 + (d as f64) * (1.0 + i as f64) * 0.5;
            out += &format!(
                "{sym};{date}T{session}Z;{p:.2};{:.2};{:.2};{:.2};1000\n",
                p + 1.0,
                p - 1.0,
                p + 0.3,
            );
        }
    }
    out
}

/// A basket over a universe split across two session opens: the strategy ranks
/// two symbols per bar, not the four it names.
#[test]
fn a_fragmented_run_universe_warns() {
    let (_, strategy) = scratch_file("frag_basket.yml", BASKET);
    let (_, series) = scratch_file(
        "frag_universe.csv",
        &series_csv(
            &[
                ("AAA", "13:30:00"),
                ("BBB", "13:30:00"),
                ("CCC", "00:00:00"),
                ("DDD", "00:00:00"),
            ],
            12,
        ),
    );
    let out = Cmd::new("run")
        .arg(&format!("basket:{strategy}"))
        .series(&series)
        .args(&["--crypto", "-f", "1d"])
        .output_dir("frag_run")
        .ok();

    assert!(
        out.stderr
            .contains("at most 2 of 4 symbols ever share a bar"),
        "no fragmentation warning on stderr:\n{}",
        out.stderr
    );
    assert!(
        out.stderr.contains("widest snapshot: CCC, DDD"),
        "widest snapshot not reported:\n{}",
        out.stderr
    );
    // The inputs block carries the same figure, under the universe it qualifies.
    assert!(
        out.stdout.contains("widest snapshot: 2 of 4 symbols"),
        "inputs block missing the overlap field:\n{}",
        out.stdout
    );
}

/// The warning survives `--quiet`, which suppresses the summary rather than a
/// finding about the data.
#[test]
fn the_run_warning_survives_quiet() {
    let (_, strategy) = scratch_file("quiet_basket.yml", BASKET);
    let (_, series) = scratch_file(
        "quiet_universe.csv",
        &series_csv(&[("AAA", "13:30:00"), ("CCC", "00:00:00")], 12),
    );
    let out = Cmd::new("run")
        .arg(&format!("basket:{strategy}"))
        .series(&series)
        .args(&["--crypto", "-f", "1d", "--quiet"])
        .output_dir("quiet_run")
        .ok();

    assert!(
        out.stdout.is_empty(),
        "--quiet still printed:\n{}",
        out.stdout
    );
    assert!(
        out.stderr
            .contains("at most 1 of 2 symbols ever share a bar"),
        "--quiet swallowed the warning:\n{}",
        out.stderr
    );
}

/// A universe stamped alike stays quiet, and still reports the positive figure.
#[test]
fn an_aligned_run_universe_warns_about_nothing() {
    let (_, strategy) = scratch_file("aligned_basket.yml", BASKET);
    let (_, series) = scratch_file(
        "aligned_universe.csv",
        &series_csv(
            &[
                ("AAA", "13:30:00"),
                ("BBB", "13:30:00"),
                ("CCC", "13:30:00"),
                ("DDD", "13:30:00"),
            ],
            12,
        ),
    );
    let out = Cmd::new("run")
        .arg(&format!("basket:{strategy}"))
        .series(&series)
        .args(&["--crypto", "-f", "1d"])
        .output_dir("aligned_run")
        .ok();

    assert!(
        !out.stderr.contains("ever share a bar"),
        "unexpected fragmentation warning:\n{}",
        out.stderr
    );
    assert!(
        out.stdout.contains("widest snapshot: 4 of 4 symbols"),
        "inputs block missing the overlap field:\n{}",
        out.stdout
    );
}

/// A single-symbol run is not a universe: no field, no warning. Uses the
/// bundled single-asset example, which is the ordinary case this must not
/// clutter.
#[test]
fn a_single_asset_run_says_nothing_about_overlap() {
    let out = Cmd::new("run")
        .arg(&at("examples/strategy.yml"))
        .series(&at("examples/candles.csv"))
        .output_dir("single_run")
        .ok();

    assert!(
        !out.stderr.contains("ever share a bar"),
        "warned:\n{}",
        out.stderr
    );
    assert!(
        !out.stdout.contains("overlap"),
        "overlap field on a single-asset run:\n{}",
        out.stdout
    );
}

/// `optimize` warns before the sweep rather than after it: every row of a grid
/// over a fragmented universe measures something other than the universe it
/// names, and that is worth knowing before the grid runs.
#[test]
fn a_fragmented_optimize_universe_warns() {
    let (_, strategy) = scratch_file(
        "opt_basket.yml",
        "selection: !top_bottom { longs: 1, shorts: 1 }
score: !roc
  source: !close { source: !pick { symbol: !slot SYM } }
  period: !param P
sizing: !equal_weight 2
",
    );
    let (_, series) = scratch_file(
        "opt_universe.csv",
        &series_csv(&[("AAA", "13:30:00"), ("CCC", "00:00:00")], 12),
    );
    let grid = unique_path("grid.csv");
    let out = Cmd::new("optimize")
        .arg(&format!("basket:{strategy}"))
        .series(&series)
        .args(&["--grid", "P=[2,3]", "-m", "sharpe"])
        .args(&["--crypto", "-f", "1d"])
        .args(&["--output", grid.to_str().expect("utf-8 scratch path")])
        .ok();

    assert!(
        out.stderr
            .contains("at most 1 of 2 symbols ever share a bar"),
        "no fragmentation warning on stderr:\n{}",
        out.stderr
    );
}
