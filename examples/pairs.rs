//! A multi-asset [`Strategy`]: two instruments traded independently from one
//! [`Wallet`].
//!
//! The per-bar input is a *snapshot* of both symbols (`Snapshot`), which
//! implements [`Market`] so the wallet can price each symbol. The strategy owns
//! a separate SMA-crossover entry/exit pair per symbol, feeds each its own
//! sub-candle, and acts on both symbols against a shared wallet — so a single
//! `evaluate` can emit orders for several instruments (the multi-asset / pairs
//! shape). Prices here are synthetic so the example is self-contained.
//!
//! Run with: `cargo run --example pairs`

use arcana::indicators::{Current, Sma};
use arcana::prelude::*;

/// One bar across both instruments.
#[derive(Clone, Copy)]
struct Snapshot {
    a: Candle,
    b: Candle,
}

/// Price each symbol off the snapshot, so `wallet.*` calls can value fills.
impl Market<&'static str> for Snapshot {
    fn price(&self, symbol: &&'static str) -> Real {
        match *symbol {
            "A" => self.a.close,
            "B" => self.b.close,
            _ => 0.0,
        }
    }
}

/// Long/flat SMA crossover on each of two configurable symbols. Owns the two
/// symbols plus four signals — an entry/exit pair per symbol — each consuming
/// that symbol's candle.
struct DualSma {
    a: &'static str,
    b: &'static str,
    a_enter: Box<dyn Signal<Input = Candle>>,
    a_exit: Box<dyn Signal<Input = Candle>>,
    b_enter: Box<dyn Signal<Input = Candle>>,
    b_exit: Box<dyn Signal<Input = Candle>>,
}

impl DualSma {
    fn new(a: &'static str, b: &'static str, fast: usize, slow: usize) -> Self {
        let cross_up = || Sma::new(Current::close(), fast).crosses_above(Sma::new(Current::close(), slow));
        let cross_dn = || Sma::new(Current::close(), fast).crosses_below(Sma::new(Current::close(), slow));
        Self {
            a,
            b,
            a_enter: Box::new(cross_up()),
            a_exit: Box::new(cross_dn()),
            b_enter: Box::new(cross_up()),
            b_exit: Box::new(cross_dn()),
        }
    }
}

impl Strategy for DualSma {
    type Input = Snapshot;
    type Symbol = &'static str;

    fn evaluate(&mut self, snap: Snapshot, wallet: &mut dyn Wallet<&'static str>) {
        // Advance every signal every bar (each fed its own symbol's candle),
        // then act — splitting capital half to each name.
        let a_enter = self.a_enter.update(snap.a);
        let a_exit = self.a_exit.update(snap.a);
        let b_enter = self.b_enter.update(snap.b);
        let b_exit = self.b_exit.update(snap.b);

        if a_enter {
            wallet.open(self.a, Side::Buy, Size::funds_frac(0.5), snap.a.close);
        } else if a_exit {
            wallet.close(self.a, snap.a.close);
        }
        if b_enter {
            wallet.open(self.b, Side::Buy, Size::funds_frac(0.5), snap.b.close);
        } else if b_exit {
            wallet.close(self.b, snap.b.close);
        }
    }

    fn reset(&mut self) {
        self.a_enter.reset();
        self.a_exit.reset();
        self.b_enter.reset();
        self.b_exit.reset();
    }
}

const STARTING_FUNDS: Real = 10_000.0;

fn main() {
    let bars = synthetic_snapshots(120);
    println!("running DualSma over {} two-symbol bars\n", bars.len());

    let mut strat = DualSma::new("A", "B", 3, 10);
    let mut wallet: PaperWallet<&'static str> = PaperWallet::new(STARTING_FUNDS);

    for (i, snap) in bars.iter().enumerate() {
        let filled = wallet.orders().len();
        strat.evaluate(*snap, &mut wallet);
        for order in &wallet.orders()[filled..] {
            let price = snap.price(&order.symbol);
            println!(
                "bar {i:>3}  {:<4} {:>3} {:8.3} @ {:7.2}",
                format!("{:?}", order.side).to_uppercase(),
                order.symbol,
                order.quantity,
                price
            );
        }
    }

    let last = *bars.last().unwrap();
    println!("\nfinal A position: {:+.3}", wallet.position(&"A"));
    println!("final B position: {:+.3}", wallet.position(&"B"));
    println!("final funds:      {:.2}", wallet.funds());
    println!("final equity:     {:.2}", wallet.equity(&last));
    println!(
        "strategy growth:  {:+.1}%",
        (wallet.equity(&last) / STARTING_FUNDS - 1.0) * 100.0
    );
}

/// Two deterministic price series (trend + oscillation), so the example needs no
/// data files. A flat OHLC bar is built from each close.
fn synthetic_snapshots(n: usize) -> Vec<Snapshot> {
    let candle = |close: Real| Candle::new(close, close, close, close, 1_000.0);
    (0..n)
        .map(|i| {
            let t = i as Real;
            let a = 100.0 + 0.3 * t + 12.0 * (t / 6.0).sin();
            let b = 50.0 + 0.5 * t + 9.0 * (t / 9.0).cos();
            Snapshot {
                a: candle(a),
                b: candle(b),
            }
        })
        .collect()
}
