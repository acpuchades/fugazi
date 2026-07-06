use crate::indicator::Indicator;
use crate::types::{Candle, Real};

/// Volume-Weighted Average Price (VWAP).
///
/// A bar indicator (consumes candles from an owned source). The running ratio
/// of cumulative `typical * volume` to cumulative volume, where the typical
/// price is `(high + low + close) / 3` (see [`Candle::typical`]).
///
/// This is the *cumulative* VWAP, anchored at construction. Since the crate has
/// no notion of trading sessions, anchor a new session by calling
/// [`reset`](Indicator::reset) at its boundary. Ready from the first bar that
/// gives a non-zero cumulative volume (`None` while cumulative volume is zero).
#[derive(Debug, Clone)]
pub struct Vwap<S> {
    source: S,
    cum_pv: Real,
    cum_volume: Real,
    /// Latest VWAP value; `None` until cumulative volume is non-zero.
    pub value: Option<Real>,
}

impl<S> Vwap<S> {
    pub fn new(source: S) -> Self {
        Self {
            source,
            cum_pv: 0.0,
            cum_volume: 0.0,
            value: None,
        }
    }
}

impl<S: Indicator<Output = Candle>> Indicator for Vwap<S> {
    type Input = S::Input;
    type Output = Real;

    fn update(&mut self, input: S::Input) -> Option<Real> {
        let candle = self.source.update(input)?;
        self.cum_pv += candle.typical() * candle.volume;
        self.cum_volume += candle.volume;
        self.value = (self.cum_volume != 0.0).then(|| self.cum_pv / self.cum_volume);
        self.value
    }

    fn value(&self) -> Option<Real> {
        self.value
    }

    /// Ready as soon as the source produces a bar with non-zero volume
    /// (all-zero-volume bars delay readiness); the anchored average itself is
    /// not unstable.
    fn warm_up_period(&self) -> usize {
        self.source.warm_up_period().max(1)
    }

    fn unstable_period(&self) -> usize {
        self.source.unstable_period()
    }

    fn reset(&mut self) {
        self.source.reset();
        self.cum_pv = 0.0;
        self.cum_volume = 0.0;
        self.value = None;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::indicators::Current;
    use crate::types::Candle;

    #[test]
    fn weights_price_by_volume() {
        let mut vwap = Vwap::new(Current::candle());
        // typical = close here; VWAP of one bar is its typical price.
        assert_eq!(
            vwap.update(Candle::new(10.0, 10.0, 10.0, 10.0, 100.0).into()),
            Some(10.0)
        );
        // Second bar at typical 20 with 3x the volume pulls VWAP toward 20:
        // (10*100 + 20*300) / 400 = 17.5
        assert_eq!(
            vwap.update(Candle::new(20.0, 20.0, 20.0, 20.0, 300.0).into()),
            Some(17.5)
        );
    }

    #[test]
    fn zero_volume_is_not_ready() {
        let mut vwap = Vwap::new(Current::candle());
        assert_eq!(
            vwap.update(Candle::new(10.0, 10.0, 10.0, 10.0, 0.0).into()),
            None
        );
    }
}
