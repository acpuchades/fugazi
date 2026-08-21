//! Composable indicator transform operators and their generic carriers.
//!
//! Five carriers, each driven by an operator type so new operators are a trait
//! impl rather than a new type:
//!
//! * [`Combine`] — a *binary* op over two sources ([`BinaryOp`]). The op carries
//!   its own input/output types, so this one carrier serves arithmetic
//!   (`Real, Real → Real`: `Add`/`Sub`/`Mul`/`Div`/`Pow`/`Min`/`Max`), comparison
//!   (`Real, Real → bool`: the operators in [`compare`](super::compare)) and
//!   boolean logic (`bool, bool → bool`: the operators in [`logic`](super::logic)).
//! * [`Unary`] — a *pointwise* op over one source ([`UnaryOp`]): `Abs`, `Sign`,
//!   `Sqrt`, `Tanh`, `Sigmoid`.
//! * [`Lookback`] — a *unary* op relating a source to its own value `period`
//!   steps ago ([`LookbackOp`]): `Lag`, `Diff`, `Ratio`.
//! * [`Extreme`] — a rolling extremum over a window ([`ExtremeOp`]).
//! * [`Cumulative`] — an *unbounded* running fold ([`CumulativeOp`]): `CumSum`,
//!   `CumMax`, `CumMin`.
//!
//! Three markers wear more than one of those hats, because the operation is one
//! idea and only the carrier differs: [`AddOp`] is both binary `+` and the fold
//! behind `CumSum`, and [`MaxOp`]/[`MinOp`] serve the pairwise, rolling and
//! cumulative extremes alike.
//!
//! Candle field accessors live in `candle`.

use std::fmt::Debug;
use std::marker::PhantomData;

use fugazi_derive::SaveState;

use crate::indicator::Indicator;
use crate::indicators::stats::Ring;
use crate::indicators::stats::WindowExtreme;
use crate::num::{max_finite, min_finite};
use crate::types::Real;

// ---------------------------------------------------------------------------
// Binary combination of two sources
// ---------------------------------------------------------------------------

/// A pointwise binary operator over two warmed-up source outputs.
///
/// Carried *by value* (so an operator can hold state, such as a comparison's
/// tolerance) and generic over its input/output types via associated types, so
/// the single [`Combine`] carrier serves arithmetic, comparison and boolean
/// logic alike.
pub trait BinaryOp {
    /// The left source's output type.
    type Lhs;
    /// The right source's output type.
    type Rhs;
    /// The type this operator produces.
    type Output: Clone + Debug;
    /// Combine `lhs` and `rhs`, or `None` when the result is undefined (e.g.
    /// division by zero).
    fn apply(&self, lhs: Self::Lhs, rhs: Self::Rhs) -> Option<Self::Output>;
}

/// Pointwise combination of two indicator sources, parameterised by operator.
///
/// Use the aliases ([`Add`], [`Sub`], [`Mul`], [`Div`], the comparisons in
/// [`compare`](super::compare), the logic ops in [`logic`](super::logic)) or the
/// `IndicatorExt`/`BoolIndicatorExt` builders. Feeds the same input to both sources
/// (hence `Input: Clone`) and yields `None` until both are warmed up.
#[derive(Debug, Clone, SaveState)]
pub struct Combine<L, R, Op: BinaryOp> {
    #[state(source)]
    lhs: L,
    #[state(source)]
    rhs: R,
    // Config, rebuilt identically from the spec (comparisons carry only an
    // `epsilon`), and not serde-serializable in general.
    #[state(skip)]
    op: Op,
    /// Latest combined value; `None` until both sources are ready (and the
    /// operation is defined). A recomputed cache — `update` refreshes it before
    /// the next `value()` read — so it is not part of the saved state (which also
    /// avoids constraining `Op::Output: Serialize`).
    #[state(skip)]
    pub value: Option<Op::Output>,
}

impl<L, R, Op: BinaryOp + Default> Combine<L, R, Op> {
    /// Combine `lhs` and `rhs` with the operator's default configuration.
    pub fn new(lhs: L, rhs: R) -> Self {
        Self::with_op(lhs, rhs, Op::default())
    }
}

impl<L, R, Op: BinaryOp> Combine<L, R, Op> {
    /// Combine `lhs` and `rhs` with an explicit operator value (e.g. a
    /// comparison with a custom tolerance).
    pub fn with_op(lhs: L, rhs: R, op: Op) -> Self {
        Self {
            lhs,
            rhs,
            op,
            value: None,
        }
    }
}

impl<L, R, Op> Indicator for Combine<L, R, Op>
where
    Op: BinaryOp,
    L: Indicator<Output = Op::Lhs>,
    R: Indicator<Input = L::Input, Output = Op::Rhs>,
    L::Input: Clone,
{
    type Input = L::Input;
    type Output = Op::Output;

    fn update(&mut self, input: Self::Input) -> Option<Op::Output> {
        let lhs = self.lhs.update(input.clone());
        let rhs = self.rhs.update(input);
        self.value = match (lhs, rhs) {
            (Some(l), Some(r)) => self.op.apply(l, r),
            _ => None,
        };
        self.value.clone()
    }

    fn value(&self) -> Option<Op::Output> {
        self.value.clone()
    }

    fn warm_up_bars(&self) -> usize {
        self.lhs.warm_up_bars().max(self.rhs.warm_up_bars())
    }

    fn unstable_bars(&self) -> usize {
        // Settled once the later-settling side is, expressed relative to this
        // indicator's own (max-of-both) warm-up.
        self.lhs.stable_bars().max(self.rhs.stable_bars()) - self.warm_up_bars()
    }

    fn reset(&mut self) {
        self.lhs.reset();
        self.rhs.reset();
        self.value = None;
    }

    fn save_state(&self) -> serde_json::Value {
        self.save_state_fields()
    }

    fn load_state(&mut self, state: &serde_json::Value) -> Result<(), String> {
        self.load_state_fields(state)
    }
}

/// `lhs + rhs`.
#[derive(Debug, Clone, Copy, Default)]
pub struct AddOp;
impl BinaryOp for AddOp {
    type Lhs = Real;
    type Rhs = Real;
    type Output = Real;
    fn apply(&self, lhs: Real, rhs: Real) -> Option<Real> {
        Some(lhs + rhs)
    }
}

/// `lhs - rhs`.
#[derive(Debug, Clone, Copy, Default)]
pub struct SubOp;
impl BinaryOp for SubOp {
    type Lhs = Real;
    type Rhs = Real;
    type Output = Real;
    fn apply(&self, lhs: Real, rhs: Real) -> Option<Real> {
        Some(lhs - rhs)
    }
}

/// `lhs * rhs`.
#[derive(Debug, Clone, Copy, Default)]
pub struct MulOp;
impl BinaryOp for MulOp {
    type Lhs = Real;
    type Rhs = Real;
    type Output = Real;
    fn apply(&self, lhs: Real, rhs: Real) -> Option<Real> {
        Some(lhs * rhs)
    }
}

/// `lhs / rhs`, or `None` when `rhs == 0`.
#[derive(Debug, Clone, Copy, Default)]
pub struct DivOp;
impl BinaryOp for DivOp {
    type Lhs = Real;
    type Rhs = Real;
    type Output = Real;
    fn apply(&self, lhs: Real, rhs: Real) -> Option<Real> {
        if rhs == 0.0 { None } else { Some(lhs / rhs) }
    }
}

/// `lhs` raised to the power `rhs`, or `None` when the result is not finite.
///
/// That guard covers both halves of what `powf` answers with a `NaN` — a base
/// and exponent with no real answer (`(-8)^(1/3)`, `0^-1`) — and an overflow to
/// infinity, exactly the contract [`Exp`](super::Exp) applies to its own
/// results. `0^0` is `1`, as `powf` has it.
#[derive(Debug, Clone, Copy, Default)]
pub struct PowOp;
impl BinaryOp for PowOp {
    type Lhs = Real;
    type Rhs = Real;
    type Output = Real;
    fn apply(&self, lhs: Real, rhs: Real) -> Option<Real> {
        let y = lhs.powf(rhs);
        y.is_finite().then_some(y)
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
/// Pointwise power of two sources (`None` when the result is not finite).
pub type Pow<L, R> = Combine<L, R, PowOp>;

// ---------------------------------------------------------------------------
// Pointwise transformation of one source
// ---------------------------------------------------------------------------

/// A pointwise unary operator over one warmed-up source output.
///
/// Stateless by construction: the answer depends on this sample alone, never on
/// the window or on the source's own past — that is [`LookbackOp`]'s job. Every
/// implementor is a zero-sized marker, so [`Unary`] holds it as a `PhantomData`
/// and the method is an associated function rather than taking `&self`.
pub trait UnaryOp {
    /// Transform `x`, or `None` where the operator has no answer: an input
    /// outside its domain (`√x` of a negative), or a result it cannot represent.
    /// The same contract [`Log`](super::Log) applies to its non-positive inputs
    /// and [`Exp`](super::Exp) to its overflows.
    fn apply(x: Real) -> Option<Real>;
}

/// Pointwise transform of a single source, parameterised by operator.
///
/// Use the aliases ([`Abs`], [`Sign`], [`Sqrt`], [`Tanh`], [`Sigmoid`]) or the
/// `IndicatorExt` builders (`a.abs()`, `a.sqrt()`). Carries no state and adds no
/// warm-up of its own: the output tracks the source one-for-one, except on the
/// samples the operator declines.
///
/// ```
/// use fugazi::prelude::*;
/// use fugazi::indicators::{Abs, Identity};
///
/// let mut abs = Abs::new(Identity::new());
/// assert_eq!(abs.update(-3.0), Some(3.0));
/// ```
#[derive(Debug, Clone, SaveState)]
pub struct Unary<S, Op> {
    #[state(source)]
    source: S,
    /// Latest transformed value; `None` whenever the source is, and on any
    /// sample the operator has no answer for. A recomputed cache — `update`
    /// refreshes it before the next `value()` read — so it is not saved state.
    #[state(skip)]
    pub value: Option<Real>,
    #[state(skip)]
    _op: PhantomData<fn() -> Op>,
}

impl<S, Op> Unary<S, Op> {
    /// Wrap `source` with the operator's pointwise transform.
    pub fn new(source: S) -> Self {
        Self {
            source,
            value: None,
            _op: PhantomData,
        }
    }
}

impl<S, Op> Indicator for Unary<S, Op>
where
    S: Indicator<Output = Real>,
    Op: UnaryOp,
{
    type Input = S::Input;
    type Output = Real;

    fn update(&mut self, input: Self::Input) -> Option<Real> {
        self.value = self.source.update(input).and_then(Op::apply);
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

/// Absolute value: `|x|`.
#[derive(Debug, Clone, Copy)]
pub struct AbsOp;
impl UnaryOp for AbsOp {
    fn apply(x: Real) -> Option<Real> {
        Some(x.abs())
    }
}

/// Sign of the input: `1`, `-1`, or `0` at exactly zero.
///
/// Deliberately not [`f64::signum`], which answers `±1` for `±0.0` — a zero
/// difference has no direction, and every call site here (a spread's sign, a
/// position's intended side) wants the flat case answered as flat. A `NaN`
/// compares false against all three, so it is declined rather than assigned an
/// arbitrary direction.
#[derive(Debug, Clone, Copy)]
pub struct SignOp;
impl UnaryOp for SignOp {
    fn apply(x: Real) -> Option<Real> {
        if x > 0.0 {
            Some(1.0)
        } else if x < 0.0 {
            Some(-1.0)
        } else if x == 0.0 {
            Some(0.0)
        } else {
            None
        }
    }
}

/// Square root, `None` on a negative input (and on `NaN`, which fails the
/// comparison) — the domain guard [`Log`](super::Log) applies to its own.
#[derive(Debug, Clone, Copy)]
pub struct SqrtOp;
impl UnaryOp for SqrtOp {
    fn apply(x: Real) -> Option<Real> {
        (x >= 0.0).then(|| x.sqrt())
    }
}

/// Hyperbolic tangent, squashing the real line into `(-1, 1)`.
///
/// The bounded response a sizing expression wants: linear near zero, saturating
/// past |x| ≈ 2, and — unlike a hand-rolled clamp — smooth at the join. Only a
/// `NaN` arriving from the source is declined; `±∞` map to `±1`.
#[derive(Debug, Clone, Copy)]
pub struct TanhOp;
impl UnaryOp for TanhOp {
    fn apply(x: Real) -> Option<Real> {
        let y = x.tanh();
        y.is_finite().then_some(y)
    }
}

/// Logistic sigmoid, `1 / (1 + e^-x)`, squashing the real line into `(0, 1)`.
///
/// The one-sided twin of [`TanhOp`] (`sigmoid(x) = (1 + tanh(x/2))/2`), for the
/// case where the output is a fraction rather than a signed response. Saturates
/// to a representable `0` or `1` rather than overflowing, so only a `NaN` from
/// the source is declined.
#[derive(Debug, Clone, Copy)]
pub struct SigmoidOp;
impl UnaryOp for SigmoidOp {
    fn apply(x: Real) -> Option<Real> {
        let y = 1.0 / (1.0 + (-x).exp());
        y.is_finite().then_some(y)
    }
}

/// Absolute value of a source.
pub type Abs<S> = Unary<S, AbsOp>;
/// Sign of a source: `1`, `-1` or `0`.
pub type Sign<S> = Unary<S, SignOp>;
/// Square root of a source (`None` on negative inputs).
pub type Sqrt<S> = Unary<S, SqrtOp>;
/// Hyperbolic tangent of a source, bounded to `(-1, 1)`.
pub type Tanh<S> = Unary<S, TanhOp>;
/// Logistic sigmoid of a source, bounded to `(0, 1)`.
pub type Sigmoid<S> = Unary<S, SigmoidOp>;

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
#[derive(Debug, Clone, SaveState)]
pub struct Lookback<I, Op> {
    #[state(source)]
    source: I,
    period: usize,
    #[state(window)]
    buffer: Ring<Option<Real>>,
    /// Latest value; `None` until `period` updates have elapsed.
    pub value: Option<Real>,
    #[state(skip)]
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
            // `period`, not `period + 1`: the deque this replaces pushed and
            // then popped, so it transiently held one extra. `Ring::push` evicts
            // and returns in a single step, so the extra slot is dead weight.
            buffer: Ring::new(period),
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
        // Once full, the evicted sample *is* the one `period` steps back — the
        // same value the `push_back` / `pop_front` pair produced, in one step.
        let past = self.buffer.push(current).flatten();
        self.value = match (current, past) {
            (Some(current), Some(past)) => Op::apply(current, past),
            _ => None,
        };
        self.value
    }

    fn value(&self) -> Option<Real> {
        self.value
    }

    fn warm_up_bars(&self) -> usize {
        // The lagged operand needs a source output `period` steps before the
        // current one. `max(1)` so a `warm_up = 0` source (e.g. `Value`) still
        // requires the full period of updates.
        self.source.warm_up_bars().max(1) + self.period
    }

    fn unstable_bars(&self) -> usize {
        self.source.unstable_bars()
    }

    fn reset(&mut self) {
        self.source.reset();
        self.buffer.clear();
        self.value = None;
    }

    fn save_state(&self) -> serde_json::Value {
        self.save_state_fields()
    }

    fn load_state(&mut self, state: &serde_json::Value) -> Result<(), String> {
        self.load_state_fields(state)
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

/// The larger of two values — pairwise ([`BinaryOp`]), rolling ([`ExtremeOp`])
/// and cumulative ([`CumulativeOp`]). One marker, three carriers: taking the
/// maximum is a single idea, and only the shape of the window differs.
#[derive(Debug, Clone, Copy, Default)]
pub struct MaxOp;
impl ExtremeOp for MaxOp {
    fn dominates(incoming: Real, current: Real) -> bool {
        incoming >= current
    }
}
impl BinaryOp for MaxOp {
    type Lhs = Real;
    type Rhs = Real;
    type Output = Real;
    fn apply(&self, lhs: Real, rhs: Real) -> Option<Real> {
        Some(max_finite(lhs, rhs))
    }
}

/// The smaller of two values — the twin of [`MaxOp`], in the same three roles.
#[derive(Debug, Clone, Copy, Default)]
pub struct MinOp;
impl ExtremeOp for MinOp {
    fn dominates(incoming: Real, current: Real) -> bool {
        incoming <= current
    }
}
impl BinaryOp for MinOp {
    type Lhs = Real;
    type Rhs = Real;
    type Output = Real;
    fn apply(&self, lhs: Real, rhs: Real) -> Option<Real> {
        Some(min_finite(lhs, rhs))
    }
}

/// Pointwise maximum of two sources.
///
/// Compares with `>`, via `crate::num::max_finite` — so a `NaN` operand
/// propagates rather than being suppressed the way [`f64::max`] would. Every
/// source in the crate emits a finite value or `None`, so that case is reachable
/// only through an expression that manufactures one.
pub type Max<L, R> = Combine<L, R, MaxOp>;
/// Pointwise minimum of two sources — the twin of [`Max`].
pub type Min<L, R> = Combine<L, R, MinOp>;

/// Rolling extremum of a source over a window, parameterised by direction.
///
/// Use the aliases ([`RollingMax`], [`RollingMin`]) or the `IndicatorExt`
/// builders (`a.rolling_max(20)`). Produces `None` until the window is full.
#[derive(Debug, Clone, SaveState)]
pub struct Extreme<S, Op> {
    #[state(source)]
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

    fn value(&self) -> Option<Real> {
        self.value
    }

    fn warm_up_bars(&self) -> usize {
        // A full window of source outputs, the first of which arrives at the
        // source's own warm-up. `max(1)` so a `warm_up = 0` source (e.g.
        // `Value`) still requires the full window of updates.
        self.source.warm_up_bars().max(1) + self.inner.period() - 1
    }

    fn unstable_bars(&self) -> usize {
        self.source.unstable_bars()
    }

    fn reset(&mut self) {
        self.source.reset();
        self.inner.reset();
        self.value = None;
    }

    fn save_state(&self) -> serde_json::Value {
        self.save_state_fields()
    }

    fn load_state(&mut self, state: &serde_json::Value) -> Result<(), String> {
        self.load_state_fields(state)
    }
}

/// Rolling maximum of a source over `period` steps.
pub type RollingMax<S> = Extreme<S, MaxOp>;
/// Rolling minimum of a source over `period` steps.
pub type RollingMin<S> = Extreme<S, MinOp>;

// ---------------------------------------------------------------------------
// Unbounded running accumulation
// ---------------------------------------------------------------------------

/// A fold over every sample the source has ever produced.
///
/// The unbounded counterpart of [`ExtremeOp`]: no window, no eviction, so the
/// state is one `Real` and the update is O(1) forever. Implementors are the
/// existing arithmetic and extremum markers — cumulative summation *is* folding
/// with [`AddOp`] — so this trait adds no new operator types.
pub trait CumulativeOp {
    /// Fold `x` into the running value. `acc` is `None` on the first sample, so
    /// an operator with no identity element (the extremes) can seed from the
    /// sample itself rather than from a sentinel.
    fn fold(acc: Option<Real>, x: Real) -> Real;
}

impl CumulativeOp for AddOp {
    fn fold(acc: Option<Real>, x: Real) -> Real {
        acc.unwrap_or(0.0) + x
    }
}

impl CumulativeOp for MaxOp {
    fn fold(acc: Option<Real>, x: Real) -> Real {
        match acc {
            Some(a) => max_finite(a, x),
            None => x,
        }
    }
}

impl CumulativeOp for MinOp {
    fn fold(acc: Option<Real>, x: Real) -> Real {
        match acc {
            Some(a) => min_finite(a, x),
            None => x,
        }
    }
}

/// Running fold of every sample a source has produced, parameterised by
/// operator.
///
/// Use the aliases ([`CumSum`], [`CumMax`], [`CumMin`]) or the `IndicatorExt`
/// builders (`a.cum_sum()`). Where the value *starts* is part of its meaning —
/// the total is anchored to the first bar of the run, exactly as
/// [`Obv`](super::Obv) is — so this reports ready as soon as its source is and
/// leaves [`unstable_bars`](Indicator::unstable_bars) to the source.
///
/// A `CumMax` is what turns any series into a drawdown: `x / cum_max(x) - 1`
/// generalises the book-anchored `!drawdown` to an arbitrary expression.
///
/// ```
/// use fugazi::prelude::*;
/// use fugazi::indicators::{CumSum, Identity};
///
/// let mut total = CumSum::new(Identity::new());
/// assert_eq!(total.update(2.0), Some(2.0));
/// assert_eq!(total.update(3.0), Some(5.0));
/// ```
#[derive(Debug, Clone, SaveState)]
pub struct Cumulative<S, Op> {
    #[state(source)]
    source: S,
    /// The running value; `None` until the source's first output. Genuine
    /// state, not a cache — it *is* the accumulator, so it is saved and
    /// restored with the rest of the run.
    pub value: Option<Real>,
    #[state(skip)]
    _op: PhantomData<fn() -> Op>,
}

impl<S, Op> Cumulative<S, Op> {
    /// Accumulate `source` from its first output onward.
    pub fn new(source: S) -> Self {
        Self {
            source,
            value: None,
            _op: PhantomData,
        }
    }
}

impl<S, Op> Indicator for Cumulative<S, Op>
where
    S: Indicator<Output = Real>,
    Op: CumulativeOp,
{
    type Input = S::Input;
    type Output = Real;

    fn update(&mut self, input: Self::Input) -> Option<Real> {
        // A bar the source declines leaves the accumulator untouched *and
        // unreported*: `None` here means "no reading this bar", not "the total
        // reset". The next sample folds into the value carried across the gap.
        if let Some(x) = self.source.update(input) {
            self.value = Some(Op::fold(self.value, x));
            self.value
        } else {
            None
        }
    }

    fn value(&self) -> Option<Real> {
        self.value
    }

    /// Ready as soon as the source is; the running total is *anchored*, not
    /// unstable — where it starts is part of its meaning, so
    /// [`unstable_bars`](Indicator::unstable_bars) stays the source's own.
    fn warm_up_bars(&self) -> usize {
        self.source.warm_up_bars().max(1)
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

/// Running total of every sample a source has produced.
pub type CumSum<S> = Cumulative<S, AddOp>;
/// Running maximum since the start of the run (the unbounded [`RollingMax`]).
pub type CumMax<S> = Cumulative<S, MaxOp>;
/// Running minimum since the start of the run (the unbounded [`RollingMin`]).
pub type CumMin<S> = Cumulative<S, MinOp>;

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

    /// The buffer behind `Lookback` moved from a `VecDeque<Option<Real>>` to a
    /// `Ring<Option<Real>>`. This is a literal blob in the shape the old derive
    /// wrote — `buffer` as a bare oldest-first array — and it must still resume a
    /// run identically, including the `null`s a source that yielded `None`
    /// leaves behind.
    #[test]
    fn the_pre_ring_lookback_state_still_resumes() {
        use crate::Indicator as _;

        let mut paused: Diff<Identity<Real>> = Diff::new(Identity::new(), 2);
        let blob = serde_json::json!({
            "source": paused.source.save_state(),
            "period": 2,
            "buffer": [10.0, 20.0],
            "value": null,
        });
        paused.load_state(&blob).expect("legacy lookback state");

        // A twin that was never paused, fed the same prefix.
        let mut twin: Diff<Identity<Real>> = Diff::new(Identity::new(), 2);
        twin.update(10.0);
        twin.update(20.0);

        for x in [30.0, 40.0, 50.0] {
            assert_eq!(paused.update(x), twin.update(x), "diverged after resume");
        }
        // 30 - 10 was the first full-window reading, so the window really was
        // restored at two samples rather than at its capacity.
        assert_eq!(twin.value, Some(50.0 - 30.0));
    }

    /// A `None` from the source has to survive the round trip as a `null`,
    /// because it is what makes the *next* `period` readings `None` too.
    #[test]
    fn a_lookback_window_holding_none_round_trips() {
        use crate::Indicator as _;
        use crate::indicators::Sma;

        // An `Sma` of period 2 yields `None` on its first sample, so the
        // lookback buffer's oldest slot is a genuine `None`.
        let build = || Diff::new(Sma::new(Identity::<Real>::new(), 2), 2);
        let (mut a, mut b) = (build(), build());
        for x in [1.0, 2.0] {
            a.update(x);
            b.update(x);
        }
        let saved = a.save_state();
        assert_eq!(
            saved["buffer"],
            serde_json::json!([null, 1.5]),
            "buffer is no longer a bare oldest-first array, or lost its `None`"
        );
        let mut restored = build();
        restored.load_state(&saved).expect("round trip");
        for x in [3.0, 4.0, 5.0] {
            assert_eq!(restored.update(x), b.update(x), "diverged after resume");
        }
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
