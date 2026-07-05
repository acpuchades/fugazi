use crate::indicator::Indicator;
use crate::indicators::stats::WindowStats;
use crate::types::Real;

/// Rolling population standard deviation of a source over a fixed window.
///
/// Owns its input source: `StdDev::new(Current::close(), 20)`. Backed by the
/// shared [`WindowStats`] core, so each update is O(1). Produces `None` until
/// the window is full.
#[derive(Debug, Clone)]
pub struct StdDev<S> {
    source: S,
    stats: WindowStats,
    /// Latest standard deviation; `None` until the window is full.
    pub value: Option<Real>,
}

impl<S> StdDev<S> {
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

impl<S: Indicator<Output = Real>> Indicator for StdDev<S> {
    type Input = S::Input;
    type Output = Real;

    fn update(&mut self, input: Self::Input) -> Option<Real> {
        self.value = match self.source.update(input) {
            Some(x) if self.stats.update(x) => Some(self.stats.stddev()),
            _ => None,
        };
        self.value
    }

    fn value(&self) -> Option<Real> {
        self.value
    }

    fn warm_up_period(&self) -> usize {
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
    use crate::indicators::Identity;

    #[test]
    fn constant_series_has_zero_stddev() {
        let mut sd = StdDev::new(Identity::new(), 3);
        assert_eq!(sd.update(5.0), None);
        assert_eq!(sd.update(5.0), None);
        assert_eq!(sd.update(5.0), Some(0.0));
    }

    #[test]
    fn known_population_stddev() {
        let mut sd = StdDev::new(Identity::new(), 3);
        sd.update(2.0);
        sd.update(4.0);
        let out = sd.update(6.0).unwrap(); // mean 4, var (4+0+4)/3 = 8/3
        assert!((out - (8.0_f64 / 3.0).sqrt()).abs() < 1e-12);
    }
}
