//! Per-`update` cost of the individual indicator cores.
//!
//! These are the leaf-level numbers everything else is built from: if a
//! composed chain costs more than the sum of its parts, the difference is
//! composition overhead, and this is the baseline that says so.
//!
//! Fed a raw `Real` / `Candle` stream rather than a `Snapshot`, deliberately —
//! this isolates the arithmetic from the projection layer (`Pick` → `Field`),
//! which `tree.rs` and `driver.rs` measure instead.

use std::hint::black_box;

use criterion::{Criterion, Throughput, criterion_group, criterion_main};

use fugazi::indicators::{
    Atr, Bollinger, CurrentBar, Ema, Identity, Macd, Percentile, Rsi, Sma, StdDev,
};
use fugazi::prelude::*;

mod common;
use common::synth_candles;

const N: usize = 50_000;

macro_rules! bench_real_source {
    ($group:expr, $name:literal, $prices:expr, $build:expr) => {{
        $group.bench_function($name, |b| {
            b.iter(|| {
                let mut ind = $build;
                for &p in $prices {
                    black_box(ind.update(p));
                }
            });
        });
    }};
}

fn bench_scalar(c: &mut Criterion) {
    let prices: Vec<Real> = synth_candles(N).iter().map(|c| c.close).collect();
    let prices = &prices[..];

    let mut g = c.benchmark_group("indicators/scalar");
    g.throughput(Throughput::Elements(N as u64));

    bench_real_source!(g, "sma_20", prices, Sma::new(Identity::new(), 20));
    bench_real_source!(g, "ema_20", prices, Ema::new(Identity::new(), 20));
    bench_real_source!(g, "rsi_14", prices, Rsi::new(Identity::new(), 14));
    bench_real_source!(g, "stddev_20", prices, StdDev::new(Identity::new(), 20));
    bench_real_source!(
        g,
        "bollinger_20",
        prices,
        Bollinger::new(Identity::new(), 20, 2.0)
    );
    bench_real_source!(
        g,
        "macd_12_26_9",
        prices,
        Macd::new(Identity::new(), 12, 26, 9)
    );
    // The O(period) window scanners — `WindowStats` dispersion reads and the
    // sorted-window quantile. Deliberately included so the documented
    // "dispersion scans the window" trade-off has a number attached.
    bench_real_source!(
        g,
        "percentile_50_of_100",
        prices,
        Percentile::new(Identity::new(), 100, 0.5)
    );
    bench_real_source!(g, "stddev_100", prices, StdDev::new(Identity::new(), 100));
    g.finish();
}

fn bench_candle(c: &mut Criterion) {
    let candles = synth_candles(N);
    let mut g = c.benchmark_group("indicators/candle");
    g.throughput(Throughput::Elements(N as u64));
    g.bench_function("atr_14", |b| {
        b.iter(|| {
            let mut ind = Atr::new(CurrentBar::new(), 14);
            for &c in &candles {
                black_box(ind.update(fugazi::types::Atom::new(c)));
            }
        });
    });
    g.finish();
}

criterion_group!(benches, bench_scalar, bench_candle);
criterion_main!(benches);
