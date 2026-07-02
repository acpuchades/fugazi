use crate::indicator::Indicator;
use crate::types::{Candle, Real};

/// On-Balance Volume (OBV).
///
/// A bar indicator (consumes the full [`Candle`]). A running volume total that
/// adds the bar's volume on an up-close, subtracts it on a down-close, and
/// leaves it unchanged when the close is flat. Following TA-Lib, it seeds at the
/// first bar's volume, so it is ready immediately.
#[derive(Debug, Clone, Default)]
pub struct Obv {
    prev_close: Option<Real>,
    /// Latest OBV value; `None` before the first bar.
    pub value: Option<Real>,
}

impl Obv {
    pub fn new() -> Self {
        Self::default()
    }
}

impl Indicator for Obv {
    type Input = Candle;
    type Output = Real;

    fn update(&mut self, candle: Candle) -> Option<Real> {
        let obv = match (self.prev_close, self.value) {
            (Some(prev_close), Some(prev_obv)) => {
                if candle.close > prev_close {
                    prev_obv + candle.volume
                } else if candle.close < prev_close {
                    prev_obv - candle.volume
                } else {
                    prev_obv
                }
            }
            // First bar: there is no prior close to compare against, so OBV
            // seeds at the bar's own volume.
            _ => candle.volume,
        };
        self.prev_close = Some(candle.close);
        self.value = Some(obv);
        self.value
    }

    fn value(&self) -> Option<Real> {
        self.value
    }

    /// `1`; the cumulative total is *anchored*, not unstable — where it starts
    /// is part of its meaning, so [`unstable_period`](Indicator::unstable_period)
    /// stays `0`.
    fn warm_up_period(&self) -> usize {
        1
    }

    fn reset(&mut self) {
        self.prev_close = None;
        self.value = None;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn bar(close: Real, volume: Real) -> Candle {
        Candle::new(close, close, close, close, volume)
    }

    #[test]
    fn accumulates_on_up_and_down_closes() {
        let mut obv = Obv::new();
        assert_eq!(obv.update(bar(10.0, 100.0)), Some(100.0)); // seed
        assert_eq!(obv.update(bar(11.0, 50.0)), Some(150.0)); // up: +50
        assert_eq!(obv.update(bar(10.5, 40.0)), Some(110.0)); // down: -40
        assert_eq!(obv.update(bar(10.5, 30.0)), Some(110.0)); // flat: unchanged
    }
}
