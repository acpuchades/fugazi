//! Universe scaling: per-bar cost as the symbol count grows.
//!
//! `MultiAssetStrategy::update` advances one chain set per symbol and feeds
//! each a clone of the whole snapshot, so linear growth in N is expected and
//! correct. Super-linear growth is not: it means a per-symbol loop is scanning
//! the whole snapshot (an O(N²) lookup per bar) or a clone is deep-copying.
//!
//! Mirrors `tests/perf_bench.rs::bench_snapshot_clone_scaling`; the per-symbol
//! ns/bar column in the criterion output is the one to read, since the absolute
//! time necessarily grows with N.

use std::hint::black_box;

use criterion::{BenchmarkId, Criterion, Throughput, criterion_group, criterion_main};

use fugazi::prelude::*;
use fugazi::strategies::MultiAssetStrategy;

mod common;
use common::multi_snapshots;

const BARS: usize = 2_000;
const SYMBOLS: [usize; 5] = [2, 8, 16, 32, 64];

fn strategy() -> MultiAssetStrategy<Symbol> {
    use fugazi::indicators::{Close, Pick, Sma};
    let close = |sym: &Symbol| Close::of(Pick::matching(Selector::by_symbol(sym.clone())));
    MultiAssetStrategy::<Symbol>::with_initial_equity(10_000.0).long_on(
        move |sym: &Symbol| Sma::new(close(sym), 5).crosses_above(Sma::new(close(sym), 20)),
        move |sym: &Symbol| Sma::new(close(sym), 5).crosses_below(Sma::new(close(sym), 20)),
    )
}

fn bench_scaling(c: &mut Criterion) {
    let mut g = c.benchmark_group("multi_asset/drive");
    for n in SYMBOLS {
        let snaps = multi_snapshots(n, BARS);
        // Per *symbol-bar*, so a linear implementation reports a flat number
        // across the whole sweep and a quadratic one climbs.
        g.throughput(Throughput::Elements((BARS * n) as u64));
        g.bench_with_input(BenchmarkId::from_parameter(n), &n, |b, _| {
            b.iter(|| {
                let mut strat = strategy();
                let mut w: PaperWallet<Symbol> = PaperWallet::new(10_000.0);
                let rep = fugazi::backtest::run(&mut strat, &mut w, snaps.iter().cloned());
                black_box(rep.equity_curve.len());
            });
        });
    }
    g.finish();
}

/// The `update` half alone — no wallet, no trading — so a regression can be
/// attributed to the strategy loop rather than to execution.
fn bench_update_only(c: &mut Criterion) {
    let mut g = c.benchmark_group("multi_asset/update");
    for n in SYMBOLS {
        let snaps = multi_snapshots(n, BARS);
        g.throughput(Throughput::Elements((BARS * n) as u64));
        g.bench_with_input(BenchmarkId::from_parameter(n), &n, |b, _| {
            let mut strat = strategy();
            b.iter(|| {
                for s in &snaps {
                    strat.update(s.clone());
                }
                black_box(&strat);
            });
        });
    }
    g.finish();
}

criterion_group!(benches, bench_scaling, bench_update_only);
criterion_main!(benches);
