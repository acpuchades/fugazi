//! Batch backtest over real OHLCV data — the same incremental code, fed from a
//! historical file instead of a live socket.
//!
//! Loads monthly AAPL candles (bundled with the crate's tests), runs an SMA(3)
//! /SMA(10) crossover, and tracks a naive long/flat position to show how signals
//! drive a simple equity curve. The CSV is parsed with the standard library
//! only, keeping the example zero-dependency like the crate itself.
//!
//! Run with: `cargo run --example backtest`

use arcana::indicators::{Current, Sma};
use arcana::prelude::*;

// Embed the sample data at compile time so the example is self-contained.
const CSV: &str = include_str!(concat!(env!("CARGO_MANIFEST_DIR"), "/tests/data/aapl_monthly.csv"));

fn main() {
    let candles = load_candles(CSV);
    println!("loaded {} monthly AAPL candles\n", candles.len());

    // Golden/death cross on a fast and slow SMA of the close.
    let mut golden_cross = Sma::new(Current::close(), 3).crosses_above(Sma::new(Current::close(), 10));
    let mut death_cross = Sma::new(Current::close(), 3).crosses_below(Sma::new(Current::close(), 10));

    let mut in_position = false;
    let mut entry_price = 0.0;
    let mut equity = 1.0; // multiplicative return of the long/flat strategy
    let mut trades = 0;

    for (date, candle) in &candles {
        // Both signals see the same bar; only one can fire on a given step.
        let enter = golden_cross.update(*candle);
        let exit = death_cross.update(*candle);

        if !in_position && enter {
            in_position = true;
            entry_price = candle.close;
            println!("{date}  BUY   @ {:.2}", candle.close);
        } else if in_position && exit {
            in_position = false;
            let ret = candle.close / entry_price;
            equity *= ret;
            trades += 1;
            println!("{date}  SELL  @ {:.2}   trade return {:+.1}%", candle.close, (ret - 1.0) * 100.0);
        }
    }

    // Mark any open position to the last close.
    if in_position {
        let last = candles.last().unwrap().1.close;
        equity *= last / entry_price;
        trades += 1;
    }

    println!("\nclosed/realized {trades} trade(s)");
    println!("strategy growth: {:+.1}%", (equity - 1.0) * 100.0);

    // Buy-and-hold benchmark over the same window.
    let (first, last) = (candles.first().unwrap().1.close, candles.last().unwrap().1.close);
    println!("buy & hold:      {:+.1}%", (last / first - 1.0) * 100.0);
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
