//! Streaming a bare `f64` price feed through an indicator.
//!
//! The simplest shape: an indicator over a raw `Real` stream. `Identity` is the
//! leaf that passes each value straight through, so `Rsi::new(Identity::new(),
//! 14)` is "RSI(14) of the incoming prices". `update` returns `None` during
//! warm-up and `Some(value)` once enough samples have been observed.
//!
//! Run with: `cargo run --example streaming`

use fugazi::indicators::{Ema, Identity, Rsi};
use fugazi::prelude::*;

fn main() {
    // RSI(14) and EMA(5) of the same raw price stream, advanced in lock-step.
    let mut rsi = Rsi::new(Identity::new(), 14);
    let mut ema = Ema::new(Identity::new(), 5);

    let prices = [
        44.34, 44.09, 44.15, 43.61, 44.33, 44.83, 45.10, 45.42, 45.84, 46.08, 45.89, 46.03, 45.61,
        46.28, 46.28, 46.00, 46.03, 46.41, 46.22, 45.64,
    ];

    println!(
        "{:>4}  {:>7}  {:>8}  {:>8}",
        "step", "price", "ema(5)", "rsi(14)"
    );
    for (step, &price) in prices.iter().enumerate() {
        let ema_out = ema.update(price);
        let rsi_out = rsi.update(price);

        // Outputs are `Option` until each indicator warms up.
        let fmt = |v: Option<Real>| v.map_or_else(|| "  --  ".to_string(), |x| format!("{x:8.3}"));
        println!(
            "{step:>4}  {price:>7.2}  {}  {}",
            fmt(ema_out),
            fmt(rsi_out)
        );
    }
}
