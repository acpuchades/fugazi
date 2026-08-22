//! End-to-end tests of the `fugazi run` backtester binary over the example
//! candles, asserting it produces non-trivial result files for both an `@file`
//! strategy and an inline one.

mod common;

use common::cli::{Cmd, assert_metrics_shape, at, scratch_file};

const FILLS_HEADER: &str = "time,symbol,side,units,price";
const TRADES_HEADER: &str =
    "entry_time,exit_time,side,units,entry_price,exit_price,pnl,return,bars_held";

/// `fugazi run <strategy> --series examples/candles.csv` into a fresh scratch dir.
fn run_backtest(out_name: &str, strategy: &str) -> common::cli::Artefacts {
    Cmd::new("run")
        .arg(strategy)
        .series(&at("examples/candles.csv"))
        .output_dir(out_name)
        .ok()
        .artefacts()
}

/// Both spellings of the positional strategy argument — `@file` and inline
/// YAML — must produce the same four artefacts with the same schema.
#[test]
fn runs_an_at_file_strategy() {
    let out = run_backtest("fugazi_e2e_file", &at("examples/strategy.yml"));

    assert!(
        out.fills.starts_with(FILLS_HEADER),
        "unexpected fills.csv header: {}",
        out.fills
    );
    assert!(
        out.fills.lines().count() >= 2,
        "expected at least one fill, got:\n{}",
        out.fills
    );
    assert!(
        out.trades.starts_with(TRADES_HEADER),
        "unexpected trades.csv header: {}",
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

#[test]
fn runs_an_inline_strategy() {
    // A bare (non-`@`) value is the strategy YAML itself.
    let out = run_backtest(
        "fugazi_e2e_inline",
        "root: BTC\nlong:\n  enter: !crosses_above { lhs: !sma { source: close, period: 2 }, rhs: !sma { source: close, period: 4 } }\n",
    );

    assert!(
        out.fills.starts_with(FILLS_HEADER),
        "unexpected fills.csv header: {}",
        out.fills
    );
    assert!(
        out.fills.lines().count() >= 2,
        "expected at least one fill, got:\n{}",
        out.fills
    );
    assert!(
        out.trades.starts_with(TRADES_HEADER),
        "unexpected trades.csv header: {}",
        out.trades
    );
    assert!(out.returns.lines().count() >= 2, "expected an equity curve");
    assert_metrics_shape(&out.metrics);
}

/// `-w/--windowed N` keeps writing `metrics.yml` (whole-run) and *also* emits
/// `metrics.csv` (one row per non-overlapping N-bar window) and `rolling.csv`
/// (one row per rolling N-bar window). Both CSVs share the same shape — same
/// columns and same `window_start,window_end,<metrics…>` layout — so R can
/// consume them interchangeably.
#[test]
fn runs_windowed_metrics() {
    let out = Cmd::new("run")
        .arg(&at("examples/strategy.yml"))
        .series(&at("examples/candles.csv"))
        .args(&["--windowed", "10"])
        .output_dir("fugazi_e2e_windowed")
        .ok();

    assert!(
        out.wrote("metrics.yml"),
        "metrics.yml should always be written (whole-run summary)"
    );

    let header = out.header("metrics.csv");
    assert!(
        header.starts_with("window_start,window_end,run.bars,"),
        "unexpected metrics.csv header: {header}"
    );
    for section in [
        "returns.total_pct",
        "risk_adjusted.sharpe",
        "drawdown.max_pct",
    ] {
        assert!(
            header.contains(section),
            "metrics.csv header missing `{section}`: {header}"
        );
    }
    // 30 bars split into 10 + 10 + 10 → 3 non-overlapping windows.
    let rows = out.rows("metrics.csv");
    assert_eq!(
        rows.len(),
        3,
        "expected one row per non-overlapping window:\n{}",
        out.read("metrics.csv")
    );
    assert!(
        rows[0].starts_with("2024-01-01,"),
        "first window should start at bar 1 of the run: {}",
        rows[0]
    );

    // rolling.csv shares every column of metrics.csv *except* the trailing
    // `selection.deflated_sharpe` — DSR isn't emitted for rolling windows because
    // their overlapping bars break the trial-variance model. See `run.rs` writer.
    let rheader = out.header("rolling.csv");
    assert_eq!(
        header,
        format!("{rheader},selection.deflated_sharpe"),
        "metrics.csv should be rolling.csv's columns plus the trailing selection.deflated_sharpe"
    );
    // 30 bars, window 10 → 30 - 10 + 1 = 21 rolling windows.
    assert_eq!(
        out.rows("rolling.csv").len(),
        21,
        "expected one row per rolling window:\n{}",
        out.read("rolling.csv")
    );
}

/// `-w/--windowed` accepts a time suffix (`1w`, `1M`, `4h`, …) — it resolves
/// against the run's trading calendar. On the example daily crypto fixture
/// with `--crypto`, `-w 1w` picks 7 bars per window, so 30 bars split into 4
/// non-overlapping ones (7+7+7+7, one short trailing chunk kept by the
/// non-overlapping reducer).
#[test]
fn runs_windowed_metrics_with_time_suffix() {
    let out = Cmd::new("run")
        .arg(&at("examples/strategy.yml"))
        .series(&at("examples/candles.csv"))
        .args(&["--crypto", "--windowed", "1w"])
        .output_dir("fugazi_e2e_windowed_time")
        .ok();

    // 30 daily bars, window = 1w = 7 bars → 4 full windows + 1 trailing
    // stub of 2 bars (the reducer keeps the tail).
    let rows = out.rows("metrics.csv");
    assert_eq!(
        rows.len(),
        5,
        "expected 4 full 7-bar windows + a 2-bar tail:\n{}",
        out.read("metrics.csv")
    );
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

/// The degenerate end of the windowed reducer: a `-w` longer than the run has
/// bars is **not** an error. It collapses to a single window spanning the whole
/// run, whose row must agree with the whole-run `metrics.yml` — otherwise the
/// windowed and whole-run reductions have drifted apart at the one input where
/// they are provably the same measurement.
#[test]
fn a_window_longer_than_the_run_collapses_to_the_whole_run() {
    let out = Cmd::new("run")
        .arg(&at("examples/strategy.yml"))
        .series(&at("examples/candles.csv"))
        .args(&["--windowed", "10000"])
        .output_dir("fugazi_e2e_window_too_long")
        .ok();

    let rows = out.rows("metrics.csv");
    assert_eq!(rows.len(), 1, "expected exactly one window:\n{rows:?}");

    // Locate `returns.total_pct` by name rather than by index — the column set
    // grows with every new metric.
    let header = out.header("metrics.csv");
    let col = header
        .split(',')
        .position(|h| h == "returns.total_pct")
        .expect("metrics.csv should carry returns.total_pct");
    let windowed: f64 = rows[0].split(',').nth(col).unwrap().parse().unwrap();

    let whole: f64 = out
        .read("metrics.yml")
        .lines()
        .find_map(|l| l.trim_start().strip_prefix("total_pct:"))
        .and_then(|s| s.trim().parse().ok())
        .expect("metrics.yml should carry returns.total_pct");

    assert_eq!(
        windowed, whole,
        "the single collapsed window must reproduce the whole-run return"
    );
}

/// End-to-end wiring for a cross-timeframe entry.
///
/// The user relies on the safe-by-default strategy-readiness gate to hold the
/// entry until the composed latch/resample/ema chain is past its stable_bars,
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
    let (_path, series) = scratch_file("fugazi_e2e_latch_resample_candles.csv", &csv);

    let strategy = r#"root: BTC
long:
  enter: !gt
    lhs: !latch { source: !resample { every: 4, inner: !ema { period: 3, source: close } } }
    rhs: !value 0
"#;

    let out = Cmd::new("run")
        .arg(strategy)
        .series(&series)
        .output_dir("fugazi_e2e_latch_resample")
        .ok();

    // The fills.csv should show at least one buy after stability.
    let fills = out.read("fills.csv");
    assert!(
        fills.lines().count() >= 2,
        "expected at least one fill line beyond the header:\n{fills}"
    );
}

/// A `root:` that is a plain selector must install the **blessed** root —
/// `Pick::rooted`, which falls back to the sole-atom unpack — not the strict
/// `Pick::matching`.
///
/// `!resample` drives its `inner:` expression over *untagged* synthesized
/// bars, so a strict root reads `None` there on every bar and the run reports a
/// plausible zero-fill backtest rather than failing. Pinned end-to-end through
/// the binary because the readiness numbers are identical either way — only the
/// fills tell the two apart.
#[test]
fn a_resampled_inner_expression_still_reads_the_blessed_root() {
    let mut csv = String::from("symbol;time;open;high;low;close;volume\n");
    for i in 0..40 {
        let close = 100.0 + i as f64;
        csv.push_str(&format!(
            "BTC;2024-{month:02}-{day:02};{c};{c};{c};{c};1000\n",
            month = i / 28 + 1,
            day = i % 28 + 1,
            c = close
        ));
    }
    let (_path, series) = scratch_file("fugazi_e2e_resample_blessed_root.csv", &csv);

    let out = Cmd::new("run")
        .arg(
            "root: BTC\nlong:\n  enter: !gt\n    lhs: !resample { every: 4, inner: !ema { period: 3, source: close } }\n    rhs: !value 0\n",
        )
        .series(&series)
        .output_dir("fugazi_e2e_resample_blessed_root")
        .ok();

    let fills = out.read("fills.csv");
    assert!(
        fills.lines().count() >= 2,
        "a bare `close` inside `!resample`'s inner expression read nothing — the root lost \
         its sole-atom fallback:\n{fills}"
    );
}
