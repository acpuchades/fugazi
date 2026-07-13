use crate::indicator::Indicator;
use crate::indicators::stats::WindowCovariance;
use crate::types::Real;

/// Rolling Pearson correlation between two Real sources over a fixed window.
///
/// Feeds the same input to both sources each step (hence `Input: Clone`) and
/// correlates their outputs over the last `period` samples via the shared
/// [`WindowCovariance`] core, so each update is O(1). Produces `None` until both
/// sources are warm *and* the window is full; once ready it reads in `[-1, 1]`,
/// with a dispersion-free leg (either source constant over the window) reading
/// `0.0` (correlation is undefined there).
///
/// One primitive, several regime features:
/// - **Cross-asset correlation** — `Correlation::new(Close::of(pick_a),
///   Close::of(pick_b), 30)`: is everything trading as one risk-on/risk-off
///   blob or dispersed.
/// - **Autocorrelation** — `Correlation::new(x.clone(), x.lag(n), period)`:
///   lag-`n` serial correlation, a trending-vs-mean-reverting signal.
/// - **Rolling beta** — `corr · σ_y / σ_x`, composed with [`StdDev`](super::StdDev),
///   no extra primitive needed.
#[derive(Debug, Clone)]
pub struct Correlation<L, R> {
    lhs: L,
    rhs: R,
    cov: WindowCovariance,
    /// Latest correlation; `None` until ready.
    pub value: Option<Real>,
}

impl<L, R> Correlation<L, R> {
    /// # Panics
    /// Panics if `period` is zero.
    pub fn new(lhs: L, rhs: R, period: usize) -> Self {
        Self {
            lhs,
            rhs,
            cov: WindowCovariance::new(period),
            value: None,
        }
    }

    pub fn period(&self) -> usize {
        self.cov.period()
    }
}

impl<L, R> Indicator for Correlation<L, R>
where
    L: Indicator<Output = Real>,
    R: Indicator<Input = L::Input, Output = Real>,
    L::Input: Clone,
{
    type Input = L::Input;
    type Output = Real;

    fn update(&mut self, input: Self::Input) -> Option<Real> {
        let x = self.lhs.update(input.clone());
        let y = self.rhs.update(input);
        self.value = match (x, y) {
            (Some(x), Some(y)) if self.cov.update(x, y) => Some(self.cov.correlation()),
            _ => None,
        };
        self.value
    }

    fn value(&self) -> Option<Real> {
        self.value
    }

    fn warm_up_period(&self) -> usize {
        // Both legs must be warm before the covariance window starts filling, so
        // the join point is the later of the two warm-ups; the window then needs
        // `period` more samples.
        self.lhs
            .warm_up_period()
            .max(self.rhs.warm_up_period())
            .max(1)
            + self.cov.period()
            - 1
    }

    fn unstable_period(&self) -> usize {
        self.lhs.unstable_period().max(self.rhs.unstable_period())
    }

    fn reset(&mut self) {
        self.lhs.reset();
        self.rhs.reset();
        self.cov.reset();
        self.value = None;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::indicators::{Identity, IndicatorExt, Sma, Value};

    #[test]
    fn perfectly_correlated_lines_read_one() {
        // y = x fed to both legs: correlation is exactly 1.
        let mut c = Correlation::new(Identity::new(), Identity::new(), 3);
        assert_eq!(c.update(1.0), None);
        assert_eq!(c.update(2.0), None);
        let out = c.update(3.0).unwrap();
        assert!((out - 1.0).abs() < 1e-12, "got {out}");
    }

    #[test]
    fn anti_correlated_lines_read_minus_one() {
        // rhs = -lhs (via `x * -1`), a perfectly negative relationship.
        let mut c = Correlation::new(Identity::new(), Identity::new().mul(Value::new(-1.0)), 3);
        c.update(1.0);
        c.update(2.0);
        let out = c.update(3.0).unwrap();
        assert!((out + 1.0).abs() < 1e-12, "got {out}");
    }

    #[test]
    fn constant_leg_reads_zero() {
        // rhs constant → its window variance is 0 → correlation undefined → 0.
        let mut c = Correlation::new(Identity::new(), Value::new(7.0), 3);
        c.update(1.0);
        c.update(2.0);
        assert_eq!(c.update(3.0), Some(0.0));
    }

    #[test]
    fn warm_up_accounts_for_both_legs_and_window() {
        // lhs SMA(2) warms at 2, window 3 → 2 + 3 − 1 = 4.
        let c = Correlation::new(Sma::new(Identity::new(), 2), Identity::new(), 3);
        assert_eq!(c.warm_up_period(), 4);
    }
}
