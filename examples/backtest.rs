//! Batch backtest over real OHLCV data — the same incremental code, fed from a
//! historical file instead of a live socket.
//!
//! Defines its *own* strategy type (`GoldenCross`) implementing [`Strategy`]:
//! each bar it reads the SMA(3)/SMA(10) crossover and trades into the `Wallet`
//! it is handed — all-in long on the golden cross, flat on the death cross.
//! Here the wallet is a [`PaperWallet`], but since the strategy takes
//! `&mut dyn Wallet` it would drive a live broker wallet unchanged. The CSV is
//! parsed with the standard library only, keeping the example zero-dependency.
//!
//! Run with: `cargo run --example backtest`

use fugazi::indicators::{Current, Sma};
use fugazi::prelude::*;

// Embed the sample data at compile time so the example is self-contained.
const CSV: &str = include_str!(concat!(
    env!("CARGO_MANIFEST_DIR"),
    "/tests/data/aapl_monthly.csv"
));

const SYMBOL: &str = "AAPL";
const STARTING_FUNDS: Real = 10_000.0;

/// Long/flat SMA crossover on a configurable symbol. Holds only the symbol it
/// trades and its two signals; the portfolio lives in the wallet it is handed.
struct GoldenCross {
    symbol: &'static str,
    enter: Box<dyn Signal>,
    exit: Box<dyn Signal>,
}

impl GoldenCross {
    fn new(symbol: &'static str, fast: usize, slow: usize) -> Self {
        Self {
            symbol,
            enter: Box::new(
                Sma::new(Current::close(), fast).crosses_above(Sma::new(Current::close(), slow)),
            ),
            exit: Box::new(
                Sma::new(Current::close(), fast).crosses_below(Sma::new(Current::close(), slow)),
            ),
        }
    }
}

impl Strategy for GoldenCross {
    type Input = Candle;
    type Symbol = &'static str;

    fn update(&mut self, candle: Candle) {
        // Advance BOTH signals every bar (never short-circuit, or the skipped
        // one desyncs from the price stream).
        self.enter.update(candle);
        self.exit.update(candle);
    }

    fn trade(&self, wallet: &mut dyn Wallet<&'static str>) {
        // Commit all equity long on the golden cross; flatten on the death cross.
        if self.enter.is_true() {
            let _ = wallet.set(self.symbol, Side::Buy, Size::value_frac(1.0));
        } else if self.exit.is_true() {
            let _ = wallet.close(self.symbol);
        }
    }

    fn reset(&mut self) {
        self.enter.reset();
        self.exit.reset();
    }
}

fn main() {
    let candles = load_candles(CSV);
    println!("loaded {} monthly AAPL candles\n", candles.len());

    let mut strategy = GoldenCross::new(SYMBOL, 3, 10);
    let mut wallet: PaperWallet<&'static str> = PaperWallet::new(STARTING_FUNDS);

    for (date, candle) in &candles {
        let filled = wallet.orders().len();
        for fill in wallet.update(SYMBOL, *candle) {
            strategy.on_fill(&fill);
        }
        strategy.update(*candle);
        strategy.trade(&mut wallet);
        // Print whatever this bar appended to the blotter.
        for order in &wallet.orders()[filled..] {
            println!(
                "{date}  {:<4} {:8.2} @ {:7.2}",
                format!("{:?}", order.side).to_uppercase(),
                order.units,
                candle.close
            );
        }
    }

    // Equity = cash on hand + the open position marked to the last fed price.
    let equity = wallet.equity().0;
    println!("\nfinal funds:     {:.2}", wallet.funds().0);
    println!("final equity:    {:.2}", equity);
    println!(
        "strategy growth: {:+.1}%",
        (equity / STARTING_FUNDS - 1.0) * 100.0
    );

    // Buy-and-hold benchmark over the same window.
    let last = candles.last().unwrap().1;
    let first = candles.first().unwrap().1.close;
    println!(
        "buy & hold:      {:+.1}%",
        (last.close / first - 1.0) * 100.0
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
