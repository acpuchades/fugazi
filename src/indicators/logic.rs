//! Boolean-logic operators and edge detection, as bool-output indicators.
//!
//! The binary connectives ([`And`], [`Or`], [`Xor`]) reuse the [`Combine`]
//! carrier with `bool → bool` operators; [`Not`] and [`Change`] are unary
//! carriers, and [`Const`] is the constant-`bool` leaf (the twin of
//! [`Value`](crate::indicators::Value)). Build them with the
//! [`BoolIndicatorExt`](crate::indicators::BoolIndicatorExt) combinators (`a.and(b)`, `s.not()`,
//! `s.changed()`); each yields `None` until its source(s) warm up.

use std::marker::PhantomData;

use crate::indicator::Indicator;
use crate::indicators::ops::{BinaryOp, Combine};

/// `lhs && rhs`.
#[derive(Debug, Clone, Copy, Default)]
pub struct AndOp;
impl BinaryOp for AndOp {
    type Lhs = bool;
    type Rhs = bool;
    type Output = bool;
    fn apply(&self, lhs: bool, rhs: bool) -> Option<bool> {
        Some(lhs && rhs)
    }
}

/// `lhs || rhs`.
#[derive(Debug, Clone, Copy, Default)]
pub struct OrOp;
impl BinaryOp for OrOp {
    type Lhs = bool;
    type Rhs = bool;
    type Output = bool;
    fn apply(&self, lhs: bool, rhs: bool) -> Option<bool> {
        Some(lhs || rhs)
    }
}

/// `lhs ^ rhs`.
#[derive(Debug, Clone, Copy, Default)]
pub struct XorOp;
impl BinaryOp for XorOp {
    type Lhs = bool;
    type Rhs = bool;
    type Output = bool;
    fn apply(&self, lhs: bool, rhs: bool) -> Option<bool> {
        Some(lhs ^ rhs)
    }
}

/// Logical AND of two bool sources. Created via [`BoolIndicatorExt::and`](crate::indicators::BoolIndicatorExt::and).
pub type And<L, R> = Combine<L, R, AndOp>;
/// Logical OR of two bool sources. Created via [`BoolIndicatorExt::or`](crate::indicators::BoolIndicatorExt::or).
pub type Or<L, R> = Combine<L, R, OrOp>;
/// Logical XOR of two bool sources. Created via [`BoolIndicatorExt::xor`](crate::indicators::BoolIndicatorExt::xor).
pub type Xor<L, R> = Combine<L, R, XorOp>;

/// Logical negation of a bool source. Created via
/// [`BoolIndicatorExt::not`](crate::indicators::BoolIndicatorExt::not).
///
/// Stateless: `None` while the source is unwarmed, `Some(!b)` once it is ready.
#[derive(Debug, Clone)]
pub struct Not<S> {
    inner: S,
    value: Option<bool>,
}

impl<S> Not<S> {
    pub(crate) fn new(inner: S) -> Self {
        Self { inner, value: None }
    }
}

impl<S: Indicator<Output = bool>> Indicator for Not<S> {
    type Input = S::Input;
    type Output = bool;

    fn update(&mut self, input: Self::Input) -> Option<bool> {
        self.value = self.inner.update(input).map(|b| !b);
        self.value
    }

    fn value(&self) -> Option<bool> {
        self.value
    }

    fn warm_up_period(&self) -> usize {
        // `max(1)` guards a `warm_up = 0` inner (e.g. `Const`) — negation
        // still needs one `update` to observe the source.
        self.inner.warm_up_period().max(1)
    }

    fn unstable_period(&self) -> usize {
        self.inner.unstable_period()
    }

    fn reset(&mut self) {
        self.inner.reset();
        self.value = None;
    }
}

/// Toggle (change) detector. Created via
/// [`BoolIndicatorExt::changed`](crate::indicators::BoolIndicatorExt::changed)
/// for `bool`-output sources and
/// [`IndicatorExt::changed`](crate::indicators::IndicatorExt::changed) for
/// `Real`-output sources.
///
/// Fires (`Some(true)`) on the single step where the source's value differs
/// from the previous step, in either direction; `Some(false)` otherwise. It
/// is `None` until the source has produced a value on two consecutive steps
/// (the first warmed value never fires — there is no prior to compare
/// against).
///
/// Generic over the source's output type: any `PartialEq + Clone` output is
/// supported, so the same detector doubles as a bool edge detector (`s.not()`
/// / `s.and(t)` / etc.) and a Real transition detector (`!month` / `!year` /
/// any custom Real signal whose value flipping is the interesting event).
///
/// `bool` inner semantics: fires when the source's `true`/`false` flip.
/// Note that on a raw bool condition this fires on *both* the rising and
/// falling edges — pair with the condition itself
/// (`s.and(s.changed())`) if only the rising edge is wanted, or use
/// [`BecameTrue`] / [`BecameFalse`] for the pre-composed forms.
///
/// `Real` inner semantics: fires when the source's Real value differs bar
/// over bar. Ideal for calendar rollovers (`!month`, `!week_of_year`) where
/// every transition is a real "rollover" event and no rising/falling
/// distinction applies.
#[derive(Debug, Clone)]
pub struct Change<S: Indicator>
where
    S::Output: PartialEq + Clone,
{
    inner: S,
    prev: Option<S::Output>,
    value: Option<bool>,
}

impl<S: Indicator> Change<S>
where
    S::Output: PartialEq + Clone,
{
    pub(crate) fn new(inner: S) -> Self {
        Self {
            inner,
            prev: None,
            value: None,
        }
    }
}

impl<S: Indicator> Indicator for Change<S>
where
    S::Output: PartialEq + Clone,
{
    type Input = S::Input;
    type Output = bool;

    fn update(&mut self, input: Self::Input) -> Option<bool> {
        let now = self.inner.update(input);
        self.value = match (&self.prev, &now) {
            (Some(prev), Some(now)) => Some(now != prev),
            _ => None,
        };
        self.prev = now;
        self.value
    }

    fn value(&self) -> Option<bool> {
        self.value
    }

    fn warm_up_period(&self) -> usize {
        // Two consecutive warmed source values: the first never fires.
        // `max(1)` so a `warm_up = 0` inner still needs a first update
        // before a second can compare against it.
        self.inner.warm_up_period().max(1) + 1
    }

    fn unstable_period(&self) -> usize {
        self.inner.unstable_period()
    }

    fn reset(&mut self) {
        self.inner.reset();
        self.prev = None;
        self.value = None;
    }
}

/// Rising-edge detector for a `bool`-output source: fires (`Some(true)`) on
/// the single bar where the source transitions `false → true`; `Some(false)`
/// otherwise. Equivalent to `source.and(source.changed())` — bundled here so
/// callers gating on "the moment a condition begins to hold" don't need to
/// name the source twice.
///
/// Created via
/// [`BoolIndicatorExt::became_true`](crate::indicators::BoolIndicatorExt::became_true).
#[derive(Debug, Clone)]
pub struct BecameTrue<S> {
    inner: S,
    prev: Option<bool>,
    value: Option<bool>,
}

impl<S> BecameTrue<S> {
    pub(crate) fn new(inner: S) -> Self {
        Self {
            inner,
            prev: None,
            value: None,
        }
    }
}

impl<S: Indicator<Output = bool>> Indicator for BecameTrue<S> {
    type Input = S::Input;
    type Output = bool;

    fn update(&mut self, input: Self::Input) -> Option<bool> {
        let now = self.inner.update(input);
        self.value = match (self.prev, now) {
            (Some(prev), Some(now)) => Some(!prev && now),
            _ => None,
        };
        self.prev = now;
        self.value
    }

    fn value(&self) -> Option<bool> {
        self.value
    }

    fn warm_up_period(&self) -> usize {
        self.inner.warm_up_period().max(1) + 1
    }

    fn unstable_period(&self) -> usize {
        self.inner.unstable_period()
    }

    fn reset(&mut self) {
        self.inner.reset();
        self.prev = None;
        self.value = None;
    }
}

/// Falling-edge detector for a `bool`-output source: fires (`Some(true)`) on
/// the single bar where the source transitions `true → false`; `Some(false)`
/// otherwise. Mirror of [`BecameTrue`]; equivalent to
/// `source.not().and(source.changed())` bundled as one indicator.
///
/// Created via
/// [`BoolIndicatorExt::became_false`](crate::indicators::BoolIndicatorExt::became_false).
#[derive(Debug, Clone)]
pub struct BecameFalse<S> {
    inner: S,
    prev: Option<bool>,
    value: Option<bool>,
}

impl<S> BecameFalse<S> {
    pub(crate) fn new(inner: S) -> Self {
        Self {
            inner,
            prev: None,
            value: None,
        }
    }
}

impl<S: Indicator<Output = bool>> Indicator for BecameFalse<S> {
    type Input = S::Input;
    type Output = bool;

    fn update(&mut self, input: Self::Input) -> Option<bool> {
        let now = self.inner.update(input);
        self.value = match (self.prev, now) {
            (Some(prev), Some(now)) => Some(prev && !now),
            _ => None,
        };
        self.prev = now;
        self.value
    }

    fn value(&self) -> Option<bool> {
        self.value
    }

    fn warm_up_period(&self) -> usize {
        self.inner.warm_up_period().max(1) + 1
    }

    fn unstable_period(&self) -> usize {
        self.inner.unstable_period()
    }

    fn reset(&mut self) {
        self.inner.reset();
        self.prev = None;
        self.value = None;
    }
}

/// A constant-`bool` source, ignoring its input — the boolean twin of
/// [`Value`](crate::indicators::Value).
///
/// The neutral "no condition" leaf: a `Const::new(false)` fills an unused
/// entry/exit slot of a
/// [`SingleAssetStrategy`](crate::strategies::SingleAssetStrategy).
///
/// Generic over the input type so it can share an `Input` with whatever it is
/// composed against (the input is ignored).
#[derive(Debug, Clone, Copy)]
pub struct Const<In> {
    value: bool,
    _input: PhantomData<fn(In)>,
}

impl<In> Const<In> {
    pub fn new(value: bool) -> Self {
        Self {
            value,
            _input: PhantomData,
        }
    }
}

impl<In> Indicator for Const<In> {
    type Input = In;
    type Output = bool;

    fn update(&mut self, _input: In) -> Option<bool> {
        Some(self.value)
    }

    fn value(&self) -> Option<bool> {
        Some(self.value)
    }

    fn warm_up_period(&self) -> usize {
        0
    }

    fn reset(&mut self) {}
}

/// A **periodic pulse** — a boolean signal that fires `true` once every
/// `period` bars, with a *delayed* first fire on bar `period - 1`
/// (0-indexed).
///
/// So `Every::new(1)` fires on every bar (bar 0, 1, 2, …);
/// `Every::new(5)` first fires on bar 4, then bar 9, 14, 19, … A common
/// use is as a `rebalance_on` gate on a multi-position strategy — e.g.
/// weekly rebalance for a daily strategy is `Every::new(5)` (or the
/// tag-form `!every 5`).
///
/// Generic over its input like [`Const`] — the timing depends only on
/// the number of [`update`](Indicator::update) calls, not on their
/// contents.
#[derive(Debug, Clone, Copy)]
pub struct Every<In> {
    period: usize,
    /// Total bars seen so far. On update N (1-indexed) we fire iff
    /// `N % period == 0` — giving the delayed-first-fire semantics
    /// documented above.
    seen: usize,
    last: Option<bool>,
    _input: PhantomData<fn(In)>,
}

impl<In> Every<In> {
    /// A pulse that fires `true` every `period` bars (bar `period-1`
    /// first, then every `period` bars after).
    ///
    /// # Panics
    /// Panics if `period` is zero — a zero-period pulse has no meaning.
    pub fn new(period: usize) -> Self {
        assert!(period > 0, "Every::new: period must be > 0");
        Self {
            period,
            seen: 0,
            last: None,
            _input: PhantomData,
        }
    }
}

impl<In> Indicator for Every<In> {
    type Input = In;
    type Output = bool;

    fn update(&mut self, _input: In) -> Option<bool> {
        self.seen = self.seen.saturating_add(1);
        let fires = self.seen.is_multiple_of(self.period);
        self.last = Some(fires);
        Some(fires)
    }

    fn value(&self) -> Option<bool> {
        self.last
    }

    /// Always ready — `Every` emits `Some(bool)` from the first bar (it
    /// just emits `false` while inside a period). Wrap in
    /// [`Unstable`](crate::indicators::Unstable) if you need to opt an
    /// enclosing strategy's readiness gate out of the pulse's contribution
    /// (not usually needed since it's zero already).
    fn warm_up_period(&self) -> usize {
        0
    }

    fn reset(&mut self) {
        self.seen = 0;
        self.last = None;
    }
}

#[cfg(test)]
mod every_tests {
    use super::*;
    use crate::Indicator;

    #[test]
    fn every_1_fires_on_every_bar() {
        let mut e: Every<()> = Every::new(1);
        for _ in 0..5 {
            assert_eq!(e.update(()), Some(true));
        }
    }

    #[test]
    fn every_5_first_fires_on_bar_four_then_periodic() {
        let mut e: Every<()> = Every::new(5);
        // Bars 0..3 (1st through 4th updates): false. Bar 4 (5th update): true.
        for _ in 0..4 {
            assert_eq!(e.update(()), Some(false));
        }
        assert_eq!(e.update(()), Some(true));
        // Bars 5..8: false. Bar 9 (10th update): true.
        for _ in 0..4 {
            assert_eq!(e.update(()), Some(false));
        }
        assert_eq!(e.update(()), Some(true));
    }

    #[test]
    fn every_warm_up_is_zero_but_first_reading_can_be_false() {
        let mut e: Every<()> = Every::new(3);
        assert_eq!(e.warm_up_period(), 0);
        assert_eq!(e.update(()), Some(false));
        assert_eq!(e.update(()), Some(false));
        assert_eq!(e.update(()), Some(true));
    }

    #[test]
    fn every_resets_the_counter() {
        let mut e: Every<()> = Every::new(3);
        e.update(());
        e.update(());
        e.update(()); // fires
        e.reset();
        assert_eq!(e.value(), None);
        assert_eq!(e.update(()), Some(false));
        assert_eq!(e.update(()), Some(false));
        assert_eq!(e.update(()), Some(true));
    }

    #[test]
    #[should_panic(expected = "period must be > 0")]
    fn every_rejects_zero_period() {
        let _: Every<()> = Every::new(0);
    }
}
