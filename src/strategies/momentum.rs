//! Momentum strategies: trade the sign of a rate-of-change or oscillator.

use crate::indicators::{Rsi};
use crate::prelude::*;

use super::SingleAssetStrategy;

/// Rate-of-change momentum, always-in long/short.
///
/// Long while the `period`-bar percentage change of the close is positive, short
/// while it is negative — the simplest time-series momentum rule.
pub fn momentum_roc<Sym: Clone + PartialEq + std::hash::Hash + Eq + 'static + Send + Sync>(symbol: Sym, period: usize) -> SingleAssetStrategy<Sym> {
    let up = || super::self_close::<Sym>().roc(period).above(0.0);
    let down = || super::self_close::<Sym>().roc(period).below(0.0);
    SingleAssetStrategy::new(symbol)
        .long_on(up(), down())
        .short_on(down(), up())
}

/// RSI midline momentum, always-in long/short.
///
/// Reads RSI as a trend gauge rather than a reversion one: long while RSI is
/// above 50, short while below — flipping as it crosses the midline.
pub fn rsi_midline<Sym: Clone + PartialEq + std::hash::Hash + Eq + 'static + Send + Sync>(symbol: Sym, period: usize) -> SingleAssetStrategy<Sym> {
    let up = || Rsi::new(super::self_close::<Sym>(), period).above(50.0);
    let down = || Rsi::new(super::self_close::<Sym>(), period).below(50.0);
    SingleAssetStrategy::new(symbol)
        .long_on(up(), down())
        .short_on(down(), up())
}
