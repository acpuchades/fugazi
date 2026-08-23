// The whole test consumes `fugazi::spec::metrics`, so it needs the `spec`
// feature (which the default `cli` enables).
#![cfg(feature = "spec")]

//! Cross-validation of fugazi's **trade-level** metrics against
//! [backtesting.py](https://kernc.github.io/backtesting.py/).
//!
//! `metrics_validation.rs` hands `from_report` a synthetic report with
//! `fills: Vec::new()`, because empyrical takes a returns series and has no
//! notion of a trade. Everything in `trades.*` — win rate, profit factor,
//! payoff, Kelly, exposure, streaks, durations — was therefore checked only
//! against fugazi's own unit tests. This suite is the missing half.
//!
//! Both sides consume `tests/data/trade_metrics_fills.csv`: the fill blotter
//! **and** the equity curve that backtesting.py itself produced. Feeding
//! fugazi the reference's own fills is what makes the comparison about the
//! statistics rather than about which trades happened.
//!
//! # Scope
//!
//! The schedule is strictly flat → position → flat. fugazi reconstructs trades
//! at volume-weighted average cost and backtesting.py splits per entry order,
//! FIFO; those two conventions coincide exactly on non-overlapping round trips
//! and genuinely differ on adds, partial closes and reversals. Comparing the
//! latter would measure a bookkeeping difference rather than an error, so they
//! stay with the unit tests in `src/metrics.rs`. `tools/gen_trade_metrics_fixtures.py`
//! refuses to generate from an overlapping schedule.
//!
//! Commission is zero throughout, and must stay so: backtesting.py reports
//! trade PnL net of commission while fugazi computes it from fill prices and
//! books commission separately. The cost pipeline is covered instead by
//! `tests/wallet_validation.rs`, against vectorbt.
//!
//! Constants (`INITIAL_CASH`, `BARS_PER_YEAR`, `RISK_FREE_RATE`) must match
//! `tools/gen_trade_metrics_fixtures.py`.

mod common;

use std::collections::HashMap;

use fugazi::backtest::{Fill, RunReport};
use fugazi::prelude::*;
use fugazi::spec::metrics;
use fugazi::wallet::{Order, OrderId, OrderKind};

use common::fixtures::{Csv, skip};

/// Must match `tools/gen_trade_metrics_fixtures.py`.
const INITIAL_CASH: Real = 100_000.0;
const BARS_PER_YEAR: Real = 252.0;
const RISK_FREE_RATE: Real = 0.0;

const SYMBOL: &str = "TEST";

/// Both sides compute the same aggregates over the same trade table, so the
/// only slack is float rounding across two implementations.
const TOL: Real = 1e-9;

/// Rebuild the report backtesting.py produced: its equity curve, and its fills
/// as fugazi `Order`s.
fn load_report() -> RunReport<&'static str> {
    let csv = Csv::require("trade_metrics_fills.csv");
    let equity = csv.floats("equity");
    let units = csv.optional_floats("fill_units");
    let prices = csv.optional_floats("fill_price");

    let mut fills = Vec::new();
    for bar in 0..csv.len() {
        let (Some(u), Some(p)) = (units[bar], prices[bar]) else {
            continue;
        };
        let order = Order::from_delta(SYMBOL, u, p, OrderKind::Market, OrderId(fills.len() as u64))
            .expect("the generator never writes a zero-unit fill");
        fills.push(Fill { bar, order });
    }

    RunReport {
        equity_curve: equity,
        fills,
        rejections: Vec::new(),
        initial_equity: INITIAL_CASH,
        ruin_bar: None,
        carry_coverage: None,
    }
}

/// Pull a scalar out of [`metrics::Metrics`] by its dotted path. One arm per
/// reference field the generator writes; an unknown key fails loudly rather
/// than being skipped, so a fixture that drifts ahead of this file is obvious.
fn field(m: &metrics::Metrics, key: &str) -> Real {
    let defined = |o: Option<Real>| o.expect("reference expects a defined value");
    match key {
        "run.final_equity" => m.run.final_equity,
        // TradeSection
        "trades.total" => m.trades.total as Real,
        "trades.wins" => m.trades.wins as Real,
        "trades.losses" => m.trades.losses as Real,
        "trades.long_trades" => m.trades.long_trades as Real,
        "trades.short_trades" => m.trades.short_trades as Real,
        "trades.total_fills" => m.trades.total_fills as Real,
        "trades.exposure_pct" => m.trades.exposure_pct,
        "trades.win_rate_pct" => defined(m.trades.win_rate_pct),
        "trades.profit_factor" => defined(m.trades.profit_factor),
        "trades.payoff_ratio" => defined(m.trades.payoff_ratio),
        "trades.expectancy" => defined(m.trades.expectancy),
        "trades.kelly_fraction" => defined(m.trades.kelly_fraction),
        "trades.average_win" => defined(m.trades.average_win),
        "trades.average_loss" => defined(m.trades.average_loss),
        "trades.largest_win" => defined(m.trades.largest_win),
        "trades.largest_loss" => defined(m.trades.largest_loss),
        "trades.average_return_pct" => defined(m.trades.average_return_pct),
        "trades.average_bars" => defined(m.trades.average_bars),
        "trades.min_bars" => m.trades.min_bars.expect("reference expects a value") as Real,
        "trades.max_bars" => m.trades.max_bars.expect("reference expects a value") as Real,
        // DrawdownSection — the duration/count fields empyrical cannot reach
        "drawdown.max_duration_bars" => m.drawdown.max_duration_bars as Real,
        "drawdown.count" => m.drawdown.count as Real,
        "drawdown.avg_duration_bars" => defined(m.drawdown.avg_duration_bars),
        "drawdown.time_in_drawdown_pct" => m.drawdown.time_in_drawdown_pct,
        "drawdown.avg" => defined(m.drawdown.avg),
        "drawdown.avg_pct" => defined(m.drawdown.avg_pct),
        other => panic!(
            "unknown reference field `{other}` — add an arm here, or drop it \
             from tools/gen_trade_metrics_fixtures.py"
        ),
    }
}

fn read_expected(csv: &Csv) -> HashMap<String, Real> {
    csv.strings("metric")
        .into_iter()
        .zip(csv.floats("expected"))
        .collect()
}

#[test]
fn trade_metrics_match_backtesting_py() {
    let csv = match Csv::load("trade_metrics_expected.csv") {
        Some(csv) => csv,
        None => {
            skip(
                "trade_metrics_validation",
                "tests/data/trade_metrics_expected.csv is not present",
                "  pixi run gen-trades\n  cargo test --test trade_metrics_validation",
            );
            return;
        }
    };
    let expected = read_expected(&csv);

    // A present-but-empty fixture would make the loop below a no-op and this
    // suite pass vacuously. The generator writes 25 values; require the bulk.
    assert!(
        expected.len() >= 22,
        "expected ~25 reference values, got {} — regenerate trade_metrics_expected.csv",
        expected.len()
    );

    let report = load_report();
    assert!(
        !report.fills.is_empty(),
        "trade_metrics_fills.csv carried no fills — the whole suite would be vacuous"
    );
    let m = metrics::from_report(&report, BARS_PER_YEAR, RISK_FREE_RATE, None);

    let mut mismatches: Vec<String> = Vec::new();
    for (key, &exp) in &expected {
        let got = field(&m, key);
        let tol = TOL.max(exp.abs() * 1e-9);
        if (got - exp).abs() > tol {
            mismatches.push(format!(
                "{key}: got {got}, expected {exp}, diff {} (tol {tol})",
                (got - exp).abs()
            ));
        }
    }

    mismatches.sort();
    assert!(
        mismatches.is_empty(),
        "backtesting.py-reference divergence:\n  {}",
        mismatches.join("\n  ")
    );
}

/// fugazi reconstructs the reference's own trades back out of the reference's
/// own fills. If that round trip ever stops producing ten non-overlapping round
/// trips, every aggregate above is being computed over a different trade table
/// than backtesting.py used and the comparison means nothing.
#[test]
fn fills_reconstruct_into_the_reference_trades() {
    let report = load_report();
    let trades = fugazi::metrics::reconstruct_trades(&report.fills);

    assert_eq!(
        trades.len(),
        report.fills.len() / 2,
        "each round trip must be exactly two fills — the schedule is flat → \
         position → flat by construction"
    );
    assert!(
        trades.iter().any(|t| matches!(t.side, Side::Buy))
            && trades.iter().any(|t| matches!(t.side, Side::Sell)),
        "fixture must exercise both long and short round trips"
    );
    assert!(
        trades.iter().any(|t| t.pnl > 0.0) && trades.iter().any(|t| t.pnl < 0.0),
        "fixture must exercise both winners and losers, or the win-rate-derived \
         metrics are degenerate"
    );
}
