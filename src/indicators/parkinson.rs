use crate::indicator::Indicator;
use crate::indicators::stats::WindowStats;
use crate::types::{Candle, Real};

/// Parkinson (1980) range-based volatility estimator over a fixed window.
///
/// Uses the high/low range only: `sqrt( (1 / (4·ln2)) · mean( ln(H/L)² ) )`,
/// the mean taken over `period` bars. Because it exploits the full intraday
/// range rather than just the close, it is statistically far more efficient
/// than a close-to-close standard deviation — but it assumes a zero-drift
/// continuous diffusion and so *under*-estimates when there are jumps or a
/// strong trend (see [`GarmanKlass`](super::GarmanKlass) /
/// [`RogersSatchell`](super::RogersSatchell) for OHLC estimators).
///
/// A bar indicator — it consumes candles from an owned source, so composition
/// is construction: `Parkinson::new(Current::candle(), 20)` is the 20-bar
/// estimator of the base stream. Backed by the shared [`WindowStats`] core;
/// O(1) per bar. Produces `None` until the window is full.
#[derive(Debug, Clone)]
pub struct Parkinson<S> {
    source: S,
    stats: WindowStats,
    /// Latest estimate; `None` until the window is full.
    pub value: Option<Real>,
}

/// `1 / (4·ln2)`, the Parkinson scaling constant.
const PARKINSON_FACTOR: Real = 1.0 / (4.0 * std::f64::consts::LN_2);

impl<S> Parkinson<S> {
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

impl<S: Indicator<Output = Candle>> Indicator for Parkinson<S> {
    type Input = S::Input;
    type Output = Real;

    fn update(&mut self, input: Self::Input) -> Option<Real> {
        self.value = match self.source.update(input) {
            Some(candle) => {
                let ln_hl = (candle.high / candle.low).ln();
                let full = self.stats.update(ln_hl * ln_hl);
                full.then(|| (PARKINSON_FACTOR * self.stats.mean()).max(0.0).sqrt())
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

    fn candle(high: Real, low: Real, close: Real) -> Candle {
        Candle::new(low, high, low, close, 0.0)
    }

    #[test]
    fn warms_up_after_period_bars() {
        let mut p = Parkinson::new(Current::candle(), 3);
        assert_eq!(p.update(candle(11.0, 9.0, 10.0).into()), None);
        assert_eq!(p.update(candle(12.0, 8.0, 11.0).into()), None);
        assert!(p.update(candle(13.0, 10.0, 12.0).into()).is_some());
        assert!(p.value.unwrap() > 0.0);
    }

    #[test]
    fn zero_range_reads_zero() {
        // High == Low on every bar → ln(H/L) = 0 → zero volatility.
        let mut p = Parkinson::new(Current::candle(), 2);
        p.update(candle(10.0, 10.0, 10.0).into());
        assert_eq!(p.update(candle(10.0, 10.0, 10.0).into()), Some(0.0));
    }

    #[test]
    fn known_value_single_window() {
        // A single-bar window with H/L = e: ln(H/L) = 1, so the estimate is
        // sqrt(1 / (4·ln2)).
        let mut p = Parkinson::new(Current::candle(), 1);
        let out = p
            .update(candle(std::f64::consts::E, 1.0, 1.0).into())
            .unwrap();
        assert!((out - PARKINSON_FACTOR.sqrt()).abs() < 1e-12);
    }
}
