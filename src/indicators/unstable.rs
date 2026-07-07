//! Passthrough wrapper that zeroes a source's `unstable_period()`.
//!
//! `Unstable<S>` is the opt-in override for the "wait until every source is
//! past its unstable tail" default a strategy or driver applies from a chain's
//! `stable_period()`. The wrapper delegates every method to its inner source
//! *except* [`unstable_period`](Indicator::unstable_period), which it forces to
//! zero — telling the world "I accept this source's IIR settling as ready".
//! `warm_up_period()` is untouched, so the FIR head still has to fill.

use crate::indicator::Indicator;

/// A transparent wrapper over `S` that reports `unstable_period() = 0`.
///
/// Wraps any indicator (real- or bool-valued, any input type) and forwards
/// every update and value read to it unchanged. The only lie it tells is about
/// stability: the wrapped source's IIR unstable tail is treated as already
/// settled, so a caller counting off `stable_period()` samples (e.g.
/// [`SingleAssetStrategy::is_ready`](crate::strategies::SingleAssetStrategy))
/// only waits for the source's `warm_up_period()`.
///
/// This is the counterpart to the strategy layer's "safe default": readiness
/// gates trading until every consulted source's `stable_period()` has elapsed,
/// and `Unstable` is the explicit way to opt out for a subtree whose settling
/// tail the caller is happy to trade through.
///
/// ```
/// use fugazi::prelude::*;
/// use fugazi::indicators::{Current, Ema, Unstable};
///
/// let raw = Ema::new(Current::close(), 20);
/// let wrapped = Unstable::new(Ema::new(Current::close(), 20));
/// // Same warm-up, same output — only the reported unstable tail changes.
/// assert_eq!(wrapped.warm_up_period(), raw.warm_up_period());
/// assert_eq!(wrapped.unstable_period(), 0);
/// assert_eq!(wrapped.stable_period(), raw.warm_up_period());
/// assert!(raw.stable_period() > raw.warm_up_period());
/// ```
#[derive(Debug, Clone)]
pub struct Unstable<S> {
    source: S,
}

impl<S> Unstable<S> {
    /// Wrap `source` so it reports `unstable_period() = 0` while otherwise
    /// behaving identically.
    pub fn new(source: S) -> Self {
        Self { source }
    }

    /// The wrapped source.
    pub fn source(&self) -> &S {
        &self.source
    }

    /// Consume the wrapper and return the inner source.
    pub fn into_inner(self) -> S {
        self.source
    }
}

impl<S: Indicator> Indicator for Unstable<S> {
    type Input = S::Input;
    type Output = S::Output;

    fn update(&mut self, input: Self::Input) -> Option<Self::Output> {
        self.source.update(input)
    }

    fn value(&self) -> Option<Self::Output> {
        self.source.value()
    }

    fn is_ready(&self) -> bool {
        self.source.is_ready()
    }

    fn warm_up_period(&self) -> usize {
        self.source.warm_up_period()
    }

    fn unstable_period(&self) -> usize {
        0
    }

    fn reset(&mut self) {
        self.source.reset();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::indicators::{Current, Ema};
    use crate::types::{Candle, Real};

    fn bar(v: Real) -> Candle {
        Candle::new(v, v, v, v, 0.0)
    }

    #[test]
    fn zeros_unstable_period_and_forwards_the_rest() {
        let raw = Ema::new(Current::close(), 5);
        let warm = raw.warm_up_period();
        let settle = raw.unstable_period();
        assert!(settle > 0, "Ema-5 should have a real unstable tail");

        let wrapped = Unstable::new(Ema::new(Current::close(), 5));
        assert_eq!(wrapped.warm_up_period(), warm);
        assert_eq!(wrapped.unstable_period(), 0);
        assert_eq!(wrapped.stable_period(), warm);
    }

    #[test]
    fn passes_updates_and_values_through() {
        let mut wrapped = Unstable::new(Ema::new(Current::close(), 3));
        let mut plain = Ema::new(Current::close(), 3);
        for px in [10.0, 11.0, 12.0, 13.0, 14.0] {
            let w = wrapped.update(bar(px).into());
            let p = plain.update(bar(px).into());
            assert_eq!(w, p);
        }
        assert_eq!(wrapped.value(), plain.value());
    }

    #[test]
    fn reset_forwards() {
        let mut wrapped = Unstable::new(Ema::new(Current::close(), 3));
        wrapped.update(bar(10.0).into());
        wrapped.update(bar(11.0).into());
        wrapped.reset();
        assert!(!wrapped.is_ready());
    }
}
