use crate::indicator::Indicator;
use crate::indicators::stats::WindowStats;
use crate::types::{Candle, Real};

/// Garman–Klass (1980) range-based volatility estimator over a fixed window.
///
/// Uses the full OHLC bar:
/// `sqrt( mean( 0.5·ln(H/L)² − (2·ln2 − 1)·ln(C/O)² ) )`, the mean taken over
/// `period` bars. Combining the high/low range with the open/close move makes
/// it more efficient than the range-only [`Parkinson`](super::Parkinson)
/// estimator, but like Parkinson it assumes zero drift and no overnight gaps —
/// its per-bar term can even go slightly negative on a large open→close move
/// inside a tight range, so the windowed mean is clamped to non-negative before
/// the square root. See [`RogersSatchell`](super::RogersSatchell) for a
/// drift-independent alternative.
///
/// A bar indicator — it consumes candles from an owned source, so composition
/// is construction: `GarmanKlass::new(Current::candle(), 20)` is the 20-bar
/// estimator of the base stream. Backed by the shared [`WindowStats`] core;
/// O(1) per bar. Produces `None` until the window is full.
#[derive(Debug, Clone)]
pub struct GarmanKlass<S> {
    source: S,
    stats: WindowStats,
    /// Latest estimate; `None` until the window is full.
    pub value: Option<Real>,
}

/// `2·ln2 − 1`, the Garman–Klass open/close coefficient.
const GK_CLOSE_COEFF: Real = 2.0 * std::f64::consts::LN_2 - 1.0;

impl<S> GarmanKlass<S> {
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

impl<S: Indicator<Output = Candle>> Indicator for GarmanKlass<S> {
    type Input = S::Input;
    type Output = Real;

    fn update(&mut self, input: Self::Input) -> Option<Real> {
        self.value = match self.source.update(input) {
            Some(candle) => {
                let ln_hl = (candle.high / candle.low).ln();
                let ln_co = (candle.close / candle.open).ln();
                let term = 0.5 * ln_hl * ln_hl - GK_CLOSE_COEFF * ln_co * ln_co;
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
        let mut gk = GarmanKlass::new(Current::candle(), 3);
        assert_eq!(gk.update(candle(10.0, 11.0, 9.0, 10.5).into()), None);
        assert_eq!(gk.update(candle(10.5, 12.0, 8.0, 11.0).into()), None);
        assert!(gk.update(candle(11.0, 13.0, 10.0, 12.0).into()).is_some());
        assert!(gk.value.unwrap() > 0.0);
    }

    #[test]
    fn flat_bar_reads_zero() {
        // OHLC all equal → both log terms vanish.
        let mut gk = GarmanKlass::new(Current::candle(), 2);
        gk.update(candle(10.0, 10.0, 10.0, 10.0).into());
        assert_eq!(gk.update(candle(10.0, 10.0, 10.0, 10.0).into()), Some(0.0));
    }
}
