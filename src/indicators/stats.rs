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
//! `kurtosis` and [`WindowCovariance::moments`] — makes one O(period) pass
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

/// Independent accumulators the centred dispersion scans run on.
///
/// The scans are `Σ f(x − μ)` over the retained window, and with one accumulator
/// each add waits on the one before it: the loop's cost is `period` × the FPU's
/// add latency no matter how tight the rest of it is. Splitting the sum across
/// four running totals cuts that chain to a quarter and gives LLVM a shape it
/// can put in one vector register.
///
/// Four rather than eight because it is the width of one AVX register at `f64`,
/// and the windows this serves are short — a `period` of 10 or 20 is typical, so
/// eight lanes would spend more of the window in the scalar remainder than in
/// the vector body.
const LANES: usize = 4;

/// `Σ(x − mean)²` over `xs`, on [`LANES`] independent accumulators.
///
/// Free rather than a method so both the full-window and the partial-window
/// paths reduce a run of samples the same way.
#[inline]
fn lanes_sum_sq(xs: &[Real], mean: Real) -> Real {
    let mut acc = [0.0 as Real; LANES];
    let (chunks, remainder) = xs.as_chunks::<LANES>();
    for chunk in chunks {
        for (a, &x) in acc.iter_mut().zip(chunk) {
            let d = x - mean;
            *a += d * d;
        }
    }
    // The tail folds into lane 0. Which lane it lands in is arbitrary — it only
    // has to be the same one every call, so a given window always reduces the
    // same way.
    for &x in remainder {
        let d = x - mean;
        acc[0] += d * d;
    }
    // Pairwise, not left-to-right: one more halving of the rounding, free.
    (acc[0] + acc[1]) + (acc[2] + acc[3])
}

/// A fixed-capacity ring buffer — the generic form of the storage
/// [`WindowStats`] and [`WindowExtreme`] hand-roll.
///
/// Those two came first and each carries a hand-written ring tuned to its own
/// access pattern ([`WindowStats`] needs the *whole* buffer as one contiguous
/// run when full, [`WindowExtreme`] pushes and pops at both ends to stay
/// monotonic), so neither is expressed in terms of this. Everything else that
/// wants "the last `capacity` samples, oldest evicted" should be.
///
/// The point is the same one trick #3 in `docs/PERFORMANCE.md` records: the
/// capacity is fixed at construction and never changes, so a [`VecDeque`]'s
/// growth checks and wrap-around bookkeeping are paid on every sample for a
/// flexibility none of these windows use. Measured on the shipped indicators,
/// callgrind instructions per sample net of a control
/// (`cargo bench --bench window_ring -- <workload>`; see `docs/PERFORMANCE.md`
/// Phase 13):
///
/// | indicator | `VecDeque` | `Ring` | |
/// |---|---:|---:|---:|
/// | `Diff` / `Lag` / `Ratio` | 49.63 | **7.62** | −84.6% |
/// | [`Wma`](super::Wma) | 65.62 | **26.64** | −59.4% |
/// | [`Hma`](super::Hma) (three at once) | 200.58 | **121.62** | −39.4% |
/// | [`Correlation`](super::Correlation) | 407.18 | **303.83** | −25.4% |
/// | [`Vwap`](super::Vwap) | 144.63 | **129.64** | −10.4% |
///
/// Wall-clock put `Vwap` and `Correlation` ~10% *slower* on the same change.
/// That reading was code layout, not work — the control drifted 13% between the
/// two runs, which is the condition trap 6 in `docs/PERFORMANCE.md` says to
/// resolve with callgrind rather than by re-running.
///
/// **Serialization is the caller's job, and it is load-bearing.** A `Ring` has
/// no `Serialize`/`Deserialize` impl on purpose: the wire format every existing
/// run-state file carries is a bare array in logical (oldest-first) order, which
/// does not record the capacity, so a `Ring` cannot restore itself from one.
/// Each owner therefore hand-writes the pair — reading its own `period` field to
/// size the ring, then replaying the array through [`push`](Self::push) — which
/// is exactly what [`WindowStats`] already does and why *its* format survived
/// the same change. See [`from_logical`](Self::from_logical).
#[derive(Debug, Clone)]
pub(crate) struct Ring<T> {
    /// `capacity` slots. Only the `len` entries starting at `head` (wrapping)
    /// are live; the rest are stale.
    buf: Box<[T]>,
    head: usize,
    len: usize,
}

impl<T: Copy + Default> Ring<T> {
    /// # Panics
    /// Panics if `capacity` is zero.
    pub fn new(capacity: usize) -> Self {
        assert!(capacity > 0, "ring capacity must be greater than zero");
        Self {
            buf: vec![T::default(); capacity].into_boxed_slice(),
            head: 0,
            len: 0,
        }
    }

    /// Rebuild from a logical (oldest-first) run of samples at a known
    /// capacity — the deserialization half, factored out because every owner's
    /// hand-written `Deserialize` needs exactly this.
    ///
    /// Extra samples beyond `capacity` are dropped from the *front*, keeping the
    /// newest: the blob is caller-supplied, and a window longer than its period
    /// is corrupt rather than fatal.
    pub fn from_logical(capacity: usize, items: impl IntoIterator<Item = T>) -> Self {
        let mut out = Self::new(capacity);
        for x in items {
            out.push(x);
        }
        out
    }

    pub fn capacity(&self) -> usize {
        self.buf.len()
    }

    pub fn len(&self) -> usize {
        self.len
    }

    pub fn is_full(&self) -> bool {
        self.len == self.buf.len()
    }

    /// Push a sample. Once full this evicts the oldest and returns it, which is
    /// the `pop_front`-then-`push_back` pair every caller used to write as two
    /// deque operations.
    pub fn push(&mut self, x: T) -> Option<T> {
        let cap = self.buf.len();
        if self.len == cap {
            // Full: overwrite the oldest slot and advance. One store, one
            // branch, no bounds juggling.
            let old = self.buf[self.head];
            self.buf[self.head] = x;
            self.head += 1;
            if self.head == cap {
                self.head = 0;
            }
            Some(old)
        } else {
            let at = self.head + self.len;
            let at = if at >= cap { at - cap } else { at };
            self.buf[at] = x;
            self.len += 1;
            None
        }
    }

    /// The live samples as up to two contiguous slices, oldest first.
    pub fn slices(&self) -> (&[T], &[T]) {
        if self.len == 0 {
            return (&[], &[]);
        }
        let end = self.head + self.len;
        if end <= self.buf.len() {
            (&self.buf[self.head..end], &[])
        } else {
            (&self.buf[self.head..], &self.buf[..end - self.buf.len()])
        }
    }

    /// The whole buffer as **one** contiguous run, when the ring is full.
    ///
    /// A full ring has every slot live, so the buffer is the window — rotated,
    /// which is irrelevant to any order-independent reduction. Handing a scan
    /// one run of `capacity` samples instead of the two short halves
    /// [`slices`](Self::slices) returns is what makes a lane-parallel reduction
    /// pay: at period 10 the split puts six of ten samples in the scalar
    /// remainder, back on a serial dependency chain. `WindowStats` reduces over
    /// exactly this, for exactly this reason — see `docs/PERFORMANCE.md`.
    ///
    /// `None` while the ring is still filling; the caller falls back to
    /// `slices`.
    pub fn full_run(&self) -> Option<&[T]> {
        (self.len == self.buf.len()).then_some(&self.buf)
    }

    /// The live samples, oldest first — the logical order the wire format uses.
    pub fn iter(&self) -> impl Iterator<Item = T> + '_ {
        let (a, b) = self.slices();
        a.iter().copied().chain(b.iter().copied())
    }

    pub fn clear(&mut self) {
        self.head = 0;
        self.len = 0;
    }
}

/// A [`Ring`] serializes as a bare array in logical (oldest-first) order — the
/// shape a `VecDeque` field produced, so every run-state file written before the
/// conversion still loads.
impl<T: Copy + Default + Serialize> Serialize for Ring<T> {
    fn serialize<S: serde::Serializer>(&self, s: S) -> Result<S::Ok, S::Error> {
        use serde::ser::SerializeSeq;
        let mut seq = s.serialize_seq(Some(self.len))?;
        let (a, b) = self.slices();
        for x in a.iter().chain(b) {
            seq.serialize_element(x)?;
        }
        seq.end()
    }
}

/// Restore a fixed-capacity window from the bare array [`Ring`]'s `Serialize`
/// emits, **taking the capacity from the value being restored into** rather than
/// from the blob.
///
/// This is why a [`Ring`] has no `Deserialize` impl and cannot have a useful
/// one: the wire format does not record the capacity, and a window saved
/// mid-warm-up is shorter than its period, so `Deserialize` alone would restore
/// it at the wrong size. It does not need to — the run-state contract in
/// `fugazi-derive` is that *the structure is always rebuilt from the spec first
/// and only the values are replayed in*, so the destination's capacity is
/// already correct by construction. `#[state(window)]` is the field annotation
/// that routes through this.
pub(crate) trait LoadWindow: Sized {
    fn load_window(&self, v: &serde_json::Value) -> Result<Self, String>;
}

impl<T: Copy + Default + serde::de::DeserializeOwned> LoadWindow for Ring<T> {
    fn load_window(&self, v: &serde_json::Value) -> Result<Self, String> {
        let items: Vec<T> = serde_json::from_value(v.clone()).map_err(|e| e.to_string())?;
        if items.len() > self.capacity() {
            return Err(format!(
                "window holds {} samples, more than its capacity of {}",
                items.len(),
                self.capacity()
            ));
        }
        Ok(Ring::from_logical(self.capacity(), items))
    }
}

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
    /// The pass runs on [`LANES`] independent accumulators rather than one, and
    /// that is the whole of its speed. A single running total makes every add
    /// wait on the previous one — a chain of `period` × the FPU's add latency,
    /// which is *all* this used to cost: at period 20 the dependency chain alone
    /// accounted for essentially the entire 20 ns/sample, and no amount of
    /// iterator tuning touches it because the chain is a property of the
    /// arithmetic, not of the loop. Four accumulators cut the chain to
    /// `period / 4` and let the multiplies issue in parallel with it; it also
    /// lets LLVM emit one vector FMA per group. See `docs/PERFORMANCE.md`.
    ///
    /// **This is not bit-identical to a single running total**, and cannot be:
    /// floating-point addition does not reassociate. It is, if anything, the
    /// *more* accurate arrangement — four partial sums each accumulate a quarter
    /// of the rounding a single one does, which is why pairwise summation is the
    /// standard remedy — and `variance_is_exact_at_market_scale` pins the result
    /// against a two-pass reference at every scale the crate cares about. What
    /// changed is the last ulp, not the guarantee.
    ///
    /// **Leave the iteration shape alone**, though: this reads the window's two
    /// contiguous halves ([`slices`](Self::slices)) directly. Going back through
    /// [`iter`](Self::iter) — `a.chain(b)` — reintroduces a per-element test of
    /// which half we are in, unless the consumer is one std specialises
    /// `Chain::fold` for. A plain `for` loop over the chain is not: that measured
    /// **187.45 → 315.18 instructions/sample**, 68% worse (`benches/icount.rs`,
    /// `stddev_scan`, period 20, net of a control).
    pub fn variance(&self) -> Real {
        let mean = self.mean();
        self.centred_sum_sq(mean) / self.period as Real
    }

    /// `Σ(x − mean)²` over the window, on [`LANES`] accumulators. Split out
    /// because [`variance`](Self::variance) is not its only caller — the
    /// centring is identical for anything that needs the second central moment.
    fn centred_sum_sq(&self, mean: Real) -> Real {
        // A **full** window is the state every dispersion read here is
        // documented to be meaningful in, and in it every slot of the ring is
        // live — so the whole buffer is one contiguous run of exactly `period`
        // samples, rotated. Rotation is irrelevant to a sum, and handing the
        // pass one run instead of two short halves is what makes the lanes work
        // at all: at period 10 the split halves put six of ten samples in the
        // scalar remainder, back on a serial chain, and the lanes bought
        // nothing. Measured, that was the difference between no improvement and
        // most of one — see `docs/PERFORMANCE.md`.
        if self.len == self.period {
            return lanes_sum_sq(&self.buf, mean);
        }
        // Partial window: not a meaningful read, but a defined one, so it still
        // has to be right. Two halves, each reduced the same way.
        let (a, b) = self.slices();
        lanes_sum_sq(a, mean) + lanes_sum_sq(b, mean)
    }

    /// Population standard deviation over the window.
    pub fn stddev(&self) -> Real {
        self.variance().sqrt()
    }

    /// The window mean and its population variance, from one pass — the same
    /// saving [`mean_and_stddev`](Self::mean_and_stddev) makes, for the callers
    /// that want the variance itself (they test it against a floor before taking
    /// a root, so they cannot go through the `stddev` form).
    pub fn mean_and_variance(&self) -> (Real, Real) {
        let mean = self.mean();
        (mean, self.centred_sum_sq(mean) / self.period as Real)
    }

    /// The window mean and its population standard deviation, from one pass.
    ///
    /// Exists for [`Bollinger`](super::Bollinger), which needs both every bar
    /// and got them by calling [`mean`](Self::mean) and [`stddev`](Self::stddev)
    /// in turn — and [`stddev`](Self::stddev) computes the mean again to centre
    /// on it. That is two `divsd`s per bar for one quotient, and a divide is
    /// long-latency enough on the critical path (~15 cycles, unpipelined) to
    /// show up next to a 20-element scan. Returning the pair drops one.
    pub fn mean_and_stddev(&self) -> (Real, Real) {
        let (mean, var) = self.mean_and_variance();
        (mean, var.sqrt())
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
///
/// **The serialized shape is unchanged** — see the `Serialize`/`Deserialize`
/// impls below, which emit the same `{period, window, sum_x, sum_y}` object with
/// the window in logical (oldest-first) order that the `VecDeque` derive
/// produced. Run-state files written by earlier versions still load.
/// `(Σ(x−x̄)², Σ(y−ȳ)², Σ(x−x̄)(y−ȳ))` over `xs`, on [`LANES`] independent
/// accumulators per term.
///
/// The paired twin of [`lanes_sum_sq`], and it exists for the same measured
/// reason: a single running total per term makes every add wait on the one
/// before it, so the scan costs `period` × the FPU's add latency no matter how
/// tight the loop. Three terms give three independent chains, which is better
/// than one and still `period` long each; four lanes per term cuts each to
/// `period / 4` and lets the multiplies issue alongside.
///
/// Like `lanes_sum_sq` this is **not** bit-identical to a single running total —
/// floating-point addition does not reassociate — and is, if anything, the more
/// accurate arrangement, for the same reason pairwise summation is the standard
/// remedy. `window_covariance_matches_a_two_pass_pearson` pins it against a
/// two-pass reference.
#[inline]
fn lanes_centred_pairs(xs: &[(Real, Real)], mean_x: Real, mean_y: Real) -> (Real, Real, Real) {
    // Three separate lane arrays, not one array of triples: the separate ones
    // are three contiguous `[f64; 4]` blocks a vector op can load whole, where
    // an interleaved `[(f64, f64, f64); 4]` would stride over 24 bytes. Zipped
    // rather than indexed, so no bounds check has to be proved away — the same
    // shape `lanes_sum_sq` uses.
    let mut acc_xx = [0.0 as Real; LANES];
    let mut acc_yy = [0.0 as Real; LANES];
    let mut acc_xy = [0.0 as Real; LANES];
    let (chunks, remainder) = xs.as_chunks::<LANES>();
    for chunk in chunks {
        let lanes = acc_xx.iter_mut().zip(&mut acc_yy).zip(&mut acc_xy);
        for (((axx, ayy), axy), &(x, y)) in lanes.zip(chunk) {
            let (dx, dy) = (x - mean_x, y - mean_y);
            *axx += dx * dx;
            *ayy += dy * dy;
            *axy += dx * dy;
        }
    }
    // The tail folds into lane 0 — arbitrary, but the same lane every call, so
    // a given window always reduces the same way.
    for &(x, y) in remainder {
        let (dx, dy) = (x - mean_x, y - mean_y);
        acc_xx[0] += dx * dx;
        acc_yy[0] += dy * dy;
        acc_xy[0] += dx * dy;
    }
    // Pairwise, not left-to-right: one more halving of the rounding, free.
    let reduce = |a: [Real; LANES]| (a[0] + a[1]) + (a[2] + a[3]);
    (reduce(acc_xx), reduce(acc_yy), reduce(acc_xy))
}

/// The second-order readings of a `WindowCovariance` window, all five from one
/// centred pass — see `WindowCovariance::moments`.
///
/// Population (divide-by-`n`) throughout. That is not a convention this crate
/// has to defend: every ratio built from these (correlation, beta, a regression
/// slope) divides one of them by another, and the `n` cancels.
#[derive(Debug, Clone, Copy)]
pub struct Moments {
    /// Mean of the `x` (left) leg.
    pub mean_x: Real,
    /// Mean of the `y` (right) leg.
    pub mean_y: Real,
    /// Population variance of the `x` leg.
    pub var_x: Real,
    /// Population variance of the `y` leg.
    pub var_y: Real,
    /// Population covariance of the two legs.
    pub cov: Real,
}

impl Moments {
    /// Pearson correlation, or `0.0` when either leg is dispersion-free
    /// (variance below the crate's shared moment epsilon) — undefined there, and
    /// the same graceful degradation `WindowStats::skewness` applies.
    ///
    /// Clamped to `[-1, 1]`: the centred form is far more accurate than the raw
    /// one but `cov/√(varₓ·var_y)` can still land a few ulps outside on a
    /// perfectly (anti-)correlated window.
    pub fn correlation(&self) -> Real {
        if self.var_x < MOMENT_EPS || self.var_y < MOMENT_EPS {
            return 0.0;
        }
        (self.cov / (self.var_x * self.var_y).sqrt()).clamp(-1.0, 1.0)
    }

    /// The coefficient of determination, `cov² / (varₓ·var_y)`, in `[0, 1]`.
    ///
    /// Computed directly rather than as `correlation()²`: squaring undoes the
    /// square root, so the direct form drops a `sqrt` — ~15 cycles, unpipelined,
    /// on the per-bar path — *and* avoids the round-trip's rounding. `0.0` on a
    /// dispersion-free leg, matching [`correlation`](Self::correlation), and
    /// clamped for the same last-ulp reason.
    pub fn r_squared(&self) -> Real {
        if self.var_x < MOMENT_EPS || self.var_y < MOMENT_EPS {
            return 0.0;
        }
        (self.cov * self.cov / (self.var_x * self.var_y)).clamp(0.0, 1.0)
    }

    /// Slope of the least-squares line fitting `y` as a function of `x`:
    /// `cov / varₓ`.
    ///
    /// `0.0` when `x` is dispersion-free — a vertical fit has no finite slope,
    /// and answering `0` keeps the degradation consistent with
    /// [`correlation`](Self::correlation) rather than propagating an infinity
    /// into a position size.
    pub fn slope_y_on_x(&self) -> Real {
        if self.var_x < MOMENT_EPS {
            return 0.0;
        }
        self.cov / self.var_x
    }

    /// Slope of the least-squares line fitting `x` as a function of `y`:
    /// `cov / var_y`. The mirror of [`slope_y_on_x`](Self::slope_y_on_x), and
    /// **not** its reciprocal — the two coincide only on a perfect fit.
    ///
    /// This is the one a rolling beta wants, since the leg being explained is
    /// the left operand and the benchmark is the right.
    pub fn slope_x_on_y(&self) -> Real {
        if self.var_y < MOMENT_EPS {
            return 0.0;
        }
        self.cov / self.var_y
    }
}

#[derive(Debug, Clone)]
pub(crate) struct WindowCovariance {
    period: usize,
    window: Ring<(Real, Real)>,
    sum_x: Real,
    sum_y: Real,
}

/// The on-the-wire shape of a [`WindowCovariance`], identical to what the old
/// `VecDeque`-backed derive produced.
#[derive(Serialize, Deserialize)]
struct WindowCovarianceRepr {
    period: usize,
    window: Vec<(Real, Real)>,
    sum_x: Real,
    sum_y: Real,
}

impl Serialize for WindowCovariance {
    fn serialize<S: serde::Serializer>(&self, s: S) -> Result<S::Ok, S::Error> {
        WindowCovarianceRepr {
            period: self.period,
            window: self.window.iter().collect(),
            sum_x: self.sum_x,
            sum_y: self.sum_y,
        }
        .serialize(s)
    }
}

impl<'de> Deserialize<'de> for WindowCovariance {
    fn deserialize<D: serde::Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        let r = WindowCovarianceRepr::deserialize(d)?;
        if r.period == 0 {
            return Err(serde::de::Error::custom(
                "window period must be greater than zero",
            ));
        }
        Ok(Self {
            period: r.period,
            window: Ring::from_logical(r.period, r.window),
            sum_x: r.sum_x,
            sum_y: r.sum_y,
        })
    }
}

impl WindowCovariance {
    pub fn new(period: usize) -> Self {
        assert!(period > 0, "window period must be greater than zero");
        Self {
            period,
            window: Ring::new(period),
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
        let evicted = self.window.push((x, y));
        self.sum_x += x;
        self.sum_y += y;
        if let Some((ox, oy)) = evicted {
            self.sum_x -= ox;
            self.sum_y -= oy;
        }
        self.is_full()
    }

    pub fn is_full(&self) -> bool {
        self.window.is_full()
    }

    /// The `x` of the oldest retained pair — the left edge of the window, in
    /// whatever coordinate the caller pushed. `None` on an empty window.
    ///
    /// A regression reports its intercept at that edge, and the caller cannot
    /// derive it from the count when the pushed `x` skips samples.
    pub fn oldest_x(&self) -> Option<Real> {
        self.window.iter().next().map(|(x, _)| x)
    }

    /// Both means, both population variances and the population covariance of
    /// the window, from **one** centred pass.
    ///
    /// The shared intermediate every second-order reading here is built from —
    /// correlation, covariance, beta, a regression slope. Asking for it once and
    /// dividing yourself costs one pass; calling two accessors costs two, which
    /// is the mistake this exists to prevent.
    ///
    /// Centred rather than `Σxy/n − μₓμ_y` for the same reason
    /// [`WindowStats::variance`] is: the raw-moment form cancels away
    /// `(μ/σ)²` significant digits and was wrong at crypto price scale. Only
    /// meaningful once [`is_full`](Self::is_full).
    pub fn moments(&self) -> Moments {
        let n = self.period as Real;
        let mean_x = self.sum_x / n;
        let mean_y = self.sum_y / n;
        // A full window is the state every reading here is documented to be
        // meaningful in, and in it every slot of the ring is live — so the whole
        // buffer is one contiguous run, which is what lets the lanes pay. A
        // partial window is not a meaningful read but still has to be a defined
        // one, so it reduces its two halves the same way.
        let (var_x, var_y, cov) = match self.window.full_run() {
            Some(run) => lanes_centred_pairs(run, mean_x, mean_y),
            None => {
                let (a, b) = self.window.slices();
                let (axx, ayy, axy) = lanes_centred_pairs(a, mean_x, mean_y);
                let (bxx, byy, bxy) = lanes_centred_pairs(b, mean_x, mean_y);
                (axx + bxx, ayy + byy, axy + bxy)
            }
        };
        Moments {
            mean_x,
            mean_y,
            var_x: var_x / n,
            var_y: var_y / n,
            cov: cov / n,
        }
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
///
/// **The serialized shape is unchanged** — see the `Serialize`/`Deserialize`
/// impls below, which emit the same `{period, window, sum, weighted}` object
/// with the window in logical (oldest-first) order that the `VecDeque` derive
/// produced. Run-state files written by earlier versions still load.
#[derive(Debug, Clone)]
pub(crate) struct WmaState {
    period: usize,
    window: Ring<Real>,
    /// Simple sum of the window.
    sum: Real,
    /// Position-weighted sum, `Σ kᵢ·xᵢ` with `kᵢ ∈ 1..=period` oldest→newest.
    weighted: Real,
}

/// The on-the-wire shape of a [`WmaState`], identical to what the old
/// `VecDeque`-backed derive produced.
#[derive(Serialize, Deserialize)]
struct WmaStateRepr {
    period: usize,
    window: Vec<Real>,
    sum: Real,
    weighted: Real,
}

impl Serialize for WmaState {
    fn serialize<S: serde::Serializer>(&self, s: S) -> Result<S::Ok, S::Error> {
        WmaStateRepr {
            period: self.period,
            window: self.window.iter().collect(),
            sum: self.sum,
            weighted: self.weighted,
        }
        .serialize(s)
    }
}

impl<'de> Deserialize<'de> for WmaState {
    fn deserialize<D: serde::Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        let r = WmaStateRepr::deserialize(d)?;
        // `period` is what sizes the ring, so a zero would panic in `Ring::new`
        // on a hand-edited blob. Reject it as bad data instead.
        if r.period == 0 {
            return Err(serde::de::Error::custom(
                "WMA period must be greater than zero",
            ));
        }
        Ok(Self {
            period: r.period,
            window: Ring::from_logical(r.period, r.window),
            sum: r.sum,
            weighted: r.weighted,
        })
    }
}

impl WmaState {
    pub fn new(period: usize) -> Self {
        assert!(period > 0, "WMA period must be greater than zero");
        Self {
            period,
            window: Ring::new(period),
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
        if let Some(old) = self.window.push(x) {
            // Sliding the window down one step lowers every retained weight by 1
            // (so `weighted` drops by the old simple sum) and the newcomer enters
            // at the top weight; the evicted sample falls out of the simple sum.
            // `Ring::push` evicts and hands back the oldest in one operation,
            // and the arithmetic order is unchanged, so this stays bit-identical
            // to the `pop_front`/`push_back` pair it replaces.
            self.weighted = self.weighted - self.sum + self.period as Real * x;
            self.sum = self.sum - old + x;
        } else {
            self.weighted += self.window.len() as Real * x;
            self.sum += x;
        }
        if self.window.is_full() {
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
/// `RollingMin`), by [`Stochastic`](super::Stochastic), by
/// [`Donchian`](super::Donchian) and — through [`since`](Self::since) — by
/// [`Aroon`](super::Aroon).
///
/// The monotonic deque is backed by a **fixed ring of `period` slots**, for the
/// same reason [`WindowStats`] is: it can never hold more than `period` entries
/// (every entry is a distinct sample index inside the window), so the capacity
/// is known at construction and a growable `VecDeque` was paying growth checks
/// and an allocation for flexibility that is never used. The gap this closed was
/// not marginal — see `docs/PERFORMANCE.md`, where `Aroon` (two of these) went
/// from losing to `TA_AROON` to beating it on the strength of this change alone.
///
/// **The serialized shape is unchanged** — the `Serialize`/`Deserialize` impls
/// below emit the same `{period, deque, count}` object, with the deque in
/// logical (front-first) order, that the `VecDeque`-backed derive produced. Run
/// states written by earlier versions still load.
#[derive(Debug, Clone)]
pub(crate) struct WindowExtreme<Op> {
    period: usize,
    /// `period` slots holding `(index, value)` pairs, kept monotonic so the
    /// front is always the extremum. Only the `len` entries starting at `head`
    /// (wrapping) are live.
    buf: Box<[(usize, Real)]>,
    head: usize,
    len: usize,
    count: usize,
    _op: PhantomData<fn() -> Op>,
}

/// The on-the-wire shape of a [`WindowExtreme`], identical to what the old
/// `VecDeque`-backed derive produced.
#[derive(Serialize, Deserialize)]
struct WindowExtremeRepr {
    period: usize,
    deque: Vec<(usize, Real)>,
    count: usize,
}

impl<Op> Serialize for WindowExtreme<Op> {
    fn serialize<S: serde::Serializer>(&self, s: S) -> Result<S::Ok, S::Error> {
        WindowExtremeRepr {
            period: self.period,
            deque: self.iter().collect(),
            count: self.count,
        }
        .serialize(s)
    }
}

impl<'de, Op> Deserialize<'de> for WindowExtreme<Op> {
    fn deserialize<D: serde::Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        let r = WindowExtremeRepr::deserialize(d)?;
        let mut out = WindowExtreme::new(r.period);
        // A saved deque can never exceed `period` entries, but the blob is
        // caller-supplied: truncate rather than write out of bounds.
        for e in r.deque.into_iter().take(r.period) {
            out.push_back(e);
        }
        out.count = r.count;
        Ok(out)
    }
}

impl<Op> WindowExtreme<Op> {
    pub fn new(period: usize) -> Self {
        assert!(period > 0, "window period must be greater than zero");
        Self {
            period,
            buf: vec![(0, 0.0); period].into_boxed_slice(),
            head: 0,
            len: 0,
            count: 0,
            _op: PhantomData,
        }
    }

    /// Index of slot `i` counting from the front, wrapping.
    #[inline]
    fn slot(&self, i: usize) -> usize {
        let at = self.head + i;
        if at >= self.period { at - self.period } else { at }
    }

    #[inline]
    fn front(&self) -> Option<(usize, Real)> {
        (self.len > 0).then(|| self.buf[self.head])
    }

    #[inline]
    fn back(&self) -> Option<(usize, Real)> {
        (self.len > 0).then(|| self.buf[self.slot(self.len - 1)])
    }

    #[inline]
    fn push_back(&mut self, e: (usize, Real)) {
        debug_assert!(self.len < self.period, "monotonic deque cannot exceed the window");
        let at = self.slot(self.len);
        self.buf[at] = e;
        self.len += 1;
    }

    #[inline]
    fn pop_back(&mut self) {
        debug_assert!(self.len > 0);
        self.len -= 1;
    }

    #[inline]
    fn pop_front(&mut self) {
        debug_assert!(self.len > 0);
        self.head = if self.head + 1 == self.period { 0 } else { self.head + 1 };
        self.len -= 1;
    }

    /// The live entries, front (extremum) first. Serialization only.
    fn iter(&self) -> impl Iterator<Item = (usize, Real)> + '_ {
        (0..self.len).map(|i| self.buf[self.slot(i)])
    }

    pub fn period(&self) -> usize {
        self.period
    }

    pub fn reset(&mut self) {
        self.head = 0;
        self.len = 0;
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
            self.front().map(|(idx, _)| current - idx)
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

        // Drop the front once it has fallen out of the window. Done *before* the
        // tail pass, not after, so the deque is never longer than `period` and
        // the fixed ring can be exactly `period` slots: with the eviction last,
        // a full deque could momentarily hold `period + 1` entries.
        //
        // The two passes are independent — the front is evicted on age and the
        // tail on dominance — so this reordering cannot change which entries
        // survive, only the instant at which the stale one leaves.
        // At most one entry ages out per sample once the invariant holds, so the
        // loop body runs at most once in a steady stream; it stays a loop so a
        // `load_state` blob carrying several stale entries drains rather than
        // overflowing the ring.
        while let Some((front_idx, _)) = self.front() {
            if front_idx + self.period <= idx {
                self.pop_front();
            } else {
                break;
            }
        }

        // Drop tail entries that `x` dominates: they can never be the extremum
        // while `x` is in the window.
        while let Some((_, back)) = self.back() {
            if Op::dominates(x, back) {
                self.pop_back();
            } else {
                break;
            }
        }
        self.push_back((idx, x));

        if self.count >= self.period {
            Some(self.front().expect("deque is non-empty").1)
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

    /// The same cancellation lived in `WindowCovariance`'s correlation, whose
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
            (cov.moments().correlation() - 1.0).abs() < 1e-9,
            "a perfectly correlated pair at price scale read {}",
            cov.moments().correlation()
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
                assert_eq!(cov.moments().correlation(), 0.0, "flat leg at sample {i}");
                continue;
            }
            let c: Real = xs
                .iter()
                .zip(yw)
                .map(|(x, y)| (x - mx) * (y - my))
                .sum::<Real>()
                / period as Real;
            close(cov.moments().correlation(), c / (vx * vy).sqrt(), "correlation", i);
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
        assert!((same.moments().correlation() - 1.0).abs() < 1e-12);
        assert!((opposite.moments().correlation() + 1.0).abs() < 1e-12);
        assert!(same.moments().correlation() <= 1.0 && opposite.moments().correlation() >= -1.0);
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
        assert!(std::panic::catch_unwind(|| Ring::<Real>::new(0)).is_err());
    }

    // ---- Ring, and the wire format its adopters must not have moved ---------

    #[test]
    fn ring_evicts_oldest_first_and_iterates_in_logical_order() {
        let mut r = Ring::new(3);
        assert_eq!(r.push(1.0), None);
        assert_eq!(r.push(2.0), None);
        assert_eq!(r.push(3.0), None);
        assert!(r.is_full());
        assert_eq!(r.iter().collect::<Vec<_>>(), vec![1.0, 2.0, 3.0]);
        // Full: each push hands back the sample leaving the window.
        assert_eq!(r.push(4.0), Some(1.0));
        assert_eq!(r.iter().collect::<Vec<_>>(), vec![2.0, 3.0, 4.0]);
        // Wrapped: the two halves still read oldest-first.
        assert_eq!(r.push(5.0), Some(2.0));
        assert_eq!(r.push(6.0), Some(3.0));
        assert_eq!(r.iter().collect::<Vec<_>>(), vec![4.0, 5.0, 6.0]);
        let (a, b) = r.slices();
        assert_eq!([a, b].concat(), vec![4.0, 5.0, 6.0]);
        r.clear();
        assert_eq!(r.len(), 0);
        assert!(r.iter().next().is_none());
    }

    /// `load_window` takes the capacity from the destination, never from the
    /// blob — the whole reason a `Ring` has no `Deserialize`. A window saved
    /// mid-warm-up is shorter than its period, and restoring it at the array's
    /// length would silently shrink the window for the rest of the run.
    #[test]
    fn load_window_sizes_from_the_destination_not_the_blob() {
        let dest: Ring<Real> = Ring::new(5);
        let partial = serde_json::json!([1.0, 2.0]);
        let restored = dest.load_window(&partial).expect("valid partial window");
        assert_eq!(restored.capacity(), 5, "capacity came from the blob");
        assert_eq!(restored.len(), 2);
        assert!(!restored.is_full());
        // Over-long is corrupt input, reported rather than silently truncated.
        let too_long = serde_json::json!([1.0, 2.0, 3.0, 4.0, 5.0, 6.0]);
        assert!(dest.load_window(&too_long).is_err());
    }

    /// The conversion from `VecDeque` to [`Ring`] is a representation change,
    /// and every run-state file already written carries the old one. These are
    /// literal blobs in the shape the `VecDeque`-backed derive produced; if a
    /// future change to `Ring` moves the format, these fail rather than the
    /// breakage surfacing as a resumed run that silently diverges.
    #[test]
    fn the_pre_ring_wire_format_still_loads() {
        // WmaState: `{period, window, sum, weighted}`, window oldest-first.
        let blob = r#"{"period":3,"window":[10.0,20.0,30.0],"sum":60.0,"weighted":140.0}"#;
        let mut wma: WmaState = serde_json::from_str(blob).expect("legacy WMA state");
        assert_eq!(wma.period(), 3);
        // Continues as a never-paused twin would: 1*20 + 2*30 + 3*40 = 200, /6.
        assert_eq!(wma.update(40.0), Some(200.0 / 6.0));

        // WindowCovariance: `{period, window, sum_x, sum_y}`.
        let blob = r#"{"period":2,"window":[[1.0,2.0],[3.0,4.0]],"sum_x":4.0,"sum_y":6.0}"#;
        let cov: WindowCovariance = serde_json::from_str(blob).expect("legacy cov state");
        assert_eq!(cov.period(), 2);
        assert!(cov.is_full());
        assert_eq!(cov.moments().correlation(), 1.0);

        // A zero period would panic in `Ring::new`; it is bad data, not a bug.
        assert!(serde_json::from_str::<WmaState>(r#"{"period":0,"window":[],"sum":0.0,"weighted":0.0}"#).is_err());
    }

    /// Round-tripping mid-warm-up is the case the capacity rule exists for:
    /// the array is shorter than the period, and the restored window must still
    /// take `period` more samples to fill.
    #[test]
    fn a_partially_filled_window_round_trips_and_continues_identically() {
        let mut a = WmaState::new(4);
        let mut b = WmaState::new(4);
        for x in [1.0, 2.0] {
            a.update(x);
            b.update(x);
        }
        let json = serde_json::to_string(&a).unwrap();
        let mut restored: WmaState = serde_json::from_str(&json).unwrap();
        for x in [3.0, 4.0, 5.0, 6.0] {
            assert_eq!(restored.update(x), b.update(x), "diverged after resume");
        }

        let mut a = WindowCovariance::new(4);
        let mut b = WindowCovariance::new(4);
        a.update(1.0, 2.0);
        b.update(1.0, 2.0);
        let json = serde_json::to_string(&a).unwrap();
        let mut restored: WindowCovariance = serde_json::from_str(&json).unwrap();
        for (x, y) in [(2.0, 3.0), (3.0, 5.0), (4.0, 4.0), (5.0, 9.0)] {
            assert_eq!(restored.update(x, y), b.update(x, y));
        }
        assert_eq!(
            restored.moments().correlation().to_bits(),
            b.moments().correlation().to_bits(),
            "restored correlation is not bit-identical"
        );
    }
}
