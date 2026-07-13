use std::collections::VecDeque;

use crate::indicator::Indicator;
use crate::indicators::stats::MOMENT_EPS;
use crate::types::Real;

/// Lo-MacKinlay variance-ratio test over a rolling window — a
/// trending-vs-mean-reverting regime classifier.
///
/// Reads `1.0` under the random-walk null; **`> 1.0` signals a trending
/// (positively autocorrelated) regime**, **`< 1.0` a mean-reverting one**. The
/// statistic is the ratio of the `lag`-period return variance to `lag` times the
/// one-period return variance: persistent moves inflate the longer-horizon
/// variance (ratio above one), while mean reversion damps it (ratio below one).
///
/// Owns its input source and works on the source's **first differences**, so it
/// is a pure transform over whatever real stream it wraps:
/// `VarianceRatio::new(Current::close(), 60, 5)` tests raw close changes;
/// `VarianceRatio::new(Log::natural(Current::close()), 60, 5)` tests log
/// returns (the usual choice — compose the log yourself, matching the crate's
/// composition-is-construction convention). Produces `None` until the window of
/// `period` samples is full; once full, a dispersion-free window (constant
/// one-period returns, e.g. a perfectly linear series) reads `1.0` — the
/// neutral, random-walk value — since the ratio is otherwise `0/0`.
///
/// ## Cost caveat — this is *not* O(1) per bar
///
/// Unlike the rest of the catalogue, the variance ratio does **not** reduce to a
/// cheap incremental recurrence. Lo-MacKinlay contrasts the dispersion of
/// overlapping `lag`-period returns against one-period returns across the whole
/// window, so this primitive retains the last `period` samples in a
/// [`VecDeque`] and **recomputes the statistic from scratch on every full-window
/// update** — O(`period`) work per bar, not O(1). That is fine for the intended
/// use case (an offline overlay fetch computing a regime feature column to join
/// against a strategy's windowed metrics) and acceptable for moderate windows
/// live, but it is a deliberate departure from the incremental design and is
/// called out here so callers size `period` with the cost in mind.
///
/// ## Estimator
///
/// With the window holding `period` observations `p[0..period]` (so `T =
/// period − 1` one-period returns) and `q = lag`, using the mean return
/// `μ = (p[T] − p[0]) / T`:
///
/// - one-period variance `σ²ₐ = (1/T) · Σ_{t=1..T} (p[t] − p[t−1] − μ)²`
/// - `q`-period variance (unbiased, overlapping)
///   `σ²𝒸 = (1/m) · Σ_{t=q..T} (p[t] − p[t−q] − q·μ)²`,
///   with the Lo-MacKinlay bias correction `m = q·(T − q + 1)·(1 − q/T)`
/// - `VR = σ²𝒸 / σ²ₐ`
///
/// The unbiased overlapping estimator needs at least two distinct `q`-blocks, so
/// `period` must exceed `lag` by at least two (`period ≥ lag + 2`).
#[derive(Debug, Clone)]
pub struct VarianceRatio<S> {
    source: S,
    window: VecDeque<Real>,
    period: usize,
    lag: usize,
    /// Latest variance ratio; `None` until the window is full.
    pub value: Option<Real>,
}

impl<S> VarianceRatio<S> {
    /// # Panics
    /// Panics if `lag < 2` (a lag-1 ratio is trivially the null), or if
    /// `period < lag + 2` (the overlapping estimator needs more than one
    /// `lag`-period block to have variance).
    pub fn new(source: S, period: usize, lag: usize) -> Self {
        assert!(lag >= 2, "variance_ratio lag must be at least 2");
        assert!(
            period >= lag + 2,
            "variance_ratio period must be at least lag + 2 (need >1 overlapping block)"
        );
        Self {
            source,
            window: VecDeque::with_capacity(period),
            period,
            lag,
            value: None,
        }
    }

    pub fn period(&self) -> usize {
        self.period
    }

    pub fn lag(&self) -> usize {
        self.lag
    }

    /// Recompute the Lo-MacKinlay variance ratio over the full retained window.
    /// Assumes the window is exactly `period` samples long.
    fn compute(&self) -> Real {
        let t = (self.period - 1) as Real; // one-period returns count
        let q = self.lag;
        let last = self.window[self.period - 1];
        let mu = (last - self.window[0]) / t;

        // One-period return variance (biased by 1/T, matching Lo-MacKinlay σ²ₐ).
        let mut var_a = 0.0;
        for i in 1..self.period {
            let r = self.window[i] - self.window[i - 1] - mu;
            var_a += r * r;
        }
        var_a /= t;
        if var_a < MOMENT_EPS {
            // No one-period dispersion (e.g. a perfectly linear window): the
            // ratio is 0/0. Report the random-walk null rather than NaN.
            return 1.0;
        }

        // Overlapping q-period return variance with the bias correction m.
        let mut num_c = 0.0;
        for i in q..self.period {
            let r = self.window[i] - self.window[i - q] - (q as Real) * mu;
            num_c += r * r;
        }
        let m = (q as Real) * (t - q as Real + 1.0) * (1.0 - q as Real / t);
        let var_c = num_c / m;

        var_c / var_a
    }
}

impl<S: Indicator<Output = Real>> Indicator for VarianceRatio<S> {
    type Input = S::Input;
    type Output = Real;

    fn update(&mut self, input: Self::Input) -> Option<Real> {
        self.value = match self.source.update(input) {
            Some(x) => {
                if self.window.len() == self.period {
                    self.window.pop_front();
                }
                self.window.push_back(x);
                if self.window.len() == self.period {
                    Some(self.compute())
                } else {
                    None
                }
            }
            None => None,
        };
        self.value
    }

    fn value(&self) -> Option<Real> {
        self.value
    }

    fn warm_up_period(&self) -> usize {
        self.source.warm_up_period().max(1) + self.period - 1
    }

    fn unstable_period(&self) -> usize {
        self.source.unstable_period()
    }

    fn reset(&mut self) {
        self.source.reset();
        self.window.clear();
        self.value = None;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::indicators::Identity;

    #[test]
    fn warms_up_on_the_full_window() {
        let mut vr = VarianceRatio::new(Identity::new(), 5, 2);
        assert_eq!(vr.update(0.0), None);
        assert_eq!(vr.update(1.0), None);
        assert_eq!(vr.update(3.0), None);
        assert_eq!(vr.update(4.0), None);
        assert!(vr.update(6.0).is_some());
    }

    #[test]
    fn mean_reverting_window_reads_below_one() {
        // Prices {0,1,3,4,6}: noisy 1-period returns {1,2,1,2} but constant
        // 2-period returns (all 3) → perfect mean reversion → VR = 0.
        let mut vr = VarianceRatio::new(Identity::new(), 5, 2);
        for p in [0.0, 1.0, 3.0, 4.0] {
            vr.update(p);
        }
        let out = vr.update(6.0).unwrap();
        assert!(out.abs() < 1e-12, "expected ~0, got {out}");
    }

    #[test]
    fn trending_window_reads_above_one() {
        // Prices {0,1,3,6,10}: accelerating (positively autocorrelated) returns.
        // σ²ₐ = 5/4, σ²𝒸 = 8/3 → VR = 32/15 ≈ 2.133.
        let mut vr = VarianceRatio::new(Identity::new(), 5, 2);
        for p in [0.0, 1.0, 3.0, 6.0] {
            vr.update(p);
        }
        let out = vr.update(10.0).unwrap();
        assert!(out > 1.0, "trending window should exceed 1, got {out}");
        assert!((out - 32.0 / 15.0).abs() < 1e-12, "got {out}");
    }

    #[test]
    fn constant_returns_read_the_null() {
        // A perfectly linear ramp has zero one-period dispersion → 0/0 → 1.0.
        let mut vr = VarianceRatio::new(Identity::new(), 6, 2);
        for p in [1.0, 2.0, 3.0, 4.0, 5.0] {
            vr.update(p);
        }
        assert_eq!(vr.update(6.0), Some(1.0));
    }

    #[test]
    #[should_panic(expected = "at least 2")]
    fn rejects_lag_below_two() {
        VarianceRatio::new(Identity::<Real>::new(), 10, 1);
    }

    #[test]
    #[should_panic(expected = "lag + 2")]
    fn rejects_period_too_small_for_lag() {
        VarianceRatio::new(Identity::<Real>::new(), 5, 4);
    }
}
