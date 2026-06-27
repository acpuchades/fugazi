use crate::indicator::Indicator;
use crate::indicators::stats::WindowStats;
use crate::types::Real;

/// Scaling constant fixing CCI so ~70–80% of values fall in `[-100, 100]`.
const CCI_FACTOR: Real = 0.015;

/// Commodity Channel Index of a source over a fixed window.
///
/// `(x − SMA(x)) / (0.015 · mean_abs_dev(x))`, measuring how far the source sits
/// from its moving average in units of mean absolute deviation. Conventionally
/// the source is the typical price, so `Cci::new(Current::typical(), 20)` matches
/// TA-Lib's `CCI(high, low, close)`. A single [`WindowStats`] core supplies both
/// the mean and the dispersion; the deviation term scans the window, so each
/// update is O(period). When the window has zero dispersion it yields `0.0`.
/// Ready after `period` samples.
#[derive(Debug, Clone)]
pub struct Cci<S> {
    source: S,
    stats: WindowStats,
    /// Latest output value; `None` until the window is full.
    pub value: Option<Real>,
}

impl<S> Cci<S> {
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

impl<S: Indicator<Output = Real>> Indicator for Cci<S> {
    type Input = S::Input;
    type Output = Real;

    fn update(&mut self, input: Self::Input) -> Option<Real> {
        self.value = match self.source.update(input) {
            Some(x) if self.stats.update(x) => {
                let mad = self.stats.mean_abs_dev();
                Some(if mad == 0.0 {
                    0.0
                } else {
                    (x - self.stats.mean()) / (CCI_FACTOR * mad)
                })
            }
            _ => None,
        };
        self.value
    }

    fn value(&self) -> Option<Real> {
        self.value
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
    use crate::indicators::Identity;

    #[test]
    fn warms_up_then_measures_deviation() {
        let mut cci = Cci::new(Identity::new(), 3);
        assert_eq!(cci.update(1.0), None);
        assert_eq!(cci.update(2.0), None);
        // window [1,2,3]: mean 2, mad = (1+0+1)/3 = 2/3, last = 3.
        // (3 - 2) / (0.015 * 2/3) = 1 / 0.01 = 100.
        let out = cci.update(3.0).unwrap();
        assert!((out - 100.0).abs() < 1e-9);
    }

    #[test]
    fn flat_window_is_zero() {
        let mut cci = Cci::new(Identity::new(), 2);
        cci.update(5.0);
        assert_eq!(cci.update(5.0), Some(0.0));
    }
}
