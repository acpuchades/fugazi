//! Comparison signals over two indicator sources, plus the [`IndicatorExt`]
//! fluent builder API.
//!
//! All operators share one [`Compare`] carrier specialised by a zero-sized
//! [`CompareOp`] marker, so the six operators are type aliases and a custom
//! comparison is just a new `CompareOp` impl. Every comparison carries an
//! absolute tolerance `epsilon` (default [`DEFAULT_EPSILON`]) so floating-point
//! noise does not cause spurious flips; values within `epsilon` are treated as
//! equal. Comparisons feed the same input to both sources (hence `Input: Clone`)
//! and are `false` until both are warmed up.

use std::marker::PhantomData;

use crate::indicator::Indicator;
use crate::indicators::{Add, Diff, Div, Lag, Mul, Ratio, Roc, RollingMax, RollingMin, Sub, Value};
use crate::signal::{And, Change, Signal, SignalExt};
use crate::types::Real;

/// Default absolute tolerance for comparison signals.
///
/// An absolute (not relative) epsilon; override per-comparison via
/// [`Compare::with_epsilon`] when working at very large or very small scales.
pub const DEFAULT_EPSILON: Real = 1e-8;

/// A tolerance-aware comparison between two warmed-up source outputs.
///
/// Implement for a zero-sized marker to define a new operator usable with
/// [`Compare`].
pub trait CompareOp {
    /// Whether `lhs` relates to `rhs` under this operator, given tolerance
    /// `epsilon`.
    fn test(lhs: Real, rhs: Real, epsilon: Real) -> bool;
}

/// A comparison signal over two indicator sources, parameterised by operator.
///
/// You normally use the aliases ([`Gt`], [`Lt`], [`Ge`], [`Le`], [`Eq`],
/// [`Ne`]) or the [`IndicatorExt`] builders rather than naming `Compare`
/// directly.
#[derive(Debug, Clone)]
pub struct Compare<L, R, Op> {
    lhs: L,
    rhs: R,
    epsilon: Real,
    value: bool,
    _op: PhantomData<fn() -> Op>,
}

impl<L, R, Op> Compare<L, R, Op> {
    /// Compare `lhs` against `rhs` using [`DEFAULT_EPSILON`].
    pub fn new(lhs: L, rhs: R) -> Self {
        Self::with_epsilon(lhs, rhs, DEFAULT_EPSILON)
    }

    /// Compare `lhs` against `rhs` with an explicit absolute tolerance, instead
    /// of the [`DEFAULT_EPSILON`] used by [`new`](Self::new) and the
    /// [`IndicatorExt`] builders.
    pub fn with_epsilon(lhs: L, rhs: R, epsilon: Real) -> Self {
        Self {
            lhs,
            rhs,
            epsilon,
            value: false,
            _op: PhantomData,
        }
    }
}

impl<L, R, Op> Signal for Compare<L, R, Op>
where
    L: Indicator<Output = Real>,
    R: Indicator<Input = L::Input, Output = Real>,
    L::Input: Clone,
    Op: CompareOp,
{
    type Input = L::Input;

    fn update(&mut self, input: Self::Input) -> bool {
        let lhs = self.lhs.update(input.clone());
        let rhs = self.rhs.update(input);
        self.value = matches!((lhs, rhs), (Some(l), Some(r)) if Op::test(l, r, self.epsilon));
        self.value
    }

    fn value(&self) -> bool {
        self.value
    }

    fn reset(&mut self) {
        self.lhs.reset();
        self.rhs.reset();
        self.value = false;
    }
}

/// `lhs > rhs` (beyond `epsilon`).
#[derive(Debug, Clone, Copy)]
pub struct GtOp;
impl CompareOp for GtOp {
    fn test(lhs: Real, rhs: Real, epsilon: Real) -> bool {
        lhs - rhs > epsilon
    }
}

/// `lhs < rhs` (beyond `epsilon`).
#[derive(Debug, Clone, Copy)]
pub struct LtOp;
impl CompareOp for LtOp {
    fn test(lhs: Real, rhs: Real, epsilon: Real) -> bool {
        rhs - lhs > epsilon
    }
}

/// `lhs >= rhs` (within `epsilon`).
#[derive(Debug, Clone, Copy)]
pub struct GeOp;
impl CompareOp for GeOp {
    fn test(lhs: Real, rhs: Real, epsilon: Real) -> bool {
        lhs - rhs >= -epsilon
    }
}

/// `lhs <= rhs` (within `epsilon`).
#[derive(Debug, Clone, Copy)]
pub struct LeOp;
impl CompareOp for LeOp {
    fn test(lhs: Real, rhs: Real, epsilon: Real) -> bool {
        lhs - rhs <= epsilon
    }
}

/// `lhs ≈ rhs` (within `epsilon`).
#[derive(Debug, Clone, Copy)]
pub struct EqOp;
impl CompareOp for EqOp {
    fn test(lhs: Real, rhs: Real, epsilon: Real) -> bool {
        (lhs - rhs).abs() <= epsilon
    }
}

/// `lhs != rhs` (beyond `epsilon`).
#[derive(Debug, Clone, Copy)]
pub struct NeOp;
impl CompareOp for NeOp {
    fn test(lhs: Real, rhs: Real, epsilon: Real) -> bool {
        (lhs - rhs).abs() > epsilon
    }
}

/// Fires while `lhs` exceeds `rhs` by more than `epsilon`.
pub type Gt<L, R> = Compare<L, R, GtOp>;
/// Fires while `lhs` is below `rhs` by more than `epsilon`.
pub type Lt<L, R> = Compare<L, R, LtOp>;
/// Fires while `lhs` is greater than or within `epsilon` of `rhs`.
pub type Ge<L, R> = Compare<L, R, GeOp>;
/// Fires while `lhs` is less than or within `epsilon` of `rhs`.
pub type Le<L, R> = Compare<L, R, LeOp>;
/// Fires while `lhs` and `rhs` are within `epsilon` of each other.
pub type Eq<L, R> = Compare<L, R, EqOp>;
/// Fires while `lhs` and `rhs` differ by more than `epsilon`.
pub type Ne<L, R> = Compare<L, R, NeOp>;

/// Fluent builders for composing indicator sources into other indicators and
/// signals.
///
/// Implemented for every [`Real`]-valued [`Indicator`], so composition reads
/// naturally:
///
/// ```
/// use arcana::prelude::*;
/// use arcana::indicators::{Current, Ema};
///
/// // "current close crosses above EMA20", consuming a Candle per bar:
/// let _sig = Current::close().crosses_above(Ema::new(Current::close(), 20));
/// // EMA20-of-close higher than the prior bar:
/// let _rising = Ema::new(Current::close(), 20).ratio(1).above(1.0);
/// ```
///
/// Comparison builders use [`DEFAULT_EPSILON`]; for a custom tolerance build the
/// comparison explicitly, e.g. `Gt::with_epsilon(a, b, 1e-4)`.
pub trait IndicatorExt: Indicator<Output = Real> + Sized {
    /// `self > rhs`.
    fn gt<R>(self, rhs: R) -> Gt<Self, R>
    where
        R: Indicator<Input = Self::Input, Output = Real>,
    {
        Gt::new(self, rhs)
    }

    /// `self < rhs`.
    fn lt<R>(self, rhs: R) -> Lt<Self, R>
    where
        R: Indicator<Input = Self::Input, Output = Real>,
    {
        Lt::new(self, rhs)
    }

    /// `self >= rhs`.
    fn ge<R>(self, rhs: R) -> Ge<Self, R>
    where
        R: Indicator<Input = Self::Input, Output = Real>,
    {
        Ge::new(self, rhs)
    }

    /// `self <= rhs`.
    fn le<R>(self, rhs: R) -> Le<Self, R>
    where
        R: Indicator<Input = Self::Input, Output = Real>,
    {
        Le::new(self, rhs)
    }

    /// `self ≈ rhs` (within tolerance).
    fn eq<R>(self, rhs: R) -> Eq<Self, R>
    where
        R: Indicator<Input = Self::Input, Output = Real>,
    {
        Eq::new(self, rhs)
    }

    /// `self != rhs` (beyond tolerance).
    fn ne<R>(self, rhs: R) -> Ne<Self, R>
    where
        R: Indicator<Input = Self::Input, Output = Real>,
    {
        Ne::new(self, rhs)
    }

    /// `self` is above a constant `level` — sugar for `self.gt(Value::new(level))`.
    fn above(self, level: Real) -> Gt<Self, Value<Self::Input>> {
        Gt::new(self, Value::new(level))
    }

    /// `self` is below a constant `level` — sugar for `self.lt(Value::new(level))`.
    fn below(self, level: Real) -> Lt<Self, Value<Self::Input>> {
        Lt::new(self, Value::new(level))
    }

    /// `self + rhs`, pointwise.
    fn add<R>(self, rhs: R) -> Add<Self, R>
    where
        R: Indicator<Input = Self::Input, Output = Real>,
    {
        Add::new(self, rhs)
    }

    /// `self - rhs`, pointwise.
    fn sub<R>(self, rhs: R) -> Sub<Self, R>
    where
        R: Indicator<Input = Self::Input, Output = Real>,
    {
        Sub::new(self, rhs)
    }

    /// `self * rhs`, pointwise.
    fn mul<R>(self, rhs: R) -> Mul<Self, R>
    where
        R: Indicator<Input = Self::Input, Output = Real>,
    {
        Mul::new(self, rhs)
    }

    /// `self / rhs`, pointwise (`None` on divide-by-zero).
    fn div<R>(self, rhs: R) -> Div<Self, R>
    where
        R: Indicator<Input = Self::Input, Output = Real>,
    {
        Div::new(self, rhs)
    }

    /// `self` delayed by `periods` steps.
    fn lag(self, periods: usize) -> Lag<Self> {
        Lag::new(self, periods)
    }

    /// Discrete difference of `self` over `periods` steps (`x[t] - x[t-n]`).
    fn diff(self, periods: usize) -> Diff<Self> {
        Diff::new(self, periods)
    }

    /// Ratio of `self` to its value `periods` steps ago (`x[t] / x[t-n]`).
    fn ratio(self, periods: usize) -> Ratio<Self> {
        Ratio::new(self, periods)
    }

    /// Percentage rate of change of `self` over `periods` steps
    /// (`100·(x[t] − x[t-n]) / x[t-n]`).
    fn roc(self, periods: usize) -> Roc<Self> {
        Roc::new(self, periods)
    }

    /// Rolling maximum of `self` over `period` steps.
    fn rolling_max(self, period: usize) -> RollingMax<Self> {
        RollingMax::new(self, period)
    }

    /// Rolling minimum of `self` over `period` steps.
    fn rolling_min(self, period: usize) -> RollingMin<Self> {
        RollingMin::new(self, period)
    }

    /// `self` rises above `rhs` on this step.
    ///
    /// Composes from primitives: the comparison is true *and* it just changed —
    /// `self.gt(rhs).and(self.gt(rhs).changed())`.
    fn crosses_above<R>(self, rhs: R) -> And<Gt<Self, R>, Change<Gt<Self, R>>>
    where
        Self: Clone,
        R: Indicator<Input = Self::Input, Output = Real> + Clone,
        Self::Input: Clone,
    {
        self.clone().gt(rhs.clone()).and(self.gt(rhs).changed())
    }

    /// `self` falls below `rhs` on this step.
    fn crosses_below<R>(self, rhs: R) -> And<Lt<Self, R>, Change<Lt<Self, R>>>
    where
        Self: Clone,
        R: Indicator<Input = Self::Input, Output = Real> + Clone,
        Self::Input: Clone,
    {
        self.clone().lt(rhs.clone()).and(self.lt(rhs).changed())
    }
}

impl<I: Indicator<Output = Real>> IndicatorExt for I {}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::indicators::{Ema, Identity, Rsi, Value};

    #[test]
    fn rsi_over_70_is_one_object() {
        let mut overbought = Rsi::new(Identity::new(), 2).above(70.0);
        let mut fired = false;
        for p in [10.0, 11.0, 12.0, 13.0, 14.0] {
            fired |= overbought.update(p);
        }
        assert!(fired);
    }

    #[test]
    fn fluent_lt_between_emas() {
        let mut sig = Ema::new(Identity::new(), 2).lt(Ema::new(Identity::new(), 5));
        let mut fired = false;
        for p in [100.0, 90.0, 80.0, 70.0, 60.0] {
            fired |= sig.update(p);
        }
        assert!(fired);
    }

    #[test]
    fn crosses_above_fires_once() {
        let mut x = Identity::new().crosses_above(Value::new(2.0));
        assert!(!x.update(1.0));
        assert!(!x.update(1.5));
        assert!(x.update(2.5)); // crossing step
        assert!(!x.update(3.0)); // already above
    }

    #[test]
    fn ratio_spike() {
        // A source more than doubling versus the prior bar.
        let mut spike = Identity::new().ratio(1).above(1.9);
        assert!(!spike.update(100.0)); // warming up
        assert!(spike.update(250.0)); // 250/100 = 2.5 > 1.9
        assert!(!spike.update(255.0)); // 255/250 ≈ 1.02
    }

    #[test]
    fn custom_epsilon_overrides_default() {
        let mut gt = Gt::with_epsilon(Identity::new(), Value::new(100.0), 1.0);
        assert!(!gt.update(100.5)); // within tolerance -> not greater
        assert!(gt.update(101.5)); // beyond tolerance -> greater
    }
}
