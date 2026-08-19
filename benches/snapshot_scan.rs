//! Does splitting a `Snapshot`'s tags from its atoms make `find` cheaper?
//!
//! `Snapshot::find` runs once per `!pick`-rooted leaf per symbol per bar, and
//! reads only the `(symbol, freq)` tag. Interleaved, an entry is 112 bytes and
//! the tag is the first 24, so the scan touches 4.7x more cache than it looks
//! at. That arithmetic is right; this measures whether it *matters*.
//!
//! Single-workload mode (`-- <name>`) runs one workload once, for callgrind —
//! wall-clock put the change at +1.7% to +16.1%, inside the band where trap 6
//! in `docs/PERFORMANCE.md` says layout and cache luck are not separable from
//! work. `control` must read identical on both sides.

use std::hint::black_box;
use std::time::Instant;

use fugazi::prelude::*;
use fugazi::types::{Atom, Candle, Snapshot, Symbol, symbol as intern};

const REPS: usize = 9;

fn universe(n: usize) -> (Vec<Symbol>, Snapshot<Symbol>) {
    let syms: Vec<Symbol> = (0..n).map(|i| intern(format!("S{i:03}"))).collect();
    let mut snap: Snapshot<Symbol> = Snapshot::new();
    for (i, s) in syms.iter().enumerate() {
        let px = 100.0 + i as f64;
        snap.push(
            Some(s.clone()),
            None,
            Atom::new(Candle::new(px, px, px, px, 1_000.0)),
        );
    }
    (syms, snap)
}

/// One full bar's worth of lookups: every symbol's leaf finds its own entry,
/// which is the O(N^2) the change was aimed at.
fn scan_bar(syms: &[Symbol], snap: &Snapshot<Symbol>) {
    for s in syms {
        let sel = Selector::by_symbol(s.clone());
        black_box(snap.find(&sel));
    }
}

fn workload(name: &str) {
    const BARS: usize = 2_000;
    match name {
        "control" => {
            let mut acc = 0.0f64;
            for i in 0..BARS * 64 {
                acc += i as f64;
            }
            black_box(acc);
        }
        "find_2" | "find_16" | "find_64" => {
            let n: usize = name.rsplit('_').next().unwrap().parse().unwrap();
            let (syms, snap) = universe(n);
            for _ in 0..BARS {
                scan_bar(&syms, &snap);
            }
        }
        other => panic!("unknown workload `{other}`"),
    }
}

fn median(mut v: Vec<f64>) -> f64 {
    v.sort_by(|a, b| a.partial_cmp(b).unwrap());
    v[v.len() / 2]
}

fn main() {
    if let Some(name) = std::env::args().nth(1).filter(|a| !a.starts_with('-')) {
        workload(&name);
        return;
    }
    println!("ns per find(), 2000 bars x N lookups\n");
    println!("{:>6}{:>12}{:>14}", "N", "ns/find", "working set");
    for n in [2usize, 8, 16, 32, 64] {
        let (syms, snap) = universe(n);
        let lookups = 2_000 * n;
        let mut times = Vec::with_capacity(REPS);
        for _ in 0..REPS {
            let t = Instant::now();
            for _ in 0..2_000 {
                scan_bar(&syms, &snap);
            }
            times.push(t.elapsed().as_secs_f64());
        }
        let ns = median(times) * 1e9 / lookups as f64;
        // Interleaved storage: 112 bytes an entry. This is the number that
        // decides whether the cache-line argument can possibly apply.
        println!("{n:>6}{ns:>12.2}{:>12} B", n * 112);
    }
}
