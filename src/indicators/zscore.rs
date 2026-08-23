use fugazi_derive::SaveState;

use crate::indicator::Indicator;
use crate::indicators::stats::{MOMENT_EPS, WindowStats};
use crate::types::Real;

/// Rolling z-score of a source over a fixed window: how many standard
/// deviations the current sample sits from its windowed mean,
/// `(x - SMA(x, period)) / stddev(x, period)`.
///
/// Owns its input source: `ZScore::new(Current::close(), 20)`. Backed by the
/// shared `WindowStats` core (one window supplies both the mean and the
/// dispersion): O(1) to update, one centred O(period) pass to read the
/// dispersion. Produces `None` until the window is
/// full; once full, a dispersion-free window reads `0.0` (the sample is exactly
/// the mean, and the z-score is otherwise undefined).
///
/// This is the standard per-series normalization every cross-sectional feature
/// needs before pooling across instruments — one transform tag in place of the
/// hand-written `!div { !sub { x, !sma {…} }, !stddev {…} }`.
#[derive(Debug, Clone, SaveState)]
pub struct ZScore<S> {
    #[state(source)]
    source: S,
    #[state(core)]
    stats: WindowStats,
    /// Latest z-score; `None` until the window is full.
    pub value: Option<Real>,
}

impl<S> ZScore<S> {
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

impl<S: Indicator<Output = Real>> Indicator for ZScore<S> {
    type Input = S::Input;
    type Output = Real;

    fn update(&mut self, input: Self::Input) -> Option<Real> {
        self.value = match self.source.update(input) {
            Some(x) if self.stats.update(x) => {
                // One pass for both: `variance` centres on the mean, so asking
                // for the mean separately divides for it twice.
                let (mean, var) = self.stats.mean_and_variance();
                if var < MOMENT_EPS {
                    Some(0.0)
                } else {
                    Some((x - mean) / var.sqrt())
                }
            }
            _ => None,
        };
        self.value
    }

    fn value(&self) -> Option<Real> {
        self.value
    }

    fn warm_up_bars(&self) -> usize {
        self.source.warm_up_bars().max(1) + self.stats.period() - 1
    }

    fn unstable_bars(&self) -> usize {
        self.source.unstable_bars()
    }

    fn reset(&mut self) {
        self.source.reset();
        self.stats.reset();
        self.value = None;
    }

    fn save_state(&self) -> serde_json::Value {
        self.save_state_fields()
    }

    fn load_state(&mut self, state: &serde_json::Value) -> Result<(), String> {
        self.load_state_fields(state)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::indicators::Identity;

    #[test]
    fn constant_window_reads_zero() {
        let mut z = ZScore::new(Identity::new(), 3);
        z.update(5.0);
        z.update(5.0);
        assert_eq!(z.update(5.0), Some(0.0));
    }

    #[test]
    fn known_zscore() {
        // Window {2, 4, 6}: mean 4, population stddev sqrt(8/3). The latest
        // sample 6 sits (6-4)/sqrt(8/3) = 2/sqrt(8/3) above the mean.
        let mut z = ZScore::new(Identity::new(), 3);
        z.update(2.0);
        z.update(4.0);
        let out = z.update(6.0).unwrap();
        let expected = 2.0 / (8.0_f64 / 3.0).sqrt();
        assert!((out - expected).abs() < 1e-12, "got {out}");
    }

    #[test]
    fn negative_when_below_mean() {
        // Latest sample is the window minimum → below the mean → negative.
        let mut z = ZScore::new(Identity::new(), 3);
        z.update(6.0);
        z.update(4.0);
        let out = z.update(2.0).unwrap();
        assert!(out < 0.0, "below-mean sample should be negative, got {out}");
    }
}
