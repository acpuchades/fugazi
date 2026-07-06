use crate::indicator::Indicator;
use crate::indicators::ops::{MaxOp, MinOp};
use crate::indicators::stats::WindowExtreme;
use crate::types::{Candle, Real};

/// Williams %R over a fixed window.
///
/// A bar indicator (consumes candles from an owned source): it relates the
/// close to the high/low range of the last `period` bars,
/// `-100·(highest_high − close)/(highest_high − lowest_low)`, in `[-100, 0]`.
/// It is the stochastic %K mirrored onto a downward scale, so the highest high
/// and lowest low share the same rolling-extremum core as
/// [`Stochastic`](super::Stochastic). When the window is flat
/// (`highest_high == lowest_low`) it yields `0.0`. Ready after `period` bars.
#[derive(Debug, Clone)]
pub struct WilliamsR<S> {
    source: S,
    highest: WindowExtreme<MaxOp>,
    lowest: WindowExtreme<MinOp>,
    /// Latest %R value in `[-100, 0]`; `None` until the window is full.
    pub value: Option<Real>,
}

impl<S> WilliamsR<S> {
    /// # Panics
    /// Panics if `period` is zero.
    pub fn new(source: S, period: usize) -> Self {
        Self {
            source,
            highest: WindowExtreme::new(period),
            lowest: WindowExtreme::new(period),
            value: None,
        }
    }

    pub fn period(&self) -> usize {
        self.highest.period()
    }
}

impl<S: Indicator<Output = Candle>> Indicator for WilliamsR<S> {
    type Input = S::Input;
    type Output = Real;

    fn update(&mut self, input: S::Input) -> Option<Real> {
        let candle = self.source.update(input)?;
        self.value = match (
            self.highest.update(candle.high),
            self.lowest.update(candle.low),
        ) {
            (Some(hh), Some(ll)) => {
                let range = hh - ll;
                Some(if range == 0.0 {
                    0.0
                } else {
                    -100.0 * (hh - candle.close) / range
                })
            }
            _ => None,
        };
        self.value
    }

    fn value(&self) -> Option<Real> {
        self.value
    }

    fn warm_up_period(&self) -> usize {
        self.source.warm_up_period().max(1) + self.highest.period() - 1
    }

    fn unstable_period(&self) -> usize {
        self.source.unstable_period()
    }

    fn reset(&mut self) {
        self.source.reset();
        self.highest.reset();
        self.lowest.reset();
        self.value = None;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::indicators::Current;
    use crate::types::Candle;

    fn bar(high: Real, low: Real, close: Real) -> Candle {
        Candle::new(low, high, low, close, 0.0)
    }

    #[test]
    fn positions_close_within_recent_range() {
        let mut wr = WilliamsR::new(Current::candle(), 3);
        assert_eq!(wr.update(bar(10.0, 8.0, 9.0).into()), None);
        assert_eq!(wr.update(bar(11.0, 9.0, 10.0).into()), None);
        // window highs [10,11,12], lows [8,9,10]; hh=12, ll=8, close=12 -> 0.
        assert_eq!(wr.update(bar(12.0, 10.0, 12.0).into()), Some(0.0));
        // hh=12, ll=8, close=8 -> -100*(12-8)/(12-8) = -100.
        assert_eq!(wr.update(bar(11.0, 8.0, 8.0).into()), Some(-100.0));
    }
}
