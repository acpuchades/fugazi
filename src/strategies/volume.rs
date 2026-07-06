//! Volume- and money-flow-based strategies.

use crate::indicators::{Ad, Current, Obv, Sma, Vwap};
use crate::prelude::*;

use super::SingleAssetStrategy;

/// On-Balance-Volume trend, long/flat.
///
/// Treats OBV crossing its own moving average as confirmation that volume is
/// backing the move: long while OBV is above its SMA, flat below it.
pub fn obv_trend<Sym>(symbol: Sym, ma_period: usize) -> SingleAssetStrategy<Sym> {
    let bullish = || Obv::new(Current::candle()).gt(Sma::new(Obv::new(Current::candle()), ma_period));
    SingleAssetStrategy::new(symbol).long_on(bullish(), bullish().not())
}

/// VWAP reversion, long/flat.
///
/// Buys when price dips below the (session-anchored) VWAP and exits when it
/// recovers above — a classic intraday "fair value" fade. Call
/// [`reset`](Strategy::reset) at each session boundary to re-anchor the VWAP.
pub fn vwap_reversion<Sym>(symbol: Sym) -> SingleAssetStrategy<Sym> {
    SingleAssetStrategy::new(symbol).long_on(
        Current::close().crosses_below(Vwap::new(Current::candle())),
        Current::close().crosses_above(Vwap::new(Current::candle())),
    )
}

/// Chaikin Accumulation/Distribution trend, long/flat.
///
/// Like [`obv_trend`] but on the Chaikin A/D line, which weights each bar's
/// volume by where the close fell within its range: long while the A/D line is
/// above its moving average, flat below.
pub fn chaikin_ad_trend<Sym>(symbol: Sym, ma_period: usize) -> SingleAssetStrategy<Sym> {
    let bullish = || Ad::new(Current::candle()).gt(Sma::new(Ad::new(Current::candle()), ma_period));
    SingleAssetStrategy::new(symbol).long_on(bullish(), bullish().not())
}
