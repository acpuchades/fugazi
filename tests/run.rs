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
    // The SMA(3)/SMA(8) crossover entry has `stable_period() = 9` (gt: 8, edge: +1;
    // FIR, so stable = warm-up). Under the activation-based crop, the strategy
    // reports itself active at bar 9 (0-based index 8, calendar 2024-01-09), so the
    // measured range starts *at* the activation bar — 22 bars split into
    // 10 + 10 + 2 → 3 windows.
    let rows: Vec<&str> = lines.collect();
    assert_eq!(rows.len(), 3, "expected one row per window:\n{metrics}");
    assert!(
        rows[0].starts_with("2024-01-09;"),
        "first window should start at the activation bar: {}",
        rows[0]
    );
}

/// End-to-end wiring for the `!latch` + `!resample` tag pair — a
/// cross-timeframe entry (EMA-3 over resampled 4-bar candles, latched) should
/// build, run, and print the expected activation-crop line.
///
/// The pipeline's stable_period as composed: Resample.warm_up = 4,
/// Ema-of-resample.warm_up = 4, unstable = 10 (in HTF-sample units — this is
/// the raw composition arithmetic, not base-bar-scaled). Wrapped in `!latch`,
/// `stable_period() = 14`, so `!stable`-gating puts the activation at bar 14
/// (index 13 — 13 bars before activation).
#[test]
fn latch_resample_gate_activates_at_composed_stable_period() {
    let mut csv = String::from("symbol;time;open;high;low;close;volume\n");
    for i in 0..60 {
        let day = (i % 28) + 1;
        let month = (i / 28) + 1;
        let close = 100.0 + i as f64 * 0.5;
        csv.push_str(&format!(
            "BTC;2024-{month:02}-{day:02};{c};{c};{c};{c};1000\n",
            c = close
        ));
    }
    let csv_path = std::env::temp_dir().join("fugazi_e2e_latch_resample_candles.csv");
    std::fs::write(&csv_path, csv).expect("write synthetic candles");

    let out_dir = std::env::temp_dir().join("fugazi_e2e_latch_resample");
    let _ = std::fs::remove_dir_all(&out_dir);

    let strategy = "symbol: BTC\n\
                    long:\n  \
                    enter: !gt\n    \
                    lhs: !latch\n      \
                    source: !ema\n        \
                    period: 3\n        \
                    source: !resample { every: 4, field: close }\n    \
                    rhs: !value 0\n";

    let output = Command::new(env!("CARGO_BIN_EXE_fugazi"))
        .args([
            "run",
            strategy,
            "--series",
            &format!("@{}", csv_path.to_str().unwrap()),
            "--output-dir",
            out_dir.to_str().unwrap(),
        ])
        .output()
        .expect("failed to launch the fugazi binary");
    assert!(
        output.status.success(),
        "fugazi run exited with failure:\n{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(
        stdout.contains("13 bars before strategy activation"),
        "expected `13 bars before strategy activation` in the run output; got:\n{stdout}"
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
