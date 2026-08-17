//! `PaperWallet` hot-path cost: the per-bar `update` (mark to market + flush
//! any queued order) and the `equity` read the driver takes once per bar.
//!
//! Both are swept across held-position count, because both walk the positions
//! map. `equity` in particular is called once per bar by the driver
//! (`src/backtest.rs`) *and* again inside `update` whenever a fractional order
//! resolves, so its cost is paid more than the once-per-bar it looks like.

use std::hint::black_box;

use criterion::{BenchmarkId, Criterion, Throughput, criterion_group, criterion_main};

use fugazi::prelude::*;

mod common;
use common::synth_candles;

const BARS: usize = 20_000;
const HELD: [usize; 4] = [1, 4, 16, 64];

fn primed(n_symbols: usize) -> (PaperWallet<Symbol>, Vec<Symbol>) {
    let syms: Vec<Symbol> = (0..n_symbols)
        .map(|i| fugazi::types::symbol(format!("S{i:03}")))
        .collect();
    let mut w: PaperWallet<Symbol> = PaperWallet::new(1_000_000.0);
    // Prime each symbol with a bar and a position so the maps are populated and
    // `equity` has real work to do.
    for s in &syms {
        let _ = w.update(s.clone(), Candle::new(100.0, 101.0, 99.0, 100.0, 1_000.0));
        let _ = w.set(s.clone(), Side::Buy, Size::units(1.0));
        let _ = w.update(s.clone(), Candle::new(100.0, 101.0, 99.0, 100.0, 1_000.0));
    }
    (w, syms)
}

fn bench_update(c: &mut Criterion) {
    let candles = synth_candles(BARS);
    let mut g = c.benchmark_group("wallet/update");
    g.throughput(Throughput::Elements(BARS as u64));
    for n in HELD {
        g.bench_with_input(BenchmarkId::from_parameter(n), &n, |b, &n| {
            let (mut w, syms) = primed(n);
            let sym = syms[0].clone();
            b.iter(|| {
                for &c in &candles {
                    black_box(w.update(sym.clone(), c));
                }
            });
        });
    }
    g.finish();
}

fn bench_equity(c: &mut Criterion) {
    let mut g = c.benchmark_group("wallet/equity");
    g.throughput(Throughput::Elements(BARS as u64));
    for n in HELD {
        g.bench_with_input(BenchmarkId::from_parameter(n), &n, |b, &n| {
            let (w, _syms) = primed(n);
            b.iter(|| {
                for _ in 0..BARS {
                    black_box(w.equity());
                }
            });
        });
    }
    g.finish();
}

/// A full submit → fill round-trip: `set` queues at bar N, the next `update`
/// resolves the size and books the fill through the cost pipeline.
fn bench_fill_roundtrip(c: &mut Criterion) {
    let candles = synth_candles(BARS);
    let mut g = c.benchmark_group("wallet/fill_roundtrip");
    g.throughput(Throughput::Elements(BARS as u64));
    g.bench_function("alternating_value_frac", |b| {
        b.iter(|| {
            let mut w: PaperWallet<Symbol> = PaperWallet::new(100_000.0);
            let sym = fugazi::types::symbol("X");
            for (i, &c) in candles.iter().enumerate() {
                black_box(w.update(sym.clone(), c));
                let side = if i % 2 == 0 { Side::Buy } else { Side::Sell };
                let _ = w.set(sym.clone(), side, Size::value_frac(0.5));
            }
            black_box(w.equity());
        });
    });
    g.finish();
}

criterion_group!(benches, bench_update, bench_equity, bench_fill_roundtrip);
criterion_main!(benches);
