//! Trend-following strategies: crossover and breakout entries that ride a move.

use crate::indicators::{Bollinger, Current, Donchian, Macd, Sma, Value};
use crate::prelude::*;

use super::SingleAssetStrategy;

/// Moving-average crossover (the "golden / death cross"), always-in long/short.
///
/// Goes long when the fast SMA crosses above the slow SMA and reverses to short
/// on the opposite cross, always committing all funds to the prevailing side.
pub fn ma_crossover<Sym>(symbol: Sym, fast: usize, slow: usize) -> SingleAssetStrategy<Sym> {
    let up = || Sma::new(Current::close(), fast).crosses_above(Sma::new(Current::close(), slow));
    let down = || Sma::new(Current::close(), fast).crosses_below(Sma::new(Current::close(), slow));
    SingleAssetStrategy::new(symbol)
        .long_on(up(), down())
        .short_on(down(), up())
}

/// MACD line / signal-line crossover, always-in long/short.
///
/// Long when the MACD line crosses above its signal line, short on the opposite
/// cross. Built straight from the MACD component accessors.
pub fn macd_crossover<Sym>(
    symbol: Sym,
    fast: usize,
    slow: usize,
    signal: usize,
) -> SingleAssetStrategy<Sym> {
    let macd = Macd::new(Current::close(), fast, slow, signal);
    let up = || macd.line().crosses_above(macd.signal());
    let down = || macd.line().crosses_below(macd.signal());
    SingleAssetStrategy::new(symbol)
        .long_on(up(), down())
        .short_on(down(), up())
}

/// MACD zero-line crossover, always-in long/short.
///
/// A pure momentum-of-momentum read: long while the MACD line is above zero
/// (fast EMA over slow), short below it, flipping on the zero crossing.
pub fn macd_zero_cross<Sym>(
    symbol: Sym,
    fast: usize,
    slow: usize,
    signal: usize,
) -> SingleAssetStrategy<Sym> {
    let macd = Macd::new(Current::close(), fast, slow, signal);
    let up = || macd.line().crosses_above(Value::new(0.0));
    let down = || macd.line().crosses_below(Value::new(0.0));
    SingleAssetStrategy::new(symbol)
        .long_on(up(), down())
        .short_on(down(), up())
}

/// Donchian-channel breakout (the classic Turtle entry), always-in long/short.
///
/// Long when the close breaks above the highest high of the prior `period` bars,
/// short when it breaks below the prior `period`-bar low. The channel is lagged
/// one bar so the breakout is measured against the *prior* channel, not one that
/// already contains the breakout bar.
pub fn donchian_breakout<Sym>(symbol: Sym, period: usize) -> SingleAssetStrategy<Sym> {
    let channel = || Donchian::new(Current::high(), Current::low(), period);
    let up = || Current::close().gt(channel().upper().lag(1));
    let down = || Current::close().lt(channel().lower().lag(1));
    SingleAssetStrategy::new(symbol)
        .long_on(up(), down())
        .short_on(down(), up())
}

/// Triple moving-average alignment, long/flat.
///
/// Holds a long position only while the three SMAs are stacked bullishly
/// (`fast > mid > slow`), flattening as soon as that alignment breaks.
pub fn triple_ma<Sym>(
    symbol: Sym,
    fast: usize,
    mid: usize,
    slow: usize,
) -> SingleAssetStrategy<Sym> {
    let aligned = || {
        Sma::new(Current::close(), fast)
            .gt(Sma::new(Current::close(), mid))
            .and(Sma::new(Current::close(), mid).gt(Sma::new(Current::close(), slow)))
    };
    SingleAssetStrategy::new(symbol).long_on(aligned(), aligned().not())
}

/// Bollinger-band breakout, always-in long/short.
///
/// Treats a close beyond a band as momentum: long above the upper band, short
/// below the lower one. (Contrast [`bollinger_reversion`](super::mean_reversion::bollinger_reversion),
/// which fades the same bands.)
pub fn bollinger_breakout<Sym>(symbol: Sym, period: usize, k: Real) -> SingleAssetStrategy<Sym> {
    let bands = Bollinger::new(Current::close(), period, k);
    let up = || Current::close().gt(bands.upper());
    let down = || Current::close().lt(bands.lower());
    SingleAssetStrategy::new(symbol)
        .long_on(up(), down())
        .short_on(down(), up())
}
