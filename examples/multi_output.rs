//! Reading the components of multi-output indicators.
//!
//! Indicators like `Bollinger` and `Macd` produce several values per step. Their
//! `Output` is a small `Copy` struct (`BollingerValue`, `MacdValue`), and the
//! latest of each component is also exposed as a public field refreshed on every
//! `update`.
//!
//! Run with: `cargo run --example multi_output`

use arcana::indicators::{Bollinger, Current, Macd};
use arcana::prelude::*;

fn main() {
    // Bollinger(20, 2.0) and MACD(12, 26, 9), both over the candle close.
    let mut bands = Bollinger::new(Current::close(), 20, 2.0);
    let mut macd = Macd::new(Current::close(), 12, 26, 9);

    let prices = [
        22.27, 22.19, 22.08, 22.17, 22.18, 22.13, 22.23, 22.43, 22.24, 22.29,
        22.15, 22.39, 22.38, 22.61, 23.36, 24.05, 23.75, 23.83, 23.95, 23.63,
        23.82, 23.87, 23.65, 23.19, 23.10, 23.33, 22.68, 23.10, 22.40, 22.17,
    ];

    println!(
        "{:>4}  {:>7}  {:>8}  {:>8}  {:>8}  {:>8}  {:>8}",
        "step", "price", "bb.up", "bb.mid", "bb.low", "macd", "signal"
    );
    for (step, &price) in prices.iter().enumerate() {
        let candle = Candle::new(price, price, price, price, 0.0);

        // Consume the whole-struct output...
        let bb = bands.update(candle);
        macd.update(candle);

        let fmt = |v: Option<Real>| v.map_or_else(|| "  --  ".to_string(), |x| format!("{x:8.3}"));
        match bb {
            Some(b) => print!(
                "{step:>4}  {price:>7.2}  {:8.3}  {:8.3}  {:8.3}",
                b.upper, b.middle, b.lower
            ),
            None => print!("{step:>4}  {price:>7.2}  {:>8}  {:>8}  {:>8}", "--", "--", "--"),
        }
        // ...or read the per-component public fields directly.
        println!("  {}  {}", fmt(macd.macd), fmt(macd.signal));
    }
}
