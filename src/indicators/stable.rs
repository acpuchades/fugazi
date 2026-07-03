//! Masking a source until its whole chain has settled.

use crate::indicator::Indicator;

/// Masks a source until its [`stable_period`](Indicator::stable_period) has
/// elapsed, converting the soft unstable period into hard warm-up.
///
/// A recursive (IIR) source — EMA, RSI, ATR, and everything built on them —
/// starts emitting `Some` at its warm-up but stays contaminated by its seed for
/// a while after (the [`unstable_period`](Indicator::unstable_period)). `Stable`
/// wraps any source and suppresses its output until `source.stable_period()`
/// samples have been consumed, then passes it through unchanged. Its own
/// introspection reflects the conversion: `warm_up_period()` is the source's
/// `stable_period()`, and `unstable_period()` is `0`.
///
/// The wrapped source keeps advancing underneath while masked, so downstream
/// state (a comparison, a [`Change`](super::Change) edge) is already correct
/// when values start flowing — no spurious edge fires on the unmask, the same
/// guarantee ordinary warm-up gives.
///
/// Output-agnostic, so it gates a real-valued source and a whole boolean signal
/// alike — wrap a strategy's entry signal and no trade can trigger off a
/// seed-contaminated value. Build it with `.stable()`
/// ([`IndicatorExt`](super::IndicatorExt) /
/// [`BoolIndicatorExt`](super::BoolIndicatorExt)) or [`Stable::new`]:
///
/// ```
/// use fugazi::prelude::*;
/// use fugazi::indicators::{Current, Ema};
///
/// let ema = Ema::new(Current::close(), 20);
/// let stable_period = ema.stable_period();
/// let gated = ema.stable(); // = Stable::new(ema)
/// assert_eq!(gated.warm_up_period(), stable_period);
/// assert_eq!(gated.unstable_period(), 0);
/// ```
///
/// No public `value` field: the latest output is the source's own, gated —
/// [`value`](Indicator::value) delegates, so an already-stable source
/// (`stable_period() == 0`, e.g. a constant) stays ready untouched.
#[derive(Debug, Clone)]
pub struct Stable<S> {
    source: S,
    /// Samples consumed so far.
    seen: usize,
}

impl<S: Indicator> Stable<S> {
    /// Wrap `source`, masking its output for its first `stable_period()`
    /// samples.
    pub fn new(source: S) -> Self {
        Self { source, seen: 0 }
    }
}

impl<S: Indicator> Indicator for Stable<S> {
    type Input = S::Input;
    type Output = S::Output;

    fn update(&mut self, input: Self::Input) -> Option<S::Output> {
        let out = self.source.update(input);
        self.seen += 1;
        if self.seen >= self.source.stable_period() {
            out
        } else {
            None
        }
    }

    fn value(&self) -> Option<S::Output> {
        if self.seen >= self.source.stable_period() {
            self.source.value()
        } else {
            None
        }
    }

    fn warm_up_period(&self) -> usize {
        // The source's first *converged* output; everything before is masked.
        // Exact: the gate opens at sample stable_period(), which is never
        // earlier than the source's own warm-up.
        self.source.stable_period()
    }

    fn unstable_period(&self) -> usize {
        // The masked stretch covers the source's settling — the whole point.
        0
    }

    fn reset(&mut self) {
        self.source.reset();
        self.seen = 0;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::indicators::{Ema, Identity, Value};
    use crate::types::Real;

    #[test]
    fn masks_until_the_stable_period_then_passes_through() {
        let mut gated = Stable::new(Ema::new(Identity::new(), 3));
        let mut raw = Ema::new(Identity::new(), 3);
        let stable = raw.stable_period();
        assert!(stable > raw.warm_up_period(), "EMA must have a soft tail");
        for i in 0..stable + 5 {
            let x = (i as Real).sin() + 2.0;
            let masked = gated.update(x);
            let reference = raw.update(x);
            if i + 1 < stable {
                assert_eq!(masked, None, "sample {} should be masked", i + 1);
            } else {
                assert_eq!(masked, reference, "pass-through from sample {stable}");
            }
        }
    }

    #[test]
    fn already_stable_sources_stay_ready_untouched() {
        let gated = Stable::new(Value::<Real>::new(7.0));
        assert_eq!(gated.warm_up_period(), 0);
        assert!(gated.is_ready());
        assert_eq!(gated.value(), Some(7.0));
    }

    #[test]
    fn reset_restores_the_mask() {
        let mut gated = Stable::new(Ema::new(Identity::new(), 3));
        for i in 0..gated.warm_up_period() {
            gated.update(i as Real);
        }
        assert!(gated.is_ready());
        gated.reset();
        assert!(!gated.is_ready());
        assert_eq!(gated.update(1.0), None);
    }
}
