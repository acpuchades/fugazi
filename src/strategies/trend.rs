//! Trend-following strategies: crossover and breakout entries that ride a move.

use crate::indicators::{Bollinger, Donchian, Macd, Sma, Value};
use crate::prelude::*;

use super::SingleAssetStrategy;

/// Moving-average crossover (the "golden / death cross"), always-in long/short.
///
/// Goes long when the fast SMA crosses above the slow SMA and reverses to short
/// on the opposite cross, always committing all funds to the prevailing side.
pub fn ma_crossover<Sym: Clone + PartialEq + std::hash::Hash + Eq + 'static + Send + Sync>(symbol: Sym, fast: usize, slow: usize) -> SingleAssetStrategy<Sym> {
    let up = || Sma::new(super::self_close::<Sym>(), fast).crosses_above(Sma::new(super::self_close::<Sym>(), slow));
    let down = || Sma::new(super::self_close::<Sym>(), fast).crosses_below(Sma::new(super::self_close::<Sym>(), slow));
    SingleAssetStrategy::new(symbol)
        .long_on(up(), down())
        .short_on(down(), up())
}

/// MACD line / signal-line crossover, always-in long/short.
///
/// Long when the MACD line crosses above its signal line, short on the opposite
/// cross. Built straight from the MACD component accessors.
pub fn macd_crossover<Sym: Clone + PartialEq + std::hash::Hash + Eq + 'static + Send + Sync>(
    symbol: Sym,
    fast: usize,
    slow: usize,
    signal: usize,
) -> SingleAssetStrategy<Sym> {
    let macd = Macd::new(super::self_close::<Sym>(), fast, slow, signal).shared();
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
pub fn macd_zero_cross<Sym: Clone + PartialEq + std::hash::Hash + Eq + 'static + Send + Sync>(
    symbol: Sym,
    fast: usize,
    slow: usize,
    signal: usize,
) -> SingleAssetStrategy<Sym> {
    let macd = Macd::new(super::self_close::<Sym>(), fast, slow, signal).shared();
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
pub fn donchian_breakout<Sym: Clone + PartialEq + std::hash::Hash + Eq + 'static + Send + Sync>(symbol: Sym, period: usize) -> SingleAssetStrategy<Sym> {
    let channel = Donchian::new(super::self_high::<Sym>(), super::self_low::<Sym>(), period).shared();
    let up = || super::self_close::<Sym>().gt(channel.upper().lag(1));
    let down = || super::self_close::<Sym>().lt(channel.lower().lag(1));
    SingleAssetStrategy::new(symbol)
        .long_on(up(), down())
        .short_on(down(), up())
}

/// Triple moving-average alignment, long/flat.
///
/// Holds a long position only while the three SMAs are stacked bullishly
/// (`fast > mid > slow`), flattening as soon as that alignment breaks.
pub fn triple_ma<Sym: Clone + PartialEq + std::hash::Hash + Eq + 'static + Send + Sync>(
    symbol: Sym,
    fast: usize,
    mid: usize,
    slow: usize,
) -> SingleAssetStrategy<Sym> {
    let aligned = || {
        Sma::new(super::self_close::<Sym>(), fast)
            .gt(Sma::new(super::self_close::<Sym>(), mid))
            .and(Sma::new(super::self_close::<Sym>(), mid).gt(Sma::new(super::self_close::<Sym>(), slow)))
    };
    SingleAssetStrategy::new(symbol).long_on(aligned(), aligned().not())
}

/// Bollinger-band breakout, always-in long/short.
///
/// Treats a close beyond a band as momentum: long above the upper band, short
/// below the lower one. (Contrast [`bollinger_reversion`](super::mean_reversion::bollinger_reversion),
/// which fades the same bands.)
pub fn bollinger_breakout<Sym: Clone + PartialEq + std::hash::Hash + Eq + 'static + Send + Sync>(symbol: Sym, period: usize, k: Real) -> SingleAssetStrategy<Sym> {
    let bands = Bollinger::new(super::self_close::<Sym>(), period, k).shared();
    let up = || super::self_close::<Sym>().gt(bands.upper());
    let down = || super::self_close::<Sym>().lt(bands.lower());
    SingleAssetStrategy::new(symbol)
        .long_on(up(), down())
        .short_on(down(), up())
}
