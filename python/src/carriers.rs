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
// Shared type-erasure vocabulary (fugazi::runtime::chain)
//
// Every wrapper in Python that used to hold its own erased trait object
// (Source<I> for Real, SignalBox<I> for bool, StrSource<I> for Arc<str>,
// AtomBox<I> for Atom) is one `runtime::Chain<In, Out>` — a
// `Box<dyn DynIndicator<In, Out>>`, which is `Send + Sync + Clone` (what pyo3
// pyclass fields need) and implements `Indicator<Input = In, Output = Out>`
// directly.
//
// PERFORMANCE — this is the hot boundary, and the reason `Chain` exists.
// These carriers used to hold a `Box<dyn PayloadIndicatorSync>` exchanging
// `PayloadValue`, an enum 88 bytes wide (as wide as its `Atom` variant). Every
// Python-built expression erases at *every* level — `sma(identity(), 10)` is
// two boxes — so a sample crossed that 88-byte boundary once per level in each
// direction, plus a discriminant branch and drop glue. Measured
// (`cargo bench --bench erasure`): +12.9 ns/sample per level of nesting with a
// payload, +0.4 ns with a `Chain`. Do not reintroduce a payload enum here to
// recover self-description; the domain is already in the type, and the `Any*`
// enums below are how the bindings recover it when they need to.
//
// The one exception is `MultiBox<I>` / `DynMulti<I>`: multi-output indicators
// emit a value struct (MacdValue, BollingerValue, …) that maps to `Vec<Real>`
// + `&'static [&'static str]` at the Python boundary. It stays local because
// it needs the *names* alongside the values, which no generic carrier carries.
// ---------------------------------------------------------------------------

/// A boxed `I -> Real` indicator. Semantics match the library: `None` until
/// warm, `Some(Real)` afterwards — no bool-signal-style flattening.
pub(crate) type Source<I> = runtime::Chain<I, Real>;

/// A boxed `I`-input signal (bool-out) that adds the "always-Some" semantics
/// Python's bool combinators depend on: warm-up `None` on the underlying source
/// is flattened to `Some(false)` at every update/value read, so a `.not_()` of a
/// warming-up signal reads as `true` (matching the Python API's promise that a
/// signal has a definite `bool` at every step).
///
/// The only carrier that is still a newtype, and only for that flattening.
pub(crate) struct SignalBox<I>(pub(crate) runtime::Chain<I, bool>);

impl<I: 'static> SignalBox<I> {
    pub(crate) fn new<T>(inner: T) -> Self
    where
        T: Indicator<Input = I, Output = bool> + Clone + Send + Sync + 'static,
    {
        Self(runtime::erase(inner))
    }
}

impl<I: 'static> Clone for SignalBox<I> {
    fn clone(&self) -> Self {
        Self(self.0.clone())
    }
}

impl<I> Indicator for SignalBox<I> {
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

/// A boxed `I -> Arc<str>` indicator — the string twin of `Source<I>`.
///
/// Backs the `GetStr` overlay-column reader and the `ValueStr` string
/// constant leaf, which compose into `str_eq` / `str_ne` signals.
pub(crate) type StrSource<I> = runtime::Chain<I, Arc<str>>;

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
    Atom(Arc<Mutex<SharedMultiCell<Atom>>>),
    Real(Arc<Mutex<SharedMultiCell<Real>>>),
    Snapshot(Arc<Mutex<SharedMultiCell<Snapshot<Symbol>>>>),
}

impl AnySharedMulti {
    pub(crate) fn names(&self) -> &'static [&'static str] {
        match self {
            AnySharedMulti::Atom(c) => c.lock().expect("mutex poisoned").names,
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
            AnySharedMulti::Atom(cell) => PyIndicator {
                src: AnySource::Atom(runtime::erase(SharedProjector::<Atom> {
                    cell: Arc::clone(cell),
                    field_index: idx,
                    local_gen: cell.lock().expect("mutex poisoned").generation,
                    last_value: None,
                })),
            },
            AnySharedMulti::Real(cell) => PyIndicator {
                src: AnySource::Real(runtime::erase(SharedProjector::<Real> {
                    cell: Arc::clone(cell),
                    field_index: idx,
                    local_gen: cell.lock().expect("mutex poisoned").generation,
                    last_value: None,
                })),
            },
            AnySharedMulti::Snapshot(cell) => PyIndicator {
                src: AnySource::Snapshot(runtime::erase(SharedProjector::<Snapshot<Symbol>> {
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
    /// Reads the **bar only** — 40 bytes, `Copy`, no drop glue. Where every
    /// candle field and bar indicator belongs, and the point of P1: an
    /// `Atom`-rooted chain costs 125.8 instructions per bar against this one's
    /// ~22, almost all of it moving and dropping 88-byte atoms to read 40 bytes
    /// of them. See `docs/PERFORMANCE.md`.
    Candle(Source<Candle>),
    /// Reads the bar **and its side channels** (`time`, `overlays`) — the
    /// overlay readers (`get*`) and the calendar leaves. A `Candle` chain lifts
    /// into this one ([`atom_over_candle`]); the reverse is impossible, since a
    /// bare candle carries neither a timestamp nor an overlay bundle.
    Atom(Source<Atom>),
    Real(Source<Real>),
    Snapshot(Source<Snapshot<Symbol>>),
    Const(Real),
}

impl AnySource {
    pub(crate) fn value(&self) -> Option<Real> {
        match self {
            AnySource::Candle(s) => Indicator::value(s),
            AnySource::Atom(s) => Indicator::value(s),
            AnySource::Real(s) => Indicator::value(s),
            AnySource::Snapshot(s) => Indicator::value(s),
            AnySource::Const(c) => Some(*c),
        }
    }
    pub(crate) fn warm_up_bars(&self) -> usize {
        match self {
            AnySource::Candle(s) => Indicator::warm_up_bars(s),
            AnySource::Atom(s) => Indicator::warm_up_bars(s),
            AnySource::Real(s) => Indicator::warm_up_bars(s),
            AnySource::Snapshot(s) => Indicator::warm_up_bars(s),
            AnySource::Const(_) => 0,
        }
    }
    pub(crate) fn unstable_bars(&self) -> usize {
        match self {
            AnySource::Candle(s) => Indicator::unstable_bars(s),
            AnySource::Atom(s) => Indicator::unstable_bars(s),
            AnySource::Real(s) => Indicator::unstable_bars(s),
            AnySource::Snapshot(s) => Indicator::unstable_bars(s),
            AnySource::Const(_) => 0,
        }
    }
    pub(crate) fn reset(&mut self) {
        match self {
            AnySource::Candle(s) => Indicator::reset(s),
            AnySource::Atom(s) => Indicator::reset(s),
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
        let py = data.py();
        Ok(match self {
            // The bar-only arm: candles go straight in, so no `Atom` is built,
            // moved through the vtable or dropped. This is where P1's win lands.
            AnySource::Candle(s) => {
                let cols = columns_from_frame(data)?;
                let mut out = Vec::with_capacity(cols.len(py));
                cols.for_each(py, |c| out.push(Indicator::update(s, c)));
                out
            }
            // Streamed, not collected: see `CandleColumns`. The `Vec<Candle>`
            // this used to build was 8 MB for a 200 000-bar frame, written and
            // read back for nothing.
            AnySource::Atom(s) => {
                let cols = columns_from_frame(data)?;
                let mut out = Vec::with_capacity(cols.len(py));
                cols.for_each(py, |c| out.push(Indicator::update(s, c.into())));
                out
            }
            AnySource::Real(s) => {
                let xs = reals_from_series(data)?;
                let mut out = Vec::with_capacity(xs.len(py));
                xs.for_each(py, |x| out.push(Indicator::update(s, x)));
                out
            }
            AnySource::Snapshot(s) => snapshots_from_sequence(data)?
                .into_iter()
                .map(|snap| Indicator::update(s, snap))
                .collect(),
            // A constant reads no input, but the frame still fixes the row count.
            AnySource::Const(c) => vec![Some(*c); columns_from_frame(data)?.len(py)],
        })
    }

    /// The same fold as [`feed_rows`](Self::feed_rows), writing each value
    /// **straight into a NumPy buffer** instead of collecting a
    /// `Vec<Option<Real>>` for someone else to copy out.
    ///
    /// This is the whole of `feed()`'s output path for the normal case (NumPy
    /// importable, which is every real deployment). `feed_rows` stays for the
    /// no-NumPy fallback, where the `Option`s must survive to become a Python
    /// list of `None`s rather than being flattened to `NaN`.
    ///
    /// Warm-up `None` becomes `NaN` here, at the point of production — the same
    /// convention `ndarray_from_values` applies, just without the round trip.
    pub(crate) fn feed_into_numpy<'py>(
        &mut self,
        py: Python<'py>,
        data: &Bound<'py, PyAny>,
    ) -> PyResult<Bound<'py, PyAny>> {
        // Each arm parses its input first (that is what fixes the row count),
        // then allocates. The cells are walked with an iterator rather than an
        // index so the write is not bounds-checked per sample; `cells.next()`
        // cannot run dry, because the count came from the same parsed input.
        match self {
            AnySource::Candle(s) => {
                let cols = columns_from_frame(data)?;
                numpy_filled(py, cols.len(py), |slice| {
                    let mut cells = slice.iter();
                    cols.for_each(py, |c| {
                        if let Some(cell) = cells.next() {
                            cell.set(Indicator::update(s, c).unwrap_or(Real::NAN));
                        }
                    });
                })
            }
            AnySource::Atom(s) => {
                let cols = columns_from_frame(data)?;
                numpy_filled(py, cols.len(py), |slice| {
                    let mut cells = slice.iter();
                    cols.for_each(py, |c| {
                        if let Some(cell) = cells.next() {
                            cell.set(Indicator::update(s, c.into()).unwrap_or(Real::NAN));
                        }
                    });
                })
            }
            AnySource::Real(s) => {
                let xs = reals_from_series(data)?;
                numpy_filled(py, xs.len(py), |slice| {
                    let mut cells = slice.iter();
                    xs.for_each(py, |x| {
                        if let Some(cell) = cells.next() {
                            cell.set(Indicator::update(s, x).unwrap_or(Real::NAN));
                        }
                    });
                })
            }
            AnySource::Snapshot(s) => {
                let snaps = snapshots_from_sequence(data)?;
                numpy_filled(py, snaps.len(), |slice| {
                    for (cell, snap) in slice.iter().zip(snaps) {
                        cell.set(Indicator::update(s, snap).unwrap_or(Real::NAN));
                    }
                })
            }
            AnySource::Const(c) => {
                let v = *c;
                let len = columns_from_frame(data)?.len(py);
                numpy_filled(py, len, |slice| {
                    for cell in slice {
                        cell.set(v);
                    }
                })
            }
        }
    }
}

/// One field of a [`Candle`], read straight off the bar.
///
/// The bar-domain twin of core's `Field`, which cannot serve here because it
/// requires an `S: Indicator<Output = Atom>` source — there is no
/// `Candle -> Real` accessor in the core. `ta.close()` and friends default to
/// this, so the most common root in the Python API stays in the cheap domain;
/// `ta.close(source=...)` still builds core's atom- or snapshot-rooted `Field`.
///
/// The markers below delegate to the same public `Candle` accessors core's own
/// markers do, so no formula is duplicated — `typical()` and `median()` in
/// particular are core's methods, not reimplementations.
#[derive(Debug, Clone)]
pub(crate) struct BarField<F: 'static> {
    value: Option<Real>,
    _field: std::marker::PhantomData<fn() -> F>,
}

impl<F: 'static> BarField<F> {
    pub(crate) fn new() -> Self {
        Self {
            value: None,
            _field: std::marker::PhantomData,
        }
    }
}

impl<F> Indicator for BarField<F>
where
    F: fugazi_core::indicators::CandleField + Send + Sync + 'static,
{
    type Input = Candle;
    type Output = Real;
    fn update(&mut self, input: Candle) -> Option<Real> {
        self.value = Some(F::get(&input));
        self.value
    }
    fn value(&self) -> Option<Real> {
        self.value
    }
    fn warm_up_bars(&self) -> usize {
        1
    }
    fn reset(&mut self) {
        self.value = None;
    }
}

macro_rules! bar_field_marker {
    ($name:ident, $get:expr) => {
        #[derive(Debug, Clone, Copy)]
        pub(crate) struct $name;
        impl fugazi_core::indicators::CandleField for $name {
            fn get(candle: &Candle) -> Real {
                let f: fn(&Candle) -> Real = $get;
                f(candle)
            }
        }
    };
}

bar_field_marker!(BarOpen, |c| c.open);
bar_field_marker!(BarHigh, |c| c.high);
bar_field_marker!(BarLow, |c| c.low);
bar_field_marker!(BarClose, |c| c.close);
bar_field_marker!(BarVolume, |c| c.volume);
bar_field_marker!(BarTypical, |c| c.typical());
bar_field_marker!(BarMedian, |c| c.median());

/// Lift a bar-only chain into the atom domain, so the two can be combined.
///
/// The only sound direction. A `Candle` chain fed an `Atom` just reads the bar
/// out of it; an `Atom` chain fed a bare `Candle` would be missing the `time`
/// and `overlays` it exists to read.
///
/// `candle` is `None` for an **overlay-only** atom — a series that is not a price
/// at all. The bar-only chain reads `None` for that bar, matching what
/// `CurrentBar` already does with the same input.
#[derive(Clone)]
pub(crate) struct AtomOverCandle<Out: 'static> {
    inner: runtime::Chain<Candle, Out>,
    value: Option<Out>,
}

impl<Out: Clone + 'static> Indicator for AtomOverCandle<Out> {
    type Input = Atom;
    type Output = Out;
    fn update(&mut self, input: Atom) -> Option<Out> {
        self.value = input.candle.and_then(|c| self.inner.update(c));
        self.value.clone()
    }
    fn value(&self) -> Option<Out> {
        self.value.clone()
    }
    fn warm_up_bars(&self) -> usize {
        self.inner.warm_up_bars()
    }
    fn unstable_bars(&self) -> usize {
        self.inner.unstable_bars()
    }
    fn reset(&mut self) {
        self.inner.reset();
        self.value = None;
    }
}

/// Lift a bar-only **signal** into the atom domain. The `SignalBox` wrapper is
/// rebuilt around the lifted chain so the warming-`None`-to-`false` flattening
/// is preserved.
pub(crate) fn atom_signal_over_candle(s: SignalBox<Candle>) -> SignalBox<Atom> {
    SignalBox(atom_over_candle(s.0))
}

/// Lift `inner` from the bar domain into the atom domain. See [`AtomOverCandle`].
pub(crate) fn atom_over_candle<Out: Clone + Send + Sync + 'static>(
    inner: runtime::Chain<Candle, Out>,
) -> runtime::Chain<Atom, Out> {
    runtime::erase(AtomOverCandle { inner, value: None })
}

/// Two sources resolved to a common concrete domain, with any neutral constant
/// materialised to match its partner.
pub(crate) enum Pair {
    Candle(Source<Candle>, Source<Candle>),
    Atom(Source<Atom>, Source<Atom>),
    Real(Source<Real>, Source<Real>),
    Snapshot(Source<Snapshot<Symbol>>, Source<Snapshot<Symbol>>),
}

/// Resolve two sources to a shared domain so they can be combined. A neutral
/// constant adopts its partner's domain; a genuine candle-vs-value-vs-snapshot
/// clash is an error. Two constants default to the candle domain (either is
/// equivalent — they ignore input).
pub(crate) fn pair(lhs: AnySource, rhs: AnySource) -> PyResult<Pair> {
    fn rval(c: Real) -> Source<Real> {
        runtime::erase(Value::<Real>::new(c))
    }
    fn sval(c: Real) -> Source<Snapshot<Symbol>> {
        runtime::erase(Value::<Snapshot<Symbol>>::new(c))
    }
    fn cval(c: Real) -> Source<Candle> {
        runtime::erase(Value::<Candle>::new(c))
    }
    match (lhs, rhs) {
        (AnySource::Candle(a), AnySource::Candle(b)) => Ok(Pair::Candle(a, b)),
        (AnySource::Atom(a), AnySource::Atom(b)) => Ok(Pair::Atom(a, b)),
        // Mixed bar/atom: lift the bar side up. Rejecting instead would break
        // `close().add(get_real(schema, "adj"))`, which worked when both were one
        // domain — see the cross-domain tests in `python/tests/test_fugazi.py`.
        (AnySource::Candle(a), AnySource::Atom(b)) => Ok(Pair::Atom(atom_over_candle(a), b)),
        (AnySource::Atom(a), AnySource::Candle(b)) => Ok(Pair::Atom(a, atom_over_candle(b))),
        (AnySource::Const(a), AnySource::Candle(b)) => Ok(Pair::Candle(cval(a), b)),
        (AnySource::Candle(a), AnySource::Const(b)) => Ok(Pair::Candle(a, cval(b))),
        (AnySource::Real(a), AnySource::Real(b)) => Ok(Pair::Real(a, b)),
        (AnySource::Snapshot(a), AnySource::Snapshot(b)) => Ok(Pair::Snapshot(a, b)),
        (AnySource::Const(a), AnySource::Atom(b)) => Ok(Pair::Atom(const_to_atom_source(a), b)),
        (AnySource::Atom(a), AnySource::Const(b)) => Ok(Pair::Atom(a, const_to_atom_source(b))),
        (AnySource::Const(a), AnySource::Real(b)) => Ok(Pair::Real(rval(a), b)),
        (AnySource::Real(a), AnySource::Const(b)) => Ok(Pair::Real(a, rval(b))),
        (AnySource::Const(a), AnySource::Snapshot(b)) => Ok(Pair::Snapshot(sval(a), b)),
        (AnySource::Snapshot(a), AnySource::Const(b)) => Ok(Pair::Snapshot(a, sval(b))),
        (AnySource::Const(a), AnySource::Const(b)) => {
            Ok(Pair::Atom(const_to_atom_source(a), const_to_atom_source(b)))
        }
        // Genuine clashes: a value stream against a bar stream, or either
        // against a multi-symbol snapshot. Bar-vs-atom is *not* one of these.
        (AnySource::Atom(_) | AnySource::Candle(_), AnySource::Real(_))
        | (AnySource::Real(_), AnySource::Atom(_) | AnySource::Candle(_))
        | (AnySource::Atom(_) | AnySource::Candle(_), AnySource::Snapshot(_))
        | (AnySource::Snapshot(_), AnySource::Atom(_) | AnySource::Candle(_))
        | (AnySource::Real(_), AnySource::Snapshot(_))
        | (AnySource::Snapshot(_), AnySource::Real(_)) => Err(domain_mismatch()),
    }
}

/// A boolean signal erased to one of the three input domains.
#[derive(Clone)]
pub(crate) enum AnySignal {
    /// Bar-only, the signal twin of [`AnySource::Candle`].
    Candle(SignalBox<Candle>),
    Atom(SignalBox<Atom>),
    Real(SignalBox<Real>),
    Snapshot(SignalBox<Snapshot<Symbol>>),
}

impl AnySignal {
    pub(crate) fn is_true(&self) -> bool {
        match self {
            AnySignal::Candle(s) => BoolIndicatorExt::is_true(s),
            AnySignal::Atom(s) => BoolIndicatorExt::is_true(s),
            AnySignal::Real(s) => BoolIndicatorExt::is_true(s),
            AnySignal::Snapshot(s) => BoolIndicatorExt::is_true(s),
        }
    }
    pub(crate) fn warm_up_bars(&self) -> usize {
        match self {
            AnySignal::Candle(s) => Indicator::warm_up_bars(s),
            AnySignal::Atom(s) => Indicator::warm_up_bars(s),
            AnySignal::Real(s) => Indicator::warm_up_bars(s),
            AnySignal::Snapshot(s) => Indicator::warm_up_bars(s),
        }
    }
    pub(crate) fn unstable_bars(&self) -> usize {
        match self {
            AnySignal::Candle(s) => Indicator::unstable_bars(s),
            AnySignal::Atom(s) => Indicator::unstable_bars(s),
            AnySignal::Real(s) => Indicator::unstable_bars(s),
            AnySignal::Snapshot(s) => Indicator::unstable_bars(s),
        }
    }
    pub(crate) fn reset(&mut self) {
        match self {
            AnySignal::Candle(s) => Indicator::reset(s),
            AnySignal::Atom(s) => Indicator::reset(s),
            AnySignal::Real(s) => Indicator::reset(s),
            AnySignal::Snapshot(s) => Indicator::reset(s),
        }
    }

    /// Dispatch a frame of samples through the domain the signal lives in,
    /// producing one `bool` per bar. `SignalBox` flattens the warm-up `None`
    /// to `false` at the source, so an unwrap-or-`false` in the loop mirrors
    /// what the runtime already guarantees for individual updates.
    pub(crate) fn feed_rows(&mut self, data: &Bound<'_, PyAny>) -> PyResult<Vec<bool>> {
        let py = data.py();
        Ok(match self {
            // The bar-only arm: candles go straight in. No `Atom` is built,
            // moved or dropped — which is the whole point of the domain.
            AnySignal::Candle(s) => {
                let cols = columns_from_frame(data)?;
                let mut out = Vec::with_capacity(cols.len(py));
                cols.for_each(py, |c| out.push(Indicator::update(s, c).unwrap_or(false)));
                out
            }
            AnySignal::Atom(s) => {
                let cols = columns_from_frame(data)?;
                let mut out = Vec::with_capacity(cols.len(py));
                cols.for_each(py, |c| {
                    out.push(Indicator::update(s, c.into()).unwrap_or(false))
                });
                out
            }
            AnySignal::Real(s) => {
                let xs = reals_from_series(data)?;
                let mut out = Vec::with_capacity(xs.len(py));
                xs.for_each(py, |x| out.push(Indicator::update(s, x).unwrap_or(false)));
                out
            }
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
    Atom(StrSource<Atom>),
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
            AnyStrSource::Atom(s) => Indicator::value(s),
            AnyStrSource::Snapshot(s) => Indicator::value(s),
            AnyStrSource::Const(c) => Some(c.clone()),
        }
    }
    pub(crate) fn warm_up_bars(&self) -> usize {
        match self {
            AnyStrSource::Atom(s) => Indicator::warm_up_bars(s),
            AnyStrSource::Snapshot(s) => Indicator::warm_up_bars(s),
            AnyStrSource::Const(_) => 0,
        }
    }
    pub(crate) fn unstable_bars(&self) -> usize {
        match self {
            AnyStrSource::Atom(s) => Indicator::unstable_bars(s),
            AnyStrSource::Snapshot(s) => Indicator::unstable_bars(s),
            AnyStrSource::Const(_) => 0,
        }
    }
    pub(crate) fn reset(&mut self) {
        match self {
            AnyStrSource::Atom(s) => Indicator::reset(s),
            AnyStrSource::Snapshot(s) => Indicator::reset(s),
            AnyStrSource::Const(_) => {}
        }
    }
}

/// Two string sources resolved to the candle domain, with any neutral constant
/// materialised via [`ValueStr`]. Both sides end up as `StrSource<Atom>`.
pub(crate) enum StrPair {
    Atom(StrSource<Atom>, StrSource<Atom>),
    Snapshot(StrSource<Snapshot<Symbol>>, StrSource<Snapshot<Symbol>>),
}

pub(crate) fn str_pair(lhs: AnyStrSource, rhs: AnyStrSource) -> PyResult<StrPair> {
    use AnyStrSource as A;
    fn lift_atom(c: Arc<str>) -> StrSource<Atom> {
        runtime::erase(ValueStr::<Atom>::new(c))
    }
    fn lift_snapshot(c: Arc<str>) -> StrSource<Snapshot<Symbol>> {
        runtime::erase(ValueStr::<Snapshot<Symbol>>::new(c))
    }
    Ok(match (lhs, rhs) {
        (A::Atom(l), A::Atom(r)) => StrPair::Atom(l, r),
        (A::Snapshot(l), A::Snapshot(r)) => StrPair::Snapshot(l, r),
        // A neutral constant adopts its partner's domain, exactly as on the
        // Real side — so `str_eq(get_str(.., source=pick("M")), "bull")` works
        // without the caller having to say which domain the literal is in.
        (A::Atom(l), A::Const(c)) => StrPair::Atom(l, lift_atom(c)),
        (A::Const(c), A::Atom(r)) => StrPair::Atom(lift_atom(c), r),
        (A::Snapshot(l), A::Const(c)) => StrPair::Snapshot(l, lift_snapshot(c)),
        (A::Const(c), A::Snapshot(r)) => StrPair::Snapshot(lift_snapshot(c), r),
        (A::Const(l), A::Const(r)) => StrPair::Atom(lift_atom(l), lift_atom(r)),
        // A genuine clash: one side reads a single atom stream, the other picks
        // out of a multi-symbol snapshot.
        _ => return Err(domain_mismatch()),
    })
}

/// A multi-output indicator erased to one of the three input domains.
pub(crate) enum AnyMulti {
    Atom(MultiBox<Atom>),
    Real(MultiBox<Real>),
    Snapshot(MultiBox<Snapshot<Symbol>>),
}

impl AnyMulti {
    pub(crate) fn names(&self) -> &'static [&'static str] {
        match self {
            AnyMulti::Atom(m) => m.0.names(),
            AnyMulti::Real(m) => m.0.names(),
            AnyMulti::Snapshot(m) => m.0.names(),
        }
    }
    pub(crate) fn value(&self) -> Option<Vec<Real>> {
        match self {
            AnyMulti::Atom(m) => m.0.value(),
            AnyMulti::Real(m) => m.0.value(),
            AnyMulti::Snapshot(m) => m.0.value(),
        }
    }
    pub(crate) fn warm_up_bars(&self) -> usize {
        match self {
            AnyMulti::Atom(m) => m.0.warm_up_bars(),
            AnyMulti::Real(m) => m.0.warm_up_bars(),
            AnyMulti::Snapshot(m) => m.0.warm_up_bars(),
        }
    }
    pub(crate) fn unstable_bars(&self) -> usize {
        match self {
            AnyMulti::Atom(m) => m.0.unstable_bars(),
            AnyMulti::Real(m) => m.0.unstable_bars(),
            AnyMulti::Snapshot(m) => m.0.unstable_bars(),
        }
    }
    pub(crate) fn reset(&mut self) {
        match self {
            AnyMulti::Atom(m) => m.0.reset(),
            AnyMulti::Real(m) => m.0.reset(),
            AnyMulti::Snapshot(m) => m.0.reset(),
        }
    }

    /// Dispatch a frame of samples through the domain the multi lives in,
    /// producing one `Option<Vec<Real>>` per bar (`None` while warming up).
    pub(crate) fn feed_rows(&mut self, data: &Bound<'_, PyAny>) -> PyResult<Vec<Option<Vec<Real>>>> {
        let py = data.py();
        Ok(match self {
            AnyMulti::Atom(m) => {
                let cols = columns_from_frame(data)?;
                let mut out = Vec::with_capacity(cols.len(py));
                cols.for_each(py, |c| out.push(m.0.update(c.into())));
                out
            }
            AnyMulti::Real(m) => {
                let xs = reals_from_series(data)?;
                let mut out = Vec::with_capacity(xs.len(py));
                xs.for_each(py, |x| out.push(m.0.update(x)));
                out
            }
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
pub(crate) fn const_to_atom_source(c: Real) -> Source<Atom> {
    runtime::erase(Value::<Atom>::new(c))
}
