use crate::indicator::Indicator;
use crate::indicators::stats::WindowStats;
use crate::types::Real;

/// Rolling population skewness of a source over a fixed window.
///
/// The standardized third central moment, `mean((x - mean)^3) / stddev^3`, with
/// mean, stddev and third moment all taken over the *same* `period` window (a
/// genuine fixed-window statistic, not a causal running-mean approximation).
/// Owns its input source: `Skewness::new(Current::close(), 20)`. Backed by the
/// shared [`WindowStats`] core. Produces `None` until the window is full; once
/// full, a dispersion-free window reads `0.0` (skewness is undefined without
/// spread).
///
/// A negatively-skewed window (crash-skewed returns: occasional large drops)
/// reads negative; a positively-skewed one (rally-skewed) reads positive.
#[derive(Debug, Clone)]
pub struct Skewness<S> {
    source: S,
    stats: WindowStats,
    /// Latest skewness; `None` until the window is full.
    pub value: Option<Real>,
}

impl<S> Skewness<S> {
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

impl<S: Indicator<Output = Real>> Indicator for Skewness<S> {
    type Input = S::Input;
    type Output = Real;

    fn update(&mut self, input: Self::Input) -> Option<Real> {
        self.value = match self.source.update(input) {
            Some(x) if self.stats.update(x) => Some(self.stats.skewness()),
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
    fn symmetric_window_has_zero_skew() {
        // A symmetric window {2, 4, 6} about its mean (4) is skew-free.
        let mut sk = Skewness::new(Identity::new(), 3);
        assert_eq!(sk.update(2.0), None);
        assert_eq!(sk.update(4.0), None);
        let out = sk.update(6.0).unwrap();
        assert!(out.abs() < 1e-12, "symmetric window should be ~0, got {out}");
    }

    #[test]
    fn constant_window_reads_zero() {
        let mut sk = Skewness::new(Identity::new(), 3);
        sk.update(5.0);
        sk.update(5.0);
        assert_eq!(sk.update(5.0), Some(0.0));
    }

    #[test]
    fn right_tail_is_positive() {
        // {0, 0, 3}: a mass at the low end with one high outlier — positive skew.
        let mut sk = Skewness::new(Identity::new(), 3);
        sk.update(0.0);
        sk.update(0.0);
        let out = sk.update(3.0).unwrap();
        assert!(out > 0.0, "right-tailed window should be positive, got {out}");
    }

    #[test]
    fn known_population_skewness() {
        // Window {0, 0, 3}: mean 1, m2 = (1+1+4)/3 = 2, m3 = (-1-1+8)/3 = 2,
        // skew = 2 / 2^1.5 = 0.70710678…
        let mut sk = Skewness::new(Identity::new(), 3);
        sk.update(0.0);
        sk.update(0.0);
        let out = sk.update(3.0).unwrap();
        assert!((out - 2.0 / 2f64.powf(1.5)).abs() < 1e-12);
    }
}
