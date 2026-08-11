//! [`Percentile`] and [`PercentileRank`]: rolling order statistics over a
//! source's own recent distribution.
//!
//! The adaptive-threshold primitive. A fixed RSI level of `70` means something
//! different in every regime; `!percentile { source: rsi, period: 252, pct: 0.8 }`
//! asks the series where *its own* 80th percentile is over the trailing year,
//! and moves with it. [`PercentileRank`] is the inverse question — not "what
//! value sits at 80%?" but "what percentage does today's value sit at?".

use fugazi_derive::SaveState;

use crate::indicator::Indicator;
use crate::indicators::stats::WindowQuantile;
use crate::types::Real;

/// The `pct`-quantile of a source over a fixed window — `0.5` is the rolling
/// median, `0.0` and `1.0` the window's min and max.
///
/// Interpolates linearly between the two bracketing samples (R's type-7,
/// `numpy`'s default), the same convention `fugazi::metrics`' `value_at_risk`
/// and `tail_ratio` use, so a percentile means one thing across the crate.
///
/// For the extremes specifically, [`RollingMax`](super::RollingMax) /
/// [`RollingMin`](super::RollingMin) are the O(1) alternatives — prefer them
/// over `pct: 1.0` / `pct: 0.0`.
///
/// Produces `None` until the window is full: a partial window's quantile is
/// meaningless for any `pct` between the extremes.
///
/// # Example
///
/// ```
/// use fugazi::prelude::*;
/// use fugazi::indicators::{Identity, Percentile};
///
/// // Rolling median of the last 3 samples.
/// let mut med = Percentile::new(Identity::new(), 3, 0.5);
/// assert_eq!(med.update(1.0), None);
/// assert_eq!(med.update(5.0), None);
/// assert_eq!(med.update(3.0), Some(3.0));
/// ```
#[derive(Debug, Clone, SaveState)]
pub struct Percentile<S> {
    #[state(source)]
    source: S,
    window: WindowQuantile,
    pct: Real,
    /// Latest quantile; `None` until the window is full.
    pub value: Option<Real>,
}

impl<S> Percentile<S> {
    /// # Panics
    /// Panics if `period` is zero, or if `pct` is outside `[0.0, 1.0]`.
    pub fn new(source: S, period: usize, pct: Real) -> Self {
        assert!(
            (0.0..=1.0).contains(&pct),
            "percentile pct must lie in [0.0, 1.0], got {pct}",
        );
        Self {
            source,
            window: WindowQuantile::new(period),
            pct,
            value: None,
        }
    }

    pub fn period(&self) -> usize {
        self.window.period()
    }

    pub fn pct(&self) -> Real {
        self.pct
    }
}

impl<S: Indicator<Output = Real>> Indicator for Percentile<S> {
    type Input = S::Input;
    type Output = Real;

    fn update(&mut self, input: Self::Input) -> Option<Real> {
        self.value = match self.source.update(input) {
            Some(x) if self.window.update(x) => Some(self.window.quantile(self.pct)),
            _ => None,
        };
        self.value
    }

    fn value(&self) -> Option<Real> {
        self.value
    }

    fn warm_up_period(&self) -> usize {
        self.source.warm_up_period().max(1) + self.window.period() - 1
    }

    fn unstable_period(&self) -> usize {
        self.source.unstable_period()
    }

    fn reset(&mut self) {
        self.source.reset();
        self.window.reset();
        self.value = None;
    }

    fn save_state(&self) -> serde_json::Value {
        self.save_state_fields()
    }

    fn load_state(&mut self, state: &serde_json::Value) -> Result<(), String> {
        self.load_state_fields(state)
    }
}

/// Where the current sample sits within its own trailing distribution, as
/// `count(v <= x) / period` in `(0, 1]`.
///
/// The current sample is part of its own window, so a fresh high reads exactly
/// `1.0` ("at or above everything in the window") and a fresh low reads
/// `1/period`, never `0.0`. Dividing by `period` rather than `period - 1` is
/// what makes `1.0` mean "at or above all of them" instead of "tied for
/// highest".
///
/// The natural cross-sectional score: `!percentile_rank { source: !roc { period: 20 }, period: 252 }`
/// ranks each symbol's momentum against its own history rather than against the
/// other symbols, so assets of different volatility compare fairly.
///
/// Produces `None` until the window is full.
///
/// # Example
///
/// ```
/// use fugazi::prelude::*;
/// use fugazi::indicators::{Identity, PercentileRank};
///
/// let mut rank = PercentileRank::new(Identity::new(), 4);
/// rank.update(10.0);
/// rank.update(20.0);
/// rank.update(30.0);
/// // 25 is above 10 and 20, plus itself: 3 of 4.
/// assert_eq!(rank.update(25.0), Some(0.75));
/// ```
#[derive(Debug, Clone, SaveState)]
pub struct PercentileRank<S> {
    #[state(source)]
    source: S,
    window: WindowQuantile,
    /// Latest rank in `(0, 1]`; `None` until the window is full.
    pub value: Option<Real>,
}

impl<S> PercentileRank<S> {
    /// # Panics
    /// Panics if `period` is zero.
    pub fn new(source: S, period: usize) -> Self {
        Self {
            source,
            window: WindowQuantile::new(period),
            value: None,
        }
    }

    pub fn period(&self) -> usize {
        self.window.period()
    }
}

impl<S: Indicator<Output = Real>> Indicator for PercentileRank<S> {
    type Input = S::Input;
    type Output = Real;

    fn update(&mut self, input: Self::Input) -> Option<Real> {
        self.value = match self.source.update(input) {
            Some(x) if self.window.update(x) => Some(self.window.rank_of(x)),
            _ => None,
        };
        self.value
    }

    fn value(&self) -> Option<Real> {
        self.value
    }

    fn warm_up_period(&self) -> usize {
        self.source.warm_up_period().max(1) + self.window.period() - 1
    }

    fn unstable_period(&self) -> usize {
        self.source.unstable_period()
    }

    fn reset(&mut self) {
        self.source.reset();
        self.window.reset();
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

    fn feed(ind: &mut impl Indicator<Input = Real, Output = Real>, xs: &[Real]) -> Option<Real> {
        let mut last = None;
        for &x in xs {
            last = ind.update(x);
        }
        last
    }

    #[test]
    fn median_of_an_odd_window_is_the_middle_sample() {
        let mut med = Percentile::new(Identity::new(), 3, 0.5);
        assert_eq!(med.update(1.0), None);
        assert_eq!(med.update(5.0), None);
        assert_eq!(med.update(3.0), Some(3.0));
    }

    #[test]
    fn median_of_an_even_window_interpolates_the_two_middles() {
        let mut med = Percentile::new(Identity::new(), 4, 0.5);
        // Sorted [1, 2, 3, 4] -> type-7 median is 2.5.
        assert_eq!(feed(&mut med, &[1.0, 2.0, 3.0, 4.0]), Some(2.5));
    }

    #[test]
    fn the_extremes_agree_with_min_and_max() {
        let xs = [4.0, 1.0, 9.0, 7.0, 2.0];
        let mut lo = Percentile::new(Identity::new(), 5, 0.0);
        let mut hi = Percentile::new(Identity::new(), 5, 1.0);
        assert_eq!(feed(&mut lo, &xs), Some(1.0));
        assert_eq!(feed(&mut hi, &xs), Some(9.0));
    }

    #[test]
    fn matches_the_metrics_percentile_convention() {
        // The whole point of sharing `quantile_of_sorted`: the same numbers
        // come out of the rolling indicator and the report-level helper.
        let xs = [10.0, 20.0, 30.0, 40.0];
        for pct in [0.0, 0.1, 0.25, 0.5, 0.75, 0.9, 1.0] {
            let mut p = Percentile::new(Identity::new(), 4, pct);
            let got = feed(&mut p, &xs).unwrap();
            let want = crate::indicators::stats::quantile_of_sorted(&xs, pct);
            assert!((got - want).abs() < 1e-12, "pct {pct}: {got} vs {want}");
        }
    }

    #[test]
    fn a_single_sample_window_is_that_sample_for_every_pct() {
        for pct in [0.0, 0.5, 1.0] {
            let mut p = Percentile::new(Identity::new(), 1, pct);
            assert_eq!(p.update(7.0), Some(7.0));
        }
    }

    #[test]
    fn the_window_rolls_forward() {
        let mut med = Percentile::new(Identity::new(), 3, 0.5);
        feed(&mut med, &[1.0, 2.0, 3.0]);
        assert_eq!(med.value(), Some(2.0));
        // 1.0 falls out, 100.0 comes in: window is [2, 3, 100].
        assert_eq!(med.update(100.0), Some(3.0));
    }

    #[test]
    fn nan_in_the_window_does_not_panic() {
        let mut med = Percentile::new(Identity::new(), 3, 0.5);
        med.update(1.0);
        med.update(Real::NAN);
        assert!(med.update(3.0).is_some());
    }

    #[test]
    #[should_panic(expected = "percentile pct must lie in [0.0, 1.0]")]
    fn rejects_an_out_of_range_pct() {
        let _ = Percentile::new(Identity::<Real>::new(), 3, 1.5);
    }

    #[test]
    fn rank_counts_the_current_sample_itself() {
        let mut rank = PercentileRank::new(Identity::new(), 4);
        feed(&mut rank, &[10.0, 20.0, 30.0]);
        assert_eq!(rank.update(25.0), Some(0.75));
    }

    #[test]
    fn a_fresh_high_ranks_one_and_a_fresh_low_ranks_one_over_period() {
        let mut rank = PercentileRank::new(Identity::new(), 4);
        assert_eq!(feed(&mut rank, &[1.0, 2.0, 3.0, 99.0]), Some(1.0));
        assert_eq!(rank.update(-99.0), Some(0.25));
    }

    #[test]
    fn ties_count_as_at_or_below() {
        let mut rank = PercentileRank::new(Identity::new(), 4);
        // Window [5, 5, 5, 5]: every sample is at-or-below the current one.
        assert_eq!(feed(&mut rank, &[5.0, 5.0, 5.0, 5.0]), Some(1.0));
    }

    #[test]
    fn warm_up_is_the_window_over_a_ready_source() {
        let p = || Percentile::new(Identity::<Real>::new(), 20, 0.8);
        let r = || PercentileRank::new(Identity::<Real>::new(), 20);
        assert_eq!(p().warm_up_period(), 20);
        assert_eq!(r().warm_up_period(), 20);
        assert_eq!(p().unstable_period(), 0);
    }

    #[test]
    fn reset_clears_the_window() {
        let mut med = Percentile::new(Identity::new(), 3, 0.5);
        feed(&mut med, &[1.0, 2.0, 3.0]);
        med.reset();
        assert_eq!(med.value(), None);
        assert_eq!(med.update(9.0), None);
    }
}
