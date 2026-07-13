use crate::indicator::Indicator;
use crate::indicators::stats::WindowStats;
use crate::types::Real;

/// Rolling population kurtosis of a source over a fixed window.
///
/// The **raw** standardized fourth central moment, `mean((x - mean)^4) /
/// variance^2` — `3.0` for a normal window, *not* excess. This keeps the tag
/// un-opinionated (matching the small-composable-primitives philosophy): a
/// caller who wants excess kurtosis subtracts `3` themselves, e.g.
/// `Kurtosis::new(src, 20).sub(Value::new(3.0))`. Mean, variance and fourth
/// moment are all taken over the *same* `period` window.
///
/// Owns its input source: `Kurtosis::new(Current::close(), 20)`. Backed by the
/// shared [`WindowStats`] core. Produces `None` until the window is full; once
/// full, a dispersion-free window reads `0.0` (kurtosis is undefined without
/// spread). A fat-tailed / jump-prone window reads well above `3`.
#[derive(Debug, Clone)]
pub struct Kurtosis<S> {
    source: S,
    stats: WindowStats,
    /// Latest kurtosis; `None` until the window is full.
    pub value: Option<Real>,
}

impl<S> Kurtosis<S> {
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

impl<S: Indicator<Output = Real>> Indicator for Kurtosis<S> {
    type Input = S::Input;
    type Output = Real;

    fn update(&mut self, input: Self::Input) -> Option<Real> {
        self.value = match self.source.update(input) {
            Some(x) if self.stats.update(x) => Some(self.stats.kurtosis()),
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
    fn constant_window_reads_zero() {
        let mut ku = Kurtosis::new(Identity::new(), 3);
        ku.update(5.0);
        ku.update(5.0);
        assert_eq!(ku.update(5.0), Some(0.0));
    }

    #[test]
    fn known_population_kurtosis() {
        // Window {-1, 0, 1}: mean 0, m2 = (1+0+1)/3 = 2/3,
        // m4 = (1+0+1)/3 = 2/3, kurtosis = m4/m2^2 = (2/3)/(4/9) = 1.5
        let mut ku = Kurtosis::new(Identity::new(), 3);
        ku.update(-1.0);
        ku.update(0.0);
        let out = ku.update(1.0).unwrap();
        assert!((out - 1.5).abs() < 1e-12, "got {out}");
    }

    #[test]
    fn is_raw_not_excess() {
        // A two-point ±1 window has kurtosis 1 (raw). Excess would be −2; raw
        // must stay positive, confirming we don't subtract 3.
        let mut ku = Kurtosis::new(Identity::new(), 2);
        ku.update(-1.0);
        let out = ku.update(1.0).unwrap();
        assert!((out - 1.0).abs() < 1e-12, "got {out}");
    }
}
