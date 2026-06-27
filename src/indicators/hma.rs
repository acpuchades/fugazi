use crate::indicator::Indicator;
use crate::indicators::Wma;
use crate::indicators::stats::WmaState;
use crate::types::Real;

/// Hull Moving Average of a source.
///
/// Hull's lag-reducing average: `WMA(2·WMA(n/2) − WMA(n))` re-smoothed over
/// `√n` steps. It owns its input source and clones it to drive the two inner
/// [`Wma`]s (hence `S: Clone` and `Input: Clone`):
/// `Hma::new(Current::close(), 16)`.
///
/// The half-length WMA reacts quickly while `2·half − full` cancels much of the
/// lag; the final `√n` WMA smooths the result. Ready after `n + round(√n) − 1`
/// samples — the longer inner `WMA(n)` plus the final `√n` smoothing window.
#[derive(Debug, Clone)]
pub struct Hma<S> {
    half: Wma<S>,
    full: Wma<S>,
    smooth: WmaState,
    /// Latest output value; `None` until warmed up.
    pub value: Option<Real>,
}

impl<S: Clone> Hma<S> {
    /// # Panics
    /// Panics if `period` is zero.
    pub fn new(source: S, period: usize) -> Self {
        assert!(period > 0, "HMA period must be greater than zero");
        let half = (period / 2).max(1);
        let sqrt = (period as Real).sqrt().round() as usize;
        Self {
            half: Wma::new(source.clone(), half),
            full: Wma::new(source, period),
            smooth: WmaState::new(sqrt.max(1)),
            value: None,
        }
    }
}

impl<S> Indicator for Hma<S>
where
    S: Indicator<Output = Real> + Clone,
    S::Input: Clone,
{
    type Input = S::Input;
    type Output = Real;

    fn update(&mut self, input: Self::Input) -> Option<Real> {
        let half = self.half.update(input.clone());
        let full = self.full.update(input);
        self.value = match (half, full) {
            (Some(half), Some(full)) => self.smooth.update(2.0 * half - full),
            _ => None,
        };
        self.value
    }

    fn value(&self) -> Option<Real> {
        self.value
    }

    fn reset(&mut self) {
        self.half.reset();
        self.full.reset();
        self.smooth.reset();
        self.value = None;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::indicators::Identity;

    #[test]
    fn warms_up_then_tracks_hull_formula() {
        // period 4: half = WMA(2), full = WMA(4), final smoothing = WMA(2).
        let mut hma = Hma::new(Identity::new(), 4);
        assert_eq!(hma.update(1.0), None); // full WMA(4) not ready
        assert_eq!(hma.update(2.0), None);
        assert_eq!(hma.update(3.0), None);
        assert_eq!(hma.update(4.0), None); // raw series has one value; √n window not full
        // Hand-computed: raw = 2·WMA2 − WMA4 = {13/3, 16/3}; WMA2 of those = 5.0.
        let out = hma.update(5.0).unwrap();
        assert!((out - 5.0).abs() < 1e-12);
    }
}
