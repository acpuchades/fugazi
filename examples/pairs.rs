//! A multi-asset [`Strategy`]: two instruments traded independently from one
//! [`Wallet`].
//!
//! The per-bar input is the library's own multi-asset frame,
//! [`Snapshot<Sym>`](fugazi::types::Snapshot) — a series of tagged atoms. The
//! strategy owns a separate SMA-crossover entry/exit pair per symbol, each
//! rooted on
//! [`Pick::matching(Selector::by_symbol(...))`](fugazi::indicators::Pick::matching)
//! to project that specific asset out of the snapshot. Prices are synthetic
//! so the example is self-contained.
//!
//! Run with: `cargo run --example pairs`

use fugazi::indicators::{Close, Pick, Sma};
use fugazi::prelude::*;
use fugazi::types::{Selector, Snapshot};

/// Long/flat SMA crossover on each of two configurable symbols. Owns the two
/// symbols plus four signals — an entry/exit pair per symbol — each rooted on
/// a symbol-matching [`Pick`] to project that asset out of every incoming
/// [`Snapshot`].
struct DualSma {
    a: &'static str,
    b: &'static str,
    a_enter: Box<dyn Signal<Snapshot<&'static str>>>,
    a_exit: Box<dyn Signal<Snapshot<&'static str>>>,
    b_enter: Box<dyn Signal<Snapshot<&'static str>>>,
    b_exit: Box<dyn Signal<Snapshot<&'static str>>>,
}

impl DualSma {
    fn new(a: &'static str, b: &'static str, fast: usize, slow: usize) -> Self {
        let close_of = |sym: &'static str| {
            Close::of(Pick::<&'static str>::matching(Selector::by_symbol(sym)))
        };
        let cross_up = |sym: &'static str| {
            Sma::new(close_of(sym), fast).crosses_above(Sma::new(close_of(sym), slow))
        };
        let cross_dn = |sym: &'static str| {
            Sma::new(close_of(sym), fast).crosses_below(Sma::new(close_of(sym), slow))
        };
        Self {
            a,
            b,
            a_enter: Box::new(cross_up(a)),
            a_exit: Box::new(cross_dn(a)),
            b_enter: Box::new(cross_up(b)),
            b_exit: Box::new(cross_dn(b)),
        }
    }
}

impl Strategy for DualSma {
    type Input = Snapshot<&'static str>;
    type Symbol = &'static str;

    fn update(&mut self, snap: Snapshot<&'static str>) {
        // Advance every signal every bar; each projects its own symbol out of
        // the snapshot via its embedded `Pick`.
        self.a_enter.update(snap.clone());
        self.a_exit.update(snap.clone());
        self.b_enter.update(snap.clone());
        self.b_exit.update(snap);
    }

    fn trade(&self, wallet: &mut dyn Wallet<&'static str>) {
        // Split capital half to each name: `value_frac(0.5)` is 50% of equity.
        if self.a_enter.is_true() {
            let _ = wallet.set(self.a, Side::Buy, Size::value_frac(0.5));
        } else if self.a_exit.is_true() {
            let _ = wallet.close(self.a);
        }
        if self.b_enter.is_true() {
            let _ = wallet.set(self.b, Side::Buy, Size::value_frac(0.5));
        } else if self.b_exit.is_true() {
            let _ = wallet.close(self.b);
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
    let bars = synthetic_snapshots(120, "A", "B");
    println!("running DualSma over {} two-symbol bars\n", bars.len());

    let mut strat = DualSma::new("A", "B", 3, 10);
    let mut wallet: PaperWallet<&'static str> = PaperWallet::new(STARTING_FUNDS);

    for (i, snap) in bars.iter().enumerate() {
        let filled = wallet.orders().len();
        // Feed each symbol's candle to the wallet for mark-to-market and fill
        // matching (they're separate wallet updates because Wallet::update is
        // per-symbol).
        for (sym, _, atom) in snap.iter() {
            if let Some(sym) = sym {
                for fill in wallet.update(*sym, atom.candle) {
                    strat.on_fill(&fill);
                }
            }
        }
        strat.update(snap.clone());
        strat.trade(&mut wallet);
        for order in &wallet.orders()[filled..] {
            let price = snap
                .find(&Selector::by_symbol(order.symbol))
                .map(|a| a.candle.close)
                .unwrap_or(0.0);
            println!(
                "bar {i:>3}  {:<4} {:>3} {:8.3} @ {:7.2}",
                format!("{:?}", order.side).to_uppercase(),
                order.symbol,
                order.units,
                price
            );
        }
    }

    println!("\nfinal A position: {:+.3}", wallet.position(&"A").amount);
    println!("final B position: {:+.3}", wallet.position(&"B").amount);
    println!("final funds:      {:.2}", wallet.funds().0);
    println!("final equity:     {:.2}", wallet.equity().0);
    println!(
        "strategy growth:  {:+.1}%",
        (wallet.equity().0 / STARTING_FUNDS - 1.0) * 100.0
    );
}

/// Two deterministic price series (trend + oscillation) packed into per-bar
/// [`Snapshot`]s tagged by symbol. A flat OHLC bar is built from each close.
fn synthetic_snapshots(
    n: usize,
    a: &'static str,
    b: &'static str,
) -> Vec<Snapshot<&'static str>> {
    let candle = |close: Real| Candle::new(close, close, close, close, 1_000.0);
    (0..n)
        .map(|i| {
            let t = i as Real;
            let a_close = 100.0 + 0.3 * t + 12.0 * (t / 6.0).sin();
            let b_close = 50.0 + 0.5 * t + 9.0 * (t / 9.0).cos();
            let mut snap = Snapshot::<&'static str>::new();
            snap.push(Some(a), None, candle(a_close).into());
            snap.push(Some(b), None, candle(b_close).into());
            snap
        })
        .collect()
}
