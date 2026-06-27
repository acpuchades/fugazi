use crate::indicator::Indicator;
use crate::types::{Candle, Real};

/// True Range: `max(high - low, |high - prev_close|, |low - prev_close|)`.
///
/// The building block of [`Atr`](crate::indicators::Atr). On the first bar
/// there is no previous close, so it falls back to `high - low`. Ready from the
/// first bar.
#[derive(Debug, Clone, Default)]
pub struct TrueRange {
    prev_close: Option<Real>,
    /// Latest true range; `None` before the first bar.
    pub value: Option<Real>,
}

impl TrueRange {
    pub fn new() -> Self {
        Self::default()
    }
}

impl Indicator for TrueRange {
    type Input = Candle;
    type Output = Real;

    fn update(&mut self, candle: Candle) -> Option<Real> {
        let tr = match self.prev_close {
            Some(prev_close) => {
                let high_low = candle.high - candle.low;
                let high_close = (candle.high - prev_close).abs();
                let low_close = (candle.low - prev_close).abs();
                high_low.max(high_close).max(low_close)
            }
            None => candle.high - candle.low,
        };
        self.prev_close = Some(candle.close);
        self.value = Some(tr);
        self.value
    }

    fn value(&self) -> Option<Real> {
        self.value
    }

    fn reset(&mut self) {
        self.prev_close = None;
        self.value = None;
    }
}
