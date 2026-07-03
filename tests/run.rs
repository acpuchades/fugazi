//! End-to-end tests of the `fugazi run` backtester binary over the example
//! candles, asserting it produces non-trivial result files for both an `@file`
//! strategy and an inline one.

use std::process::Command;

/// The three result artefacts a run writes into its `--output-dir`.
struct Artefacts {
    trades: String,
    returns: String,
    metrics: String,
}

/// Run the binary with the given `--strategy` value into a fresh `out_name`
/// scratch dir, asserting success, and return its result files.
fn run_backtest(out_name: &str, strategy: &str) -> Artefacts {
    let manifest = env!("CARGO_MANIFEST_DIR");
    let out = std::env::temp_dir().join(out_name);
    let _ = std::fs::remove_dir_all(&out);

    let status = Command::new(env!("CARGO_BIN_EXE_fugazi"))
        .args([
            "run",
            strategy, // positional
            "--series",
            &format!("@{manifest}/examples/candles.csv"),
            "--output-dir",
            out.to_str().unwrap(),
        ])
        .status()
        .expect("failed to launch the fugazi binary");
    assert!(status.success(), "fugazi run exited with failure");

    Artefacts {
        trades: std::fs::read_to_string(out.join("trades.csv")).expect("trades.csv"),
        returns: std::fs::read_to_string(out.join("returns.csv")).expect("returns.csv"),
        metrics: std::fs::read_to_string(out.join("metrics.yml")).expect("metrics.yml"),
    }
}

/// The metrics YAML should be a top-level mapping with every section header the
/// crate's `compute()` produces — enough to catch a missing section or a rename
/// without hard-coding the numeric values (which move with the fixture).
fn assert_metrics_shape(metrics: &str) {
    for section in ["run:", "returns:", "risk_adjusted:", "drawdown:", "trades:"] {
        assert!(
            metrics.contains(section),
            "metrics.yml missing `{section}` section:\n{metrics}"
        );
    }
}

#[test]
fn runs_an_at_file_strategy() {
    let manifest = env!("CARGO_MANIFEST_DIR");
    let out = run_backtest(
        "fugazi_e2e_file",
        &format!("@{manifest}/examples/strategy.yml"),
    );

    assert!(
        out.trades.starts_with("time;symbol;side;units;price"),
        "unexpected trades.csv header: {}",
        out.trades
    );
    assert!(
        out.trades.lines().count() >= 2,
        "expected at least one trade, got:\n{}",
        out.trades
    );
    // Header + one row per candle (30 bars in the example).
    assert!(
        out.returns.lines().count() >= 2,
        "expected an equity curve, got:\n{}",
        out.returns
    );
    assert_metrics_shape(&out.metrics);
}

/// `-w/--windowed N` writes `metrics.csv` (one row per non-overlapping N-bar
/// window, tagged with the window's start/end times) instead of `metrics.yml`.
#[test]
fn runs_windowed_metrics() {
    let manifest = env!("CARGO_MANIFEST_DIR");
    let out = std::env::temp_dir().join("fugazi_e2e_windowed");
    let _ = std::fs::remove_dir_all(&out);

    let status = Command::new(env!("CARGO_BIN_EXE_fugazi"))
        .args([
            "run",
            &format!("@{manifest}/examples/strategy.yml"),
            "--series",
            &format!("@{manifest}/examples/candles.csv"),
            "--output-dir",
            out.to_str().unwrap(),
            "--windowed",
            "10",
        ])
        .status()
        .expect("failed to launch the fugazi binary");
    assert!(status.success(), "fugazi run -w exited with failure");

    assert!(
        !out.join("metrics.yml").exists(),
        "windowed run should not write metrics.yml"
    );
    let metrics = std::fs::read_to_string(out.join("metrics.csv")).expect("metrics.csv");
    let mut lines = metrics.lines();
    let header = lines.next().expect("metrics.csv header");
    assert!(
        header.starts_with("window_start;window_end;run.bars;"),
        "unexpected metrics.csv header: {header}"
    );
    for section in ["returns.total_pct", "risk_adjusted.sharpe", "drawdown.max_pct"] {
        assert!(
            header.contains(section),
            "metrics.csv header missing `{section}`: {header}"
        );
    }
    // The SMA(3)/SMA(8) crossover entry warms up in 9 bars (gt: 8, edge: +1;
    // FIR, so stable = warm-up): the measured range starts at bar 9
    // (2024-01-10) and its 21 bars split into 10 + 10 + 1 → 3 windows.
    let rows: Vec<&str> = lines.collect();
    assert_eq!(rows.len(), 3, "expected one row per window:\n{metrics}");
    assert!(
        rows[0].starts_with("2024-01-10;"),
        "first window should start at the stability-gate anchor: {}",
        rows[0]
    );
}

#[test]
fn runs_an_inline_strategy() {
    // A bare (non-`@`) value is the strategy YAML itself.
    let out = run_backtest(
        "fugazi_e2e_inline",
        "symbol: BTC\nlong:\n  enter: !crosses_above { lhs: !sma { source: close, period: 2 }, rhs: !sma { source: close, period: 4 } }\n",
    );

    assert!(
        out.trades.starts_with("time;symbol;side;units;price"),
        "unexpected trades.csv header: {}",
        out.trades
    );
    assert!(
        out.trades.lines().count() >= 2,
        "expected at least one trade, got:\n{}",
        out.trades
    );
    assert!(
        out.returns.lines().count() >= 2,
        "expected an equity curve"
    );
    assert_metrics_shape(&out.metrics);
}
