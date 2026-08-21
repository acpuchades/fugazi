//! What the Python bindings' multi-output `feed` costs on the **Rust** side.
//!
//! `PyMulti::feed` folds a chunk of bars at a time into a flat row-major buffer
//! and then scatters that buffer into one full-length output column per line
//! (`feed_into_columns` in `python/src/carriers.rs`). Measured through Python,
//! a fixed ~25 ns/sample appears the moment an indicator emits a second output
//! column, and it does not grow much with the third. This target answers the
//! only question that splits the blame: **how much of that is our Rust?**
//!
//! Two workloads, one binary, so the delta is the answer:
//!
//!   `raw`         `Aroon::update` per bar, output discarded — the control.
//!   `feed_shape`  the same indicator driven the way `feed_into_columns` drives
//!                 it: chunked fold into `flat`, then the column scatter into
//!                 three 200 000-element buffers.
//!
//! ## Why this is not in `benches/three_tier.rs`
//!
//! It was, for one commit, and it **cost that file's `aroon` row 68%** — 9.50 →
//! 15.94 ns/sample, reproducibly, with every other row unmoved. Nothing ran
//! differently: the probe sits *after* `aroon` in the run order and cannot
//! perturb it at run time. It is a *compile*-time effect. Both workloads
//! construct `Aroon<Identity<Candle>>`, and a second call site of the same
//! monomorphisation changes what LLVM chooses to inline, so the measured row
//! got slower because a diagnostic was added beside it.
//!
//! That is a trap worth stating plainly, because it inverts the usual advice
//! about controls: **a benchmark file's workloads are not independent.** Adding
//! one can move the others, and it moves them most when it shares a type with
//! them — exactly when a diagnostic is most useful. Hence a separate target: a
//! bench target is its own crate, so nothing here can reach `three_tier`.
//!
//! Run with `cargo bench --bench multi_feed`.

use std::hint::black_box;
use std::time::Instant;

use fugazi::indicators::{Aroon, Identity};
use fugazi::market::Candle;
use fugazi::prelude::*;

mod common;
use common::synth_candles;

/// Matches `tools/bench_three_tier.py` and `benches/three_tier.rs`.
const AROON_P: usize = 14;
/// Matches `FOLD_CHUNK` in `python/src/carriers.rs`.
const CHUNK: usize = 128;
/// `Aroon` emits {up, down, oscillator}.
const LINES: usize = 3;

const REPS: usize = 7;
const WARMUP: usize = 2;

fn bench(n: usize, mut f: impl FnMut()) -> (f64, f64) {
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
    (times[0], times[times.len() / 2])
}

fn main() {
    let n: usize = std::env::var("FUGAZI_MULTI_FEED_N")
        .ok()
        .and_then(|s| s.parse().ok())
        .unwrap_or(200_000);
    let candles = synth_candles(n);

    // Control: the indicator alone, nothing written anywhere.
    let raw = bench(n, || {
        let mut ind = Aroon::new(Identity::<Candle>::new(), AROON_P);
        for c in &candles {
            black_box(ind.update(*c));
        }
    });

    // The `feed_into_columns` shape, into plain `Vec<Real>` rather than NumPy
    // buffers — deliberately, since NumPy is the other half of the split and
    // including it here would answer both questions at once and neither
    // cleanly. The output columns are full length and freshly allocated each
    // iteration, so the first-touch page faults a real `feed` pays are paid
    // here too.
    let feed_shape = bench(n, || {
        let mut ind = Aroon::new(Identity::<Candle>::new(), AROON_P);
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
    });

    // The same fold and scatter, but into buffers allocated **once** outside the
    // timed loop. A real `feed` cannot do this — it returns fresh NumPy arrays —
    // so this is not a candidate implementation. It is the split: whatever
    // `feed_shape` costs above this is the price of touching 4.8 MB of
    // never-before-written memory, which is paid as page faults *during* the
    // scatter and therefore cannot be moved elsewhere, only made smaller.
    let mut cols_kept: Vec<Vec<Real>> = (0..LINES).map(|_| vec![0.0; n]).collect();
    let feed_shape_reused = bench(n, || {
        let mut ind = Aroon::new(Identity::<Candle>::new(), AROON_P);
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
            for (j, col) in cols_kept.iter_mut().enumerate() {
                let dst = &mut col[row..row + chunk.len()];
                for (r, cell) in dst.iter_mut().enumerate() {
                    *cell = flat[r * LINES + j];
                }
            }
            row += chunk.len();
        }
    });

    // Two output columns instead of three, fresh each iteration. `TA_AROON`
    // emits two lines; fugazi's `Aroon` emits three, the third being
    // `oscillator = up - down`. If the gap between this and `feed_shape` is
    // real, that convenience column is costing a third of the output budget for
    // a subtraction the caller could do.
    let feed_shape_2 = bench(n, || {
        let mut ind = Aroon::new(Identity::<Candle>::new(), AROON_P);
        let mut cols: Vec<Vec<Real>> = (0..2).map(|_| vec![0.0; n]).collect();
        let mut flat = vec![0.0 as Real; CHUNK * 2];
        let mut row = 0usize;
        for chunk in candles.chunks(CHUNK) {
            let flat = &mut flat[..chunk.len() * 2];
            for (r, c) in chunk.iter().enumerate() {
                let dst = &mut flat[r * 2..(r + 1) * 2];
                match ind.update(*c) {
                    Some(v) => dst.copy_from_slice(&[v.up, v.down]),
                    None => dst.fill(Real::NAN),
                }
            }
            for (j, col) in cols.iter_mut().enumerate() {
                let dst = &mut col[row..row + chunk.len()];
                for (r, cell) in dst.iter_mut().enumerate() {
                    *cell = flat[r * 2 + j];
                }
            }
            row += chunk.len();
        }
        black_box(cols.len());
    });

    println!("n = {n} samples, min and median of {REPS}\n");
    println!("{:<16}{:>10}{:>10}", "workload", "min", "median");
    println!("{:<16}{:>10.2}{:>10.2}", "raw", raw.0, raw.1);
    println!(
        "{:<16}{:>10.2}{:>10.2}",
        "feed_shape", feed_shape.0, feed_shape.1
    );
    println!(
        "{:<16}{:>10.2}{:>10.2}",
        "  reused bufs", feed_shape_reused.0, feed_shape_reused.1
    );
    println!(
        "{:<16}{:>10.2}{:>10.2}",
        "  2 lines", feed_shape_2.0, feed_shape_2.1
    );
    black_box(cols_kept.len());
    println!(
        "\nfold + {LINES}-column scatter, fresh buffers:  {:+.2} ns/sample over raw",
        feed_shape.0 - raw.0
    );
    println!(
        "        ... with the buffers reused:        {:+.2} ns/sample over raw",
        feed_shape_reused.0 - raw.0
    );
    println!(
        "  so first-touching 3 x {} KB costs:        {:+.2} ns/sample",
        n * 8 / 1024,
        feed_shape.0 - feed_shape_reused.0
    );
    println!(
        "  and the third output column costs:       {:+.2} ns/sample",
        feed_shape.0 - feed_shape_2.0
    );
    println!(
        "Measured through Python the same step reads ~25 ns/sample; whatever this\n\
         number is, the rest is pyo3/NumPy. See docs/PERFORMANCE.md."
    );
}
