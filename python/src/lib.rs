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
//! An indicator is rooted either at a candle accessor ([`Current`], `Input =
//! Candle`) or at [`Identity`] (a raw value stream, `Input = Real`); the
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

use fugazi_core::Indicator;
use fugazi_core::indicators::compare::{EqOp, GeOp, GtOp, LeOp, LtOp, NeOp};
use fugazi_core::indicators::{
    Ad, Adx, AdxValue, Aroon, AroonValue, Atr, Bollinger, BollingerValue, Cci, Current, Dmi,
    DmiValue, Donchian, DonchianValue, Ema, Hma, Identity, Keltner, KeltnerValue, Macd, MacdValue,
    Mfi, Obv, Rma, Rsi, Sar, Sma, StdDev, Stochastic, TrueRange, Value, Vwap, WilliamsR, Wma,
};
use fugazi_core::indicators::{BoolIndicatorExt, Combine, DEFAULT_EPSILON, IndicatorExt};
use fugazi_core::strategy::{
    Order, PaperWallet, Quantity, Reference, Side, Size, Wallet, WalletError,
};
use fugazi_core::types::{Candle, Real};

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
    fn reset(&mut self) {
        self.0.reset()
    }
}

/// Object-safe shim over an `I`-input boolean indicator (a signal). Exposes the
/// warmed-up `bool` directly (`false` until ready), as the Python API expects.
trait DynSignal<I>: Send + Sync {
    fn update(&mut self, input: I) -> bool;
    fn is_true(&self) -> bool;
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
    fn reset(&mut self) {
        Indicator::reset(self)
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
    Candle(Source<Candle>),
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
    Candle(Source<Candle>, Source<Candle>),
    Real(Source<Real>, Source<Real>),
}

/// Resolve two sources to a shared domain so they can be combined. A neutral
/// constant adopts its partner's domain; a genuine candle-vs-value clash is an
/// error. Two constants default to the candle domain (either is equivalent —
/// they ignore input).
fn pair(lhs: AnySource, rhs: AnySource) -> PyResult<Pair> {
    fn cval(c: Real) -> Source<Candle> {
        Source::new(Value::<Candle>::new(c))
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
    Candle(SignalBox<Candle>),
    Real(SignalBox<Real>),
}

impl AnySignal {
    fn is_true(&self) -> bool {
        match self {
            AnySignal::Candle(s) => BoolIndicatorExt::is_true(s),
            AnySignal::Real(s) => BoolIndicatorExt::is_true(s),
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
    Candle(MultiBox<Candle>),
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
                let $s = Source::<Candle>::new(Value::<Candle>::new(c));
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
                let $s = Source::<Candle>::new(Value::<Candle>::new(c));
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
                let $s = Source::<Candle>::new(Value::<Candle>::new(c));
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
            AnySource::Candle(s) => Ok(Indicator::update(s, extract_candle(sample)?)),
            AnySource::Real(s) => Ok(Indicator::update(s, extract_real(sample)?)),
            // A bare constant defaults to candle-rooted; it ignores the bar.
            AnySource::Const(c) => {
                extract_candle(sample)?;
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
                .map(|c| Indicator::update(s, c))
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
                Ok(Indicator::update(s, extract_candle(sample)?).unwrap_or(false))
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
                .map(|c| Indicator::update(s, c).unwrap_or(false))
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
            AnyMulti::Candle(m) => m.0.update(extract_candle(sample)?),
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
                .map(|c| m.0.update(c))
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

/// A filled order: `symbol`, `side` ("buy"/"sell"), and a positive `quantity`.
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
    fn quantity(&self) -> f64 {
        self.inner.quantity
    }
    /// `+quantity` for a buy, `-quantity` for a sell.
    fn signed_quantity(&self) -> f64 {
        self.inner.signed_quantity()
    }
    fn __repr__(&self) -> String {
        format!(
            "Order(symbol='{}', side='{}', quantity={})",
            self.inner.symbol,
            self.side(),
            self.inner.quantity
        )
    }
}

/// A paper-trading wallet a strategy trades into: funds, per-symbol positions,
/// the prices fed to it, and a blotter of executed orders. (The live-broker
/// counterpart would be a separate wallet type implementing the same interface.)
///
/// Feed each symbol's price every tick with `update(symbol, price)`; the wallet
/// is otherwise market-agnostic. `set(symbol, side, size)` targets an absolute
/// position (an opposite-side `set` reverses; `Size.value_frac(1.0)` is all-in),
/// `set_position(symbol, target)` drives to an absolute unit count, and `close`
/// flattens. Each returns the resulting `Order`, or `None` if nothing traded,
/// and raises `ValueError` if the movement is impossible (no/zero price, or a
/// buy beyond available funds).
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

    /// Whether no positions are currently held.
    fn is_flat(&self) -> bool {
        self.inner.is_flat()
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

    /// Forget the blotter history (positions, prices and funds are untouched).
    fn clear_blotter(&mut self) {
        self.inner.clear_blotter();
    }

    /// Mark-to-market equity: funds plus each position valued at its fed price.
    fn equity(&self) -> f64 {
        self.inner.equity().0
    }

    /// Feed `symbol`'s current price. Call this each tick before trading or
    /// reading `equity`.
    fn update(&mut self, symbol: String, price: f64) {
        self.inner.update(symbol, Reference(price));
    }

    /// Drive the position in `symbol` to `target` signed units.
    fn set_position(&mut self, symbol: String, target: f64) -> PyResult<Option<PyOrder>> {
        wrap_order(self.inner.set_position(Quantity {
            symbol,
            amount: target,
        }))
    }

    /// Set the target position in `symbol` to `side` `size`.
    fn set(
        &mut self,
        symbol: String,
        side: &str,
        size: &Bound<'_, PyAny>,
    ) -> PyResult<Option<PyOrder>> {
        wrap_order(
            self.inner
                .set(symbol, parse_side(side)?, coerce_size(size)?),
        )
    }

    /// Flatten `symbol`.
    fn close(&mut self, symbol: String) -> PyResult<Option<PyOrder>> {
        wrap_order(self.inner.close(symbol))
    }
}

/// Map a wallet result to Python: an order, `None`, or a `ValueError`.
fn wrap_order(result: Result<Option<Order<String>>, WalletError>) -> PyResult<Option<PyOrder>> {
    match result {
        Ok(order) => Ok(order.map(|inner| PyOrder { inner })),
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
fn require_candle_source(src: AnySource) -> PyResult<Source<Candle>> {
    match src {
        AnySource::Candle(s) => Ok(s),
        AnySource::Const(c) => Ok(Source::new(Value::<Candle>::new(c))),
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

/// Wrap a candle-consuming indicator as a candle-rooted source.
fn candle_source<T>(inner: T) -> PyIndicator
where
    T: Indicator<Input = Candle, Output = Real> + Clone + Send + Sync + 'static,
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
    ($name:ident, $ty:ty, $doc:literal) => {
        #[doc = $doc]
        #[pyfunction]
        fn $name(period: usize) -> PyResult<PyIndicator> {
            ensure_period(period)?;
            Ok(candle_source(<$ty>::new(period)))
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
    ($name:ident, $ty:ty, $doc:literal) => {
        #[doc = $doc]
        #[pyfunction]
        fn $name() -> PyIndicator {
            candle_source(<$ty>::new())
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
    ($name:ident, $ty:ty, $doc:literal) => {
        #[doc = $doc]
        #[pyfunction]
        fn $name(period: usize) -> PyResult<PyMulti> {
            ensure_period(period)?;
            Ok(PyMulti {
                inner: AnyMulti::Candle(MultiBox::new(<$ty>::new(period))),
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
    candle_source(Sar::new(step, max))
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
            s, ema_period, atr_period, multiplier,
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
// Module
// ---------------------------------------------------------------------------

#[pymodule]
fn fugazi(m: &Bound<'_, PyModule>) -> PyResult<()> {
    m.add_class::<PyCandle>()?;
    m.add_class::<PyIndicator>()?;
    m.add_class::<PySignal>()?;
    m.add_class::<PyMulti>()?;
    m.add_class::<PyWallet>()?;
    m.add_class::<PyOrder>()?;
    m.add_class::<PySize>()?;

    m.add("DEFAULT_EPSILON", DEFAULT_EPSILON)?;

    macro_rules! reg {
        ($($f:ident),* $(,)?) => { $( m.add_function(wrap_pyfunction!($f, m)?)?; )* };
    }
    reg!(
        open, high, low, close, volume, typical, median, identity, value, sma, ema, rma, wma, hma,
        rsi, stddev, stochastic, cci, atr, mfi, williams_r, obv, vwap, ad, true_range, adx, dmi,
        aroon, sar, macd, bollinger, keltner, donchian, stoch_rsi,
    );
    Ok(())
}
