use crate::indicator::Indicator;
use crate::indicators::smoothing::EmaState;
use crate::types::Real;

/// Exponential Moving Average of a source.
///
/// Owns its input source, so composition is construction:
/// `Ema::new(Current::close(), 20)` is the EMA-20 of the close, and
/// `Ema::new(Sma::new(src, 10), 20)` is the EMA of an SMA. Seeds on the source's
/// first output, so it is ready as soon as the source is.
#[derive(Debug, Clone)]
pub struct Ema<S> {
    source: S,
    state: EmaState,
    /// Latest output value; `None` until the source produces its first value.
    pub value: Option<Real>,
}

impl<S> Ema<S> {
    /// Smoothing factor `2 / (period + 1)`.
    ///
    /// # Panics
    /// Panics if `period` is zero.
    pub fn new(source: S, period: usize) -> Self {
        Self {
            source,
            state: EmaState::new(period),
            value: None,
        }
    }

    /// Construct directly from a smoothing factor in `(0, 1]`.
    ///
    /// # Panics
    /// Panics if `alpha` is outside `(0, 1]`.
    pub fn with_alpha(source: S, alpha: Real) -> Self {
        Self {
            source,
            state: EmaState::with_alpha(alpha),
            value: None,
        }
    }
}

impl<S: Indicator<Output = Real>> Indicator for Ema<S> {
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
    fn seeds_with_first_sample() {
        let mut ema = Ema::new(Identity::new(), 3); // alpha = 0.5
        assert_eq!(ema.update(10.0), Some(10.0));
        // 0.5 * 20 + 0.5 * 10 = 15
        assert_eq!(ema.update(20.0), Some(15.0));
    }
}
