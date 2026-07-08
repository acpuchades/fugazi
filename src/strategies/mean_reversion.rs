//! Mean-reversion strategies: fade an extreme, exit as price returns to normal.

use crate::indicators::{Bollinger, DEFAULT_EPSILON, Mfi, Rsi, Sma, StdDev, Stochastic, Value};
use crate::prelude::*;

use super::SingleAssetStrategy;

fn is_long(position: Real) -> bool {
    position > DEFAULT_EPSILON
}

fn is_short(position: Real) -> bool {
    position < -DEFAULT_EPSILON
}

/// RSI oversold-bounce, long/flat.
///
/// Buys the dip when RSI crosses *down* through `oversold`, and exits when RSI
/// recovers up through `exit_level` (e.g. 30 → 50).
pub fn rsi_reversal<Sym: Clone + PartialEq + 'static>(
    symbol: Sym,
    period: usize,
    oversold: Real,
    exit_level: Real,
) -> SingleAssetStrategy<Sym> {
    SingleAssetStrategy::new(symbol).long_on(
        Rsi::new(super::self_close::<Sym>(), period).crosses_below(Value::new(oversold)),
        Rsi::new(super::self_close::<Sym>(), period).crosses_above(Value::new(exit_level)),
    )
}

/// Bollinger-band reversion, long/flat.
///
/// Buys when the close crosses below the lower band and exits when it crosses
/// back above the middle band (the moving average). Fades the bands rather than
/// chasing the breakout.
pub fn bollinger_reversion<Sym: Clone + PartialEq + 'static>(symbol: Sym, period: usize, k: Real) -> SingleAssetStrategy<Sym> {
    let bands = Bollinger::new(super::self_close::<Sym>(), period, k).shared();
    SingleAssetStrategy::new(symbol).long_on(
        super::self_close::<Sym>().crosses_below(bands.lower()),
        super::self_close::<Sym>().crosses_above(bands.middle()),
    )
}

/// Stochastic oscillator oversold-bounce, long/flat.
///
/// The stochastic ranges `0..1` here, so `oversold`/`overbought` are fractions
/// (e.g. 0.2 / 0.8). Buys when %K crosses down through `oversold`, exits when it
/// crosses up through `overbought`.
pub fn stochastic_reversal<Sym: Clone + PartialEq + 'static>(
    symbol: Sym,
    period: usize,
    oversold: Real,
    overbought: Real,
) -> SingleAssetStrategy<Sym> {
    SingleAssetStrategy::new(symbol).long_on(
        Stochastic::new(super::self_close::<Sym>(), period).crosses_below(Value::new(oversold)),
        Stochastic::new(super::self_close::<Sym>(), period).crosses_above(Value::new(overbought)),
    )
}

/// StochRSI oversold-bounce, long/flat.
///
/// The stochastic transform over an RSI source (also `0..1`): a more responsive
/// oscillator than either alone. Same dip-buy / recovery-exit edges as
/// [`stochastic_reversal`].
pub fn stoch_rsi_reversal<Sym: Clone + PartialEq + 'static>(
    symbol: Sym,
    rsi_period: usize,
    stoch_period: usize,
    oversold: Real,
    overbought: Real,
) -> SingleAssetStrategy<Sym> {
    let stoch_rsi = || Stochastic::new(Rsi::new(super::self_close::<Sym>(), rsi_period), stoch_period);
    SingleAssetStrategy::new(symbol).long_on(
        stoch_rsi().crosses_below(Value::new(oversold)),
        stoch_rsi().crosses_above(Value::new(overbought)),
    )
}

/// Money-Flow-Index oversold-bounce, long/flat.
///
/// Volume-weighted RSI cousin (`0..100`): buys when MFI crosses down through
/// `oversold`, exits when it crosses up through `overbought` (e.g. 20 / 80).
pub fn mfi_reversal<Sym: Clone + PartialEq + 'static>(
    symbol: Sym,
    period: usize,
    oversold: Real,
    overbought: Real,
) -> SingleAssetStrategy<Sym> {
    SingleAssetStrategy::new(symbol).long_on(
        Mfi::new(super::self_bar::<Sym>(), period).crosses_below(Value::new(oversold)),
        Mfi::new(super::self_bar::<Sym>(), period).crosses_above(Value::new(overbought)),
    )
}

/// Z-score reversion, always-in long/short with a flat rest state.
///
/// Trades the standardised deviation of price from its mean,
/// `z = (close − SMA) / StdDev`: long when `z ≤ −entry` (cheap), short when
/// `z ≥ entry` (rich), and flattening once `z` reverts back through zero. Built
/// by composing the arithmetic operators over the close, its SMA and its StdDev.
///
/// Unlike the rest of the catalogue this is **not** a
/// [`SingleAssetStrategy`] specialisation — its
/// long/short/flat decision reads the raw `z` indicator directly — so it spells
/// out its own [`Strategy`] impl.
pub struct ZScoreReversion<Sym> {
    symbol: Sym,
    z: Box<dyn Indicator<Input = crate::types::Snapshot<Sym>, Output = Real>>,
    entry: Real,
}

impl<Sym: Clone + PartialEq + 'static> ZScoreReversion<Sym> {
    pub fn new(symbol: Sym, period: usize, entry: Real) -> Self {
        Self {
            symbol,
            z: Box::new(
                super::self_close::<Sym>()
                    .sub(Sma::new(super::self_close::<Sym>(), period))
                    .div(StdDev::new(super::self_close::<Sym>(), period)),
            ),
            entry,
        }
    }
}

impl<Sym: Clone + PartialEq + 'static> Strategy for ZScoreReversion<Sym> {
    type Input = crate::types::Snapshot<Sym>;
    type Symbol = Sym;

    fn update(&mut self, snap: crate::types::Snapshot<Sym>) {
        self.z.update(snap);
    }

    fn trade(&self, wallet: &mut dyn Wallet<Sym>) {
        let pos = wallet.position(&self.symbol).amount;
        if let Some(z) = self.z.value() {
            if z <= -self.entry && !is_long(pos) {
                let _ = wallet.set(self.symbol.clone(), Side::Buy, Size::value_frac(1.0));
            } else if z >= self.entry && !is_short(pos) {
                let _ = wallet.set(self.symbol.clone(), Side::Sell, Size::value_frac(1.0));
            } else if (is_long(pos) && z >= 0.0) || (is_short(pos) && z <= 0.0) {
                let _ = wallet.close(self.symbol.clone());
            }
        }
    }

    fn reset(&mut self) {
        self.z.reset();
    }
}
