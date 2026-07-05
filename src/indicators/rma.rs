use crate::indicator::Indicator;
use crate::indicators::smoothing::WilderState;
use crate::types::Real;

/// Wilder's smoothing (RMA / SMMA) of a source.
///
/// Owns its input source: `Rma::new(Current::close(), 14)`, or
/// `Rma::new(TrueRange::new(), 14)` — the latter is exactly the ATR. Seeds with
/// the mean of the source's first `period` outputs, then applies Wilder's
/// recursion. Produces `None` until `period` source values have been seen.
#[derive(Debug, Clone)]
pub struct Rma<S> {
    source: S,
    state: WilderState,
    /// Latest output value; `None` until warmed up.
    pub value: Option<Real>,
}

impl<S> Rma<S> {
    /// # Panics
    /// Panics if `period` is zero.
    pub fn new(source: S, period: usize) -> Self {
        Self {
            source,
            state: WilderState::new(period),
            value: None,
        }
    }

    pub fn period(&self) -> usize {
        self.state.period()
    }
}

impl<S: Indicator<Output = Real>> Indicator for Rma<S> {
    type Input = S::Input;
    type Output = Real;

    fn update(&mut self, input: Self::Input) -> Option<Real> {
        self.value = match self.source.update(input) {
            Some(x) => self.state.update(x),
            None => None,
        };
        self.value
    }

    fn value(&self) -> Option<Real> {
        self.value
    }

    fn warm_up_period(&self) -> usize {
        // The mean seed consumes a full period of source outputs. `max(1)`
        // so a `warm_up = 0` source still requires the full period of updates.
        self.source.warm_up_period().max(1) + self.state.period() - 1
    }

    fn unstable_period(&self) -> usize {
        self.source.unstable_period() + self.state.settle_period()
    }

    fn reset(&mut self) {
        self.source.reset();
        self.state.reset();
        self.value = None;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::indicators::Identity;

    #[test]
    fn seeds_with_simple_mean_then_smooths() {
        let mut rma = Rma::new(Identity::new(), 3);
        assert_eq!(rma.update(1.0), None);
        assert_eq!(rma.update(2.0), None);
        assert_eq!(rma.update(3.0), Some(2.0)); // mean of [1,2,3]
        // (2 * 2 + 6) / 3 = 10/3
        assert_eq!(rma.update(6.0), Some(10.0 / 3.0));
    }
}
