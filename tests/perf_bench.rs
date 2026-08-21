//! Perf probes — run with:
//!   cargo test --release --test perf_bench -- --ignored --nocapture
//!
//! Not a formal benchmark suite; just Instant::now() around the code path the
//! audit called out, to decide whether the fix is worth pursuing.

use std::time::Instant;

use fugazi::backtest::run;
use fugazi::indicators::Macd;
use fugazi::prelude::*;
use fugazi::strategies::trend::macd_crossover;
use fugazi::types::{Symbol, symbol as intern};

const BARS: usize = 200_000;
const REPS: usize = 3;

/// Deterministic geometric random-walk candles — cheap to build, warms up all
/// classical trend indicators, and gives us a stable input across runs.
fn synth_candles(n: usize) -> Vec<Candle> {
    let mut out = Vec::with_capacity(n);
    let mut px = 100.0_f64;
    // Small LCG so the walk is deterministic without pulling in `rand`.
    let mut s: u64 = 0x5eed_1234_5678_9abc;
    for _ in 0..n {
        s = s
            .wrapping_mul(6364136223846793005)
            .wrapping_add(1442695040888963407);
        let noise = ((s >> 33) as f64 / u32::MAX as f64) - 0.5; // ~[-0.5, 0.5]
        let ret = 0.0002 + 0.01 * noise;
        let open = px;
        let close = px * (1.0 + ret);
        let high = open.max(close) * 1.001;
        let low = open.min(close) * 0.999;
        out.push(Candle {
            open,
            high,
            low,
            close,
            volume: 1_000.0,
        });
        px = close;
    }
    out
}

// ---------------------------------------------------------------------------
// Custom-built single-Macd strategy: the theoretical minimum work for the
// `line-crosses-signal` decision — one Macd instance, tracks the sign of
// (line - signal) itself, no cloning, no Component adapters.
// ---------------------------------------------------------------------------

struct MacdCrossoverManual<Sym> {
    symbol: Sym,
    macd: Macd<fugazi::indicators::Close<fugazi::indicators::Pick<Sym>>>,
    /// Sign of the previous (line - signal), None until first warm bar.
    prev_sign: Option<i8>,
    /// The crossover event to trade on the next bar's `trade` call.
    event: Option<Side>,
}

impl<Sym: Clone + PartialEq + std::hash::Hash + Eq + 'static> MacdCrossoverManual<Sym> {
    fn new(symbol: Sym, fast: usize, slow: usize, signal: usize) -> Self {
        use fugazi::indicators::{Close, Pick};
        Self {
            symbol,
            macd: Macd::new(Close::of(Pick::<Sym>::new()), fast, slow, signal),
            prev_sign: None,
            event: None,
        }
    }
}

impl<Sym: Clone + PartialEq + std::hash::Hash + Eq + 'static> Strategy
    for MacdCrossoverManual<Sym>
{
    type Input = fugazi::types::Snapshot<Sym>;
    type Symbol = Sym;
    fn update(&mut self, snap: fugazi::types::Snapshot<Sym>) {
        let v = self.macd.update(snap);
        self.event = None;
        if let Some(mv) = v {
            let diff = mv.macd - mv.signal;
            let sign: i8 = if diff > 0.0 {
                1
            } else if diff < 0.0 {
                -1
            } else {
                0
            };
            if let Some(prev) = self.prev_sign {
                if prev < 0 && sign > 0 {
                    self.event = Some(Side::Buy);
                } else if prev > 0 && sign < 0 {
                    self.event = Some(Side::Sell);
                }
            }
            if sign != 0 {
                self.prev_sign = Some(sign);
            }
        }
    }
    fn trade(&self, wallet: &mut dyn Wallet<Sym>) {
        if let Some(side) = self.event {
            let _ = wallet.set(self.symbol.clone(), side, Size::value_frac(1.0));
        }
    }
    fn reset(&mut self) {
        self.macd.reset();
        self.prev_sign = None;
        self.event = None;
    }
}

fn median(mut xs: Vec<f64>) -> f64 {
    xs.sort_by(|a, b| a.partial_cmp(b).unwrap());
    xs[xs.len() / 2]
}

#[test]
#[ignore]
fn bench_macd_crossover_components() {
    let candles = synth_candles(BARS);
    eprintln!("bars={} reps={}", BARS, REPS);

    let mut baseline = vec![];
    for _ in 0..REPS {
        let mut strat = macd_crossover(intern("X"), 12, 26, 9);
        let mut w: PaperWallet<Symbol> = PaperWallet::new(10_000.0);
        let t = Instant::now();
        let rep = run(
            &mut strat,
            &mut w,
            candles
                .iter()
                .map(|c| fugazi::types::Snapshot::single(intern("X"), (*c).into())),
        );
        let el = t.elapsed().as_secs_f64();
        baseline.push(el);
        let _ = std::hint::black_box(rep.equity_curve.len());
    }

    let mut manual = vec![];
    for _ in 0..REPS {
        let mut strat = MacdCrossoverManual::new(intern("X"), 12, 26, 9);
        let mut w: PaperWallet<Symbol> = PaperWallet::new(10_000.0);
        let t = Instant::now();
        let rep = run(
            &mut strat,
            &mut w,
            candles
                .iter()
                .map(|c| fugazi::types::Snapshot::single(intern("X"), (*c).into())),
        );
        let el = t.elapsed().as_secs_f64();
        manual.push(el);
        let _ = std::hint::black_box(rep.equity_curve.len());
    }

    let bm = median(baseline.clone());
    let mm = median(manual.clone());
    eprintln!(
        "macd_crossover (library)  median = {:.3} s  ({:.1} ns/bar)",
        bm,
        bm * 1e9 / BARS as f64
    );
    eprintln!(
        "MacdCrossoverManual        median = {:.3} s  ({:.1} ns/bar)",
        mm,
        mm * 1e9 / BARS as f64
    );
    eprintln!("multiplier = {:.2}×", bm / mm);
}

// ---------------------------------------------------------------------------
// Snapshot clone cost as the universe grows.
//
// `Snapshot` is fed to every signal slot of every symbol each bar, so if a
// clone is a deep copy the per-bar cost grows with the *square* of the
// universe: N symbols × (slots × N-entry Vec copy). This probe drives the same
// strategy over 2, 8, 32 and 64 symbols and prints ns/bar — a deep-copying
// Snapshot shows super-linear growth, a refcounted one stays linear in N (the
// per-symbol chains still have to run).
// ---------------------------------------------------------------------------

fn multi_snapshots(n_symbols: usize, bars: usize) -> Vec<fugazi::types::Snapshot<Symbol>> {
    let candles = synth_candles(bars);
    let syms: Vec<Symbol> = (0..n_symbols)
        .map(|i| fugazi::types::symbol(format!("S{i:03}")))
        .collect();
    (0..bars)
        .map(|b| {
            let mut snap = fugazi::types::Snapshot::new();
            for (i, s) in syms.iter().enumerate() {
                // Vary each symbol's series a little so the chains do real work.
                let c = candles[(b + i * 7) % bars];
                snap.push(Some(s.clone()), None, fugazi::types::Atom::new(c));
            }
            snap
        })
        .collect()
}

#[test]
#[ignore]
fn bench_snapshot_clone_scaling() {
    use fugazi::strategies::MultiAssetStrategy;

    const N_BARS: usize = 4_000;
    eprintln!("bars={N_BARS} reps={REPS}");
    eprintln!("{:>8}  {:>12}  {:>14}", "symbols", "median s", "ns/bar");

    for &n in &[2usize, 8, 16, 32, 64] {
        let snaps = multi_snapshots(n, N_BARS);
        let mut times = vec![];
        for _ in 0..REPS {
            // One SMA-crossover decision per symbol: four signal slots, each
            // fed a clone of the whole snapshot every bar.
            let mut strat = MultiAssetStrategy::<Symbol>::with_initial_equity(10_000.0).long_on(
                |sym: &Symbol| {
                    use fugazi::indicators::{Close, Pick, Sma};
                    let close = || {
                        Close::of(Pick::matching(fugazi::types::Selector::by_symbol(
                            sym.clone(),
                        )))
                    };
                    Sma::new(close(), 5).crosses_above(Sma::new(close(), 20))
                },
                |sym: &Symbol| {
                    use fugazi::indicators::{Close, Pick, Sma};
                    let close = || {
                        Close::of(Pick::matching(fugazi::types::Selector::by_symbol(
                            sym.clone(),
                        )))
                    };
                    Sma::new(close(), 5).crosses_below(Sma::new(close(), 20))
                },
            );
            let mut w: PaperWallet<Symbol> = PaperWallet::new(10_000.0);
            let t = Instant::now();
            let rep = run(&mut strat, &mut w, snaps.iter().cloned());
            times.push(t.elapsed().as_secs_f64());
            let _ = std::hint::black_box(rep.equity_curve.len());
        }
        let m = median(times);
        eprintln!("{:>8}  {:>12.4}  {:>14.1}", n, m, m * 1e9 / N_BARS as f64);
    }
}

/// What does the YAML path cost over the equivalent Rust strategy?
///
/// Both express the same MACD crossover. The Rust catalogue's
/// `macd_crossover` composes it through `.shared()`, so one `Macd` drives both
/// components; the spec builder has no equivalent, so `!macd_line` and
/// `!macd_signal` build two independent `Macd`s.
///
/// **The duplicate is not where the ~1.9× goes.** Teaching `NodeSpec::build` to
/// memoise multi-output sub-trees into a `Shared` handle was tried and
/// measured: the cache hits, one `Macd` genuinely drives both components, and
/// the total moves ~3% — inside the noise. Four `SharedComponent`s taking a
/// mutex per bar costs about what the duplicated arithmetic did. Don't
/// re-attempt the memo without re-measuring.
///
/// **Correction (v0.59.0): those mutexes were not in `update`.** They were in
/// `SharedComponent::warm_up_bars` / `unstable_bars`, which `Strategy::is_ready`
/// called — through the whole tree — on *every* bar, for two values that are
/// fixed at construction. Caching them on the component made the Rust
/// `macd_crossover` side of this bench **~38% faster** on its own; the YAML side,
/// which builds two independent `Macd`s and holds no `Shared`, did not move.
/// So the shared-handle machinery was never the cost — the readiness walk was.
/// See `docs/PERFORMANCE.md` (F1, F2) and `benches/tree.rs`, which isolates
/// `is_ready` from `update`.
#[test]
#[ignore]
fn bench_yaml_vs_rust_macd_crossover() {
    use fugazi::spec::SingleStrategySpec;
    let candles = synth_candles(BARS);
    let snaps: Vec<fugazi::types::Snapshot<Symbol>> = candles
        .iter()
        .map(|c| fugazi::types::Snapshot::single(fugazi::types::symbol("X"), (*c).into()))
        .collect();

    let yaml = r#"
        symbol: X
        long:
          enter: !crosses_above
            lhs: !macd_line { fast: 12, slow: 26, signal: 9 }
            rhs: !macd_signal { fast: 12, slow: 26, signal: 9 }
          exit: !crosses_below
            lhs: !macd_line { fast: 12, slow: 26, signal: 9 }
            rhs: !macd_signal { fast: 12, slow: 26, signal: 9 }
    "#;
    let spec: SingleStrategySpec = SingleStrategySpec::from_text_with_params_in(
        yaml,
        &Default::default(),
        std::path::Path::new("."),
        "(bench)",
    )
    .unwrap();
    let schema = fugazi::market::Schema::empty();

    let mut yaml_times = vec![];
    for _ in 0..REPS {
        let mut strat = spec.build(10_000.0, &schema);
        let mut w: PaperWallet<Symbol> = PaperWallet::new(10_000.0);
        let t = Instant::now();
        let rep = run(&mut strat, &mut w, snaps.iter().cloned());
        yaml_times.push(t.elapsed().as_secs_f64());
        let _ = std::hint::black_box(rep.equity_curve.len());
    }

    let mut rust_times = vec![];
    for _ in 0..REPS {
        let mut strat =
            fugazi::strategies::trend::macd_crossover(fugazi::types::symbol("X"), 12, 26, 9);
        let mut w: PaperWallet<Symbol> = PaperWallet::new(10_000.0);
        let t = Instant::now();
        let rep = run(&mut strat, &mut w, snaps.iter().cloned());
        rust_times.push(t.elapsed().as_secs_f64());
        let _ = std::hint::black_box(rep.equity_curve.len());
    }

    let (y, r) = (median(yaml_times), median(rust_times));
    eprintln!(
        "YAML  !macd_line/!macd_signal : {:.1} ns/bar",
        y * 1e9 / BARS as f64
    );
    eprintln!(
        "Rust  macd_crossover .shared(): {:.1} ns/bar",
        r * 1e9 / BARS as f64
    );
    eprintln!("YAML / Rust = {:.2}×", y / r);
}
