//! End-to-end tests of the `fugazi run` / `fugazi check` subcommands'
//! `--costs` flag over the example candles.
//!
//! Backward-compat: a run without `--costs` produces the pre-costs `fills.csv`
//! header shape (no `commission` column) and a `metrics.yml` that omits the
//! `costs:` section — so an existing pipeline reads it unchanged.
//!
//! With `--costs`, the wallet applies the spread → slippage → commission
//! pipeline; `fills.csv` gains a populated `commission` column and
//! `metrics.yml` gains a `costs:` block with `total_commission`,
//! `total_slippage_cost`, and `cost_drag_pct`.

mod common;

use common::cli::{Artefacts, Cmd, at, scratch_file};

const NO_COSTS_HEADER: &str = "time,symbol,side,units,price,kind";
const COSTS_HEADER: &str = "time,symbol,side,units,price,kind,commission";

/// `fugazi run examples/strategy.yml` with zero or more `--costs` terms.
fn run_with(costs_flags: &[&str], out_name: &str) -> Artefacts {
    let mut cmd = Cmd::new("run")
        .arg(&at("examples/strategy.yml"))
        .series(&at("examples/candles.csv"))
        .arg("--quiet")
        .output_dir(out_name);
    for f in costs_flags {
        cmd = cmd.costs(f);
    }
    cmd.ok().artefacts()
}

/// The `total_commission:` scalar out of a `metrics.yml`.
#[track_caller]
fn total_commission(metrics: &str) -> f64 {
    metrics
        .lines()
        .find_map(|l| l.trim_start().strip_prefix("total_commission:"))
        .and_then(|s| s.trim().parse::<f64>().ok())
        .unwrap_or_else(|| panic!("total_commission not found in:\n{metrics}"))
}

// ---------------------------------------------------------------------------
// Single-asset runs
// ---------------------------------------------------------------------------

/// A run without `--costs` matches the pre-costs schema byte-for-byte: no
/// `commission` column on `fills.csv`, no `costs:` section on `metrics.yml`.
#[test]
fn no_costs_flag_preserves_pre_costs_schema() {
    let out = run_with(&[], "fugazi_costs_absent");
    let header = out.fills.lines().next().expect("fills.csv header");
    assert_eq!(
        header, NO_COSTS_HEADER,
        "fills.csv header should not include `commission` when no cost flag was passed"
    );
    assert!(
        !out.metrics.contains("costs:"),
        "metrics.yml should omit costs section when no cost flag was passed:\n{}",
        out.metrics
    );
}

/// `--costs none` opts into the frictionless behavior explicitly (silencing
/// the warning banner) — output shape is still the zero-cost one.
#[test]
fn costs_none_matches_no_costs_schema() {
    let a = run_with(&[], "fugazi_costs_none_a");
    let b = run_with(&["none"], "fugazi_costs_none_b");
    assert_eq!(a.fills, b.fills, "fills.csv should be identical");
    assert_eq!(a.metrics, b.metrics, "metrics.yml should be identical");
}

/// A run with a non-trivial cost model gains a `commission` column populated
/// with non-zero values, and a `costs:` block on `metrics.yml`.
#[test]
fn costs_flag_populates_commission_and_costs_section() {
    let out = run_with(
        &["commission=!percentage { rate: 0.001 },spread=!bps { bps: 5 }"],
        "fugazi_costs_binance_like",
    );
    let header = out.fills.lines().next().expect("fills.csv header");
    assert_eq!(
        header, COSTS_HEADER,
        "fills.csv header should include `commission` when a cost model is set"
    );
    // At least one fill row should record a positive commission.
    let has_commission = out
        .fills
        .lines()
        .skip(1)
        .filter_map(|l| l.rsplit(',').next())
        .filter_map(|c| c.parse::<f64>().ok())
        .any(|v| v > 0.0);
    assert!(
        has_commission,
        "expected at least one non-zero commission cell:\n{}",
        out.fills
    );
    // metrics.yml should carry a populated costs section.
    assert!(
        out.metrics.contains("costs:"),
        "metrics.yml should include costs section:\n{}",
        out.metrics
    );
    for field in [
        "total_commission:",
        "total_slippage_cost:",
        "cost_drag_pct:",
    ] {
        assert!(
            out.metrics.contains(field),
            "metrics.yml costs section missing `{field}`:\n{}",
            out.metrics
        );
    }
}

/// Costs must *reduce* the run's realized P&L: the same strategy over the same
/// bars with a 5% round-turn commission cannot finish richer than the
/// frictionless run. Pins the direction the whole cost pipeline exists to
/// produce — the shape assertions above would pass just as happily if the
/// commission column were computed and then never charged.
#[test]
fn costs_drag_the_equity_curve_down() {
    let free = run_with(&["none"], "fugazi_costs_drag_free");
    let charged = run_with(
        &["commission=!percentage { rate: 0.05 }"],
        "fugazi_costs_drag_charged",
    );

    let final_equity = |m: &str| -> f64 {
        m.lines()
            .find_map(|l| l.trim_start().strip_prefix("final_equity:"))
            .and_then(|s| s.trim().parse::<f64>().ok())
            .unwrap_or_else(|| panic!("final_equity not found in:\n{m}"))
    };
    let (free_eq, charged_eq) = (final_equity(&free.metrics), final_equity(&charged.metrics));
    assert!(
        charged_eq < free_eq,
        "a 5% commission should leave less equity than none: {charged_eq} vs {free_eq}"
    );
    assert!(
        total_commission(&charged.metrics) > 0.0,
        "the charged run should book commission:\n{}",
        charged.metrics
    );
}

/// `--flatten` closes through the cost pipeline, so its closing legs appear in
/// the priced *and* the zero-cost gross run.
///
/// The gross twin exists only to attribute costs: `costs_section` pairs net
/// fills against gross fills bar-for-bar, so a closing leg present in one and
/// absent from the other would drop straight out of `total_slippage_cost` and
/// understate the drag — silently, since nothing else compares the two counts.
#[test]
fn flatten_books_its_closing_legs_in_both_the_priced_and_gross_runs() {
    let flat = Cmd::new("run")
        .arg(&at("examples/strategy.yml"))
        .series(&at("examples/candles.csv"))
        .costs("commission=!percentage { rate: 0.05 }")
        .arg("--flatten")
        .arg("--quiet")
        .output_dir("fugazi_costs_flatten")
        .ok()
        .artefacts();
    let carried = run_with(
        &["commission=!percentage { rate: 0.05 }"],
        "fugazi_costs_flatten_carried",
    );

    // The flattened run books strictly more fills — the closing legs.
    let count = |a: &Artefacts| a.fills.lines().count();
    assert!(
        count(&flat) > count(&carried),
        "--flatten should book closing legs: {} vs {}",
        count(&flat),
        count(&carried)
    );
    // And the drag is still attributed: a flatten leg that the gross twin never
    // booked would leave this at the carried run's value or below.
    assert!(
        total_commission(&flat.metrics) >= total_commission(&carried.metrics),
        "flatten's closing legs must carry commission too:\n{}",
        flat.metrics
    );
}

/// The binance preset — a real-world YAML file with `by_symbol` — parses,
/// runs, and populates the same fields.
#[test]
fn binance_preset_end_to_end() {
    let out = run_with(
        &[&at("examples/binance.yml")],
        "fugazi_costs_binance_preset",
    );
    assert!(
        out.fills.lines().next().unwrap().ends_with(",commission"),
        "binance preset should populate the commission column"
    );
    assert!(
        out.metrics.contains("total_commission:"),
        "binance preset should populate the costs section"
    );
}

/// The `ibkr` preset exercises the nested-model path (`!max` over a `!per_unit`
/// and a `!fixed`), which the binance preset doesn't reach.
#[test]
fn ibkr_preset_end_to_end() {
    let out = run_with(&[&at("examples/ibkr.yml")], "fugazi_costs_ibkr_preset");
    assert!(
        out.fills.lines().next().unwrap().ends_with(",commission"),
        "ibkr preset should populate the commission column"
    );
    assert!(
        out.metrics.contains("total_commission:"),
        "ibkr preset should populate the costs section"
    );
}

/// `check costs` accepts a well-formed spec and rejects an unknown model variant
/// with a non-zero exit code (linting a bad spec at CI time, before a real run).
#[test]
fn check_costs_accepts_valid_and_rejects_invalid() {
    Cmd::new("check")
        .args(&["costs", "commission=!percentage { rate: 0.001 }"])
        .ok();

    let bad = Cmd::new("check")
        .args(&["costs", "commission=!martian { rate: 0.001 }"])
        .fails();
    assert!(
        format!("{}{}", bad.stderr, bad.stdout).contains("martian"),
        "the diagnostic should name the unknown variant, got:\n{}",
        bad.stderr
    );
}

/// The `SYMBOL[FREQ]:` scope on `--costs` applies to the resolution used by
/// the run, matching against the *effective* cadence — user-set
/// `--frequency` or, absent that, the value auto-detected from the series'
/// `time` column. A BTC[1d]-scoped commission fires for `symbol: BTC` on
/// daily bars (either explicit `-f 1d` or auto-detected); forcing an
/// unrelated `-f 4h` disqualifies the scope and the run falls back to the
/// default. Verified by comparing the `total_commission` cell across the
/// three configurations.
#[test]
fn scope_precedence_applies_at_run_time() {
    // The strategy in examples/ trades BTC on daily bars. Set a small default
    // commission and a much larger BTC[1d]-scoped one; only the run whose
    // effective cadence is 1d takes the scoped model.
    let costs =
        "commission=!percentage { rate: 0.0001 },BTC[1d]:commission=!percentage { rate: 0.05 }";

    let run = |name: &str, freq: Option<&str>| -> f64 {
        let mut cmd = Cmd::new("run")
            .arg(&at("examples/strategy.yml"))
            .series(&at("examples/candles.csv"))
            .arg("--quiet")
            .costs(costs)
            .output_dir(name);
        if let Some(f) = freq {
            cmd = cmd.args(&["--frequency", f]);
        }
        total_commission(&cmd.ok().read("metrics.yml"))
    };

    // With `-f 4h` the effective cadence is 4h → BTC[1d] doesn't match, so the
    // default (0.01%) fires.
    let mismatch = run("fugazi_costs_scope_mismatch", Some("4h"));
    // With `-f 1d`, the BTC[1d] scoped model wins → commission > 0.
    let daily = run("fugazi_costs_scope_daily", Some("1d"));
    // Omitting `--frequency` altogether lets the detector pick 1d from the
    // daily-cadence CSV — same total commission as the explicit 1d run.
    let detected = run("fugazi_costs_scope_detected", None);

    // Same fill schedule; scoped rate 0.05 vs default 0.0001 → ~500× larger.
    assert!(
        daily > mismatch * 100.0,
        "daily ({daily}) should dominate mismatch ({mismatch})",
    );
    // Detection routes the same 1d into the cost resolver, so the omitted-freq
    // run matches the explicit-`-f 1d` run cell-for-cell.
    assert_eq!(
        detected, daily,
        "detected 1d should reproduce explicit `-f 1d` total commission",
    );
}

/// When two `--costs` terms with the same scope are given, the later one wins
/// (matching `--params`'s left-to-right override rule).
#[test]
fn later_term_wins_at_same_scope() {
    // Only the "wins" 5% commission.
    let alone = run_with(
        &["commission=!percentage { rate: 0.05 }"],
        "fugazi_costs_first",
    );
    // The 0% is set first, then the same 5% overrides it.
    let overridden = run_with(
        &[
            "commission=!percentage { rate: 0.0 }",
            "commission=!percentage { rate: 0.05 }",
        ],
        "fugazi_costs_second",
    );
    assert_eq!(
        total_commission(&alone.metrics),
        total_commission(&overridden.metrics),
        "the later term should win, reproducing the alone-5% run"
    );
    // Guard the guard: with the terms the other way round the 0% must win, so
    // the run has to reproduce the zero-rate-alone run rather than the 5% one.
    // (A zero-rate model books no commission, and the writer omits an empty
    // `costs:` section entirely — so this compares whole documents rather than
    // a `total_commission` cell that isn't there.)
    let reversed = run_with(
        &[
            "commission=!percentage { rate: 0.05 }",
            "commission=!percentage { rate: 0.0 }",
        ],
        "fugazi_costs_reversed",
    );
    let zero_only = run_with(
        &["commission=!percentage { rate: 0.0 }"],
        "fugazi_costs_zero_only",
    );
    assert_eq!(
        reversed.metrics, zero_only.metrics,
        "a trailing 0% term should win, reproducing the zero-rate-alone run"
    );
    assert_ne!(
        reversed.metrics, alone.metrics,
        "…and must not silently keep the earlier 5% term"
    );
}

// ---------------------------------------------------------------------------
// Pairs runs — per-leg cost resolution
// ---------------------------------------------------------------------------

/// Two aligned 20-bar series: `A` flat at 100, `B` mean-reverting between 88
/// and 96. The spread therefore crosses the entry (`> 8`) and exit (`< 2`)
/// levels repeatedly, so **both legs fill several times** — which is what makes
/// per-leg commission rates measurable.
fn pairs_series_csv() -> String {
    let mut rows = String::from("symbol;time;open;high;low;close;volume\n");
    let a = [100.0; 20];
    // `b[0]` is 90 rather than 88 so the run opens inside the band and the
    // first entry is a genuine crossing rather than a warm-up artefact.
    let b = [
        90.0, 96.0, 88.0, 96.0, 88.0, 96.0, 88.0, 96.0, 88.0, 96.0, 88.0, 96.0, 88.0, 96.0, 88.0,
        96.0, 88.0, 96.0, 88.0, 96.0,
    ];
    for (sym, series) in [("A", &a[..]), ("B", &b[..])] {
        for (i, &p) in series.iter().enumerate() {
            rows.push_str(&format!(
                "{sym};2024-01-{:02};{p};{p};{p};{p};1000\n",
                i + 1
            ));
        }
    }
    rows
}

const PAIRS_YAML: &str = r#"
left: A
right: B
enter: !above
  source: !sub
    lhs: !close { source: !pick { symbol: A } }
    rhs: !close { source: !pick { symbol: B } }
  level: 8.0
exit: !below
  source: !sub
    lhs: !close { source: !pick { symbol: A } }
    rhs: !close { source: !pick { symbol: B } }
  level: 2.0
"#;

/// Run the pairs fixture with `costs` (verbatim) and return the realized
/// commission **rate** (`commission / notional`) of every fill, split by leg.
///
/// Reading the rate rather than the absolute commission is what makes the
/// assertions independent of how many times the strategy happened to trade.
fn pairs_commission_rates(out_name: &str, costs: &str) -> (Vec<f64>, Vec<f64>) {
    let (_csv, series) = scratch_file(&format!("{out_name}_series.csv"), &pairs_series_csv());
    let (_yml, strategy) = scratch_file(&format!("{out_name}_strategy.yml"), PAIRS_YAML);

    let out = Cmd::new("run")
        .arg(&format!("pairs:{strategy}"))
        .series(&series)
        .args(&["--crypto", "-f", "1d", "--quiet"])
        .costs(costs)
        .output_dir(out_name)
        .ok();

    let fills = out.read("fills.csv");
    assert_eq!(
        fills.lines().next().unwrap(),
        COSTS_HEADER,
        "a costed pairs run should emit the commission column"
    );

    let (mut a, mut b) = (Vec::new(), Vec::new());
    for row in fills.lines().skip(1) {
        let cols: Vec<&str> = row.split(',').collect();
        assert_eq!(cols.len(), 7, "unexpected fills.csv row: {row}");
        let units: f64 = cols[3].parse().expect("units");
        let price: f64 = cols[4].parse().expect("price");
        let commission: f64 = cols[6].parse().expect("commission");
        let rate = commission / (units * price);
        match cols[1] {
            "A" => a.push(rate),
            "B" => b.push(rate),
            other => panic!("unexpected symbol `{other}` in fills.csv"),
        }
    }
    assert!(!a.is_empty(), "expected A fills:\n{fills}");
    assert!(!b.is_empty(), "expected B fills:\n{fills}");
    (a, b)
}

#[track_caller]
fn assert_all_rates(rates: &[f64], want: f64, what: &str) {
    for r in rates {
        assert!(
            (r - want).abs() < 1e-6,
            "{what}: expected rate {want}, got {r}"
        );
    }
}

/// Per-leg costs for a `pairs:` strategy: `--costs 'A:...,B:...'` scopes each
/// symbol on its own commission model, so the pairs backtest applies each
/// leg's model to its own fills. If the CLI applied one bundle to both legs
/// (the pre-refactor behavior) both would carry the same rate.
#[test]
fn pairs_run_applies_per_leg_costs() {
    let (a, b) = pairs_commission_rates(
        "fugazi_pairs_per_leg_costs",
        "A:commission=!percentage { rate: 0.10 },B:commission=!percentage { rate: 0.01 }",
    );
    assert_all_rates(&a, 0.10, "A leg");
    assert_all_rates(&b, 0.01, "B leg");
}

/// An **unscoped** (global) commission applies to *every* traded symbol in a
/// pairs run — the CLI resolves the cost config per leg, and each leg falls
/// through to the default when no scope matches.
#[test]
fn pairs_run_applies_global_default_costs_to_every_leg() {
    let (a, b) = pairs_commission_rates(
        "fugazi_pairs_global_default",
        "commission=!percentage { rate: 0.03 }",
    );
    assert_all_rates(&a, 0.03, "A leg");
    assert_all_rates(&b, 0.03, "B leg");
}

/// A **frequency-scoped** commission (`[1d]:commission=...`) fires for every
/// symbol that trades on 1d bars — the CLI resolves per-leg with the
/// effective bar cadence, so a `by_interval[1d]` scope catches both legs of
/// a pairs run on daily data.
#[test]
fn pairs_run_applies_frequency_scoped_costs_to_every_leg() {
    let (a, b) = pairs_commission_rates(
        "fugazi_pairs_freq_scope",
        "[1d]:commission=!percentage { rate: 0.05 }",
    );
    assert_all_rates(&a, 0.05, "A leg");
    assert_all_rates(&b, 0.05, "B leg");
}

/// **Mixed**: an unscoped default plus a symbol-scoped override for one leg
/// only. The scoped leg picks the override (specificity wins), the other
/// leg falls back to the global default.
#[test]
fn pairs_run_mixes_global_default_with_symbol_override() {
    let (a, b) = pairs_commission_rates(
        "fugazi_pairs_mixed_default",
        "commission=!percentage { rate: 0.001 },A:commission=!percentage { rate: 0.05 }",
    );
    assert_all_rates(&a, 0.05, "A leg (symbol scope should win)");
    assert_all_rates(&b, 0.001, "B leg (unscoped default should apply)");
}
