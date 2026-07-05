use crate::indicator::Indicator;
use crate::indicators::Rsi;
use crate::indicators::ops::{MaxOp, MinOp};
use crate::indicators::stats::WindowExtreme;
use crate::types::Real;

/// Stochastic oscillator of a source: where the current value sits within its
/// recent range, `(x - min) / (max - min)`, in `[0, 1]`.
///
/// Owns its input source and reuses the rolling-extremum core for both the
/// window min and max. When the window is flat (`max == min`) it yields `0.0`.
/// Applied to an [`Rsi`] source this is StochRSI — see the [`StochRsi`] alias
/// and `IndicatorExt::stoch_rsi`.
#[derive(Debug, Clone)]
pub struct Stochastic<S> {
    source: S,
    min: WindowExtreme<MinOp>,
    max: WindowExtreme<MaxOp>,
    /// Latest oscillator value in `[0, 1]`; `None` until the window is full.
    pub value: Option<Real>,
}

impl<S> Stochastic<S> {
    /// # Panics
    /// Panics if `period` is zero.
    pub fn new(source: S, period: usize) -> Self {
        Self {
            source,
            min: WindowExtreme::new(period),
            max: WindowExtreme::new(period),
            value: None,
        }
    }
}

impl<S: Indicator<Output = Real>> Indicator for Stochastic<S> {
    type Input = S::Input;
    type Output = Real;

    fn update(&mut self, input: Self::Input) -> Option<Real> {
        self.value = match self.source.update(input) {
            Some(x) => match (self.min.update(x), self.max.update(x)) {
                (Some(min), Some(max)) => {
                    let range = max - min;
                    Some(if range == 0.0 { 0.0 } else { (x - min) / range })
                }
                _ => None,
            },
            None => None,
        };
        self.value
    }

    fn value(&self) -> Option<Real> {
        self.value
    }

    fn warm_up_period(&self) -> usize {
        self.source.warm_up_period().max(1) + self.min.period() - 1
    }

    fn unstable_period(&self) -> usize {
        self.source.unstable_period()
    }

    fn reset(&mut self) {
        self.source.reset();
        self.min.reset();
        self.max.reset();
        self.value = None;
    }
}

/// StochRSI: the [`Stochastic`] oscillator applied to an [`Rsi`] source.
///
/// Build with `IndicatorExt::stoch_rsi`, or directly:
/// `Stochastic::new(Rsi::new(src, 14), 14)`.
pub type StochRsi<S> = Stochastic<Rsi<S>>;

#[cfg(test)]
mod tests {
    use super::*;
    use crate::indicators::Identity;

    #[test]
    fn positions_value_within_range() {
        let mut st = Stochastic::new(Identity::new(), 3);
        assert_eq!(st.update(1.0), None);
        assert_eq!(st.update(2.0), None);
        // window [1,2,3]: (3-1)/(3-1) = 1.0
        assert_eq!(st.update(3.0), Some(1.0));
        // window [2,3,2]: (2-2)/(3-2) = 0.0
        assert_eq!(st.update(2.0), Some(0.0));
        // window [3,2,2.5]: (2.5-2)/(3-2) = 0.5
        assert_eq!(st.update(2.5), Some(0.5));
    }

    #[test]
    fn flat_window_is_zero() {
        let mut st = Stochastic::new(Identity::new(), 2);
        st.update(5.0);
        assert_eq!(st.update(5.0), Some(0.0));
    }
}
