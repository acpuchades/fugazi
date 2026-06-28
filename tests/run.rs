//! End-to-end test of the `fugazi run` backtester binary over the example
//! strategy + candles, asserting it produces non-trivial result files.

use std::process::Command;

#[test]
fn runs_the_example_backtest() {
    let manifest = env!("CARGO_MANIFEST_DIR");
    let out = std::env::temp_dir().join("fugazi_e2e_out");
    let _ = std::fs::remove_dir_all(&out);

    let status = Command::new(env!("CARGO_BIN_EXE_fugazi"))
        .args([
            "run",
            "--strategy",
            &format!("{manifest}/examples/strategy.yml"),
            "--series",
            &format!("symbol=BTC,@{manifest}/examples/candles.csv"),
            "--output-dir",
            out.to_str().unwrap(),
        ])
        .status()
        .expect("failed to launch the fugazi binary");
    assert!(status.success(), "fugazi run exited with failure");

    let trades = std::fs::read_to_string(out.join("trades.csv")).expect("trades.csv");
    let returns = std::fs::read_to_string(out.join("returns.csv")).expect("returns.csv");

    assert!(
        trades.starts_with("time;symbol;side;quantity;price"),
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
