//! End-to-end tests of the `fugazi run` backtester binary over the example
//! candles, asserting it produces non-trivial result files for both an `@file`
//! strategy and an inline one.

use std::process::Command;

/// Run the binary with the given `--strategy` value into a fresh `out_name`
/// scratch dir, asserting success, and return `(trades.csv, returns.csv)`.
fn run_backtest(out_name: &str, strategy: &str) -> (String, String) {
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

    let trades = std::fs::read_to_string(out.join("trades.csv")).expect("trades.csv");
    let returns = std::fs::read_to_string(out.join("returns.csv")).expect("returns.csv");
    (trades, returns)
}

#[test]
fn runs_an_at_file_strategy() {
    let manifest = env!("CARGO_MANIFEST_DIR");
    let (trades, returns) = run_backtest(
        "fugazi_e2e_file",
        &format!("@{manifest}/examples/strategy.yml"),
    );

    assert!(
        trades.starts_with("time;symbol;side;units;price"),
        "unexpected trades.csv header: {trades}"
    );
    assert!(
        trades.lines().count() >= 2,
        "expected at least one trade, got:\n{trades}"
    );
    // Header + one row per candle (30 bars in the example).
    assert!(
        returns.lines().count() >= 2,
        "expected an equity curve, got:\n{returns}"
    );
}

#[test]
fn runs_an_inline_strategy() {
    // A bare (non-`@`) value is the strategy YAML itself.
    let (trades, returns) = run_backtest(
        "fugazi_e2e_inline",
        "symbol: BTC\nlong:\n  enter: !crosses_above { lhs: !sma { source: close, period: 2 }, rhs: !sma { source: close, period: 4 } }\n",
    );

    assert!(
        trades.starts_with("time;symbol;side;units;price"),
        "unexpected trades.csv header: {trades}"
    );
    assert!(
        trades.lines().count() >= 2,
        "expected at least one trade, got:\n{trades}"
    );
    assert!(returns.lines().count() >= 2, "expected an equity curve");
}
