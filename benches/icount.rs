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
//! Workloads: `sma_rust` · `sma_yaml` · `macd_rust` · `macd_yaml` · `tree8`
//! · `atr_none` · `atr_atom` · `atr_candle` · `atr_manual_max`
//! · `chain_candle` · `chain_atom`
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
        "(icount)",
    )
    .expect("icount spec parses")
}

const SMA_YAML: &str = r#"
symbol: X
long:
  enter: !crosses_above
    lhs: !sma { source: close, period: 5 }
    rhs: !sma { source: close, period: 20 }
  exit: !crosses_below
    lhs: !sma { source: close, period: 5 }
    rhs: !sma { source: close, period: 20 }
"#;

const MACD_YAML: &str = r#"
symbol: X
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
    format!("symbol: X\nlong:\n  enter: {expr}\n")
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
            Some(pc) => max2(max2(c.high - c.low, (c.high - pc).abs()), (c.low - pc).abs()),
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

fn main() {
    let workload = std::env::args().nth(1).unwrap_or_else(|| {
        eprintln!(
            "usage: icount <sma_rust|sma_yaml|macd_rust|macd_yaml|tree8\
             |atr_none|atr_atom|atr_candle|atr_manual_max|chain_candle|chain_atom>"
        );
        std::process::exit(2);
    });
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
            let mut s = fugazi::strategies::trend::macd_crossover(fugazi::types::symbol("X"), 12, 26, 9);
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
        // `atr_none` is the control: identical setup, no indicator. Subtract it
        // from the others and what remains is the ATR work itself, which is what
        // the native-C comparison needs.
        "atr_none" | "atr_atom" | "atr_candle" | "atr_manual_max" => {
            let candles = common::synth_candles(BARS);
            match workload.as_str() {
                // Exactly what `benches/three_tier.rs` times, `Atom` clone included.
                "atr_atom" => {
                    let atoms: Vec<fugazi::types::Atom> =
                        candles.iter().map(|c| fugazi::types::Atom::new(*c)).collect();
                    let mut ind = fugazi::indicators::Atr::new(
                        fugazi::indicators::CurrentBar::new(),
                        14,
                    );
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
        other => {
            eprintln!("unknown workload {other:?}");
            std::process::exit(2);
        }
    };
    println!("{workload}: {} bars", black_box(bars));
}
