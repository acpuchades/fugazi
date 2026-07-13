use crate::indicator::Indicator;
use crate::indicators::stats::WindowStats;
use crate::types::{Candle, Real};

/// Rogers–Satchell (1991) range-based volatility estimator over a fixed window.
///
/// Uses the full OHLC bar:
/// `sqrt( mean( ln(H/C)·ln(H/O) + ln(L/C)·ln(L/O) ) )`, the mean taken over
/// `period` bars. Its defining property is **drift-independence** — unlike
/// [`Parkinson`](super::Parkinson) and [`GarmanKlass`](super::GarmanKlass) it
/// stays unbiased when the price trends within the bar, because each term pairs
/// a high/low log with the matching close/open log. Every per-bar term is
/// non-negative by construction (`H ≥ C, O` and `L ≤ C, O`), so no clamping is
/// needed.
///
/// A bar indicator — it consumes candles from an owned source, so composition
/// is construction: `RogersSatchell::new(Current::candle(), 20)` is the 20-bar
/// estimator of the base stream. Backed by the shared [`WindowStats`] core;
/// O(1) per bar. Produces `None` until the window is full.
#[derive(Debug, Clone)]
pub struct RogersSatchell<S> {
    source: S,
    stats: WindowStats,
    /// Latest estimate; `None` until the window is full.
    pub value: Option<Real>,
}

impl<S> RogersSatchell<S> {
    /// # Panics
    /// Panics if `period` is zero.
    pub fn new(source: S, period: usize) -> Self {
        Self {
            source,
            stats: WindowStats::new(period),
            value: None,
        }
    }

    pub fn period(&self) -> usize {
        self.stats.period()
    }
}

impl<S: Indicator<Output = Candle>> Indicator for RogersSatchell<S> {
    type Input = S::Input;
    type Output = Real;

    fn update(&mut self, input: Self::Input) -> Option<Real> {
        self.value = match self.source.update(input) {
            Some(candle) => {
                let ln_hc = (candle.high / candle.close).ln();
                let ln_ho = (candle.high / candle.open).ln();
                let ln_lc = (candle.low / candle.close).ln();
                let ln_lo = (candle.low / candle.open).ln();
                let term = ln_hc * ln_ho + ln_lc * ln_lo;
                let full = self.stats.update(term);
                full.then(|| self.stats.mean().max(0.0).sqrt())
            }
            None => None,
        };
        self.value
    }

    fn value(&self) -> Option<Real> {
        self.value
    }

    fn warm_up_period(&self) -> usize {
        // The per-bar estimate is ready as soon as the source produces a candle;
        // the rolling window then consumes a full `period` of them.
        self.source.warm_up_period().max(1) + self.stats.period() - 1
    }

    fn unstable_period(&self) -> usize {
        self.source.unstable_period()
    }

    fn reset(&mut self) {
        self.source.reset();
        self.stats.reset();
        self.value = None;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::indicators::Current;

    fn candle(open: Real, high: Real, low: Real, close: Real) -> Candle {
        Candle::new(open, high, low, close, 0.0)
    }

    #[test]
    fn warms_up_after_period_bars() {
        let mut rs = RogersSatchell::new(Current::candle(), 3);
        assert_eq!(rs.update(candle(10.0, 11.0, 9.0, 10.5).into()), None);
        assert_eq!(rs.update(candle(10.5, 12.0, 8.0, 11.0).into()), None);
        assert!(rs.update(candle(11.0, 13.0, 10.0, 12.0).into()).is_some());
        assert!(rs.value.unwrap() > 0.0);
    }

    #[test]
    fn flat_bar_reads_zero() {
        // OHLC all equal → every log term vanishes.
        let mut rs = RogersSatchell::new(Current::candle(), 2);
        rs.update(candle(10.0, 10.0, 10.0, 10.0).into());
        assert_eq!(rs.update(candle(10.0, 10.0, 10.0, 10.0).into()), Some(0.0));
    }
}
