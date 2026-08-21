//! End-to-end `backtest::run` cost, and the YAML-vs-Rust gap.
//!
//! Mirrors `tests/perf_bench.rs::bench_yaml_vs_rust_macd_crossover` so the
//! number stays comparable with the earlier audit's reading (which concluded
//! the YAML path costs ~1.9× the Rust one, and attributed the gap to the
//! type-erasure layer). Both sides express the same MACD crossover; the Rust
//! catalogue's `macd_crossover` composes it through `.shared()`, so one `Macd`
//! drives both components, while the spec builder builds two.

use std::hint::black_box;

use criterion::{Criterion, Throughput, criterion_group, criterion_main};

use fugazi::prelude::*;
use fugazi::spec::SingleStrategySpec;

mod common;
use common::single_snapshots;

const BARS: usize = 50_000;

const MACD_YAML: &str = r#"
symbol: X
long:
  enter: !crosses_above
    lhs: !macd_line { fast: 12, slow: 26, signal: 9 }
    rhs: !macd_signal { fast: 12, slow: 26, signal: 9 }
  exit: !crosses_below
    lhs: !macd_line { fast: 12, slow: 26, signal: 9 }
    rhs: !macd_signal { fast: 12, slow: 26, signal: 9 }
"#;

const SMA_YAML: &str = r#"
symbol: X
long:
  enter: !crosses_above
    lhs: !sma { source: close, period: 5 }
    rhs: !sma { source: close, period: 20 }
  exit: !crosses_below
    lhs: !sma { source: close, period: 5 }
    rhs: !sma { source: close, period: 20 }
"#;

fn spec_of(yaml: &str) -> SingleStrategySpec {
    SingleStrategySpec::from_text_with_params_in(
        yaml,
        &Default::default(),
        std::path::Path::new("."),
        "(bench)",
    )
    .expect("bench spec parses")
}

fn bench_macd(c: &mut Criterion) {
    let snaps = single_snapshots("X", BARS);
    let spec = spec_of(MACD_YAML);
    let schema = fugazi::market::Schema::empty();

    let mut g = c.benchmark_group("driver/macd_crossover");
    g.throughput(Throughput::Elements(BARS as u64));

    g.bench_function("rust", |b| {
        b.iter(|| {
            let mut strat =
                fugazi::strategies::trend::macd_crossover(fugazi::types::symbol("X"), 12, 26, 9);
            let mut w: PaperWallet<Symbol> = PaperWallet::new(10_000.0);
            let rep = fugazi::backtest::run(&mut strat, &mut w, snaps.iter().cloned());
            black_box(rep.equity_curve.len());
        });
    });

    g.bench_function("yaml", |b| {
        b.iter(|| {
            let mut strat = spec
                .try_build(10_000.0, &schema)
                .expect("bench spec builds");
            let mut w: PaperWallet<Symbol> = PaperWallet::new(10_000.0);
            let rep = fugazi::backtest::run(&mut strat, &mut w, snaps.iter().cloned());
            black_box(rep.equity_curve.len());
        });
    });
    g.finish();
}

/// The same comparison on a shallower tree with no `.shared()` on either side —
/// isolates the type-erasure cost from the shared-component mutex.
fn bench_sma(c: &mut Criterion) {
    let snaps = single_snapshots("X", BARS);
    let spec = spec_of(SMA_YAML);
    let schema = fugazi::market::Schema::empty();

    let mut g = c.benchmark_group("driver/sma_crossover");
    g.throughput(Throughput::Elements(BARS as u64));

    g.bench_function("rust", |b| {
        b.iter(|| {
            let mut strat =
                fugazi::strategies::trend::ma_crossover(fugazi::types::symbol("X"), 5, 20);
            let mut w: PaperWallet<Symbol> = PaperWallet::new(10_000.0);
            let rep = fugazi::backtest::run(&mut strat, &mut w, snaps.iter().cloned());
            black_box(rep.equity_curve.len());
        });
    });

    g.bench_function("yaml", |b| {
        b.iter(|| {
            let mut strat = spec
                .try_build(10_000.0, &schema)
                .expect("bench spec builds");
            let mut w: PaperWallet<Symbol> = PaperWallet::new(10_000.0);
            let rep = fugazi::backtest::run(&mut strat, &mut w, snaps.iter().cloned());
            black_box(rep.equity_curve.len());
        });
    });
    g.finish();
}

criterion_group!(benches, bench_macd, bench_sma);
criterion_main!(benches);
