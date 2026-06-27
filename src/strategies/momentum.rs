//! Momentum strategies: trade the sign of a rate-of-change or oscillator.

use crate::indicators::{Current, Rsi};
use crate::prelude::*;

use super::SingleAssetStrategy;

/// Rate-of-change momentum, always-in long/short.
///
/// Long while the `period`-bar percentage change of the close is positive, short
/// while it is negative — the simplest time-series momentum rule.
pub fn momentum_roc<Sym>(symbol: Sym, period: usize) -> SingleAssetStrategy<Sym> {
    let up = || Current::close().roc(period).above(0.0);
    let down = || Current::close().roc(period).below(0.0);
    SingleAssetStrategy::new(symbol)
        .long_on(up(), down())
        .short_on(down(), up())
}

/// RSI midline momentum, always-in long/short.
///
/// Reads RSI as a trend gauge rather than a reversion one: long while RSI is
/// above 50, short while below — flipping as it crosses the midline.
pub fn rsi_midline<Sym>(symbol: Sym, period: usize) -> SingleAssetStrategy<Sym> {
    let up = || Rsi::new(Current::close(), period).above(50.0);
    let down = || Rsi::new(Current::close(), period).below(50.0);
    SingleAssetStrategy::new(symbol)
        .long_on(up(), down())
        .short_on(down(), up())
}
