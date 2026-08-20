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
use fugazi::time::Frequency;
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

/// A universe whose symbols differ in *length*, so a `str` comparison rejects on
/// the length check and never reaches `memcmp`. Everything else is identical to
/// [`universe`].
fn universe_ragged(n: usize) -> (Vec<Symbol>, Snapshot<Symbol>) {
    let syms: Vec<Symbol> = (0..n).map(|i| intern("S".repeat(i + 1))).collect();
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

fn workload(name: &str) {
    const BARS: usize = 2_000;
    const N: usize = 64;
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
        // ---- decomposition ------------------------------------------------
        //
        // Every row below drives the SAME loop shape — iterate a pre-built
        // `Vec` of 64 distinct selectors — and differs only in how far each
        // scan runs and what the per-entry comparison has to do. Sharing the
        // shape is what makes them comparable: an earlier cut reused one
        // selector for the miss rows, which lets LLVM CSE a `readonly` call
        // across the inner loop and made them look 4.5x cheaper per entry than
        // the hit row for no reason but the optimiser.
        "hit_prebuilt" | "miss_prebuilt" | "miss_ragged" | "miss_freq" => {
            let (syms, snap, sels): (_, _, Vec<Selector<Symbol>>) = match name {
                // Each selector hits at its own index, so the scan runs
                // 1, 2, .. N entries — average (N+1)/2 = 32.5.
                "hit_prebuilt" => {
                    let (syms, snap) = universe(N);
                    let sels = syms
                        .iter()
                        .map(|s| Selector::by_symbol(s.clone()))
                        .collect();
                    (syms, snap, sels)
                }
                // Nobody matches, so every scan runs the full N and the
                // per-entry cost is not averaged over a short prefix.
                "miss_prebuilt" => {
                    let (syms, snap) = universe(N);
                    let sels = (0..N)
                        .map(|i| Selector::by_symbol(intern(format!("X{i:03}"))))
                        .collect();
                    (syms, snap, sels)
                }
                // Same, but every stored symbol differs in *length* from the
                // query, so `str`'s length check rejects before `memcmp`. The
                // gap against `miss_prebuilt` is the memcmp.
                "miss_ragged" => {
                    let (syms, snap) = universe_ragged(N);
                    let sels = (0..N)
                        .map(|i| Selector::by_symbol(intern("!".repeat(i + 1))))
                        .collect();
                    (syms, snap, sels)
                }
                // Freq-only queries: `matches` short-circuits the symbol arm on
                // `is_none()`, so this walks the same N entries doing no string
                // work at all — the floor for a scan of this shape.
                _ => {
                    let (syms, snap) = universe(N);
                    let sels = (0..N)
                        .map(|i| Selector::by_freq(Frequency::Minute(i as u32 + 1)))
                        .collect();
                    (syms, snap, sels)
                }
            };
            black_box(&syms);
            for _ in 0..BARS {
                for sel in &sels {
                    black_box(snap.find(sel));
                }
            }
        }
        // ---- what the comparison costs end to end -------------------------
        //
        // The same `MultiAssetStrategy` drive as `benches/multi_asset.rs` at
        // N = 64, over two universes that differ *only* in whether the symbols
        // share a length. Equal-length symbols reach `memcmp` on every rejected
        // entry; ragged ones are rejected by the length check first. Neither
        // needs a library change, so the gap is a clean ceiling on what making
        // the symbol comparison cheap can be worth to a whole run.
        // Prices the *scan* specifically. Identical to `drive_equal` in every
        // way — same 64 symbols, same 4 `Pick` leaves per symbol-bar, same SMA
        // arithmetic — except that every leaf reads the symbol at index 0, so
        // each `find` stops after one entry instead of scanning 32.5 on
        // average. The gap is what a `symbol -> index` side table could remove.
        "drive_index0" => {
            use fugazi::strategies::MultiAssetStrategy;
            let names: Vec<String> = (0..N).map(|i| format!("S{i:03}")).collect();
            let snaps = drive_snapshots(&names, 300);
            let first = intern(&names[0]);
            let close = {
                let first = first.clone();
                move || {
                    fugazi::indicators::Close::of(fugazi::indicators::Pick::matching(
                        Selector::by_symbol(first.clone()),
                    ))
                }
            };
            let mut strat = MultiAssetStrategy::<Symbol>::with_initial_equity(10_000.0)
                .long_on(
                    {
                        let close = close.clone();
                        move |_: &Symbol| {
                            fugazi::indicators::Sma::new(close(), 5)
                                .crosses_above(fugazi::indicators::Sma::new(close(), 20))
                        }
                    },
                    move |_: &Symbol| {
                        fugazi::indicators::Sma::new(close(), 5)
                            .crosses_below(fugazi::indicators::Sma::new(close(), 20))
                    },
                );
            let mut w: PaperWallet<Symbol> = PaperWallet::new(10_000.0);
            black_box(fugazi::backtest::run(&mut strat, &mut w, snaps));
        }
        // A `BasketStrategy` over the same 64 symbols. It keys nine maps by
        // symbol and touches at least three of them per symbol per bar (the
        // discovery `contains_key`, plus `latest_score` / `latest_size`), so it
        // is the shape most exposed to the hasher those maps use.
        "drive_basket" => {
            use fugazi::indicators::sizing::equal_weight;
            use fugazi::strategies::BasketStrategy;
            let names: Vec<String> = (0..N).map(|i| format!("S{i:03}")).collect();
            let snaps = drive_snapshots(&names, 300);
            let mut strat: BasketStrategy<Symbol> = BasketStrategy::with_initial_equity(100_000.0)
                .scored_by(|sym: &Symbol| {
                    fugazi::indicators::Sma::new(
                        fugazi::indicators::Close::of(fugazi::indicators::Pick::matching(
                            Selector::by_symbol(sym.clone()),
                        )),
                        10,
                    )
                })
                .sized_by(|_: &Symbol| equal_weight::<Symbol>(8))
                .top_bottom(4, 4);
            let mut w: PaperWallet<Symbol> = PaperWallet::new(100_000.0);
            black_box(fugazi::backtest::run(&mut strat, &mut w, snaps));
        }
        "drive_equal" | "drive_ragged" => {
            use fugazi::strategies::MultiAssetStrategy;
            let names: Vec<String> = if name == "drive_equal" {
                (0..N).map(|i| format!("S{i:03}")).collect()
            } else {
                (0..N).map(|i| "S".repeat(i + 1)).collect()
            };
            let snaps = drive_snapshots(&names, 300);
            let close = |sym: &Symbol| {
                fugazi::indicators::Close::of(fugazi::indicators::Pick::matching(
                    Selector::by_symbol(sym.clone()),
                ))
            };
            let mut strat = MultiAssetStrategy::<Symbol>::with_initial_equity(10_000.0)
                .long_on(
                    move |sym: &Symbol| {
                        fugazi::indicators::Sma::new(close(sym), 5)
                            .crosses_above(fugazi::indicators::Sma::new(close(sym), 20))
                    },
                    move |sym: &Symbol| {
                        fugazi::indicators::Sma::new(close(sym), 5)
                            .crosses_below(fugazi::indicators::Sma::new(close(sym), 20))
                    },
                );
            let mut w: PaperWallet<Symbol> = PaperWallet::new(10_000.0);
            black_box(fugazi::backtest::run(&mut strat, &mut w, snaps));
        }
        other => panic!("unknown workload `{other}`"),
    }
}

/// One snapshot per bar carrying every named symbol, phase-shifted so each
/// chain does real work. Mirrors `benches/common::multi_snapshots`, but takes
/// the names so the symbol *shape* can be varied.
fn drive_snapshots(names: &[String], bars: usize) -> Vec<Snapshot<Symbol>> {
    let syms: Vec<Symbol> = names.iter().map(intern).collect();
    (0..bars)
        .map(|b| {
            let mut snap = Snapshot::new();
            for (i, s) in syms.iter().enumerate() {
                let px = 100.0 + ((b + i * 7) % 23) as f64;
                snap.push(
                    Some(s.clone()),
                    None,
                    Atom::new(Candle::new(px, px * 1.001, px * 0.999, px, 1_000.0)),
                );
            }
            snap
        })
        .collect()
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
