//! Signal reporting whether a source has been fed at least its
//! `stable_period()` samples.

use std::marker::PhantomData;

use crate::indicator::Indicator;

/// A `bool`-output signal that reports whether **enough samples have elapsed
/// for a source to be past its unstable tail**.
///
/// Doesn't hold the source it's checking — captures the source's
/// [`stable_period`](Indicator::stable_period) at construction and then just
/// counts the samples fed to itself. So an `!and`-composed entry like
///
/// ```yaml
/// enter: !and
///   - !crosses_above { lhs: !ema { period: 12 }, rhs: !ema { period: 26 } }
///   - !stable { signal: !crosses_above { lhs: !ema { period: 12 }, rhs: !ema { period: 26 } } }
/// ```
///
/// fires only once the crossover signal is both currently true *and* has been
/// fed at least its own `stable_period()` samples.
///
/// Once at least `stable_period()` samples have arrived, [`update`](Indicator::update)
/// returns `Some(true)`; before that it returns `Some(false)`. `warm_up_period()`
/// is `0` and `unstable_period()` is `0` — the check is always available.
///
/// ```
/// use fugazi::prelude::*;
/// use fugazi::indicators::{Current, Ema, Stable};
///
/// let ema = Ema::new(Current::close(), 3);
/// // "true from the bar the Ema is past its unstable tail":
/// let mut ready = Stable::<fugazi::Candle>::from_source(&ema);
/// // Feed 11 candles (Ema-3's stable_period) — the 11th update flips true.
/// # let _ = &mut ready;
/// ```
#[derive(Debug, Clone)]
pub struct Stable<In> {
    stable_period: usize,
    samples: usize,
    _in: PhantomData<fn(In)>,
}

impl<In> Stable<In> {
    /// Construct from an explicit sample threshold. `update` returns
    /// `Some(true)` from the `stable_period`-th sample onwards.
    pub fn from_period(stable_period: usize) -> Self {
        Self {
            stable_period,
            samples: 0,
            _in: PhantomData,
        }
    }

    /// Capture `source`'s [`stable_period`](Indicator::stable_period) and
    /// build a check against it. `source` is only read once — the resulting
    /// `Stable` doesn't hold it.
    pub fn from_source<S: Indicator>(source: &S) -> Self {
        Self::from_period(source.stable_period())
    }

    /// The captured threshold, in samples.
    pub fn threshold(&self) -> usize {
        self.stable_period
    }
}

impl<In> Indicator for Stable<In> {
    type Input = In;
    type Output = bool;

    fn update(&mut self, _input: In) -> Option<bool> {
        self.samples = self.samples.saturating_add(1);
        Some(self.samples >= self.stable_period)
    }

    fn value(&self) -> Option<bool> {
        Some(self.samples >= self.stable_period)
    }

    fn warm_up_period(&self) -> usize {
        0
    }

    fn unstable_period(&self) -> usize {
        0
    }

    fn reset(&mut self) {
        self.samples = 0;
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
    fn flips_true_after_stable_period_samples() {
        let ema = Ema::new(Current::close(), 3);
        let period = ema.stable_period();
        assert!(period > 1, "Ema-3 must have a real settling tail");
        let mut check: Stable<Candle> = Stable::from_source(&ema);

        for i in 1..period {
            assert_eq!(
                check.update(bar(i as Real)),
                Some(false),
                "sample {i} should still report unstable"
            );
        }
        // The `stable_period`-th sample flips the check.
        assert_eq!(check.update(bar(period as Real)), Some(true));
        assert_eq!(check.update(bar((period + 1) as Real)), Some(true));
    }

    #[test]
    fn value_matches_update_return() {
        let mut check: Stable<Real> = Stable::from_period(3);
        assert_eq!(check.value(), Some(false));
        check.update(0.0);
        assert_eq!(check.value(), Some(false));
        check.update(0.0);
        assert_eq!(check.value(), Some(false));
        check.update(0.0);
        assert_eq!(check.value(), Some(true));
    }

    #[test]
    fn reset_zeros_the_counter() {
        let mut check: Stable<Real> = Stable::from_period(2);
        check.update(0.0);
        check.update(0.0);
        assert_eq!(check.value(), Some(true));
        check.reset();
        assert_eq!(check.value(), Some(false));
        assert_eq!(check.update(0.0), Some(false));
    }
}
