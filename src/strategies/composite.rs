//! Composite strategies: multiple conditions combined into one entry — where the
//! signal combinators and component accessors earn their keep.

use crate::indicators::{Adx, Current, Keltner, Rsi, Sma, Value};
use crate::prelude::*;

use super::SingleAssetStrategy;

/// ADX-gated moving-average crossover, long/flat.
///
/// Takes the SMA golden cross only when the trend is strong enough — ADX above
/// `adx_min` — and exits on the death cross. The strength gate uses the ADX
/// component accessor (`adx.adx()`), filtering out crossovers in chop.
pub fn adx_trend_filter<Sym>(
    symbol: Sym,
    fast: usize,
    slow: usize,
    adx_period: usize,
    adx_min: Real,
) -> SingleAssetStrategy<Sym> {
    let cross_up = Sma::new(Current::close(), fast).crosses_above(Sma::new(Current::close(), slow));
    SingleAssetStrategy::new(symbol).long_on(
        cross_up.and(Adx::new(Current::candle(), adx_period).adx().above(adx_min)),
        Sma::new(Current::close(), fast).crosses_below(Sma::new(Current::close(), slow)),
    )
}

/// RSI pullback within an uptrend, long/flat.
///
/// Buys an RSI dip (RSI crossing down through `oversold`) **only while** the
/// close is above its long `trend`-period SMA, so dips are bought with the trend,
/// not against it. Exits when RSI recovers up through `exit_level`.
pub fn rsi_pullback<Sym>(
    symbol: Sym,
    rsi_period: usize,
    trend: usize,
    oversold: Real,
    exit_level: Real,
) -> SingleAssetStrategy<Sym> {
    let dip = Rsi::new(Current::close(), rsi_period).crosses_below(Value::new(oversold));
    let uptrend = Current::close().gt(Sma::new(Current::close(), trend));
    SingleAssetStrategy::new(symbol).long_on(
        dip.and(uptrend),
        Rsi::new(Current::close(), rsi_period).crosses_above(Value::new(exit_level)),
    )
}

/// Keltner-channel breakout, always-in long/short.
///
/// An ATR-banded cousin of the Bollinger breakout: long when the close pierces
/// the upper Keltner band, short below the lower one, using the channel's
/// component accessors.
pub fn keltner_breakout<Sym>(
    symbol: Sym,
    ema_period: usize,
    atr_period: usize,
    multiplier: Real,
) -> SingleAssetStrategy<Sym> {
    let channel =
        Keltner::new(Current::close(), Current::candle(), ema_period, atr_period, multiplier)
            .shared();
    let up = || Current::close().gt(channel.upper());
    let down = || Current::close().lt(channel.lower());
    SingleAssetStrategy::new(symbol)
        .long_on(up(), down())
        .short_on(down(), up())
}
