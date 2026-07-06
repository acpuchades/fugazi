use crate::indicator::Indicator;
use crate::types::{Candle, Real};

/// True Range: `max(high - low, |high - prev_close|, |low - prev_close|)`.
///
/// The building block of [`Atr`](crate::indicators::Atr). Owns a candle source
/// so it composes with the rest of the indicator layer:
/// `TrueRange::new(Current::candle())` reads the base bar stream directly, but
/// wrapping [`Resample`](super::Resample) reads higher-timeframe bars instead.
/// On the first bar there is no previous close, so it falls back to
/// `high - low`. Ready as soon as the source is (typically the first bar).
#[derive(Debug, Clone)]
pub struct TrueRange<S> {
    source: S,
    prev_close: Option<Real>,
    /// Latest true range; `None` before the source produces its first candle.
    pub value: Option<Real>,
}

impl<S> TrueRange<S> {
    /// True Range of `source`'s candle stream.
    pub fn new(source: S) -> Self {
        Self {
            source,
            prev_close: None,
            value: None,
        }
    }
}

impl<S: Indicator<Output = Candle>> Indicator for TrueRange<S> {
    type Input = S::Input;
    type Output = Real;

    fn update(&mut self, input: S::Input) -> Option<Real> {
        let candle = self.source.update(input)?;
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

    fn warm_up_period(&self) -> usize {
        // Ready as soon as the source is (typically the first bar).
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
