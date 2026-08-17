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

fn main() {
    let workload = std::env::args().nth(1).unwrap_or_else(|| {
        eprintln!("usage: icount <sma_rust|sma_yaml|macd_rust|macd_yaml|tree8>");
        std::process::exit(2);
    });
    let snaps = single_snapshots("X", BARS);
    let schema = fugazi::market::Schema::empty();

    let bars = match workload.as_str() {
        "sma_rust" => {
            let mut s = fugazi::strategies::trend::ma_crossover(fugazi::types::symbol("X"), 5, 20);
            let mut w: PaperWallet<Symbol> = PaperWallet::new(10_000.0);
            fugazi::backtest::run(&mut s, &mut w, snaps.iter().cloned())
                .equity_curve
                .len()
        }
        "macd_rust" => {
            let mut s = fugazi::strategies::trend::macd_crossover(fugazi::types::symbol("X"), 12, 26, 9);
            let mut w: PaperWallet<Symbol> = PaperWallet::new(10_000.0);
            fugazi::backtest::run(&mut s, &mut w, snaps.iter().cloned())
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
            fugazi::backtest::run(&mut s, &mut w, snaps.iter().cloned())
                .equity_curve
                .len()
        }
        other => {
            eprintln!("unknown workload {other:?}");
            std::process::exit(2);
        }
    };
    println!("{workload}: {} bars", black_box(bars));
}
