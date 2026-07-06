use crate::indicator::Indicator;
use crate::types::{Candle, Real};

/// On-Balance Volume (OBV).
///
/// A bar indicator (consumes candles from an owned source). A running volume
/// total that adds the bar's volume on an up-close, subtracts it on a
/// down-close, and leaves it unchanged when the close is flat. Following
/// TA-Lib, it seeds at the first bar's volume, so it is ready as soon as the
/// source is.
#[derive(Debug, Clone)]
pub struct Obv<S> {
    source: S,
    prev_close: Option<Real>,
    /// Latest OBV value; `None` before the source produces its first bar.
    pub value: Option<Real>,
}

impl<S> Obv<S> {
    pub fn new(source: S) -> Self {
        Self {
            source,
            prev_close: None,
            value: None,
        }
    }
}

impl<S: Indicator<Output = Candle>> Indicator for Obv<S> {
    type Input = S::Input;
    type Output = Real;

    fn update(&mut self, input: S::Input) -> Option<Real> {
        let candle = self.source.update(input)?;
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

    /// Ready as soon as the source is; the cumulative total is *anchored*, not
    /// unstable — where it starts is part of its meaning, so
    /// [`unstable_period`](Indicator::unstable_period) stays `0`.
    fn warm_up_period(&self) -> usize {
        self.source.warm_up_period().max(1)
    }

    fn unstable_period(&self) -> usize {
        self.source.unstable_period()
    }

    fn reset(&mut self) {
        self.source.reset();
        self.prev_close = None;
        self.value = None;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::indicators::Current;
    use crate::types::Candle;

    fn bar(close: Real, volume: Real) -> Candle {
        Candle::new(close, close, close, close, volume)
    }

    #[test]
    fn accumulates_on_up_and_down_closes() {
        let mut obv = Obv::new(Current::candle());
        assert_eq!(obv.update(bar(10.0, 100.0).into()), Some(100.0)); // seed
        assert_eq!(obv.update(bar(11.0, 50.0).into()), Some(150.0)); // up: +50
        assert_eq!(obv.update(bar(10.5, 40.0).into()), Some(110.0)); // down: -40
        assert_eq!(obv.update(bar(10.5, 30.0).into()), Some(110.0)); // flat: unchanged
    }
}
