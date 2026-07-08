//! Composite strategies: multiple conditions combined into one entry — where the
//! signal combinators and component accessors earn their keep.

use crate::indicators::{Adx, Keltner, Rsi, Sma, Value};
use crate::prelude::*;

use super::SingleAssetStrategy;

/// ADX-gated moving-average crossover, long/flat.
///
/// Takes the SMA golden cross only when the trend is strong enough — ADX above
/// `adx_min` — and exits on the death cross. The strength gate uses the ADX
/// component accessor (`adx.adx()`), filtering out crossovers in chop.
pub fn adx_trend_filter<Sym: Clone + PartialEq + 'static>(
    symbol: Sym,
    fast: usize,
    slow: usize,
    adx_period: usize,
    adx_min: Real,
) -> SingleAssetStrategy<Sym> {
    let cross_up = Sma::new(super::self_close::<Sym>(), fast).crosses_above(Sma::new(super::self_close::<Sym>(), slow));
    SingleAssetStrategy::new(symbol).long_on(
        cross_up.and(Adx::new(super::self_bar::<Sym>(), adx_period).adx().above(adx_min)),
        Sma::new(super::self_close::<Sym>(), fast).crosses_below(Sma::new(super::self_close::<Sym>(), slow)),
    )
}

/// RSI pullback within an uptrend, long/flat.
///
/// Buys an RSI dip (RSI crossing down through `oversold`) **only while** the
/// close is above its long `trend`-period SMA, so dips are bought with the trend,
/// not against it. Exits when RSI recovers up through `exit_level`.
pub fn rsi_pullback<Sym: Clone + PartialEq + 'static>(
    symbol: Sym,
    rsi_period: usize,
    trend: usize,
    oversold: Real,
    exit_level: Real,
) -> SingleAssetStrategy<Sym> {
    let dip = Rsi::new(super::self_close::<Sym>(), rsi_period).crosses_below(Value::new(oversold));
    let uptrend = super::self_close::<Sym>().gt(Sma::new(super::self_close::<Sym>(), trend));
    SingleAssetStrategy::new(symbol).long_on(
        dip.and(uptrend),
        Rsi::new(super::self_close::<Sym>(), rsi_period).crosses_above(Value::new(exit_level)),
    )
}

/// Keltner-channel breakout, always-in long/short.
///
/// An ATR-banded cousin of the Bollinger breakout: long when the close pierces
/// the upper Keltner band, short below the lower one, using the channel's
/// component accessors.
pub fn keltner_breakout<Sym: Clone + PartialEq + 'static>(
    symbol: Sym,
    ema_period: usize,
    atr_period: usize,
    multiplier: Real,
) -> SingleAssetStrategy<Sym> {
    let channel =
        Keltner::new(super::self_close::<Sym>(), super::self_bar::<Sym>(), ema_period, atr_period, multiplier)
            .shared();
    let up = || super::self_close::<Sym>().gt(channel.upper());
    let down = || super::self_close::<Sym>().lt(channel.lower());
    SingleAssetStrategy::new(symbol)
        .long_on(up(), down())
        .short_on(down(), up())
}
