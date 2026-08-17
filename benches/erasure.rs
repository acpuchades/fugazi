//! What a narrower erasure vocabulary would buy.
//!
//! Both runtime-typed builders — the YAML/spec layer (`NodeSpec::build` →
//! `wrap(Sma::new(AsReal::try_new(child)?, p))`) and the Python bindings
//! (`Source::new(Sma::new(source, p))`) — erase at **every level** of an
//! expression. A sample therefore crosses a `DynValue` boundary once per node,
//! and `size_of::<DynValue>()` is 88 bytes because the enum is as wide as its
//! `Atom` variant.
//!
//! Measured elsewhere: ~30 ns/sample per erased layer, which is the whole of
//! the Python gap to TA-Lib and most of the YAML-vs-Rust gap.
//!
//! This benchmark prices the alternative *before* anyone commits to it. The
//! overwhelming majority of expression nodes are `Real -> Real`; a trait
//! narrowed to that carries an `f64` instead of an 88-byte enum, with no
//! discriminant branch and no drop glue. `NarrowReal` below is that trait, in
//! the smallest form that still answers the question.
//!
//! It is a **ceiling**, not a design: a real version has to keep the
//! `DynValue` vocabulary for the bar-shaped and string-shaped nodes and fall
//! back to it whenever a chain is not purely scalar.
//!
//! Run with `cargo bench --bench erasure`.

use std::hint::black_box;
use std::time::Instant;

use fugazi::indicators::{Ema, Identity, Sma};
use fugazi::prelude::*;
use fugazi::runtime::{self, PayloadValue as DynValue};

const SMA_P: usize = 10;
const EMA_P: usize = 5;
const REPS: usize = 9;

mod common;
use common::synth_candles;

// ---------------------------------------------------------------------------
// The narrow vocabulary
// ---------------------------------------------------------------------------

/// A `Real -> Real` indicator, erased. The whole point is the signature: an
/// `f64` in and an `Option<f64>` out, where `DynIndicator` moves 88 bytes each
/// way.
trait NarrowReal: Send + Sync {
    fn update(&mut self, x: Real) -> Option<Real>;
}

/// Blanket: every concrete scalar indicator already satisfies this.
struct NarrowAdapter<I>(I);

impl<I: Indicator<Input = Real, Output = Real> + Send + Sync> NarrowReal for NarrowAdapter<I> {
    fn update(&mut self, x: Real) -> Option<Real> {
        self.0.update(x)
    }
}

/// A scalar node whose *source* is itself erased through [`NarrowReal`] — the
/// narrow twin of what `AsReal`/`TypedSource` do with `DynValue`.
struct NarrowSource(Box<dyn NarrowReal>);

impl Indicator for NarrowSource {
    type Input = Real;
    type Output = Real;
    fn update(&mut self, x: Real) -> Option<Real> {
        self.0.update(x)
    }
    fn value(&self) -> Option<Real> {
        None
    }
    fn warm_up_bars(&self) -> usize {
        1
    }
    fn reset(&mut self) {}
}

/// The `DynValue` twin of [`NarrowSource`], so the two chains differ only in
/// the payload they carry.
#[derive(Clone)]
struct WideSource(Box<dyn runtime::PayloadIndicatorSync>);

impl Indicator for WideSource {
    type Input = Real;
    type Output = Real;
    fn update(&mut self, x: Real) -> Option<Real> {
        let out = self.0.update(DynValue::Real(x))?;
        Real::try_from(out).ok()
    }
    fn value(&self) -> Option<Real> {
        None
    }
    fn warm_up_bars(&self) -> usize {
        1
    }
    fn reset(&mut self) {}
}

fn median(mut xs: Vec<f64>) -> f64 {
    xs.sort_by(f64::total_cmp);
    xs[xs.len() / 2]
}

fn bench(n: usize, mut f: impl FnMut()) -> f64 {
    let mut times = Vec::with_capacity(REPS);
    for _ in 0..REPS {
        let t = Instant::now();
        f();
        times.push(t.elapsed().as_secs_f64());
    }
    median(times) * 1e9 / n as f64
}

/// Stack `levels` erased nodes the way the Python builders do — `identity()` at
/// the bottom, then alternating `sma`/`ema` upwards, each wrapping the erased
/// handle below it.
///
/// **`#[inline(never)]` is load-bearing.** Built inline, the concrete type is
/// visible at the `erase` call and LLVM devirtualises the whole chain — which
/// is what an earlier version of this benchmark did, reporting +0.4 ns/level
/// for a vocabulary that actually costs far more. A boxed chain a builder
/// assembled at run time can never be devirtualised, so the benchmark must not
/// be either.
#[inline(never)]
fn chain_levels(levels: usize) -> runtime::Chain<Real, Real> {
    let mut c: runtime::Chain<Real, Real> = runtime::erase(Identity::<Real>::new());
    for i in 1..levels {
        c = if i % 2 == 1 {
            runtime::erase(Sma::new(c, SMA_P))
        } else {
            runtime::erase(Ema::new(c, EMA_P))
        };
    }
    c
}

/// The payload twin of [`chain_levels`], opaque for the same reason.
#[inline(never)]
fn payload_levels(levels: usize) -> Box<dyn runtime::PayloadIndicatorSync> {
    let mut c = runtime::wrap_sync(Identity::<Real>::new());
    for i in 1..levels {
        let inner = WideSource(c);
        c = if i % 2 == 1 {
            runtime::wrap_sync(Sma::new(inner, SMA_P))
        } else {
            runtime::wrap_sync(Ema::new(inner, EMA_P))
        };
    }
    c
}

fn main() {
    let n: usize = 200_000;
    let closes: Vec<Real> = synth_candles(n).iter().map(|c| c.close).collect();

    println!("size_of::<PayloadValue>()    = {} B", std::mem::size_of::<DynValue>());
    println!("size_of::<Option<Real>>()    = {} B\n", std::mem::size_of::<Option<Real>>());

    // ---- baseline: no erasure at all ---------------------------------------
    let concrete_2 = bench(n, || {
        let mut ind = Sma::new(Identity::<Real>::new(), SMA_P);
        for &p in &closes {
            black_box(ind.update(p));
        }
    });

    let payload = |levels: usize| {
        bench(n, || {
            let mut ind = black_box(payload_levels(levels));
            for &p in &closes {
                black_box(ind.update(DynValue::Real(p)));
            }
        })
    };
    let chain = |levels: usize| {
        bench(n, || {
            let mut ind = black_box(chain_levels(levels));
            for &p in &closes {
                black_box(ind.update(p));
            }
        })
    };

    // The hand-rolled single-method trait the first pass of this benchmark
    // used, kept as the *floor*: the least an erased scalar boundary can cost.
    let narrow_2 = bench(n, || {
        let inner = NarrowSource(Box::new(NarrowAdapter(Identity::<Real>::new())));
        let mut ind: Box<dyn NarrowReal> = black_box(Box::new(NarrowAdapter(Sma::new(inner, SMA_P))));
        for &p in &closes {
            black_box(ind.update(p));
        }
    });

    let (p2, p3, p5) = (payload(2), payload(3), payload(5));
    let (c2, c3, c5) = (chain(2), chain(3), chain(5));

    println!("{:<34}{:>12}{:>13}", "chain", "ns/sample", "vs concrete");
    let row = |name: &str, v: f64| {
        println!("{name:<34}{v:>12.2}{:>12.1}x", v / concrete_2);
    };
    row("concrete (no erasure), 1 node", concrete_2);
    row("PayloadValue, 2 levels", p2);
    row("PayloadValue, 3 levels", p3);
    row("PayloadValue, 5 levels", p5);
    row("Chain, 2 levels", c2);
    row("Chain, 3 levels", c3);
    row("Chain, 5 levels", c5);
    row("hand-rolled 1-method trait, 2 levels", narrow_2);

    println!(
        "\nper extra level:  PayloadValue {:+.1} ns   Chain {:+.1} ns",
        (p5 - p2) / 3.0,
        (c5 - c2) / 3.0,
    );
}
