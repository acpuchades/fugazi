use crate::indicator::Indicator;
use crate::indicators::stats::WindowStats;
use crate::types::{Candle, Real};

/// Money Flow Index (MFI): a volume-weighted RSI over the typical price.
///
/// A bar indicator (consumes candles from an owned source). Each bar's raw
/// money flow `typical * volume` — with typical price
/// `(high + low + close) / 3` — is classed as positive or negative by the move
/// in typical price, then `MFI = 100 - 100 / (1 + positive_flow / negative_flow)`
/// over the rolling `period` window (equivalently
/// `100 * positive / (positive + negative)`).
///
/// Reuses the shared [`WindowStats`] core to keep the positive/negative flow
/// sums in O(1). Produces `None` until `period + 1` bars have been seen — one to
/// seed the first typical-price move, then a full window of `period` flows.
#[derive(Debug, Clone)]
pub struct Mfi<S> {
    source: S,
    prev_typical: Option<Real>,
    positive: WindowStats,
    negative: WindowStats,
    /// Latest MFI value in `[0, 100]`; `None` until warmed up.
    pub value: Option<Real>,
}

impl<S> Mfi<S> {
    /// # Panics
    /// Panics if `period` is zero.
    pub fn new(source: S, period: usize) -> Self {
        Self {
            source,
            prev_typical: None,
            positive: WindowStats::new(period),
            negative: WindowStats::new(period),
            value: None,
        }
    }

    pub fn period(&self) -> usize {
        self.positive.period()
    }
}

impl<S: Indicator<Output = Candle>> Indicator for Mfi<S> {
    type Input = S::Input;
    type Output = Real;

    fn update(&mut self, input: S::Input) -> Option<Real> {
        let candle = self.source.update(input)?;
        let typical = candle.typical();
        let prev = match self.prev_typical {
            Some(prev) => prev,
            None => {
                // First bar: no prior typical price to classify the flow yet.
                self.prev_typical = Some(typical);
                self.value = None;
                return None;
            }
        };
        self.prev_typical = Some(typical);

        let flow = typical * candle.volume;
        // A flat typical price contributes to neither side. Both windows advance
        // in lockstep so they fill together.
        let (positive, negative) = if typical > prev {
            (flow, 0.0)
        } else if typical < prev {
            (0.0, flow)
        } else {
            (0.0, 0.0)
        };
        let full = self.positive.update(positive);
        self.negative.update(negative);

        self.value = full.then(|| {
            // Means share the window length, so their ratio is the ratio of the
            // positive and negative flow sums.
            let (pos, neg) = (self.positive.mean(), self.negative.mean());
            if neg == 0.0 {
                100.0
            } else {
                100.0 - 100.0 / (1.0 + pos / neg)
            }
        });
        self.value
    }

    fn value(&self) -> Option<Real> {
        self.value
    }

    fn warm_up_period(&self) -> usize {
        // One bar seeds the typical-price move, then a full window of flows.
        self.source.warm_up_period().max(1) + self.positive.period()
    }

    fn unstable_period(&self) -> usize {
        self.source.unstable_period()
    }

    fn reset(&mut self) {
        self.source.reset();
        self.prev_typical = None;
        self.positive.reset();
        self.negative.reset();
        self.value = None;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::indicators::Current;
    use crate::types::Candle;

    fn bar(typical: Real, volume: Real) -> Candle {
        // high = low = close = typical makes the typical price exactly `typical`.
        Candle::new(typical, typical, typical, typical, volume)
    }

    #[test]
    fn all_rising_flows_pin_to_100() {
        let mut mfi = Mfi::new(Current::candle(), 3);
        assert_eq!(mfi.update(bar(10.0, 100.0).into()), None); // seed
        assert_eq!(mfi.update(bar(11.0, 100.0).into()), None);
        assert_eq!(mfi.update(bar(12.0, 100.0).into()), None);
        // Third up-move fills the window; only positive flow -> MFI = 100.
        assert_eq!(mfi.update(bar(13.0, 100.0).into()), Some(100.0));
    }

    #[test]
    fn warms_up_after_period_plus_one() {
        let mut mfi = Mfi::new(Current::candle(), 2);
        assert_eq!(mfi.update(bar(10.0, 10.0).into()), None); // seeds prev typical
        assert_eq!(mfi.update(bar(11.0, 10.0).into()), None); // 1st flow
        assert!(mfi.update(bar(12.0, 10.0).into()).is_some()); // 2nd flow -> ready
    }
}
