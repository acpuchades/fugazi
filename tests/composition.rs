//! End-to-end checks that indicators and signals compose through the public API.

use arcana::indicators::{Current, Ema, Identity, Rsi, Sma, Value};
use arcana::prelude::*;
use arcana::indicators::{Gt, Lt};

#[test]
fn rsi_threshold_is_a_single_signal() {
    // "RSI over 70" as one composable object.
    let mut overbought = Gt::new(Rsi::new(Identity::new(), 14), Value::new(70.0));
    // RSI(14) needs 15 samples to warm up; feed a monotonic rise past that.
    for step in 0..20 {
        overbought.update(10.0 + step as Real);
    }
    assert!(
        overbought.is_true(),
        "monotonic rise should push RSI above 70"
    );
}

#[test]
fn compound_signal_with_combinators() {
    // Enter zone: price above 100 AND RSI not yet overbought.
    let mut sig = Gt::new(Identity::new(), Value::new(100.0))
        .and(Lt::new(Rsi::new(Identity::new(), 3), Value::new(70.0)));

    // First few bars warm up the RSI; just make sure it advances without panic.
    for price in [101.0, 100.5, 101.2, 102.0] {
        sig.update(price);
    }
    let _ = sig.is_true();
}

#[test]
fn moving_average_crossover() {
    // A crossover is the rising edge of a level comparison.
    let mut cross = Sma::new(Identity::new(), 2).crosses_above(Sma::new(Identity::new(), 4));
    let mut fired = false;
    // Dip then sharp rally so the fast MA crosses above the slow MA.
    for price in [10.0, 9.0, 8.0, 7.0, 12.0, 14.0, 16.0] {
        cross.update(price);
        fired |= cross.is_true();
    }
    assert!(fired, "fast MA should cross above slow MA on the rally");
}

#[test]
fn close_crosses_above_ema_from_candles() {
    // The headline signal: feed one Candle per bar, no remembering inputs.
    let mut sig = Current::close().crosses_above(Ema::new(Current::close(), 3));
    let bar = |close: Real| Candle::new(close, close, close, close, 0.0);

    let mut fired = false;
    // Flat (close == ema) then a jump so close crosses above its own EMA.
    for close in [10.0, 10.0, 10.0, 10.0, 20.0] {
        sig.update(bar(close));
        fired |= sig.is_true();
    }
    assert!(fired, "close should cross above its EMA on the jump");
}
