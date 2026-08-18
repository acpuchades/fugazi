//! Exponential of a real-valued source: `base^x`.
//!
//! Stateless unary transform, and the inverse of [`Log`](super::Log). Warm-up
//! and unstable-period delegate to the source; the output tracks the source
//! one-for-one *except* on samples whose result leaves the finite range —
//! those emit `None`.

use fugazi_derive::SaveState;

use crate::indicator::Indicator;
use crate::types::Real;

/// Exponential of a real-valued source in a fixed `base`: `base^x`.
///
/// The counterpart of [`Log`](super::Log), and its inverse over the positive
/// reals: `Exp::natural(Log::natural(s))` reproduces `s`. Emits `None` on the
/// same step the source does, and additionally on any step whose result is not
/// finite — an input large enough to overflow to `inf`, or a `NaN` arriving
/// from the source. That is the mirror of `Log`'s domain guard: `Log` refuses
/// the inputs it has no value for, `Exp` refuses the outputs it cannot
/// represent. Underflow is *not* refused — `exp(-1000.0)` is `0.0`, a
/// representable answer.
///
/// The natural path (`base == e`) uses [`f64::exp`] to avoid the logarithm
/// [`f64::powf`] takes internally.
///
/// # Panics
/// Panics if `base` is not a finite positive number distinct from `1.0` — the
/// same bases [`Log`](super::Log) admits, so the pair stays inverse.
///
/// ```
/// use fugazi::prelude::*;
/// use fugazi::indicators::{Exp, Identity};
///
/// let mut e = Exp::natural(Identity::new());
/// assert!((e.update(1.0).unwrap() - std::f64::consts::E).abs() < 1e-12);
///
/// let mut exp10 = Exp::new(Identity::new(), 10.0);
/// assert!((exp10.update(3.0).unwrap() - 1000.0).abs() < 1e-9);
///
/// // Results too large to represent emit `None`.
/// assert_eq!(Exp::natural(Identity::new()).update(1e6), None);
/// ```
#[derive(Debug, Clone, SaveState)]
pub struct Exp<S> {
    #[state(source)]
    source: S,
    base: Real,
    /// Latest exponential; `None` until the source is warmed, and on any step
    /// whose result overflows the finite range.
    pub value: Option<Real>,
}

impl<S> Exp<S> {
    /// Wrap `source` with a `base^x` transform.
    ///
    /// # Panics
    /// Panics if `base` is not a finite positive number distinct from `1.0`.
    pub fn new(source: S, base: Real) -> Self {
        assert!(
            base.is_finite() && base > 0.0 && base != 1.0,
            "exp base must be a finite positive number distinct from 1.0, got {base}",
        );
        Self {
            source,
            base,
            value: None,
        }
    }

    /// Natural exponential — shorthand for `Exp::new(source, std::f64::consts::E)`.
    pub fn natural(source: S) -> Self {
        Self::new(source, std::f64::consts::E)
    }

    /// The exponential base.
    pub fn base(&self) -> Real {
        self.base
    }
}

impl<S> Indicator for Exp<S>
where
    S: Indicator<Output = Real>,
{
    type Input = S::Input;
    type Output = Real;

    fn update(&mut self, input: Self::Input) -> Option<Real> {
        self.value = self.source.update(input).and_then(|x| {
            let y = if self.base == std::f64::consts::E {
                x.exp()
            } else {
                self.base.powf(x)
            };
            y.is_finite().then_some(y)
        });
        self.value
    }

    fn value(&self) -> Option<Real> {
        self.value
    }

    fn warm_up_bars(&self) -> usize {
        self.source.warm_up_bars()
    }

    fn unstable_bars(&self) -> usize {
        self.source.unstable_bars()
    }

    fn reset(&mut self) {
        self.source.reset();
        self.value = None;
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
    use crate::indicators::{Identity, Log, Sma, Value};

    #[test]
    fn natural_exp_matches_exp() {
        let mut e = Exp::natural(Identity::new());
        for x in [-2.0, 0.0, 1.0, 2.5, 10.0] {
            let got = e.update(x).unwrap();
            assert!((got - x.exp()).abs() <= 1e-12 * x.exp().abs(), "exp({x})");
        }
    }

    #[test]
    fn base_10_matches_powf() {
        let mut exp10 = Exp::new(Identity::new(), 10.0);
        for x in [0.0, 1.0, 3.0, 6.0] {
            let got = exp10.update(x).unwrap();
            assert!(
                (got - 10.0f64.powf(x)).abs() <= 1e-12 * 10.0f64.powf(x),
                "10^{x}"
            );
        }
    }

    #[test]
    fn base_2_matches_exp2() {
        let mut exp2 = Exp::new(Identity::new(), 2.0);
        for x in [0.0, 1.0, 10.0, -3.0] {
            let got = exp2.update(x).unwrap();
            assert!((got - x.exp2()).abs() <= 1e-12 * x.exp2(), "2^{x}");
        }
    }

    #[test]
    fn inverts_log_over_the_positive_reals() {
        let mut round_trip = Exp::natural(Log::natural(Identity::new()));
        for x in [0.5, 1.0, 42.0, 61_237.25] {
            let got = round_trip.update(x).unwrap();
            assert!((got - x).abs() <= 1e-9 * x, "exp(ln({x}))");
        }
    }

    #[test]
    fn overflowing_results_emit_none() {
        let mut e = Exp::natural(Identity::new());
        assert_eq!(e.update(1e6), None);
        // Recovers on the next representable sample.
        assert!((e.update(0.0).unwrap() - 1.0).abs() < 1e-12);
    }

    #[test]
    fn underflow_is_a_value_not_a_gap() {
        // `exp(-1000)` is 0.0 — small, but representable, so it is an answer.
        let mut e = Exp::natural(Identity::new());
        assert_eq!(e.update(-1000.0), Some(0.0));
    }

    #[test]
    fn delegates_warm_up_and_unstable_to_source() {
        let inner = Sma::new(Identity::new(), 5);
        let inner_warm = inner.warm_up_bars();
        let inner_unstable = inner.unstable_bars();
        let exp = Exp::natural(Sma::new(Identity::new(), 5));
        assert_eq!(exp.warm_up_bars(), inner_warm);
        assert_eq!(exp.unstable_bars(), inner_unstable);
    }

    #[test]
    fn none_from_source_propagates() {
        // Sma-3 emits None for the first two samples.
        let mut exp = Exp::natural(Sma::new(Identity::new(), 3));
        assert_eq!(exp.update(1.0), None);
        assert_eq!(exp.update(2.0), None);
        assert!(exp.update(3.0).is_some());
    }

    #[test]
    fn reset_clears_state() {
        let mut exp = Exp::natural(Identity::new());
        exp.update(2.0);
        exp.reset();
        assert!(exp.value().is_none());
    }

    #[test]
    #[should_panic(expected = "exp base must be")]
    fn zero_base_panics() {
        let _ = Exp::new(Value::<Real>::new(1.0), 0.0);
    }

    #[test]
    #[should_panic(expected = "exp base must be")]
    fn negative_base_panics() {
        let _ = Exp::new(Value::<Real>::new(1.0), -2.0);
    }

    #[test]
    #[should_panic(expected = "exp base must be")]
    fn base_one_panics() {
        let _ = Exp::new(Value::<Real>::new(1.0), 1.0);
    }
}
