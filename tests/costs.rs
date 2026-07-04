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
        header, "time;symbol;side;units;price;kind",
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
        header, "time;symbol;side;units;price;kind;commission",
        "trades.csv header should include `commission` when a cost model is set"
    );
    // At least one trade row should record a positive commission.
    let has_commission = out
        .trades
        .lines()
        .skip(1)
        .filter_map(|l| l.rsplit(';').next())
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
        out.trades.lines().next().unwrap().ends_with(";commission"),
        "binance preset should populate the commission column"
    );
    assert!(
        out.metrics.contains("total_commission:"),
        "binance preset should populate the costs section"
    );
}

/// `check costs` accepts a well-formed spec and rejects an unknown `kind:` with
/// a non-zero exit code (linting a bad spec at CI time, before a real run).
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
        "unknown `kind:` should fail check with non-zero exit"
    );
}

/// The `SYMBOL[FREQ]:` scope on `--costs` applies to the resolution used by
/// the run: a BTC[1d]-scoped commission fires for `symbol: BTC`+`--frequency
/// 1d` but the run against the same series without `--frequency` falls back
/// to the global default. Verified by comparing the `total_commission` cell
/// across two configurations.
#[test]
fn scope_precedence_applies_at_run_time() {
    let manifest = env!("CARGO_MANIFEST_DIR");
    // The strategy in examples/ trades BTC. Set a small default commission and
    // a much larger BTC[1d]-scoped one; the frequency-aware run should take
    // the scoped model (higher total commission).
    let costs = "commission=!percentage { rate: 0.0001 },BTC[1d]:commission=!percentage { rate: 0.05 }";

    // Without --frequency, the freq-agnostic resolution should fall through
    // scoped and pick default = 0%.
    let out_no_freq = std::env::temp_dir().join("fugazi_costs_scope_no_freq");
    let _ = std::fs::remove_dir_all(&out_no_freq);
    let status = Command::new(env!("CARGO_BIN_EXE_fugazi"))
        .args([
            "run",
            &format!("@{manifest}/examples/strategy.yml"),
            "--series",
            &format!("@{manifest}/examples/candles.csv"),
            "--output-dir",
            out_no_freq.to_str().unwrap(),
            "--quiet",
            "--costs",
            costs,
        ])
        .status()
        .expect("failed to launch fugazi");
    assert!(status.success());
    let no_freq_metrics =
        std::fs::read_to_string(out_no_freq.join("metrics.yml")).expect("metrics.yml");

    // With `--frequency 1d`, the BTC[1d] scoped model wins → commission > 0.
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

    // The daily run should record a strictly higher total commission than the
    // no-freq run: same fill schedule, scoped rate 0.05 vs default 0.0001 —
    // ~500× larger.
    let extract = |m: &str| -> f64 {
        m.lines()
            .find_map(|l| l.trim_start().strip_prefix("total_commission:"))
            .and_then(|s| s.trim().parse::<f64>().ok())
            .unwrap_or_else(|| panic!("total_commission not found in:\n{m}"))
    };
    let n = extract(&no_freq_metrics);
    let d = extract(&daily_metrics);
    assert!(d > n * 100.0, "daily ({d}) should dominate no-freq ({n})");
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
