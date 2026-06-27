//! Composing a trade signal from `Candle` bars.
//!
//! Indicators own their source, so a compound entry rule is a single object you
//! feed one `Candle` per bar — no remembering which value goes where. Here the
//! entry is "close crosses above its EMA-20 *and* RSI(14) is not yet
//! overbought", built by nesting constructors and combining with `BoolIndicatorExt`.
//!
//! Run with: `cargo run --example candle_signal`

use arcana::indicators::{Current, Ema, Rsi};
use arcana::prelude::*;

fn main() {
    // Entry trigger: bullish EMA crossover, gated by an RSI filter.
    let mut entry = Current::close()
        .crosses_above(Ema::new(Current::close(), 20))
        .and(Rsi::new(Current::close(), 14).below(70.0));

    // A synthetic feed: a slow drift down, then a sustained rally that pulls the
    // close up through its EMA.
    let closes: Vec<Real> = (0..40)
        .map(|i| {
            let i = i as Real;
            if i < 20.0 {
                100.0 - i * 0.5
            } else {
                90.0 + (i - 20.0) * 1.2
            }
        })
        .collect();

    for (bar, &close) in closes.iter().enumerate() {
        // Build a flat candle from the close for this illustrative feed.
        let candle = Candle::new(close, close, close, close, 1_000.0);
        entry.update(candle);
        if entry.is_true() {
            println!("bar {bar:>2}: ENTRY  (close = {close:.2})");
        }
    }
}
