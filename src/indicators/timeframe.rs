//! Cross-timeframe composition: two composable primitives for running an
//! indicator on candles coarser than the base stream, without changing the
//! [`Strategy`](crate::Strategy) trait or the per-bar loop.
//!
//! * [`Resample`] buckets `every` base candles into one higher-timeframe
//!   [`Candle`], emitting `Some(Candle)` on the completing tick and `None`
//!   between. Recursive downstream smoothers (EMA, RSI, ATR, …) already treat
//!   `None` as "don't advance", so they naturally see one genuine new sample
//!   per higher-timeframe bar.
//! * [`Latch`] holds the last emitted output of a source, re-emitting it on
//!   `None` ticks — so a per-base-tick comparison against another indicator
//!   sees the finished higher-timeframe value between boundaries.
//!
//! The **only correct ordering** is Resample → recursive smoother → Latch:
//! latching *before* an EMA/RSI/… would feed it a held (repeated) value on
//! every base tick, distorting the recurrence:
//!
//! ```
//! use fugazi::prelude::*;
//! use fugazi::indicators::{Current, Ema, Latch, Resample};
//!
//! // EMA-20 of the close of every 4-bar candle, latched for per-base-tick reads.
//! let _htf_ema = Latch::new(
//!     Ema::new(Resample::new(Current::candle(), 4).close(), 20),
//! );
//! ```

use fugazi_derive::SaveState;

use crate::indicator::Indicator;
use crate::indicators::component::Component;
use crate::types::{Candle, Real};

// ---------------------------------------------------------------------------
// Resample
// ---------------------------------------------------------------------------

/// Aggregates `every` consecutive candles from an inner source into a single
/// higher-timeframe [`Candle`].
///
/// Bar-count based (no timestamp dependency — [`Candle`] has none): each
/// `every` inner-emitted candles fill one bucket, with `open` from the first,
/// `high` / `low` as running extrema, `close` from the last, and `volume` as
/// the running sum. Emits `Some(Candle)` only on the tick that completes the
/// bucket and returns `None` otherwise.
///
/// **The clock stays base-timeframe.** `Resample` is fed one base candle per
/// `update()` call and reports results at that same base cadence — the
/// emitted `Option<Candle>` marks *whether* a bucket has just completed. It
/// carries no internal notion of "an HTF tick"; `warm_up_bars()` is
/// measured in **base samples**, not HTF ones, and matches the base index of
/// the first emission (`inner.warm_up_bars() + every - 1`). Any recursive
/// downstream indicator (EMA/RSI/ATR/…) reasons in HTF-sample units of its
/// own, so its `warm_up_bars()` and `unstable_bars()` composed with a
/// `Resample` are also in HTF-sample units — feed the pipeline enough
/// leading history for the recursive tail to decay if you need base-bar-
/// correct stability accounting.
///
/// The output is a plain `Candle`, so use `.close()`/`.high()`/… (or the
/// generic [`Component`] projection) to feed a scalar into an EMA / band /
/// oscillator downstream. To hold the last emitted value between higher-
/// timeframe boundaries, wrap in [`Latch`].
///
/// ```
/// use fugazi::prelude::*;
/// use fugazi::indicators::{Current, Ema, Latch, Resample};
///
/// let _htf_ema = Latch::new(
///     Ema::new(Resample::new(Current::candle(), 4).close(), 20),
/// );
/// ```
///
/// # Panics
/// Constructor panics when `every == 0`.
#[derive(Debug, Clone, SaveState)]
pub struct Resample<S> {
    #[state(source)]
    inner: S,
    #[state(config)]
    every: usize,
    count: usize,
    open: Option<Real>,
    high: Real,
    low: Real,
    close: Real,
    volume: Real,
    /// Latest emitted higher-timeframe candle; `None` on any non-boundary tick.
    /// A recomputed cache — set every `update` from the (restored) bucket
    /// accumulators — so it is not part of the saved state (which also avoids
    /// a `Candle` serde dependency here).
    #[state(skip)]
    pub value: Option<Candle>,
}

impl<S> Resample<S> {
    /// Aggregate `inner`'s output into buckets of `every` inner-emitted candles.
    ///
    /// # Panics
    /// Panics if `every` is zero.
    pub fn new(inner: S, every: usize) -> Self {
        assert!(every > 0, "resample every must be greater than zero");
        Self {
            inner,
            every,
            count: 0,
            open: None,
            high: 0.0,
            low: 0.0,
            close: 0.0,
            volume: 0.0,
            value: None,
        }
    }

    /// How many inner-emitted candles fill one higher-timeframe bucket.
    pub fn every(&self) -> usize {
        self.every
    }
}

impl<S: Indicator<Output = Candle>> Resample<S> {
    /// Project the higher-timeframe candle's `close`.
    pub fn close(self) -> Component<Self> {
        Component::new(self, |c: Candle| c.close)
    }

    /// Project the higher-timeframe candle's `open`.
    pub fn open(self) -> Component<Self> {
        Component::new(self, |c: Candle| c.open)
    }

    /// Project the higher-timeframe candle's `high`.
    pub fn high(self) -> Component<Self> {
        Component::new(self, |c: Candle| c.high)
    }

    /// Project the higher-timeframe candle's `low`.
    pub fn low(self) -> Component<Self> {
        Component::new(self, |c: Candle| c.low)
    }

    /// Project the higher-timeframe candle's `volume`.
    pub fn volume(self) -> Component<Self> {
        Component::new(self, |c: Candle| c.volume)
    }

    /// Project the higher-timeframe candle's typical price
    /// (`(high + low + close) / 3`).
    pub fn typical(self) -> Component<Self> {
        Component::new(self, |c: Candle| c.typical())
    }

    /// Project the higher-timeframe candle's median price (`(high + low) / 2`).
    pub fn median(self) -> Component<Self> {
        Component::new(self, |c: Candle| c.median())
    }
}

impl<S: Indicator<Output = Candle>> Indicator for Resample<S> {
    type Input = S::Input;
    type Output = Candle;

    fn update(&mut self, input: Self::Input) -> Option<Candle> {
        self.value = match self.inner.update(input) {
            Some(bar) => {
                if self.open.is_none() {
                    self.open = Some(bar.open);
                    self.high = bar.high;
                    self.low = bar.low;
                    self.volume = 0.0;
                } else {
                    if bar.high > self.high {
                        self.high = bar.high;
                    }
                    if bar.low < self.low {
                        self.low = bar.low;
                    }
                }
                self.close = bar.close;
                self.volume += bar.volume;
                self.count += 1;
                if self.count >= self.every {
                    let out = Candle::new(
                        self.open.take().unwrap(),
                        self.high,
                        self.low,
                        self.close,
                        self.volume,
                    );
                    self.count = 0;
                    Some(out)
                } else {
                    None
                }
            }
            None => None,
        };
        self.value
    }

    fn value(&self) -> Option<Candle> {
        self.value
    }

    fn warm_up_bars(&self) -> usize {
        // The k-th inner-emitted candle arrives at input sample
        // `inner.warm_up_bars() + k - 1`. The first bucket completes on the
        // `every`-th inner emission — sample `inner.warm_up_bars() + every - 1`.
        self.inner.warm_up_bars() + self.every - 1
    }

    fn unstable_bars(&self) -> usize {
        // Windowed / FIR: no additional instability of its own; downstream
        // recursive smoothers reason in inner-emit units, so any base-bar
        // interpretation of `stable_bars()` is only correct in higher-timeframe
        // sample counts — not in base bars.
        self.inner.unstable_bars()
    }

    fn reset(&mut self) {
        self.inner.reset();
        self.count = 0;
        self.open = None;
        // The bucket accumulators too. They are dead while `open` is `None` —
        // the next `update` overwrites all four before reading any — so leaving
        // them behind changed no reading, and that is exactly why it survived:
        // `reset` is documented to return the indicator to its *constructed*
        // condition, and `save_state` is the only complete view of whether it
        // did. A reset instance that still serializes a previous run's high
        // makes the two disagree, and couples correctness to an invariant
        // ("`open` is checked first") three fields away.
        self.high = 0.0;
        self.low = 0.0;
        self.close = 0.0;
        self.volume = 0.0;
        self.value = None;
    }

    fn save_state(&self) -> serde_json::Value {
        self.save_state_fields()
    }

    fn load_state(&mut self, state: &serde_json::Value) -> Result<(), String> {
        self.load_state_fields(state)
    }
}

// ---------------------------------------------------------------------------
// Latch
// ---------------------------------------------------------------------------

/// Holds the most recent [`Some`] output of an inner source, re-emitting it on
/// ticks where the source returns `None`.
///
/// Output-agnostic (works over `Real`, `Candle`, or boolean sources — the
/// [`Indicator`] trait's `Output: Clone` supplies the necessary bound). Once
/// at least one value has arrived, `update` and [`value`](Indicator::value)
/// always report the last emitted output until the next `Some` from the source
/// replaces it.
///
/// The intended shape is **latch *after* any recursive smoother**, not before:
/// feeding a repeated held value into an EMA / RSI / ATR distorts the
/// recurrence. The correct construction order is
/// `Latch::new(Ema::new(Resample::new(src, N).close(), period))`.
///
/// Warm-up and unstable-period are pure passthroughs — `Latch` doesn't add
/// delay, and (crucially) doesn't mask an unsettled inner value into looking
/// stable to `Stable` or the CLI's gate.
#[derive(Clone, SaveState)]
pub struct Latch<S: Indicator> {
    #[state(source)]
    inner: S,
    /// The last emitted output; `None` until the inner source has produced one.
    ///
    /// `S::Output` is an unbounded associated type, so the held value can't be
    /// serialized in general and is skipped. A resumed `Latch` therefore holds
    /// `None` until the inner source next emits `Some` — between higher-timeframe
    /// boundaries it re-warms rather than re-emitting the pre-resume value. This
    /// is the one bounded, self-healing fidelity gap in the resume path (shared
    /// with the generic `Change` toggle detector).
    #[state(skip)]
    pub value: Option<S::Output>,
}

impl<S: Indicator> Latch<S> {
    /// Wrap `inner`, latching its most recent output.
    pub fn new(inner: S) -> Self {
        Self { inner, value: None }
    }
}

impl<S: Indicator> Indicator for Latch<S> {
    type Input = S::Input;
    type Output = S::Output;

    fn update(&mut self, input: Self::Input) -> Option<S::Output> {
        if let Some(v) = self.inner.update(input) {
            self.value = Some(v);
        }
        self.value.clone()
    }

    fn value(&self) -> Option<S::Output> {
        self.value.clone()
    }

    fn warm_up_bars(&self) -> usize {
        // `max(1)` guards a `warm_up = 0` inner — Latch holds nothing before
        // its first `update`, so its first `Some` is at update ≥ 1.
        self.inner.warm_up_bars().max(1)
    }

    fn unstable_bars(&self) -> usize {
        self.inner.unstable_bars()
    }

    fn reset(&mut self) {
        self.inner.reset();
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
    use crate::indicators::{Current, Ema};

    fn bar(close: Real) -> Candle {
        Candle::new(close, close, close, close, 0.0)
    }

    fn bars() -> Vec<Candle> {
        vec![
            Candle::new(10.0, 12.0, 9.0, 11.0, 100.0),
            Candle::new(11.0, 13.0, 10.5, 12.5, 200.0),
            Candle::new(12.5, 14.0, 12.0, 13.5, 150.0),
            Candle::new(13.5, 15.0, 13.0, 14.0, 50.0),
            Candle::new(14.0, 16.0, 13.5, 15.5, 300.0),
            Candle::new(15.5, 17.0, 15.0, 16.5, 200.0),
            Candle::new(16.5, 18.0, 16.0, 17.0, 100.0),
            Candle::new(17.0, 19.0, 16.0, 18.5, 250.0),
        ]
    }

    // ---- Resample ----

    #[test]
    fn resample_every_1_is_identity_passthrough() {
        let mut r = Resample::new(Current::candle(), 1);
        for c in bars() {
            assert_eq!(r.update(c.into()).unwrap(), c);
        }
    }

    #[test]
    fn resample_emits_on_the_nth_bar_only() {
        let mut r = Resample::new(Current::candle(), 4);
        let bars = bars();
        for c in &bars[..3] {
            assert!(r.update((*c).into()).is_none());
        }
        let htf1 = r.update(bars[3].into()).unwrap();
        assert_eq!(htf1.open, 10.0);
        assert_eq!(htf1.close, 14.0);
        assert_eq!(htf1.high, 15.0);
        assert_eq!(htf1.low, 9.0);
        assert_eq!(htf1.volume, 100.0 + 200.0 + 150.0 + 50.0);
        for c in &bars[4..7] {
            assert!(r.update((*c).into()).is_none());
        }
        let htf2 = r.update(bars[7].into()).unwrap();
        assert_eq!(htf2.open, 14.0);
        assert_eq!(htf2.close, 18.5);
        assert_eq!(htf2.high, 19.0);
        assert_eq!(htf2.low, 13.5);
        assert_eq!(htf2.volume, 300.0 + 200.0 + 100.0 + 250.0);
    }

    #[test]
    fn resample_warm_up_lands_on_the_first_emit() {
        let mut r = Resample::new(Current::candle(), 4);
        assert_eq!(r.warm_up_bars(), 4);
        for (i, c) in bars().into_iter().enumerate() {
            let sample = i + 1;
            let ready = r.update(c.into()).is_some();
            assert_eq!(
                ready,
                sample % 4 == 0,
                "unexpected emission at sample {sample}"
            );
        }
    }

    #[test]
    fn resample_reset_clears_the_accumulator() {
        let mut r = Resample::new(Current::candle(), 3);
        for c in bars().into_iter().take(2) {
            r.update(c.into());
        }
        r.reset();
        let bars = bars();
        assert!(r.update(bars[0].into()).is_none());
        assert!(r.update(bars[1].into()).is_none());
        let out = r.update(bars[2].into()).unwrap();
        assert_eq!(out.open, bars[0].open);
        assert_eq!(out.close, bars[2].close);
    }

    #[test]
    #[should_panic(expected = "resample every must be greater than zero")]
    fn resample_zero_every_panics() {
        let _ = Resample::new(Current::candle(), 0);
    }

    // ---- Latch ----

    #[test]
    fn latch_holds_the_last_emitted_value_across_none_ticks() {
        let mut latch = Latch::new(Resample::new(Current::candle(), 3));
        assert_eq!(latch.update(bar(1.0).into()), None);
        assert_eq!(latch.update(bar(2.0).into()), None);
        let first = latch.update(bar(3.0).into()).unwrap();
        assert_eq!(first.close, 3.0);
        assert_eq!(latch.update(bar(4.0).into()).unwrap().close, 3.0);
        assert_eq!(latch.update(bar(5.0).into()).unwrap().close, 3.0);
        assert_eq!(latch.update(bar(6.0).into()).unwrap().close, 6.0);
    }

    #[test]
    fn latch_returns_none_before_the_source_has_ever_emitted() {
        let mut latch = Latch::new(Resample::new(Current::candle(), 4));
        for close in [1.0, 2.0, 3.0] {
            assert_eq!(latch.update(bar(close).into()), None);
            assert_eq!(latch.value, None);
        }
    }

    #[test]
    fn latch_unstable_bars_passes_through() {
        let raw = Ema::new(Current::close(), 20);
        let latched = Latch::new(Ema::new(Current::close(), 20));
        assert_eq!(latched.unstable_bars(), raw.unstable_bars());
        assert_eq!(latched.warm_up_bars(), raw.warm_up_bars());
    }

    #[test]
    fn latch_reset_clears_the_held_value() {
        let mut latch = Latch::new(Resample::new(Current::candle(), 2));
        latch.update(bar(1.0).into());
        latch.update(bar(2.0).into());
        assert!(latch.value.is_some());
        latch.reset();
        assert!(latch.value.is_none());
        assert!(latch.update(bar(3.0).into()).is_none());
    }

    // ---- Composition-order regression ----

    /// The correct pipeline: Resample → Ema → Latch. The Ema recurses only over
    /// real resampled closes; the Latch holds the finished EMA value between
    /// higher-timeframe boundaries so a per-base-tick comparison keeps working.
    ///
    /// The wrong pipeline: Resample → Latch → Ema. The Latch feeds a repeated
    /// held close on every non-boundary tick, so the EMA gets 3 phantom updates
    /// for every real one and its recurrence diverges from what you'd get by
    /// running the same EMA over a pre-aggregated 4-bar candle series.
    #[test]
    fn composition_order_correct_vs_wrong() {
        use crate::indicators::{Latch, Resample};

        // Synthetic drift so the EMA seed matters.
        let bars: Vec<Candle> = (0..24)
            .map(|i| {
                let close = 100.0 + (i as Real) * 0.5 + (i as Real * 0.9).sin();
                Candle::new(close, close, close, close, 1.0)
            })
            .collect();

        // Reference: pre-aggregate 4-bar candles by hand, then Ema over closes.
        let mut reference = Ema::new(Current::close(), 3);
        let mut expected_at_boundary: Vec<Real> = Vec::new();
        for chunk in bars.chunks(4) {
            if chunk.len() < 4 {
                break;
            }
            let close = chunk.last().unwrap().close;
            expected_at_boundary.push(
                reference
                    .update(Candle::new(close, close, close, close, 0.0).into())
                    .unwrap(),
            );
        }

        // Correct: Latch(Ema(Resample.close, 3)).
        let mut correct = Latch::new(Ema::new(Resample::new(Current::candle(), 4).close(), 3));
        // Wrong: Ema(Latch(Resample.close), 3).
        let mut wrong = Ema::new(Latch::new(Resample::new(Current::candle(), 4).close()), 3);

        let mut correct_at_boundary: Vec<Real> = Vec::new();
        let mut wrong_at_boundary: Vec<Real> = Vec::new();
        for (i, bar) in bars.iter().enumerate() {
            let c = correct.update((*bar).into());
            let w = wrong.update((*bar).into());
            let sample = i + 1;
            if sample % 4 == 0 {
                correct_at_boundary.push(c.expect("correct value at boundary"));
                wrong_at_boundary.push(w.expect("wrong value at boundary"));
            } else if let Some(last) = correct_at_boundary.last() {
                // Between boundaries (after the first), the correct pipeline
                // latches the last finished EMA — unchanged since the previous
                // boundary.
                assert_eq!(c.unwrap(), *last);
            }
        }

        assert_eq!(correct_at_boundary.len(), expected_at_boundary.len());
        for (got, expect) in correct_at_boundary.iter().zip(expected_at_boundary.iter()) {
            assert!(
                (got - expect).abs() < 1e-12,
                "correct pipeline diverged from reference: {got} vs {expect}"
            );
        }

        // The wrong pipeline agrees at the first boundary (Ema just seeds) and
        // often at the second (the Latch replays the same value in-between, so
        // the Ema stays at its seed once), but diverges materially from the
        // reference thereafter. Sanity-check that later boundaries diverge and
        // that the maximum divergence is much larger than the correct one's.
        let mut wrong_max = 0.0f64;
        for (i, (w, e)) in wrong_at_boundary
            .iter()
            .zip(expected_at_boundary.iter())
            .enumerate()
        {
            let d = (*w - *e).abs();
            if i >= 2 {
                assert!(
                    d > 1e-3,
                    "wrong-order pipeline should diverge from reference at \
                     boundary {i}: {w} vs {e}"
                );
            }
            if d > wrong_max {
                wrong_max = d;
            }
        }
        assert!(
            wrong_max > 1e-2,
            "wrong-order pipeline barely diverges: max {wrong_max}"
        );
    }
}

// ---------------------------------------------------------------------------
// Accumulate — information-driven bar sampling
// ---------------------------------------------------------------------------

/// How much one base candle contributes to an [`Accumulate`] bucket.
///
/// The marker that turns one accumulator into the whole family of
/// information-driven bar types. A new sampling scheme is a trait impl over a
/// zero-sized marker, not a new struct — the same shape [`CandleField`] and
/// [`CalendarField`] use.
///
/// [`CandleField`]: super::CandleField
/// [`CalendarField`]: super::calendar::CalendarField
pub trait BarMeasure {
    /// This candle's contribution to the running bucket. Must be non-negative:
    /// a negative contribution would let a bucket walk away from its threshold.
    fn amount(candle: &Candle) -> Real;
}

/// Traded quantity — [`VolumeBars`].
#[derive(Debug, Clone, Copy, Default)]
pub struct VolumeMeasure;

impl BarMeasure for VolumeMeasure {
    fn amount(candle: &Candle) -> Real {
        candle.volume
    }
}

/// Traded *notional* — [`DollarBars`].
///
/// Estimated as `typical × volume` rather than `close × volume`: the typical
/// price is the bar's own `(high + low + close) / 3`, so it does not hang the
/// whole bar's notional on the single print that happened to end it.
///
/// It is still an estimate. A venue that publishes a quote-volume column
/// (Binance's `quote_volume`) states the figure exactly, and joining that in as
/// an overlay is more accurate than any reconstruction from OHLCV.
#[derive(Debug, Clone, Copy, Default)]
pub struct DollarMeasure;

impl BarMeasure for DollarMeasure {
    fn amount(candle: &Candle) -> Real {
        candle.typical() * candle.volume
    }
}

/// Aggregates base candles into one bar per `threshold` units of a
/// [`BarMeasure`] — volume bars, dollar bars, and anything else that closes a
/// bar on *how much traded* rather than on how much time passed.
///
/// Sibling of [`Resample`], and deliberately the same shape: emits
/// `Some(Candle)` on the tick that completes a bucket and `None` between, so
/// every recursive smoother downstream already treats it correctly (`None`
/// means "don't advance"). The correct ordering is the same too —
/// Accumulate → recursive smoother → [`Latch`].
///
/// # Why sample this way
///
/// Time bars over-sample a quiet market and under-sample a busy one, which
/// makes their returns heteroskedastic and serially dependent. Sampling on
/// activity instead gives returns closer to IID — which is the assumption the
/// significance machinery in this crate leans on (PSR, DSR, and the block
/// bootstrap in [`crate::montecarlo`]). The gain is statistical, not cosmetic.
///
/// # Two approximations, both stated
///
/// **Buckets are whole base candles.** A bucket closes on the first candle that
/// takes the running total *at or past* the threshold, so a bar generally
/// overshoots, and the overshoot is **not carried** into the next bucket.
/// Carrying it would be worse here: one base candle can exceed the threshold
/// several times over, and the carry would then emit a run of bars containing
/// no new data at all. Feed finer base candles to shrink the overshoot — dollar
/// bars off `1m` klines are close enough for most work, which is the point.
///
/// **Warm-up is not a bar count.** Unlike `Resample`, whose first emission lands
/// at a known sample, a threshold gives no deterministic answer: how many base
/// candles fill a bucket is data. So [`warm_up_bars`](Indicator::warm_up_bars)
/// reports the inner source's own warm-up — the earliest an emission *could*
/// happen — and the first one may arrive well after it. That is the
/// "data permitting" clause in the [`Indicator`] contract, load-bearing here
/// rather than incidental.
///
/// ```
/// use fugazi::prelude::*;
/// use fugazi::indicators::{Current, DollarBars, Ema, Latch};
///
/// // EMA-20 of the close of every $1M of traded notional.
/// let _ema = Latch::new(Ema::new(DollarBars::new(Current::candle(), 1e6).close(), 20));
/// ```
///
/// # Panics
/// Constructor panics unless `threshold` is finite and strictly positive.
#[derive(Debug, Clone, SaveState)]
pub struct Accumulate<S, M> {
    #[state(source)]
    inner: S,
    #[state(config)]
    threshold: Real,
    /// The bucket's running measure. Saved state: dropping it on resume would
    /// restart the bucket mid-flight and emit a short bar at the seam.
    running: Real,
    open: Option<Real>,
    high: Real,
    low: Real,
    close: Real,
    volume: Real,
    /// Latest emitted bar; `None` on any non-boundary tick. A recomputed cache
    /// — set every `update` from the (restored) accumulators — so it is not
    /// part of the saved state.
    #[state(skip)]
    pub value: Option<Candle>,
    #[state(skip)]
    _measure: std::marker::PhantomData<fn() -> M>,
}

/// One bar per `threshold` units of traded quantity.
pub type VolumeBars<S> = Accumulate<S, VolumeMeasure>;
/// One bar per `threshold` units of traded notional.
pub type DollarBars<S> = Accumulate<S, DollarMeasure>;

impl<S, M> Accumulate<S, M> {
    /// Aggregate `inner`'s candles into buckets of `threshold` measure units.
    ///
    /// # Panics
    /// Panics unless `threshold` is finite and `> 0`.
    pub fn new(inner: S, threshold: Real) -> Self {
        assert!(
            threshold > 0.0 && threshold.is_finite(),
            "accumulate threshold must be finite and > 0, got {threshold}"
        );
        Self {
            inner,
            threshold,
            running: 0.0,
            open: None,
            high: 0.0,
            low: 0.0,
            close: 0.0,
            volume: 0.0,
            value: None,
            _measure: std::marker::PhantomData,
        }
    }

    /// The measure total one bucket holds before it emits.
    pub fn threshold(&self) -> Real {
        self.threshold
    }
}

impl<S: Indicator<Output = Candle>, M: BarMeasure> Accumulate<S, M> {
    /// Project the emitted bar's `close`.
    pub fn close(self) -> Component<Self> {
        Component::new(self, |c: Candle| c.close)
    }

    /// Project the emitted bar's `open`.
    pub fn open(self) -> Component<Self> {
        Component::new(self, |c: Candle| c.open)
    }

    /// Project the emitted bar's `high`.
    pub fn high(self) -> Component<Self> {
        Component::new(self, |c: Candle| c.high)
    }

    /// Project the emitted bar's `low`.
    pub fn low(self) -> Component<Self> {
        Component::new(self, |c: Candle| c.low)
    }

    /// Project the emitted bar's `volume`.
    pub fn volume(self) -> Component<Self> {
        Component::new(self, |c: Candle| c.volume)
    }

    /// Project the emitted bar's typical price (`(high + low + close) / 3`).
    pub fn typical(self) -> Component<Self> {
        Component::new(self, |c: Candle| c.typical())
    }

    /// Project the emitted bar's median price (`(high + low) / 2`).
    pub fn median(self) -> Component<Self> {
        Component::new(self, |c: Candle| c.median())
    }
}

impl<S: Indicator<Output = Candle>, M: BarMeasure> Indicator for Accumulate<S, M> {
    type Input = S::Input;
    type Output = Candle;

    fn update(&mut self, input: Self::Input) -> Option<Candle> {
        self.value = match self.inner.update(input) {
            Some(bar) => {
                if self.open.is_none() {
                    self.open = Some(bar.open);
                    self.high = bar.high;
                    self.low = bar.low;
                    self.volume = 0.0;
                } else {
                    if bar.high > self.high {
                        self.high = bar.high;
                    }
                    if bar.low < self.low {
                        self.low = bar.low;
                    }
                }
                self.close = bar.close;
                self.volume += bar.volume;
                // A negative or non-finite contribution is ignored rather than
                // allowed to walk the bucket away from its threshold — the same
                // refusal-to-poison rule the wallet applies to a bad price.
                let amount = M::amount(&bar);
                if amount.is_finite() && amount > 0.0 {
                    self.running += amount;
                }
                if self.running >= self.threshold {
                    let out = Candle::new(
                        self.open.take().unwrap(),
                        self.high,
                        self.low,
                        self.close,
                        self.volume,
                    );
                    // Reset rather than carry the overshoot — see the type doc.
                    self.running = 0.0;
                    Some(out)
                } else {
                    None
                }
            }
            None => None,
        };
        self.value
    }

    fn value(&self) -> Option<Candle> {
        self.value
    }

    fn warm_up_bars(&self) -> usize {
        // The earliest an emission *could* land. How many base candles fill a
        // bucket is data, not configuration, so this is a lower bound rather
        // than the exact sample the first output appears on. See the type doc.
        self.inner.warm_up_bars()
    }

    fn unstable_bars(&self) -> usize {
        self.inner.unstable_bars()
    }

    fn reset(&mut self) {
        self.inner.reset();
        self.running = 0.0;
        self.open = None;
        // Every accumulator, not just the live ones — `reset` returns the
        // indicator to its *constructed* condition, and `save_state` is the
        // only complete view of whether it did. See `Resample::reset`.
        self.high = 0.0;
        self.low = 0.0;
        self.close = 0.0;
        self.volume = 0.0;
        self.value = None;
    }
}

#[cfg(test)]
mod accumulate_tests {
    use super::*;
    use crate::indicators::{CurrentBar, Identity};

    /// A candle at `close` with the given traded quantity.
    fn vol_bar(close: Real, volume: Real) -> Candle {
        Candle::new(close, close, close, close, volume)
    }

    fn feed<M: BarMeasure>(threshold: Real, bars: &[Candle]) -> Vec<Option<Candle>> {
        let mut acc: Accumulate<Identity<Candle>, M> = Accumulate::new(Identity::new(), threshold);
        bars.iter().map(|b| acc.update(*b)).collect()
    }

    /// A bucket closes on the candle that takes it at or past the threshold,
    /// and the aggregate is a genuine OHLCV roll-up of what went into it.
    #[test]
    fn volume_bars_close_on_the_threshold_and_aggregate_correctly() {
        let bars = [
            Candle::new(10.0, 12.0, 9.0, 11.0, 40.0),
            Candle::new(11.0, 13.0, 10.0, 12.0, 40.0),
            Candle::new(12.0, 14.0, 11.0, 13.0, 40.0),
        ];
        let out = feed::<VolumeMeasure>(100.0, &bars);
        assert_eq!(out[0], None, "40 < 100");
        assert_eq!(out[1], None, "80 < 100");
        let emitted = out[2].expect("120 >= 100 closes the bucket");
        assert_eq!(emitted.open, 10.0, "open of the first candle in the bucket");
        assert_eq!(emitted.high, 14.0, "running high across all three");
        assert_eq!(emitted.low, 9.0, "running low across all three");
        assert_eq!(emitted.close, 13.0, "close of the closing candle");
        assert_eq!(emitted.volume, 120.0, "summed volume");
    }

    /// Dollar bars measure notional, so an identical volume profile at a higher
    /// price fills a bucket sooner. This is the whole distinction between the
    /// two measures — if it did not hold, one is silently the other.
    #[test]
    fn dollar_bars_measure_notional_not_quantity() {
        // 10 units at a price of 10 = 100 notional: one bar per candle.
        let cheap = [vol_bar(10.0, 10.0), vol_bar(10.0, 10.0)];
        let out = feed::<DollarMeasure>(100.0, &cheap);
        assert!(out[0].is_some() && out[1].is_some());

        // The same 10 units at a price of 5 = 50 notional: two candles per bar.
        let dear = [vol_bar(5.0, 10.0), vol_bar(5.0, 10.0)];
        let out = feed::<DollarMeasure>(100.0, &dear);
        assert_eq!(out[0], None);
        assert!(out[1].is_some());
    }

    /// The overshoot is dropped rather than carried. Documented behaviour, and
    /// pinned because the alternative — carrying it — would emit a run of bars
    /// containing no new data after one outsized candle.
    #[test]
    fn the_overshoot_is_not_carried_into_the_next_bucket() {
        // 500 units against a threshold of 100: one bar, not five.
        let bars = [vol_bar(1.0, 500.0), vol_bar(1.0, 10.0), vol_bar(1.0, 10.0)];
        let out = feed::<VolumeMeasure>(100.0, &bars);
        assert!(out[0].is_some(), "the outsized candle closes a bucket");
        assert_eq!(out[1], None, "and the next bucket starts from zero");
        assert_eq!(out[2], None, "20 < 100");
    }

    /// A zero-volume candle contributes nothing and must not close a bucket —
    /// otherwise a halted or synthetic bar emits a bar of no traded activity.
    #[test]
    fn a_zero_volume_candle_does_not_close_a_bucket() {
        let bars = [vol_bar(1.0, 0.0), vol_bar(1.0, 0.0)];
        assert_eq!(feed::<VolumeMeasure>(1.0, &bars), vec![None, None]);
    }

    /// Warm-up is a lower bound, not a count: with a threshold no run of bars
    /// reaches, nothing is ever emitted even far past `warm_up_bars()`.
    #[test]
    fn warm_up_is_a_lower_bound_not_a_promise() {
        let mut acc: VolumeBars<CurrentBar> = Accumulate::new(CurrentBar::new(), 1e12);
        assert_eq!(acc.warm_up_bars(), CurrentBar::new().warm_up_bars());
        for _ in 0..50 {
            assert_eq!(acc.update(crate::types::Atom::new(vol_bar(1.0, 1.0))), None);
        }
    }

    /// `reset` returns every accumulator to its constructed condition, so a
    /// mid-bucket reset does not leak the previous run's high into the next bar.
    #[test]
    fn reset_clears_a_part_filled_bucket() {
        let mut acc: Accumulate<Identity<Candle>, VolumeMeasure> =
            Accumulate::new(Identity::new(), 100.0);
        assert_eq!(acc.update(Candle::new(50.0, 99.0, 1.0, 50.0, 40.0)), None);
        acc.reset();
        let out = acc
            .update(Candle::new(10.0, 11.0, 9.0, 10.0, 200.0))
            .expect("200 >= 100");
        assert_eq!(out.open, 10.0);
        assert_eq!(out.high, 11.0, "not the 99.0 from before the reset");
        assert_eq!(out.low, 9.0);
        assert_eq!(out.volume, 200.0, "not 240.0");
    }

    /// The constructor refuses a threshold that could never close a bucket.
    #[test]
    #[should_panic(expected = "accumulate threshold must be finite and > 0")]
    fn a_non_positive_threshold_panics() {
        let _: VolumeBars<CurrentBar> = Accumulate::new(CurrentBar::new(), 0.0);
    }
}
