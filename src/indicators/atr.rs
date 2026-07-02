use crate::indicator::Indicator;
use crate::indicators::TrueRange;
use crate::indicators::smoothing::WilderState;
use crate::types::{Candle, Real};

/// Average True Range: Wilder's smoothing of the [`TrueRange`].
///
/// A bar indicator — it consumes the full [`Candle`] directly (it is not "ATR of
/// a price"), so unlike the price-series indicators it takes only a period.
/// Equivalent to `Rma::new(TrueRange::new(), period)`. Ready after `period`
/// bars.
#[derive(Debug, Clone)]
pub struct Atr {
    true_range: TrueRange,
    state: WilderState,
    /// Latest ATR value; `None` until warmed up.
    pub value: Option<Real>,
}

impl Atr {
    /// # Panics
    /// Panics if `period` is zero.
    pub fn new(period: usize) -> Self {
        Self {
            true_range: TrueRange::new(),
            state: WilderState::new(period),
            value: None,
        }
    }
}

impl Indicator for Atr {
    type Input = Candle;
    type Output = Real;

    fn update(&mut self, candle: Candle) -> Option<Real> {
        let tr = self
            .true_range
            .update(candle)
            .expect("true range ready from first bar");
        self.value = self.state.update(tr);
        self.value
    }

    fn value(&self) -> Option<Real> {
        self.value
    }

    fn warm_up_period(&self) -> usize {
        // The true range is ready from the first bar; the Wilder seed then
        // consumes a full period of them.
        self.state.period()
    }

    fn unstable_period(&self) -> usize {
        self.state.settle_period()
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

    fn candle(high: Real, low: Real, close: Real) -> Candle {
        Candle::new(low, high, low, close, 0.0)
    }

    #[test]
    fn warms_up_after_period_bars() {
        let mut atr = Atr::new(3);
        assert_eq!(atr.update(candle(10.0, 9.0, 9.5)), None);
        assert_eq!(atr.update(candle(11.0, 10.0, 10.5)), None);
        assert!(atr.update(candle(12.0, 11.0, 11.5)).is_some());
        assert!(atr.value.unwrap() > 0.0);
    }
}
