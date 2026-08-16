//! Shared fixtures for the benchmark targets.
//!
//! Each `benches/*.rs` is its own crate, so this is `mod common;`-included
//! rather than imported. It carries the deterministic input series every bench
//! runs against, plus the two footprint instruments (a counting allocator and a
//! peak-RSS reader) that `benches/footprint.rs` drives.
//!
//! The price walk is bit-identical to the one in `tests/perf_bench.rs` so the
//! criterion numbers stay comparable with that file's historical readings.

#![allow(dead_code)] // each bench target uses a different subset

use fugazi::types::{Atom, Candle, Snapshot};

/// Deterministic geometric random walk — the same LCG and coefficients as
/// `tests/perf_bench.rs::synth_candles`, so a bench here and a probe there are
/// measuring the same input.
pub fn synth_candles(n: usize) -> Vec<Candle> {
    let mut out = Vec::with_capacity(n);
    let mut px = 100.0_f64;
    let mut s: u64 = 0x5eed_1234_5678_9abc;
    for _ in 0..n {
        s = s
            .wrapping_mul(6364136223846793005)
            .wrapping_add(1442695040888963407);
        let noise = ((s >> 33) as f64 / u32::MAX as f64) - 0.5;
        let ret = 0.0002 + 0.01 * noise;
        let open = px;
        let close = px * (1.0 + ret);
        out.push(Candle {
            open,
            high: open.max(close) * 1.001,
            low: open.min(close) * 0.999,
            close,
            volume: 1_000.0,
        });
        px = close;
    }
    out
}

/// One tagged single-asset snapshot per bar — what `fugazi run` feeds the
/// driver (`src/cli/run.rs`).
pub fn single_snapshots(symbol: &str, bars: usize) -> Vec<Snapshot<String>> {
    synth_candles(bars)
        .into_iter()
        .map(|c| Snapshot::single(symbol.to_string(), Atom::new(c)))
        .collect()
}

/// An `n_symbols`-wide snapshot per bar, each symbol on a phase-shifted slice of
/// the same walk so every per-symbol chain does real work. Mirrors
/// `tests/perf_bench.rs::multi_snapshots`.
pub fn multi_snapshots(n_symbols: usize, bars: usize) -> Vec<Snapshot<String>> {
    let candles = synth_candles(bars);
    let syms: Vec<String> = (0..n_symbols).map(|i| format!("S{i:03}")).collect();
    (0..bars)
        .map(|b| {
            let mut snap = Snapshot::new();
            for (i, s) in syms.iter().enumerate() {
                snap.push(
                    Some(s.clone()),
                    None,
                    Atom::new(candles[(b + i * 7) % bars]),
                );
            }
            snap
        })
        .collect()
}

// ---------------------------------------------------------------------------
// Footprint instruments
// ---------------------------------------------------------------------------

pub mod alloc_count {
    //! A pass-through `GlobalAlloc` that tallies allocation count and bytes.
    //!
    //! Installed only by `benches/footprint.rs`. Counters are `Relaxed` atomics:
    //! the measured runs are single-threaded, and exact cross-thread ordering
    //! would cost more than the number is worth.

    use std::alloc::{GlobalAlloc, Layout, System};
    use std::sync::atomic::{AtomicU64, Ordering::Relaxed};

    pub static ALLOCS: AtomicU64 = AtomicU64::new(0);
    pub static BYTES: AtomicU64 = AtomicU64::new(0);

    pub struct Counting;

    unsafe impl GlobalAlloc for Counting {
        unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
            ALLOCS.fetch_add(1, Relaxed);
            BYTES.fetch_add(layout.size() as u64, Relaxed);
            unsafe { System.alloc(layout) }
        }
        unsafe fn dealloc(&self, ptr: *mut u8, layout: Layout) {
            unsafe { System.dealloc(ptr, layout) }
        }
        unsafe fn realloc(&self, ptr: *mut u8, layout: Layout, new_size: usize) -> *mut u8 {
            ALLOCS.fetch_add(1, Relaxed);
            BYTES.fetch_add(new_size.saturating_sub(layout.size()) as u64, Relaxed);
            unsafe { System.realloc(ptr, layout, new_size) }
        }
    }

    /// Allocation count and total bytes requested while `f` ran.
    pub fn measure<T>(f: impl FnOnce() -> T) -> (T, u64, u64) {
        let a0 = ALLOCS.load(Relaxed);
        let b0 = BYTES.load(Relaxed);
        let out = f();
        (
            out,
            ALLOCS.load(Relaxed) - a0,
            BYTES.load(Relaxed) - b0,
        )
    }
}

/// Peak resident set size in bytes, read from `/proc/self/status`'s `VmHWM`.
/// `None` on a platform without procfs. High-water mark, so it never falls —
/// read it once at the end of the run you care about.
pub fn peak_rss_bytes() -> Option<u64> {
    let status = std::fs::read_to_string("/proc/self/status").ok()?;
    let line = status.lines().find(|l| l.starts_with("VmHWM:"))?;
    let kb: u64 = line.split_whitespace().nth(1)?.parse().ok()?;
    Some(kb * 1024)
}
