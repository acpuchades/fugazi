use crate::indicator::Indicator;
use crate::indicators::TrueRange;
use crate::indicators::smoothing::WilderState;
use crate::types::{Candle, Real};

/// Average True Range: Wilder's smoothing of the [`TrueRange`].
///
/// A bar indicator — it consumes candles from an owned source, so composition
/// is construction: `Atr::new(Current::candle(), 14)` is the classic 14-bar
/// ATR of the base stream. Equivalent to
/// `Rma::new(TrueRange::new(source), period)`. Ready `period` bars after the
/// source is.
#[derive(Debug, Clone)]
pub struct Atr<S> {
    true_range: TrueRange<S>,
    state: WilderState,
    /// Latest ATR value; `None` until warmed up.
    pub value: Option<Real>,
}

impl<S> Atr<S> {
    /// # Panics
    /// Panics if `period` is zero.
    pub fn new(source: S, period: usize) -> Self {
        Self {
            true_range: TrueRange::new(source),
            state: WilderState::new(period),
            value: None,
        }
    }
}

impl<S: Indicator<Output = Candle>> Indicator for Atr<S> {
    type Input = S::Input;
    type Output = Real;

    fn update(&mut self, input: S::Input) -> Option<Real> {
        let tr = self.true_range.update(input)?;
        self.value = self.state.update(tr);
        self.value
    }

    fn value(&self) -> Option<Real> {
        self.value
    }

    fn warm_up_period(&self) -> usize {
        // The true range is ready as soon as the source is; the Wilder seed then
        // consumes a full period of them.
        self.true_range.warm_up_period() + self.state.period() - 1
    }

    fn unstable_period(&self) -> usize {
        self.true_range.unstable_period() + self.state.settle_period()
    }

    fn reset(&mut self) {
        self.true_range.reset();
        self.state.reset();
        self.value = None;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::indicators::Current;
    use crate::types::Candle;

    fn candle(high: Real, low: Real, close: Real) -> Candle {
        Candle::new(low, high, low, close, 0.0)
    }

    #[test]
    fn warms_up_after_period_bars() {
        let mut atr = Atr::new(Current::candle(), 3);
        assert_eq!(atr.update(candle(10.0, 9.0, 9.5).into()), None);
        assert_eq!(atr.update(candle(11.0, 10.0, 10.5).into()), None);
        assert!(atr.update(candle(12.0, 11.0, 11.5).into()).is_some());
        assert!(atr.value.unwrap() > 0.0);
    }
}
