//! Internal rolling-window statistics core shared by the windowed indicators.
//!
//! Maintains the last `period` samples plus running sum and sum-of-squares, so
//! `mean` and (population) `variance`/`stddev` are O(1) per update. Embedded by
//! [`Sma`](super::Sma), [`StdDev`](super::StdDev) and
//! [`Bollinger`](super::Bollinger) — anything needing a moving average and/or
//! dispersion over the same window.

use std::collections::VecDeque;
use std::marker::PhantomData;

use serde::{Deserialize, Serialize};

use crate::indicators::ops::ExtremeOp;
use crate::types::Real;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub(crate) struct WindowStats {
    period: usize,
    window: VecDeque<Real>,
    sum: Real,
    sum_sq: Real,
}

impl WindowStats {
    pub fn new(period: usize) -> Self {
        assert!(period > 0, "window period must be greater than zero");
        Self {
            period,
            window: VecDeque::with_capacity(period),
            sum: 0.0,
            sum_sq: 0.0,
        }
    }

    pub fn period(&self) -> usize {
        self.period
    }

    /// Push a sample, evicting the oldest once the window is full. Returns
    /// whether the window is now full (i.e. statistics are valid).
    pub fn update(&mut self, x: Real) -> bool {
        self.window.push_back(x);
        self.sum += x;
        self.sum_sq += x * x;
        if self.window.len() > self.period {
            let old = self.window.pop_front().expect("window is non-empty");
            self.sum -= old;
            self.sum_sq -= old * old;
        }
        self.is_full()
    }

    pub fn is_full(&self) -> bool {
        self.window.len() == self.period
    }

    /// Mean over the window. Only meaningful once [`is_full`](Self::is_full).
    pub fn mean(&self) -> Real {
        self.sum / self.period as Real
    }

    /// Population variance over the window (clamped to non-negative against
    /// floating-point round-off).
    pub fn variance(&self) -> Real {
        let n = self.period as Real;
        let mean = self.sum / n;
        (self.sum_sq / n - mean * mean).max(0.0)
    }

    /// Population standard deviation over the window.
    pub fn stddev(&self) -> Real {
        self.variance().sqrt()
    }

    /// **Sample** (`n − 1` divisor) standard deviation over the window — the
    /// form [`metrics::stddev_return`](crate::metrics::stddev_return) uses,
    /// whereas [`stddev`](Self::stddev) is the population (`n` divisor) form
    /// backing [`StdDev`](super::StdDev)/[`Bollinger`](super::Bollinger). The
    /// trailing risk indicators ([`Sharpe`](super::Sharpe) /
    /// [`Volatility`](super::Volatility)) use this so a full-window reading
    /// equals the whole-run [`metrics`](crate::metrics) number. Returns `0.0`
    /// for `period < 2` (sample variance is undefined with one sample). Only
    /// meaningful once [`is_full`](Self::is_full).
    pub fn sample_stddev(&self) -> Real {
        if self.period < 2 {
            return 0.0;
        }
        let n = self.period as Real;
        (self.variance() * n / (n - 1.0)).sqrt()
    }

    /// Downside deviation about `threshold`: `sqrt(mean(min(x − threshold, 0)²))`
    /// with an `n` divisor, scanning the retained window (O(period), like
    /// [`mean_abs_dev`](Self::mean_abs_dev)). Matches
    /// [`metrics`](crate::metrics)' `downside_stddev` (empyrical's
    /// `downside_risk`), so it backs the rolling [`Sortino`](super::Sortino).
    /// Only meaningful once [`is_full`](Self::is_full).
    pub fn downside_dev(&self, threshold: Real) -> Real {
        let sum_sq: Real = self
            .window
            .iter()
            .map(|x| (x - threshold).min(0.0).powi(2))
            .sum();
        (sum_sq / self.period as Real).sqrt()
    }

    /// Mean absolute deviation about the window mean, `mean(|x - mean|)`. Unlike
    /// `mean`/`variance` this scans the retained window (O(period)); used by
    /// [`Cci`](super::Cci). Only meaningful once [`is_full`](Self::is_full).
    pub fn mean_abs_dev(&self) -> Real {
        let mean = self.mean();
        let sum: Real = self.window.iter().map(|x| (x - mean).abs()).sum();
        sum / self.period as Real
    }

    /// Population skewness: the standardized third central moment
    /// `mean((x - mean)^3) / stddev^3`. Like [`mean_abs_dev`](Self::mean_abs_dev)
    /// this scans the retained window (O(period)) from the window mean, so the
    /// three moments share one exact pass rather than a running approximation.
    /// Returns `0.0` for a dispersion-free window (variance below
    /// [`MOMENT_EPS`]), matching how [`variance`](Self::variance)/[`stddev`](Self::stddev)
    /// degrade gracefully. Only meaningful once [`is_full`](Self::is_full).
    pub fn skewness(&self) -> Real {
        let (m2, m3, _m4) = self.central_moments();
        if m2 < MOMENT_EPS {
            return 0.0;
        }
        m3 / m2.powf(1.5)
    }

    /// Population kurtosis: the **raw** standardized fourth central moment
    /// `mean((x - mean)^4) / variance^2` — `3.0` for a normal window, *not*
    /// excess (a caller subtracts `3` for excess kurtosis). Same single-pass
    /// window scan as [`skewness`](Self::skewness); returns `0.0` for a
    /// dispersion-free window. Only meaningful once [`is_full`](Self::is_full).
    pub fn kurtosis(&self) -> Real {
        let (m2, _m3, m4) = self.central_moments();
        if m2 < MOMENT_EPS {
            return 0.0;
        }
        m4 / (m2 * m2)
    }

    /// The 2nd/3rd/4th central moments over the window in one pass:
    /// `(mean((x-μ)^2), mean((x-μ)^3), mean((x-μ)^4))`.
    fn central_moments(&self) -> (Real, Real, Real) {
        let mean = self.mean();
        let n = self.period as Real;
        let (mut m2, mut m3, mut m4) = (0.0, 0.0, 0.0);
        for x in &self.window {
            let d = x - mean;
            let d2 = d * d;
            m2 += d2;
            m3 += d2 * d;
            m4 += d2 * d2;
        }
        (m2 / n, m3 / n, m4 / n)
    }

    pub fn reset(&mut self) {
        self.window.clear();
        self.sum = 0.0;
        self.sum_sq = 0.0;
    }
}

/// Variance floor below which a standardized moment (skewness, kurtosis) or a
/// correlation is reported as `0.0` rather than dividing by a vanishing spread.
pub(crate) const MOMENT_EPS: Real = 1e-12;

/// Two-variable rolling-window statistics: keeps the last `period` `(x, y)`
/// pairs plus running sums (`Σx`, `Σy`, `Σx²`, `Σy²`, `Σxy`), so Pearson
/// correlation over the window is O(1) per update. Backs
/// [`Correlation`](super::Correlation); the shared covariance machinery also
/// makes rolling beta a one-line composition (`corr · σ_y / σ_x`) without a
/// second core.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub(crate) struct WindowCovariance {
    period: usize,
    window: VecDeque<(Real, Real)>,
    sum_x: Real,
    sum_y: Real,
    sum_xx: Real,
    sum_yy: Real,
    sum_xy: Real,
}

impl WindowCovariance {
    pub fn new(period: usize) -> Self {
        assert!(period > 0, "window period must be greater than zero");
        Self {
            period,
            window: VecDeque::with_capacity(period),
            sum_x: 0.0,
            sum_y: 0.0,
            sum_xx: 0.0,
            sum_yy: 0.0,
            sum_xy: 0.0,
        }
    }

    pub fn period(&self) -> usize {
        self.period
    }

    /// Push a paired sample, evicting the oldest once the window is full.
    /// Returns whether the window is now full (statistics valid).
    pub fn update(&mut self, x: Real, y: Real) -> bool {
        self.window.push_back((x, y));
        self.sum_x += x;
        self.sum_y += y;
        self.sum_xx += x * x;
        self.sum_yy += y * y;
        self.sum_xy += x * y;
        if self.window.len() > self.period {
            let (ox, oy) = self.window.pop_front().expect("window is non-empty");
            self.sum_x -= ox;
            self.sum_y -= oy;
            self.sum_xx -= ox * ox;
            self.sum_yy -= oy * oy;
            self.sum_xy -= ox * oy;
        }
        self.is_full()
    }

    pub fn is_full(&self) -> bool {
        self.window.len() == self.period
    }

    /// Pearson correlation over the window, clamped to `[-1, 1]`. Returns `0.0`
    /// when either series is dispersion-free (variance below [`MOMENT_EPS`]) —
    /// correlation is undefined there. Only meaningful once
    /// [`is_full`](Self::is_full).
    pub fn correlation(&self) -> Real {
        let n = self.period as Real;
        let mean_x = self.sum_x / n;
        let mean_y = self.sum_y / n;
        let var_x = (self.sum_xx / n - mean_x * mean_x).max(0.0);
        let var_y = (self.sum_yy / n - mean_y * mean_y).max(0.0);
        if var_x < MOMENT_EPS || var_y < MOMENT_EPS {
            return 0.0;
        }
        let cov = self.sum_xy / n - mean_x * mean_y;
        (cov / (var_x * var_y).sqrt()).clamp(-1.0, 1.0)
    }

    pub fn reset(&mut self) {
        self.window.clear();
        self.sum_x = 0.0;
        self.sum_y = 0.0;
        self.sum_xx = 0.0;
        self.sum_yy = 0.0;
        self.sum_xy = 0.0;
    }
}

/// Windowed weighted moving-average core: a linear-weight WMA over the last
/// `period` samples (oldest weighted `1`, newest weighted `period`), updated in
/// O(1) by carrying both the simple sum and the position-weighted sum. Operates
/// on a plain `Real` stream (no source, no `Indicator` impl) so [`Wma`](super::Wma)
/// can wrap a source while [`Hma`](super::Hma) reuses it to smooth a value it
/// computes internally.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub(crate) struct WmaState {
    period: usize,
    window: VecDeque<Real>,
    /// Simple sum of the window.
    sum: Real,
    /// Position-weighted sum, `Σ kᵢ·xᵢ` with `kᵢ ∈ 1..=period` oldest→newest.
    weighted: Real,
}

impl WmaState {
    pub fn new(period: usize) -> Self {
        assert!(period > 0, "WMA period must be greater than zero");
        Self {
            period,
            window: VecDeque::with_capacity(period),
            sum: 0.0,
            weighted: 0.0,
        }
    }

    pub fn period(&self) -> usize {
        self.period
    }

    /// Push a sample; returns the weighted average once the window is full
    /// (`None` during warm-up).
    pub fn update(&mut self, x: Real) -> Option<Real> {
        if self.window.len() == self.period {
            // Sliding the window down one step lowers every retained weight by 1
            // (so `weighted` drops by the old simple sum) and the newcomer enters
            // at the top weight; the evicted sample falls out of the simple sum.
            let old = self.window.pop_front().expect("window is full");
            self.weighted = self.weighted - self.sum + self.period as Real * x;
            self.sum = self.sum - old + x;
            self.window.push_back(x);
        } else {
            self.window.push_back(x);
            self.weighted += self.window.len() as Real * x;
            self.sum += x;
        }
        if self.window.len() == self.period {
            let denom = (self.period * (self.period + 1) / 2) as Real;
            Some(self.weighted / denom)
        } else {
            None
        }
    }

    pub fn reset(&mut self) {
        self.window.clear();
        self.sum = 0.0;
        self.weighted = 0.0;
    }
}

/// Ascending-order comparison that tolerates `NaN` by treating it as equal to
/// everything, matching [`ExtremeOp`]'s convention. `sort_unstable_by` and
/// `binary_search_by` both panic on a comparator that returns `None`, so every
/// ordered read in the crate funnels through this.
pub(crate) fn cmp_asc(a: &Real, b: &Real) -> std::cmp::Ordering {
    a.partial_cmp(b).unwrap_or(std::cmp::Ordering::Equal)
}

/// Linearly-interpolated `p`-quantile of a sorted-ascending slice (R's type-7,
/// `numpy`'s default). `p` in `[0, 1]`; `0.0` on an empty slice.
///
/// The crate's single quantile convention: shared by the rolling
/// [`WindowQuantile`] core and by `metrics`' `value_at_risk` /
/// `conditional_value_at_risk` / `tail_ratio`, so an indicator's 5th percentile
/// and a report's 5th percentile mean the same thing.
pub(crate) fn quantile_of_sorted(sorted: &[Real], p: Real) -> Real {
    if sorted.is_empty() {
        return 0.0;
    }
    let n = sorted.len();
    if n == 1 {
        return sorted[0];
    }
    let idx = p * (n - 1) as Real;
    let lo = idx.floor() as usize;
    let hi = (lo + 1).min(n - 1);
    let frac = idx - lo as Real;
    sorted[lo] * (1.0 - frac) + sorted[hi] * frac
}

/// Rolling order statistics over the last `period` samples: arbitrary
/// quantiles and the rank of a value within the window.
///
/// The third shared core, beside [`WindowStats`] (moments) and
/// [`WindowExtreme`] (extrema). It is deliberately *not* folded into
/// `WindowStats`: that one's O(1) contract rests on running sum / sum-of-squares,
/// which say nothing about order, and sorted access would quietly break it.
///
/// Keeps two views of the same window — a [`VecDeque`] in arrival order (to know
/// what to evict) and a sorted `Vec` (to answer order queries). Each update is
/// one binary search plus one `Vec` insert and one remove, so O(period) from the
/// memmove and O(log period) for the queries — strictly better than re-sorting
/// per bar, and the same complexity class as
/// [`VarianceRatio`](super::VarianceRatio), the crate's other non-O(1) window.
///
/// Ordering is `NaN`-tolerant via [`cmp_asc`]; a `NaN` in the window sorts
/// wherever it lands rather than panicking.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub(crate) struct WindowQuantile {
    period: usize,
    /// Arrival order — the front is the next sample to evict.
    window: VecDeque<Real>,
    /// The same samples, kept sorted ascending.
    sorted: Vec<Real>,
}

impl WindowQuantile {
    pub fn new(period: usize) -> Self {
        assert!(period > 0, "window period must be greater than zero");
        Self {
            period,
            window: VecDeque::with_capacity(period),
            sorted: Vec::with_capacity(period),
        }
    }

    pub fn period(&self) -> usize {
        self.period
    }

    /// Push a sample, evicting the oldest once the window is full. Returns
    /// whether the window is now full (i.e. order statistics are valid).
    pub fn update(&mut self, x: Real) -> bool {
        self.window.push_back(x);
        let at = self.sorted.partition_point(|v| cmp_asc(v, &x).is_lt());
        self.sorted.insert(at, x);
        if self.window.len() > self.period {
            let old = self.window.pop_front().expect("window is non-empty");
            // `binary_search_by` finds *an* equal element, which is all that is
            // needed: equal values are interchangeable in the sorted view.
            if let Ok(at) = self.sorted.binary_search_by(|v| cmp_asc(v, &old)) {
                self.sorted.remove(at);
            }
        }
        self.is_full()
    }

    pub fn is_full(&self) -> bool {
        self.window.len() == self.period
    }

    /// The `p`-quantile over the window (`p` in `[0, 1]`). Only meaningful once
    /// [`is_full`](Self::is_full).
    pub fn quantile(&self, p: Real) -> Real {
        quantile_of_sorted(&self.sorted, p)
    }

    /// The fraction of the window at or below `x` — `count(v <= x) / period`,
    /// in `(0, 1]` for a value drawn from the window itself. Only meaningful
    /// once [`is_full`](Self::is_full).
    pub fn rank_of(&self, x: Real) -> Real {
        let at_or_below = self.sorted.partition_point(|v| cmp_asc(v, &x).is_le());
        at_or_below as Real / self.period as Real
    }

    pub fn reset(&mut self) {
        self.window.clear();
        self.sorted.clear();
    }
}

/// Rolling extremum over the last `period` samples via a monotonic deque, so
/// each update is O(1) amortised. The direction (max/min) is the [`ExtremeOp`]
/// marker. Embedded by [`Extreme`](super::ops::Extreme) (→ `RollingMax`/
/// `RollingMin`) and by [`Stochastic`](super::Stochastic).
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(bound = "")]
pub(crate) struct WindowExtreme<Op> {
    period: usize,
    // (index, value), kept monotonic so the front is always the extremum.
    deque: VecDeque<(usize, Real)>,
    count: usize,
    #[serde(skip)]
    _op: PhantomData<fn() -> Op>,
}

impl<Op> WindowExtreme<Op> {
    pub fn new(period: usize) -> Self {
        assert!(period > 0, "window period must be greater than zero");
        Self {
            period,
            deque: VecDeque::new(),
            count: 0,
            _op: PhantomData,
        }
    }

    pub fn period(&self) -> usize {
        self.period
    }

    pub fn reset(&mut self) {
        self.deque.clear();
        self.count = 0;
    }

    /// Number of steps since the current extremum was last seen (`0` if it is the
    /// most recent sample), once `period` samples have been observed. Backs
    /// [`Aroon`](super::Aroon), whose lines measure how recently the window high
    /// / low occurred. On ties the *most recent* occurrence wins (the deque keeps
    /// the newer of equal extrema), so `since` is the smallest such gap.
    pub fn since(&self) -> Option<usize> {
        if self.count >= self.period {
            let current = self.count - 1;
            self.deque.front().map(|&(idx, _)| current - idx)
        } else {
            None
        }
    }
}

impl<Op: ExtremeOp> WindowExtreme<Op> {
    /// Push a sample; returns the extremum over the window once `period` samples
    /// have been seen (`None` during warm-up).
    pub fn update(&mut self, x: Real) -> Option<Real> {
        let idx = self.count;
        self.count += 1;

        // Drop tail entries that `x` dominates: they can never be the extremum
        // while `x` is in the window.
        while let Some(&(_, back)) = self.deque.back() {
            if Op::dominates(x, back) {
                self.deque.pop_back();
            } else {
                break;
            }
        }
        self.deque.push_back((idx, x));

        // Drop the front once it has fallen out of the window.
        while let Some(&(front_idx, _)) = self.deque.front() {
            if front_idx + self.period <= idx {
                self.deque.pop_front();
            } else {
                break;
            }
        }

        if self.count >= self.period {
            Some(self.deque.front().expect("deque is non-empty").1)
        } else {
            None
        }
    }
}
