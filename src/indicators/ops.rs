//! Composable indicator transform operators and their generic carriers.
//!
//! Two carriers, each driven by a zero-sized operator marker so new operators
//! are a trait impl rather than a new type:
//!
//! * [`Combine`] — a *binary* op over two sources ([`BinaryOp`]): `Add`, `Sub`,
//!   `Mul`, `Div`.
//! * [`Lookback`] — a *unary* op relating a source to its own value `period`
//!   steps ago ([`LookbackOp`]): `Lag`, `Diff`, `Ratio`.
//!
//! Candle field accessors live in [`candle`](super::candle); comparison
//! operators (which yield signals) live in
//! [`signals::compare`](crate::signals::compare).

use std::collections::VecDeque;
use std::marker::PhantomData;

use crate::indicator::Indicator;
use crate::indicators::stats::WindowExtreme;
use crate::types::Real;

// ---------------------------------------------------------------------------
// Binary combination of two sources
// ---------------------------------------------------------------------------

/// A pointwise binary arithmetic operator over two warmed-up source outputs.
pub trait BinaryOp {
    /// Combine `lhs` and `rhs`, or `None` when the result is undefined (e.g.
    /// division by zero).
    fn apply(lhs: Real, rhs: Real) -> Option<Real>;
}

/// Pointwise combination of two indicator sources, parameterised by operator.
///
/// Use the aliases ([`Add`], [`Sub`], [`Mul`], [`Div`]) or the `IndicatorExt`
/// builders (`a.div(b)`, …). Feeds the same input to both sources (hence
/// `Input: Clone`) and yields `None` until both are warmed up.
#[derive(Debug, Clone)]
pub struct Combine<L, R, Op> {
    lhs: L,
    rhs: R,
    /// Latest combined value; `None` until both sources are ready (and the
    /// operation is defined).
    pub value: Option<Real>,
    _op: PhantomData<fn() -> Op>,
}

impl<L, R, Op> Combine<L, R, Op> {
    pub fn new(lhs: L, rhs: R) -> Self {
        Self {
            lhs,
            rhs,
            value: None,
            _op: PhantomData,
        }
    }
}

impl<L, R, Op> Indicator for Combine<L, R, Op>
where
    L: Indicator<Output = Real>,
    R: Indicator<Input = L::Input, Output = Real>,
    L::Input: Clone,
    Op: BinaryOp,
{
    type Input = L::Input;
    type Output = Real;

    fn update(&mut self, input: Self::Input) -> Option<Real> {
        let lhs = self.lhs.update(input.clone());
        let rhs = self.rhs.update(input);
        self.value = match (lhs, rhs) {
            (Some(l), Some(r)) => Op::apply(l, r),
            _ => None,
        };
        self.value
    }

    fn current(&self) -> Option<Real> {
        self.value
    }

    fn reset(&mut self) {
        self.lhs.reset();
        self.rhs.reset();
        self.value = None;
    }
}

/// `lhs + rhs`.
#[derive(Debug, Clone, Copy)]
pub struct AddOp;
impl BinaryOp for AddOp {
    fn apply(lhs: Real, rhs: Real) -> Option<Real> {
        Some(lhs + rhs)
    }
}

/// `lhs - rhs`.
#[derive(Debug, Clone, Copy)]
pub struct SubOp;
impl BinaryOp for SubOp {
    fn apply(lhs: Real, rhs: Real) -> Option<Real> {
        Some(lhs - rhs)
    }
}

/// `lhs * rhs`.
#[derive(Debug, Clone, Copy)]
pub struct MulOp;
impl BinaryOp for MulOp {
    fn apply(lhs: Real, rhs: Real) -> Option<Real> {
        Some(lhs * rhs)
    }
}

/// `lhs / rhs`, or `None` when `rhs == 0`.
#[derive(Debug, Clone, Copy)]
pub struct DivOp;
impl BinaryOp for DivOp {
    fn apply(lhs: Real, rhs: Real) -> Option<Real> {
        if rhs == 0.0 {
            None
        } else {
            Some(lhs / rhs)
        }
    }
}

/// Pointwise sum of two sources.
pub type Add<L, R> = Combine<L, R, AddOp>;
/// Pointwise difference of two sources.
pub type Sub<L, R> = Combine<L, R, SubOp>;
/// Pointwise product of two sources.
pub type Mul<L, R> = Combine<L, R, MulOp>;
/// Pointwise quotient of two sources (`None` on divide-by-zero).
pub type Div<L, R> = Combine<L, R, DivOp>;

// ---------------------------------------------------------------------------
// Unary operators relating a source to its own past
// ---------------------------------------------------------------------------

/// A unary operator relating a source's `current` output to its value `period`
/// steps ago (`past`).
pub trait LookbackOp {
    /// Produce the output from the current and lagged values, or `None` when
    /// undefined (e.g. division by zero).
    fn apply(current: Real, past: Real) -> Option<Real>;
}

/// Relates a single source to its own value `period` steps in the past.
///
/// Use the aliases ([`Lag`], [`Diff`], [`Ratio`]) or the `IndicatorExt`
/// builders (`a.lag(1)`, `a.diff(1)`, `a.ratio(1)`). Buffers the last
/// `period` outputs, so each update is O(1); yields `None` for the first
/// `period` updates.
#[derive(Debug, Clone)]
pub struct Lookback<I, Op> {
    source: I,
    period: usize,
    buffer: VecDeque<Option<Real>>,
    /// Latest value; `None` until `period` updates have elapsed.
    pub value: Option<Real>,
    _op: PhantomData<fn() -> Op>,
}

impl<I, Op> Lookback<I, Op> {
    /// # Panics
    /// Panics if `period` is zero.
    pub fn new(source: I, period: usize) -> Self {
        assert!(period > 0, "lookback period must be greater than zero");
        Self {
            source,
            period,
            buffer: VecDeque::with_capacity(period + 1),
            value: None,
            _op: PhantomData,
        }
    }

    pub fn period(&self) -> usize {
        self.period
    }
}

impl<I, Op> Indicator for Lookback<I, Op>
where
    I: Indicator<Output = Real>,
    Op: LookbackOp,
{
    type Input = I::Input;
    type Output = Real;

    fn update(&mut self, input: Self::Input) -> Option<Real> {
        let current = self.source.update(input);
        self.buffer.push_back(current);
        let past = if self.buffer.len() > self.period {
            self.buffer.pop_front().flatten()
        } else {
            None
        };
        self.value = match (current, past) {
            (Some(current), Some(past)) => Op::apply(current, past),
            _ => None,
        };
        self.value
    }

    fn current(&self) -> Option<Real> {
        self.value
    }

    fn reset(&mut self) {
        self.source.reset();
        self.buffer.clear();
        self.value = None;
    }
}

/// The source's value `period` steps ago.
#[derive(Debug, Clone, Copy)]
pub struct LagOp;
impl LookbackOp for LagOp {
    fn apply(_current: Real, past: Real) -> Option<Real> {
        Some(past)
    }
}

/// Discrete diff / first difference: `current - past`.
#[derive(Debug, Clone, Copy)]
pub struct DiffOp;
impl LookbackOp for DiffOp {
    fn apply(current: Real, past: Real) -> Option<Real> {
        Some(current - past)
    }
}

/// Ratio to the past value: `current / past` (`None` when `past == 0`).
#[derive(Debug, Clone, Copy)]
pub struct RatioOp;
impl LookbackOp for RatioOp {
    fn apply(current: Real, past: Real) -> Option<Real> {
        if past == 0.0 {
            None
        } else {
            Some(current / past)
        }
    }
}

/// Rate of change as a percentage: `100·(current − past)/past` (`None` when
/// `past == 0`). Matches TA-Lib's `ROC`.
#[derive(Debug, Clone, Copy)]
pub struct RocOp;
impl LookbackOp for RocOp {
    fn apply(current: Real, past: Real) -> Option<Real> {
        if past == 0.0 {
            None
        } else {
            Some(100.0 * (current - past) / past)
        }
    }
}

/// Delays a source's output by `period` steps.
pub type Lag<I> = Lookback<I, LagOp>;
/// Discrete diff of a source over `period` steps.
pub type Diff<I> = Lookback<I, DiffOp>;
/// Ratio of a source to its value `period` steps ago.
pub type Ratio<I> = Lookback<I, RatioOp>;
/// Percentage rate of change of a source over `period` steps.
pub type Roc<I> = Lookback<I, RocOp>;

// ---------------------------------------------------------------------------
// Rolling extremum over a window
// ---------------------------------------------------------------------------

/// Direction marker for a rolling extremum ([`Extreme`]).
pub trait ExtremeOp {
    /// True if `incoming` is at least as extreme as `current` (so `current` can
    /// be discarded).
    fn dominates(incoming: Real, current: Real) -> bool;
}

/// Running maximum.
#[derive(Debug, Clone, Copy)]
pub struct MaxOp;
impl ExtremeOp for MaxOp {
    fn dominates(incoming: Real, current: Real) -> bool {
        incoming >= current
    }
}

/// Running minimum.
#[derive(Debug, Clone, Copy)]
pub struct MinOp;
impl ExtremeOp for MinOp {
    fn dominates(incoming: Real, current: Real) -> bool {
        incoming <= current
    }
}

/// Rolling extremum of a source over a window, parameterised by direction.
///
/// Use the aliases ([`RollingMax`], [`RollingMin`]) or the `IndicatorExt`
/// builders (`a.rolling_max(20)`). Produces `None` until the window is full.
#[derive(Debug, Clone)]
pub struct Extreme<S, Op> {
    source: S,
    inner: WindowExtreme<Op>,
    /// Latest extremum; `None` until warmed up.
    pub value: Option<Real>,
}

impl<S, Op> Extreme<S, Op> {
    /// # Panics
    /// Panics if `period` is zero.
    pub fn new(source: S, period: usize) -> Self {
        Self {
            source,
            inner: WindowExtreme::new(period),
            value: None,
        }
    }

    pub fn period(&self) -> usize {
        self.inner.period()
    }
}

impl<S, Op> Indicator for Extreme<S, Op>
where
    S: Indicator<Output = Real>,
    Op: ExtremeOp,
{
    type Input = S::Input;
    type Output = Real;

    fn update(&mut self, input: Self::Input) -> Option<Real> {
        self.value = match self.source.update(input) {
            Some(x) => self.inner.update(x),
            None => None,
        };
        self.value
    }

    fn current(&self) -> Option<Real> {
        self.value
    }

    fn reset(&mut self) {
        self.source.reset();
        self.inner.reset();
        self.value = None;
    }
}

/// Rolling maximum of a source over `period` steps.
pub type RollingMax<S> = Extreme<S, MaxOp>;
/// Rolling minimum of a source over `period` steps.
pub type RollingMin<S> = Extreme<S, MinOp>;

#[cfg(test)]
mod tests {
    use super::*;
    use crate::indicators::{Identity, Value};

    #[test]
    fn binary_ops_combine_two_sources() {
        let mut add = Add::new(Identity::new(), Value::new(1.0));
        assert_eq!(add.update(4.0), Some(5.0));

        let mut div = Div::new(Identity::new(), Value::new(2.0));
        assert_eq!(div.update(10.0), Some(5.0));

        let mut by_zero = Div::new(Identity::new(), Value::new(0.0));
        assert_eq!(by_zero.update(10.0), None);
    }

    #[test]
    fn lookback_ops_relate_to_the_past() {
        let mut lag = Lag::new(Identity::new(), 1);
        assert_eq!(lag.update(1.0), None);
        assert_eq!(lag.update(2.0), Some(1.0));
        assert_eq!(lag.update(3.0), Some(2.0));

        let mut deriv = Diff::new(Identity::new(), 1);
        assert_eq!(deriv.update(1.0), None);
        assert_eq!(deriv.update(4.0), Some(3.0)); // 4 - 1

        let mut ratio = Ratio::new(Identity::new(), 1);
        assert_eq!(ratio.update(2.0), None);
        assert_eq!(ratio.update(6.0), Some(3.0)); // 6 / 2
    }
}
