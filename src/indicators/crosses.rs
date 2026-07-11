//! Native crossover primitives — `CrossesAbove<L, R>` / `CrossesBelow<L, R>`.
//!
//! The classical composed form was
//! `lhs.gt(rhs).and(lhs.gt(rhs).changed())` — i.e. `And<Gt<L, R>, Change<Gt<L,
//! R>>>` — which clones both operands so each side is advanced twice per bar,
//! doubling the work of the underlying source chain. These primitives hold a
//! single comparison state plus one previous-value slot, so an update
//! advances each operand only once and the edge check is a pair of bool
//! comparisons.
//!
//! Behaviour is byte-identical to the composed form (see the equivalence
//! tests below): `Some(true)` on the step where `lhs > rhs` (or `lhs < rhs`
//! for `CrossesBelow`) first *becomes* true after having been false the
//! previous step; `Some(false)` on every other post-warm-up step;
//! `None` until both operands have been warmed *and* one more update has
//! occurred (needed to establish the previous state).
//!
//! `IndicatorExt::crosses_above` / `crosses_below` now return these
//! primitives directly — see [`IndicatorExt`](crate::indicators::IndicatorExt).

use crate::indicator::Indicator;
use crate::indicators::compare::DEFAULT_EPSILON;
use crate::types::Real;

/// `lhs > rhs` on this step and the strict comparison just flipped upward.
///
/// See the [module doc](self) for the equivalence with the historical
/// `lhs.gt(rhs).and(lhs.gt(rhs).changed())` composition. The comparison is
/// tolerance-aware: `lhs > rhs` reads as true only when `lhs - rhs > epsilon`
/// (default [`DEFAULT_EPSILON`]).
#[derive(Debug, Clone)]
pub struct CrossesAbove<L, R> {
    lhs: L,
    rhs: R,
    epsilon: Real,
    prev: Option<bool>,
    value: Option<bool>,
}

impl<L, R> CrossesAbove<L, R> {
    /// Build with the default absolute tolerance [`DEFAULT_EPSILON`].
    pub fn new(lhs: L, rhs: R) -> Self {
        Self::with_epsilon(lhs, rhs, DEFAULT_EPSILON)
    }

    /// Build with an explicit absolute tolerance — the same knob
    /// [`Gt::with_epsilon`](crate::indicators::Gt) exposes on the underlying
    /// comparison. Use at very large or very small numeric scales where
    /// [`DEFAULT_EPSILON`] doesn't fit.
    pub fn with_epsilon(lhs: L, rhs: R, epsilon: Real) -> Self {
        Self {
            lhs,
            rhs,
            epsilon,
            prev: None,
            value: None,
        }
    }
}

impl<L, R> Indicator for CrossesAbove<L, R>
where
    L: Indicator<Output = Real>,
    R: Indicator<Input = L::Input, Output = Real>,
    L::Input: Clone,
{
    type Input = L::Input;
    type Output = bool;

    fn update(&mut self, input: Self::Input) -> Option<bool> {
        let l = self.lhs.update(input.clone());
        let r = self.rhs.update(input);
        // The comparison is Some once both operands have warmed up; matches
        // the tolerance-aware `Gt::apply` (l - r > epsilon).
        let now = match (l, r) {
            (Some(lv), Some(rv)) => Some(lv - rv > self.epsilon),
            _ => None,
        };
        // A cross fires on the transition false → true. Every other combined
        // state after both are warmed is `Some(false)`; unwarmed steps are
        // `None`.
        self.value = match (self.prev, now) {
            (Some(false), Some(true)) => Some(true),
            (Some(_), Some(_)) => Some(false),
            _ => None,
        };
        self.prev = now;
        self.value
    }

    fn value(&self) -> Option<bool> {
        self.value
    }

    fn warm_up_period(&self) -> usize {
        // Same as the composed `And<Gt, Change<Gt>>`: max of the operands,
        // clamped to 1 so a `warm_up = 0` operand (e.g. a `Const`) still
        // needs one update before the prev slot can be compared against,
        // plus 1 for the edge detection.
        self.lhs
            .warm_up_period()
            .max(self.rhs.warm_up_period())
            .max(1)
            + 1
    }

    fn unstable_period(&self) -> usize {
        self.lhs.unstable_period().max(self.rhs.unstable_period())
    }

    fn reset(&mut self) {
        self.lhs.reset();
        self.rhs.reset();
        self.prev = None;
        self.value = None;
    }
}

/// `lhs < rhs` on this step and the strict comparison just flipped upward.
///
/// The downward twin of [`CrossesAbove`], expressed as a newtype around
/// `CrossesAbove<R, L>` with the operands swapped: `lhs < rhs` is exactly
/// the same test as `rhs > lhs` under the same tolerance, so one impl of
/// the state machine serves both directions. The public `new` /
/// `with_epsilon` constructors take the operands in the natural order and
/// swap them internally.
#[derive(Debug, Clone)]
pub struct CrossesBelow<L, R>(CrossesAbove<R, L>);

impl<L, R> CrossesBelow<L, R> {
    /// Build with the default absolute tolerance [`DEFAULT_EPSILON`].
    pub fn new(lhs: L, rhs: R) -> Self {
        Self(CrossesAbove::new(rhs, lhs))
    }

    /// Build with an explicit absolute tolerance. See
    /// [`CrossesAbove::with_epsilon`].
    pub fn with_epsilon(lhs: L, rhs: R, epsilon: Real) -> Self {
        Self(CrossesAbove::with_epsilon(rhs, lhs, epsilon))
    }
}

impl<L, R> Indicator for CrossesBelow<L, R>
where
    L: Indicator<Output = Real>,
    R: Indicator<Input = L::Input, Output = Real>,
    L::Input: Clone,
{
    type Input = L::Input;
    type Output = bool;

    fn update(&mut self, input: Self::Input) -> Option<bool> {
        self.0.update(input)
    }

    fn value(&self) -> Option<bool> {
        self.0.value()
    }

    fn warm_up_period(&self) -> usize {
        self.0.warm_up_period()
    }

    fn unstable_period(&self) -> usize {
        self.0.unstable_period()
    }

    fn reset(&mut self) {
        self.0.reset();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::indicators::{BoolIndicatorExt, Identity, IndicatorExt, Sma, Value};

    /// Walk a sequence of Reals through both `native` and `composed` and
    /// assert their per-step values match. Verifies byte-for-byte
    /// equivalence with the historical composed form.
    fn assert_equivalent(
        mut native: impl Indicator<Input = Real, Output = bool>,
        mut composed: impl Indicator<Input = Real, Output = bool>,
        samples: &[Real],
    ) {
        assert_eq!(
            native.warm_up_period(),
            composed.warm_up_period(),
            "warm_up_period mismatch",
        );
        for &x in samples {
            let n = native.update(x);
            let c = composed.update(x);
            assert_eq!(n, c, "value mismatch on sample {x}");
        }
    }

    #[test]
    fn native_crosses_above_matches_composed_form() {
        // Rising line crossing above a Value(3.0).
        // Composed: Value(3.0).gt(Identity).and(Value(3.0).gt(Identity).changed())
        // — hmm, `crosses_above` is on self so let me use the composed
        // form explicitly. Use `sma_of_identity.crosses_above(const)`
        // vs the same rebuilt by hand as `And<Gt, Change<Gt>>`.
        let composed_form = |sma_period: usize, level: Real| {
            let lhs = Sma::new(Identity::<Real>::new(), sma_period);
            let rhs = Value::<Real>::new(level);
            lhs.clone().gt(rhs).and(lhs.gt(rhs).changed())
        };
        let native_form = |sma_period: usize, level: Real| {
            CrossesAbove::new(
                Sma::new(Identity::<Real>::new(), sma_period),
                Value::<Real>::new(level),
            )
        };

        let samples = [
            1.0, 2.0, 3.0, 4.0, 5.0, 4.0, 3.0, 2.0, 3.0, 4.0, 5.0,
        ];
        assert_equivalent(native_form(2, 3.0), composed_form(2, 3.0), &samples);
    }

    #[test]
    fn native_crosses_below_matches_composed_form() {
        let composed_form = |sma_period: usize, level: Real| {
            let lhs = Sma::new(Identity::<Real>::new(), sma_period);
            let rhs = Value::<Real>::new(level);
            lhs.clone().lt(rhs).and(lhs.lt(rhs).changed())
        };
        let native_form = |sma_period: usize, level: Real| {
            CrossesBelow::new(
                Sma::new(Identity::<Real>::new(), sma_period),
                Value::<Real>::new(level),
            )
        };

        let samples = [
            5.0, 4.0, 3.0, 2.0, 1.0, 2.0, 3.0, 4.0, 3.0, 2.0, 1.0,
        ];
        assert_equivalent(native_form(2, 3.0), composed_form(2, 3.0), &samples);
    }

    #[test]
    fn epsilon_override_prevents_spurious_flip() {
        // With a huge epsilon, `2.0 > 1.5` reads as false, so no cross fires.
        let mut xa = CrossesAbove::with_epsilon(
            Identity::<Real>::new(),
            Value::<Real>::new(1.5),
            10.0,
        );
        for &x in &[1.0, 2.0, 3.0, 4.0] {
            let out = xa.update(x);
            assert_ne!(out, Some(true), "no cross expected at {x} with huge eps");
        }
    }

    #[test]
    fn reset_returns_to_unwarmed_state() {
        let mut xa = CrossesAbove::new(Identity::<Real>::new(), Value::<Real>::new(3.0));
        for x in [1.0, 5.0, 5.0] {
            xa.update(x);
        }
        assert!(xa.is_true() || !xa.is_true()); // just to touch value()
        xa.reset();
        assert_eq!(xa.value(), None);
        // First bar after reset: unwarmed.
        assert_eq!(xa.update(5.0), None);
    }
}
