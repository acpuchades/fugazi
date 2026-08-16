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
fn report(bars: usize) -> RunReport<String> {
    let candles = synth_candles(bars);
    let equity_curve: Vec<Real> = candles.iter().map(|c| c.close * 100.0).collect();
    let fills: Vec<Fill<String>> = (0..bars)
        .step_by(50)
        .enumerate()
        .map(|(i, bar)| Fill {
            bar,
            order: Order {
                id: OrderId(i as u64),
                symbol: "X".to_string(),
                side: if i % 2 == 0 { Side::Buy } else { Side::Sell },
                units: 1.0,
                price: candles[bar].close,
                kind: OrderKind::Market,
                commission: 0.0,
            },
        })
        .collect();
    RunReport {
        equity_curve,
        fills,
        rejections: Vec::new(),
        initial_equity: candles[0].close * 100.0,
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

criterion_group!(benches, bench_from_report, bench_intermediates);
criterion_main!(benches);
