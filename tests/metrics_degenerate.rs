// Reads `fugazi::spec::metrics`, so it needs the `spec` feature (which the
// default `cli` enables).
#![cfg(feature = "spec")]

//! Every metric over every degenerate run, asserting `None` or a finite number.
//!
//! `tests/metrics_validation.rs` pins the *values* against empyrical over one
//! well-behaved 252-bar series. Nothing walked the same surface over the runs
//! that actually break arithmetic: a run with no bars at all, one bar, a flat
//! curve with no dispersion, an account ruined on the first bar, a curve that
//! never moves off zero.
//!
//! The assertion is the same bargain `tests/degenerate_inputs.rs` strikes on the
//! indicator side — weak on value, strict on kind. What a Sharpe over a
//! zero-variance curve *should* be is a convention; that it must be `None`
//! rather than a `NaN` is not. `metrics.yml`, `metrics.csv` and
//! `optimize --best-by` all read these, and a `NaN` there sorts unpredictably,
//! prints as a number, and survives into a ranking.
//!
//! The field list comes from `metrics::flatten`, which the compiler forces you
//! to extend when you add a metric — so a new metric is covered here the day it
//! is added, exactly as `tests/metrics_coverage.rs` arranges for the reference
//! values.

use fugazi::backtest::{Fill, RunReport};
use fugazi::market::Real;
use fugazi::spec::metrics;
use fugazi::wallet::{Order, OrderId, OrderKind, Side};

/// A report with a hand-built equity curve and no fills.
fn report(initial: Real, curve: &[Real], ruin_bar: Option<usize>) -> RunReport<String> {
    RunReport {
        equity_curve: curve.to_vec(),
        fills: Vec::new(),
        rejections: Vec::new(),
        initial_equity: initial,
        ruin_bar,
        carry_coverage: None,
    }
}

fn cases() -> Vec<(&'static str, RunReport<String>)> {
    vec![
        ("no bars at all", report(10_000.0, &[], None)),
        ("one bar", report(10_000.0, &[10_000.0], None)),
        (
            "two bars, no movement",
            report(10_000.0, &[10_000.0; 2], None),
        ),
        (
            "flat curve, no dispersion",
            report(10_000.0, &[10_000.0; 60], None),
        ),
        (
            "ruined on the first bar",
            report(10_000.0, &[0.0; 60], Some(0)),
        ),
        (
            "ruined mid-run",
            report(
                10_000.0,
                &(0..60)
                    .map(|i| {
                        if i < 30 {
                            10_000.0 - i as Real * 300.0
                        } else {
                            0.0
                        }
                    })
                    .collect::<Vec<_>>(),
                Some(30),
            ),
        ),
        (
            "monotonically rising, never a losing bar",
            report(
                100.0,
                &(0..60).map(|i| 100.0 + i as Real).collect::<Vec<_>>(),
                None,
            ),
        ),
        (
            "monotonically falling, never a winning bar",
            report(
                100.0,
                &(0..60).map(|i| 100.0 - i as Real).collect::<Vec<_>>(),
                None,
            ),
        ),
        // A zero starting stake is bad input, not a crash: every ratio to it is
        // undefined and must say so.
        ("zero initial equity", report(0.0, &[0.0; 30], None)),
        (
            "a single non-zero bar in an otherwise flat curve",
            report(
                10_000.0,
                &(0..60)
                    .map(|i| if i == 30 { 10_100.0 } else { 10_000.0 })
                    .collect::<Vec<_>>(),
                None,
            ),
        ),
    ]
}

#[test]
fn no_metric_reports_a_non_finite_value_on_a_degenerate_run() {
    for (name, r) in cases() {
        for &bars_per_year in &[252.0, 1.0] {
            for &rf in &[0.0, 0.05] {
                let m = metrics::from_report(&r, bars_per_year, rf, Some(86_400.0));
                for (key, value) in metrics::flatten(&m) {
                    if let Some(v) = value {
                        assert!(
                            v.is_finite(),
                            "`{key}` is {v} on a {name} run \
                             (bars_per_year {bars_per_year}, rf {rf}) — an undefined \
                             metric must be absent, not a NaN or an infinity"
                        );
                    }
                }
            }
        }
    }
}

/// A degenerate `bars_per_year` is reachable from `-f/--frequency` plus an
/// asset class, and divides the annualization of half the surface.
#[test]
fn a_degenerate_annualization_factor_does_not_produce_a_non_finite_metric() {
    let r = report(
        10_000.0,
        &(0..60)
            .map(|i| 10_000.0 + (i as Real * 0.7).sin() * 200.0)
            .collect::<Vec<_>>(),
        None,
    );
    for bars_per_year in [0.0, 1.0, 1e-9, 1e9] {
        let m = metrics::from_report(&r, bars_per_year, 0.0, Some(86_400.0));
        for (key, value) in metrics::flatten(&m) {
            if let Some(v) = value {
                assert!(
                    v.is_finite(),
                    "`{key}` is {v} at bars_per_year {bars_per_year}"
                );
            }
        }
    }
}

/// Windowed and rolling reductions slice the same curve, and a window longer
/// than the run — or exactly one bar wide — is what a `-w` on a short series
/// produces.
#[test]
fn a_degenerate_window_length_slices_without_panicking() {
    let r = report(
        10_000.0,
        &(0..40)
            .map(|i| 10_000.0 + (i as Real * 0.7).sin() * 200.0)
            .collect::<Vec<_>>(),
        None,
    );
    for window in [1usize, 2, 39, 40, 41, 1_000] {
        let windows = metrics::windowed_from_report(&r, window, 252.0, 0.0, Some(86_400.0));
        for w in &windows {
            for (key, value) in metrics::flatten(&w.metrics) {
                if let Some(v) = value {
                    assert!(v.is_finite(), "`{key}` is {v} at window {window}");
                }
            }
        }
        let rolling = metrics::rolling_from_report(&r, window, 252.0, 0.0, Some(86_400.0));
        for w in &rolling {
            for (key, value) in metrics::flatten(&w.metrics) {
                if let Some(v) = value {
                    assert!(v.is_finite(), "`{key}` is {v} at rolling window {window}");
                }
            }
        }
    }
}

/// One market fill.
fn fill(bar: usize, side: Side, units: Real, price: Real) -> Fill<String> {
    Fill {
        bar,
        order: Order {
            symbol: "X".to_string(),
            side,
            units,
            price,
            kind: OrderKind::Market,
            id: OrderId(bar as u64),
            commission: 0.0,
            requested_units: units,
        },
    }
}

/// A round trip: in at `entry`, out at `exit`.
fn round_trip(bar: usize, entry: Real, exit: Real) -> Vec<Fill<String>> {
    vec![
        fill(bar, Side::Buy, 1.0, entry),
        fill(bar + 1, Side::Sell, 1.0, exit),
    ]
}

/// The trade-level half of the surface: profit factor divides gross win by
/// gross loss, the win rate divides by the trade count, and the streak /
/// duration stats index into a list that can be empty. Each of those has a
/// denominator a real run can zero.
#[test]
fn no_trade_metric_reports_a_non_finite_value_on_a_degenerate_blotter() {
    let curve: Vec<Real> = (0..40).map(|i| 10_000.0 + i as Real).collect();

    let blotters: Vec<(&str, Vec<Fill<String>>)> = vec![
        ("no trades at all", Vec::new()),
        (
            "one open position, never closed",
            vec![fill(0, Side::Buy, 1.0, 100.0)],
        ),
        ("one winning trade, no losses", round_trip(0, 100.0, 110.0)),
        ("one losing trade, no wins", round_trip(0, 110.0, 100.0)),
        (
            "every trade a scratch — zero PnL both ways",
            [round_trip(0, 100.0, 100.0), round_trip(4, 100.0, 100.0)].concat(),
        ),
        (
            "entry and exit on the same bar",
            vec![
                fill(3, Side::Buy, 1.0, 100.0),
                fill(3, Side::Sell, 1.0, 105.0),
            ],
        ),
        (
            "a zero-unit fill",
            vec![
                fill(0, Side::Buy, 0.0, 100.0),
                fill(1, Side::Sell, 0.0, 110.0),
            ],
        ),
        (
            "all winners, so gross loss is zero",
            [
                round_trip(0, 100.0, 110.0),
                round_trip(4, 100.0, 120.0),
                round_trip(8, 100.0, 130.0),
            ]
            .concat(),
        ),
        (
            "all losers, so gross win is zero",
            [
                round_trip(0, 110.0, 100.0),
                round_trip(4, 120.0, 100.0),
                round_trip(8, 130.0, 100.0),
            ]
            .concat(),
        ),
    ];

    for (name, fills) in blotters {
        let r = RunReport {
            equity_curve: curve.clone(),
            fills,
            rejections: Vec::new(),
            initial_equity: 10_000.0,
            ruin_bar: None,
            carry_coverage: None,
        };
        let m = metrics::from_report(&r, 252.0, 0.0, Some(86_400.0));
        for (key, value) in metrics::flatten(&m) {
            if let Some(v) = value {
                assert!(
                    v.is_finite(),
                    "`{key}` is {v} on a `{name}` blotter — an undefined metric \
                     must be absent, not a NaN or an infinity"
                );
            }
        }
    }
}
