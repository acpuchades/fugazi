//! Python bindings for the `fugazi` incremental technical-analysis library.
//!
//! The Rust library is built around "composition is construction": a
//! price-series indicator owns its input source and is generic over it, so an
//! "EMA of an SMA of the close" is just `Ema::new(Sma::new(Current::close(),
//! 10), 20)`. Those generics are monomorphised at compile time and cannot cross
//! the Python boundary directly, so this crate erases them behind boxed trait
//! objects ([`Source`], [`SignalBox`], [`MultiBox`]).
//!
//! Erasing a trait object throws away its associated `Input` type, so we keep
//! the one bit that matters — the input *domain* — as an explicit runtime tag.
//! An indicator is rooted either at a candle accessor ([`Current`]) — the
//! Python surface exposes those as `Candle`-consuming, but the library feeds
//! them `Atom`s internally (an `Atom` is a `Candle` plus an optional overlay
//! bundle; the Python side lifts each `Candle` to a bare `Atom` at the
//! boundary) — or at [`Identity`] (a raw value stream, `Input = Real`); the
//! [`AnySource`]/[`AnySignal`]/[`AnyMulti`] enums record which, and `feed()` /
//! `update()` dispatch on it. The two domains never mix within one chain (a
//! literal lifts to whichever side it is combined with).
//!
//! ```python
//! import fugazi as ta
//! ema_of_sma = ta.ema(ta.sma(ta.close(), 10), 20)   # candle-rooted
//! rsi_of_prices = ta.rsi(ta.identity(), 14)         # value-rooted
//! signal = ta.close().crosses_above(ta.ema(ta.close(), 20))
//! ```

use pyo3::exceptions::{PyTypeError, PyValueError};
use pyo3::prelude::*;
use pyo3::types::PyDict;

use std::sync::{Arc, Mutex};

use fugazi_core::Indicator;
use fugazi_core::indicators::compare::{EqOp, GeOp, GtOp, LeOp, LtOp, NeOp, StrEqOp, StrNeOp};
use fugazi_core::indicators::{
    Ad, Adx, AdxValue, Aroon, AroonValue, Atr, Bollinger, BollingerValue, Cci, Close, CurrentBar,
    Day, DayOfWeek, DayOfYear, Dmi, DmiValue, Donchian, DonchianValue, Ema, GetBool, GetReal,
    GetStr, High, Hma, Hour, Identity, IsWeekday, IsWeekend, Keltner, KeltnerValue, Latch, Low,
    Macd, MacdValue, Median, Mfi, Minute, Month, Obv, Open, Pick, Quarter, Resample, Rma, Rsi,
    Sar, Second, Sma, StdDev, Stochastic, TrueRange, Typical, UnixMillis, UnixSeconds, Value,
    ValueStr, Volume, Vwap, WeekOfYear, WilliamsR, Wma, Year,
};
use fugazi_core::indicators::{BoolIndicatorExt, Combine, DEFAULT_EPSILON, IndicatorExt};
use fugazi_core::sources::{Binance, CandleSource, Interval, SourceError, Timestamp, Yahoo};
use fugazi_core::strategy::{
    Ack, Order, OrderKind, PaperWallet, Units, Reference, Side, Size, Wallet, WalletError,
};
use fugazi_core::types::{
    Atom, Candle, Frequency, OverlayInfo, OverlayType, OverlayValue, Real, Schema, SchemaBuilder,
    Selector, Snapshot,
};
use fugazi_core::backtest::Fill;
use fugazi_core::metrics as core_metrics;
use fugazi_core::metrics::{DrawdownSegment, Trade};

// ---------------------------------------------------------------------------
// Type-erasing carriers
//
// The library is generic over both its input and its source; Python is not. We
// box every `I -> Real` indicator behind a local newtype so we can (a) satisfy
// the orphan rules and re-implement `Indicator` for the box, and (b) nest
// dynamically the way the Rust API nests statically. `I` is monomorphic per box
// (`Candle` or `Real`); the `Any*` enums below pick between the two at runtime.
// ---------------------------------------------------------------------------

/// Object-safe shim over an `I -> Real` indicator.
trait DynIndicator<I>: Send + Sync {
    fn update(&mut self, input: I) -> Option<Real>;
    fn value(&self) -> Option<Real>;
    fn warm_up_period(&self) -> usize;
    fn unstable_period(&self) -> usize;
    fn reset(&mut self);
    fn box_clone(&self) -> Box<dyn DynIndicator<I>>;
}

impl<I, T> DynIndicator<I> for T
where
    T: Indicator<Input = I, Output = Real> + Clone + Send + Sync + 'static,
{
    fn update(&mut self, input: I) -> Option<Real> {
        Indicator::update(self, input)
    }
    fn value(&self) -> Option<Real> {
        Indicator::value(self)
    }
    fn warm_up_period(&self) -> usize {
        Indicator::warm_up_period(self)
    }
    fn unstable_period(&self) -> usize {
        Indicator::unstable_period(self)
    }
    fn reset(&mut self) {
        Indicator::reset(self)
    }
    fn box_clone(&self) -> Box<dyn DynIndicator<I>> {
        Box::new(self.clone())
    }
}

/// A boxed `I -> Real` indicator. Implements [`Indicator`] itself, so it can be
/// fed straight back into any source-wrapping constructor.
struct Source<I>(Box<dyn DynIndicator<I>>);

impl<I> Source<I> {
    fn new<T>(inner: T) -> Self
    where
        T: Indicator<Input = I, Output = Real> + Clone + Send + Sync + 'static,
    {
        Source(Box::new(inner))
    }
}

impl<I> Clone for Source<I> {
    fn clone(&self) -> Self {
        Source(self.0.box_clone())
    }
}

impl<I> Indicator for Source<I> {
    type Input = I;
    type Output = Real;
    fn update(&mut self, input: I) -> Option<Real> {
        self.0.update(input)
    }
    fn value(&self) -> Option<Real> {
        self.0.value()
    }
    fn warm_up_period(&self) -> usize {
        self.0.warm_up_period()
    }
    fn unstable_period(&self) -> usize {
        self.0.unstable_period()
    }
    fn reset(&mut self) {
        self.0.reset()
    }
}

/// Object-safe shim over an `I`-input boolean indicator (a signal). Exposes the
/// warmed-up `bool` directly (`false` until ready), as the Python API expects.
trait DynSignal<I>: Send + Sync {
    fn update(&mut self, input: I) -> bool;
    fn is_true(&self) -> bool;
    fn warm_up_period(&self) -> usize;
    fn unstable_period(&self) -> usize;
    fn reset(&mut self);
    fn box_clone(&self) -> Box<dyn DynSignal<I>>;
}

impl<I, T> DynSignal<I> for T
where
    T: Indicator<Input = I, Output = bool> + Clone + Send + Sync + 'static,
{
    fn update(&mut self, input: I) -> bool {
        Indicator::update(self, input).unwrap_or(false)
    }
    fn is_true(&self) -> bool {
        self.value().unwrap_or(false)
    }
    fn warm_up_period(&self) -> usize {
        Indicator::warm_up_period(self)
    }
    fn unstable_period(&self) -> usize {
        Indicator::unstable_period(self)
    }
    fn reset(&mut self) {
        Indicator::reset(self)
    }
    fn box_clone(&self) -> Box<dyn DynSignal<I>> {
        Box::new(self.clone())
    }
}

/// A boxed `I`-input signal. Implements `Indicator<Output = bool>` so the
/// `BoolIndicatorExt` combinators nest.
struct SignalBox<I>(Box<dyn DynSignal<I>>);

impl<I> SignalBox<I> {
    fn new<T>(inner: T) -> Self
    where
        T: Indicator<Input = I, Output = bool> + Clone + Send + Sync + 'static,
    {
        SignalBox(Box::new(inner))
    }
}

impl<I> Clone for SignalBox<I> {
    fn clone(&self) -> Self {
        SignalBox(self.0.box_clone())
    }
}

impl<I> Indicator for SignalBox<I> {
    type Input = I;
    type Output = bool;
    fn update(&mut self, input: I) -> Option<bool> {
        Some(self.0.update(input))
    }
    fn value(&self) -> Option<bool> {
        Some(self.0.is_true())
    }
    fn warm_up_period(&self) -> usize {
        self.0.warm_up_period()
    }
    fn unstable_period(&self) -> usize {
        self.0.unstable_period()
    }
    fn reset(&mut self) {
        self.0.reset()
    }
}

/// Object-safe shim over an `I -> Arc<str>` indicator — the string twin of
/// [`DynIndicator`]. Backs the `GetStr` overlay-column reader and the
/// `ValueStr` string constant leaf, which compose into `str_eq` / `str_ne`
/// signals.
trait DynStr<I>: Send + Sync {
    fn update(&mut self, input: I) -> Option<Arc<str>>;
    fn value(&self) -> Option<Arc<str>>;
    fn warm_up_period(&self) -> usize;
    fn unstable_period(&self) -> usize;
    fn reset(&mut self);
    fn box_clone(&self) -> Box<dyn DynStr<I>>;
}

impl<I, T> DynStr<I> for T
where
    T: Indicator<Input = I, Output = Arc<str>> + Clone + Send + Sync + 'static,
{
    fn update(&mut self, input: I) -> Option<Arc<str>> {
        Indicator::update(self, input)
    }
    fn value(&self) -> Option<Arc<str>> {
        Indicator::value(self)
    }
    fn warm_up_period(&self) -> usize {
        Indicator::warm_up_period(self)
    }
    fn unstable_period(&self) -> usize {
        Indicator::unstable_period(self)
    }
    fn reset(&mut self) {
        Indicator::reset(self)
    }
    fn box_clone(&self) -> Box<dyn DynStr<I>> {
        Box::new(self.clone())
    }
}

/// A boxed `I -> Arc<str>` indicator. Implements [`Indicator`] itself, so it
/// can be fed back into any string-consuming constructor (e.g. `StrEq` over
/// two `Arc<str>` sources).
struct StrSource<I>(Box<dyn DynStr<I>>);

impl<I> StrSource<I> {
    fn new<T>(inner: T) -> Self
    where
        T: Indicator<Input = I, Output = Arc<str>> + Clone + Send + Sync + 'static,
    {
        StrSource(Box::new(inner))
    }
}

impl<I> Clone for StrSource<I> {
    fn clone(&self) -> Self {
        StrSource(self.0.box_clone())
    }
}

impl<I> Indicator for StrSource<I> {
    type Input = I;
    type Output = Arc<str>;
    fn update(&mut self, input: I) -> Option<Arc<str>> {
        self.0.update(input)
    }
    fn value(&self) -> Option<Arc<str>> {
        self.0.value()
    }
    fn warm_up_period(&self) -> usize {
        self.0.warm_up_period()
    }
    fn unstable_period(&self) -> usize {
        self.0.unstable_period()
    }
    fn reset(&mut self) {
        self.0.reset()
    }
}

/// Maps a multi-output value struct to its line names and their values (in the
/// same order). The names are available without an instance so warm-up rows can
/// still be placed in the right column.
trait MultiOutput {
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
trait DynMulti<I>: Send + Sync {
    fn names(&self) -> &'static [&'static str];
    fn update(&mut self, input: I) -> Option<Vec<Real>>;
    fn value(&self) -> Option<Vec<Real>>;
    fn warm_up_period(&self) -> usize;
    fn unstable_period(&self) -> usize;
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
    fn warm_up_period(&self) -> usize {
        Indicator::warm_up_period(self)
    }
    fn unstable_period(&self) -> usize {
        Indicator::unstable_period(self)
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
struct ResampleThen {
    resample: Resample<CurrentBar>,
    inner: Source<Atom>,
    value: Option<Real>,
}

impl ResampleThen {
    fn new(every: usize, inner: Source<Atom>) -> Self {
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

    fn warm_up_period(&self) -> usize {
        // Plain library-style composition: resample's own warm-up (`every`)
        // plus `inner.warm_up_period() - 1` more HTF-emissions for the inner
        // to be ready (one emission coincides with resample's first). The
        // inner side is in HTF-sample units, not base-bar units — same
        // arithmetic as `Ema::new(Resample.close(), P)` in pure Rust.
        Indicator::warm_up_period(&self.resample)
            .saturating_add(Indicator::warm_up_period(&self.inner).saturating_sub(1))
    }

    fn unstable_period(&self) -> usize {
        Indicator::unstable_period(&self.resample)
            .saturating_add(Indicator::unstable_period(&self.inner))
    }

    fn reset(&mut self) {
        Indicator::reset(&mut self.resample);
        Indicator::reset(&mut self.inner);
        self.value = None;
    }
}

/// A boxed multi-output indicator (terminal: not usable as a source).
struct MultiBox<I>(Box<dyn DynMulti<I>>);

impl<I> MultiBox<I> {
    fn new<T>(inner: T) -> Self
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
struct SharedMultiCell<I> {
    multi: Box<dyn DynMulti<I>>,
    generation: u64,
    last_output: Option<Vec<Real>>,
    names: &'static [&'static str],
}

/// One projected component out of a shared multi. Implements the
/// `Real`-output [`Indicator`] shim so it can be boxed into a [`Source`] and
/// composed like any other indicator.
struct SharedProjector<I> {
    cell: Arc<Mutex<SharedMultiCell<I>>>,
    field_index: usize,
    local_gen: u64,
    last_value: Option<Real>,
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

    fn warm_up_period(&self) -> usize {
        // Match `SharedComponent::warm_up_period`: the projection still needs
        // one update to advance the source when the inner reports 0.
        self.cell
            .lock()
            .expect("shared multi-output cell mutex poisoned")
            .multi
            .warm_up_period()
            .max(1)
    }

    fn unstable_period(&self) -> usize {
        self.cell
            .lock()
            .expect("shared multi-output cell mutex poisoned")
            .multi
            .unstable_period()
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
enum AnySharedMulti {
    Candle(Arc<Mutex<SharedMultiCell<Atom>>>),
    Real(Arc<Mutex<SharedMultiCell<Real>>>),
    Snapshot(Arc<Mutex<SharedMultiCell<Snapshot<Selector>>>>),
}

impl AnySharedMulti {
    fn names(&self) -> &'static [&'static str] {
        match self {
            AnySharedMulti::Candle(c) => c.lock().expect("mutex poisoned").names,
            AnySharedMulti::Real(c) => c.lock().expect("mutex poisoned").names,
            AnySharedMulti::Snapshot(c) => c.lock().expect("mutex poisoned").names,
        }
    }

    fn field_index(&self, name: &str) -> PyResult<usize> {
        let names = self.names();
        names.iter().position(|n| *n == name).ok_or_else(|| {
            PyValueError::new_err(format!(
                "component `{name}` not found on this multi-output (available: {names:?})"
            ))
        })
    }

    fn project(&self, name: &str) -> PyResult<PyIndicator> {
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
                src: AnySource::Snapshot(Source::new(SharedProjector::<Snapshot<Selector>> {
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
enum AnySource {
    Candle(Source<Atom>),
    Real(Source<Real>),
    Snapshot(Source<Snapshot<Selector>>),
    Const(Real),
}

impl AnySource {
    fn value(&self) -> Option<Real> {
        match self {
            AnySource::Candle(s) => Indicator::value(s),
            AnySource::Real(s) => Indicator::value(s),
            AnySource::Snapshot(s) => Indicator::value(s),
            AnySource::Const(c) => Some(*c),
        }
    }
    fn warm_up_period(&self) -> usize {
        match self {
            AnySource::Candle(s) => Indicator::warm_up_period(s),
            AnySource::Real(s) => Indicator::warm_up_period(s),
            AnySource::Snapshot(s) => Indicator::warm_up_period(s),
            AnySource::Const(_) => 0,
        }
    }
    fn unstable_period(&self) -> usize {
        match self {
            AnySource::Candle(s) => Indicator::unstable_period(s),
            AnySource::Real(s) => Indicator::unstable_period(s),
            AnySource::Snapshot(s) => Indicator::unstable_period(s),
            AnySource::Const(_) => 0,
        }
    }
    fn reset(&mut self) {
        match self {
            AnySource::Candle(s) => Indicator::reset(s),
            AnySource::Real(s) => Indicator::reset(s),
            AnySource::Snapshot(s) => Indicator::reset(s),
            AnySource::Const(_) => {}
        }
    }
}

/// Two sources resolved to a common concrete domain, with any neutral constant
/// materialised to match its partner.
enum Pair {
    Candle(Source<Atom>, Source<Atom>),
    Real(Source<Real>, Source<Real>),
    Snapshot(Source<Snapshot<Selector>>, Source<Snapshot<Selector>>),
}

/// Resolve two sources to a shared domain so they can be combined. A neutral
/// constant adopts its partner's domain; a genuine candle-vs-value-vs-snapshot
/// clash is an error. Two constants default to the candle domain (either is
/// equivalent — they ignore input).
fn pair(lhs: AnySource, rhs: AnySource) -> PyResult<Pair> {
    fn cval(c: Real) -> Source<Atom> {
        Source::new(Value::<Atom>::new(c))
    }
    fn rval(c: Real) -> Source<Real> {
        Source::new(Value::<Real>::new(c))
    }
    fn sval(c: Real) -> Source<Snapshot<Selector>> {
        Source::new(Value::<Snapshot<Selector>>::new(c))
    }
    match (lhs, rhs) {
        (AnySource::Candle(a), AnySource::Candle(b)) => Ok(Pair::Candle(a, b)),
        (AnySource::Real(a), AnySource::Real(b)) => Ok(Pair::Real(a, b)),
        (AnySource::Snapshot(a), AnySource::Snapshot(b)) => Ok(Pair::Snapshot(a, b)),
        (AnySource::Const(a), AnySource::Candle(b)) => Ok(Pair::Candle(cval(a), b)),
        (AnySource::Candle(a), AnySource::Const(b)) => Ok(Pair::Candle(a, cval(b))),
        (AnySource::Const(a), AnySource::Real(b)) => Ok(Pair::Real(rval(a), b)),
        (AnySource::Real(a), AnySource::Const(b)) => Ok(Pair::Real(a, rval(b))),
        (AnySource::Const(a), AnySource::Snapshot(b)) => Ok(Pair::Snapshot(sval(a), b)),
        (AnySource::Snapshot(a), AnySource::Const(b)) => Ok(Pair::Snapshot(a, sval(b))),
        (AnySource::Const(a), AnySource::Const(b)) => Ok(Pair::Candle(cval(a), cval(b))),
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
enum AnySignal {
    Candle(SignalBox<Atom>),
    Real(SignalBox<Real>),
    Snapshot(SignalBox<Snapshot<Selector>>),
}

impl AnySignal {
    fn is_true(&self) -> bool {
        match self {
            AnySignal::Candle(s) => BoolIndicatorExt::is_true(s),
            AnySignal::Real(s) => BoolIndicatorExt::is_true(s),
            AnySignal::Snapshot(s) => BoolIndicatorExt::is_true(s),
        }
    }
    fn warm_up_period(&self) -> usize {
        match self {
            AnySignal::Candle(s) => Indicator::warm_up_period(s),
            AnySignal::Real(s) => Indicator::warm_up_period(s),
            AnySignal::Snapshot(s) => Indicator::warm_up_period(s),
        }
    }
    fn unstable_period(&self) -> usize {
        match self {
            AnySignal::Candle(s) => Indicator::unstable_period(s),
            AnySignal::Real(s) => Indicator::unstable_period(s),
            AnySignal::Snapshot(s) => Indicator::unstable_period(s),
        }
    }
    fn reset(&mut self) {
        match self {
            AnySignal::Candle(s) => Indicator::reset(s),
            AnySignal::Real(s) => Indicator::reset(s),
            AnySignal::Snapshot(s) => Indicator::reset(s),
        }
    }
}

/// A string-valued source (`Arc<str>` output) erased to a candle-rooted box or
/// a domain-neutral constant. There is no value-rooted (`Real`-input) string
/// source in the library — every string overlay leaf reads an atom's overlay
/// side channel — so the `Real` variant present on [`AnySource`] has no twin
/// here.
#[derive(Clone)]
enum AnyStrSource {
    Candle(StrSource<Atom>),
    /// A constant string (the `ValueStr` leaf), domain-neutral. Adopts a
    /// candle-rooted partner when composed against one (see [`str_pair`]).
    Const(Arc<str>),
}

impl AnyStrSource {
    fn value(&self) -> Option<Arc<str>> {
        match self {
            AnyStrSource::Candle(s) => Indicator::value(s),
            AnyStrSource::Const(c) => Some(c.clone()),
        }
    }
    fn warm_up_period(&self) -> usize {
        match self {
            AnyStrSource::Candle(s) => Indicator::warm_up_period(s),
            AnyStrSource::Const(_) => 0,
        }
    }
    fn unstable_period(&self) -> usize {
        match self {
            AnyStrSource::Candle(s) => Indicator::unstable_period(s),
            AnyStrSource::Const(_) => 0,
        }
    }
    fn reset(&mut self) {
        match self {
            AnyStrSource::Candle(s) => Indicator::reset(s),
            AnyStrSource::Const(_) => {}
        }
    }
}

/// Two string sources resolved to the candle domain, with any neutral constant
/// materialised via [`ValueStr`]. Both sides end up as `StrSource<Atom>`.
fn str_pair(
    lhs: AnyStrSource,
    rhs: AnyStrSource,
) -> (StrSource<Atom>, StrSource<Atom>) {
    fn lift(c: Arc<str>) -> StrSource<Atom> {
        StrSource::new(ValueStr::<Atom>::new(c))
    }
    let l = match lhs {
        AnyStrSource::Candle(s) => s,
        AnyStrSource::Const(c) => lift(c),
    };
    let r = match rhs {
        AnyStrSource::Candle(s) => s,
        AnyStrSource::Const(c) => lift(c),
    };
    (l, r)
}

/// A multi-output indicator erased to one of the three input domains.
enum AnyMulti {
    Candle(MultiBox<Atom>),
    Real(MultiBox<Real>),
    Snapshot(MultiBox<Snapshot<Selector>>),
}

impl AnyMulti {
    fn names(&self) -> &'static [&'static str] {
        match self {
            AnyMulti::Candle(m) => m.0.names(),
            AnyMulti::Real(m) => m.0.names(),
            AnyMulti::Snapshot(m) => m.0.names(),
        }
    }
    fn value(&self) -> Option<Vec<Real>> {
        match self {
            AnyMulti::Candle(m) => m.0.value(),
            AnyMulti::Real(m) => m.0.value(),
            AnyMulti::Snapshot(m) => m.0.value(),
        }
    }
    fn warm_up_period(&self) -> usize {
        match self {
            AnyMulti::Candle(m) => m.0.warm_up_period(),
            AnyMulti::Real(m) => m.0.warm_up_period(),
            AnyMulti::Snapshot(m) => m.0.warm_up_period(),
        }
    }
    fn unstable_period(&self) -> usize {
        match self {
            AnyMulti::Candle(m) => m.0.unstable_period(),
            AnyMulti::Real(m) => m.0.unstable_period(),
            AnyMulti::Snapshot(m) => m.0.unstable_period(),
        }
    }
    fn reset(&mut self) {
        match self {
            AnyMulti::Candle(m) => m.0.reset(),
            AnyMulti::Real(m) => m.0.reset(),
            AnyMulti::Snapshot(m) => m.0.reset(),
        }
    }
}

fn domain_mismatch() -> PyErr {
    PyTypeError::new_err(
        "cannot combine indicators rooted in different domains — both operands \
         must be rooted in the same domain (candle / identity / snapshot)",
    )
}

/// Apply a source-wrapping constructor to a source, preserving its domain. A
/// neutral constant defaults to the candle domain.
macro_rules! map_source {
    ($src:expr, |$s:ident| $build:expr) => {
        match $src {
            AnySource::Candle($s) => AnySource::Candle(Source::new($build)),
            AnySource::Real($s) => AnySource::Real(Source::new($build)),
            AnySource::Snapshot($s) => AnySource::Snapshot(Source::new($build)),
            AnySource::Const(c) => {
                let $s = Source::<Atom>::new(Value::<Atom>::new(c));
                AnySource::Candle(Source::new($build))
            }
        }
    };
}

/// Combine two sources into a new source; resolves a constant against its
/// partner, errors on a genuine domain clash.
macro_rules! combine_sources {
    ($lhs:expr, $rhs:expr, |$l:ident, $r:ident| $build:expr) => {
        pair($lhs, $rhs).map(|p| match p {
            Pair::Candle($l, $r) => AnySource::Candle(Source::new($build)),
            Pair::Real($l, $r) => AnySource::Real(Source::new($build)),
            Pair::Snapshot($l, $r) => AnySource::Snapshot(Source::new($build)),
        })
    };
}

/// Turn one source into a signal, preserving its domain. A neutral constant
/// defaults to the candle domain.
macro_rules! source_to_signal {
    ($src:expr, |$s:ident| $build:expr) => {
        match $src {
            AnySource::Candle($s) => AnySignal::Candle(SignalBox::new($build)),
            AnySource::Real($s) => AnySignal::Real(SignalBox::new($build)),
            AnySource::Snapshot($s) => AnySignal::Snapshot(SignalBox::new($build)),
            AnySource::Const(c) => {
                let $s = Source::<Atom>::new(Value::<Atom>::new(c));
                AnySignal::Candle(SignalBox::new($build))
            }
        }
    };
}

/// Turn two sources into a signal; resolves a constant against its partner,
/// errors on a genuine domain clash.
macro_rules! sources_to_signal {
    ($lhs:expr, $rhs:expr, |$l:ident, $r:ident| $build:expr) => {
        pair($lhs, $rhs).map(|p| match p {
            Pair::Candle($l, $r) => AnySignal::Candle(SignalBox::new($build)),
            Pair::Real($l, $r) => AnySignal::Real(SignalBox::new($build)),
            Pair::Snapshot($l, $r) => AnySignal::Snapshot(SignalBox::new($build)),
        })
    };
}

/// Transform one signal, preserving its domain.
macro_rules! map_signal {
    ($sig:expr, |$s:ident| $build:expr) => {
        match $sig {
            AnySignal::Candle($s) => AnySignal::Candle(SignalBox::new($build)),
            AnySignal::Real($s) => AnySignal::Real(SignalBox::new($build)),
            AnySignal::Snapshot($s) => AnySignal::Snapshot(SignalBox::new($build)),
        }
    };
}

/// Combine two signals; errors if their domains differ.
macro_rules! combine_signals {
    ($lhs:expr, $rhs:expr, |$l:ident, $r:ident| $build:expr) => {
        match ($lhs, $rhs) {
            (AnySignal::Candle($l), AnySignal::Candle($r)) => {
                Ok(AnySignal::Candle(SignalBox::new($build)))
            }
            (AnySignal::Real($l), AnySignal::Real($r)) => {
                Ok(AnySignal::Real(SignalBox::new($build)))
            }
            (AnySignal::Snapshot($l), AnySignal::Snapshot($r)) => {
                Ok(AnySignal::Snapshot(SignalBox::new($build)))
            }
            _ => Err(domain_mismatch()),
        }
    };
}

/// Wrap one source in a multi-output constructor, preserving its domain. A
/// neutral constant defaults to the candle domain.
macro_rules! map_multi {
    ($src:expr, |$s:ident| $build:expr) => {
        match $src {
            AnySource::Candle($s) => AnyMulti::Candle(MultiBox::new($build)),
            AnySource::Real($s) => AnyMulti::Real(MultiBox::new($build)),
            AnySource::Snapshot($s) => AnyMulti::Snapshot(MultiBox::new($build)),
            AnySource::Const(c) => {
                let $s = Source::<Atom>::new(Value::<Atom>::new(c));
                AnyMulti::Candle(MultiBox::new($build))
            }
        }
    };
}

/// Wrap two sources in a multi-output constructor; resolves a constant against
/// its partner, errors on a genuine domain clash.
macro_rules! combine_multi {
    ($lhs:expr, $rhs:expr, |$l:ident, $r:ident| $build:expr) => {
        pair($lhs, $rhs).map(|p| match p {
            Pair::Candle($l, $r) => AnyMulti::Candle(MultiBox::new($build)),
            Pair::Real($l, $r) => AnyMulti::Real(MultiBox::new($build)),
            Pair::Snapshot($l, $r) => AnyMulti::Snapshot(MultiBox::new($build)),
        })
    };
}

// ---------------------------------------------------------------------------
// Python classes
// ---------------------------------------------------------------------------

/// A single OHLCV bar.
#[pyclass(name = "Candle", frozen, skip_from_py_object)]
#[derive(Clone, Copy)]
struct PyCandle {
    inner: Candle,
}

#[pymethods]
impl PyCandle {
    #[new]
    fn new(open: f64, high: f64, low: f64, close: f64, volume: f64) -> Self {
        PyCandle {
            inner: Candle::new(open, high, low, close, volume),
        }
    }

    #[getter]
    fn open(&self) -> f64 {
        self.inner.open
    }
    #[getter]
    fn high(&self) -> f64 {
        self.inner.high
    }
    #[getter]
    fn low(&self) -> f64 {
        self.inner.low
    }
    #[getter]
    fn close(&self) -> f64 {
        self.inner.close
    }
    #[getter]
    fn volume(&self) -> f64 {
        self.inner.volume
    }

    /// Typical price, `(high + low + close) / 3`.
    fn typical(&self) -> f64 {
        self.inner.typical()
    }

    /// Median price, `(high + low) / 2`.
    fn median(&self) -> f64 {
        self.inner.median()
    }

    fn __repr__(&self) -> String {
        let c = &self.inner;
        format!(
            "Candle(open={}, high={}, low={}, close={}, volume={})",
            c.open, c.high, c.low, c.close, c.volume
        )
    }
}

// ---------------------------------------------------------------------------
// Overlay-side types: Schema (name→index), SchemaBuilder, OverlayInfo, Atom
// ---------------------------------------------------------------------------

/// An immutable name→(index, type) registry that binds an [`OverlayInfo`]'s
/// values array to the columns a `get()` indicator references. Built with a
/// `SchemaBuilder` and frozen once — every column carries its declared type
/// (`"real"` / `"bool"` / `"str"`), which `get()` reads to pick the right typed
/// leaf.
#[pyclass(name = "Schema", frozen, skip_from_py_object)]
#[derive(Clone)]
struct PySchema {
    inner: Arc<Schema>,
}

#[pymethods]
impl PySchema {
    fn __len__(&self) -> usize {
        self.inner.len()
    }

    fn __contains__(&self, key: &str) -> bool {
        self.inner.contains(key)
    }

    /// The zero-based column index of `key`, or `None` if unregistered.
    fn index_of(&self, key: &str) -> Option<usize> {
        self.inner.index_of(key)
    }

    /// The declared type of the column at `index` — one of `"real"`,
    /// `"bool"`, `"str"` — or `None` if the index is out of range.
    fn type_of(&self, index: usize) -> Option<&'static str> {
        self.inner.type_of(index).map(overlay_type_name)
    }

    /// The declared type of column `key` — one of `"real"`, `"bool"`, `"str"`
    /// — or `None` if `key` is not registered.
    fn type_of_key(&self, key: &str) -> Option<&'static str> {
        self.inner.type_of_key(key).map(overlay_type_name)
    }

    /// All registered column names, in insertion order.
    fn keys(&self) -> Vec<String> {
        self.inner.keys().map(str::to_string).collect()
    }

    fn __repr__(&self) -> String {
        let cols: Vec<String> = self
            .inner
            .keys()
            .map(|k| {
                let ty = self.inner.type_of_key(k).map(overlay_type_name).unwrap_or("?");
                format!("{k}:{ty}")
            })
            .collect();
        format!("Schema(columns={cols:?})")
    }
}

fn overlay_type_name(ty: OverlayType) -> &'static str {
    match ty {
        OverlayType::Real => "real",
        OverlayType::Bool => "bool",
        OverlayType::Str => "str",
    }
}

/// Mutable builder for a [`Schema`]. Add typed columns with `add_real()` /
/// `add_bool()` / `add_str()` (each idempotent per key), then freeze into an
/// immutable [`Schema`] with `finish()`. `add()` remains for the pre-typed
/// callers as an alias for `add_real()`.
#[pyclass(name = "SchemaBuilder")]
struct PySchemaBuilder {
    inner: Option<SchemaBuilder>,
}

#[pymethods]
impl PySchemaBuilder {
    #[new]
    fn new() -> Self {
        Self {
            inner: Some(SchemaBuilder::default()),
        }
    }

    /// Register `key` as a `Real` column. Returns the assigned column index; a
    /// repeated key returns the previously-assigned index without adding a slot.
    /// Re-registering with a different type raises `ValueError`.
    fn add_real(&mut self, key: &str) -> PyResult<usize> {
        self.with_builder(|b| b.add_real(key.to_string()))
    }

    /// Register `key` as a `Bool` column. A `Bool` overlay reads as a signal
    /// directly — no `str_eq true` needed.
    fn add_bool(&mut self, key: &str) -> PyResult<usize> {
        self.with_builder(|b| b.add_bool(key.to_string()))
    }

    /// Register `key` as a `Str` column. Consumed via `get_str(...).eq("...")`
    /// (or the underlying `str_eq(...)`).
    fn add_str(&mut self, key: &str) -> PyResult<usize> {
        self.with_builder(|b| b.add_str(key.to_string()))
    }

    /// Back-compat alias for [`add_real`](Self::add_real). Prefer the typed
    /// method in new code.
    fn add(&mut self, key: &str) -> PyResult<usize> {
        self.add_real(key)
    }

    fn __len__(&self) -> PyResult<usize> {
        self.inner
            .as_ref()
            .ok_or_else(|| {
                PyValueError::new_err("SchemaBuilder has already been finished")
            })
            .map(|b| b.len())
    }

    /// Freeze into an immutable [`Schema`]. The builder is consumed — further
    /// calls raise `ValueError`.
    fn finish(&mut self) -> PyResult<PySchema> {
        let builder = self.inner.take().ok_or_else(|| {
            PyValueError::new_err("SchemaBuilder has already been finished")
        })?;
        Ok(PySchema {
            inner: builder.finish(),
        })
    }

    fn __repr__(&self) -> String {
        match &self.inner {
            Some(b) => format!("SchemaBuilder(len={})", b.len()),
            None => "SchemaBuilder(finished)".to_string(),
        }
    }
}

impl PySchemaBuilder {
    /// Common wrapper around a `SchemaBuilder` call: unwraps the option,
    /// runs the closure, catches the library's `assert!` panic on a
    /// type-mismatch re-registration and turns it into a Python `ValueError`.
    fn with_builder<F>(&mut self, f: F) -> PyResult<usize>
    where
        F: FnOnce(&mut SchemaBuilder) -> usize + std::panic::UnwindSafe,
    {
        let builder = self.inner.as_mut().ok_or_else(|| {
            PyValueError::new_err("SchemaBuilder has already been finished")
        })?;
        // The library asserts on a type-mismatch re-registration; catch it so
        // Python sees a normal ValueError instead of a hard abort.
        match std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| f(builder))) {
            Ok(idx) => Ok(idx),
            Err(payload) => {
                let msg = panic_message(&payload);
                Err(PyValueError::new_err(msg))
            }
        }
    }
}

/// Best-effort recovery of a panic payload's message. `String` and `&str`
/// payloads are the two common shapes for `assert!(cond, "…")` panics.
fn panic_message(payload: &Box<dyn std::any::Any + Send + 'static>) -> String {
    if let Some(s) = payload.downcast_ref::<String>() {
        return s.clone();
    }
    if let Some(s) = payload.downcast_ref::<&'static str>() {
        return (*s).to_string();
    }
    "unknown error".to_string()
}

/// Per-atom overlay values, bound to a shared [`Schema`]. Construct as
/// `OverlayInfo(schema, values)` — `values` is a list whose length matches
/// `len(schema)`, with each entry a Python `float` / `bool` / `str` matching
/// the column's declared type. Reading a value with `get()` returns the
/// native Python type; the typed accessors (`get_real` / `get_bool` /
/// `get_str`) return `None` on a type mismatch.
///
/// The internal `Rc<[OverlayValue]>` (per-atom, non-atomic refcount) makes
/// this class `unsendable` — it's confined to the Python thread that created
/// it. This is fine under the GIL and keeps overlay clones cheap in the hot
/// per-bar loop.
#[pyclass(name = "OverlayInfo", frozen, unsendable, skip_from_py_object)]
#[derive(Clone)]
struct PyOverlayInfo {
    inner: OverlayInfo,
}

#[pymethods]
impl PyOverlayInfo {
    #[new]
    fn new(schema: &PySchema, values: Vec<Py<PyAny>>, py: Python<'_>) -> PyResult<Self> {
        if values.len() != schema.inner.len() {
            return Err(PyValueError::new_err(format!(
                "values length ({}) must match schema length ({})",
                values.len(),
                schema.inner.len(),
            )));
        }
        let mut typed: Vec<OverlayValue> = Vec::with_capacity(values.len());
        for (i, v) in values.into_iter().enumerate() {
            let declared = schema.inner.type_of(i).expect("schema index in range");
            let bound = v.bind(py);
            typed.push(python_to_overlay_value(bound, declared, i)?);
        }
        Ok(Self {
            inner: OverlayInfo::new(schema.inner.clone(), typed),
        })
    }

    fn __len__(&self) -> usize {
        self.inner.values().len()
    }

    /// Read the value at a resolved column index as its native Python type
    /// (`float` for `Real`, `bool` for `Bool`, `str` for `Str`), or `None` if
    /// the index is out of bounds.
    fn get(&self, py: Python<'_>, index: usize) -> Option<Py<PyAny>> {
        self.inner.get(index).and_then(|v| overlay_to_python(py, v).ok())
    }

    /// Read the value by column name (`None` if the key isn't registered).
    fn get_by_key(&self, py: Python<'_>, key: &str) -> Option<Py<PyAny>> {
        self.inner
            .schema()
            .index_of(key)
            .and_then(|i| self.get(py, i))
    }

    /// Typed reader: `Real` value at `index`, or `None` on out-of-bounds or a
    /// type mismatch (the schema declares a different type at this index).
    fn get_real(&self, index: usize) -> Option<Real> {
        self.inner.get_real(index)
    }

    /// Typed reader: `Bool` value at `index`, or `None` on out-of-bounds or a
    /// type mismatch.
    fn get_bool(&self, index: usize) -> Option<bool> {
        self.inner.get_bool(index)
    }

    /// Typed reader: `Str` value at `index`, or `None` on out-of-bounds or a
    /// type mismatch.
    fn get_str(&self, index: usize) -> Option<String> {
        self.inner.get_str(index).map(|s| s.to_string())
    }

    fn __repr__(&self) -> String {
        format!("OverlayInfo(values={:?})", self.inner.values())
    }
}

/// Convert one Python value into an [`OverlayValue`] of the declared type. On
/// mismatch, raises a `ValueError` mentioning the slot index + expected type
/// so a caller can locate the bad value in a large list.
fn python_to_overlay_value(
    bound: &Bound<'_, PyAny>,
    declared: OverlayType,
    slot: usize,
) -> PyResult<OverlayValue> {
    match declared {
        // Bool has to be tested first: `bool` extracts as `f64` too under
        // pyo3's numeric coercion, so a naive Real match would swallow it.
        OverlayType::Bool => bound
            .extract::<bool>()
            .map(OverlayValue::Bool)
            .map_err(|_| slot_type_error(slot, declared, bound)),
        OverlayType::Real => {
            // A stray `True`/`False` in a Real slot — reject rather than
            // silently coerce to 1.0 / 0.0.
            if bound.extract::<bool>().is_ok() && is_python_bool(bound) {
                return Err(slot_type_error(slot, declared, bound));
            }
            bound
                .extract::<Real>()
                .map(OverlayValue::Real)
                .map_err(|_| slot_type_error(slot, declared, bound))
        }
        OverlayType::Str => bound
            .extract::<String>()
            .map(|s| OverlayValue::Str(Arc::from(s.as_str())))
            .map_err(|_| slot_type_error(slot, declared, bound)),
    }
}

/// Whether `bound` is a Python `bool` (distinguishes `True`/`False` from
/// numeric `1`/`0`). PyO3 coerces `bool` to `f64`, so distinguishing them
/// requires an explicit type check on the Python side.
fn is_python_bool(bound: &Bound<'_, PyAny>) -> bool {
    bound
        .get_type()
        .name()
        .ok()
        .map(|n| n == "bool")
        .unwrap_or(false)
}

fn slot_type_error(slot: usize, declared: OverlayType, bound: &Bound<'_, PyAny>) -> PyErr {
    let got = bound
        .get_type()
        .name()
        .map(|n| n.to_string())
        .unwrap_or_else(|_| "<unknown>".to_string());
    PyValueError::new_err(format!(
        "overlay value at index {slot}: schema declared {declared}, got Python {got:?}",
    ))
}

/// Convert an [`OverlayValue`] to its native Python object counterpart.
fn overlay_to_python(py: Python<'_>, v: &OverlayValue) -> PyResult<Py<PyAny>> {
    use pyo3::IntoPyObject;
    Ok(match v {
        OverlayValue::Real(x) => x.into_pyobject(py)?.into_any().unbind(),
        OverlayValue::Bool(b) => b.into_pyobject(py)?.to_owned().into_any().unbind(),
        OverlayValue::Str(s) => s.as_ref().into_pyobject(py)?.into_any().unbind(),
    })
}

/// A single bar's full input to the indicator chain: an OHLCV [`Candle`] and,
/// optionally, per-bar overlay values keyed by a shared [`Schema`]. Every
/// candle-rooted indicator's `update()` accepts either a bare `Candle` (lifted
/// to an atom with no overlays) or an `Atom` — pass an `Atom` when the chain
/// includes a `get()` indicator that needs overlay context.
///
/// `unsendable` because the inner [`OverlayInfo`] holds an `Rc<[Real]>` for
/// per-atom overlay values. The Python GIL confines it to one thread anyway.
#[pyclass(name = "Atom", frozen, unsendable, skip_from_py_object)]
#[derive(Clone)]
struct PyAtom {
    inner: Atom,
}

#[pymethods]
impl PyAtom {
    /// `time` is the bar-open UTC millisecond stamp (an `int`) — passed through
    /// to any calendar indicator (`year()`, `month()`, `day_of_week()`, …) in
    /// the chain. `None` on synthetic bars leaves those calendar reads at
    /// `None` (matching a not-yet-warm result).
    #[new]
    #[pyo3(signature = (candle, overlays = None, time = None))]
    fn new(
        candle: &PyCandle,
        overlays: Option<&PyOverlayInfo>,
        time: Option<i64>,
    ) -> Self {
        let time = time.map(Timestamp);
        let atom = match (overlays, time) {
            (Some(ov), Some(t)) => {
                Atom::with_overlays_and_time(candle.inner, ov.inner.clone(), t)
            }
            (Some(ov), None) => Atom::with_overlays(candle.inner, ov.inner.clone()),
            (None, Some(t)) => Atom::with_time(candle.inner, t),
            (None, None) => Atom::new(candle.inner),
        };
        Self { inner: atom }
    }

    #[getter]
    fn candle(&self) -> PyCandle {
        PyCandle {
            inner: self.inner.candle,
        }
    }

    #[getter]
    fn overlays(&self) -> Option<PyOverlayInfo> {
        self.inner
            .overlays
            .as_ref()
            .cloned()
            .map(|ov| PyOverlayInfo { inner: ov })
    }

    /// The bar-open time as a UTC millisecond epoch (an `int`), or `None`
    /// if the atom was constructed without one.
    #[getter]
    fn time(&self) -> Option<i64> {
        self.inner.time.map(|t| t.0)
    }

    fn __repr__(&self) -> String {
        let candle = &self.inner.candle;
        let time = self.inner.time.map(|t| t.0);
        match (&self.inner.overlays, time) {
            (Some(ov), Some(t)) => format!(
                "Atom(candle={:?}, overlays={:?}, time={})",
                candle,
                ov.values(),
                t,
            ),
            (Some(ov), None) => format!(
                "Atom(candle={:?}, overlays={:?})",
                candle,
                ov.values(),
            ),
            (None, Some(t)) => format!("Atom(candle={:?}, time={})", candle, t),
            (None, None) => format!("Atom(candle={:?})", candle),
        }
    }

    // --- comparison / hashing by bar-open time --------------------------------
    // Mirrors the Rust `impl PartialEq / Eq / Ord for Atom` — identity is the
    // bar-open Timestamp; OHLCV numbers and overlays are payload, not identity.
    fn __eq__(&self, other: &Bound<'_, PyAny>) -> PyResult<bool> {
        match other.cast::<PyAtom>() {
            Ok(o) => Ok(self.inner == o.borrow().inner),
            Err(_) => Ok(false),
        }
    }

    fn __ne__(&self, other: &Bound<'_, PyAny>) -> PyResult<bool> {
        Ok(!self.__eq__(other)?)
    }

    fn __hash__(&self) -> u64 {
        // `Timestamp: Hash` already; `None` hashes to a distinct sentinel.
        use std::collections::hash_map::DefaultHasher;
        use std::hash::{Hash, Hasher};
        let mut h = DefaultHasher::new();
        self.inner.time.map(|t| t.0).hash(&mut h);
        h.finish()
    }

    fn __lt__(&self, other: PyRef<'_, PyAtom>) -> bool {
        self.inner < other.inner
    }
    fn __le__(&self, other: PyRef<'_, PyAtom>) -> bool {
        self.inner <= other.inner
    }
    fn __gt__(&self, other: PyRef<'_, PyAtom>) -> bool {
        self.inner > other.inner
    }
    fn __ge__(&self, other: PyRef<'_, PyAtom>) -> bool {
        self.inner >= other.inner
    }
}

// ---------------------------------------------------------------------------
// Frequency, Selector, Snapshot + Pick — the cross-asset input frame
// ---------------------------------------------------------------------------

/// A bar cadence: `1m`, `4h`, `1d`, `1w`, `1M`.
///
/// Parsed from the canonical `N<unit>` token where `m` is minute, `h` hour,
/// `d` day, `w` week, `M` month (uppercase, so lowercase `m` stays unambiguously
/// "minute"). Round-trips through `str()` and `repr()`. Hashable and total-order
/// sortable by duration (so `Frequency("120m") > Frequency("1h")` behaves the
/// way you expect regardless of variant tag).
#[pyclass(name = "Frequency", frozen, skip_from_py_object)]
#[derive(Clone, Copy)]
struct PyFrequency {
    inner: Frequency,
}

#[pymethods]
impl PyFrequency {
    /// Parse an `N<unit>` token (`"1m"`, `"5m"`, `"1h"`, `"4h"`, `"1d"`,
    /// `"1w"`, `"1M"`, …). Raises `ValueError` on any other shape.
    #[new]
    fn new(token: &str) -> PyResult<Self> {
        use std::str::FromStr;
        let inner = Frequency::from_str(token).map_err(PyValueError::new_err)?;
        Ok(Self { inner })
    }

    /// The canonical token — the round-trip of the constructor.
    fn __str__(&self) -> String {
        self.inner.as_token()
    }

    fn __repr__(&self) -> String {
        format!("Frequency({:?})", self.inner.as_token())
    }

    fn __hash__(&self) -> u64 {
        use std::collections::hash_map::DefaultHasher;
        use std::hash::{Hash, Hasher};
        let mut h = DefaultHasher::new();
        self.inner.hash(&mut h);
        h.finish()
    }

    fn __eq__(&self, other: &Bound<'_, PyAny>) -> bool {
        match other.cast::<PyFrequency>() {
            Ok(o) => self.inner == o.borrow().inner,
            Err(_) => false,
        }
    }

    fn __ne__(&self, other: &Bound<'_, PyAny>) -> bool {
        !self.__eq__(other)
    }

    fn __lt__(&self, other: PyRef<'_, PyFrequency>) -> bool {
        self.inner < other.inner
    }
    fn __le__(&self, other: PyRef<'_, PyFrequency>) -> bool {
        self.inner <= other.inner
    }
    fn __gt__(&self, other: PyRef<'_, PyFrequency>) -> bool {
        self.inner > other.inner
    }
    fn __ge__(&self, other: PyRef<'_, PyFrequency>) -> bool {
        self.inner >= other.inner
    }
}

/// A **selector**: a partial key naming *which* asset in a [`Snapshot`] a
/// [`Pick`](fugazi.pick) should read. Symbol and frequency are both optional;
/// an empty selector is legal and stands for the [`Pick`] no-query,
/// single-entry-unpack path.
///
/// Coerced automatically from a Python `str` (symbol only), from a
/// `Frequency` (freq only), from a `(str, Frequency | str | None)` tuple, and
/// from a `dict` — so `ta.Snapshot({"BTC": ...})` and
/// `ta.Snapshot({ta.Selector(symbol="BTC", freq="1h"): ...})` both work.
#[pyclass(name = "Selector", frozen, skip_from_py_object)]
#[derive(Clone)]
struct PySelector {
    inner: Selector,
}

#[pymethods]
impl PySelector {
    /// Build a selector. Both fields are optional and default to `None`; an
    /// empty selector is legal and drives the [`Pick`] single-entry-unpack path.
    /// `freq` accepts a `Frequency` instance or a token string (`"1h"`, `"1d"`).
    #[new]
    #[pyo3(signature = (symbol = None, freq = None))]
    fn new(symbol: Option<String>, freq: Option<&Bound<'_, PyAny>>) -> PyResult<Self> {
        let freq = freq.map(coerce_frequency).transpose()?;
        Ok(Self {
            inner: Selector::new(symbol, freq),
        })
    }

    #[getter]
    fn symbol(&self) -> Option<String> {
        self.inner.symbol.clone()
    }

    #[getter]
    fn freq(&self) -> Option<PyFrequency> {
        self.inner.freq.map(|inner| PyFrequency { inner })
    }

    /// True when both fields are `None` — the `Pick` no-query case.
    fn is_empty(&self) -> bool {
        self.inner.is_empty()
    }

    /// Match this selector as a **query** against `storage`: each `None`
    /// field on the query is a wildcard; a `Some` field must equal storage.
    fn matches(&self, storage: PyRef<'_, PySelector>) -> bool {
        self.inner.matches(&storage.inner)
    }

    fn __hash__(&self) -> u64 {
        use std::collections::hash_map::DefaultHasher;
        use std::hash::{Hash, Hasher};
        let mut h = DefaultHasher::new();
        self.inner.hash(&mut h);
        h.finish()
    }

    fn __eq__(&self, other: &Bound<'_, PyAny>) -> bool {
        match coerce_selector(other) {
            Ok(sel) => self.inner == sel,
            Err(_) => false,
        }
    }

    fn __ne__(&self, other: &Bound<'_, PyAny>) -> bool {
        !self.__eq__(other)
    }

    fn __repr__(&self) -> String {
        match (&self.inner.symbol, &self.inner.freq) {
            (Some(s), Some(f)) => format!("Selector(symbol={:?}, freq={:?})", s, f.as_token()),
            (Some(s), None) => format!("Selector(symbol={s:?})"),
            (None, Some(f)) => format!("Selector(freq={:?})", f.as_token()),
            (None, None) => "Selector()".to_string(),
        }
    }
}

/// Extract a [`Frequency`] from a Python `PyFrequency` or a token `str`.
fn coerce_frequency(obj: &Bound<'_, PyAny>) -> PyResult<Frequency> {
    if let Ok(f) = obj.cast::<PyFrequency>() {
        return Ok(f.borrow().inner);
    }
    if let Ok(s) = obj.extract::<String>() {
        use std::str::FromStr;
        return Frequency::from_str(&s).map_err(PyValueError::new_err);
    }
    Err(PyTypeError::new_err(
        "expected a Frequency or a str token (e.g. \"1h\", \"1d\")",
    ))
}

/// Coerce a Python key into a [`Selector`]. Accepts:
///
/// - `PySelector` directly.
/// - `str` — parsed as a symbol (`Selector::by_symbol`).
/// - `PyFrequency` — parsed as a frequency (`Selector::by_freq`).
/// - `(str, Frequency | str | None)` tuple — a `(symbol, freq)` pair.
fn coerce_selector(obj: &Bound<'_, PyAny>) -> PyResult<Selector> {
    if let Ok(sel) = obj.cast::<PySelector>() {
        return Ok(sel.borrow().inner.clone());
    }
    if let Ok(f) = obj.cast::<PyFrequency>() {
        return Ok(Selector::by_freq(f.borrow().inner));
    }
    if let Ok(s) = obj.extract::<String>() {
        return Ok(Selector::by_symbol(s));
    }
    if let Ok((sym, freq)) = obj.extract::<(String, Option<Py<PyAny>>)>() {
        let freq = match freq {
            None => None,
            Some(f) => Some(coerce_frequency(f.bind(obj.py()))?),
        };
        return Ok(Selector::new(Some(sym), freq));
    }
    Err(PyTypeError::new_err(
        "Snapshot keys must be a Selector, a str (symbol), a Frequency, or a (symbol, freq) tuple",
    ))
}

/// A per-bar snapshot of several assets: keyed collection of [`PyAtom`]s.
///
/// The multi-asset input frame — a strategy or cross-asset indicator's
/// `update` is fed one `Snapshot` per bar and the [`Pick`] leaf projects one
/// asset out by [`Selector`]. Dict-like: `snap[selector]` reads,
/// `snap[selector] = atom` writes, `selector in snap` tests membership,
/// `len(snap)` counts assets.
#[pyclass(name = "Snapshot", unsendable, skip_from_py_object)]
#[derive(Clone)]
struct PySnapshot {
    inner: Snapshot<Selector>,
}

#[pymethods]
impl PySnapshot {
    #[new]
    #[pyo3(signature = (mapping = None))]
    fn new(mapping: Option<&Bound<'_, PyAny>>) -> PyResult<Self> {
        let inner = match mapping {
            None => Snapshot::<Selector>::new(),
            Some(m) => extract_snapshot(m)?,
        };
        Ok(Self { inner })
    }

    /// Read the atom for `key`; raises `KeyError` if absent (dict semantics).
    fn __getitem__(&self, key: &Bound<'_, PyAny>) -> PyResult<PyAtom> {
        let sel = coerce_selector(key)?;
        self.inner
            .get(&sel)
            .cloned()
            .map(|inner| PyAtom { inner })
            .ok_or_else(|| pyo3::exceptions::PyKeyError::new_err(format!("{sel:?}")))
    }

    /// Insert or replace the atom for `key`.
    fn __setitem__(&mut self, key: &Bound<'_, PyAny>, atom: PyRef<'_, PyAtom>) -> PyResult<()> {
        let sel = coerce_selector(key)?;
        self.inner.insert(sel, atom.inner.clone());
        Ok(())
    }

    fn __contains__(&self, key: &Bound<'_, PyAny>) -> PyResult<bool> {
        let sel = coerce_selector(key)?;
        Ok(self.inner.contains_key(&sel))
    }

    fn __len__(&self) -> usize {
        self.inner.len()
    }

    /// Non-raising variant of `snap[key]` — returns `None` on a miss.
    fn get(&self, key: &Bound<'_, PyAny>) -> PyResult<Option<PyAtom>> {
        let sel = coerce_selector(key)?;
        Ok(self.inner.get(&sel).cloned().map(|inner| PyAtom { inner }))
    }

    /// Insert or replace; returns the previous atom if any.
    fn insert(
        &mut self,
        key: &Bound<'_, PyAny>,
        atom: PyRef<'_, PyAtom>,
    ) -> PyResult<Option<PyAtom>> {
        let sel = coerce_selector(key)?;
        Ok(self
            .inner
            .insert(sel, atom.inner.clone())
            .map(|inner| PyAtom { inner }))
    }

    /// The list of [`Selector`] keys present in this snapshot (arbitrary order).
    fn keys(&self) -> Vec<PySelector> {
        self.inner
            .keys()
            .cloned()
            .map(|inner| PySelector { inner })
            .collect()
    }

    /// True if this snapshot carries no assets.
    fn is_empty(&self) -> bool {
        self.inner.is_empty()
    }

    /// Structural lookup: returns the first atom whose stored [`Selector`]
    /// matches `query` (`None` fields on the query are wildcards). If a
    /// query could match more than one key, iteration order is arbitrary;
    /// disambiguate by supplying both `symbol` and `freq`.
    fn find(&self, query: &Bound<'_, PyAny>) -> PyResult<Option<PyAtom>> {
        let sel = coerce_selector(query)?;
        Ok(self.inner.find(&sel).cloned().map(|inner| PyAtom { inner }))
    }

    /// The sole atom in a single-entry snapshot, if there is exactly one.
    /// Returns `None` on an empty snapshot; raises `RuntimeError` (translated
    /// from a Rust panic) on 2+ entries — the same loud failure the no-query
    /// `pick()` uses.
    fn sole_atom(&self) -> Option<PyAtom> {
        self.inner.sole_atom().cloned().map(|inner| PyAtom { inner })
    }

    fn __repr__(&self) -> String {
        let keys: Vec<String> = self
            .inner
            .keys()
            .map(|k| PySelector { inner: k.clone() }.__repr__())
            .collect();
        format!("Snapshot(keys=[{}])", keys.join(", "))
    }
}

// ---------------------------------------------------------------------------
// Atom-emitting source — the box behind the `pick()` leaf and the `.of(source)`
// method on every atom-input leaf constructor (close, high, year, ...).
// ---------------------------------------------------------------------------

/// Object-safe shim over an `I -> Atom` indicator.
trait DynAtomIndicator<I>: Send + Sync {
    fn update(&mut self, input: I) -> Option<Atom>;
    fn value(&self) -> Option<Atom>;
    fn warm_up_period(&self) -> usize;
    fn unstable_period(&self) -> usize;
    fn reset(&mut self);
    fn box_clone(&self) -> Box<dyn DynAtomIndicator<I>>;
}

impl<I, T> DynAtomIndicator<I> for T
where
    T: Indicator<Input = I, Output = Atom> + Clone + Send + Sync + 'static,
{
    fn update(&mut self, input: I) -> Option<Atom> {
        Indicator::update(self, input)
    }
    fn value(&self) -> Option<Atom> {
        Indicator::value(self)
    }
    fn warm_up_period(&self) -> usize {
        Indicator::warm_up_period(self)
    }
    fn unstable_period(&self) -> usize {
        Indicator::unstable_period(self)
    }
    fn reset(&mut self) {
        Indicator::reset(self)
    }
    fn box_clone(&self) -> Box<dyn DynAtomIndicator<I>> {
        Box::new(self.clone())
    }
}

/// A boxed `I -> Atom` indicator. Implements [`Indicator`] itself so it can be
/// fed back into any `Output = Atom` source constructor.
struct AtomBox<I>(Box<dyn DynAtomIndicator<I>>);

impl<I> AtomBox<I> {
    fn new<T>(inner: T) -> Self
    where
        T: Indicator<Input = I, Output = Atom> + Clone + Send + Sync + 'static,
    {
        AtomBox(Box::new(inner))
    }
}

impl<I> Clone for AtomBox<I> {
    fn clone(&self) -> Self {
        AtomBox(self.0.box_clone())
    }
}

impl<I> Indicator for AtomBox<I> {
    type Input = I;
    type Output = Atom;
    fn update(&mut self, input: I) -> Option<Atom> {
        self.0.update(input)
    }
    fn value(&self) -> Option<Atom> {
        self.0.value()
    }
    fn warm_up_period(&self) -> usize {
        self.0.warm_up_period()
    }
    fn unstable_period(&self) -> usize {
        self.0.unstable_period()
    }
    fn reset(&mut self) {
        self.0.reset()
    }
}

/// An atom-emitting source erased to one of the two input domains it can be
/// rooted in on the Python side: `Atom` (the identity passthrough) or
/// `Snapshot<Selector>` (a `Pick`). Feeds the optional `source=` argument every
/// atom-input leaf pyfunction accepts (`close(source=...)`, `year(source=...)`, …).
#[derive(Clone)]
enum AnyAtomSource {
    /// The trivial atom passthrough — `Identity<Atom>`, so the caller can build
    /// an atom-input leaf explicitly rooted on the atom stream itself. Kept in
    /// the enum for surface completeness; not currently produced by any leaf
    /// pyfunction (the raw-atom shape is already what a zero-arg `close()`
    /// returns).
    #[allow(dead_code)]
    Atom(AtomBox<Atom>),
    Snapshot(AtomBox<Snapshot<Selector>>),
}

/// A source that emits `Atom`s per bar — the intermediate between a raw
/// `Snapshot` and a scalar leaf like `close()`.
///
/// Produced by `pick(key)` (rooted on a `Snapshot`) and used as the optional
/// `source=` argument of every atom-input leaf constructor:
///
/// ```python
/// btc_close = ta.close(ta.pick("BTC"))
/// spread = ta.close(ta.pick("BTC")) - ta.close(ta.pick("ETH"))
/// ```
#[pyclass(name = "AtomSource", skip_from_py_object)]
#[derive(Clone)]
struct PyAtomSource {
    inner: AnyAtomSource,
}

#[pymethods]
impl PyAtomSource {
    /// The most recent atom, without advancing state.
    fn value(&self) -> Option<PyAtom> {
        let out = match &self.inner {
            AnyAtomSource::Atom(s) => Indicator::value(s),
            AnyAtomSource::Snapshot(s) => Indicator::value(s),
        };
        out.map(|inner| PyAtom { inner })
    }

    /// Feed the next sample. Pass an `Atom` for an atom-rooted source (the
    /// trivial identity), a `Snapshot` for a `pick()`-rooted one.
    fn update(&mut self, sample: &Bound<'_, PyAny>) -> PyResult<Option<PyAtom>> {
        let out = match &mut self.inner {
            AnyAtomSource::Atom(s) => Indicator::update(s, extract_atom(sample)?),
            AnyAtomSource::Snapshot(s) => Indicator::update(s, extract_snapshot(sample)?),
        };
        Ok(out.map(|inner| PyAtom { inner }))
    }

    fn warm_up_period(&self) -> usize {
        match &self.inner {
            AnyAtomSource::Atom(s) => Indicator::warm_up_period(s),
            AnyAtomSource::Snapshot(s) => Indicator::warm_up_period(s),
        }
    }

    fn unstable_period(&self) -> usize {
        match &self.inner {
            AnyAtomSource::Atom(s) => Indicator::unstable_period(s),
            AnyAtomSource::Snapshot(s) => Indicator::unstable_period(s),
        }
    }

    fn stable_period(&self) -> usize {
        self.warm_up_period() + self.unstable_period()
    }

    fn reset(&mut self) {
        match &mut self.inner {
            AnyAtomSource::Atom(s) => Indicator::reset(s),
            AnyAtomSource::Snapshot(s) => Indicator::reset(s),
        }
    }

    fn __repr__(&self) -> String {
        match &self.inner {
            AnyAtomSource::Atom(_) => "AtomSource(root=atom)".to_string(),
            AnyAtomSource::Snapshot(_) => "AtomSource(root=snapshot)".to_string(),
        }
    }
}

/// A scalar (`-> float`) indicator. Compose it with the fluent operator methods;
/// build named indicators with the module-level constructors.
///
/// An indicator is rooted either at a candle accessor (`close()`, `atr()`, …),
/// in which case it consumes `Candle`s, or at `identity()`, in which case it
/// consumes a raw value stream of `float`s.
#[pyclass(name = "Indicator")]
struct PyIndicator {
    src: AnySource,
}

impl PyIndicator {
    fn wrap(src: AnySource) -> Self {
        PyIndicator { src }
    }
}

#[pymethods]
impl PyIndicator {
    /// Feed the next sample; returns the current value, or `None` while warming
    /// up. Pass a `Candle` for a candle-rooted indicator, a `float` for an
    /// identity-rooted one.
    fn update(&mut self, sample: &Bound<'_, PyAny>) -> PyResult<Option<f64>> {
        match &mut self.src {
            AnySource::Candle(s) => Ok(Indicator::update(s, extract_atom(sample)?)),
            AnySource::Real(s) => Ok(Indicator::update(s, extract_real(sample)?)),
            AnySource::Snapshot(s) => Ok(Indicator::update(s, extract_snapshot(sample)?)),
            // A bare constant defaults to candle-rooted; it ignores the bar.
            AnySource::Const(c) => {
                extract_atom(sample)?;
                Ok(Some(*c))
            }
        }
    }

    /// Compute the indicator over a whole series at once, returning one output
    /// per bar (`None` while warming up).
    ///
    /// A candle-rooted indicator takes a pandas/polars `DataFrame` (or a `dict`)
    /// with `open`/`high`/`low`/`close`/`volume` columns — only those present
    /// are used, and `close` is required. An identity-rooted indicator takes a
    /// plain 1-D sequence (`list`, NumPy array, or pandas/polars `Series`).
    /// A snapshot-rooted indicator (built through `pick()`) takes a Python
    /// sequence of `Snapshot`s (or dicts of the same shape).
    ///
    /// The output mirrors the input: a pandas/polars `Series` (index preserved
    /// for pandas) when given that library's frame/series, otherwise a NumPy
    /// `ndarray`. Warm-up bars are `NaN`. The data is fed through the current
    /// state — call `reset()` first for a clean pass.
    fn feed(&mut self, py: Python<'_>, data: &Bound<'_, PyAny>) -> PyResult<Py<PyAny>> {
        let kind = OutputKind::detect(data)?;
        let values: Vec<Option<f64>> = match &mut self.src {
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
            // A bare constant defaults to candle-rooted; emit it for every bar.
            AnySource::Const(c) => candles_from_frame(data)?.iter().map(|_| Some(*c)).collect(),
        };
        build_floats(py, &kind, values)
    }

    /// The most recent value, without advancing state.
    fn value(&self) -> Option<f64> {
        self.src.value()
    }

    /// Whether enough samples have been seen to produce a value.
    fn is_ready(&self) -> bool {
        self.src.value().is_some()
    }

    /// The number of samples needed before the first value can appear.
    fn warm_up_period(&self) -> usize {
        self.src.warm_up_period()
    }

    /// Extra samples after warm-up before a recursive indicator (EMA, RSI, …)
    /// has effectively converged; `0` for windowed indicators.
    fn unstable_period(&self) -> usize {
        self.src.unstable_period()
    }

    /// `warm_up_period() + unstable_period()`: how much history to feed before
    /// trusting the output.
    fn stable_period(&self) -> usize {
        self.src.warm_up_period() + self.src.unstable_period()
    }

    /// Reset all internal state to freshly-constructed.
    fn reset(&mut self) {
        self.src.reset()
    }

    // --- comparisons -> Signal -------------------------------------------------
    #[pyo3(signature = (other, epsilon = None))]
    fn gt(&self, other: &Bound<'_, PyAny>, epsilon: Option<f64>) -> PyResult<PySignal> {
        let rhs = coerce_operand(other)?;
        let eps = epsilon.unwrap_or(DEFAULT_EPSILON);
        Ok(PySignal::wrap(sources_to_signal!(
            self.src.clone(),
            rhs,
            |l, r| Combine::<_, _, GtOp>::with_epsilon(l, r, eps)
        )?))
    }

    #[pyo3(signature = (other, epsilon = None))]
    fn lt(&self, other: &Bound<'_, PyAny>, epsilon: Option<f64>) -> PyResult<PySignal> {
        let rhs = coerce_operand(other)?;
        let eps = epsilon.unwrap_or(DEFAULT_EPSILON);
        Ok(PySignal::wrap(sources_to_signal!(
            self.src.clone(),
            rhs,
            |l, r| Combine::<_, _, LtOp>::with_epsilon(l, r, eps)
        )?))
    }

    #[pyo3(signature = (other, epsilon = None))]
    fn ge(&self, other: &Bound<'_, PyAny>, epsilon: Option<f64>) -> PyResult<PySignal> {
        let rhs = coerce_operand(other)?;
        let eps = epsilon.unwrap_or(DEFAULT_EPSILON);
        Ok(PySignal::wrap(sources_to_signal!(
            self.src.clone(),
            rhs,
            |l, r| Combine::<_, _, GeOp>::with_epsilon(l, r, eps)
        )?))
    }

    #[pyo3(signature = (other, epsilon = None))]
    fn le(&self, other: &Bound<'_, PyAny>, epsilon: Option<f64>) -> PyResult<PySignal> {
        let rhs = coerce_operand(other)?;
        let eps = epsilon.unwrap_or(DEFAULT_EPSILON);
        Ok(PySignal::wrap(sources_to_signal!(
            self.src.clone(),
            rhs,
            |l, r| Combine::<_, _, LeOp>::with_epsilon(l, r, eps)
        )?))
    }

    #[pyo3(signature = (other, epsilon = None))]
    fn eq(&self, other: &Bound<'_, PyAny>, epsilon: Option<f64>) -> PyResult<PySignal> {
        let rhs = coerce_operand(other)?;
        let eps = epsilon.unwrap_or(DEFAULT_EPSILON);
        Ok(PySignal::wrap(sources_to_signal!(
            self.src.clone(),
            rhs,
            |l, r| Combine::<_, _, EqOp>::with_epsilon(l, r, eps)
        )?))
    }

    #[pyo3(signature = (other, epsilon = None))]
    fn ne(&self, other: &Bound<'_, PyAny>, epsilon: Option<f64>) -> PyResult<PySignal> {
        let rhs = coerce_operand(other)?;
        let eps = epsilon.unwrap_or(DEFAULT_EPSILON);
        Ok(PySignal::wrap(sources_to_signal!(
            self.src.clone(),
            rhs,
            |l, r| Combine::<_, _, NeOp>::with_epsilon(l, r, eps)
        )?))
    }

    /// `self > level` for a constant level.
    fn above(&self, level: f64) -> PySignal {
        PySignal::wrap(source_to_signal!(self.src.clone(), |s| s.above(level)))
    }

    /// `self < level` for a constant level.
    fn below(&self, level: f64) -> PySignal {
        PySignal::wrap(source_to_signal!(self.src.clone(), |s| s.below(level)))
    }

    /// `self` rises above `other` on this step.
    fn crosses_above(&self, other: &Bound<'_, PyAny>) -> PyResult<PySignal> {
        let rhs = coerce_operand(other)?;
        Ok(PySignal::wrap(sources_to_signal!(
            self.src.clone(),
            rhs,
            |l, r| l.crosses_above(r)
        )?))
    }

    /// `self` falls below `other` on this step.
    fn crosses_below(&self, other: &Bound<'_, PyAny>) -> PyResult<PySignal> {
        let rhs = coerce_operand(other)?;
        Ok(PySignal::wrap(sources_to_signal!(
            self.src.clone(),
            rhs,
            |l, r| l.crosses_below(r)
        )?))
    }

    // --- arithmetic -> Indicator ----------------------------------------------
    /// Pointwise `self + other` (`other` may be an Indicator or a number).
    fn add(&self, other: &Bound<'_, PyAny>) -> PyResult<PyIndicator> {
        let rhs = coerce_operand(other)?;
        Ok(PyIndicator::wrap(combine_sources!(
            self.src.clone(),
            rhs,
            |l, r| l.add(r)
        )?))
    }
    /// Pointwise `self - other`.
    fn sub(&self, other: &Bound<'_, PyAny>) -> PyResult<PyIndicator> {
        let rhs = coerce_operand(other)?;
        Ok(PyIndicator::wrap(combine_sources!(
            self.src.clone(),
            rhs,
            |l, r| l.sub(r)
        )?))
    }
    /// Pointwise `self * other`.
    fn mul(&self, other: &Bound<'_, PyAny>) -> PyResult<PyIndicator> {
        let rhs = coerce_operand(other)?;
        Ok(PyIndicator::wrap(combine_sources!(
            self.src.clone(),
            rhs,
            |l, r| l.mul(r)
        )?))
    }
    /// Pointwise `self / other` (`None` on divide-by-zero).
    fn div(&self, other: &Bound<'_, PyAny>) -> PyResult<PyIndicator> {
        let rhs = coerce_operand(other)?;
        Ok(PyIndicator::wrap(combine_sources!(
            self.src.clone(),
            rhs,
            |l, r| l.div(r)
        )?))
    }

    fn __add__(&self, other: &Bound<'_, PyAny>) -> PyResult<PyIndicator> {
        self.add(other)
    }
    fn __sub__(&self, other: &Bound<'_, PyAny>) -> PyResult<PyIndicator> {
        self.sub(other)
    }
    fn __mul__(&self, other: &Bound<'_, PyAny>) -> PyResult<PyIndicator> {
        self.mul(other)
    }
    fn __truediv__(&self, other: &Bound<'_, PyAny>) -> PyResult<PyIndicator> {
        self.div(other)
    }
    fn __radd__(&self, other: &Bound<'_, PyAny>) -> PyResult<PyIndicator> {
        let lhs = coerce_operand(other)?;
        Ok(PyIndicator::wrap(combine_sources!(
            lhs,
            self.src.clone(),
            |l, r| l.add(r)
        )?))
    }
    fn __rsub__(&self, other: &Bound<'_, PyAny>) -> PyResult<PyIndicator> {
        let lhs = coerce_operand(other)?;
        Ok(PyIndicator::wrap(combine_sources!(
            lhs,
            self.src.clone(),
            |l, r| l.sub(r)
        )?))
    }
    fn __rmul__(&self, other: &Bound<'_, PyAny>) -> PyResult<PyIndicator> {
        let lhs = coerce_operand(other)?;
        Ok(PyIndicator::wrap(combine_sources!(
            lhs,
            self.src.clone(),
            |l, r| l.mul(r)
        )?))
    }
    fn __rtruediv__(&self, other: &Bound<'_, PyAny>) -> PyResult<PyIndicator> {
        let lhs = coerce_operand(other)?;
        Ok(PyIndicator::wrap(combine_sources!(
            lhs,
            self.src.clone(),
            |l, r| l.div(r)
        )?))
    }

    // --- lookback / rolling -> Indicator --------------------------------------
    /// `self` delayed by `periods` steps.
    fn lag(&self, periods: usize) -> PyIndicator {
        PyIndicator::wrap(map_source!(self.src.clone(), |s| s.lag(periods)))
    }
    /// Discrete difference over `periods` steps (`x[t] - x[t-n]`).
    fn diff(&self, periods: usize) -> PyIndicator {
        PyIndicator::wrap(map_source!(self.src.clone(), |s| s.diff(periods)))
    }
    /// Ratio to the value `periods` steps ago (`x[t] / x[t-n]`).
    fn ratio(&self, periods: usize) -> PyIndicator {
        PyIndicator::wrap(map_source!(self.src.clone(), |s| s.ratio(periods)))
    }
    /// Percentage rate of change over `periods` steps.
    fn roc(&self, periods: usize) -> PyIndicator {
        PyIndicator::wrap(map_source!(self.src.clone(), |s| s.roc(periods)))
    }
    /// Rolling maximum over `period` steps.
    fn rolling_max(&self, period: usize) -> PyResult<PyIndicator> {
        ensure_period(period)?;
        Ok(PyIndicator::wrap(
            map_source!(self.src.clone(), |s| s.rolling_max(period))
        ))
    }
    /// Rolling minimum over `period` steps.
    fn rolling_min(&self, period: usize) -> PyResult<PyIndicator> {
        ensure_period(period)?;
        Ok(PyIndicator::wrap(
            map_source!(self.src.clone(), |s| s.rolling_min(period))
        ))
    }

    /// Passthrough that forces this indicator's reported `unstable_period()` to
    /// `0`. Output and `warm_up_period()` are unchanged; a downstream reader of
    /// `stable_period()` (a strategy readiness gate, an overlay trim) no longer
    /// waits for this subtree's IIR settling tail. Use to explicitly opt out of
    /// the safe default that waits for it.
    fn unstable(&self) -> PyIndicator {
        PyIndicator::wrap(map_source!(self.src.clone(), |s| s.unstable()))
    }

    fn __repr__(&self) -> String {
        match self.src.value() {
            Some(v) => format!("Indicator(value={v})"),
            None => "Indicator(value=None)".to_string(),
        }
    }
}

/// A boolean signal. Combine signals with `&` / `|` / `^` / `~` (or the named
/// `and_` / `or_` / `xor_` / `not_` / `changed` methods).
#[pyclass(name = "Signal")]
struct PySignal {
    sig: AnySignal,
}

impl PySignal {
    fn wrap(sig: AnySignal) -> Self {
        PySignal { sig }
    }
}

#[pymethods]
impl PySignal {
    /// Feed the next sample; returns the current boolean state. Pass a `Candle`
    /// for a candle-rooted signal, a `float` for an identity-rooted one.
    fn update(&mut self, sample: &Bound<'_, PyAny>) -> PyResult<bool> {
        match &mut self.sig {
            AnySignal::Candle(s) => {
                Ok(Indicator::update(s, extract_atom(sample)?).unwrap_or(false))
            }
            AnySignal::Real(s) => Ok(Indicator::update(s, extract_real(sample)?).unwrap_or(false)),
            AnySignal::Snapshot(s) => Ok(
                Indicator::update(s, extract_snapshot(sample)?).unwrap_or(false),
            ),
        }
    }

    /// Compute the signal over a whole series at once, returning one boolean per
    /// bar. `data` is the same as for [`Indicator.feed`](PyIndicator): a
    /// DataFrame/dict of OHLCV columns for a candle-rooted signal, or a 1-D
    /// series for an identity-rooted one, or a sequence of `Snapshot`s for a
    /// snapshot-rooted one. The output mirrors the input: a boolean
    /// pandas/polars `Series`, otherwise a boolean NumPy `ndarray`. Fed
    /// through the current state — call `reset()` first for a clean pass.
    fn feed(&mut self, py: Python<'_>, data: &Bound<'_, PyAny>) -> PyResult<Py<PyAny>> {
        let kind = OutputKind::detect(data)?;
        let values: Vec<bool> = match &mut self.sig {
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
        };
        build_bools(py, &kind, values)
    }

    /// The most recent boolean state, without advancing.
    fn is_true(&self) -> bool {
        self.sig.is_true()
    }

    /// The number of samples needed before the signal can produce a real state
    /// (it reads `False` while warming up).
    fn warm_up_period(&self) -> usize {
        self.sig.warm_up_period()
    }

    /// Extra samples after warm-up before any recursive sources inside the
    /// signal have effectively converged; `0` for windowed ones.
    fn unstable_period(&self) -> usize {
        self.sig.unstable_period()
    }

    /// `warm_up_period() + unstable_period()`: how much history to feed before
    /// trusting the signal.
    fn stable_period(&self) -> usize {
        self.sig.warm_up_period() + self.sig.unstable_period()
    }

    /// Reset all internal state.
    fn reset(&mut self) {
        self.sig.reset()
    }

    /// Logical AND.
    fn and_(&self, other: PyRef<'_, PySignal>) -> PyResult<PySignal> {
        Ok(PySignal::wrap(combine_signals!(
            self.sig.clone(),
            other.sig.clone(),
            |a, b| a.and(b)
        )?))
    }
    /// Logical OR.
    fn or_(&self, other: PyRef<'_, PySignal>) -> PyResult<PySignal> {
        Ok(PySignal::wrap(combine_signals!(
            self.sig.clone(),
            other.sig.clone(),
            |a, b| a.or(b)
        )?))
    }
    /// Logical XOR.
    fn xor_(&self, other: PyRef<'_, PySignal>) -> PyResult<PySignal> {
        Ok(PySignal::wrap(combine_signals!(
            self.sig.clone(),
            other.sig.clone(),
            |a, b| a.xor(b)
        )?))
    }
    /// Logical NOT.
    fn not_(&self) -> PySignal {
        PySignal::wrap(map_signal!(self.sig.clone(), |s| s.not()))
    }
    /// Fires on the single step where this signal toggles (either direction).
    fn changed(&self) -> PySignal {
        PySignal::wrap(map_signal!(self.sig.clone(), |s| s.changed()))
    }

    /// Passthrough that forces this signal's reported `unstable_period()` to
    /// `0`. Output and `warm_up_period()` are unchanged; a downstream reader of
    /// `stable_period()` (a strategy readiness gate) no longer waits for this
    /// subtree's IIR settling tail. Mirrors the free `unstable(x)` function; use
    /// to explicitly opt out of the safe default that waits for the tail.
    fn unstable(&self) -> PySignal {
        PySignal::wrap(map_signal!(self.sig.clone(), |s| s.unstable()))
    }

    fn __and__(&self, other: PyRef<'_, PySignal>) -> PyResult<PySignal> {
        self.and_(other)
    }
    fn __or__(&self, other: PyRef<'_, PySignal>) -> PyResult<PySignal> {
        self.or_(other)
    }
    fn __xor__(&self, other: PyRef<'_, PySignal>) -> PyResult<PySignal> {
        self.xor_(other)
    }
    fn __invert__(&self) -> PySignal {
        self.not_()
    }

    fn __repr__(&self) -> String {
        format!("Signal(value={})", self.sig.is_true())
    }
}

/// A string-valued source (`Arc<str>` output). Produced by `get_str()` for a
/// `Str`-typed overlay column and `value_str()` for a string literal;
/// consumed by `str_eq()` / `str_ne()` to build a boolean signal.
///
/// Distinct from `Indicator` because a real-valued signal chain has no notion
/// of a string output — the only thing you can do with a `StrSource` is
/// compare it (against another `StrSource` or a Python `str`). All string
/// sources are atom-rooted: `get_str()` reads an overlay slot, and
/// `value_str()`'s constant ignores its input.
#[pyclass(name = "StrSource")]
struct PyStrSource {
    src: AnyStrSource,
}

impl PyStrSource {
    fn wrap(src: AnyStrSource) -> Self {
        PyStrSource { src }
    }
}

#[pymethods]
impl PyStrSource {
    /// Feed the next sample; returns the current string, or `None` while
    /// warming up. Always accepts an `Atom` (or a `Candle`, lifted to an
    /// overlay-free atom — which makes an overlay-reading source yield
    /// `None`).
    fn update(&mut self, sample: &Bound<'_, PyAny>) -> PyResult<Option<String>> {
        let atom = extract_atom(sample)?;
        let out = match &mut self.src {
            AnyStrSource::Candle(s) => Indicator::update(s, atom),
            AnyStrSource::Const(c) => Some(c.clone()),
        };
        Ok(out.map(|s| s.to_string()))
    }

    /// The most recent value, without advancing state.
    fn value(&self) -> Option<String> {
        self.src.value().map(|s| s.to_string())
    }

    fn is_ready(&self) -> bool {
        self.src.value().is_some()
    }

    fn warm_up_period(&self) -> usize {
        self.src.warm_up_period()
    }

    fn unstable_period(&self) -> usize {
        self.src.unstable_period()
    }

    fn stable_period(&self) -> usize {
        self.src.warm_up_period() + self.src.unstable_period()
    }

    fn reset(&mut self) {
        self.src.reset()
    }

    /// `self == other` — build a boolean signal that fires when both string
    /// sources agree. `other` may be another `StrSource` or a Python `str`
    /// (lifted to a `ValueStr` constant).
    fn eq(&self, other: &Bound<'_, PyAny>) -> PyResult<PySignal> {
        let rhs = coerce_str_operand(other)?;
        let (l, r) = str_pair(self.src.clone(), rhs);
        Ok(PySignal::wrap(AnySignal::Candle(SignalBox::new(
            Combine::<_, _, StrEqOp>::new(l, r),
        ))))
    }

    /// `self != other` — the string counterpart to [`eq`](Self::eq).
    fn ne(&self, other: &Bound<'_, PyAny>) -> PyResult<PySignal> {
        let rhs = coerce_str_operand(other)?;
        let (l, r) = str_pair(self.src.clone(), rhs);
        Ok(PySignal::wrap(AnySignal::Candle(SignalBox::new(
            Combine::<_, _, StrNeOp>::new(l, r),
        ))))
    }

    fn __repr__(&self) -> String {
        match self.src.value() {
            Some(v) => format!("StrSource(value={v:?})"),
            None => "StrSource(value=None)".to_string(),
        }
    }
}

/// Coerce a Python operand for a string-comparison RHS: accepts either a
/// `PyStrSource` or a Python `str` (lifted to `AnyStrSource::Const`).
fn coerce_str_operand(other: &Bound<'_, PyAny>) -> PyResult<AnyStrSource> {
    if let Ok(src) = other.cast::<PyStrSource>() {
        return Ok(src.borrow().src.clone());
    }
    if let Ok(s) = other.extract::<String>() {
        return Ok(AnyStrSource::Const(Arc::from(s.as_str())));
    }
    Err(PyTypeError::new_err(
        "expected a StrSource or a str for string comparison",
    ))
}

/// A multi-output indicator (MACD, Bollinger, ADX, …). `update`/`value`
/// return a dict of the named output lines. Terminal: it cannot be used as a
/// source for further composition.
#[pyclass(name = "MultiIndicator")]
struct PyMulti {
    inner: AnyMulti,
}

#[pymethods]
impl PyMulti {
    /// Feed the next sample; returns a dict of output lines, or `None` while
    /// warming up. Pass a `Candle` for a candle-rooted indicator, a `float` for
    /// an identity-rooted one.
    fn update<'py>(
        &mut self,
        py: Python<'py>,
        sample: &Bound<'_, PyAny>,
    ) -> PyResult<Option<Bound<'py, PyDict>>> {
        let names = self.inner.names();
        let out = match &mut self.inner {
            AnyMulti::Candle(m) => m.0.update(extract_atom(sample)?),
            AnyMulti::Real(m) => m.0.update(extract_real(sample)?),
            AnyMulti::Snapshot(m) => m.0.update(extract_snapshot(sample)?),
        };
        match out {
            Some(values) => Ok(Some(values_to_dict(py, names, &values)?)),
            None => Ok(None),
        }
    }

    /// Compute the indicator over a whole series at once. `data` is the same as
    /// for [`Indicator.feed`](PyIndicator): a DataFrame/dict of OHLCV columns for
    /// a candle-rooted indicator, or a 1-D series for an identity-rooted one.
    ///
    /// The output is a frame with one column per line: a pandas/polars
    /// `DataFrame` (index preserved for pandas) when given that library's
    /// frame/series, otherwise a `dict` of NumPy arrays. Warm-up bars are `NaN`.
    /// Fed through the current state — call `reset()` first for a clean pass.
    fn feed(&mut self, py: Python<'_>, data: &Bound<'_, PyAny>) -> PyResult<Py<PyAny>> {
        let kind = OutputKind::detect(data)?;
        let names = self.inner.names();
        let rows: Vec<Option<Vec<f64>>> = match &mut self.inner {
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
        };
        build_multi(py, &kind, names, rows)
    }

    /// The most recent output dict, without advancing.
    fn value<'py>(&self, py: Python<'py>) -> PyResult<Option<Bound<'py, PyDict>>> {
        let names = self.inner.names();
        match self.inner.value() {
            Some(values) => Ok(Some(values_to_dict(py, names, &values)?)),
            None => Ok(None),
        }
    }

    /// Whether enough samples have been seen to produce a value.
    fn is_ready(&self) -> bool {
        self.inner.value().is_some()
    }

    /// The number of samples needed before the first value can appear (for the
    /// slowest output line).
    fn warm_up_period(&self) -> usize {
        self.inner.warm_up_period()
    }

    /// Extra samples after warm-up before a recursive indicator (MACD, ADX, …)
    /// has effectively converged; `0` for windowed indicators.
    fn unstable_period(&self) -> usize {
        self.inner.unstable_period()
    }

    /// `warm_up_period() + unstable_period()`: how much history to feed before
    /// trusting the output.
    fn stable_period(&self) -> usize {
        self.inner.warm_up_period() + self.inner.unstable_period()
    }

    /// Reset all internal state.
    fn reset(&mut self) {
        self.inner.reset()
    }

    /// Wrap this multi in a [`SharedMultiIndicator`](PySharedMulti) handle so
    /// per-line component accessors (`.line()`, `.upper()`, …) built off the
    /// handle project into the **same** underlying source and advance it at
    /// most once per bar — the analogue of Rust's `.shared()`. The original
    /// `MultiIndicator` is left untouched (it keeps a deep-cloned copy of the
    /// source), so both handles can coexist.
    ///
    /// ```python
    /// macd = ta.macd(ta.close(), 12, 26, 9).shared()
    /// # Both accessors project the same MACD; the full MACD math runs once
    /// # per bar however many accessors read out of it.
    /// bullish = macd.line().crosses_above(macd.signal())
    /// ```
    fn shared(&self) -> PySharedMulti {
        let cloned = match &self.inner {
            AnyMulti::Candle(m) => AnySharedMulti::Candle(Arc::new(Mutex::new(SharedMultiCell {
                names: m.0.names(),
                multi: m.0.clone_box(),
                generation: 0,
                last_output: None,
            }))),
            AnyMulti::Real(m) => AnySharedMulti::Real(Arc::new(Mutex::new(SharedMultiCell {
                names: m.0.names(),
                multi: m.0.clone_box(),
                generation: 0,
                last_output: None,
            }))),
            AnyMulti::Snapshot(m) => {
                AnySharedMulti::Snapshot(Arc::new(Mutex::new(SharedMultiCell {
                    names: m.0.names(),
                    multi: m.0.clone_box(),
                    generation: 0,
                    last_output: None,
                })))
            }
        };
        PySharedMulti { inner: cloned }
    }
}

/// A shared handle over a multi-output indicator: per-line accessors
/// (`.line()`, `.signal()`, `.histogram()`, `.upper()`, `.middle()`,
/// `.lower()`, `.plus_di()`, `.minus_di()`, `.adx()`, `.up()`, `.down()`,
/// `.oscillator()`) all project into the same underlying source, so the
/// multi advances **once per bar** regardless of how many accessors the
/// surrounding expression tree contains.
///
/// Construct via [`MultiIndicator.shared()`](PyMulti::shared). Every accessor
/// returns a plain [`Indicator`](PyIndicator) — the returned handle is
/// composable with the same operators (`gt`, `crosses_above`, `add`, …) any
/// other `Real`-output source is.
#[pyclass(name = "SharedMultiIndicator")]
struct PySharedMulti {
    inner: AnySharedMulti,
}

/// Emit the accessor list on `PySharedMulti`. Each generated method resolves
/// the name against the underlying multi's field list (declared once per
/// concrete `MultiOutput` impl); an accessor whose name doesn't match a field
/// of *this* particular multi errors clearly at call time.
#[pymethods]
impl PySharedMulti {
    /// MACD line (fast EMA − slow EMA) as a standalone indicator.
    fn macd(&self) -> PyResult<PyIndicator> {
        self.inner.project("macd")
    }
    /// MACD line — alias for [`macd`](Self::macd), matching Rust's
    /// `Macd::line()` accessor.
    fn line(&self) -> PyResult<PyIndicator> {
        self.inner.project("macd")
    }
    /// MACD signal line (EMA of the MACD line).
    fn signal(&self) -> PyResult<PyIndicator> {
        self.inner.project("signal")
    }
    /// MACD histogram (line − signal).
    fn histogram(&self) -> PyResult<PyIndicator> {
        self.inner.project("histogram")
    }
    /// Bollinger / Keltner / Donchian upper band.
    fn upper(&self) -> PyResult<PyIndicator> {
        self.inner.project("upper")
    }
    /// Bollinger / Keltner / Donchian middle band.
    fn middle(&self) -> PyResult<PyIndicator> {
        self.inner.project("middle")
    }
    /// Bollinger / Keltner / Donchian lower band.
    fn lower(&self) -> PyResult<PyIndicator> {
        self.inner.project("lower")
    }
    /// ADX / DMI positive directional indicator, `+DI`.
    fn plus_di(&self) -> PyResult<PyIndicator> {
        self.inner.project("plus_di")
    }
    /// ADX / DMI negative directional indicator, `−DI`.
    fn minus_di(&self) -> PyResult<PyIndicator> {
        self.inner.project("minus_di")
    }
    /// ADX line (the trend-strength value).
    fn adx(&self) -> PyResult<PyIndicator> {
        self.inner.project("adx")
    }
    /// Aroon Up.
    fn up(&self) -> PyResult<PyIndicator> {
        self.inner.project("up")
    }
    /// Aroon Down.
    fn down(&self) -> PyResult<PyIndicator> {
        self.inner.project("down")
    }
    /// Aroon oscillator (up − down).
    fn oscillator(&self) -> PyResult<PyIndicator> {
        self.inner.project("oscillator")
    }

    /// The output field names available on the underlying multi.
    fn names(&self) -> Vec<String> {
        self.inner.names().iter().map(|s| s.to_string()).collect()
    }

    /// Project the component named `name` (one of [`names`](Self::names)) as a
    /// standalone [`Indicator`]. Prefer the named accessors when one matches;
    /// this is the fallback for programmatic lookup.
    fn component(&self, name: &str) -> PyResult<PyIndicator> {
        self.inner.project(name)
    }

    fn __repr__(&self) -> String {
        let names = self.inner.names();
        format!("SharedMultiIndicator(fields={names:?})")
    }
}

// ---------------------------------------------------------------------------
// Strategy layer: Wallet + Order + Size
//
// A strategy in Python is just code that, each bar, reads signals/indicators and
// acts on a Wallet. So rather than binding a Rust strategy trait, we expose the
// Wallet the strategy trades into. Symbols are plain strings; sides are "buy" /
// "sell"; sizes are a unit count or a `Size`.
// ---------------------------------------------------------------------------

/// How much to trade: a bare number is units, or use the relative constructors.
#[pyclass(name = "Size", frozen, from_py_object)]
#[derive(Clone, Copy)]
struct PySize {
    inner: Size,
}

#[pymethods]
impl PySize {
    /// An absolute number of units.
    #[staticmethod]
    fn units(units: f64) -> Self {
        PySize {
            inner: Size::Units(units),
        }
    }
    /// A fraction of available funds (cash), converted to units at the price.
    #[staticmethod]
    fn funds_frac(fraction: f64) -> Self {
        PySize {
            inner: Size::FundsFraction(fraction),
        }
    }
    /// A fraction of total equity, converted to units at the price.
    /// `value_frac(1.0)` is "all-in" and reverses cleanly on a flip.
    #[staticmethod]
    fn value_frac(fraction: f64) -> Self {
        PySize {
            inner: Size::ValueFraction(fraction),
        }
    }
    /// A fraction of the symbol's current position (adjust-only).
    #[staticmethod]
    fn position_frac(fraction: f64) -> Self {
        PySize {
            inner: Size::PositionFraction(fraction),
        }
    }
}

/// A filled order: `symbol`, `side` ("buy"/"sell"), and a positive `units`.
#[pyclass(name = "Order", frozen, skip_from_py_object)]
#[derive(Clone)]
struct PyOrder {
    inner: Order<String>,
}

#[pymethods]
impl PyOrder {
    #[getter]
    fn symbol(&self) -> String {
        self.inner.symbol.clone()
    }
    #[getter]
    fn side(&self) -> &'static str {
        side_str(self.inner.side)
    }
    #[getter]
    fn units(&self) -> f64 {
        self.inner.units
    }
    /// The per-unit price this order filled at.
    #[getter]
    fn price(&self) -> f64 {
        self.inner.price
    }
    /// What produced this fill: `"market"`, `"stop"`, or `"take_profit"`.
    #[getter]
    fn kind(&self) -> &'static str {
        kind_str(self.inner.kind)
    }
    /// `+units` for a buy, `-units` for a sell.
    fn signed_units(&self) -> f64 {
        self.inner.signed_units()
    }
    fn __repr__(&self) -> String {
        format!(
            "Order(symbol='{}', side='{}', units={}, price={}, kind='{}')",
            self.inner.symbol,
            self.side(),
            self.inner.units,
            self.inner.price,
            self.kind(),
        )
    }
}

/// A paper-trading wallet a strategy trades into: funds, per-symbol positions,
/// the prices fed to it, and a blotter of executed orders. (The live-broker
/// counterpart would be a separate wallet type implementing the same interface.)
///
/// Feed each symbol's bar every tick with `update(symbol, candle_or_price)`,
/// which returns the orders that filled on it (the fill stream); the wallet is
/// otherwise market-agnostic. `set(symbol, side, size)` targets an absolute
/// position (an opposite-side `set` reverses; `Size.value_frac(1.0)` is all-in),
/// `set_position(symbol, target)` drives to an absolute unit count, and `close`
/// flattens. These are **market orders**: they queue and fill on the next
/// `update`, at that bar's `open` (so a backtest never fills on the same bar whose
/// `close` triggered the signal), returning `None` — the filled `Order` shows up
/// in that `update`'s return (and in `orders()`). Protective exits are **resting
/// orders**: `set_stop(symbol, trigger)` and `set_take_profit(symbol, trigger)`
/// register a level (idempotent, latest-wins per symbol; re-submit to trail) that
/// the wallet triggers and prices itself — filling at the level, or the bar's
/// `open` on a gap — and `cancel_protective(symbol)` drops both legs. Each `Order`
/// carries a `kind` of `"market"`, `"stop"`, or `"take_profit"`.
#[pyclass(name = "PaperWallet")]
struct PyWallet {
    inner: PaperWallet<String>,
}

#[pymethods]
impl PyWallet {
    /// A wallet seeded with `funds` of cash and no positions.
    #[new]
    fn new(funds: f64) -> Self {
        PyWallet {
            inner: PaperWallet::new(funds),
        }
    }

    /// The available cash balance.
    #[getter]
    fn funds(&self) -> f64 {
        self.inner.funds().0
    }

    /// The signed position in `symbol` (positive long, negative short).
    fn position(&self, symbol: &str) -> f64 {
        self.inner.position(&symbol.to_string()).amount
    }

    /// The last price fed for `symbol`, or `None` if never fed.
    fn price(&self, symbol: &str) -> Option<f64> {
        self.inner.price(&symbol.to_string()).map(|p| p.0)
    }

    /// The held positions as a `{symbol: quantity}` dict.
    fn positions<'py>(&self, py: Python<'py>) -> PyResult<Bound<'py, PyDict>> {
        let dict = PyDict::new(py);
        for position in self.inner.positions() {
            dict.set_item(position.symbol, position.amount)?;
        }
        Ok(dict)
    }

    /// Every order executed so far (the trade blotter).
    fn orders(&self) -> Vec<PyOrder> {
        self.inner
            .orders()
            .iter()
            .cloned()
            .map(|inner| PyOrder { inner })
            .collect()
    }

    /// Restore the wallet to its freshly-constructed state — the seed funds it
    /// was built with, no positions, no fed prices, no pending or resting
    /// orders, and an empty blotter.
    fn reset(&mut self) {
        self.inner.reset();
    }

    /// Mark-to-market equity: funds plus each position valued at its fed price.
    fn equity(&self) -> f64 {
        self.inner.equity().0
    }

    /// Feed `symbol`'s current bar and return the orders that filled on it (the
    /// fill stream — a queued market order at this bar's `open`, and any resting
    /// stop / take-profit this bar triggers). Accepts a `Candle` (whose `close`
    /// marks to market and whose `[low, high]` bounds fills) or a bare price
    /// `float` (a flat bar `open = high = low = close`). Call this each tick before
    /// trading or reading `equity`.
    fn update(&mut self, symbol: String, bar: &Bound<'_, PyAny>) -> PyResult<Vec<PyOrder>> {
        let candle = if let Ok(candle) = bar.cast::<PyCandle>() {
            candle.borrow().inner
        } else {
            let price: f64 = bar.extract()?;
            Candle::new(price, price, price, price, 0.0)
        };
        Ok(self
            .inner
            .update(symbol, candle)
            .into_iter()
            .map(|inner| PyOrder { inner })
            .collect())
    }

    /// Queue a market order driving `symbol` to `target` signed units; it fills on
    /// the next `update`, at that bar's `open`. Returns `None` (working — the fill
    /// shows up in that `update`'s return, not here).
    fn set_position(&mut self, symbol: String, target: f64) -> PyResult<Option<PyOrder>> {
        wrap_ack(self.inner.set_position(Units {
            symbol,
            amount: target,
        }))
    }

    /// Queue a market order targeting `side` `size` of `symbol`; it fills on the
    /// next `update`, at that bar's `open` (where the `size` is resolved, so an
    /// all-in stays exact). Returns `None` — working.
    fn set(
        &mut self,
        symbol: String,
        side: &str,
        size: &Bound<'_, PyAny>,
    ) -> PyResult<Option<PyOrder>> {
        wrap_ack(
            self.inner
                .set(symbol, parse_side(side)?, coerce_size(size)?),
        )
    }

    /// Queue a market order flattening `symbol`; it fills on the next `update`, at
    /// that bar's `open`. Returns `None` — working.
    fn close(&mut self, symbol: String) -> PyResult<Option<PyOrder>> {
        wrap_ack(self.inner.close(symbol))
    }

    /// Rest a stop-loss on `symbol` at `trigger` — an adverse level the wallet
    /// fills when a bar trades through it (the side is read from the current
    /// position). Idempotent, latest-wins per symbol; re-submit to trail. Returns
    /// `None` (the resting order is working until it triggers in some `update`).
    fn set_stop(&mut self, symbol: String, trigger: f64) -> PyResult<Option<PyOrder>> {
        wrap_ack(self.inner.set_stop(symbol, Reference(trigger)))
    }

    /// Rest a take-profit on `symbol` at `trigger` — the favourable twin of
    /// `set_stop`. Idempotent, latest-wins per symbol. Returns `None` (working).
    fn set_take_profit(&mut self, symbol: String, trigger: f64) -> PyResult<Option<PyOrder>> {
        wrap_ack(self.inner.set_take_profit(symbol, Reference(trigger)))
    }

    /// Cancel both resting protective legs (stop and take-profit) on `symbol`.
    fn cancel_protective(&mut self, symbol: String) -> PyResult<()> {
        self.inner
            .cancel_protective(&symbol)
            .map_err(|error| PyValueError::new_err(error.to_string()))
    }
}

/// Map a wallet `Ack` to Python: the fill if it filled synchronously, `None` if it
/// is merely working, or a `ValueError`.
fn wrap_ack(result: Result<Ack<String>, WalletError>) -> PyResult<Option<PyOrder>> {
    match result {
        Ok(Ack::Filled(inner)) => Ok(Some(PyOrder { inner })),
        Ok(Ack::Working(_)) => Ok(None),
        Err(error) => Err(PyValueError::new_err(error.to_string())),
    }
}

/// Parse a side string into a [`Side`].
fn parse_side(side: &str) -> PyResult<Side> {
    match side.to_ascii_lowercase().as_str() {
        "buy" | "long" => Ok(Side::Buy),
        "sell" | "short" => Ok(Side::Sell),
        _ => Err(PyValueError::new_err("side must be 'buy' or 'sell'")),
    }
}

/// Coerce a Python argument into a [`Size`]: a number is units, or a `Size`.
fn coerce_size(obj: &Bound<'_, PyAny>) -> PyResult<Size> {
    if let Ok(size) = obj.extract::<PySize>() {
        Ok(size.inner)
    } else if let Ok(units) = obj.extract::<f64>() {
        Ok(Size::Units(units))
    } else {
        Err(PyTypeError::new_err(
            "size must be a number of units or a Size",
        ))
    }
}

/// The `"buy"`/`"sell"` string for a [`Side`].
fn side_str(side: Side) -> &'static str {
    match side {
        Side::Buy => "buy",
        Side::Sell => "sell",
    }
}

/// The `"market"`/`"stop"`/`"take_profit"` string for an [`OrderKind`].
fn kind_str(kind: OrderKind) -> &'static str {
    match kind {
        OrderKind::Market => "market",
        OrderKind::Stop => "stop",
        OrderKind::TakeProfit => "take_profit",
    }
}

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

fn ensure_period(period: usize) -> PyResult<()> {
    if period == 0 {
        Err(PyValueError::new_err("period must be greater than 0"))
    } else {
        Ok(())
    }
}

/// Extract a `Candle` for a candle-rooted node's `update`.
fn extract_candle(sample: &Bound<'_, PyAny>) -> PyResult<Candle> {
    let candle = sample.cast::<PyCandle>().map_err(|_| {
        PyTypeError::new_err(
            "this indicator consumes candles; pass a Candle. (For a value-series \
             indicator, root it at identity() and pass a float.)",
        )
    })?;
    Ok(candle.borrow().inner)
}

/// Extract an [`Atom`] for a candle-rooted node's `update`. Accepts either a
/// bare `Candle` (lifted via `From<Candle>`, with no overlays) or an `Atom`
/// carrying an [`OverlayInfo`] side-channel. This is the input for every
/// bar-consuming indicator/signal on the Python side.
fn extract_atom(sample: &Bound<'_, PyAny>) -> PyResult<Atom> {
    if let Ok(atom) = sample.cast::<PyAtom>() {
        return Ok(atom.borrow().inner.clone());
    }
    let candle = extract_candle(sample)?;
    Ok(candle.into())
}

/// Extract a `float` for an identity-rooted node's `update`.
fn extract_real(sample: &Bound<'_, PyAny>) -> PyResult<Real> {
    sample
        .extract::<f64>()
        .map_err(|_| PyTypeError::new_err("this indicator consumes a value stream; pass a float"))
}

/// Iterate a Python sequence of snapshots (`list[Snapshot]` or `list[dict]`)
/// into a native `Vec<Snapshot<Selector>>` for a snapshot-rooted node's `feed`.
fn snapshots_from_sequence(obj: &Bound<'_, PyAny>) -> PyResult<Vec<Snapshot<Selector>>> {
    let mut out = Vec::new();
    let iter = obj.try_iter().map_err(|_| {
        PyTypeError::new_err(
            "snapshot-rooted feed(): expected an iterable of Snapshot (or dict) values",
        )
    })?;
    for item in iter {
        out.push(extract_snapshot(&item?)?);
    }
    Ok(out)
}

/// Extract a `Snapshot<Selector>` for a snapshot-rooted node's `update`.
/// Accepts a `PySnapshot` directly, or a Python `dict` whose keys are coerced
/// via [`coerce_selector`] (str → symbol, Frequency → freq, Selector as-is,
/// (str, freq) tuple → both fields).
fn extract_snapshot(sample: &Bound<'_, PyAny>) -> PyResult<Snapshot<Selector>> {
    if let Ok(snap) = sample.cast::<PySnapshot>() {
        return Ok(snap.borrow().inner.clone());
    }
    if let Ok(dict) = sample.cast::<pyo3::types::PyDict>() {
        let mut out = Snapshot::<Selector>::new();
        for (k, v) in dict.iter() {
            let key = coerce_selector(&k)?;
            let atom = extract_atom(&v)?;
            out.insert(key, atom);
        }
        return Ok(out);
    }
    Err(PyTypeError::new_err(
        "this indicator consumes a Snapshot; pass a Snapshot or a dict[str, Atom|Candle]",
    ))
}

/// Collect any 1-D sequence of numbers (`list`, NumPy array, pandas `Series`,
/// …) into a `Vec<f64>`, attributing failures to the named column.
fn column_to_vec(obj: &Bound<'_, PyAny>, name: &str) -> PyResult<Vec<f64>> {
    let err = || {
        PyTypeError::new_err(format!(
            "'{name}' must be a 1-D sequence of numbers (list, NumPy array, or pandas Series)"
        ))
    };
    let mut values = Vec::new();
    for item in obj.try_iter().map_err(|_| err())? {
        values.push(item?.extract::<f64>().map_err(|_| err())?);
    }
    Ok(values)
}

/// Zip OHLCV columns into candles. `close` is the anchor: omitted `open`/`high`/
/// `low` default to it and omitted `volume` to `0`.
fn assemble_candles(
    close: Vec<f64>,
    open: Option<Vec<f64>>,
    high: Option<Vec<f64>>,
    low: Option<Vec<f64>>,
    volume: Option<Vec<f64>>,
) -> PyResult<Vec<Candle>> {
    let n = close.len();
    for (name, col) in [
        ("open", &open),
        ("high", &high),
        ("low", &low),
        ("volume", &volume),
    ] {
        if let Some(col) = col
            && col.len() != n
        {
            return Err(PyValueError::new_err(format!(
                "'{name}' has length {} but 'close' has length {n}",
                col.len()
            )));
        }
    }
    Ok((0..n)
        .map(|i| {
            let c = close[i];
            Candle::new(
                open.as_ref().map_or(c, |a| a[i]),
                high.as_ref().map_or(c, |a| a[i]),
                low.as_ref().map_or(c, |a| a[i]),
                c,
                volume.as_ref().map_or(0.0, |a| a[i]),
            )
        })
        .collect())
}

/// Build the candle series a candle-rooted `feed()` consumes from its `data`
/// argument: a pandas/polars `DataFrame` or a `dict` of OHLCV columns. A bare
/// numeric series is rejected — root the indicator at `identity()` for that.
fn candles_from_frame(data: &Bound<'_, PyAny>) -> PyResult<Vec<Candle>> {
    if data.hasattr("columns")? || data.is_instance_of::<PyDict>() {
        frame_to_candles(data)
    } else {
        Err(PyTypeError::new_err(
            "this indicator consumes candles: pass a DataFrame or dict with OHLCV columns. \
             To compute over a bare numeric series, root the indicator at identity().",
        ))
    }
}

/// Build the value series an identity-rooted `feed()` consumes: a plain 1-D
/// numeric sequence. A `DataFrame`/`dict` is rejected — it has no single value
/// stream to read.
fn reals_from_series(data: &Bound<'_, PyAny>) -> PyResult<Vec<Real>> {
    if data.hasattr("columns")? || data.is_instance_of::<PyDict>() {
        return Err(PyTypeError::new_err(
            "an identity-rooted indicator consumes a 1-D numeric series (list, NumPy array, \
             or pandas/polars Series), not a DataFrame or dict.",
        ));
    }
    column_to_vec(data, "input")
}

/// Pull `open`/`high`/`low`/`close`/`volume` columns from a `DataFrame`/`dict`
/// (only those present; `close` is required). Column names are matched
/// case-insensitively, so `Close`/`CLOSE`/`close` all work.
fn frame_to_candles(frame: &Bound<'_, PyAny>) -> PyResult<Vec<Candle>> {
    let col = |name: &str| -> PyResult<Option<Vec<f64>>> {
        let cap = {
            let mut chars = name.chars();
            chars
                .next()
                .map(|c| c.to_ascii_uppercase())
                .into_iter()
                .collect::<String>()
                + chars.as_str()
        };
        for key in [name.to_string(), cap, name.to_uppercase()] {
            if let Ok(series) = frame.get_item(&key) {
                return Ok(Some(column_to_vec(&series, name)?));
            }
        }
        Ok(None)
    };
    let close = col("close")?.ok_or_else(|| {
        PyValueError::new_err("a DataFrame/dict passed to feed() must have a 'close' column")
    })?;
    assemble_candles(
        close,
        col("open")?,
        col("high")?,
        col("low")?,
        col("volume")?,
    )
}

/// Turn a Python argument into an [`AnySource`] in the requested domain: either
/// an existing `Indicator` (cloned, so the argument stays usable) or a number
/// lifted to a constant of that domain.
/// Require a candle-rooted source. Some indicators (e.g. Keltner) read OHLC
/// bars internally, so their source must consume `Candle`s too.
fn require_candle_source(src: AnySource) -> PyResult<Source<Atom>> {
    match src {
        AnySource::Candle(s) => Ok(s),
        AnySource::Const(c) => Ok(Source::new(Value::<Atom>::new(c))),
        AnySource::Real(_) | AnySource::Snapshot(_) => Err(PyTypeError::new_err(
            "this indicator reads OHLC bars internally, so its source must be \
             candle-rooted (e.g. close()), not identity- or snapshot-rooted",
        )),
    }
}

fn coerce_operand(obj: &Bound<'_, PyAny>) -> PyResult<AnySource> {
    if let Ok(ind) = obj.cast::<PyIndicator>() {
        Ok(ind.borrow().src.clone())
    } else if let Ok(x) = obj.extract::<f64>() {
        Ok(AnySource::Const(x))
    } else {
        Err(PyTypeError::new_err(
            "expected an fugazi Indicator or a number",
        ))
    }
}

fn values_to_dict<'py>(
    py: Python<'py>,
    names: &[&str],
    values: &[Real],
) -> PyResult<Bound<'py, PyDict>> {
    let dict = PyDict::new(py);
    for (name, value) in names.iter().zip(values) {
        dict.set_item(name, value)?;
    }
    Ok(dict)
}

// ---------------------------------------------------------------------------
// Output shaping: mirror the input library, NaN warm-up
// ---------------------------------------------------------------------------

/// Where a `feed()` result should be materialised, inferred from the input.
enum OutputKind {
    /// pandas — carries the input's index to preserve alignment.
    Pandas(Py<PyAny>),
    Polars,
    /// Anything else (lists, dicts, NumPy): a NumPy array, or a plain Python
    /// container if NumPy is not importable.
    Numpy,
}

impl OutputKind {
    fn detect(data: &Bound<'_, PyAny>) -> PyResult<Self> {
        match module_root(data).as_deref() {
            Some("pandas") => Ok(OutputKind::Pandas(data.getattr("index")?.unbind())),
            Some("polars") => Ok(OutputKind::Polars),
            _ => Ok(OutputKind::Numpy),
        }
    }
}

/// The top-level package a Python object's type comes from, e.g. `"pandas"` for
/// a `DataFrame` (whose type lives in `pandas.core.frame`).
fn module_root(obj: &Bound<'_, PyAny>) -> Option<String> {
    let module: String = obj.get_type().getattr("__module__").ok()?.extract().ok()?;
    Some(module.split('.').next().unwrap_or("").to_string())
}

/// Build a numeric output series. Warm-up `None`s become `NaN`.
fn build_floats(
    py: Python<'_>,
    kind: &OutputKind,
    values: Vec<Option<f64>>,
) -> PyResult<Py<PyAny>> {
    let nums: Vec<f64> = values.iter().map(|v| v.unwrap_or(f64::NAN)).collect();
    match kind {
        OutputKind::Pandas(index) => {
            let series = py.import("pandas")?.getattr("Series")?;
            let kwargs = PyDict::new(py);
            kwargs.set_item("index", index.bind(py))?;
            Ok(series.call((nums,), Some(&kwargs))?.unbind())
        }
        OutputKind::Polars => Ok(py
            .import("polars")?
            .getattr("Series")?
            .call1((nums,))?
            .unbind()),
        OutputKind::Numpy => match py.import("numpy") {
            Ok(np) => Ok(np.getattr("asarray")?.call1((nums,))?.unbind()),
            Err(_) => Ok(values.into_pyobject(py)?.into_any().unbind()),
        },
    }
}

/// Build a boolean output series. Signals never warm up to a missing value.
fn build_bools(py: Python<'_>, kind: &OutputKind, values: Vec<bool>) -> PyResult<Py<PyAny>> {
    match kind {
        OutputKind::Pandas(index) => {
            let series = py.import("pandas")?.getattr("Series")?;
            let kwargs = PyDict::new(py);
            kwargs.set_item("index", index.bind(py))?;
            Ok(series.call((values,), Some(&kwargs))?.unbind())
        }
        OutputKind::Polars => Ok(py
            .import("polars")?
            .getattr("Series")?
            .call1((values,))?
            .unbind()),
        OutputKind::Numpy => match py.import("numpy") {
            Ok(np) => Ok(np.getattr("asarray")?.call1((values,))?.unbind()),
            Err(_) => Ok(values.into_pyobject(py)?.into_any().unbind()),
        },
    }
}

/// Build a multi-line output: a column per line. Warm-up rows become `NaN`.
fn build_multi(
    py: Python<'_>,
    kind: &OutputKind,
    names: &[&str],
    rows: Vec<Option<Vec<f64>>>,
) -> PyResult<Py<PyAny>> {
    // Transpose rows into one NaN-filled column per line.
    let columns: Vec<Vec<f64>> = (0..names.len())
        .map(|j| {
            rows.iter()
                .map(|row| row.as_ref().map_or(f64::NAN, |v| v[j]))
                .collect()
        })
        .collect();

    match kind {
        OutputKind::Pandas(index) => {
            let data = PyDict::new(py);
            for (name, col) in names.iter().zip(&columns) {
                data.set_item(name, col.as_slice())?;
            }
            let frame = py.import("pandas")?.getattr("DataFrame")?;
            let kwargs = PyDict::new(py);
            kwargs.set_item("index", index.bind(py))?;
            Ok(frame.call((data,), Some(&kwargs))?.unbind())
        }
        OutputKind::Polars => {
            let data = PyDict::new(py);
            for (name, col) in names.iter().zip(&columns) {
                data.set_item(name, col.as_slice())?;
            }
            Ok(py
                .import("polars")?
                .getattr("DataFrame")?
                .call1((data,))?
                .unbind())
        }
        OutputKind::Numpy => {
            let data = PyDict::new(py);
            let np = py.import("numpy").ok();
            for (name, col) in names.iter().zip(&columns) {
                match &np {
                    Some(np) => {
                        data.set_item(name, np.getattr("asarray")?.call1((col.as_slice(),))?)?
                    }
                    None => data.set_item(name, col.as_slice())?,
                }
            }
            Ok(data.into_any().unbind())
        }
    }
}

// ---------------------------------------------------------------------------
// Constructors
// ---------------------------------------------------------------------------

/// Wrap a candle-consuming indicator (`Input = Atom`) as a candle-rooted source.
fn candle_source<T>(inner: T) -> PyIndicator
where
    T: Indicator<Input = Atom, Output = Real> + Clone + Send + Sync + 'static,
{
    PyIndicator::wrap(AnySource::Candle(Source::new(inner)))
}

/// Every atom-input source leaf on the Python side follows the same shape:
/// zero-arg default (candle-rooted `Identity<Atom>`) or optional `source=`
/// for re-rooting onto a `PyAtomSource` (a `pick()`, typically). The result's
/// domain follows the source: an atom-rooted source stays candle-rooted, a
/// snapshot-rooted source produces a snapshot-rooted [`PyIndicator`].
macro_rules! atom_leaf_source {
    ($name:ident, $default_ctor:expr, $of_ctor:path, $doc:literal) => {
        #[doc = $doc]
        #[pyfunction]
        #[pyo3(signature = (source = None))]
        fn $name(source: Option<PyRef<'_, PyAtomSource>>) -> PyIndicator {
            match source.map(|s| s.inner.clone()) {
                None => PyIndicator::wrap(AnySource::Candle(Source::new($default_ctor))),
                Some(AnyAtomSource::Atom(s)) => {
                    PyIndicator::wrap(AnySource::Candle(Source::new($of_ctor(s))))
                }
                Some(AnyAtomSource::Snapshot(s)) => {
                    PyIndicator::wrap(AnySource::Snapshot(Source::new($of_ctor(s))))
                }
            }
        }
    };
}

/// Twin of [`atom_leaf_source!`] for the boolean signal leaves (`is_weekday`,
/// `is_weekend`).
macro_rules! atom_leaf_signal {
    ($name:ident, $default_ctor:expr, $of_ctor:path, $doc:literal) => {
        #[doc = $doc]
        #[pyfunction]
        #[pyo3(signature = (source = None))]
        fn $name(source: Option<PyRef<'_, PyAtomSource>>) -> PySignal {
            match source.map(|s| s.inner.clone()) {
                None => PySignal::wrap(AnySignal::Candle(SignalBox::new($default_ctor))),
                Some(AnyAtomSource::Atom(s)) => {
                    PySignal::wrap(AnySignal::Candle(SignalBox::new($of_ctor(s))))
                }
                Some(AnyAtomSource::Snapshot(s)) => {
                    PySignal::wrap(AnySignal::Snapshot(SignalBox::new($of_ctor(s))))
                }
            }
        }
    };
}

atom_leaf_source!(open, Open::new(), Open::of, "Source: the bar's open price.");
atom_leaf_source!(high, High::new(), High::of, "Source: the bar's high price.");
atom_leaf_source!(low, Low::new(), Low::of, "Source: the bar's low price.");
atom_leaf_source!(
    close,
    Close::new(),
    Close::of,
    "Source: the bar's close price. Pass `source=ta.pick(key)` to read a specific asset's close out of a `Snapshot`."
);
atom_leaf_source!(
    volume,
    Volume::new(),
    Volume::of,
    "Source: the bar's volume."
);
atom_leaf_source!(
    typical,
    Typical::new(),
    Typical::of,
    "Source: the bar's typical price, (high + low + close) / 3."
);
atom_leaf_source!(
    median,
    Median::new(),
    Median::of,
    "Source: the bar's median price, (high + low) / 2."
);

// Calendar accessors: each reads `atom.time` and emits the decomposed field
// (year, month, …) as a Real. `None` on bars whose `time` is `None`. Anything
// else — day-of-month == 15, hour < 9, "trading window" — is a composition
// against these numeric sources.
atom_leaf_source!(
    year,
    Year::new(),
    Year::of,
    "Source: the Gregorian year of `atom.time` (UTC), or `None` if unset."
);
atom_leaf_source!(
    month,
    Month::new(),
    Month::of,
    "Source: the Gregorian month, 1 (Jan) through 12 (Dec)."
);
atom_leaf_source!(
    day,
    Day::new(),
    Day::of,
    "Source: the day of the month, 1 through 31."
);
atom_leaf_source!(
    hour,
    Hour::new(),
    Hour::of,
    "Source: the hour of the day (UTC), 0 through 23."
);
atom_leaf_source!(
    minute,
    Minute::new(),
    Minute::of,
    "Source: the minute of the hour, 0 through 59."
);
atom_leaf_source!(
    second,
    Second::new(),
    Second::of,
    "Source: the second of the minute, 0 through 59."
);
atom_leaf_source!(
    day_of_week,
    DayOfWeek::new(),
    DayOfWeek::of,
    "Source: ISO 8601 weekday, 1 (Monday) through 7 (Sunday)."
);
atom_leaf_source!(
    day_of_year,
    DayOfYear::new(),
    DayOfYear::of,
    "Source: day of the year, 1 through 366."
);
atom_leaf_source!(
    week_of_year,
    WeekOfYear::new(),
    WeekOfYear::of,
    "Source: ISO 8601 week of the year, 1 through 53."
);
atom_leaf_source!(
    quarter,
    Quarter::new(),
    Quarter::of,
    "Source: calendar quarter, 1 through 4."
);
atom_leaf_source!(
    unix_seconds,
    UnixSeconds::new(),
    UnixSeconds::of,
    "Source: Unix seconds since the epoch (as a float)."
);
atom_leaf_source!(
    unix_millis,
    UnixMillis::new(),
    UnixMillis::of,
    "Source: Unix milliseconds since the epoch (as a float)."
);

atom_leaf_signal!(
    is_weekday,
    IsWeekday::new(),
    IsWeekday::of,
    "Signal: true on Monday through Friday, false on Saturday/Sunday. `False` on bars whose `atom.time` is `None`."
);
atom_leaf_signal!(
    is_weekend,
    IsWeekend::new(),
    IsWeekend::of,
    "Signal: true on Saturday/Sunday, false Monday through Friday. `False` on bars whose `atom.time` is `None`."
);

///// Source (atom-emitting): project one asset's `Atom` out of a `Snapshot` by
/// [`Selector`]. Compose with any atom-input leaf by passing the returned
/// `AtomSource` as its `source=` argument.
///
/// `symbol` and `freq` are the two [`Selector`] fields; both optional. Legal
/// forms:
///
/// - `pick("BTC")` / `pick(symbol="BTC")` — match by symbol, any frequency.
/// - `pick(freq="1h")` — match by frequency, any symbol.
/// - `pick(symbol="BTC", freq="1h")` — exact match.
/// - `pick()` — *no query*. Every `update` runs the [`Snapshot`] sole-atom
///   unpack: the snapshot must contain exactly one entry, otherwise the call
///   panics (translated to a Python `RuntimeError`). This is the
///   single-series ergonomic shortcut — writes cleanly for a strategy that
///   was authored assuming one asset but fed through a `Snapshot`-shaped
///   driver.
///
/// ```python
/// import fugazi as ta
/// btc_close = ta.close(source=ta.pick("BTC"))
/// spread = ta.close(ta.pick("BTC")) - ta.close(ta.pick("ETH"))
/// # Cross-frequency:
/// hourly   = ta.close(ta.pick(freq="1h"))
/// # Single-series:
/// close    = ta.close(source=ta.pick())
/// ```
#[pyfunction]
#[pyo3(signature = (symbol = None, freq = None))]
fn pick(symbol: Option<&Bound<'_, PyAny>>, freq: Option<&Bound<'_, PyAny>>) -> PyResult<PyAtomSource> {
    // Allow `pick("BTC")` alongside `pick(symbol="BTC")`: the first positional
    // arg accepts either a plain str (→ symbol) or a Selector.
    let selector = match (symbol, freq) {
        (None, None) => Selector::default(),
        (Some(s), None) => {
            // If the first arg is already a full Selector / Frequency /
            // tuple, honor it verbatim. Otherwise treat it as a symbol str.
            coerce_selector(s)?
        }
        (None, Some(f)) => Selector::by_freq(coerce_frequency(f)?),
        (Some(s), Some(f)) => {
            let sym = s.extract::<String>().map_err(|_| {
                PyTypeError::new_err(
                    "when both `symbol` and `freq` are given, `symbol` must be a str",
                )
            })?;
            Selector::exact(sym, coerce_frequency(f)?)
        }
    };
    let pick = if selector.is_empty() {
        Pick::new()
    } else {
        Pick::matching(selector)
    };
    Ok(PyAtomSource {
        inner: AnyAtomSource::Snapshot(AtomBox::new(pick)),
    })
}

/// Source: the raw value stream, passed straight through. Root an indicator
/// here (instead of a candle accessor) to consume a bare 1-D series of numbers
/// — `update(float)` and `feed([...])` rather than candles.
#[pyfunction]
fn identity() -> PyIndicator {
    PyIndicator::wrap(AnySource::Real(Source::new(Identity::new())))
}

/// Source: a constant value, ignoring the input. Mirrors Rust's `Value`, which
/// is generic over the input — so this is domain-**neutral**: in an operator it
/// adopts its partner's domain (works in both `rsi(close()).gt(value(70))` and
/// `rsi(identity()).gt(value(70))`). Used entirely on its own it is candle-
/// rooted. A bare Python number works the same way, so `gt(70)` == `gt(value(70))`.
#[pyfunction]
fn value(value: f64) -> PyIndicator {
    PyIndicator::wrap(AnySource::Const(value))
}

macro_rules! src_period {
    ($name:ident, $ty:ident, $doc:literal) => {
        #[doc = $doc]
        #[pyfunction]
        fn $name(source: PyRef<'_, PyIndicator>, period: usize) -> PyResult<PyIndicator> {
            ensure_period(period)?;
            Ok(PyIndicator::wrap(map_source!(source.src.clone(), |s| {
                $ty::new(s, period)
            })))
        }
    };
}

src_period!(sma, Sma, "Simple moving average of `source` over `period`.");
src_period!(
    ema,
    Ema,
    "Exponential moving average of `source` over `period`."
);
src_period!(
    rma,
    Rma,
    "Wilder (running) moving average of `source` over `period`."
);
src_period!(
    wma,
    Wma,
    "Weighted moving average of `source` over `period`."
);
src_period!(hma, Hma, "Hull moving average of `source` over `period`.");
src_period!(
    rsi,
    Rsi,
    "Relative strength index of `source` over `period`."
);
src_period!(
    stddev,
    StdDev,
    "Rolling standard deviation of `source` over `period`."
);
src_period!(
    stochastic,
    Stochastic,
    "Stochastic %K of `source` over `period`."
);
src_period!(
    cci,
    Cci,
    "Commodity channel index of `source` over `period`."
);

macro_rules! bar_period {
    ($name:ident, $ty:ident, $doc:literal) => {
        #[doc = $doc]
        #[pyfunction]
        fn $name(period: usize) -> PyResult<PyIndicator> {
            ensure_period(period)?;
            Ok(candle_source($ty::new(CurrentBar::new(), period)))
        }
    };
}

bar_period!(
    atr,
    Atr,
    "Average true range over `period` (consumes the full bar)."
);
bar_period!(
    mfi,
    Mfi,
    "Money-flow index over `period` (consumes the full bar)."
);
bar_period!(
    williams_r,
    WilliamsR,
    "Williams %R over `period` (consumes the full bar)."
);

macro_rules! bar_noarg {
    ($name:ident, $ty:ident, $doc:literal) => {
        #[doc = $doc]
        #[pyfunction]
        fn $name() -> PyIndicator {
            candle_source($ty::new(CurrentBar::new()))
        }
    };
}

bar_noarg!(
    obv,
    Obv,
    "On-balance volume (cumulative; reset to re-anchor)."
);
bar_noarg!(
    vwap,
    Vwap,
    "Volume-weighted average price (cumulative; reset at session boundaries)."
);
bar_noarg!(
    ad,
    Ad,
    "Chaikin accumulation/distribution line (cumulative)."
);
bar_noarg!(true_range, TrueRange, "True range of the current bar.");

macro_rules! bar_period_multi {
    ($name:ident, $ty:ident, $doc:literal) => {
        #[doc = $doc]
        #[pyfunction]
        fn $name(period: usize) -> PyResult<PyMulti> {
            ensure_period(period)?;
            Ok(PyMulti {
                inner: AnyMulti::Candle(MultiBox::new($ty::new(CurrentBar::new(), period))),
            })
        }
    };
}

bar_period_multi!(
    adx,
    Adx,
    "Average directional index: {plus_di, minus_di, adx}."
);
bar_period_multi!(dmi, Dmi, "Directional movement index: {plus_di, minus_di}.");
bar_period_multi!(aroon, Aroon, "Aroon indicator: {up, down, oscillator}.");

/// Parabolic SAR. `step` is the acceleration increment, `max` its cap.
#[pyfunction]
#[pyo3(signature = (step = 0.02, max = 0.2))]
fn sar(step: f64, max: f64) -> PyIndicator {
    candle_source(Sar::new(CurrentBar::new(), step, max))
}

/// MACD of `source`: {macd, signal, histogram}.
#[pyfunction]
#[pyo3(signature = (source, fast_period = 12, slow_period = 26, signal_period = 9))]
fn macd(
    source: PyRef<'_, PyIndicator>,
    fast_period: usize,
    slow_period: usize,
    signal_period: usize,
) -> PyResult<PyMulti> {
    ensure_period(fast_period)?;
    ensure_period(slow_period)?;
    ensure_period(signal_period)?;
    Ok(PyMulti {
        inner: map_multi!(source.src.clone(), |s| Macd::new(
            s,
            fast_period,
            slow_period,
            signal_period
        )),
    })
}

/// Bollinger bands of `source`: {upper, middle, lower}, `k` stddevs wide.
#[pyfunction]
#[pyo3(signature = (source, period = 20, k = 2.0))]
fn bollinger(source: PyRef<'_, PyIndicator>, period: usize, k: f64) -> PyResult<PyMulti> {
    ensure_period(period)?;
    Ok(PyMulti {
        inner: map_multi!(source.src.clone(), |s| Bollinger::new(s, period, k)),
    })
}

/// Keltner channels around an EMA of `source`: {upper, middle, lower}.
#[pyfunction]
#[pyo3(signature = (source, ema_period = 20, atr_period = 10, multiplier = 2.0))]
fn keltner(
    source: PyRef<'_, PyIndicator>,
    ema_period: usize,
    atr_period: usize,
    multiplier: f64,
) -> PyResult<PyMulti> {
    ensure_period(ema_period)?;
    ensure_period(atr_period)?;
    let s = require_candle_source(source.src.clone())?;
    Ok(PyMulti {
        inner: AnyMulti::Candle(MultiBox::new(Keltner::new(
            s,
            CurrentBar::new(),
            ema_period,
            atr_period,
            multiplier,
        ))),
    })
}

/// Donchian channel from a `high` source and a `low` source: {upper, middle,
/// lower}. Both sources must be rooted in the same domain.
#[pyfunction]
fn donchian(
    high: PyRef<'_, PyIndicator>,
    low: PyRef<'_, PyIndicator>,
    period: usize,
) -> PyResult<PyMulti> {
    ensure_period(period)?;
    Ok(PyMulti {
        inner: combine_multi!(high.src.clone(), low.src.clone(), |h, l| Donchian::new(
            h, l, period
        ))?,
    })
}

/// Stochastic RSI: the stochastic transform over an RSI of `source`. Sugar for
/// `stochastic(rsi(source, rsi_period), stoch_period)`.
#[pyfunction]
#[pyo3(signature = (source, rsi_period = 14, stoch_period = 14))]
fn stoch_rsi(
    source: PyRef<'_, PyIndicator>,
    rsi_period: usize,
    stoch_period: usize,
) -> PyResult<PyIndicator> {
    ensure_period(rsi_period)?;
    ensure_period(stoch_period)?;
    Ok(PyIndicator::wrap(map_source!(source.src.clone(), |s| {
        Stochastic::new(Rsi::new(s, rsi_period), stoch_period)
    })))
}

// ---------------------------------------------------------------------------
// Cross-timeframe primitives: resample + latch + stable
// ---------------------------------------------------------------------------

/// Aggregate every `every` base candles into one higher-timeframe candle and
/// run `inner` (any candle-rooted Real source — `close()`, `ema(close(), 20)`,
/// …) over that HTF stream. `inner` advances only on emissions from the
/// resample, so an EMA inside `resample` recurses over the HTF closes (not
/// the base ones); on the base ticks in between the composed source emits
/// `None`. Wrap the outermost result in `latch()` if per-base-tick reads
/// should see the finished value.
///
/// ```python
/// import fugazi as ta
/// # EMA-20 of the closes of every 4-bar candle, latched for per-base-tick reads.
/// htf_ema = ta.latch(ta.resample(4, ta.ema(ta.close(), 20)))
/// ```
#[pyfunction]
fn resample(every: usize, inner: PyRef<'_, PyIndicator>) -> PyResult<PyIndicator> {
    if every == 0 {
        return Err(PyValueError::new_err(
            "resample every must be greater than zero",
        ));
    }
    // The composition semantically feeds an HTF candle to `inner`, so `inner`
    // must be candle-rooted (or a bare constant, which we lift into the candle
    // domain — it will just ignore the bar and emit its constant on every
    // HTF boundary).
    let inner_candle = require_candle_source(inner.src.clone())?;
    Ok(PyIndicator::wrap(AnySource::Candle(Source::new(
        ResampleThen::new(every, inner_candle),
    ))))
}

/// Hold the last `Some` output of an indicator or signal, re-emitting it on
/// ticks where the source returns `None`. Domain-preserving: `latch()` of a
/// candle-rooted source is candle-rooted, of an identity-rooted signal is
/// identity-rooted, and so on. Pair with `resample()` so per-base-tick reads
/// see the finished HTF value between boundaries.
#[pyfunction]
fn latch<'py>(py: Python<'py>, source: &Bound<'py, PyAny>) -> PyResult<Py<PyAny>> {
    if let Ok(ind) = source.cast::<PyIndicator>() {
        let out = match ind.borrow().src.clone() {
            AnySource::Candle(s) => AnySource::Candle(Source::new(Latch::new(s))),
            AnySource::Real(s) => AnySource::Real(Source::new(Latch::new(s))),
            AnySource::Snapshot(s) => AnySource::Snapshot(Source::new(Latch::new(s))),
            // A latched constant is still that constant — the source never
            // emits `None`, so the latch never fires. Return as-is.
            other @ AnySource::Const(_) => other,
        };
        return Ok(PyIndicator::wrap(out).into_pyobject(py)?.into_any().unbind());
    }
    if let Ok(sig) = source.cast::<PySignal>() {
        let out = match sig.borrow().sig.clone() {
            AnySignal::Candle(s) => AnySignal::Candle(SignalBox::new(Latch::new(s))),
            AnySignal::Real(s) => AnySignal::Real(SignalBox::new(Latch::new(s))),
            AnySignal::Snapshot(s) => AnySignal::Snapshot(SignalBox::new(Latch::new(s))),
        };
        return Ok(PySignal::wrap(out).into_pyobject(py)?.into_any().unbind());
    }
    Err(PyTypeError::new_err(
        "latch() expects an fugazi Indicator or Signal",
    ))
}

/// Passthrough wrapper that forces the argument's reported `unstable_period()`
/// to `0`. Same output, same `warm_up_period()`; a downstream reader of
/// `stable_period()` no longer waits for this subtree's IIR settling tail.
/// Accepts either an `Indicator` or a `Signal` and returns the same kind. The
/// explicit opt-out of the safe default that waits for the tail:
///
/// ```python
/// # Skip the Ema's unstable tail when computing readiness.
/// src = ta.unstable(ta.ema(ta.close(), 20))
/// ```
#[pyfunction]
fn unstable(py: Python<'_>, arg: &Bound<'_, PyAny>) -> PyResult<Py<PyAny>> {
    if let Ok(ind) = arg.cast::<PyIndicator>() {
        let out = ind.borrow().unstable();
        return Ok(out.into_pyobject(py)?.into_any().unbind());
    }
    if let Ok(sig) = arg.cast::<PySignal>() {
        let out = sig.borrow().unstable();
        return Ok(out.into_pyobject(py)?.into_any().unbind());
    }
    Err(PyTypeError::new_err(
        "unstable() expects an fugazi Indicator or Signal",
    ))
}

/// Read a per-atom overlay column by its `key` in `schema`. Rooted at the
/// atom stream, so it slots into the same candle-rooted pipelines as
/// `close()`/`atr()`/etc. When fed a bare `Candle` (no overlays), the reader
/// yields `None` — pass an `Atom` carrying an `OverlayInfo` bound to the same
/// schema to see values.
///
/// **Polymorphic on the column's declared type**: a `Real` column yields an
/// `Indicator`, a `Bool` column yields a `Signal`, and a `Str` column yields
/// a `StrSource`. Use `get_real()` / `get_bool()` / `get_str()` if you want
/// to assert the returned type at the call site.
///
/// Raises `ValueError` if `key` isn't registered in `schema`.
#[pyfunction]
fn get<'py>(py: Python<'py>, schema: &PySchema, key: &str) -> PyResult<Py<PyAny>> {
    match schema.inner.type_of_key(key) {
        Some(OverlayType::Real) => {
            let ind = build_get_real(schema, key)?;
            Ok(ind.into_pyobject(py)?.into_any().unbind())
        }
        Some(OverlayType::Bool) => {
            let sig = build_get_bool(schema, key)?;
            Ok(sig.into_pyobject(py)?.into_any().unbind())
        }
        Some(OverlayType::Str) => {
            let src = build_get_str(schema, key)?;
            Ok(src.into_pyobject(py)?.into_any().unbind())
        }
        None => Err(unknown_key_error(schema, key)),
    }
}

/// Read a `Real`-typed overlay column. Always returns an `Indicator`; raises
/// `ValueError` if the column is missing or its declared type isn't `Real`.
#[pyfunction]
fn get_real(schema: &PySchema, key: &str) -> PyResult<PyIndicator> {
    if !schema.inner.contains(key) {
        return Err(unknown_key_error(schema, key));
    }
    build_get_real(schema, key)
}

/// Read a `Bool`-typed overlay column. Always returns a `Signal`; raises
/// `ValueError` if the column is missing or its declared type isn't `Bool`.
#[pyfunction]
fn get_bool(schema: &PySchema, key: &str) -> PyResult<PySignal> {
    if !schema.inner.contains(key) {
        return Err(unknown_key_error(schema, key));
    }
    build_get_bool(schema, key)
}

/// Read a `Str`-typed overlay column. Always returns a `StrSource`; raises
/// `ValueError` if the column is missing or its declared type isn't `Str`.
#[pyfunction]
fn get_str(schema: &PySchema, key: &str) -> PyResult<PyStrSource> {
    if !schema.inner.contains(key) {
        return Err(unknown_key_error(schema, key));
    }
    build_get_str(schema, key)
}

fn build_get_real(schema: &PySchema, key: &str) -> PyResult<PyIndicator> {
    let src = GetReal::try_new(&schema.inner, key).map_err(|e| PyValueError::new_err(e.to_string()))?;
    Ok(PyIndicator::wrap(AnySource::Candle(Source::new(src))))
}

fn build_get_bool(schema: &PySchema, key: &str) -> PyResult<PySignal> {
    let src = GetBool::try_new(&schema.inner, key).map_err(|e| PyValueError::new_err(e.to_string()))?;
    Ok(PySignal::wrap(AnySignal::Candle(SignalBox::new(src))))
}

fn build_get_str(schema: &PySchema, key: &str) -> PyResult<PyStrSource> {
    let src = GetStr::try_new(&schema.inner, key).map_err(|e| PyValueError::new_err(e.to_string()))?;
    Ok(PyStrSource::wrap(AnyStrSource::Candle(StrSource::new(src))))
}

/// The "unknown overlay key" error message used by `get*()` — lists the
/// registered keys so a typo is easy to spot, or hints that the caller
/// forgot to bind a schema.
fn unknown_key_error(schema: &PySchema, key: &str) -> PyErr {
    let registered: Vec<String> = schema.inner.keys().map(str::to_string).collect();
    if registered.is_empty() {
        PyValueError::new_err(format!(
            "unknown overlay key {key:?}: no columns registered on this schema"
        ))
    } else {
        PyValueError::new_err(format!(
            "unknown overlay key {key:?}. Registered columns: {}",
            registered.join(", "),
        ))
    }
}

/// A constant string source — the string twin of `value(x)`. Feeds a
/// [`ValueStr`] leaf as a `StrSource` that ignores its input and always emits
/// `s`. Usually you don't need to build one explicitly: `StrSource.eq("foo")`
/// accepts a raw Python `str` on the right-hand side and lifts internally.
#[pyfunction]
fn value_str(s: &str) -> PyStrSource {
    PyStrSource::wrap(AnyStrSource::Const(Arc::from(s)))
}

/// `lhs == rhs` on two string sources. `lhs` is a `StrSource`; `rhs` may be
/// another `StrSource` or a Python `str` (lifted to a `ValueStr` constant).
/// Returns a `Signal`.
#[pyfunction]
fn str_eq(lhs: &PyStrSource, rhs: &Bound<'_, PyAny>) -> PyResult<PySignal> {
    let rhs = coerce_str_operand(rhs)?;
    let (l, r) = str_pair(lhs.src.clone(), rhs);
    Ok(PySignal::wrap(AnySignal::Candle(SignalBox::new(
        Combine::<_, _, StrEqOp>::new(l, r),
    ))))
}

/// `lhs != rhs` on two string sources. The complement of [`str_eq`].
#[pyfunction]
fn str_ne(lhs: &PyStrSource, rhs: &Bound<'_, PyAny>) -> PyResult<PySignal> {
    let rhs = coerce_str_operand(rhs)?;
    let (l, r) = str_pair(lhs.src.clone(), rhs);
    Ok(PySignal::wrap(AnySignal::Candle(SignalBox::new(
        Combine::<_, _, StrNeOp>::new(l, r),
    ))))
}

// ---------------------------------------------------------------------------
// Remote candle sources
//
// The library-level `fugazi::sources` API takes only objects/enums; the string
// parsing that maps user-facing kwargs (`freq="1d"`, `since="2024-01-01"`) to
// those objects lives here.
// ---------------------------------------------------------------------------

/// Process-wide tokio runtime, lazily built on first use. Sharing one runtime
/// across fetch calls avoids the ~10ms startup cost of building a fresh one
/// per call and keeps the fetcher thread pool warm.
static SOURCES_RUNTIME: std::sync::OnceLock<tokio::runtime::Runtime> = std::sync::OnceLock::new();

fn sources_runtime() -> &'static tokio::runtime::Runtime {
    SOURCES_RUNTIME.get_or_init(|| {
        tokio::runtime::Builder::new_multi_thread()
            .enable_all()
            .worker_threads(2)
            .thread_name("fugazi-sources")
            .build()
            .expect("build tokio runtime")
    })
}

/// Map a fugazi [`SourceError`] to an appropriate Python exception type.
fn source_error_to_py(e: SourceError) -> PyErr {
    match e {
        SourceError::UnknownSymbol(msg) => PyValueError::new_err(format!("unknown symbol: {msg}")),
        SourceError::UnsupportedInterval(i) => {
            PyValueError::new_err(format!("unsupported interval: {i:?}"))
        }
        other => PyValueError::new_err(other.to_string()),
    }
}

/// Chosen DataFrame library for the return value of `Binance.candles()` /
/// `fugazi.get()`.
#[derive(Clone, Copy)]
enum CandlesOutput {
    Polars,
    Pandas,
    Numpy,
}

impl CandlesOutput {
    fn from_kwarg(s: &str) -> PyResult<Self> {
        match s.to_ascii_lowercase().as_str() {
            "polars" => Ok(CandlesOutput::Polars),
            "pandas" => Ok(CandlesOutput::Pandas),
            "numpy" | "dict" => Ok(CandlesOutput::Numpy),
            other => Err(PyValueError::new_err(format!(
                "output must be 'polars', 'pandas', or 'numpy' (got {other:?})"
            ))),
        }
    }
}

// -- Interval token parser (accepts `1m`, `4h`, `1d`, `1w`, `1M`) -----------

fn parse_interval_token(s: &str) -> PyResult<Interval> {
    let s = s.trim();
    if s.is_empty() {
        return Err(PyValueError::new_err("interval token is empty"));
    }
    let (num, unit) = s.split_at(s.len() - 1);
    let n: u32 = if num.is_empty() {
        1
    } else {
        num.parse()
            .map_err(|_| PyValueError::new_err(format!("invalid interval {s:?}")))?
    };
    if n == 0 {
        return Err(PyValueError::new_err(format!(
            "interval {s:?}: multiplier must be positive"
        )));
    }
    match unit {
        "m" => Ok(Interval::Minute(n)),
        "h" => Ok(Interval::Hour(n)),
        "d" => Ok(Interval::Day(n)),
        "w" => Ok(Interval::Week(n)),
        "M" => Ok(Interval::Month(n)),
        _ => Err(PyValueError::new_err(format!(
            "interval {s:?}: unknown unit {unit:?}"
        ))),
    }
}

// -- Date parser (`today` / `yesterday` / `Nd ago` / ISO / EU) --------------

fn parse_date_token(input: &str, now: time::OffsetDateTime) -> PyResult<time::OffsetDateTime> {
    let raw = input.trim();
    let lower = raw.to_ascii_lowercase();
    if lower == "today" {
        return Ok(midnight_utc(now.date()));
    }
    if lower == "yesterday" {
        return Ok(midnight_utc(now.date() - time::Duration::days(1)));
    }
    if let Some((n, unit)) = parse_relative(&lower) {
        let d = match unit {
            'd' => time::Duration::days(n as i64),
            'w' => time::Duration::weeks(n as i64),
            _ => unreachable!(),
        };
        return Ok(midnight_utc(now.date() - d));
    }
    if let Some(date) = parse_absolute(raw) {
        return Ok(midnight_utc(date));
    }
    Err(PyValueError::new_err(format!("invalid date {input:?}")))
}

fn midnight_utc(date: time::Date) -> time::OffsetDateTime {
    date.with_time(time::Time::MIDNIGHT).assume_utc()
}

fn parse_relative(s: &str) -> Option<(u32, char)> {
    let rest = s.strip_suffix("ago")?.trim_end();
    let idx = rest.find(['d', 'w'])?;
    let unit = rest.as_bytes()[idx] as char;
    if !rest[idx + 1..].trim().is_empty() {
        return None;
    }
    let n: u32 = rest[..idx].trim().parse().ok()?;
    if n == 0 {
        return None;
    }
    Some((n, unit))
}

fn parse_absolute(s: &str) -> Option<time::Date> {
    let parts: Vec<&str> = s.split('-').collect();
    if parts.len() != 3 {
        return None;
    }
    if !parts.iter().all(|p| !p.is_empty() && p.bytes().all(|b| b.is_ascii_digit())) {
        return None;
    }
    let first_len = parts[0].len();
    let (year, month, day) = if first_len == 4 {
        let y: i32 = parts[0].parse().ok()?;
        let m: u32 = parts[1].parse().ok()?;
        let d: u32 = parts[2].parse().ok()?;
        (y, m, d)
    } else if first_len == 1 || first_len == 2 {
        if parts[2].len() != 4 {
            return None;
        }
        let d: u32 = parts[0].parse().ok()?;
        let m: u32 = parts[1].parse().ok()?;
        let y: i32 = parts[2].parse().ok()?;
        (y, m, d)
    } else {
        return None;
    };
    let month = time::Month::try_from(u8::try_from(month).ok()?).ok()?;
    time::Date::from_calendar_date(year, month, u8::try_from(day).ok()?).ok()
}

fn resolve_since_until(
    since: &str,
    until: Option<&str>,
) -> PyResult<(Timestamp, Option<Timestamp>)> {
    let now = time::OffsetDateTime::now_utc();
    let since_dt = parse_date_token(since, now)?;
    let until_dt = match until {
        Some(u) => Some(parse_date_token(u, now)?),
        None => None,
    };
    if let Some(u) = until_dt
        && u <= since_dt
    {
        return Err(PyValueError::new_err(format!(
            "until ({}) must be strictly after since ({})",
            until.unwrap_or(""),
            since
        )));
    }
    Ok((
        Timestamp::from_datetime(since_dt),
        until_dt.map(Timestamp::from_datetime),
    ))
}

/// Format a UTC millisecond stamp as `YYYY-MM-DDTHH:MM:SSZ`.
fn format_ts_iso(ms: i64) -> String {
    let nanos = (ms as i128).saturating_mul(1_000_000);
    match time::OffsetDateTime::from_unix_timestamp_nanos(nanos) {
        Ok(dt) => dt
            .format(&time::format_description::well_known::Rfc3339)
            .unwrap_or_else(|_| ms.to_string()),
        Err(_) => ms.to_string(),
    }
}

/// Materialise a single-symbol/single-interval fetch into a DataFrame.
///
/// Columns: `time` (ISO 8601 UTC str), `open`/`high`/`low`/`close`/`volume`
/// (f64), then one column per source-provided overlay (Binance's
/// `quote_volume` / `n_trades` / `taker_buy_base_volume` /
/// `taker_buy_quote_volume`; Yahoo's `adj_close`) — same names as the atom
/// schema's keys. Bool / Str overlay columns land as Python-native lists;
/// Real ones as `f64` lists.
fn build_candles_frame(
    py: Python<'_>,
    output: CandlesOutput,
    atoms: Vec<Atom>,
) -> PyResult<Py<PyAny>> {
    let n = atoms.len();
    let mut times: Vec<String> = Vec::with_capacity(n);
    let mut opens: Vec<f64> = Vec::with_capacity(n);
    let mut highs: Vec<f64> = Vec::with_capacity(n);
    let mut lows: Vec<f64> = Vec::with_capacity(n);
    let mut closes: Vec<f64> = Vec::with_capacity(n);
    let mut volumes: Vec<f64> = Vec::with_capacity(n);
    // Overlay column order comes from any atom's schema (all atoms in one
    // fetch share the same `Arc<Schema>`).
    let schema = fugazi_core::sources::schema_of(&atoms);
    let n_over = schema.len();
    let mut over_real: Vec<Vec<f64>> = (0..n_over).map(|_| Vec::with_capacity(n)).collect();
    let mut over_bool: Vec<Vec<bool>> = (0..n_over).map(|_| Vec::with_capacity(n)).collect();
    let mut over_str: Vec<Vec<String>> = (0..n_over).map(|_| Vec::with_capacity(n)).collect();
    for atom in atoms {
        let time = atom
            .time
            .expect("candle source atoms always carry a time")
            .0;
        times.push(format_ts_iso(time));
        opens.push(atom.candle.open);
        highs.push(atom.candle.high);
        lows.push(atom.candle.low);
        closes.push(atom.candle.close);
        volumes.push(atom.candle.volume);
        for i in 0..n_over {
            let cell = atom.overlays.as_ref().and_then(|ov| ov.get(i));
            match schema.type_of(i).expect("schema has N columns") {
                fugazi_core::OverlayType::Real => {
                    over_real[i].push(match cell {
                        Some(OverlayValue::Real(x)) => *x,
                        _ => f64::NAN,
                    });
                }
                fugazi_core::OverlayType::Bool => {
                    over_bool[i].push(match cell {
                        Some(OverlayValue::Bool(b)) => *b,
                        _ => false,
                    });
                }
                fugazi_core::OverlayType::Str => {
                    over_str[i].push(match cell {
                        Some(OverlayValue::Str(s)) => s.to_string(),
                        _ => String::new(),
                    });
                }
            }
        }
    }
    let data = PyDict::new(py);
    data.set_item("time", &times)?;
    data.set_item("open", &opens)?;
    data.set_item("high", &highs)?;
    data.set_item("low", &lows)?;
    data.set_item("close", &closes)?;
    data.set_item("volume", &volumes)?;
    for (i, name) in schema.keys().enumerate() {
        match schema.type_of(i).expect("schema has N columns") {
            fugazi_core::OverlayType::Real => data.set_item(name, &over_real[i])?,
            fugazi_core::OverlayType::Bool => data.set_item(name, &over_bool[i])?,
            fugazi_core::OverlayType::Str => data.set_item(name, &over_str[i])?,
        }
    }
    match output {
        CandlesOutput::Polars => {
            let polars = py.import("polars").map_err(|_| {
                PyValueError::new_err(
                    "output='polars' requested but the polars package is not installed",
                )
            })?;
            Ok(polars.getattr("DataFrame")?.call1((data,))?.unbind())
        }
        CandlesOutput::Pandas => {
            let pandas = py.import("pandas").map_err(|_| {
                PyValueError::new_err(
                    "output='pandas' requested but the pandas package is not installed",
                )
            })?;
            Ok(pandas.getattr("DataFrame")?.call1((data,))?.unbind())
        }
        CandlesOutput::Numpy => Ok(data.into_any().unbind()),
    }
}

/// Fetch a single (symbol, interval) window through the shared runtime,
/// releasing the GIL for the network I/O.
fn fetch_bars<C>(
    py: Python<'_>,
    source: &C,
    symbol: &str,
    interval: Interval,
    since: Timestamp,
    until: Option<Timestamp>,
) -> PyResult<Vec<Atom>>
where
    C: CandleSource + Clone,
{
    let client = source.clone();
    let symbol = symbol.to_string();
    py.detach(|| {
        sources_runtime()
            .block_on(async move { client.atoms(&symbol, interval, since, until).await })
    })
    .map_err(source_error_to_py)
}

/// A Binance klines client.
///
/// ```python
/// b = fugazi.Binance()                  # public endpoint, defaults
/// df = b.candles(symbol="BTCUSDT", freq="1d",
///                since="2020-01-01", until="today")
/// ```
///
/// One call = one (symbol, freq) fetch = one DataFrame. Batch multiple
/// symbols or frequencies by looping in Python.
#[pyclass(name = "Binance", frozen)]
struct PyBinance {
    inner: Binance,
}

#[pymethods]
impl PyBinance {
    /// Construct a client. `base_url` overrides the API endpoint (default
    /// `https://api.binance.com`), useful for local test servers.
    #[new]
    #[pyo3(signature = (base_url = None))]
    fn new(base_url: Option<String>) -> Self {
        let mut inner = Binance::new();
        if let Some(url) = base_url {
            inner = inner.with_base_url(url);
        }
        Self { inner }
    }

    /// Fetch OHLCV candles for one `(symbol, freq)` window.
    ///
    /// * `symbol` — e.g. `"BTCUSDT"`, `"ETHEUR"`. Sent verbatim to Binance.
    /// * `freq` — bar cadence: `"1m"`/`"5m"`/`"1h"`/`"4h"`/`"1d"`/`"1w"`/`"1M"`.
    /// * `since` / `until` — dates. Formats: ISO `"YYYY-MM-DD"`, EU
    ///   `"D-M-YYYY"`, or relative (`"today"`, `"yesterday"`, `"Nd ago"`,
    ///   `"Nw ago"`). `until` is exclusive; `None` means "up to now".
    /// * `output` — `"polars"` (default), `"pandas"`, or `"numpy"` (dict of arrays).
    ///
    /// Returned DataFrame columns: `time` (ISO 8601 UTC), `open`, `high`,
    /// `low`, `close`, `volume`, plus the Binance kline extras
    /// `quote_volume`, `n_trades`, `taker_buy_base_volume`,
    /// `taker_buy_quote_volume` (all f64).
    #[pyo3(signature = (symbol, freq = "1d", since = "2020-01-01", until = None, output = "polars"))]
    fn candles(
        &self,
        py: Python<'_>,
        symbol: &str,
        freq: &str,
        since: &str,
        until: Option<&str>,
        output: &str,
    ) -> PyResult<Py<PyAny>> {
        let interval = parse_interval_token(freq)?;
        let (since_ts, until_ts) = resolve_since_until(since, until)?;
        let out = CandlesOutput::from_kwarg(output)?;
        let atoms = fetch_bars(py, &self.inner, symbol, interval, since_ts, until_ts)?;
        build_candles_frame(py, out, atoms)
    }
}

/// A Yahoo Finance chart-API client (stocks, ETFs, indices, FX).
///
/// ```python
/// y = fugazi.Yahoo()                     # public endpoint, defaults
/// df = y.candles(symbol="AAPL", freq="1d",
///                since="2020-01-01", until="today")
/// ```
///
/// One call = one (symbol, freq) fetch = one DataFrame. Batch multiple
/// symbols or frequencies by looping in Python.
#[pyclass(name = "Yahoo", frozen)]
struct PyYahoo {
    inner: Yahoo,
}

#[pymethods]
impl PyYahoo {
    /// Construct a client. `base_url` overrides the API endpoint (default
    /// `https://query1.finance.yahoo.com`), useful for local test servers;
    /// `user_agent` overrides the default `User-Agent` header Yahoo's chart
    /// endpoint requires.
    #[new]
    #[pyo3(signature = (base_url = None, user_agent = None))]
    fn new(base_url: Option<String>, user_agent: Option<String>) -> Self {
        let mut inner = Yahoo::new();
        if let Some(url) = base_url {
            inner = inner.with_base_url(url);
        }
        if let Some(ua) = user_agent {
            inner = inner.with_user_agent(ua);
        }
        Self { inner }
    }

    /// Fetch OHLCV candles for one `(symbol, freq)` window.
    ///
    /// * `symbol` — e.g. `"AAPL"`, `"^GSPC"`, `"EURUSD=X"`. Sent verbatim to Yahoo.
    /// * `freq` — bar cadence: `"1m"`/`"5m"`/`"1h"`/`"4h"`/`"1d"`/`"1w"`/`"1M"`.
    /// * `since` / `until` — dates. Formats: ISO `"YYYY-MM-DD"`, EU
    ///   `"D-M-YYYY"`, or relative (`"today"`, `"yesterday"`, `"Nd ago"`,
    ///   `"Nw ago"`). `until` is exclusive; `None` means "up to now".
    /// * `output` — `"polars"` (default), `"pandas"`, or `"numpy"` (dict of arrays).
    ///
    /// Returned DataFrame columns: `time` (ISO 8601 UTC), `open`, `high`,
    /// `low`, `close`, `volume`, plus the Yahoo extra `adj_close` — the
    /// split- and dividend-adjusted close (all f64).
    #[pyo3(signature = (symbol, freq = "1d", since = "2020-01-01", until = None, output = "polars"))]
    fn candles(
        &self,
        py: Python<'_>,
        symbol: &str,
        freq: &str,
        since: &str,
        until: Option<&str>,
        output: &str,
    ) -> PyResult<Py<PyAny>> {
        let interval = parse_interval_token(freq)?;
        let (since_ts, until_ts) = resolve_since_until(since, until)?;
        let out = CandlesOutput::from_kwarg(output)?;
        let atoms = fetch_bars(py, &self.inner, symbol, interval, since_ts, until_ts)?;
        build_candles_frame(py, out, atoms)
    }
}

/// Fetch OHLCV candles from a named provider and return a DataFrame.
///
/// ```python
/// df = fugazi.fetch(provider="binance", symbol="BTCUSDT", freq="1d",
///                   since="2020-01-01", until="today", output="polars")
/// ```
///
/// Same shape as `Binance().candles(...)` / `Yahoo().candles(...)`; the extra
/// `provider` argument dispatches to the right client (`"binance"` or
/// `"yfinance"`).
#[pyfunction]
#[pyo3(signature = (provider, symbol, freq = "1d", since = "2020-01-01", until = None, output = "polars"))]
fn fetch(
    py: Python<'_>,
    provider: &str,
    symbol: &str,
    freq: &str,
    since: &str,
    until: Option<&str>,
    output: &str,
) -> PyResult<Py<PyAny>> {
    let interval = parse_interval_token(freq)?;
    let (since_ts, until_ts) = resolve_since_until(since, until)?;
    let out = CandlesOutput::from_kwarg(output)?;
    let bars = match provider {
        "binance" => fetch_bars(py, &Binance::new(), symbol, interval, since_ts, until_ts)?,
        "yfinance" => fetch_bars(py, &Yahoo::new(), symbol, interval, since_ts, until_ts)?,
        other => {
            return Err(PyValueError::new_err(format!(
                "unknown provider {other:?}. Known providers: binance, yfinance"
            )));
        }
    };
    build_candles_frame(py, out, bars)
}

// ---------------------------------------------------------------------------
// Metrics: mirror `fugazi::metrics::*` as the `fugazi.metrics` submodule
//
// One `#[pyfunction]` per library metric, plus lightweight pyclasses over the
// public `Fill`, `Trade`, `DrawdownSegment` intermediates. Ratios that return
// `Option<Real>` in Rust map to `Optional[float]` in Python (`None` on the
// degenerate case); metrics that always return a `Real` map to plain `float`.
// Bar counts stay `int` (`usize` in Rust). Values are natural units — `0.15`
// is +15%, not `15.0`.
// ---------------------------------------------------------------------------

/// A bar-tagged order: an [`Order`] paired with the bar index at which it
/// filled. `PaperWallet.update()` returns bare `Order`s (no bar); a user
/// driving the loop tags each with its bar index to build the list that
/// `metrics.reconstruct_trades` / `metrics.exposure_ratio` consume:
///
/// ```python
/// fills = []
/// for i, candle in enumerate(candles):
///     for order in wallet.update("BTC", candle):
///         fills.append(fugazi.Fill(bar=i, order=order))
/// ```
#[pyclass(name = "Fill", frozen, from_py_object)]
#[derive(Clone)]
struct PyFill {
    inner: Fill<String>,
}

#[pymethods]
impl PyFill {
    #[new]
    fn new(bar: usize, order: &PyOrder) -> Self {
        PyFill {
            inner: Fill {
                bar,
                order: order.inner.clone(),
            },
        }
    }

    /// The bar index at which this order filled.
    #[getter]
    fn bar(&self) -> usize {
        self.inner.bar
    }

    /// The filled [`Order`].
    #[getter]
    fn order(&self) -> PyOrder {
        PyOrder {
            inner: self.inner.order.clone(),
        }
    }

    fn __repr__(&self) -> String {
        format!(
            "Fill(bar={}, order=Order(symbol='{}', side='{}', units={}, price={}, kind='{}'))",
            self.inner.bar,
            self.inner.order.symbol,
            side_str(self.inner.order.side),
            self.inner.order.units,
            self.inner.order.price,
            kind_str(self.inner.order.kind),
        )
    }
}

/// A closed round-trip trade reconstructed from the fill blotter by
/// [`reconstruct_trades`](core_metrics::reconstruct_trades). Frozen; all fields
/// are read-only.
#[pyclass(name = "Trade", frozen, from_py_object)]
#[derive(Clone, Copy)]
struct PyTrade {
    inner: Trade,
}

#[pymethods]
impl PyTrade {
    /// Bar index at which the leg was opened (or last re-averaged).
    #[getter]
    fn entry_bar(&self) -> usize {
        self.inner.entry_bar
    }
    /// Bar index at which the leg was closed.
    #[getter]
    fn exit_bar(&self) -> usize {
        self.inner.exit_bar
    }
    /// `"buy"` (long) or `"sell"` (short).
    #[getter]
    fn side(&self) -> &'static str {
        side_str(self.inner.side)
    }
    /// The magnitude of the closed leg, in instrument units.
    #[getter]
    fn units(&self) -> f64 {
        self.inner.units
    }
    /// Volume-weighted average price of the opening leg.
    #[getter]
    fn entry_price(&self) -> f64 {
        self.inner.entry_price
    }
    /// Fill price of the closing leg.
    #[getter]
    fn exit_price(&self) -> f64 {
        self.inner.exit_price
    }
    /// Realized PnL in reference (quote) currency.
    #[getter]
    fn pnl(&self) -> f64 {
        self.inner.pnl
    }
    /// PnL as a fraction of the entry notional (`pnl / (entry_price * units)`).
    #[getter]
    fn return_ratio(&self) -> f64 {
        self.inner.return_ratio
    }
    /// Bar count from entry to exit — `exit_bar - entry_bar`.
    fn bars_held(&self) -> usize {
        self.inner.bars_held()
    }

    fn __repr__(&self) -> String {
        format!(
            "Trade(entry_bar={}, exit_bar={}, side='{}', units={}, entry_price={}, \
             exit_price={}, pnl={}, return_ratio={})",
            self.inner.entry_bar,
            self.inner.exit_bar,
            side_str(self.inner.side),
            self.inner.units,
            self.inner.entry_price,
            self.inner.exit_price,
            self.inner.pnl,
            self.inner.return_ratio,
        )
    }
}

/// One drawdown segment: a peak → trough → recovery-or-end stretch where the
/// equity curve was below a prior peak. Built by
/// [`drawdown_segments`](core_metrics::drawdown_segments). Frozen.
#[pyclass(name = "DrawdownSegment", frozen, from_py_object)]
#[derive(Clone, Copy)]
struct PyDrawdownSegment {
    inner: DrawdownSegment,
}

#[pymethods]
impl PyDrawdownSegment {
    /// Bar index of the pre-drawdown peak.
    #[getter]
    fn peak_bar(&self) -> usize {
        self.inner.peak_bar
    }
    /// Bar index of the deepest point.
    #[getter]
    fn trough_bar(&self) -> usize {
        self.inner.trough_bar
    }
    /// `(peak - trough) / peak`, in fractional form; always non-negative.
    #[getter]
    fn depth_ratio(&self) -> f64 {
        self.inner.depth_ratio
    }
    /// Peak-to-trough distance in bars.
    #[getter]
    fn duration_bars(&self) -> usize {
        self.inner.duration_bars
    }
    /// Bars strictly below the peak in this segment.
    #[getter]
    fn underwater_bars(&self) -> usize {
        self.inner.underwater_bars
    }

    fn __repr__(&self) -> String {
        format!(
            "DrawdownSegment(peak_bar={}, trough_bar={}, depth_ratio={}, \
             duration_bars={}, underwater_bars={})",
            self.inner.peak_bar,
            self.inner.trough_bar,
            self.inner.depth_ratio,
            self.inner.duration_bars,
            self.inner.underwater_bars,
        )
    }
}

// -- Intermediate builders --------------------------------------------------

/// Per-bar fractional return series: `(equity[i] - prev) / prev`, seeded from
/// `initial_equity`. Zero-denominator bars contribute `0.0`. The returned list
/// has the same length as `equity_curve`.
#[pyfunction]
fn per_bar_returns(equity_curve: Vec<Real>, initial_equity: Real) -> Vec<Real> {
    core_metrics::per_bar_returns(&equity_curve, initial_equity)
}

/// Walk `fills` with a signed position and a volume-weighted entry price,
/// producing one `Trade` per closed leg. A reversal fill closes the current
/// leg and reopens the remainder at the same fill price as a fresh trade.
#[pyfunction]
fn reconstruct_trades(fills: Vec<PyFill>) -> Vec<PyTrade> {
    let native: Vec<Fill<String>> = fills.into_iter().map(|f| f.inner).collect();
    core_metrics::reconstruct_trades(&native)
        .into_iter()
        .map(|inner| PyTrade { inner })
        .collect()
}

/// Build the drawdown segments of `equity_curve` — one entry per peak →
/// trough → recovery-or-end stretch. A monotone-non-decreasing curve produces
/// an empty list.
#[pyfunction]
fn drawdown_segments(equity_curve: Vec<Real>) -> Vec<PyDrawdownSegment> {
    core_metrics::drawdown_segments(&equity_curve)
        .into_iter()
        .map(|inner| PyDrawdownSegment { inner })
        .collect()
}

// -- Return moments and distribution shape ----------------------------------

/// Arithmetic mean of `returns`. `0.0` on empty input.
#[pyfunction]
fn mean_return(returns: Vec<Real>) -> Real {
    core_metrics::mean_return(&returns)
}

/// Median of `returns`. `0.0` on empty input; the mean of the two middle
/// values on even-length input.
#[pyfunction]
fn median_return(returns: Vec<Real>) -> Real {
    core_metrics::median_return(&returns)
}

/// Sample (Bessel-corrected, `ddof=1`) standard deviation of `returns`. `0.0`
/// on empty or single-sample input.
#[pyfunction]
fn stddev_return(returns: Vec<Real>) -> Real {
    core_metrics::stddev_return(&returns)
}

/// Largest single-bar return, or `0.0` on empty input.
#[pyfunction]
fn best_return(returns: Vec<Real>) -> Real {
    core_metrics::best_return(&returns)
}

/// Smallest single-bar return, or `0.0` on empty input.
#[pyfunction]
fn worst_return(returns: Vec<Real>) -> Real {
    core_metrics::worst_return(&returns)
}

/// Fraction of bars with a strictly positive return. `0.0` on empty input.
#[pyfunction]
fn positive_bars_ratio(returns: Vec<Real>) -> Real {
    core_metrics::positive_bars_ratio(&returns)
}

/// Biased (population) skewness `g1 = m3 / m2^(3/2)`. Matches
/// `scipy.stats.skew(bias=True)`. `None` when the second moment is zero.
#[pyfunction]
fn skewness(returns: Vec<Real>) -> Option<Real> {
    core_metrics::skewness(&returns)
}

/// Biased excess kurtosis `g2 = m4 / m2^2 − 3`. Matches
/// `scipy.stats.kurtosis(bias=True, fisher=True)`. `None` when the second
/// moment is zero.
#[pyfunction]
fn kurtosis(returns: Vec<Real>) -> Option<Real> {
    core_metrics::kurtosis(&returns)
}

/// Historical VaR at `confidence` (e.g. `0.95`) as a positive loss fraction.
#[pyfunction]
fn value_at_risk(returns: Vec<Real>, confidence: Real) -> Real {
    core_metrics::value_at_risk(&returns, confidence)
}

/// Historical Conditional VaR (Expected Shortfall) at `confidence` as a
/// positive loss fraction.
#[pyfunction]
fn conditional_value_at_risk(returns: Vec<Real>, confidence: Real) -> Real {
    core_metrics::conditional_value_at_risk(&returns, confidence)
}

/// `|P95| / |P5|` — a coarse symmetry check on the tails. `None` when the
/// P5-magnitude is zero.
#[pyfunction]
fn tail_ratio(returns: Vec<Real>) -> Option<Real> {
    core_metrics::tail_ratio(&returns)
}

// -- Compound-return metrics ------------------------------------------------

/// Total return as a fraction: `(final - initial) / initial`. `0.0` when the
/// initial equity is zero.
#[pyfunction]
fn total_return(equity_curve: Vec<Real>, initial_equity: Real) -> Real {
    core_metrics::total_return(&equity_curve, initial_equity)
}

/// Compound annual growth rate as a fraction. `None` when the equity path is
/// non-positive at either endpoint, the run is empty, or `bars_per_year <= 0`.
#[pyfunction]
fn cagr(equity_curve: Vec<Real>, initial_equity: Real, bars_per_year: Real) -> Option<Real> {
    core_metrics::cagr(&equity_curve, initial_equity, bars_per_year)
}

/// Arithmetic mean of `returns` scaled by `bars_per_year`.
#[pyfunction]
fn annualized_return(returns: Vec<Real>, bars_per_year: Real) -> Real {
    core_metrics::annualized_return(&returns, bars_per_year)
}

/// Sample stddev of `returns` scaled by `sqrt(bars_per_year)`.
#[pyfunction]
fn annualized_volatility(returns: Vec<Real>, bars_per_year: Real) -> Real {
    core_metrics::annualized_volatility(&returns, bars_per_year)
}

// -- Risk-adjusted ratios ---------------------------------------------------

/// Annualized Sharpe ratio. `risk_free_rate` is the annualized rf as a
/// fraction. `None` when the annualized volatility is zero.
#[pyfunction]
fn sharpe(returns: Vec<Real>, risk_free_rate: Real, bars_per_year: Real) -> Option<Real> {
    core_metrics::sharpe(&returns, risk_free_rate, bars_per_year)
}

/// Annualized Sortino ratio (downside deviation, `n` divisor). `None` when
/// every bar clears the threshold or `returns` is empty.
#[pyfunction]
fn sortino(returns: Vec<Real>, risk_free_rate: Real, bars_per_year: Real) -> Option<Real> {
    core_metrics::sortino(&returns, risk_free_rate, bars_per_year)
}

/// Calmar ratio: `cagr / max_drawdown`. `None` when either is undefined.
#[pyfunction]
fn calmar(equity_curve: Vec<Real>, initial_equity: Real, bars_per_year: Real) -> Option<Real> {
    core_metrics::calmar(&equity_curve, initial_equity, bars_per_year)
}

/// Omega ratio at `threshold`. For an annualized rf comparison, pass the
/// per-bar rate (`rf / bars_per_year`) as `threshold`. `None` when every
/// return clears the threshold (no downside).
#[pyfunction]
fn omega(returns: Vec<Real>, threshold: Real) -> Option<Real> {
    core_metrics::omega(&returns, threshold)
}

/// Peter Martin's Ulcer Index, in fractional form. `0.0` on a monotone-
/// non-decreasing curve.
#[pyfunction]
fn ulcer_index(equity_curve: Vec<Real>) -> Real {
    core_metrics::ulcer_index(&equity_curve)
}

/// Ulcer Performance Index: `(cagr − risk_free_rate) / ulcer_index`. `None`
/// when either input is degenerate.
#[pyfunction]
fn ulcer_performance_index(
    equity_curve: Vec<Real>,
    initial_equity: Real,
    risk_free_rate: Real,
    bars_per_year: Real,
) -> Option<Real> {
    core_metrics::ulcer_performance_index(
        &equity_curve,
        initial_equity,
        risk_free_rate,
        bars_per_year,
    )
}

// -- Higher-moment / multiple-testing Sharpe corrections --------------------

/// Probabilistic Sharpe Ratio (Bailey & López de Prado, 2012): probability
/// that the true Sharpe of the return-generating process exceeds
/// `benchmark_sharpe` (annualized), given the observed Sharpe over `returns`
/// and the empirical skewness + kurtosis. `None` when the underlying Sharpe /
/// skew / kurtosis is undefined.
#[pyfunction]
fn probabilistic_sharpe(
    returns: Vec<Real>,
    risk_free_rate: Real,
    bars_per_year: Real,
    benchmark_sharpe: Real,
) -> Option<Real> {
    core_metrics::probabilistic_sharpe(&returns, risk_free_rate, bars_per_year, benchmark_sharpe)
}

/// The Probabilistic Sharpe Ratio computed from pre-aggregated statistics —
/// use when the Sharpe / skew / kurtosis are already known (e.g. a summary
/// row from a grid) and re-scanning the returns vector would be wasted work.
/// `None` propagates from any `None` input.
#[pyfunction]
fn probabilistic_sharpe_from_stats(
    sharpe_annualized: Option<Real>,
    skewness_biased: Option<Real>,
    excess_kurtosis: Option<Real>,
    n_returns: usize,
    bars_per_year: Real,
    benchmark_sharpe: Real,
) -> Option<Real> {
    core_metrics::probabilistic_sharpe_from_stats(
        sharpe_annualized,
        skewness_biased,
        excess_kurtosis,
        n_returns,
        bars_per_year,
        benchmark_sharpe,
    )
}

/// Deflated Sharpe Ratio (Bailey & López de Prado, 2014): PSR against the
/// selection-bias-adjusted benchmark `E[max SR]` across `n_trials` candidates.
/// `trial_sharpe_variance` is the variance of the annualized Sharpe estimates
/// across the trials. `None` when `n_trials < 2`, the trial variance is not
/// strictly positive, or the underlying PSR is undefined.
#[pyfunction]
fn deflated_sharpe(
    returns: Vec<Real>,
    risk_free_rate: Real,
    bars_per_year: Real,
    n_trials: usize,
    trial_sharpe_variance: Real,
) -> Option<Real> {
    core_metrics::deflated_sharpe(
        &returns,
        risk_free_rate,
        bars_per_year,
        n_trials,
        trial_sharpe_variance,
    )
}

/// The Deflated Sharpe Ratio computed from pre-aggregated statistics — the
/// stats-only twin of `deflated_sharpe`. `None` when `n_trials < 2`, the trial
/// variance is not strictly positive, or the underlying PSR is undefined.
#[pyfunction]
#[allow(clippy::too_many_arguments)]
fn deflated_sharpe_from_stats(
    sharpe_annualized: Option<Real>,
    skewness_biased: Option<Real>,
    excess_kurtosis: Option<Real>,
    n_returns: usize,
    bars_per_year: Real,
    n_trials: usize,
    trial_sharpe_variance: Real,
) -> Option<Real> {
    core_metrics::deflated_sharpe_from_stats(
        sharpe_annualized,
        skewness_biased,
        excess_kurtosis,
        n_returns,
        bars_per_year,
        n_trials,
        trial_sharpe_variance,
    )
}

// -- Drawdown metrics -------------------------------------------------------

/// Deepest drawdown in `segments`, as a fraction. `0.0` on empty input.
#[pyfunction]
fn max_drawdown(segments: Vec<PyDrawdownSegment>) -> Real {
    let native: Vec<DrawdownSegment> = segments.iter().map(|s| s.inner).collect();
    core_metrics::max_drawdown(&native)
}

/// Peak-to-trough duration of the **deepest** drawdown segment (not the
/// longest duration overall). `0` on empty input.
#[pyfunction]
fn max_drawdown_duration(segments: Vec<PyDrawdownSegment>) -> usize {
    let native: Vec<DrawdownSegment> = segments.iter().map(|s| s.inner).collect();
    core_metrics::max_drawdown_duration(&native)
}

/// Mean drawdown depth across all segments; `None` on empty input.
#[pyfunction]
fn average_drawdown(segments: Vec<PyDrawdownSegment>) -> Option<Real> {
    let native: Vec<DrawdownSegment> = segments.iter().map(|s| s.inner).collect();
    core_metrics::average_drawdown(&native)
}

/// Mean peak-to-trough duration across all segments; `None` on empty input.
#[pyfunction]
fn average_drawdown_duration(segments: Vec<PyDrawdownSegment>) -> Option<Real> {
    let native: Vec<DrawdownSegment> = segments.iter().map(|s| s.inner).collect();
    core_metrics::average_drawdown_duration(&native)
}

/// Number of drawdown segments.
#[pyfunction]
fn drawdown_count(segments: Vec<PyDrawdownSegment>) -> usize {
    let native: Vec<DrawdownSegment> = segments.iter().map(|s| s.inner).collect();
    core_metrics::drawdown_count(&native)
}

/// Fraction of bars spent below a prior peak. `0.0` when `total_bars == 0`.
#[pyfunction]
fn time_in_drawdown_ratio(segments: Vec<PyDrawdownSegment>, total_bars: usize) -> Real {
    let native: Vec<DrawdownSegment> = segments.iter().map(|s| s.inner).collect();
    core_metrics::time_in_drawdown_ratio(&native, total_bars)
}

/// `total_return / max_drawdown` — the non-annualized cousin of Calmar.
/// `None` when the max drawdown is zero.
#[pyfunction]
fn recovery_factor(equity_curve: Vec<Real>, initial_equity: Real) -> Option<Real> {
    core_metrics::recovery_factor(&equity_curve, initial_equity)
}

// -- Trade metrics ----------------------------------------------------------

fn to_native_trades(trades: Vec<PyTrade>) -> Vec<Trade> {
    trades.into_iter().map(|t| t.inner).collect()
}

/// Count of closed round-trip trades.
#[pyfunction]
fn total_trades(trades: Vec<PyTrade>) -> usize {
    core_metrics::total_trades(&to_native_trades(trades))
}

/// Count of trades with strictly positive PnL.
#[pyfunction]
fn winning_trades(trades: Vec<PyTrade>) -> usize {
    core_metrics::winning_trades(&to_native_trades(trades))
}

/// Count of trades with strictly negative PnL.
#[pyfunction]
fn losing_trades(trades: Vec<PyTrade>) -> usize {
    core_metrics::losing_trades(&to_native_trades(trades))
}

/// Count of trades with exactly zero PnL.
#[pyfunction]
fn flat_trades(trades: Vec<PyTrade>) -> usize {
    core_metrics::flat_trades(&to_native_trades(trades))
}

/// Count of trades entered on the long side.
#[pyfunction]
fn long_trades(trades: Vec<PyTrade>) -> usize {
    core_metrics::long_trades(&to_native_trades(trades))
}

/// Count of trades entered on the short side.
#[pyfunction]
fn short_trades(trades: Vec<PyTrade>) -> usize {
    core_metrics::short_trades(&to_native_trades(trades))
}

/// Longest consecutive run of winning trades. `0` on empty input.
#[pyfunction]
fn max_consecutive_wins(trades: Vec<PyTrade>) -> usize {
    core_metrics::max_consecutive_wins(&to_native_trades(trades))
}

/// Longest consecutive run of losing trades. `0` on empty input.
#[pyfunction]
fn max_consecutive_losses(trades: Vec<PyTrade>) -> usize {
    core_metrics::max_consecutive_losses(&to_native_trades(trades))
}

/// Fraction of trades with strictly positive PnL. `None` on empty input.
#[pyfunction]
fn win_rate(trades: Vec<PyTrade>) -> Option<Real> {
    core_metrics::win_rate(&to_native_trades(trades))
}

/// `Σ winning_pnl / |Σ losing_pnl|`. `None` when there are no losing trades.
#[pyfunction]
fn profit_factor(trades: Vec<PyTrade>) -> Option<Real> {
    core_metrics::profit_factor(&to_native_trades(trades))
}

/// `average_win / |average_loss|`. `None` when either input is undefined.
#[pyfunction]
fn payoff_ratio(trades: Vec<PyTrade>) -> Option<Real> {
    core_metrics::payoff_ratio(&to_native_trades(trades))
}

/// Mean PnL per trade. `None` on empty input.
#[pyfunction]
fn expectancy(trades: Vec<PyTrade>) -> Option<Real> {
    core_metrics::expectancy(&to_native_trades(trades))
}

/// Kelly-optimal fraction of bankroll per trade under the current win rate
/// and payoff ratio (`p − (1 − p)/b`). Can be negative. `None` when either
/// input is undefined or the payoff ratio is non-positive.
#[pyfunction]
fn kelly_fraction(trades: Vec<PyTrade>) -> Option<Real> {
    core_metrics::kelly_fraction(&to_native_trades(trades))
}

/// Mean PnL across winning trades. `None` when there are no winners.
#[pyfunction]
fn average_win(trades: Vec<PyTrade>) -> Option<Real> {
    core_metrics::average_win(&to_native_trades(trades))
}

/// Mean PnL across losing trades (a negative number). `None` when there are
/// no losers.
#[pyfunction]
fn average_loss(trades: Vec<PyTrade>) -> Option<Real> {
    core_metrics::average_loss(&to_native_trades(trades))
}

/// Largest single-trade PnL. `None` on empty input.
#[pyfunction]
fn largest_win(trades: Vec<PyTrade>) -> Option<Real> {
    core_metrics::largest_win(&to_native_trades(trades))
}

/// Most-negative single-trade PnL. `None` on empty input.
#[pyfunction]
fn largest_loss(trades: Vec<PyTrade>) -> Option<Real> {
    core_metrics::largest_loss(&to_native_trades(trades))
}

/// Mean per-trade return as a fraction of the entry notional. `None` on empty
/// input.
#[pyfunction]
fn average_trade_return(trades: Vec<PyTrade>) -> Option<Real> {
    core_metrics::average_trade_return(&to_native_trades(trades))
}

/// Mean bars-held across trades. `None` on empty input.
#[pyfunction]
fn average_bars_held(trades: Vec<PyTrade>) -> Option<Real> {
    core_metrics::average_bars_held(&to_native_trades(trades))
}

/// Shortest bars-held across trades. `None` on empty input.
#[pyfunction]
fn min_bars_held(trades: Vec<PyTrade>) -> Option<usize> {
    core_metrics::min_bars_held(&to_native_trades(trades))
}

/// Longest bars-held across trades. `None` on empty input.
#[pyfunction]
fn max_bars_held(trades: Vec<PyTrade>) -> Option<usize> {
    core_metrics::max_bars_held(&to_native_trades(trades))
}

/// Fraction of bars during which the wallet held a non-zero position. `0.0`
/// when `total_bars == 0`.
#[pyfunction]
fn exposure_ratio(fills: Vec<PyFill>, total_bars: usize) -> Real {
    let native: Vec<Fill<String>> = fills.into_iter().map(|f| f.inner).collect();
    core_metrics::exposure_ratio(&native, total_bars)
}

/// Register every metric function on the `fugazi.metrics` submodule.
fn register_metrics_module(m: &Bound<'_, PyModule>) -> PyResult<()> {
    m.add_class::<PyTrade>()?;
    m.add_class::<PyDrawdownSegment>()?;

    macro_rules! reg {
        ($($f:ident),* $(,)?) => { $( m.add_function(wrap_pyfunction!($f, m)?)?; )* };
    }
    reg!(
        per_bar_returns,
        reconstruct_trades,
        drawdown_segments,
        mean_return,
        median_return,
        stddev_return,
        best_return,
        worst_return,
        positive_bars_ratio,
        skewness,
        kurtosis,
        value_at_risk,
        conditional_value_at_risk,
        tail_ratio,
        total_return,
        cagr,
        annualized_return,
        annualized_volatility,
        sharpe,
        sortino,
        calmar,
        omega,
        ulcer_index,
        ulcer_performance_index,
        probabilistic_sharpe,
        probabilistic_sharpe_from_stats,
        deflated_sharpe,
        deflated_sharpe_from_stats,
        max_drawdown,
        max_drawdown_duration,
        average_drawdown,
        average_drawdown_duration,
        drawdown_count,
        time_in_drawdown_ratio,
        recovery_factor,
        total_trades,
        winning_trades,
        losing_trades,
        flat_trades,
        long_trades,
        short_trades,
        max_consecutive_wins,
        max_consecutive_losses,
        win_rate,
        profit_factor,
        payoff_ratio,
        expectancy,
        kelly_fraction,
        average_win,
        average_loss,
        largest_win,
        largest_loss,
        average_trade_return,
        average_bars_held,
        min_bars_held,
        max_bars_held,
        exposure_ratio,
    );
    Ok(())
}

// ---------------------------------------------------------------------------
// Module
// ---------------------------------------------------------------------------

#[pymodule]
fn fugazi(m: &Bound<'_, PyModule>) -> PyResult<()> {
    m.add_class::<PyCandle>()?;
    m.add_class::<PySchema>()?;
    m.add_class::<PySchemaBuilder>()?;
    m.add_class::<PyOverlayInfo>()?;
    m.add_class::<PyAtom>()?;
    m.add_class::<PyFrequency>()?;
    m.add_class::<PySelector>()?;
    m.add_class::<PySnapshot>()?;
    m.add_class::<PyAtomSource>()?;
    m.add_class::<PyIndicator>()?;
    m.add_class::<PySignal>()?;
    m.add_class::<PyStrSource>()?;
    m.add_class::<PyMulti>()?;
    m.add_class::<PySharedMulti>()?;
    m.add_class::<PyWallet>()?;
    m.add_class::<PyOrder>()?;
    m.add_class::<PySize>()?;
    m.add_class::<PyFill>()?;
    m.add_class::<PyBinance>()?;
    m.add_class::<PyYahoo>()?;

    m.add("DEFAULT_EPSILON", DEFAULT_EPSILON)?;

    macro_rules! reg {
        ($($f:ident),* $(,)?) => { $( m.add_function(wrap_pyfunction!($f, m)?)?; )* };
    }
    reg!(
        open, high, low, close, volume, typical, median, identity, value, value_str, sma, ema, rma,
        wma, hma, rsi, stddev, stochastic, cci, atr, mfi, williams_r, obv, vwap, ad, true_range,
        adx, dmi, aroon, sar, macd, bollinger, keltner, donchian, stoch_rsi, resample, latch,
        unstable, get, get_real, get_bool, get_str, str_eq, str_ne, fetch,
        // Calendar accessors + weekday/weekend signals; consume `atom.time`.
        year, month, day, hour, minute, second, day_of_week, day_of_year, week_of_year, quarter,
        unix_seconds, unix_millis, is_weekday, is_weekend,
        // Cross-asset: project one asset's Atom out of a Snapshot by key.
        pick,
    );

    // `fugazi.metrics` — mirror of `fugazi::metrics::*`. Registered as a
    // submodule *and* injected into `sys.modules` so `from fugazi.metrics
    // import sharpe` works (pyo3 submodules aren't visible to Python's import
    // machinery by default).
    let metrics = PyModule::new(m.py(), "metrics")?;
    register_metrics_module(&metrics)?;
    m.add_submodule(&metrics)?;
    m.py()
        .import("sys")?
        .getattr("modules")?
        .set_item("fugazi.metrics", &metrics)?;

    Ok(())
}
