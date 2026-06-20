use crate::indicator::Indicator;
use crate::types::{Candle, Real};

/// Accumulation/Distribution Line (Chaikin A/D).
///
/// A bar indicator (consumes the full [`Candle`]). A cumulative volume flow
/// weighted by the *close-location value* — where the close sits within the
/// bar's range, `((close - low) - (high - close)) / (high - low)`, in `[-1, 1]`.
/// A bar whose high equals its low has no range and contributes nothing. Seeds
/// at the first bar, so it is ready immediately.
#[derive(Debug, Clone, Default)]
pub struct Ad {
    cumulative: Real,
    /// Latest A/D line value; `None` before the first bar.
    pub value: Option<Real>,
}

impl Ad {
    pub fn new() -> Self {
        Self::default()
    }
}

impl Indicator for Ad {
    type Input = Candle;
    type Output = Real;

    fn update(&mut self, candle: Candle) -> Option<Real> {
        let range = candle.high - candle.low;
        if range != 0.0 {
            let clv = ((candle.close - candle.low) - (candle.high - candle.close)) / range;
            self.cumulative += clv * candle.volume;
        }
        self.value = Some(self.cumulative);
        self.value
    }

    fn current(&self) -> Option<Real> {
        self.value
    }

    fn reset(&mut self) {
        self.cumulative = 0.0;
        self.value = None;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn close_at_high_accumulates_full_volume() {
        let mut ad = Ad::new();
        // close == high: CLV = +1, so the whole volume is accumulated.
        assert_eq!(ad.update(Candle::new(10.0, 12.0, 8.0, 12.0, 100.0)), Some(100.0));
        // close == low: CLV = -1, distributes the whole volume back.
        assert_eq!(ad.update(Candle::new(12.0, 14.0, 10.0, 10.0, 50.0)), Some(50.0));
    }

    #[test]
    fn flat_bar_contributes_nothing() {
        let mut ad = Ad::new();
        assert_eq!(ad.update(Candle::new(10.0, 10.0, 10.0, 10.0, 100.0)), Some(0.0));
    }
}
