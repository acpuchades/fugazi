//! Expression-tree scaling: what one bar costs as the tree gets deeper.
//!
//! The tree is a **left spine** — `!and { lhs: <depth-1>, rhs: !gt { sma, sma } }` —
//! so node count grows *linearly* in depth (two SMAs and a comparison per
//! level). Anything that grows faster than linearly here is the recursion
//! shape, not the work.
//!
//! Three things are timed separately, because they scale differently:
//!
//! * `update` — advancing the chain. Linear in node count, as expected.
//! * `is_ready` — `bars_seen >= stable_bars()`, which walks the whole tree
//!   *every bar*. `Combine::unstable_bars` calls `stable_bars()` on both
//!   children and then `warm_up_bars()` on itself, which walks both children
//!   again; through `Box<dyn Signal>` the calls are opaque, so LLVM cannot fold
//!   the repetition away. If this curve is super-linear while `update` is
//!   linear, the readiness check — not the arithmetic — is the cost.
//! * `drive` — the two together, as `backtest::run` pays them.

use std::hint::black_box;

use criterion::{BenchmarkId, Criterion, criterion_group, criterion_main};

use fugazi::prelude::*;
use fugazi::spec::{DynSingleStrategy, SingleStrategySpec};

mod common;
use common::single_snapshots;

const DEPTHS: [usize; 5] = [1, 2, 4, 6, 8];
const BARS: usize = 2_000;

/// A left-spine `!and` chain of `depth` SMA comparisons, each on its own period
/// pair so nothing collapses into a shared sub-tree.
fn spine_yaml(depth: usize) -> String {
    fn leaf(i: usize) -> String {
        let (fast, slow) = (3 + i, 20 + i * 3);
        format!(
            "!gt {{ lhs: !sma {{ source: close, period: {fast} }}, \
                    rhs: !sma {{ source: close, period: {slow} }} }}"
        )
    }
    let mut expr = leaf(0);
    for i in 1..depth {
        expr = format!("!and {{ lhs: {expr}, rhs: {} }}", leaf(i));
    }
    format!("root: X\nlong:\n  enter: {expr}\n")
}

fn build(depth: usize) -> DynSingleStrategy {
    let spec = SingleStrategySpec::from_text_with_params_in(
        &spine_yaml(depth),
        &Default::default(),
        std::path::Path::new("."),
        std::path::Path::new("."),
        "(bench)",
    )
    .expect("bench spec parses");
    spec.try_build(10_000.0, &fugazi::market::Schema::empty())
        .expect("bench spec builds")
}

fn bench_update(c: &mut Criterion) {
    let snaps = single_snapshots("X", BARS);
    let mut g = c.benchmark_group("tree/update");
    g.throughput(criterion::Throughput::Elements(BARS as u64));
    for depth in DEPTHS {
        g.bench_with_input(BenchmarkId::from_parameter(depth), &depth, |b, &depth| {
            let mut strat = build(depth);
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

/// The F1 probe: `is_ready()` alone, nothing else. Flat means the threshold is
/// cached; a curve means it is recomputed per bar.
fn bench_is_ready(c: &mut Criterion) {
    let mut g = c.benchmark_group("tree/is_ready");
    g.throughput(criterion::Throughput::Elements(BARS as u64));
    for depth in DEPTHS {
        g.bench_with_input(BenchmarkId::from_parameter(depth), &depth, |b, &depth| {
            let strat = build(depth);
            b.iter(|| {
                for _ in 0..BARS {
                    black_box(strat.is_ready());
                }
            });
        });
    }
    g.finish();
}

/// End-to-end: what the driver actually pays per bar at each depth.
fn bench_drive(c: &mut Criterion) {
    let snaps = single_snapshots("X", BARS);
    let mut g = c.benchmark_group("tree/drive");
    g.throughput(criterion::Throughput::Elements(BARS as u64));
    for depth in DEPTHS {
        g.bench_with_input(BenchmarkId::from_parameter(depth), &depth, |b, &depth| {
            b.iter(|| {
                let mut strat = build(depth);
                let mut w: PaperWallet<Symbol> = PaperWallet::new(10_000.0);
                let rep = fugazi::backtest::run(&mut strat, &mut w, snaps.iter().cloned());
                black_box(rep.equity_curve.len());
            });
        });
    }
    g.finish();
}

criterion_group!(benches, bench_update, bench_is_ready, bench_drive);
criterion_main!(benches);
