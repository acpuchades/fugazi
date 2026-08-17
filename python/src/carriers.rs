use crate::prelude::*;
// The binding modules were one flat namespace before the split and still read
// as one: each pulls in its siblings, so a cross-module reference needs no path.
#[allow(unused_imports)]
use crate::classes::*;
#[allow(unused_imports)]
use crate::strategy::*;
#[allow(unused_imports)]
use crate::constructors::*;
#[allow(unused_imports)]
use crate::sources::*;
#[allow(unused_imports)]
use crate::metrics::*;
#[allow(unused_imports)]
use crate::spec::*;

// ---------------------------------------------------------------------------
// Shared type-erasure vocabulary (fugazi::runtime)
//
// The core library's `runtime::PayloadIndicatorSync` adds `Send + Sync` and an
// autotrait-preserving deep clone on top of `PayloadIndicator` — exactly what pyo3
// pyclasses need on every field. Every wrapper in Python that used to hold its
// own erased trait object (Source<I> for Real, SignalBox<I> for bool,
// StrSource<I> for Arc<str>, AtomBox<I> for Atom) collapses to a single
// generic `TypedSource<In, Out>` that carries the runtime handle and
// compile-time markers for the input and output types.
//
// The one exception is `MultiBox<I>` / `DynMulti<I>`: multi-output indicators
// emit a value struct (MacdValue, BollingerValue, …) that maps to `Vec<Real>`
// + `&'static [&'static str]` at the Python boundary, a shape that doesn't
// fit the runtime's `PayloadValue` payload enum. Unifying it would need a `Multi`
// variant on `PayloadValue` plus a `multi_names()` method on the trait — a
// library API expansion for negligible savings, so `DynMulti` intentionally
// stays local.
// ---------------------------------------------------------------------------

/// A runtime-typed handle carrying compile-time `In`/`Out` markers so it
/// implements `Indicator<Input = In, Output = Out>` cleanly. The single
/// carrier that replaces Python's per-input-domain / per-output-type boxes.
///
/// Construction takes a concrete `T: Indicator<Input = In, Output = Out> +
/// Clone + Send + Sync + 'static` and wraps it through
/// [`runtime::wrap_sync`]; every subsequent method call routes through the
/// runtime's `PayloadValue` payload but the surface `Indicator` impl below keeps
/// `In`/`Out` typed at every call site.
pub(crate) struct TypedSource<In, Out>(
    pub(crate) Box<dyn runtime::PayloadIndicatorSync>,
    pub(crate) std::marker::PhantomData<fn(In) -> Out>,
);

impl<In, Out> TypedSource<In, Out>
where
    In: TryFrom<PayloadValue, Error = PayloadType>
        + TypeOf
        + Into<PayloadValue>
        + Clone
        + Send
        + Sync
        + 'static,
    Out: TryFrom<PayloadValue, Error = PayloadType>
        + TypeOf
        + Into<PayloadValue>
        + Clone
        + Send
        + Sync
        + 'static,
{
    pub(crate) fn new<T>(inner: T) -> Self
    where
        T: Indicator<Input = In, Output = Out> + Clone + Send + Sync + 'static,
    {
        Self(runtime::wrap_sync(inner), std::marker::PhantomData)
    }
}

impl<In, Out> Clone for TypedSource<In, Out> {
    fn clone(&self) -> Self {
        Self(self.0.clone(), std::marker::PhantomData)
    }
}

impl<In, Out> Indicator for TypedSource<In, Out>
where
    In: Into<PayloadValue>,
    Out: TryFrom<PayloadValue> + Clone,
{
    type Input = In;
    type Output = Out;

    fn update(&mut self, input: In) -> Option<Out> {
        let payload = self.0.update(input.into())?;
        // Out's compile-time TypeOf matches the runtime output_type() by
        // construction (Adapter blanket + TypedSource::new bounds), so the
        // TryFrom back is always Ok.
        Some(Out::try_from(payload).ok().expect(
            "TypedSource: runtime output type doesn't match compile-time Out",
        ))
    }

    fn value(&self) -> Option<Out> {
        let payload = self.0.value()?;
        Some(Out::try_from(payload).ok().expect(
            "TypedSource: runtime output type doesn't match compile-time Out",
        ))
    }

    fn warm_up_bars(&self) -> usize {
        self.0.warm_up_bars()
    }

    fn unstable_bars(&self) -> usize {
        self.0.unstable_bars()
    }

    fn reset(&mut self) {
        self.0.reset()
    }
}

/// A boxed `I -> Real` indicator — a type alias over the shared
/// [`TypedSource`] carrier. The dedicated `PayloadIndicator<I>` trait +
/// blanket impl it used to have collapsed into [`runtime::Adapter`]'s
/// coverage. Semantics match the library: `None` until warm, `Some(Real)`
/// afterwards — no bool-signal-style flattening.
pub(crate) type Source<I> = TypedSource<I, Real>;

/// A boxed `I`-input signal (bool-out). Wraps the shared [`TypedSource`]
/// carrier and adds the "always-Some" semantics Python's bool combinators
/// depend on: warm-up `None` on the underlying source is flattened to
/// `Some(false)` at every update/value read, so a `.not_()` of a warming-up
/// signal reads as `true` (matching the Python API's promise that a signal
/// has a definite `bool` at every step).
///
/// The dedicated `DynSignal<I>` trait + blanket impl it used to have
/// collapsed into [`runtime::Adapter`]'s coverage; only the flattening
/// wrapper survives here.
pub(crate) struct SignalBox<I>(pub(crate) TypedSource<I, bool>);

impl<I> SignalBox<I>
where
    I: TryFrom<PayloadValue, Error = PayloadType> + TypeOf + Into<PayloadValue> + Clone + Send + Sync + 'static,
{
    pub(crate) fn new<T>(inner: T) -> Self
    where
        T: Indicator<Input = I, Output = bool> + Clone + Send + Sync + 'static,
    {
        Self(TypedSource::new(inner))
    }
}

impl<I> Clone for SignalBox<I> {
    fn clone(&self) -> Self {
        Self(self.0.clone())
    }
}

impl<I> Indicator for SignalBox<I>
where
    I: Into<PayloadValue>,
{
    type Input = I;
    type Output = bool;
    fn update(&mut self, input: I) -> Option<bool> {
        Some(self.0.update(input).unwrap_or(false))
    }
    fn value(&self) -> Option<bool> {
        Some(self.0.value().unwrap_or(false))
    }
    fn warm_up_bars(&self) -> usize {
        self.0.warm_up_bars()
    }
    fn unstable_bars(&self) -> usize {
        self.0.unstable_bars()
    }
    fn reset(&mut self) {
        self.0.reset()
    }
}

/// A boxed `I -> Arc<str>` indicator — the string twin of `Source<I>`. Now a
/// type alias over the shared [`TypedSource`] carrier; the dedicated
/// `DynStr<I>` trait + blanket impl it used to have collapsed into
/// [`runtime::Adapter`]'s coverage.
///
/// Backs the `GetStr` overlay-column reader and the `ValueStr` string
/// constant leaf, which compose into `str_eq` / `str_ne` signals.
pub(crate) type StrSource<I> = TypedSource<I, Arc<str>>;

/// Maps a multi-output value struct to its line names and their values (in the
/// same order). The names are available without an instance so warm-up rows can
/// still be placed in the right column.
pub(crate) trait MultiOutput {
    fn names() -> &'static [&'static str]
    where
        Self: Sized;
    fn values(&self) -> Vec<Real>;
}

impl MultiOutput for MacdValue {
    fn names() -> &'static [&'static str] {
        &["macd", "signal", "histogram"]
    }
    fn values(&self) -> Vec<Real> {
        vec![self.macd, self.signal, self.histogram]
    }
}
impl MultiOutput for BollingerValue {
    fn names() -> &'static [&'static str] {
        &["upper", "middle", "lower"]
    }
    fn values(&self) -> Vec<Real> {
        vec![self.upper, self.middle, self.lower]
    }
}
impl MultiOutput for KeltnerValue {
    fn names() -> &'static [&'static str] {
        &["upper", "middle", "lower"]
    }
    fn values(&self) -> Vec<Real> {
        vec![self.upper, self.middle, self.lower]
    }
}
impl MultiOutput for DonchianValue {
    fn names() -> &'static [&'static str] {
        &["upper", "middle", "lower"]
    }
    fn values(&self) -> Vec<Real> {
        vec![self.upper, self.middle, self.lower]
    }
}
impl MultiOutput for AdxValue {
    fn names() -> &'static [&'static str] {
        &["plus_di", "minus_di", "adx"]
    }
    fn values(&self) -> Vec<Real> {
        vec![self.plus_di, self.minus_di, self.adx]
    }
}
impl MultiOutput for DmiValue {
    fn names() -> &'static [&'static str] {
        &["plus_di", "minus_di"]
    }
    fn values(&self) -> Vec<Real> {
        vec![self.plus_di, self.minus_di]
    }
}
impl MultiOutput for AroonValue {
    fn names() -> &'static [&'static str] {
        &["up", "down", "oscillator"]
    }
    fn values(&self) -> Vec<Real> {
        vec![self.up, self.down, self.oscillator]
    }
}

/// Object-safe shim over any multi-output `I`-input indicator.
pub(crate) trait DynMulti<I>: Send + Sync {
    fn names(&self) -> &'static [&'static str];
    fn update(&mut self, input: I) -> Option<Vec<Real>>;
    fn value(&self) -> Option<Vec<Real>>;
    fn warm_up_bars(&self) -> usize;
    fn unstable_bars(&self) -> usize;
    fn reset(&mut self);
    /// Deep-clone the erased indicator into a fresh box. Used by [`PyMulti::shared`]
    /// to hand its concrete multi off to a shared-cell carrier without losing
    /// the type (the original `PyMulti` keeps its own independent copy).
    fn clone_box(&self) -> Box<dyn DynMulti<I>>;
}

impl<I, T> DynMulti<I> for T
where
    T: Indicator<Input = I> + Clone + Send + Sync + 'static,
    T::Output: MultiOutput,
{
    fn names(&self) -> &'static [&'static str] {
        <T::Output as MultiOutput>::names()
    }
    fn update(&mut self, input: I) -> Option<Vec<Real>> {
        Indicator::update(self, input).map(|o| o.values())
    }
    fn value(&self) -> Option<Vec<Real>> {
        Indicator::value(self).map(|o| o.values())
    }
    fn warm_up_bars(&self) -> usize {
        Indicator::warm_up_bars(self)
    }
    fn unstable_bars(&self) -> usize {
        Indicator::unstable_bars(self)
    }
    fn reset(&mut self) {
        Indicator::reset(self)
    }
    fn clone_box(&self) -> Box<dyn DynMulti<I>> {
        Box::new(self.clone())
    }
}

// ---------------------------------------------------------------------------
// Cross-timeframe composition: a resample-then-project chain matching the
// CLI's `!resample { every, inner }`. The library-level `Resample<S>` outputs
// `Candle`; Python has no candle-source carrier, so we compose it inline with
// a candle-rooted Real source and expose only the composed Real-output form.
// ---------------------------------------------------------------------------

/// `Resample<CurrentBar>` chained with a candle-consuming Real source: on the
/// base tick that completes an `every`-bar bucket, feed the aggregated candle
/// to `inner` (lifted to an `Atom`) and emit its output; on other ticks, emit
/// `None`.
#[derive(Clone)]
pub(crate) struct ResampleThen {
    pub(crate) resample: Resample<CurrentBar>,
    pub(crate) inner: Source<Atom>,
    pub(crate) value: Option<Real>,
}

impl ResampleThen {
    pub(crate) fn new(every: usize, inner: Source<Atom>) -> Self {
        Self {
            resample: Resample::new(CurrentBar::new(), every),
            inner,
            value: None,
        }
    }
}

impl Indicator for ResampleThen {
    type Input = Atom;
    type Output = Real;

    fn update(&mut self, atom: Atom) -> Option<Real> {
        self.value = match self.resample.update(atom) {
            Some(htf) => Indicator::update(&mut self.inner, htf.into()),
            None => None,
        };
        self.value
    }

    fn value(&self) -> Option<Real> {
        self.value
    }

    fn warm_up_bars(&self) -> usize {
        // Plain library-style composition: resample's own warm-up (`every`)
        // plus `inner.warm_up_bars() - 1` more HTF-emissions for the inner
        // to be ready (one emission coincides with resample's first). The
        // inner side is in HTF-sample units, not base-bar units — same
        // arithmetic as `Ema::new(Resample.close(), P)` in pure Rust.
        Indicator::warm_up_bars(&self.resample)
            .saturating_add(Indicator::warm_up_bars(&self.inner).saturating_sub(1))
    }

    fn unstable_bars(&self) -> usize {
        Indicator::unstable_bars(&self.resample)
            .saturating_add(Indicator::unstable_bars(&self.inner))
    }

    fn reset(&mut self) {
        Indicator::reset(&mut self.resample);
        Indicator::reset(&mut self.inner);
        self.value = None;
    }
}

/// A boxed multi-output indicator (terminal: not usable as a source).
pub(crate) struct MultiBox<I>(pub(crate) Box<dyn DynMulti<I>>);

impl<I> MultiBox<I> {
    pub(crate) fn new<T>(inner: T) -> Self
    where
        T: Indicator<Input = I> + Clone + Send + Sync + 'static,
        T::Output: MultiOutput,
    {
        MultiBox(Box::new(inner))
    }
}

// ---------------------------------------------------------------------------
// Shared multi-output source: the Python analogue of Rust's
// `fugazi::indicators::Shared` / `SharedComponent` pair, so per-line
// projections (`macd.line()`, `macd.signal()`, `bands.upper()`, …) built off
// one handle all advance the underlying multi at most once per bar — the
// classic-strategy optimisation, ported.
// ---------------------------------------------------------------------------

/// The cell every [`SharedProjector`] built from one shared handle borrows
/// into. `generation` ticks on every source `update`; each projector remembers
/// the last `generation` it observed as `local_gen`, so whichever projector is
/// called first each bar advances the multi (its `local_gen` equals the shared
/// counter) and the rest read the cached output.
pub(crate) struct SharedMultiCell<I> {
    pub(crate) multi: Box<dyn DynMulti<I>>,
    pub(crate) generation: u64,
    pub(crate) last_output: Option<Vec<Real>>,
    pub(crate) names: &'static [&'static str],
}

/// One projected component out of a shared multi. Implements the
/// `Real`-output [`Indicator`] shim so it can be boxed into a [`Source`] and
/// composed like any other indicator.
pub(crate) struct SharedProjector<I> {
    pub(crate) cell: Arc<Mutex<SharedMultiCell<I>>>,
    pub(crate) field_index: usize,
    pub(crate) local_gen: u64,
    pub(crate) last_value: Option<Real>,
}

impl<I> Clone for SharedProjector<I> {
    fn clone(&self) -> Self {
        Self {
            cell: Arc::clone(&self.cell),
            field_index: self.field_index,
            // Preserve the current sync state on clone: an operand cloned by
            // `crosses_above` etc. shouldn't spuriously re-trigger the advance.
            local_gen: self.local_gen,
            last_value: self.last_value,
        }
    }
}

impl<I: Clone + Send + Sync + 'static> Indicator for SharedProjector<I> {
    type Input = I;
    type Output = Real;

    fn update(&mut self, input: I) -> Option<Real> {
        let mut cell = self
            .cell
            .lock()
            .expect("shared multi-output cell mutex poisoned");
        if self.local_gen == cell.generation {
            // First projector-of-this-bar drives the underlying multi.
            let out = cell.multi.update(input);
            cell.last_output = out;
            cell.generation = cell.generation.wrapping_add(1);
        }
        self.local_gen = cell.generation;
        self.last_value = cell.last_output.as_ref().map(|v| v[self.field_index]);
        self.last_value
    }

    fn value(&self) -> Option<Real> {
        self.last_value
    }

    fn warm_up_bars(&self) -> usize {
        // Match `SharedComponent::warm_up_bars`: the projection still needs
        // one update to advance the source when the inner reports 0.
        self.cell
            .lock()
            .expect("shared multi-output cell mutex poisoned")
            .multi
            .warm_up_bars()
            .max(1)
    }

    fn unstable_bars(&self) -> usize {
        self.cell
            .lock()
            .expect("shared multi-output cell mutex poisoned")
            .multi
            .unstable_bars()
    }

    fn reset(&mut self) {
        let mut cell = self
            .cell
            .lock()
            .expect("shared multi-output cell mutex poisoned");
        cell.multi.reset();
        cell.last_output = None;
        // Leave `generation` alone; all sibling projectors will re-sync via
        // the usual `local_gen < generation → read cached` path.
        self.local_gen = cell.generation;
        self.last_value = None;
    }
}

/// A shared multi-output handle erased over the two input domains — the
/// Python analogue of `Shared<M>` in Rust. Component accessors return
/// [`PyIndicator`]s that borrow into the same underlying multi.
pub(crate) enum AnySharedMulti {
    Candle(Arc<Mutex<SharedMultiCell<Atom>>>),
    Real(Arc<Mutex<SharedMultiCell<Real>>>),
    Snapshot(Arc<Mutex<SharedMultiCell<Snapshot<Symbol>>>>),
}

impl AnySharedMulti {
    pub(crate) fn names(&self) -> &'static [&'static str] {
        match self {
            AnySharedMulti::Candle(c) => c.lock().expect("mutex poisoned").names,
            AnySharedMulti::Real(c) => c.lock().expect("mutex poisoned").names,
            AnySharedMulti::Snapshot(c) => c.lock().expect("mutex poisoned").names,
        }
    }

    pub(crate) fn field_index(&self, name: &str) -> PyResult<usize> {
        let names = self.names();
        names.iter().position(|n| *n == name).ok_or_else(|| {
            PyValueError::new_err(format!(
                "component `{name}` not found on this multi-output (available: {names:?})"
            ))
        })
    }

    pub(crate) fn project(&self, name: &str) -> PyResult<PyIndicator> {
        let idx = self.field_index(name)?;
        Ok(match self {
            AnySharedMulti::Candle(cell) => PyIndicator {
                src: AnySource::Candle(Source::new(SharedProjector::<Atom> {
                    cell: Arc::clone(cell),
                    field_index: idx,
                    local_gen: cell.lock().expect("mutex poisoned").generation,
                    last_value: None,
                })),
            },
            AnySharedMulti::Real(cell) => PyIndicator {
                src: AnySource::Real(Source::new(SharedProjector::<Real> {
                    cell: Arc::clone(cell),
                    field_index: idx,
                    local_gen: cell.lock().expect("mutex poisoned").generation,
                    last_value: None,
                })),
            },
            AnySharedMulti::Snapshot(cell) => PyIndicator {
                src: AnySource::Snapshot(Source::new(SharedProjector::<Snapshot<Symbol>> {
                    cell: Arc::clone(cell),
                    field_index: idx,
                    local_gen: cell.lock().expect("mutex poisoned").generation,
                    last_value: None,
                })),
            },
        })
    }
}

// ---------------------------------------------------------------------------
// Input domain: the runtime tag recovering the erased `Input` type
// ---------------------------------------------------------------------------

/// A scalar source erased to one of the three input domains, plus a fourth
/// domain-**neutral** case for a constant. A constant reads no input (it mirrors
/// Rust's `Value<I>`, generic over the input), so it carries no domain of its
/// own and instead adopts its partner's when composed — see [`pair`]. Used
/// entirely on its own it behaves as candle-rooted.
#[derive(Clone)]
pub(crate) enum AnySource {
    Candle(Source<Atom>),
    Real(Source<Real>),
    Snapshot(Source<Snapshot<Symbol>>),
    Const(Real),
}

impl AnySource {
    pub(crate) fn value(&self) -> Option<Real> {
        match self {
            AnySource::Candle(s) => Indicator::value(s),
            AnySource::Real(s) => Indicator::value(s),
            AnySource::Snapshot(s) => Indicator::value(s),
            AnySource::Const(c) => Some(*c),
        }
    }
    pub(crate) fn warm_up_bars(&self) -> usize {
        match self {
            AnySource::Candle(s) => Indicator::warm_up_bars(s),
            AnySource::Real(s) => Indicator::warm_up_bars(s),
            AnySource::Snapshot(s) => Indicator::warm_up_bars(s),
            AnySource::Const(_) => 0,
        }
    }
    pub(crate) fn unstable_bars(&self) -> usize {
        match self {
            AnySource::Candle(s) => Indicator::unstable_bars(s),
            AnySource::Real(s) => Indicator::unstable_bars(s),
            AnySource::Snapshot(s) => Indicator::unstable_bars(s),
            AnySource::Const(_) => 0,
        }
    }
    pub(crate) fn reset(&mut self) {
        match self {
            AnySource::Candle(s) => Indicator::reset(s),
            AnySource::Real(s) => Indicator::reset(s),
            AnySource::Snapshot(s) => Indicator::reset(s),
            AnySource::Const(_) => {}
        }
    }

    /// Dispatch a frame of samples through the domain the source lives in,
    /// producing one `Option<Real>` per bar. Extracts the correct input type
    /// from `data` (OHLCV frame / 1-D series / snapshot sequence) and folds it
    /// through `Indicator::update`. A `Const` source re-emits its value for
    /// every bar and reads the frame as candles (its neutral default domain).
    pub(crate) fn feed_rows(&mut self, data: &Bound<'_, PyAny>) -> PyResult<Vec<Option<Real>>> {
        Ok(match self {
            AnySource::Candle(s) => candles_from_frame(data)?
                .into_iter()
                .map(|c| Indicator::update(s, c.into()))
                .collect(),
            AnySource::Real(s) => reals_from_series(data)?
                .into_iter()
                .map(|x| Indicator::update(s, x))
                .collect(),
            AnySource::Snapshot(s) => snapshots_from_sequence(data)?
                .into_iter()
                .map(|snap| Indicator::update(s, snap))
                .collect(),
            AnySource::Const(c) => candles_from_frame(data)?.iter().map(|_| Some(*c)).collect(),
        })
    }
}

/// Two sources resolved to a common concrete domain, with any neutral constant
/// materialised to match its partner.
pub(crate) enum Pair {
    Candle(Source<Atom>, Source<Atom>),
    Real(Source<Real>, Source<Real>),
    Snapshot(Source<Snapshot<Symbol>>, Source<Snapshot<Symbol>>),
}

/// Resolve two sources to a shared domain so they can be combined. A neutral
/// constant adopts its partner's domain; a genuine candle-vs-value-vs-snapshot
/// clash is an error. Two constants default to the candle domain (either is
/// equivalent — they ignore input).
pub(crate) fn pair(lhs: AnySource, rhs: AnySource) -> PyResult<Pair> {
    fn rval(c: Real) -> Source<Real> {
        Source::new(Value::<Real>::new(c))
    }
    fn sval(c: Real) -> Source<Snapshot<Symbol>> {
        Source::new(Value::<Snapshot<Symbol>>::new(c))
    }
    match (lhs, rhs) {
        (AnySource::Candle(a), AnySource::Candle(b)) => Ok(Pair::Candle(a, b)),
        (AnySource::Real(a), AnySource::Real(b)) => Ok(Pair::Real(a, b)),
        (AnySource::Snapshot(a), AnySource::Snapshot(b)) => Ok(Pair::Snapshot(a, b)),
        (AnySource::Const(a), AnySource::Candle(b)) => Ok(Pair::Candle(const_to_candle_source(a), b)),
        (AnySource::Candle(a), AnySource::Const(b)) => Ok(Pair::Candle(a, const_to_candle_source(b))),
        (AnySource::Const(a), AnySource::Real(b)) => Ok(Pair::Real(rval(a), b)),
        (AnySource::Real(a), AnySource::Const(b)) => Ok(Pair::Real(a, rval(b))),
        (AnySource::Const(a), AnySource::Snapshot(b)) => Ok(Pair::Snapshot(sval(a), b)),
        (AnySource::Snapshot(a), AnySource::Const(b)) => Ok(Pair::Snapshot(a, sval(b))),
        (AnySource::Const(a), AnySource::Const(b)) => {
            Ok(Pair::Candle(const_to_candle_source(a), const_to_candle_source(b)))
        }
        (AnySource::Candle(_), AnySource::Real(_))
        | (AnySource::Real(_), AnySource::Candle(_))
        | (AnySource::Candle(_), AnySource::Snapshot(_))
        | (AnySource::Snapshot(_), AnySource::Candle(_))
        | (AnySource::Real(_), AnySource::Snapshot(_))
        | (AnySource::Snapshot(_), AnySource::Real(_)) => Err(domain_mismatch()),
    }
}

/// A boolean signal erased to one of the three input domains.
#[derive(Clone)]
pub(crate) enum AnySignal {
    Candle(SignalBox<Atom>),
    Real(SignalBox<Real>),
    Snapshot(SignalBox<Snapshot<Symbol>>),
}

impl AnySignal {
    pub(crate) fn is_true(&self) -> bool {
        match self {
            AnySignal::Candle(s) => BoolIndicatorExt::is_true(s),
            AnySignal::Real(s) => BoolIndicatorExt::is_true(s),
            AnySignal::Snapshot(s) => BoolIndicatorExt::is_true(s),
        }
    }
    pub(crate) fn warm_up_bars(&self) -> usize {
        match self {
            AnySignal::Candle(s) => Indicator::warm_up_bars(s),
            AnySignal::Real(s) => Indicator::warm_up_bars(s),
            AnySignal::Snapshot(s) => Indicator::warm_up_bars(s),
        }
    }
    pub(crate) fn unstable_bars(&self) -> usize {
        match self {
            AnySignal::Candle(s) => Indicator::unstable_bars(s),
            AnySignal::Real(s) => Indicator::unstable_bars(s),
            AnySignal::Snapshot(s) => Indicator::unstable_bars(s),
        }
    }
    pub(crate) fn reset(&mut self) {
        match self {
            AnySignal::Candle(s) => Indicator::reset(s),
            AnySignal::Real(s) => Indicator::reset(s),
            AnySignal::Snapshot(s) => Indicator::reset(s),
        }
    }

    /// Dispatch a frame of samples through the domain the signal lives in,
    /// producing one `bool` per bar. `SignalBox` flattens the warm-up `None`
    /// to `false` at the source, so an unwrap-or-`false` in the loop mirrors
    /// what the runtime already guarantees for individual updates.
    pub(crate) fn feed_rows(&mut self, data: &Bound<'_, PyAny>) -> PyResult<Vec<bool>> {
        Ok(match self {
            AnySignal::Candle(s) => candles_from_frame(data)?
                .into_iter()
                .map(|c| Indicator::update(s, c.into()).unwrap_or(false))
                .collect(),
            AnySignal::Real(s) => reals_from_series(data)?
                .into_iter()
                .map(|x| Indicator::update(s, x).unwrap_or(false))
                .collect(),
            AnySignal::Snapshot(s) => snapshots_from_sequence(data)?
                .into_iter()
                .map(|snap| Indicator::update(s, snap).unwrap_or(false))
                .collect(),
        })
    }
}

/// A string-valued source (`Arc<str>` output) erased to a candle-rooted box or
/// a domain-neutral constant. There is no value-rooted (`Real`-input) string
/// source in the library — every string overlay leaf reads an atom's overlay
/// side channel — so the `Real` variant present on [`AnySource`] has no twin
/// here.
#[derive(Clone)]
pub(crate) enum AnyStrSource {
    Candle(StrSource<Atom>),
    /// Snapshot-rooted — a `Str` overlay column read through an explicit
    /// atom source (`get_str(schema, key, source=pick("M"))`). The candle
    /// domain cannot express that: picking one asset out of a multi-symbol
    /// bar needs the whole snapshot as input.
    Snapshot(StrSource<Snapshot<Symbol>>),
    /// A constant string (the `ValueStr` leaf), domain-neutral. Adopts a
    /// candle-rooted partner when composed against one (see [`str_pair`]).
    Const(Arc<str>),
}

impl AnyStrSource {
    pub(crate) fn value(&self) -> Option<Arc<str>> {
        match self {
            AnyStrSource::Candle(s) => Indicator::value(s),
            AnyStrSource::Snapshot(s) => Indicator::value(s),
            AnyStrSource::Const(c) => Some(c.clone()),
        }
    }
    pub(crate) fn warm_up_bars(&self) -> usize {
        match self {
            AnyStrSource::Candle(s) => Indicator::warm_up_bars(s),
            AnyStrSource::Snapshot(s) => Indicator::warm_up_bars(s),
            AnyStrSource::Const(_) => 0,
        }
    }
    pub(crate) fn unstable_bars(&self) -> usize {
        match self {
            AnyStrSource::Candle(s) => Indicator::unstable_bars(s),
            AnyStrSource::Snapshot(s) => Indicator::unstable_bars(s),
            AnyStrSource::Const(_) => 0,
        }
    }
    pub(crate) fn reset(&mut self) {
        match self {
            AnyStrSource::Candle(s) => Indicator::reset(s),
            AnyStrSource::Snapshot(s) => Indicator::reset(s),
            AnyStrSource::Const(_) => {}
        }
    }
}

/// Two string sources resolved to the candle domain, with any neutral constant
/// materialised via [`ValueStr`]. Both sides end up as `StrSource<Atom>`.
pub(crate) enum StrPair {
    Candle(StrSource<Atom>, StrSource<Atom>),
    Snapshot(StrSource<Snapshot<Symbol>>, StrSource<Snapshot<Symbol>>),
}

pub(crate) fn str_pair(lhs: AnyStrSource, rhs: AnyStrSource) -> PyResult<StrPair> {
    use AnyStrSource as A;
    fn lift_candle(c: Arc<str>) -> StrSource<Atom> {
        StrSource::new(ValueStr::<Atom>::new(c))
    }
    fn lift_snapshot(c: Arc<str>) -> StrSource<Snapshot<Symbol>> {
        StrSource::new(ValueStr::<Snapshot<Symbol>>::new(c))
    }
    Ok(match (lhs, rhs) {
        (A::Candle(l), A::Candle(r)) => StrPair::Candle(l, r),
        (A::Snapshot(l), A::Snapshot(r)) => StrPair::Snapshot(l, r),
        // A neutral constant adopts its partner's domain, exactly as on the
        // Real side — so `str_eq(get_str(.., source=pick("M")), "bull")` works
        // without the caller having to say which domain the literal is in.
        (A::Candle(l), A::Const(c)) => StrPair::Candle(l, lift_candle(c)),
        (A::Const(c), A::Candle(r)) => StrPair::Candle(lift_candle(c), r),
        (A::Snapshot(l), A::Const(c)) => StrPair::Snapshot(l, lift_snapshot(c)),
        (A::Const(c), A::Snapshot(r)) => StrPair::Snapshot(lift_snapshot(c), r),
        (A::Const(l), A::Const(r)) => StrPair::Candle(lift_candle(l), lift_candle(r)),
        // A genuine clash: one side reads a single atom stream, the other picks
        // out of a multi-symbol snapshot.
        _ => return Err(domain_mismatch()),
    })
}

/// A multi-output indicator erased to one of the three input domains.
pub(crate) enum AnyMulti {
    Candle(MultiBox<Atom>),
    Real(MultiBox<Real>),
    Snapshot(MultiBox<Snapshot<Symbol>>),
}

impl AnyMulti {
    pub(crate) fn names(&self) -> &'static [&'static str] {
        match self {
            AnyMulti::Candle(m) => m.0.names(),
            AnyMulti::Real(m) => m.0.names(),
            AnyMulti::Snapshot(m) => m.0.names(),
        }
    }
    pub(crate) fn value(&self) -> Option<Vec<Real>> {
        match self {
            AnyMulti::Candle(m) => m.0.value(),
            AnyMulti::Real(m) => m.0.value(),
            AnyMulti::Snapshot(m) => m.0.value(),
        }
    }
    pub(crate) fn warm_up_bars(&self) -> usize {
        match self {
            AnyMulti::Candle(m) => m.0.warm_up_bars(),
            AnyMulti::Real(m) => m.0.warm_up_bars(),
            AnyMulti::Snapshot(m) => m.0.warm_up_bars(),
        }
    }
    pub(crate) fn unstable_bars(&self) -> usize {
        match self {
            AnyMulti::Candle(m) => m.0.unstable_bars(),
            AnyMulti::Real(m) => m.0.unstable_bars(),
            AnyMulti::Snapshot(m) => m.0.unstable_bars(),
        }
    }
    pub(crate) fn reset(&mut self) {
        match self {
            AnyMulti::Candle(m) => m.0.reset(),
            AnyMulti::Real(m) => m.0.reset(),
            AnyMulti::Snapshot(m) => m.0.reset(),
        }
    }

    /// Dispatch a frame of samples through the domain the multi lives in,
    /// producing one `Option<Vec<Real>>` per bar (`None` while warming up).
    pub(crate) fn feed_rows(&mut self, data: &Bound<'_, PyAny>) -> PyResult<Vec<Option<Vec<Real>>>> {
        Ok(match self {
            AnyMulti::Candle(m) => candles_from_frame(data)?
                .into_iter()
                .map(|c| m.0.update(c.into()))
                .collect(),
            AnyMulti::Real(m) => reals_from_series(data)?
                .into_iter()
                .map(|x| m.0.update(x))
                .collect(),
            AnyMulti::Snapshot(m) => snapshots_from_sequence(data)?
                .into_iter()
                .map(|snap| m.0.update(snap))
                .collect(),
        })
    }
}

pub(crate) fn domain_mismatch() -> PyErr {
    PyTypeError::new_err(
        "cannot combine indicators rooted in different domains — both operands \
         must be rooted in the same domain (candle / identity / snapshot)",
    )
}

/// Materialise a `Const` source's payload as a candle-rooted `Source<Atom>` so
/// the single-source dispatch macros can feed it into a source-slot builder.
/// The candle domain is neutral (matches the enum's own default), so a bare
/// constant used on its own reads as a per-bar constant candle stream.
pub(crate) fn const_to_candle_source(c: Real) -> Source<Atom> {
    Source::new(Value::<Atom>::new(c))
}
