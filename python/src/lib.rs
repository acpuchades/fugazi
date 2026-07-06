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

use std::sync::Arc;

use fugazi_core::Indicator;
use fugazi_core::indicators::compare::{EqOp, GeOp, GtOp, LeOp, LtOp, NeOp};
use fugazi_core::indicators::{
    Ad, Adx, AdxValue, Aroon, AroonValue, Atr, Bollinger, BollingerValue, Cci, Current, CurrentBar,
    Dmi, DmiValue, Donchian, DonchianValue, Ema, Get, Hma, Identity, Keltner, KeltnerValue, Latch,
    Macd, MacdValue, Mfi, Obv, Resample, Rma, Rsi, Sar, Sma, Stable, StdDev, Stochastic, TrueRange,
    Value, Vwap, WilliamsR, Wma,
};
use fugazi_core::indicators::{BoolIndicatorExt, Combine, DEFAULT_EPSILON, IndicatorExt};
use fugazi_core::sources::{Binance, CandleSource, Interval, SourceError, Timestamp};
use fugazi_core::strategy::{
    Ack, Order, OrderKind, PaperWallet, Units, Reference, Side, Size, Wallet, WalletError,
};
use fugazi_core::types::{Atom, Candle, OverlayInfo, Real, Schema, SchemaBuilder};

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
}

impl<I, T> DynMulti<I> for T
where
    T: Indicator<Input = I> + Send + Sync + 'static,
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
        T: Indicator<Input = I> + Send + Sync + 'static,
        T::Output: MultiOutput,
    {
        MultiBox(Box::new(inner))
    }
}

// ---------------------------------------------------------------------------
// Input domain: the runtime tag recovering the erased `Input` type
// ---------------------------------------------------------------------------

/// A scalar source erased to one of the two input domains, plus a third
/// domain-**neutral** case for a constant. A constant reads no input (it mirrors
/// Rust's `Value<I>`, generic over the input), so it carries no domain of its
/// own and instead adopts its partner's when composed — see [`pair`]. Used
/// entirely on its own it behaves as candle-rooted.
#[derive(Clone)]
enum AnySource {
    Candle(Source<Atom>),
    Real(Source<Real>),
    Const(Real),
}

impl AnySource {
    fn value(&self) -> Option<Real> {
        match self {
            AnySource::Candle(s) => Indicator::value(s),
            AnySource::Real(s) => Indicator::value(s),
            AnySource::Const(c) => Some(*c),
        }
    }
    fn warm_up_period(&self) -> usize {
        match self {
            AnySource::Candle(s) => Indicator::warm_up_period(s),
            AnySource::Real(s) => Indicator::warm_up_period(s),
            AnySource::Const(_) => 0,
        }
    }
    fn unstable_period(&self) -> usize {
        match self {
            AnySource::Candle(s) => Indicator::unstable_period(s),
            AnySource::Real(s) => Indicator::unstable_period(s),
            AnySource::Const(_) => 0,
        }
    }
    fn reset(&mut self) {
        match self {
            AnySource::Candle(s) => Indicator::reset(s),
            AnySource::Real(s) => Indicator::reset(s),
            AnySource::Const(_) => {}
        }
    }
}

/// Two sources resolved to a common concrete domain, with any neutral constant
/// materialised to match its partner.
enum Pair {
    Candle(Source<Atom>, Source<Atom>),
    Real(Source<Real>, Source<Real>),
}

/// Resolve two sources to a shared domain so they can be combined. A neutral
/// constant adopts its partner's domain; a genuine candle-vs-value clash is an
/// error. Two constants default to the candle domain (either is equivalent —
/// they ignore input).
fn pair(lhs: AnySource, rhs: AnySource) -> PyResult<Pair> {
    fn cval(c: Real) -> Source<Atom> {
        Source::new(Value::<Atom>::new(c))
    }
    fn rval(c: Real) -> Source<Real> {
        Source::new(Value::<Real>::new(c))
    }
    match (lhs, rhs) {
        (AnySource::Candle(a), AnySource::Candle(b)) => Ok(Pair::Candle(a, b)),
        (AnySource::Real(a), AnySource::Real(b)) => Ok(Pair::Real(a, b)),
        (AnySource::Const(a), AnySource::Candle(b)) => Ok(Pair::Candle(cval(a), b)),
        (AnySource::Candle(a), AnySource::Const(b)) => Ok(Pair::Candle(a, cval(b))),
        (AnySource::Const(a), AnySource::Real(b)) => Ok(Pair::Real(rval(a), b)),
        (AnySource::Real(a), AnySource::Const(b)) => Ok(Pair::Real(a, rval(b))),
        (AnySource::Const(a), AnySource::Const(b)) => Ok(Pair::Candle(cval(a), cval(b))),
        (AnySource::Candle(_), AnySource::Real(_)) | (AnySource::Real(_), AnySource::Candle(_)) => {
            Err(domain_mismatch())
        }
    }
}

/// A boolean signal erased to one of the two input domains.
#[derive(Clone)]
enum AnySignal {
    Candle(SignalBox<Atom>),
    Real(SignalBox<Real>),
}

impl AnySignal {
    fn is_true(&self) -> bool {
        match self {
            AnySignal::Candle(s) => BoolIndicatorExt::is_true(s),
            AnySignal::Real(s) => BoolIndicatorExt::is_true(s),
        }
    }
    fn warm_up_period(&self) -> usize {
        match self {
            AnySignal::Candle(s) => Indicator::warm_up_period(s),
            AnySignal::Real(s) => Indicator::warm_up_period(s),
        }
    }
    fn unstable_period(&self) -> usize {
        match self {
            AnySignal::Candle(s) => Indicator::unstable_period(s),
            AnySignal::Real(s) => Indicator::unstable_period(s),
        }
    }
    fn reset(&mut self) {
        match self {
            AnySignal::Candle(s) => Indicator::reset(s),
            AnySignal::Real(s) => Indicator::reset(s),
        }
    }
}

/// A multi-output indicator erased to one of the two input domains.
enum AnyMulti {
    Candle(MultiBox<Atom>),
    Real(MultiBox<Real>),
}

impl AnyMulti {
    fn names(&self) -> &'static [&'static str] {
        match self {
            AnyMulti::Candle(m) => m.0.names(),
            AnyMulti::Real(m) => m.0.names(),
        }
    }
    fn value(&self) -> Option<Vec<Real>> {
        match self {
            AnyMulti::Candle(m) => m.0.value(),
            AnyMulti::Real(m) => m.0.value(),
        }
    }
    fn warm_up_period(&self) -> usize {
        match self {
            AnyMulti::Candle(m) => m.0.warm_up_period(),
            AnyMulti::Real(m) => m.0.warm_up_period(),
        }
    }
    fn unstable_period(&self) -> usize {
        match self {
            AnyMulti::Candle(m) => m.0.unstable_period(),
            AnyMulti::Real(m) => m.0.unstable_period(),
        }
    }
    fn reset(&mut self) {
        match self {
            AnyMulti::Candle(m) => m.0.reset(),
            AnyMulti::Real(m) => m.0.reset(),
        }
    }
}

fn domain_mismatch() -> PyErr {
    PyTypeError::new_err(
        "cannot combine a candle-rooted indicator with a value-rooted (identity) one; \
         both operands must be rooted in the same domain",
    )
}

/// Apply a source-wrapping constructor to a source, preserving its domain. A
/// neutral constant defaults to the candle domain.
macro_rules! map_source {
    ($src:expr, |$s:ident| $build:expr) => {
        match $src {
            AnySource::Candle($s) => AnySource::Candle(Source::new($build)),
            AnySource::Real($s) => AnySource::Real(Source::new($build)),
            AnySource::Const(c) => {
                let $s = Source::<Atom>::new(Value::<Atom>::new(c));
                AnySource::Candle(Source::new($build))
            }
        }
    };
}

/// Combine two sources into a new source; resolves a constant against its
/// partner, errors on a genuine candle-vs-value clash.
macro_rules! combine_sources {
    ($lhs:expr, $rhs:expr, |$l:ident, $r:ident| $build:expr) => {
        pair($lhs, $rhs).map(|p| match p {
            Pair::Candle($l, $r) => AnySource::Candle(Source::new($build)),
            Pair::Real($l, $r) => AnySource::Real(Source::new($build)),
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
            AnySource::Const(c) => {
                let $s = Source::<Atom>::new(Value::<Atom>::new(c));
                AnySignal::Candle(SignalBox::new($build))
            }
        }
    };
}

/// Turn two sources into a signal; resolves a constant against its partner,
/// errors on a genuine candle-vs-value clash.
macro_rules! sources_to_signal {
    ($lhs:expr, $rhs:expr, |$l:ident, $r:ident| $build:expr) => {
        pair($lhs, $rhs).map(|p| match p {
            Pair::Candle($l, $r) => AnySignal::Candle(SignalBox::new($build)),
            Pair::Real($l, $r) => AnySignal::Real(SignalBox::new($build)),
        })
    };
}

/// Transform one signal, preserving its domain.
macro_rules! map_signal {
    ($sig:expr, |$s:ident| $build:expr) => {
        match $sig {
            AnySignal::Candle($s) => AnySignal::Candle(SignalBox::new($build)),
            AnySignal::Real($s) => AnySignal::Real(SignalBox::new($build)),
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
            AnySource::Const(c) => {
                let $s = Source::<Atom>::new(Value::<Atom>::new(c));
                AnyMulti::Candle(MultiBox::new($build))
            }
        }
    };
}

/// Wrap two sources in a multi-output constructor; resolves a constant against
/// its partner, errors on a genuine candle-vs-value clash.
macro_rules! combine_multi {
    ($lhs:expr, $rhs:expr, |$l:ident, $r:ident| $build:expr) => {
        pair($lhs, $rhs).map(|p| match p {
            Pair::Candle($l, $r) => AnyMulti::Candle(MultiBox::new($build)),
            Pair::Real($l, $r) => AnyMulti::Real(MultiBox::new($build)),
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

/// An immutable name→index registry that binds an [`OverlayInfo`]'s values
/// array to the columns a `get()` indicator references. Built with
/// `SchemaBuilder().add("key").finish()` — the frozen handle is what
/// indicators and overlays share.
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

    /// All registered column names, unordered.
    fn keys(&self) -> Vec<String> {
        self.inner.keys().map(str::to_string).collect()
    }

    fn __repr__(&self) -> String {
        let mut keys: Vec<String> = self.inner.keys().map(str::to_string).collect();
        keys.sort();
        format!("Schema(keys={keys:?})")
    }
}

/// Mutable builder for a [`Schema`]. Add columns with `add()` (idempotent —
/// a repeated key returns the existing index), then freeze into an immutable
/// [`Schema`] with `finish()`.
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

    /// Register `key`. Returns the assigned column index; a repeated key
    /// returns the previously-assigned index without adding a slot.
    fn add(&mut self, key: &str) -> PyResult<usize> {
        self.inner
            .as_mut()
            .ok_or_else(|| {
                PyValueError::new_err("SchemaBuilder has already been finished")
            })
            .map(|b| b.add(key.to_string()))
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

/// Per-atom overlay values, bound to a shared [`Schema`]. Construct as
/// `OverlayInfo(schema, values)` where `values` is a list of floats whose
/// length matches `len(schema)`.
///
/// The internal `Rc<[Real]>` (per-atom, non-atomic refcount) makes this
/// class `unsendable` — it's confined to the Python thread that created it.
/// This is fine under the GIL and keeps overlay clones cheap in the hot
/// per-bar loop.
#[pyclass(name = "OverlayInfo", frozen, unsendable, skip_from_py_object)]
#[derive(Clone)]
struct PyOverlayInfo {
    inner: OverlayInfo,
}

#[pymethods]
impl PyOverlayInfo {
    #[new]
    fn new(schema: &PySchema, values: Vec<Real>) -> PyResult<Self> {
        if values.len() != schema.inner.len() {
            return Err(PyValueError::new_err(format!(
                "values length ({}) must match schema length ({})",
                values.len(),
                schema.inner.len(),
            )));
        }
        Ok(Self {
            inner: OverlayInfo::new(schema.inner.clone(), values),
        })
    }

    fn __len__(&self) -> usize {
        self.inner.values().len()
    }

    /// Read the value at a resolved column index (`None` if out of bounds).
    fn get(&self, index: usize) -> Option<Real> {
        self.inner.get(index)
    }

    /// Read the value by column name (`None` if the key isn't registered).
    fn get_by_key(&self, key: &str) -> Option<Real> {
        self.inner.get_by_key(key)
    }

    fn __repr__(&self) -> String {
        format!("OverlayInfo(values={:?})", self.inner.values())
    }
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
    #[new]
    #[pyo3(signature = (candle, overlays = None))]
    fn new(candle: &PyCandle, overlays: Option<&PyOverlayInfo>) -> Self {
        let atom = match overlays {
            Some(ov) => Atom::with_overlays(candle.inner, ov.inner.clone()),
            None => Atom::new(candle.inner),
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

    fn __repr__(&self) -> String {
        match &self.inner.overlays {
            Some(ov) => format!(
                "Atom(candle={:?}, overlays={:?})",
                self.inner.candle,
                ov.values(),
            ),
            None => format!("Atom(candle={:?})", self.inner.candle),
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
        }
    }

    /// Compute the signal over a whole series at once, returning one boolean per
    /// bar. `data` is the same as for [`Indicator.feed`](PyIndicator): a
    /// DataFrame/dict of OHLCV columns for a candle-rooted signal, or a 1-D
    /// series for an identity-rooted one. The output mirrors the input: a
    /// boolean pandas/polars `Series`, otherwise a boolean NumPy `ndarray`. Fed
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
        AnySource::Real(_) => Err(PyTypeError::new_err(
            "this indicator reads OHLC bars internally, so its source must be \
             candle-rooted (e.g. close()), not identity-rooted",
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

macro_rules! leaf {
    ($name:ident, $ctor:expr, $doc:literal) => {
        #[doc = $doc]
        #[pyfunction]
        fn $name() -> PyIndicator {
            candle_source($ctor)
        }
    };
}

leaf!(open, Current::open(), "Source: the bar's open price.");
leaf!(high, Current::high(), "Source: the bar's high price.");
leaf!(low, Current::low(), "Source: the bar's low price.");
leaf!(close, Current::close(), "Source: the bar's close price.");
leaf!(volume, Current::volume(), "Source: the bar's volume.");
leaf!(
    typical,
    Current::typical(),
    "Source: the bar's typical price, (high + low + close) / 3."
);
leaf!(
    median,
    Current::median(),
    "Source: the bar's median price, (high + low) / 2."
);

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
        };
        return Ok(PySignal::wrap(out).into_pyobject(py)?.into_any().unbind());
    }
    Err(PyTypeError::new_err(
        "latch() expects an fugazi Indicator or Signal",
    ))
}

/// A bool signal that flips `True` once its argument has been fed at least its
/// `stable_period()` samples — i.e. "the signal is past its unstable tail".
/// The `Stable` check doesn't hold the signal (it snapshots `stable_period()`
/// at construction and counts samples itself), so it composes cheaply into an
/// `all()` / `and_()` alongside the same signal to gate a strategy on
/// stability:
///
/// ```python
/// entry = ta.close().crosses_above(ta.ema(ta.close(), 20))
/// gated = entry.and_(ta.stable(entry))
/// ```
#[pyfunction]
fn stable(signal: PyRef<'_, PySignal>) -> PySignal {
    PySignal::wrap(match &signal.sig {
        AnySignal::Candle(s) => AnySignal::Candle(SignalBox::new(Stable::<Atom>::from_source(s))),
        AnySignal::Real(s) => AnySignal::Real(SignalBox::new(Stable::<Real>::from_source(s))),
    })
}

/// Read a per-atom overlay column by its `key` in `schema`. Rooted at the
/// atom stream, so it slots into the same candle-rooted pipelines as
/// `close()`/`atr()`/etc. When fed a bare `Candle` (no overlays), the reader
/// yields `None` — pass an `Atom` carrying an `OverlayInfo` bound to the same
/// schema to see values.
///
/// Raises `ValueError` if `key` isn't registered in `schema`.
#[pyfunction]
fn get(schema: &PySchema, key: &str) -> PyResult<PyIndicator> {
    let source = Get::try_new(&schema.inner, key).map_err(|e| PyValueError::new_err(e.key))?;
    Ok(PyIndicator::wrap(AnySource::Candle(Source::new(source))))
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
/// Columns: `time` (ISO 8601 UTC str), `open`/`high`/`low`/`close`/`volume` (f64).
fn build_candles_frame(
    py: Python<'_>,
    output: CandlesOutput,
    bars: Vec<fugazi_core::TimedCandle>,
) -> PyResult<Py<PyAny>> {
    let n = bars.len();
    let mut times: Vec<String> = Vec::with_capacity(n);
    let mut opens: Vec<f64> = Vec::with_capacity(n);
    let mut highs: Vec<f64> = Vec::with_capacity(n);
    let mut lows: Vec<f64> = Vec::with_capacity(n);
    let mut closes: Vec<f64> = Vec::with_capacity(n);
    let mut volumes: Vec<f64> = Vec::with_capacity(n);
    for tc in bars {
        times.push(format_ts_iso(tc.time.0));
        opens.push(tc.candle.open);
        highs.push(tc.candle.high);
        lows.push(tc.candle.low);
        closes.push(tc.candle.close);
        volumes.push(tc.candle.volume);
    }
    let data = PyDict::new(py);
    data.set_item("time", &times)?;
    data.set_item("open", &opens)?;
    data.set_item("high", &highs)?;
    data.set_item("low", &lows)?;
    data.set_item("close", &closes)?;
    data.set_item("volume", &volumes)?;
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
fn fetch_bars(
    py: Python<'_>,
    binance: &Binance,
    symbol: &str,
    interval: Interval,
    since: Timestamp,
    until: Option<Timestamp>,
) -> PyResult<Vec<fugazi_core::TimedCandle>> {
    let client = binance.clone();
    let symbol = symbol.to_string();
    py.detach(|| {
        sources_runtime()
            .block_on(async move { client.candles(&symbol, interval, since, until).await })
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
    /// `low`, `close`, `volume` (all f64).
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
        let bars = fetch_bars(py, &self.inner, symbol, interval, since_ts, until_ts)?;
        build_candles_frame(py, out, bars)
    }
}

/// Fetch OHLCV candles from a named provider and return a DataFrame.
///
/// ```python
/// df = fugazi.get(provider="binance", symbol="BTCUSDT", freq="1d",
///                 since="2020-01-01", until="today", output="polars")
/// ```
///
/// Same shape as `Binance().candles(...)`; the extra `provider` argument
/// dispatches to the right client (`"binance"` is currently the only value).
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
    let client = match provider {
        "binance" => Binance::new(),
        other => {
            return Err(PyValueError::new_err(format!(
                "unknown provider {other:?}. Known providers: binance"
            )));
        }
    };
    let bars = fetch_bars(py, &client, symbol, interval, since_ts, until_ts)?;
    build_candles_frame(py, out, bars)
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
    m.add_class::<PyIndicator>()?;
    m.add_class::<PySignal>()?;
    m.add_class::<PyMulti>()?;
    m.add_class::<PyWallet>()?;
    m.add_class::<PyOrder>()?;
    m.add_class::<PySize>()?;
    m.add_class::<PyBinance>()?;

    m.add("DEFAULT_EPSILON", DEFAULT_EPSILON)?;

    macro_rules! reg {
        ($($f:ident),* $(,)?) => { $( m.add_function(wrap_pyfunction!($f, m)?)?; )* };
    }
    reg!(
        open, high, low, close, volume, typical, median, identity, value, sma, ema, rma, wma, hma,
        rsi, stddev, stochastic, cci, atr, mfi, williams_r, obv, vwap, ad, true_range, adx, dmi,
        aroon, sar, macd, bollinger, keltner, donchian, stoch_rsi, resample, latch, stable, get,
        fetch,
    );
    Ok(())
}
