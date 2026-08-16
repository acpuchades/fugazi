//! Signal reporting whether a source has been fed at least its
//! `stable_bars()` samples.

use std::marker::PhantomData;

use fugazi_derive::SaveState;

use crate::indicator::Indicator;

/// A `bool`-output signal that reports whether **enough samples have elapsed
/// for a source to be past its unstable tail**.
///
/// Doesn't hold the source it's checking — captures the source's
/// [`stable_bars`](Indicator::stable_bars) at construction and then just
/// counts the samples fed to itself. So an `!and`-composed entry like
///
/// ```yaml
/// enter: !and
///   - !crosses_above { lhs: !ema { period: 12 }, rhs: !ema { period: 26 } }
///   - !stable { signal: !crosses_above { lhs: !ema { period: 12 }, rhs: !ema { period: 26 } } }
/// ```
///
/// fires only once the crossover signal is both currently true *and* has been
/// fed at least its own `stable_bars()` samples.
///
/// Once at least `stable_bars()` samples have arrived, [`update`](Indicator::update)
/// returns `Some(true)`; before that it returns `Some(false)`. `warm_up_bars()`
/// is `0` and `unstable_bars()` is `0` — the check is always available.
///
/// ```
/// use fugazi::prelude::*;
/// use fugazi::indicators::{Current, Ema, Stable};
///
/// let ema = Ema::new(Current::close(), 3);
/// // "true from the bar the Ema is past its unstable tail":
/// let mut ready = Stable::<fugazi::Candle>::from_source(&ema);
/// // Feed 11 candles (Ema-3's stable_bars) — the 11th update flips true.
/// # let _ = &mut ready;
/// ```
#[derive(Debug, Clone, SaveState)]
pub struct Stable<In> {
    stable_bars: usize,
    samples: usize,
    #[state(skip)]
    _in: PhantomData<fn(In)>,
}

impl<In> Stable<In> {
    /// Construct from an explicit sample threshold. `update` returns
    /// `Some(true)` from the `stable_bars`-th sample onwards.
    pub fn from_bars(stable_bars: usize) -> Self {
        Self {
            stable_bars,
            samples: 0,
            _in: PhantomData,
        }
    }

    /// Capture `source`'s [`stable_bars`](Indicator::stable_bars) and
    /// build a check against it. `source` is only read once — the resulting
    /// `Stable` doesn't hold it.
    pub fn from_source<S: Indicator>(source: &S) -> Self {
        Self::from_bars(source.stable_bars())
    }

    /// The captured threshold, in samples.
    pub fn threshold(&self) -> usize {
        self.stable_bars
    }
}

impl<In> Indicator for Stable<In> {
    type Input = In;
    type Output = bool;

    fn update(&mut self, _input: In) -> Option<bool> {
        self.samples = self.samples.saturating_add(1);
        Some(self.samples >= self.stable_bars)
    }

    fn value(&self) -> Option<bool> {
        Some(self.samples >= self.stable_bars)
    }

    fn warm_up_bars(&self) -> usize {
        0
    }

    fn unstable_bars(&self) -> usize {
        0
    }

    fn reset(&mut self) {
        self.samples = 0;
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
    use crate::types::{Candle, Real};

    fn bar(v: Real) -> Candle {
        Candle::new(v, v, v, v, 0.0)
    }

    #[test]
    fn flips_true_after_stable_bars() {
        let ema = Ema::new(Current::close(), 3);
        let period = ema.stable_bars();
        assert!(period > 1, "Ema-3 must have a real settling tail");
        let mut check: Stable<Candle> = Stable::from_source(&ema);

        for i in 1..period {
            assert_eq!(
                check.update(bar(i as Real)),
                Some(false),
                "sample {i} should still report unstable"
            );
        }
        // The `stable_bars`-th sample flips the check.
        assert_eq!(check.update(bar(period as Real)), Some(true));
        assert_eq!(check.update(bar((period + 1) as Real)), Some(true));
    }

    #[test]
    fn value_matches_update_return() {
        let mut check: Stable<Real> = Stable::from_bars(3);
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
        let mut check: Stable<Real> = Stable::from_bars(2);
        check.update(0.0);
        check.update(0.0);
        assert_eq!(check.value(), Some(true));
        check.reset();
        assert_eq!(check.value(), Some(false));
        assert_eq!(check.update(0.0), Some(false));
    }
}
