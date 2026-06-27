//! Reading the components of multi-output indicators — three ways.
//!
//! Indicators like `Bollinger` and `Macd` produce several values per step. Their
//! `Output` is a small `Copy` struct (`BollingerValue`, `MacdValue`), and the
//! latest of each component is also exposed as a public field refreshed on every
//! `update`. Beyond reading them, each component has an **accessor** that
//! projects that one field back into an ordinary `Indicator<Output = Real>`
//! (`macd.line()`, `bands.upper()`, …), so a single component composes and
//! compares like any other source. This example shows all three:
//!
//! 1. the whole-struct output (`BollingerValue`);
//! 2. the per-component public fields (`macd.macd`, `macd.signal`);
//! 3. components projected into composable signals via the accessors
//!    (`macd.line().crosses_above(macd.signal())`,
//!    `close.crosses_above(bands.middle())`).
//!
//! Run with: `cargo run --example multi_output`

use arcana::indicators::{Bollinger, Current, Macd};
use arcana::prelude::*;

fn main() {
    // Bollinger(20, 2.0) and MACD(12, 26, 9), both over the candle close.
    let mut bands = Bollinger::new(Current::close(), 20, 2.0);
    let mut macd = Macd::new(Current::close(), 12, 26, 9);

    // Way 3: build signals straight from the component accessors. Each accessor
    // clones its source, so these are self-contained — they own their own MACD /
    // Bollinger and just need the same candle fed to them each bar.
    let macd_proto = Macd::new(Current::close(), 12, 26, 9);
    let mut bullish_cross = macd_proto.line().crosses_above(macd_proto.signal());
    let mut above_mid =
        Current::close().crosses_above(Bollinger::new(Current::close(), 20, 2.0).middle());

    let prices = [
        22.27, 22.19, 22.08, 22.17, 22.18, 22.13, 22.23, 22.43, 22.24, 22.29, 22.15, 22.39, 22.38,
        22.61, 23.36, 24.05, 23.75, 23.83, 23.95, 23.63, 23.82, 23.87, 23.65, 23.19, 23.10, 23.33,
        22.68, 23.10, 22.40, 22.17,
    ];

    println!(
        "{:>4}  {:>7}  {:>8}  {:>8}  {:>8}  {:>8}  {:>8}  {:>5}  {:>4}",
        "step", "price", "bb.up", "bb.mid", "bb.low", "macd", "signal", "cross", "mid↑"
    );
    for (step, &price) in prices.iter().enumerate() {
        let candle = Candle::new(price, price, price, price, 0.0);

        // Way 1: consume the whole-struct output...
        let bb = bands.update(candle);
        macd.update(candle);
        // Way 3: advance the composed signals (one bar each, like any signal).
        let crossed = bullish_cross.update(candle).unwrap_or(false);
        let crossed_mid = above_mid.update(candle).unwrap_or(false);

        let fmt = |v: Option<Real>| v.map_or_else(|| "  --  ".to_string(), |x| format!("{x:8.3}"));
        let flag = |b: bool| if b { "✓" } else { "·" };
        match bb {
            Some(b) => print!(
                "{step:>4}  {price:>7.2}  {:8.3}  {:8.3}  {:8.3}",
                b.upper, b.middle, b.lower
            ),
            None => print!(
                "{step:>4}  {price:>7.2}  {:>8}  {:>8}  {:>8}",
                "--", "--", "--"
            ),
        }
        // Way 2: read the per-component public fields directly.
        println!(
            "  {}  {}  {:>5}  {:>4}",
            fmt(macd.macd),
            fmt(macd.signal),
            flag(crossed),
            flag(crossed_mid)
        );
    }
}
