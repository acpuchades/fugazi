//! The Rust tier of the TA-Lib comparison (`tools/bench_three_tier.py`).
//!
//! Not a criterion target: it has to emit machine-readable ns/sample that the
//! Python driver can parse and line up against TA-Lib and the bindings, and it
//! has to honour `FUGAZI_THREE_TIER_N` so all three tiers run the same input
//! length. Criterion's output format and sampling policy fight both.
//!
//! Run directly for a human-readable table:
//!
//!     cargo bench --bench three_tier
//!
//! Add `--emit-json` for one JSON record per indicator, which is what the
//! Python driver consumes.

use std::hint::black_box;
use std::time::Instant;

use fugazi::indicators::{Atr, CurrentBar, Ema, Identity, Rsi, Sma, StdDev};
use fugazi::prelude::*;
use fugazi::runtime::{self, PayloadValue as DynValue};

mod common;
use common::synth_candles;

/// Keep in sync with `tools/bench_three_tier.py`.
const SMA_P: usize = 10;
const EMA_P: usize = 10;
const RSI_P: usize = 14;
const STDDEV_P: usize = 10;
const ATR_P: usize = 14;

const REPS: usize = 7;

fn median(mut xs: Vec<f64>) -> f64 {
    xs.sort_by(f64::total_cmp);
    xs[xs.len() / 2]
}

/// Median ns/sample over `REPS` runs of `f` across `n` samples.
fn bench(n: usize, mut f: impl FnMut()) -> f64 {
    let mut times = Vec::with_capacity(REPS);
    for _ in 0..REPS {
        let t = Instant::now();
        f();
        times.push(t.elapsed().as_secs_f64());
    }
    median(times) * 1e9 / n as f64
}

/// A `Real -> Real` view over an erased indicator — the shape
/// `python/src/carriers.rs`'s `TypedSource<Real, Real>` has. The library's own
/// `As<Out>` is `Snapshot`-input only, so it cannot stand in here.
#[derive(Clone)]
struct ErasedReal(Box<dyn runtime::PayloadIndicator>);

impl Indicator for ErasedReal {
    type Input = Real;
    type Output = Real;
    fn update(&mut self, x: Real) -> Option<Real> {
        let out = self.0.update(DynValue::Real(x))?;
        Real::try_from(out).ok()
    }
    fn value(&self) -> Option<Real> {
        self.0.value().and_then(|v| Real::try_from(v).ok())
    }
    fn warm_up_bars(&self) -> usize {
        self.0.warm_up_bars()
    }
    fn reset(&mut self) {
        self.0.reset();
    }
}

fn main() {
    let n: usize = std::env::var("FUGAZI_THREE_TIER_N")
        .ok()
        .and_then(|s| s.parse().ok())
        .unwrap_or(200_000);
    let json = std::env::args().any(|a| a == "--emit-json");

    let candles = synth_candles(n);
    let closes: Vec<Real> = candles.iter().map(|c| c.close).collect();

    let mut out: Vec<(&str, f64)> = Vec::new();

    out.push(("sma", bench(n, || {
        let mut ind = Sma::new(Identity::new(), SMA_P);
        for &p in &closes {
            black_box(ind.update(p));
        }
    })));
    out.push(("ema", bench(n, || {
        let mut ind = Ema::new(Identity::new(), EMA_P);
        for &p in &closes {
            black_box(ind.update(p));
        }
    })));
    out.push(("rsi", bench(n, || {
        let mut ind = Rsi::new(Identity::new(), RSI_P);
        for &p in &closes {
            black_box(ind.update(p));
        }
    })));
    out.push(("stddev", bench(n, || {
        let mut ind = StdDev::new(Identity::new(), STDDEV_P);
        for &p in &closes {
            black_box(ind.update(p));
        }
    })));
    // ATR consumes bars, so it is fed pre-built atoms — constructing an `Atom`
    // inside the timed loop would measure that instead.
    let atoms: Vec<fugazi::types::Atom> =
        candles.iter().map(|c| fugazi::types::Atom::new(*c)).collect();
    out.push(("atr", bench(n, || {
        let mut ind = Atr::new(CurrentBar::new(), ATR_P);
        for a in &atoms {
            black_box(ind.update(a.clone()));
        }
    })));

    // The same SMA driven through the runtime type-erasure layer — a
    // `Box<dyn DynIndicator>` exchanging `DynValue` payloads, which is exactly
    // what the Python bindings hold. Measuring it here, with no Python in
    // sight, separates the erasure cost from the FFI boundary: whatever this
    // costs above `sma`, a Python caller pays too and cannot avoid.
    out.push(("sma_erased", bench(n, || {
        let mut ind = runtime::wrap(Sma::new(Identity::<Real>::new(), SMA_P));
        for &p in &closes {
            black_box(ind.update(DynValue::Real(p)));
        }
    })));

    // Faithful to what the Python bindings actually build: `sma(identity())`
    // wraps an *already erased* source, so the chain is
    // `Box<dyn> -> Sma -> Box<dyn> -> Identity` and every sample crosses the
    // `DynValue` boundary twice in each direction. The single-boundary
    // `sma_erased` above understates it.
    out.push(("sma_erased_nested", bench(n, || {
        let inner = ErasedReal(runtime::wrap(Identity::<Real>::new()));
        let mut ind = runtime::wrap(Sma::new(inner, SMA_P));
        for &p in &closes {
            black_box(ind.update(DynValue::Real(p)));
        }
    })));

    // Everything `PyIndicator::feed` does on the Rust side of the boundary,
    // with no Python at all: the nested erased chain, the `Vec<Option<Real>>`
    // it collects into, and the `Vec<f64>` `build_floats` maps that to. If this
    // lands near the measured `feed()` cost the problem is ours; if it lands
    // far below, the rest is pyo3/NumPy and has to be chased there.
    out.push(("feed_rust_side", bench(n, || {
        let inner = ErasedReal(runtime::wrap(Identity::<Real>::new()));
        let mut ind = runtime::wrap(Sma::new(inner, SMA_P));
        let values: Vec<Option<Real>> = closes
            .iter()
            .map(|&p| ind.update(DynValue::Real(p)).and_then(|v| Real::try_from(v).ok()))
            .collect();
        let nums: Vec<Real> = values.iter().map(|v| v.unwrap_or(Real::NAN)).collect();
        black_box(nums.len());
    })));

    if json {
        for (name, ns) in &out {
            println!("{{\"name\":\"{name}\",\"ns_per_sample\":{ns:.4}}}");
        }
    } else {
        println!("n = {n} samples, median of {REPS}\n");
        println!(
            "size_of::<DynValue>() = {} B  (the payload every erased `update` moves)\n",
            std::mem::size_of::<DynValue>()
        );
        println!("{:<20}{:>14}", "indicator", "ns/sample");
        for (name, ns) in &out {
            println!("{name:<20}{ns:>14.2}");
        }
    }
}
