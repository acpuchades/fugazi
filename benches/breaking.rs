//! Prototypes for the **breaking** changes proposed in `docs/PERFORMANCE.md`.
//!
//! Each candidate would touch a large surface, so each is measured here in a
//! form that does not require making the change — an upper bound on the win,
//! paid for in bench code rather than in a refactor nobody has agreed to.
//!
//! (`Sym = Arc<str>` used to be measured here too. It has since been
//! implemented — `fugazi::Symbol` — so its numbers live in the results section
//! of `docs/PERFORMANCE.md` rather than here. This file is for candidates that
//! have *not* been done.)
//!
//! **`Indicator::update(&Self::Input)`** is prototyped as a parallel
//!    by-reference chain (`RefIndicator`) computing the same SMA crossover as
//!    the library's by-value one. The gap is the clone traffic the signature
//!    forces: `Combine` feeds the same input to both sides, so every binary node
//!    clones, and `Pick` clones the projected `Atom` twice per bar.
//!
//! These are ceilings, not promises: the by-reference chain is monomorphic and
//! shallow, so it also enjoys inlining a `Box<dyn Indicator>` tree would not.

use std::hint::black_box;
use criterion::{Criterion, Throughput, criterion_group, criterion_main};

use fugazi::indicators::{Close, Pick, Sma};
use fugazi::prelude::*;
use fugazi::types::{Atom, Selector, Snapshot};

mod common;
use common::synth_candles;

const BARS: usize = 2_000;

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
    symbol: Symbol,
}
impl RefIndicator for RefClose {
    type Input = Snapshot<Symbol>;
    type Output = Real;
    fn update(&mut self, snap: &Snapshot<Symbol>) -> Option<Real> {
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

fn ref_chain(symbol: &Symbol, fast: usize, slow: usize) -> impl RefIndicator<Input = Snapshot<Symbol>> {
    let sma = |p: usize| RefSma {
        source: RefClose { symbol: symbol.clone() },
        period: p,
        window: std::collections::VecDeque::with_capacity(p),
        sum: 0.0,
    };
    RefCrossesAbove { lhs: sma(fast), rhs: sma(slow), prev: None }
}

fn bench_input_by_reference(c: &mut Criterion) {
    let syms: Vec<Symbol> = vec![fugazi::types::symbol("SYMBOL0000")];
    let candles = synth_candles(BARS);
    let snaps: Vec<Snapshot<Symbol>> = candles
        .iter()
        .map(|c| {
            let mut s = Snapshot::new();
            s.push(Some(syms[0].clone()), None, Atom::new(*c));
            s
        })
        .collect();

    let mut g = c.benchmark_group("breaking/input_by_ref");
    g.throughput(Throughput::Elements(BARS as u64));

    g.bench_function("by_value_library", |b| {
        b.iter(|| {
            let close = || Close::of(Pick::matching(Selector::by_symbol(syms[0].clone())));
            let mut chain = Sma::new(close(), 5).crosses_above(Sma::new(close(), 20));
            for s in &snaps {
                black_box(chain.update(s.clone()));
            }
        });
    });

    g.bench_function("by_reference_proto", |b| {
        b.iter(|| {
            let mut chain = ref_chain(&syms[0], 5, 20);
            for s in &snaps {
                black_box(chain.update(s));
            }
        });
    });
    g.finish();
}

criterion_group!(benches, bench_input_by_reference);
criterion_main!(benches);
