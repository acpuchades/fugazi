//! Prototypes for the **breaking** changes proposed in `docs/PERFORMANCE.md`.
//!
//! Each candidate would touch a large surface, so each is measured here in a
//! form that does not require making the change — an upper bound on the win,
//! paid for in bench code rather than in a refactor nobody has agreed to.
//!
//! 1. **`Sym = Arc<str>`** needs no prototype at all: `Snapshot`, `PaperWallet`
//!    and `MultiAssetStrategy` are already generic over `Sym`, and `Arc<str>`
//!    satisfies every bound. The same workload is simply run both ways. What a
//!    real change would add is *interning* — one `Arc` per distinct symbol for
//!    the whole run — which this approximates by cloning from a fixed set.
//!
//! 2. **`Indicator::update(&Self::Input)`** is prototyped as a parallel
//!    by-reference chain (`RefIndicator`) computing the same SMA crossover as
//!    the library's by-value one. The gap is the clone traffic the signature
//!    forces: `Combine` feeds the same input to both sides, so every binary node
//!    clones, and `Pick` clones the projected `Atom` twice per bar.
//!
//! These are ceilings, not promises: the by-reference chain is monomorphic and
//! shallow, so it also enjoys inlining a `Box<dyn Indicator>` tree would not.

use std::hint::black_box;
use std::sync::Arc;

use criterion::{BenchmarkId, Criterion, Throughput, criterion_group, criterion_main};

use fugazi::indicators::{Close, Pick, Sma};
use fugazi::prelude::*;
use fugazi::strategies::MultiAssetStrategy;
use fugazi::types::{Atom, Selector, Snapshot};

mod common;
use common::synth_candles;

const BARS: usize = 2_000;
const SYMBOLS: [usize; 3] = [8, 32, 64];

// ---------------------------------------------------------------------------
// Candidate 1 — Sym = Arc<str> vs Sym = String
// ---------------------------------------------------------------------------

/// Snapshots keyed by a caller-chosen symbol type, built from one fixed symbol
/// set — so `Arc<str>` clones are refcount bumps and `String` clones are
/// allocations, which is exactly the difference under test.
fn snapshots_keyed<S: Clone>(syms: &[S], bars: usize) -> Vec<Snapshot<S>> {
    let candles = synth_candles(bars);
    (0..bars)
        .map(|b| {
            let mut snap = Snapshot::new();
            for (i, s) in syms.iter().enumerate() {
                snap.push(Some(s.clone()), None, Atom::new(candles[(b + i * 7) % bars]));
            }
            snap
        })
        .collect()
}

fn strategy_for<S>() -> MultiAssetStrategy<S>
where
    S: Clone + PartialEq + std::hash::Hash + Eq + 'static + Send + Sync,
{
    let close = |sym: &S| Close::of(Pick::matching(Selector::by_symbol(sym.clone())));
    MultiAssetStrategy::<S>::with_initial_equity(10_000.0).long_on(
        move |sym: &S| Sma::new(close(sym), 5).crosses_above(Sma::new(close(sym), 20)),
        move |sym: &S| Sma::new(close(sym), 5).crosses_below(Sma::new(close(sym), 20)),
    )
}

fn bench_symbol_type(c: &mut Criterion) {
    let mut g = c.benchmark_group("breaking/symbol_type");
    for n in SYMBOLS {
        let owned: Vec<String> = (0..n).map(|i| format!("SYMBOL{i:04}")).collect();
        let shared: Vec<Arc<str>> = owned.iter().map(|s| Arc::from(s.as_str())).collect();
        let snaps_owned = snapshots_keyed(&owned, BARS);
        let snaps_shared = snapshots_keyed(&shared, BARS);

        g.throughput(Throughput::Elements((BARS * n) as u64));
        g.bench_with_input(BenchmarkId::new("String", n), &n, |b, _| {
            b.iter(|| {
                let mut s = strategy_for::<String>();
                let mut w: PaperWallet<String> = PaperWallet::new(10_000.0);
                let r = fugazi::backtest::run(&mut s, &mut w, snaps_owned.iter().cloned());
                black_box(r.equity_curve.len());
            });
        });
        g.bench_with_input(BenchmarkId::new("Arc_str", n), &n, |b, _| {
            b.iter(|| {
                let mut s = strategy_for::<Arc<str>>();
                let mut w: PaperWallet<Arc<str>> = PaperWallet::new(10_000.0);
                let r = fugazi::backtest::run(&mut s, &mut w, snaps_shared.iter().cloned());
                black_box(r.equity_curve.len());
            });
        });
    }
    g.finish();
}

/// Snapshot *construction* — the 3-allocations-per-bar, 201-bytes-per-bar figure
/// from `benches/footprint.rs` is dominated by the per-entry symbol clone.
fn bench_snapshot_build(c: &mut Criterion) {
    let mut g = c.benchmark_group("breaking/snapshot_build");
    let n = 8usize;
    let owned: Vec<String> = (0..n).map(|i| format!("SYMBOL{i:04}")).collect();
    let shared: Vec<Arc<str>> = owned.iter().map(|s| Arc::from(s.as_str())).collect();
    g.throughput(Throughput::Elements((BARS * n) as u64));
    g.bench_function("String", |b| {
        b.iter(|| black_box(snapshots_keyed(&owned, BARS).len()));
    });
    g.bench_function("Arc_str", |b| {
        b.iter(|| black_box(snapshots_keyed(&shared, BARS).len()));
    });
    g.finish();
}

// ---------------------------------------------------------------------------
// Candidate 2 — update(&Input) vs update(Input)
// ---------------------------------------------------------------------------

/// The by-reference twin of [`Indicator`], with just enough surface to express
/// an SMA crossover. Deliberately minimal — the point is the `&` in `update`.
trait RefIndicator {
    type Input: ?Sized;
    type Output: Copy;
    fn update(&mut self, input: &Self::Input) -> Option<Self::Output>;
}

/// `Pick` + `Close`, fused: projects one symbol's close out of a snapshot
/// **without cloning the `Atom`**. The library pair clones it twice per bar
/// (once into `Pick::value`, once for the return) to satisfy `Output = Atom`.
struct RefClose {
    symbol: String,
}
impl RefIndicator for RefClose {
    type Input = Snapshot<String>;
    type Output = Real;
    fn update(&mut self, snap: &Snapshot<String>) -> Option<Real> {
        snap.iter()
            .find(|(s, _, _)| *s == Some(&self.symbol))
            .and_then(|(_, _, a)| a.candle)
            .map(|c| c.close)
    }
}

struct RefSma<S> {
    source: S,
    period: usize,
    window: std::collections::VecDeque<Real>,
    sum: Real,
}
impl<S: RefIndicator<Output = Real>> RefIndicator for RefSma<S> {
    type Input = S::Input;
    type Output = Real;
    fn update(&mut self, input: &S::Input) -> Option<Real> {
        let x = self.source.update(input)?;
        self.window.push_back(x);
        self.sum += x;
        if self.window.len() > self.period {
            self.sum -= self.window.pop_front().expect("non-empty");
        }
        (self.window.len() == self.period).then(|| self.sum / self.period as Real)
    }
}

/// The node that motivates the change: it feeds **the same input to both
/// sides**. By value that is a clone per bar per binary node; by reference it is
/// nothing at all.
struct RefCrossesAbove<L, R> {
    lhs: L,
    rhs: R,
    prev: Option<bool>,
}
impl<I: ?Sized, L, R> RefIndicator for RefCrossesAbove<L, R>
where
    L: RefIndicator<Input = I, Output = Real>,
    R: RefIndicator<Input = I, Output = Real>,
{
    type Input = I;
    type Output = bool;
    fn update(&mut self, input: &I) -> Option<bool> {
        let l = self.lhs.update(input);
        let r = self.rhs.update(input);
        let now = match (l, r) {
            (Some(l), Some(r)) => l > r,
            _ => return None,
        };
        let fired = self.prev.is_some_and(|p| !p) && now;
        self.prev = Some(now);
        Some(fired)
    }
}

fn ref_chain(symbol: &str, fast: usize, slow: usize) -> impl RefIndicator<Input = Snapshot<String>> {
    let sma = |p: usize| RefSma {
        source: RefClose { symbol: symbol.to_string() },
        period: p,
        window: std::collections::VecDeque::with_capacity(p),
        sum: 0.0,
    };
    RefCrossesAbove { lhs: sma(fast), rhs: sma(slow), prev: None }
}

fn bench_input_by_reference(c: &mut Criterion) {
    let syms: Vec<String> = vec!["SYMBOL0000".to_string()];
    let snaps = snapshots_keyed(&syms, BARS);

    let mut g = c.benchmark_group("breaking/input_by_ref");
    g.throughput(Throughput::Elements(BARS as u64));

    g.bench_function("by_value_library", |b| {
        b.iter(|| {
            let close = || Close::of(Pick::matching(Selector::by_symbol("SYMBOL0000".to_string())));
            let mut chain = Sma::new(close(), 5).crosses_above(Sma::new(close(), 20));
            for s in &snaps {
                black_box(chain.update(s.clone()));
            }
        });
    });

    g.bench_function("by_reference_proto", |b| {
        b.iter(|| {
            let mut chain = ref_chain("SYMBOL0000", 5, 20);
            for s in &snaps {
                black_box(chain.update(s));
            }
        });
    });
    g.finish();
}

criterion_group!(
    benches,
    bench_symbol_type,
    bench_snapshot_build,
    bench_input_by_reference
);
criterion_main!(benches);
