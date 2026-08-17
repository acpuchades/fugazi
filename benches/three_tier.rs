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

    if json {
        for (name, ns) in &out {
            println!("{{\"name\":\"{name}\",\"ns_per_sample\":{ns:.4}}}");
        }
    } else {
        println!("n = {n} samples, median of {REPS}\n");
        println!("{:<10}{:>14}", "indicator", "ns/sample");
        for (name, ns) in &out {
            println!("{name:<10}{ns:>14.2}");
        }
    }
}
