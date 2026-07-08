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
        out.trades.starts_with("time,symbol,side,units,price"),
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

/// `-w/--windowed N` keeps writing `metrics.yml` (whole-run) and *also* emits
/// `metrics.csv` (one row per non-overlapping N-bar window) and `rolling.csv`
/// (one row per rolling N-bar window). Both CSVs share the same shape — same
/// columns and same `window_start,window_end,<metrics…>` layout — so R can
/// consume them interchangeably.
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
        out.join("metrics.yml").exists(),
        "metrics.yml should always be written (whole-run summary)"
    );

    let metrics = std::fs::read_to_string(out.join("metrics.csv")).expect("metrics.csv");
    let mut lines = metrics.lines();
    let header = lines.next().expect("metrics.csv header");
    assert!(
        header.starts_with("window_start,window_end,run.bars,"),
        "unexpected metrics.csv header: {header}"
    );
    for section in ["returns.total_pct", "risk_adjusted.sharpe", "drawdown.max_pct"] {
        assert!(
            header.contains(section),
            "metrics.csv header missing `{section}`: {header}"
        );
    }
    // 30 bars split into 10 + 10 + 10 → 3 non-overlapping windows.
    let rows: Vec<&str> = lines.collect();
    assert_eq!(rows.len(), 3, "expected one row per non-overlapping window:\n{metrics}");
    assert!(
        rows[0].starts_with("2024-01-01,"),
        "first window should start at bar 1 of the run: {}",
        rows[0]
    );

    let rolling = std::fs::read_to_string(out.join("rolling.csv")).expect("rolling.csv");
    let mut rlines = rolling.lines();
    let rheader = rlines.next().expect("rolling.csv header");
    // rolling.csv shares every column of metrics.csv *except* the trailing
    // `selection.deflated_sharpe` — DSR isn't emitted for rolling windows because their
    // overlapping bars break the trial-variance model. See `run.rs` writer.
    assert_eq!(
        header,
        format!("{rheader},selection.deflated_sharpe"),
        "metrics.csv should be rolling.csv's columns plus the trailing selection.deflated_sharpe"
    );
    // 30 bars, window 10 → 30 - 10 + 1 = 21 rolling windows.
    let rrows: Vec<&str> = rlines.collect();
    assert_eq!(rrows.len(), 21, "expected one row per rolling window:\n{rolling}");
}

/// `-w/--windowed` accepts a time suffix (`1w`, `1M`, `4h`, …) — it resolves
/// against the run's trading calendar. On the example daily crypto fixture
/// with `--crypto`, `-w 1w` picks 7 bars per window, so 30 bars split into 4
/// non-overlapping ones (7+7+7+7, one short trailing chunk kept by the
/// non-overlapping reducer).
#[test]
fn runs_windowed_metrics_with_time_suffix() {
    let manifest = env!("CARGO_MANIFEST_DIR");
    let out = std::env::temp_dir().join("fugazi_e2e_windowed_time");
    let _ = std::fs::remove_dir_all(&out);

    let status = Command::new(env!("CARGO_BIN_EXE_fugazi"))
        .args([
            "run",
            &format!("@{manifest}/examples/strategy.yml"),
            "--series",
            &format!("@{manifest}/examples/candles.csv"),
            "--output-dir",
            out.to_str().unwrap(),
            "--crypto",
            "--windowed",
            "1w",
        ])
        .status()
        .expect("failed to launch the fugazi binary");
    assert!(status.success(), "fugazi run -w 1w exited with failure");

    let metrics = std::fs::read_to_string(out.join("metrics.csv")).expect("metrics.csv");
    // 30 daily bars, window = 1w = 7 bars → 4 full windows + 1 trailing
    // stub of 2 bars (the reducer keeps the tail).
    let rows: Vec<&str> = metrics.lines().skip(1).collect();
    assert_eq!(rows.len(), 5, "expected 4 full 7-bar windows + a 2-bar tail:\n{metrics}");
    assert!(
        rows[0].starts_with("2024-01-01,2024-01-07,"),
        "first window should span Jan 1-7: {}",
        rows[0]
    );
    assert!(
        rows[4].starts_with("2024-01-29,2024-01-30,"),
        "last (stub) window should span Jan 29-30: {}",
        rows[4]
    );
}

/// End-to-end wiring for a cross-timeframe entry.
///
/// The user relies on the safe-by-default strategy-readiness gate to hold the
/// entry until the composed latch/resample/ema chain is past its stable_period,
/// so the entry signal is just the plain comparison:
///
/// ```yaml
/// enter: !gt { lhs: !latch { source: !resample { every, inner: !ema {…} } }, rhs: !value 0 }
/// ```
///
/// Verifies that this runs end-to-end and the entry actually fires once
/// readiness elapses.
#[test]
fn latch_resample_entry_gated_by_readiness_runs_end_to_end() {
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

    let strategy = r#"symbol: BTC
long:
  enter: !gt
    lhs: !latch { source: !resample { every: 4, inner: !ema { period: 3, source: close } } }
    rhs: !value 0
"#;

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
    // The trades.csv should show at least one buy after stability.
    let trades = std::fs::read_to_string(out_dir.join("trades.csv")).expect("trades.csv");
    assert!(
        trades.lines().count() >= 2,
        "expected at least one trade line beyond the header:\n{trades}"
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
        out.trades.starts_with("time,symbol,side,units,price"),
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
