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

use arcana::indicators::{Current, Sma};
use arcana::prelude::*;

const CSV: &str = include_str!(concat!(env!("CARGO_MANIFEST_DIR"), "/tests/data/aapl_monthly.csv"));

const SYMBOL: &str = "AAPL";
const STARTING_FUNDS: Real = 10_000.0;

struct Reversal {
    symbol: &'static str,
    long: Box<dyn Signal<Input = Candle>>,
    short: Box<dyn Signal<Input = Candle>>,
}

impl Reversal {
    fn new(symbol: &'static str, fast: usize, slow: usize) -> Self {
        Self {
            symbol,
            long: Box::new(
                Sma::new(Current::close(), fast).crosses_above(Sma::new(Current::close(), slow)),
            ),
            short: Box::new(
                Sma::new(Current::close(), fast).crosses_below(Sma::new(Current::close(), slow)),
            ),
        }
    }
}

impl Strategy for Reversal {
    type Input = Candle;
    type Symbol = &'static str;

    fn evaluate(&mut self, candle: Candle, wallet: &mut dyn Wallet<&'static str>) {
        // Advance both signals every bar (never short-circuit, or the skipped
        // one desyncs from the price stream).
        let long = self.long.update(candle);
        let short = self.short.update(candle);
        // No close: the position is always set to a direction, so the strategy
        // is continuously in the market and reverses as the trend flips.
        if long {
            wallet.set(self.symbol, Side::Buy, Size::funds_frac(0.95), candle.close);
        } else if short {
            wallet.set(self.symbol, Side::Sell, Size::funds_frac(0.95), candle.close);
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
        strat.evaluate(*candle, &mut wallet);
        for order in &wallet.orders()[filled..] {
            println!(
                "{date}  {:<4} {:8.4} @ {:7.2}   position -> {:+.4}",
                format!("{:?}", order.side).to_uppercase(),
                order.quantity,
                candle.close,
                wallet.position(&SYMBOL)
            );
        }
    }

    let last = candles.last().unwrap().1;
    let equity = wallet.equity(&last);
    println!("\nfinal position:  {:+.4} units", wallet.position(&SYMBOL));
    println!("final equity:    {:.2}", equity);
    println!("strategy growth: {:+.1}%", (equity / STARTING_FUNDS - 1.0) * 100.0);
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
