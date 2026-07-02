use crate::indicator::Indicator;
use crate::indicators::stats::WindowStats;
use crate::types::Real;

/// Simple Moving Average of a source over a fixed window.
///
/// Owns its input source: `Sma::new(Current::close(), 20)`. Backed by the shared
/// [`WindowStats`] core (running sum over a ring buffer), so each update is O(1).
/// Produces `None` until the window is full.
#[derive(Debug, Clone)]
pub struct Sma<S> {
    source: S,
    stats: WindowStats,
    /// Latest output value; `None` until `period` source values have been seen.
    pub value: Option<Real>,
}

impl<S> Sma<S> {
    /// # Panics
    /// Panics if `period` is zero.
    pub fn new(source: S, period: usize) -> Self {
        Self {
            source,
            stats: WindowStats::new(period),
            value: None,
        }
    }

    pub fn period(&self) -> usize {
        self.stats.period()
    }
}

impl<S: Indicator<Output = Real>> Indicator for Sma<S> {
    type Input = S::Input;
    type Output = Real;

    fn update(&mut self, input: Self::Input) -> Option<Real> {
        self.value = match self.source.update(input) {
            Some(x) if self.stats.update(x) => Some(self.stats.mean()),
            _ => None,
        };
        self.value
    }

    fn value(&self) -> Option<Real> {
        self.value
    }

    fn warm_up_period(&self) -> usize {
        self.source.warm_up_period() + self.stats.period() - 1
    }

    fn unstable_period(&self) -> usize {
        self.source.unstable_period()
    }

    fn reset(&mut self) {
        self.source.reset();
        self.stats.reset();
        self.value = None;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::indicators::Identity;

    #[test]
    fn warms_up_then_averages() {
        let mut sma = Sma::new(Identity::new(), 3);
        assert_eq!(sma.update(1.0), None);
        assert_eq!(sma.update(2.0), None);
        assert_eq!(sma.update(3.0), Some(2.0));
        assert_eq!(sma.update(4.0), Some(3.0));
        assert_eq!(sma.value, Some(3.0));
    }
}
