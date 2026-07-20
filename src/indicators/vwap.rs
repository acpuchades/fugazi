use std::collections::VecDeque;

use crate::indicator::Indicator;
use crate::types::{Candle, Real};

/// Volume-Weighted Average Price (VWAP), rolling over the last `period` bars.
///
/// A bar indicator (consumes candles from an owned source). Maintains running
/// sums of `typical * volume` and `volume` across the retained window, so each
/// update is O(1); the value is the ratio of the two. Typical price is
/// `(high + low + close) / 3` (see [`Candle::typical`]).
///
/// Anchored / session VWAP is not modelled — the crate has no notion of
/// trading sessions, so the rolling form is the only shape that generalises
/// across the 24/7 markets it targets. Ready once `period` bars have been
/// observed *and* the retained window carries non-zero volume (a stretch of
/// zero-volume bars in the window returns `None`).
#[derive(Debug, Clone)]
pub struct Vwap<S> {
    source: S,
    period: usize,
    window: VecDeque<(Real, Real)>,
    sum_pv: Real,
    sum_volume: Real,
    value: Option<Real>,
}

impl<S> Vwap<S> {
    pub fn new(source: S, period: usize) -> Self {
        assert!(period > 0, "VWAP period must be greater than zero");
        Self {
            source,
            period,
            window: VecDeque::with_capacity(period),
            sum_pv: 0.0,
            sum_volume: 0.0,
            value: None,
        }
    }
}

impl<S: Indicator<Output = Candle>> Indicator for Vwap<S> {
    type Input = S::Input;
    type Output = Real;

    fn update(&mut self, input: S::Input) -> Option<Real> {
        let candle = self.source.update(input)?;
        let pv = candle.typical() * candle.volume;
        self.window.push_back((pv, candle.volume));
        self.sum_pv += pv;
        self.sum_volume += candle.volume;
        if self.window.len() > self.period {
            let (old_pv, old_v) = self.window.pop_front().expect("window is non-empty");
            self.sum_pv -= old_pv;
            self.sum_volume -= old_v;
        }
        self.value = (self.window.len() == self.period && self.sum_volume != 0.0)
            .then(|| self.sum_pv / self.sum_volume);
        self.value
    }

    fn value(&self) -> Option<Real> {
        self.value
    }

    fn warm_up_period(&self) -> usize {
        self.source.warm_up_period() + self.period - 1
    }

    fn unstable_period(&self) -> usize {
        self.source.unstable_period()
    }

    fn reset(&mut self) {
        self.source.reset();
        self.window.clear();
        self.sum_pv = 0.0;
        self.sum_volume = 0.0;
        self.value = None;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::indicators::Current;
    use crate::types::Candle;

    #[test]
    fn weights_price_by_volume_over_window() {
        let mut vwap = Vwap::new(Current::candle(), 2);
        // First bar: window not full yet.
        assert_eq!(
            vwap.update(Candle::new(10.0, 10.0, 10.0, 10.0, 100.0).into()),
            None
        );
        // Second bar completes the window: (10*100 + 20*300) / 400 = 17.5
        assert_eq!(
            vwap.update(Candle::new(20.0, 20.0, 20.0, 20.0, 300.0).into()),
            Some(17.5)
        );
        // Third bar evicts the first: (20*300 + 30*200) / 500 = 24.0
        assert_eq!(
            vwap.update(Candle::new(30.0, 30.0, 30.0, 30.0, 200.0).into()),
            Some(24.0)
        );
    }

    #[test]
    fn zero_volume_window_is_not_ready() {
        let mut vwap = Vwap::new(Current::candle(), 2);
        assert_eq!(
            vwap.update(Candle::new(10.0, 10.0, 10.0, 10.0, 0.0).into()),
            None
        );
        assert_eq!(
            vwap.update(Candle::new(20.0, 20.0, 20.0, 20.0, 0.0).into()),
            None
        );
    }
}
