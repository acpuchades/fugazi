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
use fugazi::runtime::{self, DynValue};

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
struct WideSource(Box<dyn runtime::DynIndicatorSync>);

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

fn main() {
    let n: usize = 200_000;
    let closes: Vec<Real> = synth_candles(n).iter().map(|c| c.close).collect();

    println!("size_of::<DynValue>()        = {} B", std::mem::size_of::<DynValue>());
    println!("size_of::<Option<Real>>()    = {} B\n", std::mem::size_of::<Option<Real>>());

    // ---- baseline: no erasure at all ---------------------------------------
    let concrete_2 = bench(n, || {
        let mut ind = Sma::new(Ema::new(Identity::<Real>::new(), EMA_P), SMA_P);
        for &p in &closes {
            black_box(ind.update(p));
        }
    });

    // ---- today: one `DynValue` boundary per level --------------------------
    let wide_2 = bench(n, || {
        let inner = WideSource(runtime::wrap_sync(Ema::new(Identity::<Real>::new(), EMA_P)));
        let mut ind = runtime::wrap(Sma::new(inner, SMA_P));
        for &p in &closes {
            black_box(ind.update(DynValue::Real(p)));
        }
    });
    let wide_3 = bench(n, || {
        let l1 = WideSource(runtime::wrap_sync(Identity::<Real>::new()));
        let l2 = WideSource(runtime::wrap_sync(Ema::new(l1, EMA_P)));
        let mut ind = runtime::wrap(Sma::new(l2, SMA_P));
        for &p in &closes {
            black_box(ind.update(DynValue::Real(p)));
        }
    });

    // ---- proposed: one `f64` boundary per level ----------------------------
    let narrow_2 = bench(n, || {
        let inner = NarrowSource(Box::new(NarrowAdapter(Ema::new(
            Identity::<Real>::new(),
            EMA_P,
        ))));
        let mut ind: Box<dyn NarrowReal> = Box::new(NarrowAdapter(Sma::new(inner, SMA_P)));
        for &p in &closes {
            black_box(ind.update(p));
        }
    });
    let narrow_3 = bench(n, || {
        let l1 = NarrowSource(Box::new(NarrowAdapter(Identity::<Real>::new())));
        let l2 = NarrowSource(Box::new(NarrowAdapter(Ema::new(l1, EMA_P))));
        let mut ind: Box<dyn NarrowReal> = Box::new(NarrowAdapter(Sma::new(l2, SMA_P)));
        for &p in &closes {
            black_box(ind.update(p));
        }
    });

    println!("{:<34}{:>12}{:>12}", "chain", "ns/sample", "vs concrete");
    let row = |name: &str, v: f64| {
        println!("{name:<34}{v:>12.2}{:>11.1}x", v / concrete_2);
    };
    row("concrete (no erasure), 2 nodes", concrete_2);
    row("DynValue erasure, 2 levels", wide_2);
    row("DynValue erasure, 3 levels", wide_3);
    row("narrow f64 erasure, 2 levels", narrow_2);
    row("narrow f64 erasure, 3 levels", narrow_3);

    println!(
        "\nper extra level:  DynValue {:+.1} ns   narrow {:+.1} ns",
        wide_3 - wide_2,
        narrow_3 - narrow_2,
    );
}
