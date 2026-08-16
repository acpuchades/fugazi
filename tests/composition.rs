//! End-to-end checks that indicators and signals compose through the public API.

use fugazi::indicators::{Current, Ema, Identity, Rsi, Sma, Value};
use fugazi::prelude::*;
use fugazi::indicators::{Gt, Lt};

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

/// `and` is `None` until *both* operands are warm — the documented reason an
/// edge coincident with warm-up never fires a spurious first-bar trade — and
/// once warm it is the plain conjunction of the two sides.
#[test]
fn compound_signal_with_combinators() {
    // Enter zone: price above 100 AND RSI not yet overbought. `Gt` against a
    // constant is ready immediately; RSI(3) needs four samples, so the
    // conjunction is gated by the slower side.
    let build = || {
        Gt::new(Identity::new(), Value::new(100.0))
            .and(Lt::new(Rsi::new(Identity::new(), 3), Value::new(70.0)))
    };

    // A monotonic rise: price is above 100 throughout, and RSI(3) with no
    // losing delta reads 100 — so the "not overbought" side is false and the
    // conjunction must be `Some(false)`, never `Some(true)`.
    let mut rising = build();
    let readings: Vec<Option<bool>> = [101.0, 102.0, 103.0, 104.0, 105.0]
        .into_iter()
        .map(|p| rising.update(p))
        .collect();
    assert_eq!(
        readings,
        vec![None, None, None, Some(false), Some(false)],
        "`and` stays None until RSI(3) is warm, then reports the conjunction"
    );

    // Same chain, but the price falls back through 100 while RSI cools off.
    // Both sides now agree, so the zone opens.
    let mut dipping = build();
    let mut fired = false;
    for price in [101.0, 105.0, 103.0, 101.5, 101.0, 100.5, 101.0] {
        dipping.update(price);
        fired |= dipping.is_true();
    }
    assert!(
        fired,
        "price over 100 with a cooled-off RSI should open the entry zone"
    );
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
        sig.update(bar(close).into());
        fired |= sig.is_true();
    }
    assert!(fired, "close should cross above its EMA on the jump");
}
