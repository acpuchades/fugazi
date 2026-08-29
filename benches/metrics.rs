//! The post-run reduction: `RunReport` → `Metrics`.
//!
//! Once per run this is negligible. `optimize` pays it once per grid row (and
//! once per walk-forward fold per row), so a sweep of a few thousand rows over
//! a long series pays it a few thousand times — which is where the redundant
//! passes inside it (`drawdown_segments` recomputed by `calmar` and
//! `recovery_factor`; four independent `sorted_asc` sorts across the quantile
//! metrics) start to show.

use std::hint::black_box;

use criterion::{BenchmarkId, Criterion, criterion_group, criterion_main};

use fugazi::backtest::{Fill, RunReport};
use fugazi::prelude::*;
use fugazi::wallet::{Order, OrderId, OrderKind};

mod common;
use common::synth_candles;

const SIZES: [usize; 3] = [10_000, 100_000, 200_000];

/// A synthetic report: an equity curve from the price walk, plus an alternating
/// fill every 50 bars so the trade-level metrics have real input.
fn report(bars: usize) -> RunReport<Symbol> {
    let candles = synth_candles(bars);
    let equity_curve: Vec<Real> = candles.iter().map(|c| c.close * 100.0).collect();
    let fills: Vec<Fill<Symbol>> = (0..bars)
        .step_by(50)
        .enumerate()
        .map(|(i, bar)| Fill {
            bar,
            order: Order {
                id: OrderId(i as u64),
                symbol: fugazi::types::symbol("X"),
                side: if i % 2 == 0 { Side::Buy } else { Side::Sell },
                units: 1.0,
                price: candles[bar].close,
                kind: OrderKind::Market,
                commission: 0.0,
                requested_units: 1.0,
            },
        })
        .collect();
    RunReport {
        equity_curve,
        fills,
        rejections: Vec::new(),
        initial_equity: candles[0].close * 100.0,
        ruin_bar: None,
        carry_coverage: None,
        attribution: None,
    }
}

fn bench_from_report(c: &mut Criterion) {
    let mut g = c.benchmark_group("metrics/from_report");
    for n in SIZES {
        let rep = report(n);
        g.bench_with_input(BenchmarkId::from_parameter(n), &n, |b, _| {
            b.iter(|| {
                black_box(fugazi::spec::metrics::from_report(&rep, 365.0, 0.045, None));
            });
        });
    }
    g.finish();
}

/// The individual intermediates, so a change to one is attributable.
fn bench_intermediates(c: &mut Criterion) {
    let rep = report(200_000);
    let equity = rep.equity_curve.as_slice();
    let returns = fugazi::metrics::per_bar_returns(equity, rep.initial_equity);

    let mut g = c.benchmark_group("metrics/parts");
    g.bench_function("per_bar_returns", |b| {
        b.iter(|| black_box(fugazi::metrics::per_bar_returns(equity, rep.initial_equity)));
    });
    g.bench_function("drawdown_segments", |b| {
        b.iter(|| black_box(fugazi::metrics::drawdown_segments(equity)));
    });
    g.bench_function("reconstruct_trades", |b| {
        b.iter(|| black_box(fugazi::metrics::reconstruct_trades(&rep.fills)));
    });
    // The four metrics that each sort the whole return series independently.
    g.bench_function("median_return", |b| {
        b.iter(|| black_box(fugazi::metrics::median_return(&returns)));
    });
    g.bench_function("value_at_risk", |b| {
        b.iter(|| black_box(fugazi::metrics::value_at_risk(&returns, 0.95)));
    });
    g.finish();
}

/// Carving one window out of a report: the shipped binary search against the
/// linear filter it replaced.
///
/// A/B'd here rather than across a git revision, so both sides are one build
/// and the comparison holds still. `linear_filter` is the pre-change body
/// verbatim — `bars.contains()` over the whole blotter — and the gap is the
/// blotter scan, which `rolling_from_report` pays once per bar.
/// `report_slice`'s body from before the binary search, verbatim: `contains()`
/// over the whole blotter. The A/B reference for both groups below.
fn linear_filter<Sym: Clone>(
    report: &RunReport<Sym>,
    bars: std::ops::Range<usize>,
) -> RunReport<Sym> {
    let fills: Vec<Fill<Sym>> = report
        .fills
        .iter()
        .filter(|f| bars.contains(&f.bar))
        .map(|f| Fill {
            bar: f.bar - bars.start,
            order: f.order.clone(),
        })
        .collect();
    let rejections: Vec<fugazi::backtest::Rejected<Sym>> = report
        .rejections
        .iter()
        .filter(|r| bars.contains(&r.bar))
        .map(|r| fugazi::backtest::Rejected {
            bar: r.bar - bars.start,
            rejection: r.rejection.clone(),
        })
        .collect();
    RunReport {
        equity_curve: report.equity_curve[bars.clone()].to_vec(),
        fills,
        rejections,
        initial_equity: if bars.start == 0 {
            report.initial_equity
        } else {
            report.equity_curve[bars.start - 1]
        },
        ruin_bar: None,
        carry_coverage: None,
        attribution: None,
    }
}

fn bench_report_slice(c: &mut Criterion) {
    let rep = report(200_000);
    let window = 252;
    let mid = 100_000;
    let mut g = c.benchmark_group("metrics/report_slice");
    g.bench_function("linear_filter", |b| {
        b.iter(|| black_box(linear_filter(&rep, mid..mid + window)));
    });
    g.bench_function("binary_search", |b| {
        b.iter(|| black_box(fugazi::spec::metrics::report_slice(&rep, mid..mid + window)));
    });
    g.finish();
}

/// The rolling reduction, which takes a [`report_slice`] **per bar** — so the
/// per-slice cost of carving fills out of the blotter is paid `bars - window + 1`
/// times rather than once.
///
/// Swept over window length at a fixed bar count: the slice cost scales with
/// the *blotter*, not the window, so it is the short windows (most windows, each
/// cheap to reduce) where a linear blotter scan dominates.
fn bench_rolling(c: &mut Criterion) {
    let rep = report(50_000);
    let mut g = c.benchmark_group("metrics/rolling_from_report");
    g.sample_size(10);
    for window in [63usize, 252, 1_000] {
        g.bench_with_input(BenchmarkId::from_parameter(window), &window, |b, &w| {
            b.iter(|| {
                black_box(fugazi::spec::metrics::rolling_from_report(
                    &rep, w, 365.0, 0.045, None,
                ))
            });
        });
    }
    g.finish();
}

/// The same A/B as `bench_report_slice`, at the scale that motivated it: one
/// slice per bar.
///
/// Serial on both sides — `rolling_from_report` farms its windows out to
/// whatever rayon pool the caller installed, and a bench crate cannot reach
/// rayon to reproduce that. Holding both sides serial keeps the comparison
/// honest about the *mechanism* (what fraction of a rolling sweep is blotter
/// scanning) while understating the wall-clock the parallel version saves by
/// roughly the pool width.
fn bench_rolling_slice_strategy(c: &mut Criterion) {
    fn sweep(
        rep: &RunReport<Symbol>,
        window: usize,
        slice: impl Fn(&RunReport<Symbol>, std::ops::Range<usize>) -> RunReport<Symbol>,
    ) -> usize {
        let bars = rep.equity_curve.len();
        (0..=(bars - window))
            .map(|start| {
                let sub = slice(rep, start..start + window);
                fugazi::spec::metrics::from_report(&sub, 365.0, 0.045, None)
                    .trades
                    .total_fills
            })
            .sum()
    }

    let rep = report(50_000);
    let window = 252;
    let mut g = c.benchmark_group("metrics/rolling_slice_strategy");
    g.sample_size(10);
    g.bench_function("linear_filter", |b| {
        b.iter(|| black_box(sweep(&rep, window, linear_filter)));
    });
    g.bench_function("binary_search", |b| {
        b.iter(|| black_box(sweep(&rep, window, fugazi::spec::metrics::report_slice)));
    });
    g.finish();
}

criterion_group!(
    benches,
    bench_from_report,
    bench_intermediates,
    bench_report_slice,
    bench_rolling,
    bench_rolling_slice_strategy
);
criterion_main!(benches);
