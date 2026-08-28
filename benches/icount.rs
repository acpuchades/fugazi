//! Deterministic instruction-count probe — a fixed workload, run **exactly
//! once**, for `valgrind --tool=callgrind`.
//!
//! criterion cannot answer "did this change do more work?" on a shared or
//! thermally-variable machine: wall-clock conflates real work with cache and
//! code-layout effects, and two separately-linked binaries differ in layout
//! even when the source change is trivial. Instruction count is immune to
//! contention and to layout, so it separates "more work" from "unluckier
//! layout" — which is the question a suspicious ±10% wall-clock swing raises.
//!
//! It is *only* that separation. Instruction count ignores cache misses, branch
//! prediction and ILP, so a change can be a genuine win while raising it (and
//! vice versa). Read it alongside a quiet-machine criterion run, never instead.
//!
//! Usage:
//!   cargo bench --bench icount --no-run
//!   valgrind --tool=callgrind --callgrind-out-file=x.out \
//!       target/release/deps/icount-<hash> <workload>
//!
//! Workloads: `metrics_none` · `metrics_reduction` · `sma_rust` · `sma_yaml` · `macd_rust` · `macd_yaml` · `tree8`
//! · `atr_none` · `atr_atom` · `atr_candle` · `atr_manual_max`
//! · `chain_candle` · `chain_atom` · `sma_two_levels` · `sma_fused`
//! · `sma_dyn_per_sample` · `sma_dyn_batch`
//! · `sma_scalar_none` · `sma_scalar_direct` · `sma_scalar_erased`
//! · `sma_scalar_fused` · `sma_scalar_fused_batched` · `sma_scalar_boxed_local`
//! · `sma_scalar_boxed_producer` · `sma_scalar_chunked_local`
//!
//! **The outer erased level costs 21 instructions/sample, and it is not the
//! vtable — it is where the state lives.** `sma_scalar_boxed_local` differs from
//! `sma_scalar_fused_batched` by one line, `let mut local = self.0.clone()` plus
//! a write-back, and that line is worth the whole 21: behind a `Box` the
//! compiler cannot prove the indicator's state does not alias the output buffer,
//! so `sum`/`head`/`len` reload and store every sample; as a local they promote
//! to registers for the loop.
//!
//! Net of the control, every variant storing an `Option<Real>` per sample:
//!
//! | | net instr/sample |
//! |---|---:|
//! | `sma_scalar_direct` — local indicator, no erasure | 16.00 |
//! | `sma_scalar_boxed_local` — boxed, whole slice, state copied local | 16.04 |
//! | `sma_scalar_chunked_local` — as above but 128-sample stack chunks | **20.56** |
//! | `sma_scalar_fused` — boxed, per sample (what ships today) | 37.02 |
//! | `sma_scalar_boxed_producer` — boxed, `&mut dyn FnMut` in and out | 46.03 |
//!
//! So batching pays **only** with the local copy; the two earlier attempts
//! without it measured -1.2 and +1.3, which is why this looked like a dead end
//! twice. And it must be a *slice*: routing samples through `&mut dyn FnMut`
//! costs ~30 instructions/sample, worse than doing nothing.
//!
//! `chunked_local` is the shippable shape — a whole-frame slice would need the
//! 8 MB `Vec<Candle>` that streaming removed, so the candle path copies into a
//! small stack buffer instead. It gives up 4.5 of the 21 to do so.
//!
//! **`sma_scalar_*` is the one that says where the Python gap now lives.** Net
//! of the control, the same `Sma::new(Identity, 14)` costs **20.0
//! instructions/sample monomorphised** and **50.0 erased the way
//! `ta.sma(ta.identity(), 14)` builds it** — so type erasure costs ~30, half
//! again as much as the arithmetic it wraps, and more than the entire Python
//! boundary (17.0, measured separately by `tools/icount_python.py`).
//!
//! That reframes an earlier reading. A per-function profile of the Python path
//! attributes only 4 instructions/sample to the inner `Erased<Identity>::update`,
//! which looks like an erased level is nearly free. It is not: the call setup,
//! the argument move and the `Option<Real>` return handling are all charged to
//! the *caller*, so the marginal cost of a level is ~15, not 4. Vary one level
//! and subtract — do not read a level's cost off its own callee total.
//!
//! **Pick the binary by mtime, not by `ls`.** `cargo` leaves every previously
//! built `icount-<hash>` in place, the hash is not a timestamp, and this
//! machine's `ls -t` is `eza` (which ignores `--time-style`). Selecting the
//! wrong one has twice produced confident numbers for code that was never
//! compiled — once reporting a +37% regression in a workload the change could
//! not touch. `scripts/perf-compare.sh` has `pick_icount()` for this; by hand:
//!
//!   find target/release/deps -name 'icount-*' ! -name '*.d' \
//!       -printf '%T@ %p\n' | sort -rn | head -1
//!
//! The last four workloads price the two candidate fixes for the Python
//! bindings' commonest shape, `ta.sma(ta.close(), 14)`, which erases twice:
//!
//! * `sma_two_levels` vs `sma_fused` — monomorphising the field leaf into `Sma`
//!   instead of boxing it. **Worth 9.0 instructions/bar** (129.38 → 120.36).
//! * `sma_dyn_per_sample` vs `sma_dyn_batch` — handing the erased chain a
//!   *slice* of candles so the loop runs on its side of the boundary, one
//!   indirect call per 64 bars instead of per bar. **Worth −1.2 instructions/bar
//!   — it is slower**, and that result is why no such method exists in `src`.
//!   An indirect call with a predictable target is about two instructions; the
//!   cost of an erased level is its wrapper, its `Option` handling and the
//!   40-byte `Candle` move, none of which batching removes. The chunk
//!   bookkeeping then costs slightly more than the calls it saves.
//!
//! `chain_candle` vs `chain_atom` prices P1 of the Python plan: the bindings
//! carry a `Chain<Atom, _>` for candle-rooted sources, so every 40-byte `Candle`
//! is lifted into an 88-byte `Atom` per bar. Wall-clock put that at ~24
//! ns/sample, which is ~75 cycles and far more than the move should cost, so it
//! needs a contention-immune second opinion before anything is restructured.
//!
//! The three `atr_*` workloads exist to attribute a measured gap against native
//! TA-Lib, on a contended machine where wall-clock cannot. They are the same
//! computation with one variable changed each:
//!
//! * `atr_atom` — as `benches/three_tier.rs` drives it, cloning an `Atom` per
//!   bar. What the reported number actually measured.
//! * `atr_candle` — fed a bare `Candle`, so the `Atom` clone is gone. The
//!   difference from `atr_atom` is the benchmark's own overhead, not fugazi's.
//! * `atr_manual_max` — `atr_candle` with `f64::max` replaced by `if a > b`.
//!   `f64::max` is specified to return the non-NaN operand, which is more than
//!   one `maxsd`; TA-Lib's C uses a plain comparison. The difference is the
//!   price of that NaN contract.

use std::hint::black_box;

use fugazi::prelude::*;
use fugazi::spec::SingleStrategySpec;

mod common;
use common::single_snapshots;

/// Small enough that callgrind's ~50x slowdown stays tolerable, large enough
/// that per-bar costs dominate one-off construction.
const BARS: usize = 20_000;

fn spec_of(yaml: &str) -> SingleStrategySpec {
    SingleStrategySpec::from_text_with_params_in(
        yaml,
        &Default::default(),
        std::path::Path::new("."),
        std::path::Path::new("."),
        "(icount)",
    )
    .expect("icount spec parses")
}

const SMA_YAML: &str = r#"
root: X
long:
  enter: !crosses_above
    lhs: !sma { source: close, period: 5 }
    rhs: !sma { source: close, period: 20 }
  exit: !crosses_below
    lhs: !sma { source: close, period: 5 }
    rhs: !sma { source: close, period: 20 }
"#;

const MACD_YAML: &str = r#"
root: X
long:
  enter: !crosses_above
    lhs: !macd_line { fast: 12, slow: 26, signal: 9 }
    rhs: !macd_signal { fast: 12, slow: 26, signal: 9 }
  exit: !crosses_below
    lhs: !macd_line { fast: 12, slow: 26, signal: 9 }
    rhs: !macd_signal { fast: 12, slow: 26, signal: 9 }
"#;

/// The same left-spine `!and` tree `benches/tree.rs` uses, at depth 8.
fn spine_yaml(depth: usize) -> String {
    fn leaf(i: usize) -> String {
        let (fast, slow) = (3 + i, 20 + i * 3);
        format!(
            "!gt {{ lhs: !sma {{ source: close, period: {fast} }}, \
                    rhs: !sma {{ source: close, period: {slow} }} }}"
        )
    }
    let mut expr = leaf(0);
    for i in 1..depth {
        expr = format!("!and {{ lhs: {expr}, rhs: {} }}", leaf(i));
    }
    format!("root: X\nlong:\n  enter: {expr}\n")
}

/// The true-range + Wilder recurrence, written the way TA-Lib's C writes it:
/// a plain comparison rather than `f64::max`'s NaN-aware form.
///
/// Not a proposal — an attribution probe. If this is materially cheaper than
/// `Atr`, the NaN contract is where the gap lives.
fn atr_manual(candles: &[fugazi::market::Candle], period: usize) -> usize {
    #[inline(always)]
    fn max2(a: Real, b: Real) -> Real {
        if a > b { a } else { b }
    }
    let mut prev_close: Option<Real> = None;
    let mut sum = 0.0;
    let mut seeded = 0usize;
    let mut atr: Option<Real> = None;
    let inv = 1.0 / period as Real;
    let mut seen = 0usize;
    for c in candles {
        let tr = match prev_close {
            Some(pc) => max2(
                max2(c.high - c.low, (c.high - pc).abs()),
                (c.low - pc).abs(),
            ),
            None => c.high - c.low,
        };
        prev_close = Some(c.close);
        match atr {
            Some(prev) => atr = Some(prev + (tr - prev) * inv),
            None => {
                sum += tr;
                seeded += 1;
                if seeded == period {
                    atr = Some(sum * inv);
                }
            }
        }
        if atr.is_some() {
            seen += 1;
        }
        black_box(atr);
    }
    seen
}

// ---------------------------------------------------------------------------
// Batching the erasure boundary — a local prototype, deliberately not in `src`
// ---------------------------------------------------------------------------
//
// Today `feed()` costs one *indirect* call per sample: the frame loop lives in
// the Python crate and calls `Chain::update` per bar, so the chain's body can
// never be inlined into the loop that drives it, and a 40-byte `Candle` is moved
// through the boundary each time.
//
// The alternative is to put the loop on the other side of the boundary: hand the
// erased chain a *slice* of candles and let it fold them itself. One indirect
// call per chunk, and inside it the concrete `I::update` inlines into a tight
// monomorphised loop.
//
// This is modelled here first, with a throwaway trait, so the win is a number
// before it is an API. Chunked through a fixed stack buffer rather than one call
// per frame, so nothing has to materialise a `Vec<Candle>` — that allocation is
// exactly what the streaming `CandleColumns` removed.
trait BatchIndicator: Send + Sync {
    fn one(&mut self, c: fugazi::market::Candle) -> Option<Real>;
    fn many(&mut self, xs: &[fugazi::market::Candle], out: &mut [Option<Real>]);
}

struct Batched<I>(I);

impl<I> BatchIndicator for Batched<I>
where
    I: Indicator<Input = fugazi::market::Candle, Output = Real> + Send + Sync,
{
    fn one(&mut self, c: fugazi::market::Candle) -> Option<Real> {
        self.0.update(c)
    }
    // Monomorphised per `I`, so `I::update` inlines into this loop even though
    // the *caller* reaches it through a vtable.
    fn many(&mut self, xs: &[fugazi::market::Candle], out: &mut [Option<Real>]) {
        for (o, &x) in out.iter_mut().zip(xs) {
            *o = self.0.update(x);
        }
    }
}

/// The chain shape `ta.sma(ta.close(), 14)` actually builds: two erased levels.
#[inline(never)]
fn boxed_sma_two_levels() -> Box<dyn BatchIndicator> {
    let leaf: fugazi::runtime::Chain<fugazi::market::Candle, Real> =
        fugazi::runtime::erase(CloseOf::default());
    Box::new(Batched(fugazi::indicators::Sma::new(leaf, 14)))
}

/// A `CloseOf`-leaf chain reading one field, standing in for the bindings'
/// `BarField<BarClose>` (which lives in `python/src` and cannot be used here).
#[derive(Clone, Default)]
struct CloseOf {
    value: Option<Real>,
}

impl Indicator for CloseOf {
    type Input = fugazi::market::Candle;
    type Output = Real;
    fn update(&mut self, input: fugazi::market::Candle) -> Option<Real> {
        self.value = Some(input.close);
        self.value
    }
    fn value(&self) -> Option<Real> {
        self.value
    }
    fn warm_up_bars(&self) -> usize {
        1
    }
    fn reset(&mut self) {
        self.value = None;
    }
}

// `#[inline(never)]`, and that is load-bearing rather than tidiness.
//
// With the chain built inline in the match arm, LLVM sees the concrete type at
// the `erase` call, devirtualises the whole thing and inlines it — so the two
// variants compiled to *identical* code and the pair measured 464,955 vs
// 464,880 instructions, a 0.004/bar difference that is pure noise. That is the
// same trap `benches/erasure.rs` documents, and it flatters erasure by
// measuring a chain that is not erased at run time at all.
//
// Returning the erased type from a function that cannot be inlined is what
// forces the indirect call a real Python-built chain always pays.
#[inline(never)]
fn sma_of_close_two_levels() -> fugazi::runtime::Chain<fugazi::market::Candle, Real> {
    let leaf: fugazi::runtime::Chain<fugazi::market::Candle, Real> =
        fugazi::runtime::erase(CloseOf::default());
    fugazi::runtime::erase(fugazi::indicators::Sma::new(leaf, 14))
}

#[inline(never)]
fn sma_of_close_fused() -> fugazi::runtime::Chain<fugazi::market::Candle, Real> {
    fugazi::runtime::erase(fugazi::indicators::Sma::new(CloseOf::default(), 14))
}

/// The exact chain `ta.sma(ta.identity(), 14)` builds: two erased levels.
/// `#[inline(never)]` for the devirtualisation reason documented above.
#[inline(never)]
fn erased_scalar_sma() -> fugazi::runtime::Chain<Real, Real> {
    let leaf: fugazi::runtime::Chain<Real, Real> =
        fugazi::runtime::erase(fugazi::indicators::Identity::<Real>::new());
    fugazi::runtime::erase(fugazi::indicators::Sma::new(leaf, 14))
}

/// The scalar twin of `BatchIndicator`, for the fused+batched probe.
trait BatchScalar: Send + Sync {
    fn many(&mut self, xs: &[Real], out: &mut [Option<Real>]);
}

struct BatchedScalar<I>(I);

impl<I> BatchScalar for BatchedScalar<I>
where
    I: Indicator<Input = Real, Output = Real> + Send + Sync,
{
    fn many(&mut self, xs: &[Real], out: &mut [Option<Real>]) {
        for (o, &x) in out.iter_mut().zip(xs) {
            *o = self.0.update(x);
        }
    }
}

/// Streams through `&mut dyn FnMut` on both sides, so it needs no input slice —
/// see `sma_scalar_boxed_producer`.
trait ProducerScalar: Send + Sync {
    fn fold(
        &mut self,
        n: usize,
        next: &mut dyn FnMut() -> Real,
        sink: &mut dyn FnMut(Option<Real>),
    );
}

struct ProducedScalar<I>(I);

impl<I> ProducerScalar for ProducedScalar<I>
where
    I: Indicator<Input = Real, Output = Real> + Clone + Send + Sync,
{
    fn fold(
        &mut self,
        n: usize,
        next: &mut dyn FnMut() -> Real,
        sink: &mut dyn FnMut(Option<Real>),
    ) {
        let mut local = self.0.clone();
        for _ in 0..n {
            sink(local.update(next()));
        }
        self.0 = local;
    }
}

/// The same batch, but the indicator is copied to a local for the duration and
/// written back once — see `sma_scalar_boxed_local`.
struct BatchedScalarLocal<I>(I);

impl<I> BatchScalar for BatchedScalarLocal<I>
where
    I: Indicator<Input = Real, Output = Real> + Clone + Send + Sync,
{
    fn many(&mut self, xs: &[Real], out: &mut [Option<Real>]) {
        let mut local = self.0.clone();
        for (o, &x) in out.iter_mut().zip(xs) {
            *o = local.update(x);
        }
        self.0 = local;
    }
}

/// One erased level over a monomorphised `Sma<Identity<Real>>` — the artifact a
/// fused plain root would build.
#[inline(never)]
fn fused_scalar_sma() -> fugazi::runtime::Chain<Real, Real> {
    fugazi::runtime::erase(fugazi::indicators::Sma::new(
        fugazi::indicators::Identity::<Real>::new(),
        14,
    ))
}

/// Bars for `metrics_reduction`. Smaller than `BARS` elsewhere would be fine —
/// the reduction is `O(bars)` and the count is per-run, not per-sample — but
/// 200 000 matches `benches/metrics.rs`, so a wall-clock reading and an
/// instruction count describe the same workload.
const REDUCTION_BARS: usize = 200_000;

/// The same synthetic report `benches/metrics.rs` reduces: an equity curve off
/// the shared price walk, plus an alternating fill every 50 bars.
fn metrics_report(bars: usize) -> fugazi::backtest::RunReport<Symbol> {
    use fugazi::backtest::{Fill, RunReport};
    use fugazi::wallet::{Order, OrderId, OrderKind};

    let candles = common::synth_candles(bars);
    RunReport {
        equity_curve: candles.iter().map(|c| c.close * 100.0).collect(),
        fills: (0..bars)
            .step_by(50)
            .enumerate()
            .map(|(i, bar)| Fill {
                bar,
                order: Order {
                    id: OrderId(i as u64),
                    symbol: fugazi::types::symbol("X"),
                    side: if i % 2 == 0 { Side::Buy } else { Side::Sell },
                    units: 1.0,
                    price: candles[bar].close,
                    kind: OrderKind::Market,
                    commission: 0.0,
                    requested_units: 1.0,
                },
            })
            .collect(),
        rejections: Vec::new(),
        initial_equity: candles[0].close * 100.0,
        ruin_bar: None,
        carry_coverage: None,
    }
}

fn main() {
    // No workload named. `cargo test --all-targets` runs every `harness = false`
    // bench target argless, and this one is a manual valgrind probe with nothing
    // to run in that mode — so print the usage and exit **clean**. It used to
    // exit 2, which made `cargo test --all-targets` red on a spotless tree: CI
    // runs plain `cargo test -p fugazi` and never saw it, so the failure only
    // ever hit whoever reached for the broader spelling. An *unknown* workload
    // is still a real mistake and still exits 2, below.
    let Some(workload) = std::env::args().nth(1) else {
        println!(
            "usage: icount <sma_rust|sma_yaml|macd_rust|macd_yaml|tree8\
             |atr_none|atr_atom|atr_candle|atr_manual_max|chain_candle|chain_atom|chain_atom_direct\
             |sma_two_levels|sma_fused|sma_dyn_per_sample|sma_dyn_batch\
             |sma_scalar_none|sma_scalar_direct|sma_scalar_erased|sma_scalar_fused|sma_scalar_fused_batched|sma_scalar_boxed_local|sma_scalar_boxed_producer|sma_scalar_chunked_local|stddev_scan\
             |multi_none|aroon_candle|adx_candle|dmi_candle|metrics_none|metrics_reduction>"
        );
        return;
    };
    let schema = fugazi::market::Schema::empty();

    // Built lazily: `single_snapshots` allocates 20 000 snapshots, which at this
    // bar count costs more instructions than some workloads *are*. Constructing
    // it unconditionally hid the `atr_*` numbers under ~850 instructions/bar of
    // unrelated setup.
    let snaps = || single_snapshots("X", BARS);

    let bars = match workload.as_str() {
        "sma_rust" => {
            let mut s = fugazi::strategies::trend::ma_crossover(fugazi::types::symbol("X"), 5, 20);
            let mut w: PaperWallet<Symbol> = PaperWallet::new(10_000.0);
            fugazi::backtest::run(&mut s, &mut w, snaps().iter().cloned())
                .equity_curve
                .len()
        }
        "macd_rust" => {
            let mut s =
                fugazi::strategies::trend::macd_crossover(fugazi::types::symbol("X"), 12, 26, 9);
            let mut w: PaperWallet<Symbol> = PaperWallet::new(10_000.0);
            fugazi::backtest::run(&mut s, &mut w, snaps().iter().cloned())
                .equity_curve
                .len()
        }
        "sma_yaml" | "macd_yaml" | "tree8" => {
            let doc = match workload.as_str() {
                "sma_yaml" => SMA_YAML.to_string(),
                "macd_yaml" => MACD_YAML.to_string(),
                _ => spine_yaml(8),
            };
            let mut s = spec_of(&doc)
                .try_build(10_000.0, &schema)
                .expect("icount spec builds");
            let mut w: PaperWallet<Symbol> = PaperWallet::new(10_000.0);
            fugazi::backtest::run(&mut s, &mut w, snaps().iter().cloned())
                .equity_curve
                .len()
        }
        // Same ATR, same one erased level, differing only in the boundary type:
        // `Chain<Candle, Real>` against `Chain<Atom, Real>` plus the per-bar
        // lift. Mirrors `_bench_frame_stage` stages 3 and 5.
        "chain_candle" | "chain_atom" => {
            let candles = common::synth_candles(BARS);
            if workload == "chain_candle" {
                let mut ind: fugazi::runtime::Chain<fugazi::market::Candle, Real> =
                    fugazi::runtime::erase(fugazi::indicators::Atr::new(
                        fugazi::indicators::Identity::<fugazi::market::Candle>::new(),
                        14,
                    ));
                for c in &candles {
                    black_box(ind.update(*c));
                }
            } else {
                let mut ind: fugazi::runtime::Chain<fugazi::types::Atom, Real> =
                    fugazi::runtime::erase(fugazi::indicators::Atr::new(
                        fugazi::indicators::CurrentBar::new(),
                        14,
                    ));
                for c in &candles {
                    black_box(ind.update((*c).into()));
                }
            }
            BARS
        }
        // Is the Atom cost the *boundary*, or `Identity<Atom>`'s store-and-clone
        // inside it? This roots ATR on a leaf that reads `atom.candle` and keeps
        // only the 40-byte Candle — same Atom input, no Atom retained.
        "chain_atom_direct" => {
            #[derive(Clone, Default)]
            struct BarOf {
                value: Option<fugazi::market::Candle>,
            }
            impl Indicator for BarOf {
                type Input = fugazi::types::Atom;
                type Output = fugazi::market::Candle;
                fn update(&mut self, input: fugazi::types::Atom) -> Option<Self::Output> {
                    self.value = input.candle;
                    self.value
                }
                fn value(&self) -> Option<Self::Output> {
                    self.value
                }
                fn warm_up_bars(&self) -> usize {
                    1
                }
                fn reset(&mut self) {
                    self.value = None;
                }
            }
            let candles = common::synth_candles(BARS);
            let mut ind: fugazi::runtime::Chain<fugazi::types::Atom, Real> =
                fugazi::runtime::erase(fugazi::indicators::Atr::new(BarOf::default(), 14));
            for c in &candles {
                black_box(ind.update((*c).into()));
            }
            BARS
        }
        // Is an erased `Sma` expensive because of the boundary, or is `Sma`
        // itself expensive? The Python profile puts `Erased<Sma>::update` at 41
        // instructions/sample while a whole erased level of `Identity` costs 4 —
        // so the boundary is cheap and the body looks dear. But the Rust tier
        // runs the same `Sma` at 1.37 ns/sample, which 41 instructions could not
        // do. One of those two readings has to give.
        //
        // `sma_scalar_none` is the control (same loop, no indicator); the other
        // two are the identical computation monomorphised and erased.
        "sma_scalar_none"
        | "sma_scalar_direct"
        | "sma_scalar_erased"
        | "sma_scalar_fused"
        | "sma_scalar_fused_batched"
        | "sma_scalar_boxed_local"
        | "sma_scalar_boxed_producer"
        | "sma_scalar_chunked_local"
        | "stddev_scan" => {
            let xs: Vec<Real> = (0..BARS).map(|i| 100.0 + (i % 97) as Real * 0.5).collect();
            match workload.as_str() {
                "sma_scalar_none" => {
                    let mut out = vec![None; BARS];
                    for (o, &x) in out.iter_mut().zip(&xs) {
                        *o = Some(black_box(x));
                    }
                    black_box(&out);
                }
                "sma_scalar_direct" => {
                    let mut ind = fugazi::indicators::Sma::new(
                        fugazi::indicators::Identity::<Real>::new(),
                        14,
                    );
                    let mut out = vec![None; BARS];
                    for (o, &x) in out.iter_mut().zip(&xs) {
                        *o = ind.update(x);
                    }
                    black_box(&out);
                }
                "sma_scalar_erased" => {
                    let mut ind = erased_scalar_sma();
                    let mut out = vec![None; BARS];
                    for (o, &x) in out.iter_mut().zip(&xs) {
                        *o = ind.update(x);
                    }
                    black_box(&out);
                }
                // Fusing *and* batching together. Batching alone was measured
                // slower and fusing alone saves 8; the question is whether they
                // are complementary — with a concrete chain inside the box and
                // the loop on its side of the boundary, nothing opaque is left
                // on the per-sample path and the whole chain can inline.
                "sma_scalar_fused_batched" => {
                    let mut ind: Box<dyn BatchScalar> =
                        Box::new(BatchedScalar(fugazi::indicators::Sma::new(
                            fugazi::indicators::Identity::<Real>::new(),
                            14,
                        )));
                    let mut out = vec![None; BARS];
                    ind.many(&xs, &mut out);
                    black_box(&out);
                }
                // Why does a boxed-but-concrete chain (35.76) still cost twice
                // a local one (16.00)? Hypothesis: it is not the call, it is
                // where the *state* lives. Behind a `Box`, the compiler cannot
                // prove `self.0` does not alias `out`, so `sum`/`head`/`len` are
                // reloaded and stored every sample; as a local they promote to
                // registers for the whole loop.
                //
                // Same boxed trait object, same whole-slice batch, one change:
                // copy the indicator into a local, run, write it back once.
                "sma_scalar_boxed_local" => {
                    let mut ind: Box<dyn BatchScalar> =
                        Box::new(BatchedScalarLocal(fugazi::indicators::Sma::new(
                            fugazi::indicators::Identity::<Real>::new(),
                            14,
                        )));
                    let mut out = vec![None; BARS];
                    ind.many(&xs, &mut out);
                    black_box(&out);
                }
                // The local-copy trick needs a whole-frame slice, which the
                // candle path cannot supply without rebuilding the 8 MB
                // `Vec<Candle>` that streaming removed. So: same trick, but the
                // samples arrive through a `&mut dyn FnMut` producer and leave
                // through a `&mut dyn FnMut` sink, which works for any input
                // shape. Two indirect calls per sample (~2 instructions each)
                // against the 21 the local copy saves.
                "sma_scalar_boxed_producer" => {
                    let mut ind: Box<dyn ProducerScalar> =
                        Box::new(ProducedScalar(fugazi::indicators::Sma::new(
                            fugazi::indicators::Identity::<Real>::new(),
                            14,
                        )));
                    let mut out = vec![None; BARS];
                    let mut i = 0usize;
                    let mut j = 0usize;
                    {
                        let xs = &xs;
                        let out = &mut out;
                        let mut next = || {
                            let v = xs[i];
                            i += 1;
                            v
                        };
                        let mut sink = |v: Option<Real>| {
                            out[j] = v;
                            j += 1;
                        };
                        ind.fold(BARS, &mut next, &mut sink);
                    }
                    black_box(&out);
                }
                // The shape that would actually ship. A whole-frame slice is not
                // available for candles without rebuilding the 8 MB `Vec<Candle>`
                // streaming removed, so: copy into a small *stack* buffer, hand
                // that slice over, repeat. The state round-trips once per chunk
                // instead of once per sample, so the register promotion survives;
                // the question is whether the buffer shuffling eats the win.
                "sma_scalar_chunked_local" => {
                    const CHUNK: usize = 128;
                    let mut ind: Box<dyn BatchScalar> =
                        Box::new(BatchedScalarLocal(fugazi::indicators::Sma::new(
                            fugazi::indicators::Identity::<Real>::new(),
                            14,
                        )));
                    let mut out = vec![None; BARS];
                    let mut buf = [0.0f64; CHUNK];
                    let mut got = [None; CHUNK];
                    for (ci, chunk) in xs.chunks(CHUNK).enumerate() {
                        buf[..chunk.len()].copy_from_slice(chunk);
                        ind.many(&buf[..chunk.len()], &mut got[..chunk.len()]);
                        out[ci * CHUNK..ci * CHUNK + chunk.len()]
                            .copy_from_slice(&got[..chunk.len()]);
                    }
                    black_box(&out);
                }
                // Isolates `WindowStats::variance`'s O(period) centred pass —
                // the deliberate accuracy-for-speed trade, and the largest
                // remaining engine gap against TA-Lib C. Shares the control and
                // the input with the `sma_scalar_*` family so the numbers are
                // directly comparable.
                "stddev_scan" => {
                    let mut ind = fugazi::indicators::StdDev::new(
                        fugazi::indicators::Identity::<Real>::new(),
                        20,
                    );
                    let mut out = vec![None; BARS];
                    for (o, &x) in out.iter_mut().zip(&xs) {
                        *o = ind.update(x);
                    }
                    black_box(&out);
                }
                // Exactly what fusing a plain root would produce: the leaf
                // monomorphised into `Sma`, the result erased once. Neither
                // `_direct` (no erasure at all) nor `_erased` (two levels) is
                // this shape, so the saving has to be measured, not subtracted.
                _ => {
                    let mut ind = fused_scalar_sma();
                    let mut out = vec![None; BARS];
                    for (o, &x) in out.iter_mut().zip(&xs) {
                        *o = ind.update(x);
                    }
                    black_box(&out);
                }
            }
            BARS
        }
        // Prices fusing the Python bindings' commonest shape. `ta.sma(ta.close(),
        // 14)` erases **twice** — `Sma` over a `Chain<Candle, Real>` holding the
        // field leaf — so every sample crosses a vtable boundary it does not need
        // to, carrying a 40-byte `Candle` by value each way.
        //
        // `sma_fused` is the same computation with the leaf monomorphised into
        // `Sma`, which is what an `AnySource::Field` carrier variant would let the
        // bindings build. The pair is here rather than in the Python harness
        // because it answers "is fusing worth the plumbing?" in one `cargo bench`
        // instead of a wheel rebuild.
        "sma_two_levels" | "sma_fused" => {
            let candles = common::synth_candles(BARS);
            let mut ind = if workload == "sma_fused" {
                sma_of_close_fused()
            } else {
                sma_of_close_two_levels()
            };
            for c in &candles {
                black_box(ind.update(black_box(*c)));
            }
            BARS
        }
        // The same two-level chain driven per-sample vs in chunks, so the delta
        // is the cost of crossing the erasure boundary 20 000 times instead of
        // 20 000/64 times. Both go through `Box<dyn BatchIndicator>`, so the
        // boundary itself is identical — only the granularity differs.
        "sma_dyn_per_sample" | "sma_dyn_batch" => {
            const CHUNK: usize = 64;
            let candles = common::synth_candles(BARS);
            let mut ind = boxed_sma_two_levels();
            let mut out = [None; CHUNK];
            if workload == "sma_dyn_batch" {
                for chunk in candles.chunks(CHUNK) {
                    ind.many(chunk, &mut out[..chunk.len()]);
                    black_box(&out);
                }
            } else {
                for c in &candles {
                    black_box(ind.one(black_box(*c)));
                }
            }
            BARS
        }
        // `atr_none` is the control: identical setup, no indicator. Subtract it
        // from the others and what remains is the ATR work itself, which is what
        // the native-C comparison needs.
        "atr_none" | "atr_atom" | "atr_candle" | "atr_manual_max" => {
            let candles = common::synth_candles(BARS);
            match workload.as_str() {
                // Exactly what `benches/three_tier.rs` times, `Atom` clone included.
                "atr_atom" => {
                    let atoms: Vec<fugazi::types::Atom> = candles
                        .iter()
                        .map(|c| fugazi::types::Atom::new(*c))
                        .collect();
                    let mut ind =
                        fugazi::indicators::Atr::new(fugazi::indicators::CurrentBar::new(), 14);
                    for a in &atoms {
                        black_box(ind.update(a.clone()));
                    }
                    BARS
                }
                // The same chain with the clone removed: `Identity<Candle>` feeds
                // the candle straight through, so only fugazi's own work remains.
                "atr_candle" => {
                    let mut ind = fugazi::indicators::Atr::new(
                        fugazi::indicators::Identity::<fugazi::market::Candle>::new(),
                        14,
                    );
                    for c in &candles {
                        black_box(ind.update(*c));
                    }
                    BARS
                }
                "atr_manual_max" => atr_manual(&candles, 14),
                _ => {
                    black_box(&candles);
                    BARS
                }
            }
        }
        // Bare multi-output engines, so the Python boundary cost can be split
        // from the indicator's own. `tools/icount_python.py` puts `aroon` at
        // 369.74 instructions/sample through the bindings and `adx` at 277.05 —
        // 93 apart, on the same output-column count and near-identical
        // wall-clock (8.92 vs 8.59 ns/sample). Either `Aroon`'s engine is that
        // much heavier and its wall-clock is hidden by ILP, or something in the
        // boundary treats it differently. These four answer that: subtract
        // `multi_none` from the rest.
        //
        // All three live in one binary on purpose. Absolute numbers from a
        // multi-workload file are not comparable to another file's (see
        // `benches/multi_feed.rs` and trap 11 in docs/PERFORMANCE.md), but a
        // difference *within* one is, and a difference is the whole question.
        // The post-run reduction, which `optimize` pays once per grid row per
        // fold. Deterministic counting is not a luxury here: after two rounds
        // of deduplication the remaining candidates are worth ~250 µs on a
        // ~2.8 ms reduction, and this machine's criterion run-to-run spread on
        // *untouched* code is ±8-17% — larger than the effect. Wall-clock
        // cannot see these; instruction count can.
        //
        // Subtract `metrics_none` from `metrics_reduction`: building the report
        // is 200 000 candles, an equity curve and 4 000 fills, and this file has
        // been bitten before by setup swamping the workload it wraps (see the
        // `atr_*` note above).
        "metrics_none" | "metrics_reduction" => {
            let rep = metrics_report(REDUCTION_BARS);
            if workload == "metrics_reduction" {
                black_box(fugazi::spec::metrics::from_report(&rep, 365.0, 0.045, None));
            } else {
                black_box(&rep);
            }
            REDUCTION_BARS
        }
        "multi_none" | "aroon_candle" | "adx_candle" | "dmi_candle" => {
            let candles = common::synth_candles(BARS);
            match workload.as_str() {
                "aroon_candle" => {
                    let mut ind = fugazi::indicators::Aroon::new(
                        fugazi::indicators::Identity::<fugazi::market::Candle>::new(),
                        14,
                    );
                    for c in &candles {
                        black_box(ind.update(*c));
                    }
                }
                "adx_candle" => {
                    let mut ind = fugazi::indicators::Adx::new(
                        fugazi::indicators::Identity::<fugazi::market::Candle>::new(),
                        14,
                    );
                    for c in &candles {
                        black_box(ind.update(*c));
                    }
                }
                "dmi_candle" => {
                    let mut ind = fugazi::indicators::Dmi::new(
                        fugazi::indicators::Identity::<fugazi::market::Candle>::new(),
                        14,
                    );
                    for c in &candles {
                        black_box(ind.update(*c));
                    }
                }
                _ => {
                    black_box(&candles);
                }
            }
            BARS
        }
        other => {
            eprintln!("unknown workload {other:?}");
            std::process::exit(2);
        }
    };
    println!("{workload}: {} bars", black_box(bars));
}
