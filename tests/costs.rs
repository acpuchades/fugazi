//! End-to-end tests of the `fugazi run` / `fugazi optimize` / `fugazi check`
//! subcommands' `--costs` flag over the example candles.
//!
//! Backward-compat: a run without `--costs` produces the pre-costs `trades.csv`
//! header shape (no `commission` column) and a `metrics.yml` that omits the
//! `costs:` section — so an existing pipeline reads it unchanged.
//!
//! With `--costs`, the wallet applies the spread → slippage → commission
//! pipeline; `trades.csv` gains a populated `commission` column and
//! `metrics.yml` gains a `costs:` block with `total_commission`,
//! `total_slippage_cost`, and `cost_drag_pct`.

use std::process::Command;

struct Artefacts {
    trades: String,
    metrics: String,
}

fn run_with(costs_flags: &[&str], out_name: &str) -> Artefacts {
    let manifest = env!("CARGO_MANIFEST_DIR");
    let out = std::env::temp_dir().join(out_name);
    let _ = std::fs::remove_dir_all(&out);

    let mut args: Vec<String> = vec![
        "run".to_string(),
        format!("@{manifest}/examples/strategy.yml"),
        "--series".to_string(),
        format!("@{manifest}/examples/candles.csv"),
        "--output-dir".to_string(),
        out.to_str().unwrap().to_string(),
        "--quiet".to_string(),
    ];
    for f in costs_flags {
        args.push("--costs".to_string());
        args.push(f.to_string());
    }
    let status = Command::new(env!("CARGO_BIN_EXE_fugazi"))
        .args(&args)
        .status()
        .expect("failed to launch the fugazi binary");
    assert!(status.success(), "fugazi run exited with failure");

    Artefacts {
        trades: std::fs::read_to_string(out.join("trades.csv")).expect("trades.csv"),
        metrics: std::fs::read_to_string(out.join("metrics.yml")).expect("metrics.yml"),
    }
}

/// A run without `--costs` matches the pre-costs schema byte-for-byte: no
/// `commission` column on `trades.csv`, no `costs:` section on `metrics.yml`.
#[test]
fn no_costs_flag_preserves_pre_costs_schema() {
    let out = run_with(&[], "fugazi_costs_absent");
    let header = out.trades.lines().next().expect("trades.csv header");
    assert_eq!(
        header, "time,symbol,side,units,price,kind",
        "trades.csv header should not include `commission` when no cost flag was passed"
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
    assert_eq!(a.trades, b.trades, "trades.csv should be identical");
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
    let header = out.trades.lines().next().expect("trades.csv header");
    assert_eq!(
        header, "time,symbol,side,units,price,kind,commission",
        "trades.csv header should include `commission` when a cost model is set"
    );
    // At least one trade row should record a positive commission.
    let has_commission = out
        .trades
        .lines()
        .skip(1)
        .filter_map(|l| l.rsplit(',').next())
        .filter_map(|c| c.parse::<f64>().ok())
        .any(|v| v > 0.0);
    assert!(
        has_commission,
        "expected at least one non-zero commission cell:\n{}",
        out.trades
    );
    // metrics.yml should carry a populated costs section.
    assert!(
        out.metrics.contains("costs:"),
        "metrics.yml should include costs section:\n{}",
        out.metrics
    );
    for field in ["total_commission:", "total_slippage_cost:", "cost_drag_pct:"] {
        assert!(
            out.metrics.contains(field),
            "metrics.yml costs section missing `{field}`:\n{}",
            out.metrics
        );
    }
}

/// The binance preset — a real-world YAML file with `by_symbol` — parses,
/// runs, and populates the same fields.
#[test]
fn binance_preset_end_to_end() {
    let manifest = env!("CARGO_MANIFEST_DIR");
    let out = run_with(
        &[&format!("@{manifest}/examples/binance.yml")],
        "fugazi_costs_binance_preset",
    );
    assert!(
        out.trades.lines().next().unwrap().ends_with(",commission"),
        "binance preset should populate the commission column"
    );
    assert!(
        out.metrics.contains("total_commission:"),
        "binance preset should populate the costs section"
    );
}

/// `check costs` accepts a well-formed spec and rejects an unknown model variant
/// with a non-zero exit code (linting a bad spec at CI time, before a real run).
#[test]
fn check_costs_accepts_valid_and_rejects_invalid() {
    let ok = Command::new(env!("CARGO_BIN_EXE_fugazi"))
        .args(["check", "costs", "commission=!percentage { rate: 0.001 }"])
        .status()
        .expect("failed to launch fugazi");
    assert!(ok.success(), "well-formed cost spec should pass check");

    let bad = Command::new(env!("CARGO_BIN_EXE_fugazi"))
        .args(["check", "costs", "commission=!martian { rate: 0.001 }"])
        .output()
        .expect("failed to launch fugazi");
    assert!(
        !bad.status.success(),
        "unknown model variant should fail check with non-zero exit"
    );
}

/// The `ibkr` preset exercises the nested-model path (`!max` over a `!per_unit`
/// and a `!fixed`), which the binance preset doesn't reach.
#[test]
fn ibkr_preset_end_to_end() {
    let manifest = env!("CARGO_MANIFEST_DIR");
    let out = run_with(
        &[&format!("@{manifest}/examples/ibkr.yml")],
        "fugazi_costs_ibkr_preset",
    );
    assert!(
        out.trades.lines().next().unwrap().ends_with(",commission"),
        "ibkr preset should populate the commission column"
    );
    assert!(
        out.metrics.contains("total_commission:"),
        "ibkr preset should populate the costs section"
    );
}

/// The `SYMBOL[FREQ]:` scope on `--costs` applies to the resolution used by
/// the run, matching against the *effective* cadence — user-set
/// `--frequency` or, absent that, the value auto-detected from the series'
/// `time` column. A BTC[1d]-scoped commission fires for `symbol: BTC` on
/// daily bars (either explicit `-f 1d` or auto-detected); forcing an
/// unrelated `-f 4h` disqualifies the scope and the run falls back to the
/// default. Verified by comparing the `total_commission` cell across the
/// two configurations.
#[test]
fn scope_precedence_applies_at_run_time() {
    let manifest = env!("CARGO_MANIFEST_DIR");
    // The strategy in examples/ trades BTC on daily bars. Set a small default
    // commission and a much larger BTC[1d]-scoped one; only the run whose
    // effective cadence is 1d takes the scoped model.
    let costs = "commission=!percentage { rate: 0.0001 },BTC[1d]:commission=!percentage { rate: 0.05 }";

    // With `-f 4h` the effective cadence is 4h → BTC[1d] doesn't match, so the
    // default (0.01%) fires.
    let out_mismatch = std::env::temp_dir().join("fugazi_costs_scope_mismatch");
    let _ = std::fs::remove_dir_all(&out_mismatch);
    let status = Command::new(env!("CARGO_BIN_EXE_fugazi"))
        .args([
            "run",
            &format!("@{manifest}/examples/strategy.yml"),
            "--series",
            &format!("@{manifest}/examples/candles.csv"),
            "--output-dir",
            out_mismatch.to_str().unwrap(),
            "--frequency",
            "4h",
            "--quiet",
            "--costs",
            costs,
        ])
        .status()
        .expect("failed to launch fugazi");
    assert!(status.success());
    let mismatch_metrics =
        std::fs::read_to_string(out_mismatch.join("metrics.yml")).expect("metrics.yml");

    // With `-f 1d`, the BTC[1d] scoped model wins → commission > 0.
    let out_daily = std::env::temp_dir().join("fugazi_costs_scope_daily");
    let _ = std::fs::remove_dir_all(&out_daily);
    let status = Command::new(env!("CARGO_BIN_EXE_fugazi"))
        .args([
            "run",
            &format!("@{manifest}/examples/strategy.yml"),
            "--series",
            &format!("@{manifest}/examples/candles.csv"),
            "--output-dir",
            out_daily.to_str().unwrap(),
            "--frequency",
            "1d",
            "--quiet",
            "--costs",
            costs,
        ])
        .status()
        .expect("failed to launch fugazi");
    assert!(status.success());
    let daily_metrics =
        std::fs::read_to_string(out_daily.join("metrics.yml")).expect("metrics.yml");

    // Omitting `--frequency` altogether lets the detector pick 1d from the
    // daily-cadence CSV — same total commission as the explicit 1d run.
    let out_detected = std::env::temp_dir().join("fugazi_costs_scope_detected");
    let _ = std::fs::remove_dir_all(&out_detected);
    let status = Command::new(env!("CARGO_BIN_EXE_fugazi"))
        .args([
            "run",
            &format!("@{manifest}/examples/strategy.yml"),
            "--series",
            &format!("@{manifest}/examples/candles.csv"),
            "--output-dir",
            out_detected.to_str().unwrap(),
            "--quiet",
            "--costs",
            costs,
        ])
        .status()
        .expect("failed to launch fugazi");
    assert!(status.success());
    let detected_metrics =
        std::fs::read_to_string(out_detected.join("metrics.yml")).expect("metrics.yml");

    let extract = |m: &str| -> f64 {
        m.lines()
            .find_map(|l| l.trim_start().strip_prefix("total_commission:"))
            .and_then(|s| s.trim().parse::<f64>().ok())
            .unwrap_or_else(|| panic!("total_commission not found in:\n{m}"))
    };
    let mismatch = extract(&mismatch_metrics);
    let daily = extract(&daily_metrics);
    let detected = extract(&detected_metrics);
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

/// Per-leg costs for a `pairs:` strategy: `--costs 'A:...,B:...'` scopes each
/// symbol on its own commission model, so the pairs backtest applies each
/// leg's model to its own fills.
#[test]
fn pairs_run_applies_per_leg_costs() {
    let out = std::env::temp_dir().join("fugazi_pairs_per_leg_costs");
    let _ = std::fs::remove_dir_all(&out);

    // Two-symbol series: A stays flat at 100, B mean-reverts around 90.
    // The strategy trades whenever the spread crosses ±3, so both legs fill.
    let csv = out.parent().unwrap().join("pairs_per_leg_costs.csv");
    let mut rows = String::from("symbol;time;open;high;low;close;volume\n");
    let a_series = [100.0; 20];
    let b_series = [
        90.0, 96.0, 88.0, 96.0, 88.0, 96.0, 88.0, 96.0, 88.0, 96.0, 88.0, 96.0, 88.0, 96.0, 88.0,
        96.0, 88.0, 96.0, 88.0, 96.0,
    ];
    for (i, &p) in a_series.iter().enumerate() {
        rows.push_str(&format!(
            "A;2024-01-{:02};{p};{p};{p};{p};1000\n",
            i + 1
        ));
    }
    for (i, &p) in b_series.iter().enumerate() {
        rows.push_str(&format!(
            "B;2024-01-{:02};{p};{p};{p};{p};1000\n",
            i + 1
        ));
    }
    std::fs::write(&csv, rows).expect("write pairs csv");

    // A pairs spec that enters when spread crosses out and exits when it
    // reverts. Signals rooted through `!pick { symbol: <SYM> }`.
    let pairs_yaml = out.parent().unwrap().join("pairs_per_leg_costs.yml");
    std::fs::write(
        &pairs_yaml,
        r#"
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
"#,
    )
    .expect("write pairs yaml");

    // Per-leg commissions: A on a 10% rate, B on a 1% rate. If the CLI applied
    // one bundle to both legs (the pre-refactor behavior) both would carry the
    // same commission.
    let status = Command::new(env!("CARGO_BIN_EXE_fugazi"))
        .arg("run")
        .arg(format!("pairs:@{}", pairs_yaml.to_str().unwrap()))
        .arg("--series")
        .arg(format!("@{}", csv.to_str().unwrap()))
        .args([
            "--output-dir",
            out.to_str().unwrap(),
            "--crypto",
            "-f",
            "1d",
            "--quiet",
            "--costs",
            "A:commission=!percentage { rate: 0.10 },B:commission=!percentage { rate: 0.01 }",
        ])
        .status()
        .expect("failed to launch fugazi");
    let _ = std::fs::remove_file(&csv);
    let _ = std::fs::remove_file(&pairs_yaml);
    assert!(status.success(), "fugazi pairs run exited with failure");

    let trades = std::fs::read_to_string(out.join("trades.csv")).expect("trades.csv");
    let header = trades.lines().next().unwrap();
    assert_eq!(header, "time,symbol,side,units,price,kind,commission");

    // Collect commission rates (commission / notional) per leg.
    let mut a_rates = Vec::new();
    let mut b_rates = Vec::new();
    for row in trades.lines().skip(1) {
        let cols: Vec<&str> = row.split(',').collect();
        assert_eq!(cols.len(), 7);
        let sym = cols[1];
        let units: f64 = cols[3].parse().unwrap();
        let price: f64 = cols[4].parse().unwrap();
        let commission: f64 = cols[6].parse().unwrap();
        let notional = units * price;
        let rate = commission / notional;
        if sym == "A" {
            a_rates.push(rate);
        } else if sym == "B" {
            b_rates.push(rate);
        }
    }
    assert!(!a_rates.is_empty(), "expected A fills:\n{trades}");
    assert!(!b_rates.is_empty(), "expected B fills:\n{trades}");
    for r in &a_rates {
        assert!((r - 0.10).abs() < 1e-6, "A leg should pay 10%: got {r}");
    }
    for r in &b_rates {
        assert!((r - 0.01).abs() < 1e-6, "B leg should pay 1%: got {r}");
    }
}

/// A pairs runtime driver that lets us reuse one CSV fixture across
/// unscoped / frequency-scoped / symbol-scoped tests. Runs the strategy
/// with `costs` (verbatim), parses `trades.csv`, returns the per-leg
/// commission rates (`commission / notional`) as a `(Vec<f64>, Vec<f64>)`
/// keyed on `A`/`B`.
fn run_pairs_with_costs(out_name: &str, costs: &str) -> (Vec<f64>, Vec<f64>) {
    let out = std::env::temp_dir().join(out_name);
    let _ = std::fs::remove_dir_all(&out);
    // Same fixture as `pairs_run_applies_per_leg_costs`: A flat, B mean-reverts.
    let csv = out
        .parent()
        .unwrap()
        .join(format!("{out_name}_series.csv"));
    let mut rows = String::from("symbol;time;open;high;low;close;volume\n");
    let a_series = [100.0; 20];
    let b_series = [
        90.0, 96.0, 88.0, 96.0, 88.0, 96.0, 88.0, 96.0, 88.0, 96.0, 88.0, 96.0, 88.0, 96.0, 88.0,
        96.0, 88.0, 96.0, 88.0, 96.0,
    ];
    for (i, &p) in a_series.iter().enumerate() {
        rows.push_str(&format!(
            "A;2024-01-{:02};{p};{p};{p};{p};1000\n",
            i + 1
        ));
    }
    for (i, &p) in b_series.iter().enumerate() {
        rows.push_str(&format!(
            "B;2024-01-{:02};{p};{p};{p};{p};1000\n",
            i + 1
        ));
    }
    std::fs::write(&csv, rows).expect("write pairs csv");

    let pairs_yaml = out
        .parent()
        .unwrap()
        .join(format!("{out_name}_strategy.yml"));
    std::fs::write(
        &pairs_yaml,
        r#"
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
"#,
    )
    .expect("write pairs yaml");

    let status = Command::new(env!("CARGO_BIN_EXE_fugazi"))
        .arg("run")
        .arg(format!("pairs:@{}", pairs_yaml.to_str().unwrap()))
        .arg("--series")
        .arg(format!("@{}", csv.to_str().unwrap()))
        .args([
            "--output-dir",
            out.to_str().unwrap(),
            "--crypto",
            "-f",
            "1d",
            "--quiet",
            "--costs",
            costs,
        ])
        .status()
        .expect("failed to launch fugazi");
    let trades = std::fs::read_to_string(out.join("trades.csv")).expect("trades.csv");
    let _ = std::fs::remove_file(&csv);
    let _ = std::fs::remove_file(&pairs_yaml);
    assert!(status.success(), "fugazi pairs run exited with failure");

    let mut a = Vec::new();
    let mut b = Vec::new();
    for row in trades.lines().skip(1) {
        let cols: Vec<&str> = row.split(',').collect();
        let sym = cols[1];
        let units: f64 = cols[3].parse().unwrap();
        let price: f64 = cols[4].parse().unwrap();
        let commission: f64 = cols[6].parse().unwrap();
        let rate = commission / (units * price);
        if sym == "A" {
            a.push(rate);
        } else if sym == "B" {
            b.push(rate);
        }
    }
    (a, b)
}

/// An **unscoped** (global) commission applies to *every* traded symbol in a
/// pairs run — the CLI resolves the cost config per leg, and each leg falls
/// through to the default when no scope matches.
#[test]
fn pairs_run_applies_global_default_costs_to_every_leg() {
    let (a_rates, b_rates) = run_pairs_with_costs(
        "fugazi_pairs_global_default",
        "commission=!percentage { rate: 0.03 }",
    );
    assert!(!a_rates.is_empty() && !b_rates.is_empty());
    for r in a_rates.iter().chain(b_rates.iter()) {
        assert!((r - 0.03).abs() < 1e-6, "expected 3% everywhere, got {r}");
    }
}

/// A **frequency-scoped** commission (`[1d]:commission=...`) fires for every
/// symbol that trades on 1d bars — the CLI resolves per-leg with the
/// effective bar cadence, so a `by_interval[1d]` scope catches both legs of
/// a pairs run on daily data.
#[test]
fn pairs_run_applies_frequency_scoped_costs_to_every_leg() {
    let (a_rates, b_rates) = run_pairs_with_costs(
        "fugazi_pairs_freq_scope",
        "[1d]:commission=!percentage { rate: 0.05 }",
    );
    assert!(!a_rates.is_empty() && !b_rates.is_empty());
    for r in a_rates.iter().chain(b_rates.iter()) {
        assert!((r - 0.05).abs() < 1e-6, "expected 5% on both legs, got {r}");
    }
}

/// **Mixed**: an unscoped default plus a symbol-scoped override for one leg
/// only. The scoped leg picks the override (specificity wins), the other
/// leg falls back to the global default.
#[test]
fn pairs_run_mixes_global_default_with_symbol_override() {
    let (a_rates, b_rates) = run_pairs_with_costs(
        "fugazi_pairs_mixed_default",
        "commission=!percentage { rate: 0.001 },A:commission=!percentage { rate: 0.05 }",
    );
    assert!(!a_rates.is_empty() && !b_rates.is_empty());
    for r in &a_rates {
        assert!((r - 0.05).abs() < 1e-6, "A: symbol scope should win, got {r}");
    }
    for r in &b_rates {
        assert!(
            (r - 0.001).abs() < 1e-6,
            "B: unscoped default should apply, got {r}"
        );
    }
}

/// When two `--costs` terms with the same scope are given, the later one wins
/// (matching `--params`'s left-to-right override rule).
#[test]
fn later_term_wins_at_same_scope() {
    let manifest = env!("CARGO_MANIFEST_DIR");
    let out_first = std::env::temp_dir().join("fugazi_costs_first");
    let out_second = std::env::temp_dir().join("fugazi_costs_second");
    let _ = std::fs::remove_dir_all(&out_first);
    let _ = std::fs::remove_dir_all(&out_second);

    // First run: only the "wins" 5% commission.
    let status = Command::new(env!("CARGO_BIN_EXE_fugazi"))
        .args([
            "run",
            &format!("@{manifest}/examples/strategy.yml"),
            "--series",
            &format!("@{manifest}/examples/candles.csv"),
            "--output-dir",
            out_first.to_str().unwrap(),
            "--quiet",
            "--costs",
            "commission=!percentage { rate: 0.05 }",
        ])
        .status()
        .expect("failed to launch fugazi");
    assert!(status.success());
    let first = std::fs::read_to_string(out_first.join("metrics.yml")).unwrap();

    // Second run: the 0% is set first, then 5% overrides.
    let status = Command::new(env!("CARGO_BIN_EXE_fugazi"))
        .args([
            "run",
            &format!("@{manifest}/examples/strategy.yml"),
            "--series",
            &format!("@{manifest}/examples/candles.csv"),
            "--output-dir",
            out_second.to_str().unwrap(),
            "--quiet",
            "--costs",
            "commission=!percentage { rate: 0.0 }",
            "--costs",
            "commission=!percentage { rate: 0.05 }",
        ])
        .status()
        .expect("failed to launch fugazi");
    assert!(status.success());
    let second = std::fs::read_to_string(out_second.join("metrics.yml")).unwrap();
    // Same "wins" commission → same total_commission.
    let extract = |m: &str| -> Option<String> {
        m.lines()
            .find(|l| l.trim_start().starts_with("total_commission:"))
            .map(|l| l.trim().to_string())
    };
    assert_eq!(extract(&first), extract(&second));
}
