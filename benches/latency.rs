//! **Per-event latency**, not throughput — the distribution of one `update`.
//!
//! Every other number in `docs/PERFORMANCE.md` is amortised throughput: total
//! time over 200 000 samples, divided. That is the right measure for a backtest,
//! which is a tight loop over a warm cache, and it is the *wrong* one for a live
//! stream, where a bar arrives, is handled, and nothing runs until the next one.
//! Between those events the i-cache, branch predictors and TLB go cold, and the
//! amortised figure says nothing about what the first update after the gap
//! costs. This target measures that, and reports p50/p90/p99/p99.9/max rather
//! than a mean, because the tail is the part a live system feels.
//!
//! Run with `cargo bench --bench latency`. Knobs:
//!
//!     FUGAZI_LATENCY_SAMPLES=20000   timed events per workload
//!     FUGAZI_LATENCY_GAP_US=1000     idle gap before each cold sample
//!
//! ## The timer is bigger than the thing being timed, and that governs the design
//!
//! `Instant::now()` costs ~20 ns a call on this platform — a bracket is two of
//! them — while an `Sma::update` is ~1.4 ns. Timing one update directly measures
//! the clock, not the indicator.
//!
//! So the **`timer` workload is not decoration: it is the instrument's own noise
//! floor**, an empty bracket measured exactly like every other row, and it is
//! printed first. Any row whose p50 sits near it is *unresolved* — the harness
//! says so in the `resolved?` column rather than letting a number that means
//! nothing be read as one. This mirrors the noise-floor discipline in
//! `tools/icount_python.py`, which reported a negative instruction count before
//! it had one.
//!
//! Two consequences worth stating, because they bound what this can conclude:
//!
//! * **The absolute latencies are upper bounds.** They include one bracket of
//!   timer overhead, which is not subtracted — subtracting a median from a
//!   distribution would understate the tail, and the tail is the point.
//! * **The warm/cold difference is the more trustworthy part, but it does not
//!   fully cancel.** The clock goes cold too — `Instant::now()` measured at 20 ns
//!   warm and **70 ns cold** here, p99.9 of 201 — so a cold row carries a more
//!   expensive bracket than its warm twin. The `timer` row is therefore repeated
//!   in the ratio table: it is the share of every other ratio that is the
//!   instrument rather than the code. Read the others *against* it, not alone.
//!
//! ## What "cold" means here
//!
//! `sleep(GAP)` before each timed event. That is a real context switch and a
//! real gap, which is what a process waiting on a socket between bars actually
//! does — not a synthetic cache flush. It leaves the caches cold the way idling
//! leaves them cold, including effects a manual eviction loop would miss
//! (scheduler migration, frequency scaling, TLB shootdown).

use std::hint::black_box;
use std::time::{Duration, Instant};

use fugazi::indicators::{Atr, Ema, Identity, Macd, Rsi, Sma};
use fugazi::market::Candle;
use fugazi::prelude::*;
use fugazi::runtime;

mod common;
use common::synth_candles;

/// Percentiles reported. p99.9 needs ~10 000 samples to mean anything, which
/// sets the default sample count.
const PCTS: [(f64, &str); 5] = [(0.50, "p50"), (0.90, "p90"), (0.99, "p99"), (0.999, "p99.9"), (1.0, "max")];

struct Row {
    name: &'static str,
    warm: Vec<f64>,
    cold: Vec<f64>,
}

fn pct(sorted: &[f64], p: f64) -> f64 {
    if sorted.is_empty() {
        return f64::NAN;
    }
    if p >= 1.0 {
        return sorted[sorted.len() - 1];
    }
    let h = (sorted.len() - 1) as f64 * p;
    let lo = h.floor() as usize;
    let hi = (lo + 1).min(sorted.len() - 1);
    sorted[lo] + (h - lo as f64) * (sorted[hi] - sorted[lo])
}

/// Time `step` once per sample, `n` times, sleeping `gap` before each when cold.
///
/// `step` takes the sample index so a workload can vary its input without the
/// loop hoisting anything out; it returns a value that is `black_box`ed, so the
/// call cannot be elided.
fn measure(n: usize, gap: Option<Duration>, mut step: impl FnMut(usize) -> f64) -> Vec<f64> {
    // Warm-up that is *not* recorded: the first few events of any workload pay
    // one-off costs (page faults on the sample vector, first-touch of the
    // indicator state) that are real but are not what a steady live stream sees.
    for i in 0..64 {
        black_box(step(i));
    }
    let mut out = Vec::with_capacity(n);
    for i in 0..n {
        if let Some(g) = gap {
            std::thread::sleep(g);
        }
        let t = Instant::now();
        black_box(step(i));
        out.push(t.elapsed().as_secs_f64() * 1e9);
    }
    out.sort_by(f64::total_cmp);
    out
}

fn main() {
    let n: usize = std::env::var("FUGAZI_LATENCY_SAMPLES")
        .ok()
        .and_then(|s| s.parse().ok())
        .unwrap_or(20_000);
    let gap_us: u64 = std::env::var("FUGAZI_LATENCY_GAP_US")
        .ok()
        .and_then(|s| s.parse().ok())
        .unwrap_or(1_000);
    let gap = Duration::from_micros(gap_us);

    let candles_owned = synth_candles(n + 128);
    let closes_owned: Vec<Real> = candles_owned.iter().map(|c| c.close).collect();
    // Borrowed as slices so each workload's closure captures a `Copy` reference
    // rather than moving the data — two closures per row need the same input.
    let candles: &[Candle] = &candles_owned;
    let closes: &[Real] = &closes_owned;

    let mut rows: Vec<Row> = Vec::new();
    macro_rules! row {
        ($name:literal, $mk:expr) => {{
            // `$mk` is evaluated twice on purpose: each run gets a freshly
            // constructed indicator, so the cold pass does not inherit state the
            // warm pass left hot.
            let warm = measure(n, None, $mk);
            let cold = measure(n, Some(gap), $mk);
            rows.push(Row { name: $name, warm, cold });
        }};
    }

    // The instrument measuring itself. Must be first: every other row is read
    // against it.
    row!("timer", |_i: usize| 0.0);

    row!("sma", {
        let mut ind = Sma::new(Identity::<Real>::new(), 14);
        move |i: usize| ind.update(closes[i]).unwrap_or(0.0)
    });
    row!("ema", {
        let mut ind = Ema::new(Identity::<Real>::new(), 14);
        move |i: usize| ind.update(closes[i]).unwrap_or(0.0)
    });
    row!("rsi", {
        let mut ind = Rsi::new(Identity::<Real>::new(), 14);
        move |i: usize| ind.update(closes[i]).unwrap_or(0.0)
    });
    row!("atr", {
        let mut ind = Atr::new(Identity::<Candle>::new(), 14);
        move |i: usize| ind.update(candles[i]).unwrap_or(0.0)
    });
    row!("macd", {
        let mut ind = Macd::new(Identity::<Real>::new(), 12, 26, 9);
        move |i: usize| ind.update(closes[i]).map_or(0.0, |v| v.macd)
    });
    // What a spec-built strategy actually runs per bar: the same SMA behind the
    // erasure the YAML and Python paths both produce. If erasure's cost changes
    // shape when cold, this is where it shows.
    row!("sma_erased", {
        let mut ind: runtime::Chain<Real, Real> = runtime::erase(Sma::new(Identity::<Real>::new(), 14));
        move |i: usize| ind.update(closes[i]).unwrap_or(0.0)
    });

    let floor = pct(&rows[0].warm, 0.50);
    println!(
        "n = {n} events per row, cold gap = {gap_us} us, times in ns (one \
         `Instant` bracket included)\n"
    );
    println!(
        "{:<12}{:>34}{:>34}{:>11}",
        "", "---------- warm ----------", "---------- cold ----------", ""
    );
    print!("{:<12}", "workload");
    for _ in 0..2 {
        for (_, label) in PCTS.iter().take(4) {
            print!("{label:>8}");
        }
        print!("{:>2}", "");
    }
    println!("{:>11}", "resolved?");

    for r in &rows {
        print!("{:<12}", r.name);
        for set in [&r.warm, &r.cold] {
            for (p, _) in PCTS.iter().take(4) {
                print!("{:>8.1}", pct(set, *p));
            }
            print!("{:>2}", "");
        }
        // A row whose warm p50 is not clear of the timer's own p50 is measuring
        // the clock. Say so rather than printing a number that reads like a
        // result.
        let resolved = r.name != "timer" && pct(&r.warm, 0.50) > floor * 1.5;
        println!("{:>11}", if r.name == "timer" { "(floor)" } else if resolved { "yes" } else { "NO" });
    }

    println!("\ntimer floor (warm p50) = {floor:.1} ns — rows at or under ~1.5x of it are\nnot resolvable by this instrument; read their warm/cold *ratio*, not the value.");
    // `max` is deliberately absent: it is one observation, and a warm run that
    // happened to be preempted once produces a warm max above the cold max and a
    // ratio below 1. Measured here at 0.0x and 308x on the same run — noise in
    // both directions, and reporting it would invite reading either as a result.
    println!("\ncold/warm per percentile — the number this target exists for.");
    println!("**`timer` is in this table on purpose**: the instrument goes cold too,\nso its row is how much of every other row is the clock rather than the code.");
    print!("{:<12}", "workload");
    for (_, label) in PCTS.iter().take(4) {
        print!("{label:>9}");
    }
    println!();
    for r in rows.iter() {
        print!("{:<12}", r.name);
        for (p, _) in PCTS.iter().take(4) {
            let (w, c) = (pct(&r.warm, *p), pct(&r.cold, *p));
            print!("{:>8.1}x", if w > 0.0 { c / w } else { f64::NAN });
        }
        println!();
    }
}
