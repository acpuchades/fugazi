//! A long/short, always-in-the-market strategy as its own [`Strategy`] type.
//!
//! `Reversal` flips between long and short on the SMA(3)/SMA(10) crossover using
//! [`Wallet::set`] — an absolute target, so an opposite-side `set` reverses the
//! position. Because direction lives in the [`Side`] and magnitude in the
//! [`Size`], short-selling and "always in the market" are simply what the code
//! does; there are no flags. Sizing to a fraction of funds lets winners compound
//! into the next position.
//!
//! Run with: `cargo run --example strategy`

use fugazi::indicators::{Close, Pick, Sma};
use fugazi::prelude::*;
use fugazi::types::Snapshot;

const CSV: &str = include_str!(concat!(
    env!("CARGO_MANIFEST_DIR"),
    "/tests/data/aapl_monthly.csv"
));

const SYMBOL: &str = "AAPL";
const STARTING_FUNDS: Real = 10_000.0;

struct Reversal {
    symbol: &'static str,
    long: Box<dyn Signal<Snapshot<&'static str>>>,
    short: Box<dyn Signal<Snapshot<&'static str>>>,
}

impl Reversal {
    fn new(symbol: &'static str, fast: usize, slow: usize) -> Self {
        let close = || Close::of(Pick::<&'static str>::new());
        Self {
            symbol,
            long: Box::new(
                Sma::new(close(), fast).crosses_above(Sma::new(close(), slow)),
            ),
            short: Box::new(
                Sma::new(close(), fast).crosses_below(Sma::new(close(), slow)),
            ),
        }
    }
}

impl Strategy for Reversal {
    type Input = Snapshot<&'static str>;
    type Symbol = &'static str;

    fn update(&mut self, snap: Snapshot<&'static str>) {
        // Advance both signals every bar (never short-circuit, or the skipped
        // one desyncs from the price stream).
        self.long.update(snap.clone());
        self.short.update(snap);
    }

    fn trade(&self, wallet: &mut dyn Wallet<&'static str>) {
        // No close: the position is always set to a direction, so the strategy
        // is continuously in the market and reverses as the trend flips. Sizing
        // to 95% of equity (which survives a reversal) keeps a small cash buffer
        // while letting winners compound into the next position.
        if self.long.is_true() {
            let _ = wallet.set(self.symbol, Side::Buy, Size::value_frac(0.95));
        } else if self.short.is_true() {
            let _ = wallet.set(self.symbol, Side::Sell, Size::value_frac(0.95));
        }
    }

    fn reset(&mut self) {
        self.long.reset();
        self.short.reset();
    }
}

fn main() {
    let candles = load_candles(CSV);
    println!("loaded {} monthly AAPL candles\n", candles.len());

    let mut strat = Reversal::new(SYMBOL, 3, 10);
    let mut wallet: PaperWallet<&'static str> = PaperWallet::new(STARTING_FUNDS);

    for (date, candle) in &candles {
        let filled = wallet.orders().len();
        for fill in wallet.update(SYMBOL, *candle) {
            strat.on_fill(&fill);
        }
        strat.update(Snapshot::<&'static str>::from(*candle));
        strat.trade(&mut wallet);
        for order in &wallet.orders()[filled..] {
            println!(
                "{date}  {:<4} {:8.4} @ {:7.2}   position -> {:+.4}",
                format!("{:?}", order.side).to_uppercase(),
                order.units,
                candle.close,
                wallet.position(&SYMBOL).amount
            );
        }
    }

    let equity = wallet.equity().0;
    println!(
        "\nfinal position:  {:+.4} units",
        wallet.position(&SYMBOL).amount
    );
    println!("final equity:    {:.2}", equity);
    println!(
        "strategy growth: {:+.1}%",
        (equity / STARTING_FUNDS - 1.0) * 100.0
    );
}

/// Parse `date,open,high,low,close,volume` rows into `(date, Candle)` pairs.
fn load_candles(csv: &str) -> Vec<(String, Candle)> {
    csv.lines()
        .skip(1) // header
        .filter(|line| !line.trim().is_empty())
        .map(|line| {
            let mut f = line.split(',');
            let date = f.next().expect("date").to_string();
            let mut num = || f.next().expect("field").parse::<Real>().expect("number");
            let candle = Candle::new(num(), num(), num(), num(), num());
            (date, candle)
        })
        .collect()
}
