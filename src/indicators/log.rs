//! Logarithm of a real-valued source: `log_base(x)`.
//!
//! Stateless unary transform. Warm-up and unstable-period delegate to the
//! source; the output tracks the source one-for-one *except* on samples where
//! the input is non-positive (log domain) — those emit `None`.

use crate::indicator::Indicator;
use crate::types::Real;

/// Logarithm of a real-valued source in a fixed `base`.
///
/// Emits `None` on the same step the source does, and additionally on any step
/// whose input is `<= 0` (log undefined outside the positive reals). The
/// natural log path (`base == e`) uses [`f64::ln`] to avoid the extra division
/// [`f64::log`] does internally.
///
/// # Panics
/// Panics if `base` is not a finite positive number distinct from `1.0`.
///
/// ```
/// use fugazi::prelude::*;
/// use fugazi::indicators::{Identity, Log};
///
/// let mut ln = Log::natural(Identity::new());
/// assert!((ln.update(std::f64::consts::E).unwrap() - 1.0).abs() < 1e-12);
///
/// let mut log10 = Log::new(Identity::new(), 10.0);
/// assert!((log10.update(1000.0).unwrap() - 3.0).abs() < 1e-12);
///
/// // Non-positive inputs emit `None`.
/// assert_eq!(Log::new(Identity::new(), 10.0).update(-1.0), None);
/// assert_eq!(Log::new(Identity::new(), 10.0).update(0.0), None);
/// ```
#[derive(Debug, Clone)]
pub struct Log<S> {
    source: S,
    base: Real,
    /// Latest logarithm; `None` until the source is warmed *and* the input is
    /// strictly positive.
    pub value: Option<Real>,
}

impl<S> Log<S> {
    /// Wrap `source` with a `log_base` transform.
    ///
    /// # Panics
    /// Panics if `base` is not a finite positive number distinct from `1.0`.
    pub fn new(source: S, base: Real) -> Self {
        assert!(
            base.is_finite() && base > 0.0 && base != 1.0,
            "log base must be a finite positive number distinct from 1.0, got {base}",
        );
        Self {
            source,
            base,
            value: None,
        }
    }

    /// Natural log — shorthand for `Log::new(source, std::f64::consts::E)`.
    pub fn natural(source: S) -> Self {
        Self::new(source, std::f64::consts::E)
    }

    /// The logarithm base.
    pub fn base(&self) -> Real {
        self.base
    }
}

impl<S> Indicator for Log<S>
where
    S: Indicator<Output = Real>,
{
    type Input = S::Input;
    type Output = Real;

    fn update(&mut self, input: Self::Input) -> Option<Real> {
        self.value = self.source.update(input).and_then(|x| {
            if x > 0.0 {
                if self.base == std::f64::consts::E {
                    Some(x.ln())
                } else {
                    Some(x.log(self.base))
                }
            } else {
                None
            }
        });
        self.value
    }

    fn value(&self) -> Option<Real> {
        self.value
    }

    fn warm_up_period(&self) -> usize {
        self.source.warm_up_period()
    }

    fn unstable_period(&self) -> usize {
        self.source.unstable_period()
    }

    fn reset(&mut self) {
        self.source.reset();
        self.value = None;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::indicators::{Identity, Sma, Value};

    #[test]
    fn natural_log_matches_ln() {
        let mut ln = Log::natural(Identity::new());
        for x in [1.0, 2.0, std::f64::consts::E, 10.0, 100.0] {
            let got = ln.update(x).unwrap();
            assert!((got - x.ln()).abs() < 1e-12, "ln({x})");
        }
    }

    #[test]
    fn base_10_matches_log10() {
        let mut log10 = Log::new(Identity::new(), 10.0);
        for x in [1.0, 10.0, 100.0, 1e6] {
            let got = log10.update(x).unwrap();
            assert!((got - x.log10()).abs() < 1e-12, "log10({x})");
        }
    }

    #[test]
    fn base_2_matches_log2() {
        let mut log2 = Log::new(Identity::new(), 2.0);
        for x in [1.0, 2.0, 4.0, 1024.0] {
            let got = log2.update(x).unwrap();
            assert!((got - x.log2()).abs() < 1e-12, "log2({x})");
        }
    }

    #[test]
    fn non_positive_inputs_emit_none() {
        let mut ln = Log::natural(Identity::new());
        assert_eq!(ln.update(0.0), None);
        assert_eq!(ln.update(-1.0), None);
        // Recovers on the next positive sample.
        assert!((ln.update(1.0).unwrap()).abs() < 1e-12);
    }

    #[test]
    fn delegates_warm_up_and_unstable_to_source() {
        let inner = Sma::new(Identity::new(), 5);
        let inner_warm = inner.warm_up_period();
        let inner_unstable = inner.unstable_period();
        let log = Log::natural(Sma::new(Identity::new(), 5));
        assert_eq!(log.warm_up_period(), inner_warm);
        assert_eq!(log.unstable_period(), inner_unstable);
    }

    #[test]
    fn none_from_source_propagates() {
        // Sma-3 emits None for the first two samples.
        let mut log = Log::natural(Sma::new(Identity::new(), 3));
        assert_eq!(log.update(1.0), None);
        assert_eq!(log.update(2.0), None);
        assert!(log.update(3.0).is_some());
    }

    #[test]
    fn reset_clears_state() {
        let mut log = Log::natural(Identity::new());
        log.update(2.0);
        log.reset();
        assert!(log.value().is_none());
    }

    #[test]
    #[should_panic(expected = "log base must be")]
    fn zero_base_panics() {
        let _ = Log::new(Value::<Real>::new(1.0), 0.0);
    }

    #[test]
    #[should_panic(expected = "log base must be")]
    fn negative_base_panics() {
        let _ = Log::new(Value::<Real>::new(1.0), -2.0);
    }

    #[test]
    #[should_panic(expected = "log base must be")]
    fn base_one_panics() {
        let _ = Log::new(Value::<Real>::new(1.0), 1.0);
    }
}
