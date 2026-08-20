//! Performance regression guards that are **exact**, not measured.
//!
//! `docs/PERFORMANCE.md` records what this crate costs. This file stops the
//! most valuable of those properties from silently reverting — but only the
//! ones that can be checked without timing anything, because a wall-clock
//! assertion on a shared CI runner alerts on contention, not on regressions.
//!
//! # What is guarded, and why these
//!
//! **Allocation *scaling*, not allocation count.** The invariant that matters
//! is that driving a run allocates a *constant* number of times regardless of
//! how many bars go through it — the per-bar path touches no allocator. Each
//! guard therefore runs the same workload at two bar counts and asserts the
//! difference is near zero. That is immune to harness noise (which adds a
//! constant to both) and to the allocator, the machine and the rustc version,
//! and it fails loudly: one `String` clone per bar turns a delta of ~0 into a
//! delta of tens of thousands.
//!
//! **Type widths.** The Phase 6 erasure work turned on `PayloadValue` being 88
//! bytes wide — as wide as its `Atom` variant — and on the replacement handle
//! being a bare fat pointer. Those are `size_of` facts, so they are checked as
//! facts.
//!
//! # What is deliberately *not* guarded here
//!
//! Anything whose unit is nanoseconds: per-erasure-level cost, the YAML-vs-Rust
//! ratio, indicator throughput. Those live in `benches/` and in
//! `scripts/perf-compare.sh icount`, and they are compared against a baseline
//! by a human who can tell a regression from a noisy runner. Adding them here
//! would buy flaky CI, not safety — see the measurement traps in
//! `docs/PERFORMANCE.md`.
//!
//! # One test on purpose
//!
//! The whole file is a single `#[test]`. A `#[global_allocator]` counts every
//! thread in the process, so a second test running concurrently would land in
//! the middle of a measurement. Cargo runs each `tests/*.rs` as its own
//! process, so one test here means nothing else is allocating while a
//! measurement is open.

use std::alloc::{GlobalAlloc, Layout, System};
use std::sync::atomic::{AtomicU64, Ordering::Relaxed};

use fugazi::market::{Candle, Schema};
use fugazi::spec::SingleStrategySpec;
use fugazi::types::{Atom, Snapshot, Symbol};
use fugazi::wallet::PaperWallet;

// ---------------------------------------------------------------------------
// Counting allocator
// ---------------------------------------------------------------------------

static ALLOCS: AtomicU64 = AtomicU64::new(0);

struct Counting;

unsafe impl GlobalAlloc for Counting {
    unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
        ALLOCS.fetch_add(1, Relaxed);
        unsafe { System.alloc(layout) }
    }
    unsafe fn dealloc(&self, ptr: *mut u8, layout: Layout) {
        unsafe { System.dealloc(ptr, layout) }
    }
    unsafe fn realloc(&self, ptr: *mut u8, layout: Layout, new_size: usize) -> *mut u8 {
        ALLOCS.fetch_add(1, Relaxed);
        unsafe { System.realloc(ptr, layout, new_size) }
    }
}

#[global_allocator]
static A: Counting = Counting;

/// Allocations performed while `f` ran.
fn allocs_of<T>(f: impl FnOnce() -> T) -> u64 {
    let before = ALLOCS.load(Relaxed);
    let out = f();
    let n = ALLOCS.load(Relaxed) - before;
    drop(out);
    n
}

// ---------------------------------------------------------------------------
// Workload
// ---------------------------------------------------------------------------

/// A deterministic price walk — the same LCG the benches use, so a number seen
/// here and a number seen there describe the same input.
fn snapshots(bars: usize) -> Vec<Snapshot<Symbol>> {
    let sym = fugazi::types::symbol("X");
    let mut px = 100.0_f64;
    let mut s = 0x5EED_1234_5678_9ABC_u64;
    (0..bars)
        .map(|_| {
            s = s
                .wrapping_mul(6364136223846793005)
                .wrapping_add(1442695040888963407);
            let noise = ((s >> 33) as f64 / u32::MAX as f64) - 0.5;
            let close = px * (1.0 + 0.0002 + 0.01 * noise);
            let (o, c) = (px, close);
            px = close;
            let candle = Candle::new(o, o.max(c) * 1.001, o.min(c) * 0.999, c, 1_000.0);
            Snapshot::single(sym.clone(), Atom::new(candle))
        })
        .collect()
}

const YAML: &str = "symbol: X\n\
     long:\n  \
       enter: !crosses_above { lhs: !sma { source: close, period: 5 }, rhs: !sma { source: close, period: 20 } }\n  \
       exit: !crosses_below { lhs: !sma { source: close, period: 5 }, rhs: !sma { source: close, period: 20 } }\n";

/// Bar counts the scaling guards compare. A 10× spread makes a per-bar
/// allocation impossible to miss while keeping the test well under a second.
const SMALL: usize = 5_000;
const LARGE: usize = 50_000;

/// How much the allocation count may grow across a 45 000-bar increase.
///
/// Not zero: `RunReport`'s equity curve and the fill blotter are `Vec`s that
/// grow geometrically, so a longer run legitimately reallocates a handful more
/// times — `log2(50_000/5_000)` ≈ 3.3 doublings per such vector.
///
/// Measured, at the time of writing: the hand-written strategy grows by **5**
/// (24 -> 29 allocations). So this budget carries ~13x headroom, while a single
/// per-bar allocation would land at 45 000 — roughly 700x over. There is no
/// plausible value in between, which is what makes the gate worth having.
const GROWTH_BUDGET: u64 = 64;

/// Assert `f`'s allocation count does not scale with the number of bars.
fn assert_flat_in_bars(what: &str, mut f: impl FnMut(usize) -> u64) {
    // Warm once at each size: first-touch growth of a lazily-built cache would
    // otherwise be charged to whichever run happened to go first.
    let _ = f(SMALL);
    let _ = f(LARGE);

    let small = allocs_of(|| f(SMALL));
    let large = allocs_of(|| f(LARGE));
    let growth = large.saturating_sub(small);

    assert!(
        growth <= GROWTH_BUDGET,
        "{what}: allocations grew by {growth} between a {SMALL}-bar and a \
         {LARGE}-bar run ({small} -> {large}), over a budget of {GROWTH_BUDGET}.\n\
         \n\
         The per-bar path is supposed to touch the allocator zero times. A \
         growth near {} means something now allocates once per bar — look for a \
         `clone()` on a `String`/`Vec`/`Symbol` in a `update`/`trade`/`on_fill` \
         path, or a scratch buffer rebuilt each bar instead of reused.\n\
         See docs/PERFORMANCE.md, Phase 3.",
        LARGE - SMALL,
    );
}

/// Prove the guard above can fail, before trusting it when it passes.
///
/// A regression test that has never been seen to fail is a comment. This feeds
/// [`assert_flat_in_bars`] a workload that allocates once per bar — exactly the
/// mistake the guards exist to catch — and requires it to panic. Without this,
/// a future edit that quietly neuters the measurement (an `allocs_of` that
/// returns 0, a budget raised "just to get CI green") would leave every guard
/// below passing vacuously.
fn guard_has_teeth() {
    let fired = std::panic::catch_unwind(|| {
        assert_flat_in_bars("deliberately allocating control", |bars| {
            let mut sink = 0u64;
            for i in 0..bars {
                // One heap allocation per bar, the shape of the regression:
                // a per-bar `String`/`Vec` clone that should not be there.
                // `black_box` so a release-mode build cannot elide the very
                // thing this control exists to perform.
                let v: Vec<u64> = std::hint::black_box(vec![i as u64]);
                sink = sink.wrapping_add(v[0]);
            }
            sink
        });
    });
    assert!(
        fired.is_err(),
        "the allocation guard did not fire on a workload that allocates once \
         per bar — it is no longer measuring anything, and every assertion \
         below it is vacuous",
    );
}

#[test]
fn performance_invariants_hold() {
    // The panic hook would otherwise print the control's (expected) failure and
    // make a passing run look broken.
    let hook = std::panic::take_hook();
    std::panic::set_hook(Box::new(|_| {}));
    guard_has_teeth();
    std::panic::set_hook(hook);

    // -- driving a run allocates a constant number of times -----------------

    let big = snapshots(LARGE);

    assert_flat_in_bars("hand-written Rust strategy", |bars| {
        let mut strat = fugazi::strategies::trend::ma_crossover(fugazi::types::symbol("X"), 5, 20);
        let mut w: PaperWallet<Symbol> = PaperWallet::new(10_000.0);
        let rep = fugazi::backtest::run(&mut strat, &mut w, big[..bars].iter().cloned());
        rep.equity_curve.len() as u64
    });

    // The YAML path is the one Phase 6 rebuilt, and the one an `optimize` sweep
    // runs thousands of times. It must be as allocation-free per bar as the
    // hand-written twin, not merely close to it.
    let spec = SingleStrategySpec::from_text_with_params_in(
        YAML,
        &Default::default(),
        std::path::Path::new("."),
        "(perf_guard)",
    )
    .expect("guard spec parses");
    let schema = Schema::empty();

    assert_flat_in_bars("spec-built (YAML) strategy", |bars| {
        let mut strat = spec.try_build(10_000.0, &schema).expect("guard spec builds");
        let mut w: PaperWallet<Symbol> = PaperWallet::new(10_000.0);
        let rep = fugazi::backtest::run(&mut strat, &mut w, big[..bars].iter().cloned());
        rep.equity_curve.len() as u64
    });

    // -- the type widths the erasure work turns on --------------------------

    // A `Chain` is a plain fat pointer: data + vtable, nothing carried
    // alongside. If this grows, someone has attached a payload to the handle
    // and the whole Phase 6 result is at risk.
    assert_eq!(
        size_of::<fugazi::runtime::RealChain>(),
        2 * size_of::<usize>(),
        "a Chain must stay a bare fat pointer — see docs/PERFORMANCE.md, Phase 6",
    );

    // The retiring payload enum is as wide as its widest variant. It is still
    // on the overlay-column path, so widening it is a real cost, not a
    // cosmetic one. 88 bytes is what the erasure benchmark priced.
    assert!(
        size_of::<fugazi::runtime::PayloadValue>() <= 88,
        "PayloadValue grew past 88 bytes ({} B) — every overlay column moves \
         one of these per bar. Box the new variant instead.",
        size_of::<fugazi::runtime::PayloadValue>(),
    );

    // A symbol wraps an `Arc<str>`, so cloning one is a refcount bump rather
    // than an allocation — the invariant behind the run-driving guards above.
    //
    // Three words, not two: Phase 13 added a cached hash of the name beside the
    // handle, so that comparing two symbols rejects on 8 bytes instead of
    // reaching `memcmp` (which measured at 47% of a 64-symbol backtest). The
    // *invariant* this guard exists for is unchanged and is the one asserted
    // below — a clone must still allocate nothing. The width is allowed to be
    // three words and no more.
    assert_eq!(
        size_of::<Symbol>(),
        3 * size_of::<usize>(),
        "Symbol must stay a thin-cloneable handle — see docs/PERFORMANCE.md, Phases 5 and 13",
    );

    // The property that actually matters, asserted directly rather than
    // inferred from the width. The width was only ever a proxy for it, and the
    // hash field is exactly the kind of change that breaks the proxy while
    // leaving the property intact.
    {
        let sym = fugazi::types::symbol("BTCUSDT");
        let mut sink: Vec<Symbol> = Vec::with_capacity(1_000);
        let allocated = allocs_of(|| {
            for _ in 0..1_000 {
                sink.push(sym.clone());
            }
        });
        assert_eq!(
            allocated, 0,
            "cloning a Symbol allocates — it must stay a refcount bump; \
             see docs/PERFORMANCE.md, Phase 5",
        );
    }
}
