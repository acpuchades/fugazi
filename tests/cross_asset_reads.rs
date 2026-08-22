//! A document may **read** series it does not trade — `!pick { symbol: X }`
//! anywhere in the tree — and the runners join exactly those into the snapshot
//! stream.
//!
//! The bug these guard is the expensive kind: before this, a single-asset
//! document naming another asset built fine, `check`ed `ok`, ran to completion,
//! and filled nothing, because the CLI narrowed the snapshot stream to the
//! traded symbol and the foreign `!pick` matched no entry on any bar. Every
//! comparison downstream stayed `None`, so it read as "the gate filtered
//! everything out" rather than "the gate never evaluated".
//!
//! Two properties, and both need asserting: the read **resolves**, and it does
//! so **without moving the timeline** — a series the document only reads must
//! not add bars the traded asset never had.

mod common;

use common::cli::{Cmd, scratch_file};

/// `TRADED` rises monotonically; `GATE` crosses `100` exactly once, upward, at
/// bar 3 and never comes back. So a strategy gated on `GATE > 100` enters once
/// and holds, and a strategy that *cannot see* `GATE` never enters at all —
/// the two outcomes are one fill versus zero, not a subtle numeric difference.
///
/// `SPARE` exists only to be ignored: it is in the frame, referenced by
/// nothing, and must not end up in the snapshots.
const TRADED_CLOSES: [f64; 6] = [10.0, 11.0, 12.0, 13.0, 14.0, 15.0];
const GATE_CLOSES: [f64; 6] = [90.0, 95.0, 99.0, 101.0, 102.0, 103.0];
const SPARE_CLOSES: [f64; 6] = [7.0, 7.0, 7.0, 7.0, 7.0, 7.0];

/// Six daily bars, `2024-01-01` onward, for whichever symbols are asked for.
/// Flat OHLC (`open == close`) so a market order filling at the next bar's open
/// fills at that bar's close.
fn frame(symbols: &[(&str, &[f64])]) -> String {
    let mut out = String::from("symbol,freq,time,open,high,low,close,volume\n");
    for (sym, closes) in symbols {
        for (i, c) in closes.iter().enumerate() {
            out.push_str(&format!(
                "{sym},1d,2024-01-0{}T00:00:00Z,{c},{c},{c},{c},1000\n",
                i + 1
            ));
        }
    }
    out
}

/// A single-asset document on `TRADED`, entering while `GATE`'s close is above
/// `100` — the regime-gate shape, the thing that silently did nothing.
const GATED: &str = "\
root: TRADED
long:
  enter: !gt { lhs: !close { source: !pick { symbol: GATE } }, rhs: !value 100 }
  exit: !never
sizing: !value 1.0
";

/// The same document with the gate rooted on its own series — the control that
/// proves the fixture, not the feature: `TRADED` never exceeds 100, so this
/// enters never.
const SELF_GATED: &str = "\
root: TRADED
long:
  enter: !gt { lhs: !close, rhs: !value 100 }
  exit: !never
sizing: !value 1.0
";

fn run(name: &str, doc: &str, csv: &str) -> common::cli::Outcome {
    let (_spec, spec_arg) = scratch_file(&format!("{name}.yml"), doc);
    let (_data, data_arg) = scratch_file(&format!("{name}.csv"), csv);
    Cmd::new("run")
        .arg(&spec_arg)
        .series(&data_arg)
        .costs("none")
        .args(&["--crypto", "-f", "1d"])
        .output_dir(name)
        .ok()
}

/// The headline case from the report: a single-asset document gating on another
/// asset's level trades, instead of completing with zero fills and no diagnostic.
#[test]
fn a_foreign_pick_resolves_in_a_single_asset_document() {
    let out = run(
        "xread_foreign",
        GATED,
        &frame(&[("TRADED", &TRADED_CLOSES), ("GATE", &GATE_CLOSES)]),
    );
    let fills = out.rows("fills.csv");
    assert_eq!(
        fills.len(),
        1,
        "the gate opens at bar 3 and never closes, so exactly one entry:\n{}",
        out.read("fills.csv")
    );
    // Submitted on the bar the gate first reads true (index 3, `2024-01-04`),
    // filled at the next bar's open — nothing fills on the bar that caused it.
    assert!(
        fills[0].starts_with("2024-01-05T00:00:00Z,TRADED,buy,"),
        "unexpected fill: {}",
        fills[0]
    );
}

/// The control: identical document, identical data, gate rooted on the traded
/// series instead. If this also filled, the test above would be proving nothing
/// about *which* series the gate read.
#[test]
fn the_same_gate_on_its_own_series_never_fires() {
    let out = run(
        "xread_self",
        SELF_GATED,
        &frame(&[("TRADED", &TRADED_CLOSES), ("GATE", &GATE_CLOSES)]),
    );
    assert!(
        out.rows("fills.csv").is_empty(),
        "TRADED never exceeds 100:\n{}",
        out.read("fills.csv")
    );
}

/// A read-only series is **left-joined** onto the traded symbol's bars, never
/// outer-joined. `GATE` here has three bars the traded symbol does not; if
/// those leaked into the timeline the run would report 6 bars, and every
/// per-bar figure derived from it — the returns column, the annualization
/// divisor — would describe a series `TRADED` never had.
#[test]
fn a_read_only_series_contributes_no_bars() {
    let out = run(
        "xread_no_bars",
        GATED,
        &frame(&[("TRADED", &TRADED_CLOSES[..3]), ("GATE", &GATE_CLOSES)]),
    );
    assert_eq!(
        out.rows("returns.csv").len(),
        3,
        "the traded symbol has three bars; the read series must not add its own:\n{}",
        out.read("returns.csv")
    );
}

/// A symbol in the frame that the document never names is not carried into the
/// snapshots: the run is identical with and without it. Asserted on the
/// artefacts rather than on snapshot width because that is what a user would
/// notice — but it is the same property, since an extra entry per bar is the
/// only way `SPARE` could change anything.
#[test]
fn an_unreferenced_symbol_in_the_frame_changes_nothing() {
    let two = run(
        "xread_two",
        GATED,
        &frame(&[("TRADED", &TRADED_CLOSES), ("GATE", &GATE_CLOSES)]),
    );
    let three = run(
        "xread_three",
        GATED,
        &frame(&[
            ("TRADED", &TRADED_CLOSES),
            ("GATE", &GATE_CLOSES),
            ("SPARE", &SPARE_CLOSES),
        ]),
    );
    assert_eq!(two.read("fills.csv"), three.read("fills.csv"));
    assert_eq!(two.read("returns.csv"), three.read("returns.csv"));
}

/// The other half of the fix. A `!pick` whose symbol is not in the input is a
/// hard error naming the symbol and listing what *is* available — not a run of
/// `None`s that completes and reports a plausible zero-trade backtest.
#[test]
fn a_pick_naming_an_absent_series_is_refused() {
    let (_spec, spec_arg) = scratch_file("xread_absent.yml", GATED);
    let (_data, data_arg) = scratch_file(
        "xread_absent.csv",
        &frame(&[("TRADED", &TRADED_CLOSES), ("SPARE", &SPARE_CLOSES)]),
    );
    let out = Cmd::new("run")
        .arg(&spec_arg)
        .series(&data_arg)
        .costs("none")
        .args(&["--crypto", "-f", "1d"])
        .output_dir("xread_absent")
        .fails();
    assert!(
        out.stderr.contains("GATE"),
        "the error must name the series that is missing:\n{}",
        out.stderr
    );
    assert!(
        out.stderr.contains("SPARE") && out.stderr.contains("TRADED"),
        "the error must list what the input does carry:\n{}",
        out.stderr
    );
}

/// The sweep path makes the same check, and makes it **once, up front** — a
/// grid whose every row silently read `None` would produce a whole table of
/// plausible zero-trade results.
#[test]
fn optimize_refuses_an_absent_series_before_the_sweep() {
    let (_spec, spec_arg) = scratch_file(
        "xread_opt.yml",
        "\
root: TRADED
long:
  enter: !gt { lhs: !close { source: !pick { symbol: GATE } }, rhs: !param LEVEL }
  exit: !never
sizing: !value 1.0
",
    );
    let (_data, data_arg) = scratch_file(
        "xread_opt.csv",
        &frame(&[("TRADED", &TRADED_CLOSES), ("SPARE", &SPARE_CLOSES)]),
    );
    let out = Cmd::new("optimize")
        .arg(&spec_arg)
        .series(&data_arg)
        .costs("none")
        .args(&[
            "--crypto",
            "-f",
            "1d",
            "--grid",
            "LEVEL=[100,101]",
            "-o",
            common::cli::unique_path("xread_opt_grid.csv")
                .to_str()
                .expect("utf-8"),
        ])
        .fails();
    assert!(
        out.stderr.contains("GATE"),
        "the sweep must name the missing series:\n{}",
        out.stderr
    );
}

/// A parameterised `!pick` head resolves through the sweep's probe, so a
/// document whose gate symbol comes from `--params` joins the series like any
/// other.
#[test]
fn optimize_resolves_a_parameterised_pick_symbol() {
    let (_spec, spec_arg) = scratch_file(
        "xread_opt_ok.yml",
        "\
root: TRADED
long:
  enter: !gt { lhs: !close { source: !pick { symbol: !param GATE_SYM } }, rhs: !param LEVEL }
  exit: !never
sizing: !value 1.0
",
    );
    let (_data, data_arg) = scratch_file(
        "xread_opt_ok.csv",
        &frame(&[("TRADED", &TRADED_CLOSES), ("GATE", &GATE_CLOSES)]),
    );
    let grid = common::cli::unique_path("xread_opt_ok_grid.csv");
    Cmd::new("optimize")
        .arg(&spec_arg)
        .series(&data_arg)
        .costs("none")
        .args(&[
            "--crypto",
            "-f",
            "1d",
            "--params",
            "GATE_SYM=GATE",
            "--grid",
            "LEVEL=[100,101]",
            "-o",
            grid.to_str().expect("utf-8"),
        ])
        .ok();
    let csv = std::fs::read_to_string(&grid).expect("grid csv written");
    assert!(
        csv.lines().count() >= 3,
        "expected a header and two rows:\n{csv}"
    );
    // `LEVEL=100` fires (GATE reaches 101), `LEVEL=101` does not — so the two
    // rows must differ. Identical rows would mean the gate read `None` in both.
    let rows: Vec<&str> = csv.lines().skip(1).collect();
    assert_ne!(
        rows[0], rows[1],
        "both grid points produced the same result, so the gate never evaluated:\n{csv}"
    );
}

/// `check` has no data, so it cannot say whether a read series is *present* —
/// but it says the document needs one, which is the half that turns "why did
/// this never trade?" into "I forgot to pass GATE".
#[test]
fn check_reports_the_series_a_document_reads() {
    let (_spec, spec_arg) = scratch_file("xread_check.yml", GATED);
    let out = Cmd::new("check").arg("strategy").arg(&spec_arg).ok();
    assert!(
        out.stdout.contains("reads") && out.stdout.contains("GATE"),
        "check should name the read-only series:\n{}",
        out.stdout
    );

    let (_plain, plain_arg) = scratch_file("xread_check_plain.yml", SELF_GATED);
    let plain = Cmd::new("check").arg("strategy").arg(&plain_arg).ok();
    assert!(
        !plain.stdout.contains("reads"),
        "a document that reads nothing extra should not grow a line:\n{}",
        plain.stdout
    );
}

/// A pairs document is already required to root every leaf through a `!pick`
/// — neither leg is blessed. What was missing is that a `!pick` could only name
/// the two legs; a third series the pair merely reads (an index level to hedge
/// the spread against, a volatility gauge to gate it) resolved to nothing.
#[test]
fn a_pairs_document_can_read_a_third_series() {
    let (_spec, spec_arg) = scratch_file(
        "xread_pairs.yml",
        "\
left: TRADED
right: SPARE
long_spread:
  enter: !gt { lhs: !close { source: !pick { symbol: GATE } }, rhs: !value 100 }
  exit: !never
",
    );
    let (_data, data_arg) = scratch_file(
        "xread_pairs.csv",
        &frame(&[
            ("TRADED", &TRADED_CLOSES),
            ("SPARE", &SPARE_CLOSES),
            ("GATE", &GATE_CLOSES),
        ]),
    );
    let out = Cmd::new("run")
        .arg(&format!("pairs:{}", spec_arg))
        .series(&data_arg)
        .costs("none")
        .args(&["--crypto", "-f", "1d"])
        .output_dir("xread_pairs")
        .ok();
    assert!(
        !out.rows("fills.csv").is_empty(),
        "the third series' gate never fired:\n{}",
        out.read("fills.csv")
    );
}

/// A portfolio's universe is what its children **declare**, not everything in
/// the frame. `SPARE` is traded by no child and read by no expression, so it
/// must not appear in the run's universe — carrying it would build an extra
/// snapshot entry on every bar for an asset nothing can touch.
#[test]
fn a_portfolio_trades_only_what_its_children_declare() {
    let (_spec, spec_arg) = scratch_file(
        "xread_portfolio.yml",
        "\
children:
  - name: gated
    strategy:
      root: TRADED
      long:
        enter: !gt { lhs: !close { source: !pick { symbol: GATE } }, rhs: !value 100 }
        exit: !never
      sizing: !value 1.0
",
    );
    let (_data, data_arg) = scratch_file(
        "xread_portfolio.csv",
        &frame(&[
            ("TRADED", &TRADED_CLOSES),
            ("GATE", &GATE_CLOSES),
            ("SPARE", &SPARE_CLOSES),
        ]),
    );
    let out = Cmd::new("run")
        .arg(&format!("portfolio:{}", spec_arg))
        .series(&data_arg)
        .costs("none")
        .args(&["--crypto", "-f", "1d"])
        .output_dir("xread_portfolio")
        .ok();
    assert!(
        out.stdout.contains("1 symbols (TRADED)"),
        "the universe should be the child's declared symbol alone:\n{}",
        out.stdout
    );
    // And the child's cross-asset gate still resolves — restricting the traded
    // universe must not restrict what can be *read*.
    assert!(
        !out.rows("fills.csv").is_empty(),
        "the child's gate on GATE never fired:\n{}",
        out.read("fills.csv")
    );
}

/// A grid may sweep the **traded instrument**, and every row must match what
/// the same document produces on its own through `run`.
///
/// Before `root:` this was refused across subgrids and *silently wrong* within
/// one: the atom slice and every snapshot tag were bound to the probe symbol
/// before the sweep started, so each row backtested the probe's bars whatever
/// its own `symbol:` had resolved to. The equality against a standalone `run`
/// is the assertion that matters — a grid that merely produced two different
/// rows would also have passed the old, broken behaviour.
#[test]
fn optimize_sweeps_the_traded_symbol() {
    let (_spec, spec_arg) = scratch_file(
        "xroot_sweep.yml",
        "\
root: !pick { symbol: !param SYM }
long:
  enter: !gt { lhs: !close, rhs: !value 0 }
  exit: !never
sizing: !value 1.0
",
    );
    let (_data, data_arg) = scratch_file(
        "xroot_sweep.csv",
        &frame(&[("TRADED", &TRADED_CLOSES), ("GATE", &GATE_CLOSES)]),
    );
    let grid = common::cli::unique_path("xroot_sweep_grid.csv");
    Cmd::new("optimize")
        .arg(&spec_arg)
        .series(&data_arg)
        .costs("none")
        .args(&[
            "--crypto",
            "-f",
            "1d",
            "--grid",
            "SYM=[\"TRADED\",\"GATE\"]",
            "--metrics",
            "returns.total",
            "-o",
            grid.to_str().expect("utf-8"),
        ])
        .ok();
    let csv = std::fs::read_to_string(&grid).expect("grid csv written");
    let header: Vec<&str> = csv.lines().next().expect("a header").split(',').collect();
    // Located by name, not by position: the writer appends its own columns
    // (`selection.deflated_sharpe`) after the requested metrics.
    let col = header
        .iter()
        .position(|h| *h == "returns.total")
        .expect("the requested metric column");
    let rows: Vec<&str> = csv.lines().skip(1).collect();
    assert_eq!(rows.len(), 2, "one row per swept symbol:\n{csv}");

    // Each row must equal the same document run standalone on that symbol —
    // the check the old behaviour could not pass.
    for sym in ["TRADED", "GATE"] {
        let out = Cmd::new("run")
            .arg(&spec_arg)
            .series(&data_arg)
            .costs("none")
            .args(&["--crypto", "-f", "1d", "--params", &format!("SYM={sym}")])
            .output_dir(&format!("xroot_sweep_run_{sym}"))
            .ok();
        // `returns.total`, read out of the `returns:` block of metrics.yml.
        let metrics = out.read("metrics.yml");
        let standalone = metrics
            .lines()
            .skip_while(|l| !l.starts_with("returns:"))
            .find_map(|l| l.trim().strip_prefix("total:"))
            .map(|v| v.trim().parse::<f64>().expect("a number"))
            .expect("returns.total in metrics.yml");
        let row = rows
            .iter()
            .find(|r| r.starts_with(sym))
            .unwrap_or_else(|| panic!("a row for {sym} in:\n{csv}"));
        let swept: f64 = row
            .split(',')
            .nth(col)
            .expect("a metric cell")
            .parse()
            .expect("a number");
        assert!(
            (swept - standalone).abs() < 1e-9,
            "row for {sym} reports {swept} but a standalone run reports {standalone} — the \
             grid is not backtesting that symbol's own bars"
        );
    }
}
