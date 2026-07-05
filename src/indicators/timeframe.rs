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
#[derive(Debug, Clone)]
pub struct Resample<S> {
    inner: S,
    every: usize,
    count: usize,
    open: Option<Real>,
    high: Real,
    low: Real,
    close: Real,
    volume: Real,
    /// Latest emitted higher-timeframe candle; `None` on any non-boundary tick.
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

    fn warm_up_period(&self) -> usize {
        // The k-th inner-emitted candle arrives at input sample
        // `inner.warm_up_period() + k - 1`. The first bucket completes on the
        // `every`-th inner emission — sample `inner.warm_up_period() + every - 1`.
        self.inner.warm_up_period() + self.every - 1
    }

    fn unstable_period(&self) -> usize {
        // Windowed / FIR: no additional instability of its own; downstream
        // recursive smoothers reason in inner-emit units, so any base-bar
        // interpretation of `stable_period()` is only correct in higher-timeframe
        // sample counts — not in base bars.
        self.inner.unstable_period()
    }

    fn reset(&mut self) {
        self.inner.reset();
        self.count = 0;
        self.open = None;
        self.value = None;
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
/// stable to [`Stable`](super::Stable) or the CLI's gate.
#[derive(Clone)]
pub struct Latch<S: Indicator> {
    inner: S,
    /// The last emitted output; `None` until the inner source has produced one.
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

    fn warm_up_period(&self) -> usize {
        // `max(1)` guards a `warm_up = 0` inner — Latch holds nothing before
        // its first `update`, so its first `Some` is at update ≥ 1.
        self.inner.warm_up_period().max(1)
    }

    fn unstable_period(&self) -> usize {
        self.inner.unstable_period()
    }

    fn reset(&mut self) {
        self.inner.reset();
        self.value = None;
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
            assert_eq!(r.update(c).unwrap(), c);
        }
    }

    #[test]
    fn resample_emits_on_the_nth_bar_only() {
        let mut r = Resample::new(Current::candle(), 4);
        let bars = bars();
        for c in &bars[..3] {
            assert!(r.update(*c).is_none());
        }
        let htf1 = r.update(bars[3]).unwrap();
        assert_eq!(htf1.open, 10.0);
        assert_eq!(htf1.close, 14.0);
        assert_eq!(htf1.high, 15.0);
        assert_eq!(htf1.low, 9.0);
        assert_eq!(htf1.volume, 100.0 + 200.0 + 150.0 + 50.0);
        for c in &bars[4..7] {
            assert!(r.update(*c).is_none());
        }
        let htf2 = r.update(bars[7]).unwrap();
        assert_eq!(htf2.open, 14.0);
        assert_eq!(htf2.close, 18.5);
        assert_eq!(htf2.high, 19.0);
        assert_eq!(htf2.low, 13.5);
        assert_eq!(htf2.volume, 300.0 + 200.0 + 100.0 + 250.0);
    }

    #[test]
    fn resample_warm_up_lands_on_the_first_emit() {
        let mut r = Resample::new(Current::candle(), 4);
        assert_eq!(r.warm_up_period(), 4);
        for (i, c) in bars().into_iter().enumerate() {
            let sample = i + 1;
            let ready = r.update(c).is_some();
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
            r.update(c);
        }
        r.reset();
        let bars = bars();
        assert!(r.update(bars[0]).is_none());
        assert!(r.update(bars[1]).is_none());
        let out = r.update(bars[2]).unwrap();
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
        assert_eq!(latch.update(bar(1.0)), None);
        assert_eq!(latch.update(bar(2.0)), None);
        let first = latch.update(bar(3.0)).unwrap();
        assert_eq!(first.close, 3.0);
        assert_eq!(latch.update(bar(4.0)).unwrap().close, 3.0);
        assert_eq!(latch.update(bar(5.0)).unwrap().close, 3.0);
        assert_eq!(latch.update(bar(6.0)).unwrap().close, 6.0);
    }

    #[test]
    fn latch_returns_none_before_the_source_has_ever_emitted() {
        let mut latch = Latch::new(Resample::new(Current::candle(), 4));
        for close in [1.0, 2.0, 3.0] {
            assert_eq!(latch.update(bar(close)), None);
            assert_eq!(latch.value, None);
        }
    }

    #[test]
    fn latch_unstable_period_passes_through() {
        let raw = Ema::new(Current::close(), 20);
        let latched = Latch::new(Ema::new(Current::close(), 20));
        assert_eq!(latched.unstable_period(), raw.unstable_period());
        assert_eq!(latched.warm_up_period(), raw.warm_up_period());
    }

    #[test]
    fn latch_reset_clears_the_held_value() {
        let mut latch = Latch::new(Resample::new(Current::candle(), 2));
        latch.update(bar(1.0));
        latch.update(bar(2.0));
        assert!(latch.value.is_some());
        latch.reset();
        assert!(latch.value.is_none());
        assert!(latch.update(bar(3.0)).is_none());
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
                    .update(Candle::new(close, close, close, close, 0.0))
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
            let c = correct.update(*bar);
            let w = wrong.update(*bar);
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
