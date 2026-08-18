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

use fugazi::indicators::{
    Adx, Aroon, Atr, Bollinger, Dmi, Ema, Identity, Macd, Rsi, Sma, StdDev,
};
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
/// Multi-output. Keep in sync with `tools/bench_talib_native.c`.
const MACD_FAST: usize = 12;
const MACD_SLOW: usize = 26;
const MACD_SIGNAL: usize = 9;
const BBANDS_P: usize = 20;
const BBANDS_K: Real = 2.0;
const AROON_P: usize = 14;
const DMI_P: usize = 14;

const REPS: usize = 7;
/// Discarded reps before timing starts.
///
/// Load-bearing, not politeness: a cold process on this machine reports TA-Lib's
/// SMA at 1.99 ns/sample and a warm one at 1.38 — a 44% error from CPU frequency
/// ramp and cold caches. Every tier of the comparison must discard the same way
/// or the tiers are not comparable; see `tools/bench_talib_native.c`.
const WARMUP: usize = 2;

/// Every per-rep ns/sample for `f`, ascending, after `WARMUP` discarded runs.
///
/// All of them, not just the median: the driver keeps the samples so a
/// distribution can be re-analysed or plotted with error bars without re-running
/// anything. On a machine where the same fixed workload has been seen to drift
/// 3x, the spread is not incidental — it is the part that says whether two
/// numbers can be compared at all.
fn samples(n: usize, mut f: impl FnMut()) -> Vec<f64> {
    for _ in 0..WARMUP {
        f();
    }
    let mut times = Vec::with_capacity(REPS);
    for _ in 0..REPS {
        let t = Instant::now();
        f();
        times.push(t.elapsed().as_secs_f64() * 1e9 / n as f64);
    }
    times.sort_by(f64::total_cmp);
    times
}

/// Alias kept so the call sites below read as measurements rather than plumbing.
fn bench(n: usize, f: impl FnMut()) -> Vec<f64> {
    samples(n, f)
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

    let mut out: Vec<(&str, Vec<f64>)> = Vec::new();

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
    // ATR consumes whole bars, so it is fed `Candle`s directly — the analogue of
    // `TA_ATR` reading three flat `double` arrays, and the only fair shape for
    // an indicator-throughput comparison.
    //
    // It used to hold a `Vec<Atom>` and pass `a.clone()`. That charged fugazi an
    // 88-byte `Atom` copy per bar which TA-Lib does not pay and which real code
    // does not either (the driver *moves* the atom out of the snapshot). Measured
    // with callgrind: 146.5 instructions/bar with the clone against 34.0 without
    // it, so **77% of the reported ATR cost was the benchmark's own bookkeeping**
    // — and it was the number that made fugazi's ATR look 2.8x slower than
    // native TA-Lib when the real figure is 2.2x. `benches/icount.rs` keeps
    // `atr_atom` / `atr_candle` as workloads so the split stays reproducible.
    out.push(("atr", bench(n, || {
        let mut ind = Atr::new(Identity::<fugazi::market::Candle>::new(), ATR_P);
        for c in &candles {
            black_box(ind.update(*c));
        }
    })));

    // ---- multi-output ----------------------------------------------------
    //
    // Every line, once per bar — the shape `TA_MACD` / `TA_BBANDS` / `TA_AROON`
    // have, where one call fills every output array. A fugazi multi-output
    // `update` returns the whole value struct, so this is the like-for-like
    // comparison; the per-*line* cost is measured separately below.
    //
    // Each is fed the domain it consumes: `Macd` and `Bollinger` take a `Real`
    // series, the three bar indicators take `Candle`s by value. Same reason
    // `atr` above does — a `Vec<Atom>` clone per bar is the benchmark's own
    // bookkeeping and TA-Lib pays no analogue of it.
    out.push(("macd", bench(n, || {
        let mut ind = Macd::new(Identity::new(), MACD_FAST, MACD_SLOW, MACD_SIGNAL);
        for &p in &closes {
            black_box(ind.update(p));
        }
    })));
    out.push(("bbands", bench(n, || {
        let mut ind = Bollinger::new(Identity::new(), BBANDS_P, BBANDS_K);
        for &p in &closes {
            black_box(ind.update(p));
        }
    })));
    out.push(("aroon", bench(n, || {
        let mut ind = Aroon::new(Identity::<fugazi::market::Candle>::new(), AROON_P);
        for c in &candles {
            black_box(ind.update(*c));
        }
    })));
    out.push(("dmi", bench(n, || {
        let mut ind = Dmi::new(Identity::<fugazi::market::Candle>::new(), DMI_P);
        for c in &candles {
            black_box(ind.update(*c));
        }
    })));
    out.push(("adx", bench(n, || {
        let mut ind = Adx::new(Identity::<fugazi::market::Candle>::new(), DMI_P);
        for c in &candles {
            black_box(ind.update(*c));
        }
    })));

    // What a *strategy* pays for two lines of one MACD, which is the question
    // the whole-struct rows above do not answer.
    //
    // `Component` clones its source, so this is two independent MACDs advanced
    // side by side — exactly what `src/spec/expr.rs` builds for a document
    // naming `!macd_line` and `!macd_signal`, and what `macd.line()` /
    // `macd.signal()` build in hand-written Rust. TA-Lib's single `TA_MACD`
    // call is the baseline for both, so whatever this costs above `macd` is
    // duplicated work fugazi is doing and TA-Lib is not.
    out.push(("macd_two_lines", bench(n, || {
        let macd = Macd::new(Identity::<Real>::new(), MACD_FAST, MACD_SLOW, MACD_SIGNAL);
        let (mut line, mut signal) = (macd.line(), macd.signal());
        for &p in &closes {
            black_box(line.update(p));
            black_box(signal.update(p));
        }
    })));

    // The same two lines off a `Shared` handle: one MACD, advanced once per bar,
    // both accessors projecting out of the cached output. The library has had
    // this since the beginning; nothing that builds from a spec uses it.
    out.push(("macd_two_lines_shared", bench(n, || {
        let macd = Macd::new(Identity::<Real>::new(), MACD_FAST, MACD_SLOW, MACD_SIGNAL).shared();
        let (mut line, mut signal) = (macd.line(), macd.signal());
        for &p in &closes {
            black_box(line.update(p));
            black_box(signal.update(p));
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

    // Everything `PyMulti::feed` does on the Rust side of the boundary, with no
    // Python at all: the chunked fold into a flat row-major buffer, then the
    // scatter into one output column per line. If this lands near the measured
    // `feed()` cost the problem is ours; if it lands far below, the rest is
    // pyo3/NumPy and has to be chased there. The scalar twin is
    // `feed_rust_side` above, and the pair is the whole point — a fixed ~25
    // ns/sample appears between a 1-column and a 2-column `feed`, and this says
    // which side of the boundary it is on.
    out.push(("feed_multi_rust_side", bench(n, || {
        const LINES: usize = 3;
        const CHUNK: usize = 128;
        let mut ind = Aroon::new(Identity::<fugazi::market::Candle>::new(), AROON_P);
        let mut cols: Vec<Vec<Real>> = (0..LINES).map(|_| vec![0.0; n]).collect();
        let mut flat = vec![0.0 as Real; CHUNK * LINES];
        let mut row = 0usize;
        for chunk in candles.chunks(CHUNK) {
            let flat = &mut flat[..chunk.len() * LINES];
            for (r, c) in chunk.iter().enumerate() {
                let dst = &mut flat[r * LINES..(r + 1) * LINES];
                match ind.update(*c) {
                    Some(v) => dst.copy_from_slice(&[v.up, v.down, v.oscillator]),
                    None => dst.fill(Real::NAN),
                }
            }
            for (j, col) in cols.iter_mut().enumerate() {
                let dst = &mut col[row..row + chunk.len()];
                for (r, cell) in dst.iter_mut().enumerate() {
                    *cell = flat[r * LINES + j];
                }
            }
            row += chunk.len();
        }
        black_box(cols.len());
    })));

    let med = |xs: &[f64]| xs[xs.len() / 2];
    if json {
        for (name, xs) in &out {
            let list: Vec<String> = xs.iter().map(|x| format!("{x:.4}")).collect();
            println!(
                "{{\"name\":\"{name}\",\"ns_per_sample\":{:.4},\"samples\":[{}]}}",
                med(xs),
                list.join(",")
            );
        }
    } else {
        println!("n = {n} samples, median of {REPS}\n");
        println!(
            "size_of::<PayloadValue>() = {} B  (the payload every erased `update` moves)\n",
            std::mem::size_of::<DynValue>()
        );
        println!("{:<20}{:>12}{:>12}{:>12}", "indicator", "min", "median", "max");
        for (name, xs) in &out {
            println!(
                "{name:<20}{:>12.2}{:>12.2}{:>12.2}",
                xs[0],
                med(xs),
                xs[xs.len() - 1]
            );
        }
    }
}
