//! The fluent builder extension traits: [`IndicatorExt`] over real-valued sources
//! and [`BoolIndicatorExt`] over boolean ones.

use crate::indicator::Indicator;
use crate::indicators::compare::{Eq, Ge, Gt, Le, Lt, Ne};
use crate::indicators::if_else::IfElse;
use crate::indicators::log::Log;
use crate::indicators::logic::{And, Change, Not, Or, Xor};
use crate::indicators::ops::{Add, Diff, Div, Lag, Mul, Ratio, Roc, RollingMax, RollingMin, Sub};
use crate::indicators::unstable::Unstable;
use crate::indicators::value::Value;
use crate::types::Real;

/// Fluent builders for composing real-valued indicator sources into other
/// indicators and into boolean comparisons.
///
/// Implemented for every [`Real`]-valued [`Indicator`], so composition reads
/// naturally:
///
/// ```
/// use fugazi::prelude::*;
/// use fugazi::indicators::{Current, Ema};
///
/// // "current close crosses above EMA20", consuming a Candle per bar:
/// let _sig = Current::close().crosses_above(Ema::new(Current::close(), 20));
/// // EMA20-of-close higher than the prior bar:
/// let _rising = Ema::new(Current::close(), 20).ratio(1).above(1.0);
/// ```
///
/// Comparison builders use [`DEFAULT_EPSILON`](super::DEFAULT_EPSILON); for a
/// custom tolerance build the comparison explicitly, e.g.
/// `Gt::with_epsilon(a, b, 1e-4)`.
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

    /// `self ≈ rhs` (within tolerance). Named `eq_to` (not `eq`) to avoid
    /// colliding with [`PartialEq::eq`] when a source type happens to
    /// implement it.
    fn eq_to<R>(self, rhs: R) -> Eq<Self, R>
    where
        R: Indicator<Input = Self::Input, Output = Real>,
    {
        Eq::new(self, rhs)
    }

    /// `self != rhs` (beyond tolerance). Named `ne_to` (not `ne`) to avoid
    /// colliding with [`PartialEq::ne`] when a source type happens to
    /// implement it.
    fn ne_to<R>(self, rhs: R) -> Ne<Self, R>
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

    /// Logarithm of `self` in `base` (emits `None` on samples where the input
    /// is non-positive). Panics if `base` is not a finite positive number
    /// distinct from `1.0`.
    fn log(self, base: Real) -> Log<Self> {
        Log::new(self, base)
    }

    /// Rolling maximum of `self` over `period` steps.
    fn rolling_max(self, period: usize) -> RollingMax<Self> {
        RollingMax::new(self, period)
    }

    /// Rolling minimum of `self` over `period` steps.
    fn rolling_min(self, period: usize) -> RollingMin<Self> {
        RollingMin::new(self, period)
    }

    /// Wrap `self` so its [`unstable_period`](Indicator::unstable_period)
    /// reports `0` — see [`Unstable`]. The output and warm-up are unchanged; a
    /// caller counting off `stable_period()` samples (e.g. a strategy's
    /// [`is_ready`](crate::Strategy::is_ready)) treats the IIR settling tail as
    /// already converged. Use when you're comfortable trading through this
    /// subtree's unstable tail (the safe default is to wait for it).
    fn unstable(self) -> Unstable<Self> {
        Unstable::new(self)
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

/// Combinators and the boolean view for `bool`-valued indicators — the boolean
/// twin of [`IndicatorExt`] (which extends `Real`-valued sources).
///
/// Blanket-implemented for every `Indicator<Output = bool>` (a *signal*, in the
/// candle case — see [`Signal`](crate::Signal)), and `?Sized` so the methods are
/// callable on a `Box<dyn Signal>` too. The binary combinators feed the *same*
/// input to both sides, which is why they require `Self::Input: Clone`.
pub trait BoolIndicatorExt: Indicator<Output = bool> {
    /// The latest state as a plain `bool`: the current value, or `false` while the
    /// indicator is still warming up (`None`).
    fn is_true(&self) -> bool {
        self.value().unwrap_or(false)
    }

    /// Logical AND of `self` and `rhs`.
    fn and<R>(self, rhs: R) -> And<Self, R>
    where
        Self: Sized,
        R: Indicator<Input = Self::Input, Output = bool>,
        Self::Input: Clone,
    {
        And::new(self, rhs)
    }

    /// Logical OR of `self` and `rhs`.
    fn or<R>(self, rhs: R) -> Or<Self, R>
    where
        Self: Sized,
        R: Indicator<Input = Self::Input, Output = bool>,
        Self::Input: Clone,
    {
        Or::new(self, rhs)
    }

    /// Logical XOR of `self` and `rhs`.
    fn xor<R>(self, rhs: R) -> Xor<Self, R>
    where
        Self: Sized,
        R: Indicator<Input = Self::Input, Output = bool>,
        Self::Input: Clone,
    {
        Xor::new(self, rhs)
    }

    /// Logical negation of `self`.
    fn not(self) -> Not<Self>
    where
        Self: Sized,
    {
        Not::new(self)
    }

    /// Fires on the single step where `self`'s value toggles (in either
    /// direction).
    ///
    /// This is the one edge primitive. Directional events compose from it:
    /// "became true" is `s.changed().and(s)` and a crossover is
    /// `a.gt(b).and(a.gt(b).changed())` — see [`IndicatorExt::crosses_above`].
    fn changed(self) -> Change<Self>
    where
        Self: Sized,
    {
        Change::new(self)
    }

    /// Wrap `self` so its [`unstable_period`](Indicator::unstable_period)
    /// reports `0` — see [`Unstable`]. The output and warm-up are unchanged; a
    /// caller counting off `stable_period()` samples (e.g. a strategy's
    /// [`is_ready`](crate::Strategy::is_ready)) treats the IIR settling tail as
    /// already converged. The safe default is to wait for it — this is the
    /// explicit opt-out.
    fn unstable(self) -> Unstable<Self>
    where
        Self: Sized,
    {
        Unstable::new(self)
    }

    /// Ternary: reads `self` each bar and yields `if_true`'s value on
    /// [`true`], `if_false`'s on [`false`], `None` when `self` reads `None`.
    /// All three sources are advanced every bar (a branch that doesn't fire
    /// this bar still warms up in the background), matching how
    /// [`Combine`](crate::indicators::Combine) treats its two operands.
    ///
    /// Reads left-to-right: `adx.gt(25.0).if_else(roc, Value::new(0.0))`
    /// is "ADX above 25 → return the ROC value, else 0" — an ADX-gated
    /// momentum score in one expression.
    ///
    /// See [`IfElse`] for the full semantics.
    fn if_else<T, F>(self, if_true: T, if_false: F) -> IfElse<Self, T, F>
    where
        Self: Sized,
        T: Indicator<Input = Self::Input, Output = Real>,
        F: Indicator<Input = Self::Input, Output = Real>,
        Self::Input: Clone,
    {
        IfElse::new(self, if_true, if_false)
    }
}

impl<I: Indicator<Output = bool> + ?Sized> BoolIndicatorExt for I {}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::indicators::logic::Const;

    /// Emits a scripted sequence of booleans, for exercising combinators.
    struct Script {
        values: Vec<bool>,
        idx: usize,
        value: Option<bool>,
    }
    impl Script {
        fn new(values: &[bool]) -> Self {
            Self {
                values: values.to_vec(),
                idx: 0,
                value: None,
            }
        }
    }
    impl Indicator for Script {
        type Input = ();
        type Output = bool;
        fn update(&mut self, _: ()) -> Option<bool> {
            let v = self.values[self.idx];
            self.idx += 1;
            self.value = Some(v);
            self.value
        }
        fn value(&self) -> Option<bool> {
            self.value
        }
        fn warm_up_period(&self) -> usize {
            1
        }
        fn reset(&mut self) {
            self.idx = 0;
            self.value = None;
        }
    }

    #[test]
    fn boolean_combinators() {
        assert_eq!(
            Const::<()>::new(true).and(Const::new(true)).update(()),
            Some(true)
        );
        assert_eq!(
            Const::<()>::new(true).and(Const::new(false)).update(()),
            Some(false)
        );
        assert_eq!(
            Const::<()>::new(false).or(Const::new(true)).update(()),
            Some(true)
        );
        assert_eq!(
            Const::<()>::new(true).xor(Const::new(false)).update(()),
            Some(true)
        );
        assert_eq!(Const::<()>::new(false).not().update(()), Some(true));
    }

    #[test]
    fn change_fires_on_each_toggle() {
        let mut c = Script::new(&[false, false, true, true, false]).changed();
        c.update(());
        assert!(!c.is_true()); // first step: no prior
        c.update(());
        assert!(!c.is_true()); // false -> false
        c.update(());
        assert!(c.is_true()); // false -> true
        c.update(());
        assert!(!c.is_true()); // true -> true
        c.update(());
        assert!(c.is_true()); // true -> false
    }
}
