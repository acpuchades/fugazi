//! Internal rolling-window statistics core shared by the windowed indicators.
//!
//! Retains the last `period` samples plus a running sum, so `mean` is O(1).
//! Embedded by [`Sma`](super::Sma), [`StdDev`](super::StdDev) and
//! [`Bollinger`](super::Bollinger) — anything needing a moving average and/or
//! dispersion over the same window.
//!
//! # Why dispersion scans the window
//!
//! Every *dispersion* read here — [`variance`](WindowStats::variance) and what
//! derives from it, plus `mean_abs_dev` / `downside_dev` / `skewness` /
//! `kurtosis` and [`WindowCovariance::correlation`] — makes one O(period) pass
//! over the retained window, centring on the mean.
//!
//! The O(1) alternative is the textbook `E[X²] − E[X]²` shortcut, carrying a
//! running sum-of-squares. It is **numerically unusable at market scale**: the
//! two terms agree to about `(mean/σ)²`, so the subtraction cancels away that
//! many significant digits. Measured on a 20-sample window, the shortcut's
//! relative error in the variance was:
//!
//! | mean | σ | relative error |
//! |---|---|---|
//! | 100 | 1 | 2e-12 |
//! | 1e5 | 100 | 2e-10 |
//! | 1e5 | 0.01 | **1e-2** |
//! | 1e9 | 0.01 | **clamps to `0.0`** |
//!
//! A high-priced instrument against a tight dispersion — a five-figure crypto
//! pair quoted to the cent — lost most of its digits, and far enough out the
//! result clamped to zero, which silently reported "no dispersion": `ZScore`
//! divides by it, and `skewness`/`kurtosis` degrade to `0.0` under
//! [`MOMENT_EPS`]. The centred pass has no such term, so its error is a few ulps
//! at any scale, and its result is a sum of squares — **non-negative by
//! construction**, so no clamp is load-bearing.
//!
//! The cost is real but small and lands only on the *query*, never on
//! [`update`](WindowStats::update): the window is retained either way, so this
//! is arithmetic on data already in cache, and four of the six statistics were
//! already computed this way. `Sma` — by far the most-used consumer — reads only
//! `mean` and stays O(1). See `variance_is_exact_at_market_scale` for the
//! pinned accuracy guarantee.

use std::collections::VecDeque;
use std::marker::PhantomData;

use serde::{Deserialize, Serialize};

use crate::indicators::ops::ExtremeOp;
use crate::types::Real;

/// A fixed-capacity ring buffer of the last `period` samples.
///
/// A `VecDeque` would do the same job, and did. The window's capacity is known
/// at construction and never changes, so the deque's growth checks and its
/// `push_back`/`pop_front` bookkeeping were paid on every sample for a
/// flexibility this never uses. Measured against TA-Lib's vectorised C, `Sma`
/// sat at 2.5x; a plain ring closes most of that (see `docs/PERFORMANCE.md`).
///
/// **The serialized shape is unchanged** — see the `Serialize`/`Deserialize`
/// impls below, which emit the same `{period, window, sum}` object with the
/// window in logical (oldest-first) order that the `VecDeque` derive produced.
/// Run-state files written by earlier versions still load.
#[derive(Debug, Clone)]
pub(crate) struct WindowStats {
    period: usize,
    /// `period` slots. Only the `len` samples starting at `head` (wrapping) are
    /// live; the rest are stale.
    buf: Box<[Real]>,
    /// Index of the oldest live sample.
    head: usize,
    len: usize,
    sum: Real,
}

/// The on-the-wire shape of a [`WindowStats`], identical to what the old
/// `VecDeque`-backed derive produced.
#[derive(Serialize, Deserialize)]
struct WindowStatsRepr {
    period: usize,
    window: Vec<Real>,
    sum: Real,
}

impl Serialize for WindowStats {
    fn serialize<S: serde::Serializer>(&self, s: S) -> Result<S::Ok, S::Error> {
        WindowStatsRepr {
            period: self.period,
            window: self.iter().collect(),
            sum: self.sum,
        }
        .serialize(s)
    }
}

impl<'de> Deserialize<'de> for WindowStats {
    fn deserialize<D: serde::Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        let r = WindowStatsRepr::deserialize(d)?;
        let mut out = WindowStats::new(r.period);
        for x in r.window {
            out.push(x);
        }
        out.sum = r.sum;
        Ok(out)
    }
}

impl WindowStats {
    pub fn new(period: usize) -> Self {
        assert!(period > 0, "window period must be greater than zero");
        Self {
            period,
            buf: vec![0.0; period].into_boxed_slice(),
            head: 0,
            len: 0,
            sum: 0.0,
        }
    }

    pub fn period(&self) -> usize {
        self.period
    }

    /// Push a sample, evicting the oldest once the window is full. Returns
    /// whether the window is now full (i.e. statistics are valid).
    pub fn update(&mut self, x: Real) -> bool {
        if self.len == self.period {
            // Full: overwrite the oldest slot and advance. One store, one
            // branch, no bounds juggling.
            self.sum -= self.buf[self.head];
            self.buf[self.head] = x;
            self.head += 1;
            if self.head == self.period {
                self.head = 0;
            }
        } else {
            let at = self.head + self.len;
            let at = if at >= self.period { at - self.period } else { at };
            self.buf[at] = x;
            self.len += 1;
        }
        self.sum += x;
        self.is_full()
    }

    /// Push without touching `sum` — used only by `Deserialize`, which restores
    /// the sum verbatim so a reloaded window is bit-identical to the saved one
    /// rather than a re-accumulation of it.
    fn push(&mut self, x: Real) {
        let at = self.head + self.len;
        let at = if at >= self.period { at - self.period } else { at };
        self.buf[at] = x;
        if self.len == self.period {
            self.head = if self.head + 1 == self.period { 0 } else { self.head + 1 };
        } else {
            self.len += 1;
        }
    }

    /// The live samples, oldest first.
    pub fn iter(&self) -> impl Iterator<Item = Real> + '_ {
        let (a, b) = self.slices();
        a.iter().copied().chain(b.iter().copied())
    }

    /// The live samples as up to two contiguous slices, oldest first. Contiguous
    /// halves are what let the O(period) dispersion scans vectorise.
    fn slices(&self) -> (&[Real], &[Real]) {
        if self.len == 0 {
            return (&[], &[]);
        }
        let end = self.head + self.len;
        if end <= self.period {
            (&self.buf[self.head..end], &[])
        } else {
            (&self.buf[self.head..], &self.buf[..end - self.period])
        }
    }

    pub fn is_full(&self) -> bool {
        self.len == self.period
    }

    /// Mean over the window. Only meaningful once [`is_full`](Self::is_full).
    pub fn mean(&self) -> Real {
        self.sum / self.period as Real
    }

    /// Population variance over the window: `mean((x − μ)²)`, computed by one
    /// centred pass (O(period)) rather than the cancelling `E[X²] − E[X]²`
    /// shortcut — see the module docs for the measured reason. Being a sum of
    /// squares it is non-negative by construction, so no clamp is needed.
    /// Only meaningful once [`is_full`](Self::is_full).
    ///
    /// **Leave the `.iter().map(..).sum()` alone.** It looks like it should be
    /// beaten by a hand-written loop over the window's two halves, since
    /// [`iter`](Self::iter) is `a.chain(b)` and a chain tests which half it is
    /// in on every element. It is not: `Sum for f64` goes through `fold`, and
    /// std **specialises `Chain::fold` into two tight loops** with no such test,
    /// while a `for` loop drives `Iterator::next()`, which keeps it. Replacing
    /// this with an explicit accumulator loop measured **187.45 → 315.18
    /// instructions/sample**, 68% worse (`benches/icount.rs`, `stddev_scan`,
    /// period 20, net of a control).
    ///
    /// Summing the halves separately and adding them is a different trap: it is
    /// not bit-identical, because `(Σa) + (Σb)` groups differently from one
    /// running total whenever the ring wraps — and the core suite does not catch
    /// it, since the fixtures happen not to exercise a wrapped window where the
    /// last ULP differs.
    pub fn variance(&self) -> Real {
        let mean = self.mean();
        let sum_sq: Real = self
            .iter()
            .map(|x| {
                let d = x - mean;
                d * d
            })
            .sum();
        sum_sq / self.period as Real
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
        let sum: Real = self.iter().map(|x| (x - mean).abs()).sum();
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
        for x in self.iter() {
            let d = x - mean;
            let d2 = d * d;
            m2 += d2;
            m3 += d2 * d;
            m4 += d2 * d2;
        }
        (m2 / n, m3 / n, m4 / n)
    }

    pub fn reset(&mut self) {
        self.head = 0;
        self.len = 0;
        self.sum = 0.0;
    }
}

/// Variance floor below which a standardized moment (skewness, kurtosis) or a
/// correlation is reported as `0.0` rather than dividing by a vanishing spread.
pub(crate) const MOMENT_EPS: Real = 1e-12;

/// Two-variable rolling-window statistics: keeps the last `period` `(x, y)`
/// pairs plus the running sums `Σx` and `Σy`, so both means are O(1); the
/// Pearson correlation makes one centred O(period) pass, for the same
/// cancellation reason [`WindowStats::variance`] does (the `Σx²/n − μ²` form
/// loses `(μ/σ)²` significant digits, and the covariance term cancels the same
/// way). Backs [`Correlation`](super::Correlation); the shared covariance
/// machinery also makes rolling beta a one-line composition (`corr · σ_y / σ_x`)
/// without a second core.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub(crate) struct WindowCovariance {
    period: usize,
    window: VecDeque<(Real, Real)>,
    sum_x: Real,
    sum_y: Real,
}

impl WindowCovariance {
    pub fn new(period: usize) -> Self {
        assert!(period > 0, "window period must be greater than zero");
        Self {
            period,
            window: VecDeque::with_capacity(period),
            sum_x: 0.0,
            sum_y: 0.0,
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
        if self.window.len() > self.period {
            let (ox, oy) = self.window.pop_front().expect("window is non-empty");
            self.sum_x -= ox;
            self.sum_y -= oy;
        }
        self.is_full()
    }

    pub fn is_full(&self) -> bool {
        self.window.len() == self.period
    }

    /// Pearson correlation over the window, from one centred pass. Returns `0.0`
    /// when either series is dispersion-free (variance below [`MOMENT_EPS`]) —
    /// correlation is undefined there. Still clamped to `[-1, 1]`: the centred
    /// form is far more accurate but `cov/√(varₓ·var_y)` can still land a few
    /// ulps outside on a perfectly (anti-)correlated window. Only meaningful
    /// once [`is_full`](Self::is_full).
    pub fn correlation(&self) -> Real {
        let n = self.period as Real;
        let mean_x = self.sum_x / n;
        let mean_y = self.sum_y / n;
        let (mut var_x, mut var_y, mut cov) = (0.0, 0.0, 0.0);
        for (x, y) in &self.window {
            let (dx, dy) = (x - mean_x, y - mean_y);
            var_x += dx * dx;
            var_y += dy * dy;
            cov += dx * dy;
        }
        let (var_x, var_y, cov) = (var_x / n, var_y / n, cov / n);
        if var_x < MOMENT_EPS || var_y < MOMENT_EPS {
            return 0.0;
        }
        (cov / (var_x * var_y).sqrt()).clamp(-1.0, 1.0)
    }

    pub fn reset(&mut self) {
        self.window.clear();
        self.sum_x = 0.0;
        self.sum_y = 0.0;
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
/// `WindowStats`: order statistics need a *sorted* view maintained on every
/// update, which would put an O(period) insert on the update path — where
/// `WindowStats` keeps O(1), paying its O(period) only on the dispersion
/// queries.
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


#[cfg(test)]
mod tests {
    use super::*;
    use crate::indicators::ops::{MaxOp, MinOp};

    /// The four cores in this file are the crate's O(1)-per-bar shortcuts:
    /// running sums, a monotonic deque, and an incrementally-maintained sorted
    /// view. Each replaces an obvious O(period) computation, and each is only
    /// correct as long as its eviction bookkeeping stays in step. These tests
    /// are therefore **differential**: drive the core and a deliberately naive
    /// recomputation over the same stream, and require them to agree bar for
    /// bar. A hand-written expected series would pin one window; this pins the
    /// eviction logic itself, which is where the bugs live.
    ///
    /// The stream is adversarial on purpose: repeats (so tie-breaking and the
    /// sorted view's duplicate handling are exercised), a run of equal values
    /// (dispersion-free windows), negatives, and a jump.
    const STREAM: [Real; 24] = [
        3.0, 1.0, 4.0, 1.0, 5.0, 9.0, 2.0, 6.0, 5.0, 3.0, 5.0, 5.0, 5.0, 5.0, -2.0, -7.0, 0.0,
        0.0, 8.0, 8.0, 8.0, 1.0, -1.0, 100.0,
    ];

    /// The trailing `period` samples ending at `end` (inclusive), or `None`
    /// while the window is still filling.
    fn window(end: usize, period: usize) -> Option<&'static [Real]> {
        (end + 1 >= period).then(|| &STREAM[end + 1 - period..=end])
    }

    fn naive_mean(w: &[Real]) -> Real {
        w.iter().sum::<Real>() / w.len() as Real
    }

    /// Two-pass population variance — the numerically stable form the O(1)
    /// running-sums shortcut is standing in for.
    fn naive_variance(w: &[Real]) -> Real {
        let m = naive_mean(w);
        w.iter().map(|x| (x - m) * (x - m)).sum::<Real>() / w.len() as Real
    }

    fn naive_central_moment(w: &[Real], k: i32) -> Real {
        let m = naive_mean(w);
        w.iter().map(|x| (x - m).powi(k)).sum::<Real>() / w.len() as Real
    }

    #[track_caller]
    fn close(got: Real, want: Real, what: &str, at: usize) {
        let scale = got.abs().max(want.abs()).max(1.0);
        assert!(
            (got - want).abs() <= 1e-9 * scale,
            "{what} at sample {at}: got {got}, want {want}"
        );
    }

    // -----------------------------------------------------------------------
    // WindowStats
    // -----------------------------------------------------------------------

    #[test]
    fn window_stats_matches_a_two_pass_recomputation() {
        for period in [1usize, 2, 3, 7, 24] {
            let mut stats = WindowStats::new(period);
            for (i, &x) in STREAM.iter().enumerate() {
                let full = stats.update(x);
                let Some(w) = window(i, period) else {
                    assert!(!full, "period {period}: reported full at sample {i}");
                    continue;
                };
                assert!(full, "period {period}: not full at sample {i}");
                close(stats.mean(), naive_mean(w), &format!("mean(p={period})"), i);
                close(
                    stats.variance(),
                    naive_variance(w),
                    &format!("variance(p={period})"),
                    i,
                );
                close(
                    stats.stddev(),
                    naive_variance(w).sqrt(),
                    &format!("stddev(p={period})"),
                    i,
                );
                close(
                    stats.mean_abs_dev(),
                    {
                        let m = naive_mean(w);
                        w.iter().map(|x| (x - m).abs()).sum::<Real>() / w.len() as Real
                    },
                    &format!("mean_abs_dev(p={period})"),
                    i,
                );
            }
        }
    }

    /// `sample_stddev` is the Bessel-corrected (`n − 1`) form the metrics layer
    /// and the trailing risk indicators use, so a full-window rolling Sharpe
    /// equals the whole-run one. A single-sample window has no sample variance
    /// and the documented answer is `0.0`, not a division by zero.
    #[test]
    fn sample_stddev_applies_the_bessel_correction_and_degrades_at_period_one() {
        let mut one = WindowStats::new(1);
        one.update(42.0);
        assert_eq!(one.sample_stddev(), 0.0);

        let period = 5;
        let mut stats = WindowStats::new(period);
        for (i, &x) in STREAM.iter().enumerate() {
            stats.update(x);
            if let Some(w) = window(i, period) {
                let n = period as Real;
                let want = (naive_variance(w) * n / (n - 1.0)).sqrt();
                close(stats.sample_stddev(), want, "sample_stddev", i);
                assert!(
                    stats.sample_stddev() >= stats.stddev() - 1e-12,
                    "the n−1 form must not read below the n form"
                );
            }
        }
    }

    /// Downside deviation only counts samples *below* the threshold, so raising
    /// the threshold can only raise it, and a threshold under the window
    /// minimum makes it zero.
    #[test]
    fn downside_dev_counts_only_the_shortfall() {
        let period = 6;
        let mut stats = WindowStats::new(period);
        for (i, &x) in STREAM.iter().enumerate() {
            stats.update(x);
            let Some(w) = window(i, period) else { continue };
            for threshold in [-10.0, 0.0, 1.0, 200.0] {
                let want = (w
                    .iter()
                    .map(|x| (x - threshold).min(0.0).powi(2))
                    .sum::<Real>()
                    / period as Real)
                    .sqrt();
                close(
                    stats.downside_dev(threshold),
                    want,
                    &format!("downside_dev({threshold})"),
                    i,
                );
            }
            let floor = w.iter().copied().fold(Real::INFINITY, Real::min);
            assert_eq!(
                stats.downside_dev(floor - 1.0),
                0.0,
                "nothing is below a threshold under the window minimum"
            );
        }
    }

    #[test]
    fn skewness_and_kurtosis_match_the_standardized_central_moments() {
        let period = 8;
        let mut stats = WindowStats::new(period);
        for (i, &x) in STREAM.iter().enumerate() {
            stats.update(x);
            let Some(w) = window(i, period) else { continue };
            let m2 = naive_central_moment(w, 2);
            if m2 < MOMENT_EPS {
                assert_eq!(stats.skewness(), 0.0);
                assert_eq!(stats.kurtosis(), 0.0);
                continue;
            }
            close(
                stats.skewness(),
                naive_central_moment(w, 3) / m2.powf(1.5),
                "skewness",
                i,
            );
            close(
                stats.kurtosis(),
                naive_central_moment(w, 4) / (m2 * m2),
                "kurtosis",
                i,
            );
        }
    }

    /// A window with no dispersion has no defined shape, and the documented
    /// degradation is `0.0` rather than a division by a vanishing spread.
    #[test]
    fn a_dispersion_free_window_reports_zero_rather_than_dividing_by_nothing() {
        let mut stats = WindowStats::new(4);
        for _ in 0..4 {
            stats.update(7.5);
        }
        assert_eq!(stats.variance(), 0.0);
        assert_eq!(stats.stddev(), 0.0);
        assert_eq!(stats.mean_abs_dev(), 0.0);
        assert_eq!(stats.skewness(), 0.0);
        assert_eq!(stats.kurtosis(), 0.0);
        assert_eq!(stats.mean(), 7.5);
    }

    /// The centred pass is accurate **at every scale**, which the
    /// `E[X²] − E[X]²` shortcut it replaced was not: that form agreed with the
    /// truth to only `(mean/σ)²`, so a five-figure price against a one-cent
    /// dispersion came out ~1% wrong and a nine-figure one clamped to `0.0`.
    ///
    /// This is the regression guard for that fix. Every row is a
    /// mean-to-dispersion ratio the crate can plausibly meet, including the two
    /// that used to be broken, and all of them must now be exact to a few ulps.
    #[test]
    fn variance_is_exact_at_market_scale() {
        let measure = |mean: Real, noise: Real| -> Real {
            let n = 20;
            let xs: Vec<Real> = (0..n)
                .map(|i| mean + noise * ((i as Real * 1.7).sin()))
                .collect();
            let mut stats = WindowStats::new(n);
            for &x in &xs {
                stats.update(x);
            }
            let exact = naive_variance(&xs);
            (stats.variance() - exact).abs() / exact
        };

        for (mean, noise, what) in [
            (100.0, 1.0, "an equity price against unit dispersion"),
            (0.0005, 0.01, "a per-bar return series"),
            (100_000.0, 100.0, "a crypto price against 0.1% dispersion"),
            // The two the shortcut got wrong: ~1e-2 relative error, then a
            // clamp to zero that silently reported "no dispersion".
            (100_000.0, 0.01, "a five-figure price quoted to the cent"),
            (1e9, 0.01, "a nine-figure notional against a tight spread"),
        ] {
            let err = measure(mean, noise);
            assert!(
                err < 1e-12,
                "{what} (mean {mean:e}, σ≈{noise:e}): relative error {err:e}"
            );
        }
    }

    /// The same cancellation lived in `WindowCovariance::correlation`, whose
    /// `Σx²/n − μ²` variance terms and `Σxy/n − μₓμy` covariance term all
    /// cancelled the same way. Two series that move together at a large offset
    /// must still read as perfectly correlated.
    #[test]
    fn correlation_is_exact_at_market_scale() {
        let n = 20;
        let mut cov = WindowCovariance::new(n);
        for i in 0..n {
            let d = 0.01 * ((i as Real * 1.7).sin());
            // Both legs sit at 1e5 with cent-scale wiggle; `y` is `x` shifted
            // and scaled, so the true correlation is exactly 1.
            cov.update(100_000.0 + d, 250_000.0 + 3.0 * d);
        }
        assert!(
            (cov.correlation() - 1.0).abs() < 1e-9,
            "a perfectly correlated pair at price scale read {}",
            cov.correlation()
        );
    }

    #[test]
    fn reset_returns_window_stats_to_its_constructed_state() {
        let mut stats = WindowStats::new(4);
        for &x in &STREAM[..10] {
            stats.update(x);
        }
        stats.reset();
        assert!(!stats.is_full());
        let mut fresh = WindowStats::new(4);
        for &x in &STREAM[..6] {
            assert_eq!(stats.update(x), fresh.update(x));
        }
        assert_eq!(stats.mean(), fresh.mean());
        assert_eq!(stats.variance(), fresh.variance());
    }

    // -----------------------------------------------------------------------
    // WindowCovariance
    // -----------------------------------------------------------------------

    #[test]
    fn window_covariance_matches_a_two_pass_pearson() {
        // Pair each sample with a lagged, negated copy so the correlation
        // sweeps the whole [-1, 1] range rather than sitting near one end.
        let ys: Vec<Real> = STREAM
            .iter()
            .enumerate()
            .map(|(i, &x)| 2.0 * x - 3.0 * STREAM[i.saturating_sub(2)])
            .collect();

        let period = 6;
        let mut cov = WindowCovariance::new(period);
        for i in 0..STREAM.len() {
            cov.update(STREAM[i], ys[i]);
            let Some(xs) = window(i, period) else { continue };
            let yw = &ys[i + 1 - period..=i];
            let (mx, my) = (naive_mean(xs), naive_mean(yw));
            let vx = naive_variance(xs);
            let vy = naive_variance(yw);
            if vx < MOMENT_EPS || vy < MOMENT_EPS {
                assert_eq!(cov.correlation(), 0.0, "flat leg at sample {i}");
                continue;
            }
            let c: Real = xs
                .iter()
                .zip(yw)
                .map(|(x, y)| (x - mx) * (y - my))
                .sum::<Real>()
                / period as Real;
            close(cov.correlation(), c / (vx * vy).sqrt(), "correlation", i);
        }
    }

    /// A series correlated with itself is exactly `1`, with its negation
    /// exactly `-1`, and the result is clamped so round-off can never produce
    /// a magnitude above one.
    #[test]
    fn correlation_of_a_series_with_itself_and_its_negation_saturates() {
        let period = 5;
        let (mut same, mut opposite) = (
            WindowCovariance::new(period),
            WindowCovariance::new(period),
        );
        for &x in &STREAM[..period] {
            same.update(x, x);
            opposite.update(x, -x);
        }
        assert!((same.correlation() - 1.0).abs() < 1e-12);
        assert!((opposite.correlation() + 1.0).abs() < 1e-12);
        assert!(same.correlation() <= 1.0 && opposite.correlation() >= -1.0);
    }

    // -----------------------------------------------------------------------
    // WmaState
    // -----------------------------------------------------------------------

    /// The O(1) slide (`weighted − sum + period·x`) has to reproduce the
    /// explicit `Σ kᵢ·xᵢ / Σ kᵢ` on every window, including the first full one
    /// where a different branch runs.
    #[test]
    fn wma_state_matches_explicit_linear_weights() {
        for period in [1usize, 2, 5, 9] {
            let mut wma = WmaState::new(period);
            for (i, &x) in STREAM.iter().enumerate() {
                let got = wma.update(x);
                match window(i, period) {
                    None => assert_eq!(got, None, "period {period} at sample {i}"),
                    Some(w) => {
                        let denom = (period * (period + 1) / 2) as Real;
                        let want = w
                            .iter()
                            .enumerate()
                            .map(|(k, x)| (k + 1) as Real * x)
                            .sum::<Real>()
                            / denom;
                        close(got.expect("full window"), want, &format!("wma(p={period})"), i);
                    }
                }
            }
        }
    }

    // -----------------------------------------------------------------------
    // quantile_of_sorted / WindowQuantile
    // -----------------------------------------------------------------------

    /// R type-7 (numpy's default): `idx = p·(n−1)`, interpolated between the
    /// bracketing order statistics. These are the values `numpy.percentile`
    /// returns for the same input — the convention the whole crate shares, so
    /// an indicator's 5th percentile and a report's 5th percentile agree.
    #[test]
    fn quantile_of_sorted_follows_r_type_7() {
        let xs = [1.0, 2.0, 3.0, 4.0];
        for (p, want) in [
            (0.0, 1.0),
            (0.25, 1.75),
            (0.5, 2.5),
            (0.75, 3.25),
            (1.0, 4.0),
            (1.0 / 3.0, 2.0),
        ] {
            close(quantile_of_sorted(&xs, p), want, &format!("q({p})"), 0);
        }
        // Degenerate shapes have defined answers rather than an index panic.
        assert_eq!(quantile_of_sorted(&[], 0.5), 0.0);
        assert_eq!(quantile_of_sorted(&[9.0], 0.5), 9.0);
        assert_eq!(quantile_of_sorted(&[9.0], 0.0), 9.0);
    }

    /// The incrementally-maintained sorted view must equal a freshly sorted
    /// copy of the window — the property that eviction by value (a binary
    /// search for *an* equal element) preserves. The stream's repeated values
    /// are what make that non-trivial.
    #[test]
    fn window_quantile_matches_re_sorting_the_window() {
        for period in [1usize, 3, 5, 12] {
            let mut q = WindowQuantile::new(period);
            for (i, &x) in STREAM.iter().enumerate() {
                let full = q.update(x);
                let Some(w) = window(i, period) else {
                    assert!(!full);
                    continue;
                };
                assert!(full);
                let mut sorted = w.to_vec();
                sorted.sort_by(cmp_asc);
                for p in [0.0, 0.1, 0.5, 0.9, 1.0] {
                    close(
                        q.quantile(p),
                        quantile_of_sorted(&sorted, p),
                        &format!("quantile(p={p}, period={period})"),
                        i,
                    );
                }
                for &probe in w {
                    let below = sorted.iter().filter(|v| **v <= probe).count();
                    close(
                        q.rank_of(probe),
                        below as Real / period as Real,
                        &format!("rank_of({probe}, period={period})"),
                        i,
                    );
                }
            }
        }
    }

    // -----------------------------------------------------------------------
    // WindowExtreme
    // -----------------------------------------------------------------------

    /// The monotonic deque must equal a brute-force scan of the same window,
    /// and `since()` must report the gap back to the extremum — with the
    /// **most recent** occurrence winning a tie, which the runs of equal
    /// values in the stream exercise directly.
    #[test]
    fn window_extreme_matches_a_brute_force_scan() {
        for period in [1usize, 2, 4, 10] {
            let mut max: WindowExtreme<MaxOp> = WindowExtreme::new(period);
            let mut min: WindowExtreme<MinOp> = WindowExtreme::new(period);
            for (i, &x) in STREAM.iter().enumerate() {
                let (got_max, got_min) = (max.update(x), min.update(x));
                let Some(w) = window(i, period) else {
                    assert_eq!(got_max, None, "period {period} at sample {i}");
                    assert_eq!(got_min, None, "period {period} at sample {i}");
                    assert_eq!(max.since(), None);
                    continue;
                };
                let want_max = w.iter().copied().fold(Real::NEG_INFINITY, Real::max);
                let want_min = w.iter().copied().fold(Real::INFINITY, Real::min);
                assert_eq!(got_max, Some(want_max), "max(p={period}) at {i}");
                assert_eq!(got_min, Some(want_min), "min(p={period}) at {i}");

                // Ties resolve to the newest occurrence, so `since` is the
                // smallest gap that attains the extremum.
                let newest = |target: Real| {
                    w.len() - 1 - w.iter().rposition(|v| *v == target).expect("in window")
                };
                assert_eq!(max.since(), Some(newest(want_max)), "since_max at {i}");
                assert_eq!(min.since(), Some(newest(want_min)), "since_min at {i}");
            }
        }
    }

    #[test]
    fn reset_returns_window_extreme_to_its_constructed_state() {
        let mut ext: WindowExtreme<MaxOp> = WindowExtreme::new(3);
        for &x in &STREAM[..8] {
            ext.update(x);
        }
        ext.reset();
        assert_eq!(ext.since(), None);
        let mut fresh: WindowExtreme<MaxOp> = WindowExtreme::new(3);
        for &x in &STREAM[..5] {
            assert_eq!(ext.update(x), fresh.update(x));
        }
    }

    /// `cmp_asc` treats `NaN` as equal to everything so the ordered reads never
    /// panic on a comparator returning `None` — `sort_unstable_by` and
    /// `binary_search_by` both do.
    #[test]
    fn nan_tolerant_ordering_keeps_the_sorted_reads_from_panicking() {
        assert_eq!(cmp_asc(&1.0, &2.0), std::cmp::Ordering::Less);
        assert_eq!(cmp_asc(&2.0, &1.0), std::cmp::Ordering::Greater);
        assert_eq!(cmp_asc(&Real::NAN, &1.0), std::cmp::Ordering::Equal);

        let mut q = WindowQuantile::new(3);
        for x in [1.0, Real::NAN, 2.0, 3.0] {
            q.update(x);
        }
        // The point is that it did not panic; the value with a NaN in the
        // window is deliberately unspecified.
        let _ = q.quantile(0.5);
    }

    /// Every core asserts a positive period rather than silently producing an
    /// empty window that would divide by zero downstream.
    #[test]
    fn a_zero_period_is_refused_by_every_core() {
        assert!(std::panic::catch_unwind(|| WindowStats::new(0)).is_err());
        assert!(std::panic::catch_unwind(|| WindowCovariance::new(0)).is_err());
        assert!(std::panic::catch_unwind(|| WmaState::new(0)).is_err());
        assert!(std::panic::catch_unwind(|| WindowQuantile::new(0)).is_err());
        assert!(std::panic::catch_unwind(|| WindowExtreme::<MaxOp>::new(0)).is_err());
    }
}
