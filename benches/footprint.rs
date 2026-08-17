//! Memory-footprint probe: allocations per bar, bytes per bar, and peak RSS.
//!
//! Run with `cargo bench --bench footprint`. Not a criterion target — it
//! installs a counting `#[global_allocator]`, which inside a criterion bench
//! would also tally criterion's own bookkeeping. Output is a plain table meant
//! to be pasted into `docs/PERFORMANCE.md`.
//!
//! Read the numbers as *per bar*: a single-asset run carries 40 bytes of actual
//! OHLCV per bar, so anything much above that in `bytes/bar` is container and
//! symbol overhead rather than data.

use fugazi::prelude::*;
use fugazi::spec::SingleStrategySpec;

mod common;
use common::{alloc_count, multi_snapshots, peak_rss_bytes, single_snapshots};

#[global_allocator]
static ALLOC: alloc_count::Counting = alloc_count::Counting;

const BARS: usize = 200_000;

fn row(label: &str, bars: usize, allocs: u64, bytes: u64) {
    println!(
        "{label:<34} {:>12} {:>12.2} {:>14.1}",
        allocs,
        allocs as f64 / bars as f64,
        bytes as f64 / bars as f64,
    );
}

fn header(title: &str) {
    println!("\n== {title}");
    println!(
        "{:<34} {:>12} {:>12} {:>14}",
        "case", "allocs", "allocs/bar", "bytes/bar"
    );
}

fn main() {
    header(&format!("snapshot construction ({BARS} bars)"));

    let (snaps, allocs, bytes) = alloc_count::measure(|| single_snapshots("X", BARS));
    row("single-asset Vec<Snapshot>", BARS, allocs, bytes);
    let held = std::mem::size_of_val(&snaps[0]);
    println!(
        "  ({} snapshots resident, Snapshot handle = {held} B, \
         entry = {} B, Atom = {} B, Candle = {} B)",
        snaps.len(),
        std::mem::size_of::<(Option<String>, Option<Frequency>, Atom)>(),
        std::mem::size_of::<Atom>(),
        std::mem::size_of::<Candle>(),
    );

    // What an interned symbol type would cost instead — see the breaking
    // candidates in docs/PERFORMANCE.md. `Snapshot` is already generic over
    // `Sym`, so this needs no change to the library, only to the caller.
    {
        use std::sync::Arc;
        let syms: Vec<Arc<str>> = (0..1).map(|i| Arc::from(format!("X{i}").as_str())).collect();
        let candles = common::synth_candles(BARS);
        let (v, allocs, bytes) = alloc_count::measure(|| {
            (0..BARS)
                .map(|b| {
                    fugazi::types::Snapshot::single(syms[0].clone(), Atom::new(candles[b]))
                })
                .collect::<Vec<fugazi::types::Snapshot<Arc<str>>>>()
        });
        row("single-asset, Sym = Arc<str>", BARS, allocs, bytes);
        drop(v);
    }

    for n in [8usize, 32] {
        let bars = BARS / 20;
        let (multi, allocs, bytes) = alloc_count::measure(|| multi_snapshots(n, bars));
        row(&format!("{n}-symbol Vec<Snapshot>"), bars, allocs, bytes);
        drop(multi);
    }

    header(&format!("driving a run ({BARS} bars, snapshots pre-built)"));

    let rust_run = || {
        let mut strat = fugazi::strategies::trend::ma_crossover("X".to_string(), 5, 20);
        let mut w: PaperWallet<String> = PaperWallet::new(10_000.0);
        let rep = fugazi::backtest::run(&mut strat, &mut w, snaps.iter().cloned());
        rep.equity_curve.len()
    };
    // Warm once so first-touch growth of the equity curve / blotter is not
    // charged to the measured pass.
    let _ = rust_run();
    let (_, allocs, bytes) = alloc_count::measure(rust_run);
    row("sma_crossover (Rust)", BARS, allocs, bytes);

    let spec = SingleStrategySpec::from_text_with_params_in(
        "symbol: X\n\
         long:\n  \
           enter: !crosses_above { lhs: !sma { source: close, period: 5 }, rhs: !sma { source: close, period: 20 } }\n  \
           exit: !crosses_below { lhs: !sma { source: close, period: 5 }, rhs: !sma { source: close, period: 20 } }\n",
        &Default::default(),
        std::path::Path::new("."),
        "(footprint)",
    )
    .expect("probe spec parses");
    let schema = fugazi::market::Schema::empty();
    let yaml_run = || {
        let mut strat = spec.try_build(10_000.0, &schema).expect("probe spec builds");
        let mut w: PaperWallet<String> = PaperWallet::new(10_000.0);
        let rep = fugazi::backtest::run(&mut strat, &mut w, snaps.iter().cloned());
        rep.equity_curve.len()
    };
    let _ = yaml_run();
    let (_, allocs, bytes) = alloc_count::measure(yaml_run);
    row("sma_crossover (YAML)", BARS, allocs, bytes);

    println!("\n== resident");
    match peak_rss_bytes() {
        Some(rss) => println!(
            "peak RSS = {:.1} MiB  ({:.1} B/bar over {BARS} bars; \
             OHLCV payload alone = {:.1} MiB)",
            rss as f64 / (1024.0 * 1024.0),
            rss as f64 / BARS as f64,
            (BARS * std::mem::size_of::<Candle>()) as f64 / (1024.0 * 1024.0),
        ),
        None => println!("peak RSS unavailable (no /proc/self/status)"),
    }
}
