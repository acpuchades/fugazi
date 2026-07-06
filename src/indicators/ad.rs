use crate::indicator::Indicator;
use crate::types::{Candle, Real};

/// Accumulation/Distribution Line (Chaikin A/D).
///
/// A bar indicator (consumes candles from an owned source). A cumulative volume
/// flow weighted by the *close-location value* — where the close sits within
/// the bar's range, `((close - low) - (high - close)) / (high - low)`, in
/// `[-1, 1]`. A bar whose high equals its low has no range and contributes
/// nothing. Seeds at the source's first candle.
#[derive(Debug, Clone)]
pub struct Ad<S> {
    source: S,
    cumulative: Real,
    /// Latest A/D line value; `None` before the source produces its first bar.
    pub value: Option<Real>,
}

impl<S> Ad<S> {
    pub fn new(source: S) -> Self {
        Self {
            source,
            cumulative: 0.0,
            value: None,
        }
    }
}

impl<S: Indicator<Output = Candle>> Indicator for Ad<S> {
    type Input = S::Input;
    type Output = Real;

    fn update(&mut self, input: S::Input) -> Option<Real> {
        let candle = self.source.update(input)?;
        let range = candle.high - candle.low;
        if range != 0.0 {
            let clv = ((candle.close - candle.low) - (candle.high - candle.close)) / range;
            self.cumulative += clv * candle.volume;
        }
        self.value = Some(self.cumulative);
        self.value
    }

    fn value(&self) -> Option<Real> {
        self.value
    }

    /// Ready as soon as the source is; the cumulative line is anchored rather
    /// than unstable.
    fn warm_up_period(&self) -> usize {
        self.source.warm_up_period().max(1)
    }

    fn unstable_period(&self) -> usize {
        self.source.unstable_period()
    }

    fn reset(&mut self) {
        self.source.reset();
        self.cumulative = 0.0;
        self.value = None;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::indicators::Current;
    use crate::types::Candle;

    #[test]
    fn close_at_high_accumulates_full_volume() {
        let mut ad = Ad::new(Current::candle());
        // close == high: CLV = +1, so the whole volume is accumulated.
        assert_eq!(
            ad.update(Candle::new(10.0, 12.0, 8.0, 12.0, 100.0).into()),
            Some(100.0)
        );
        // close == low: CLV = -1, distributes the whole volume back.
        assert_eq!(
            ad.update(Candle::new(12.0, 14.0, 10.0, 10.0, 50.0).into()),
            Some(50.0)
        );
    }

    #[test]
    fn flat_bar_contributes_nothing() {
        let mut ad = Ad::new(Current::candle());
        assert_eq!(
            ad.update(Candle::new(10.0, 10.0, 10.0, 10.0, 100.0).into()),
            Some(0.0)
        );
    }
}
