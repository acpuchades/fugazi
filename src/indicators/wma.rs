use crate::indicator::Indicator;
use crate::indicators::stats::WmaState;
use crate::types::Real;

/// Weighted Moving Average of a source over a fixed window.
///
/// The newest sample carries weight `period`, the next `period - 1`, …, down to
/// `1` for the oldest, normalised by `period·(period + 1)/2`. Owns its input
/// source, so composition is construction: `Wma::new(Current::close(), 20)`.
/// Backed by the shared [`WmaState`] core (running simple and weighted sums), so
/// each update is O(1). Produces `None` until the window is full.
#[derive(Debug, Clone)]
pub struct Wma<S> {
    source: S,
    state: WmaState,
    /// Latest output value; `None` until `period` source values have been seen.
    pub value: Option<Real>,
}

impl<S> Wma<S> {
    /// # Panics
    /// Panics if `period` is zero.
    pub fn new(source: S, period: usize) -> Self {
        Self {
            source,
            state: WmaState::new(period),
            value: None,
        }
    }

    pub fn period(&self) -> usize {
        self.state.period()
    }
}

impl<S: Indicator<Output = Real>> Indicator for Wma<S> {
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
        self.source.warm_up_period() + self.state.period() - 1
    }

    fn unstable_period(&self) -> usize {
        self.source.unstable_period()
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
    fn warms_up_then_weights_recent_higher() {
        let mut wma = Wma::new(Identity::new(), 3);
        assert_eq!(wma.update(1.0), None);
        assert_eq!(wma.update(2.0), None);
        // (1*1 + 2*2 + 3*3) / (1+2+3) = 14 / 6
        assert_eq!(wma.update(3.0), Some(14.0 / 6.0));
        // window [2,3,4]: (1*2 + 2*3 + 3*4) / 6 = 20 / 6
        assert_eq!(wma.update(4.0), Some(20.0 / 6.0));
    }
}
