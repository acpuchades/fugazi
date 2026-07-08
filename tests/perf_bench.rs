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
        s = s.wrapping_mul(6364136223846793005).wrapping_add(1442695040888963407);
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

impl<Sym: Clone + PartialEq + 'static> MacdCrossoverManual<Sym> {
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

impl<Sym: Clone + PartialEq + 'static> Strategy for MacdCrossoverManual<Sym> {
    type Input = fugazi::types::Snapshot<Sym>;
    type Symbol = Sym;
    fn update(&mut self, snap: fugazi::types::Snapshot<Sym>) {
        let v = self.macd.update(snap);
        self.event = None;
        if let Some(mv) = v {
            let diff = mv.macd - mv.signal;
            let sign: i8 = if diff > 0.0 { 1 } else if diff < 0.0 { -1 } else { 0 };
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
        let mut strat = macd_crossover("X", 12, 26, 9);
        let mut w: PaperWallet<&'static str> = PaperWallet::new(10_000.0);
        let t = Instant::now();
        let rep = run(
            &mut strat,
            &mut w,
            candles
                .iter()
                .map(|c| fugazi::types::Snapshot::single("X", (*c).into())),
        );
        let el = t.elapsed().as_secs_f64();
        baseline.push(el);
        let _ = std::hint::black_box(rep.equity_curve.len());
    }

    let mut manual = vec![];
    for _ in 0..REPS {
        let mut strat = MacdCrossoverManual::new("X", 12, 26, 9);
        let mut w: PaperWallet<&'static str> = PaperWallet::new(10_000.0);
        let t = Instant::now();
        let rep = run(
            &mut strat,
            &mut w,
            candles
                .iter()
                .map(|c| fugazi::types::Snapshot::single("X", (*c).into())),
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
