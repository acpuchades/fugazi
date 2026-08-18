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

/// How many samples `feed()` hands to `DynIndicator::update_slice` at once.
///
/// The slice form is worth ~21 instructions/sample over per-sample `update`
/// (see that method), but only for a *contiguous* slice — and the candle path
/// cannot produce a whole-frame one without rebuilding the `Vec<Candle>` that
/// streaming removed. So it chunks through a stack buffer, which gives back 4.5
/// of the 21 and keeps the 8 MB allocation gone.
///
/// 128 puts the per-chunk state write-back at ~0.04 instructions/sample while
/// keeping the buffers small: 1 KB of `Real`, 5 KB of `Candle`. Raising it has
/// nothing left to win and starts to matter on a thread with a small stack.
pub(crate) const FOLD_CHUNK: usize = 128;

/// Upper bound on a multi-output value struct's line count, so a single row can
/// be staged on the stack. Three is the widest in the crate today (`MacdValue`,
/// `AroonValue`, `AdxValue`, the three channel triples); the cap is checked at
/// the one place it could be exceeded rather than trusted.
pub(crate) const MAX_LINES: usize = 8;

/// A neutral bar for initialising a chunk buffer; every slot is overwritten
/// before it is read.
pub(crate) const ZERO_BAR: Candle = Candle {
    open: 0.0,
    high: 0.0,
    low: 0.0,
    close: 0.0,
    volume: 0.0,
};

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
///
/// The primitive is [`write_row`](MultiOutput::write_row), writing into a
/// caller-owned **slice**, because this is called **once per bar**: a
/// `vec![self.macd, self.signal, self.histogram]` per bar is 200 000 heap
/// allocations for a 200 000-bar frame, of three `f64` each. That is the same
/// allocator pressure that turned out to dominate the scalar path (see `Column`
/// in `constructors.rs`), just spread over many small blocks instead of a few
/// large ones.
///
/// A slice rather than the `&mut Vec` this used to take, because the batch path
/// already owns the destination row — `update_slice_flat` holds a
/// `chunks_mut(lines)` row that is exactly the right length. Going through a
/// `Vec` there meant a `clear`, a capacity check per line, and then a
/// `copy_from_slice` out of it again, for a destination that was ready to be
/// written in place. [`write_into`](MultiOutput::write_into) survives as the
/// `Vec`-shaped form the per-bar `update_into` still wants.
pub(crate) trait MultiOutput {
    fn names() -> &'static [&'static str]
    where
        Self: Sized;

    /// Write this value's lines into `out`, in `names()` order. `out` is
    /// exactly `names().len()` long.
    fn write_row(&self, out: &mut [Real]);

    /// Replace `out`'s contents with this value's lines, in `names()` order.
    fn write_into(&self, out: &mut Vec<Real>)
    where
        Self: Sized,
    {
        out.clear();
        out.resize(Self::names().len(), 0.0);
        self.write_row(out);
    }

    /// Allocating form, for the one-shot `value()` accessor where a per-call
    /// `Vec` is not on any hot path.
    fn values(&self) -> Vec<Real>
    where
        Self: Sized,
    {
        let mut out = Vec::new();
        self.write_into(&mut out);
        out
    }
}

impl MultiOutput for MacdValue {
    fn names() -> &'static [&'static str] {
        &["macd", "signal", "histogram"]
    }
    fn write_row(&self, out: &mut [Real]) {
        out.copy_from_slice(&[self.macd, self.signal, self.histogram]);
    }
}
impl MultiOutput for BollingerValue {
    fn names() -> &'static [&'static str] {
        &["upper", "middle", "lower"]
    }
    fn write_row(&self, out: &mut [Real]) {
        out.copy_from_slice(&[self.upper, self.middle, self.lower]);
    }
}
impl MultiOutput for KeltnerValue {
    fn names() -> &'static [&'static str] {
        &["upper", "middle", "lower"]
    }
    fn write_row(&self, out: &mut [Real]) {
        out.copy_from_slice(&[self.upper, self.middle, self.lower]);
    }
}
impl MultiOutput for DonchianValue {
    fn names() -> &'static [&'static str] {
        &["upper", "middle", "lower"]
    }
    fn write_row(&self, out: &mut [Real]) {
        out.copy_from_slice(&[self.upper, self.middle, self.lower]);
    }
}
impl MultiOutput for AdxValue {
    fn names() -> &'static [&'static str] {
        &["plus_di", "minus_di", "adx"]
    }
    fn write_row(&self, out: &mut [Real]) {
        out.copy_from_slice(&[self.plus_di, self.minus_di, self.adx]);
    }
}
impl MultiOutput for DmiValue {
    fn names() -> &'static [&'static str] {
        &["plus_di", "minus_di"]
    }
    fn write_row(&self, out: &mut [Real]) {
        out.copy_from_slice(&[self.plus_di, self.minus_di]);
    }
}
impl MultiOutput for AroonValue {
    fn names() -> &'static [&'static str] {
        &["up", "down", "oscillator"]
    }
    fn write_row(&self, out: &mut [Real]) {
        out.copy_from_slice(&[self.up, self.down, self.oscillator]);
    }
}

/// Object-safe shim over any multi-output `I`-input indicator.
pub(crate) trait DynMulti<I>: Send + Sync {
    fn names(&self) -> &'static [&'static str];
    /// Advance one sample, writing the produced lines into `out` (cleared first)
    /// and returning whether there were any. `out` is the caller's reused
    /// scratch — see [`MultiOutput`] for why this is not `-> Option<Vec<Real>>`.
    fn update_into(&mut self, input: I, out: &mut Vec<Real>) -> bool;

    /// Fold a **slice**, writing `inputs.len()` values per line **column-major**
    /// into `out` (line `j` occupies `out[j * inputs.len() ..][.. inputs.len()]`)
    /// and `NaN` for warm-up rows.
    ///
    /// Column-major, not row-major, because the caller's destination is one
    /// NumPy array *per line*: with this layout each line is already a
    /// contiguous run and the scatter is a `copy_from_slice` — a memcpy — rather
    /// than a strided per-element walk. Row-major made the caller transpose,
    /// and the transpose was most of the multi-output boundary cost (callgrind:
    /// ~185-340 instructions/sample against the scalar path's ~19).
    ///
    /// The multi-output twin of `DynIndicator::update_slice`, and it exists for
    /// the same reason: the implementation copies the concrete indicator into a
    /// local first, so its state lives in registers for the loop instead of
    /// being reloaded and stored every sample because the compiler cannot prove
    /// it does not alias `out`. See that method for the measurement.
    fn update_slice_flat(&mut self, inputs: &[I], out: &mut [Real], lines: usize);
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
    I: Clone,
{
    fn names(&self) -> &'static [&'static str] {
        <T::Output as MultiOutput>::names()
    }
    fn update_into(&mut self, input: I, out: &mut Vec<Real>) -> bool {
        out.clear();
        match Indicator::update(self, input) {
            Some(o) => {
                o.write_into(out);
                true
            }
            None => false,
        }
    }

    fn update_slice_flat(&mut self, inputs: &[I], out: &mut [Real], lines: usize) {
        // `local` is the whole point — see the trait.
        let mut local = self.clone();
        let rows = inputs.len();
        // One row of lines at a time, on the stack. `write_row` wants the lines
        // contiguous and the destination wants them `rows` apart, so the row
        // lands here first and is fanned out. It is `MAX_LINES` doubles — the
        // widest value struct in the crate emits three — so it stays in
        // registers or L1, and the fan-out is a handful of stores into a buffer
        // measured in kilobytes.
        let mut row_buf = [0.0 as Real; MAX_LINES];
        let row_buf = &mut row_buf[..lines];
        for (r, x) in inputs.iter().enumerate() {
            match Indicator::update(&mut local, x.clone()) {
                Some(o) => o.write_row(row_buf),
                None => row_buf.fill(Real::NAN),
            }
            for (j, v) in row_buf.iter().enumerate() {
                out[j * rows + r] = *v;
            }
        }
        *self = local;
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

impl<I: Clone> MultiBox<I> {
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
    /// The last output's lines, and whether there were any. Kept as a reused
    /// buffer rather than `Option<Vec<Real>>` so driving the underlying multi
    /// does not allocate once per bar; `Vec::clear` retains the capacity.
    pub(crate) last_output: Vec<Real>,
    pub(crate) last_valid: bool,
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
            // One deref of the guard, then three disjoint field borrows.
            let cell = &mut *cell;
            cell.last_valid = cell.multi.update_into(input, &mut cell.last_output);
            cell.generation = cell.generation.wrapping_add(1);
        }
        self.local_gen = cell.generation;
        self.last_value = if cell.last_valid {
            Some(cell.last_output[self.field_index])
        } else {
            None
        };
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
        cell.last_output.clear();
        cell.last_valid = false;
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
    Candle(Arc<Mutex<SharedMultiCell<Candle>>>),
    Atom(Arc<Mutex<SharedMultiCell<Atom>>>),
    Real(Arc<Mutex<SharedMultiCell<Real>>>),
    Snapshot(Arc<Mutex<SharedMultiCell<Snapshot<Symbol>>>>),
}

impl AnySharedMulti {
    pub(crate) fn names(&self) -> &'static [&'static str] {
        match self {
            AnySharedMulti::Candle(c) => c.lock().expect("mutex poisoned").names,
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
            AnySharedMulti::Candle(cell) => PyIndicator {
                src: AnySource::Candle(runtime::erase(SharedProjector::<Candle> {
                    cell: Arc::clone(cell),
                    field_index: idx,
                    local_gen: cell.lock().expect("mutex poisoned").generation,
                    last_value: None,
                })),
                root: None,
            },
            AnySharedMulti::Atom(cell) => PyIndicator {
                src: AnySource::Atom(runtime::erase(SharedProjector::<Atom> {
                    cell: Arc::clone(cell),
                    field_index: idx,
                    local_gen: cell.lock().expect("mutex poisoned").generation,
                    last_value: None,
                })),
                root: None,
            },
            AnySharedMulti::Real(cell) => PyIndicator {
                src: AnySource::Real(runtime::erase(SharedProjector::<Real> {
                    cell: Arc::clone(cell),
                    field_index: idx,
                    local_gen: cell.lock().expect("mutex poisoned").generation,
                    last_value: None,
                })),
                root: None,
            },
            AnySharedMulti::Snapshot(cell) => PyIndicator {
                src: AnySource::Snapshot(runtime::erase(SharedProjector::<Snapshot<Symbol>> {
                    cell: Arc::clone(cell),
                    field_index: idx,
                    local_gen: cell.lock().expect("mutex poisoned").generation,
                    last_value: None,
                })),
                root: None,
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
    ///
    /// # Kept in step with [`feed_into_numpy`](Self::feed_into_numpy) by hand
    ///
    /// **A new `AnySource` variant needs an arm in both.** These two hold the
    /// same five-arm dispatch and differ only in where each value goes: a `Vec`
    /// here, NumPy's own buffer there. That is duplication, and it is deliberate
    /// — merging them was tried and is worse:
    ///
    /// * A shared fold taking `&mut dyn FnMut(Option<Real>)` puts an indirect
    ///   call on the per-sample path. Measured at this granularity (`icount`'s
    ///   `sma_dyn_*` pair) that costs more than it saves.
    /// * A `Sink` trait cannot hold the NumPy buffer *and* a slice borrowed from
    ///   it — that is self-referential. Scoping the borrow in a closure
    ///   ([`numpy_filled`]) is what avoids it, and a closure needs the row count
    ///   up front, which is exactly what forces the split.
    ///
    /// This one exists only for the **no-NumPy** path: NumPy is an optional
    /// dependency, and without it `feed()` returns a plain list that keeps the
    /// warm-up `None`s rather than flattening them to `NaN`. So it is cold code
    /// that must stay correct, which is the worst combination — hence the note
    /// rather than a silent divergence.
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
    /// **A new `AnySource` variant needs an arm here *and* in `feed_rows`** — see
    /// that method for why the two are not merged.
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
                    let mut buf = [ZERO_BAR; FOLD_CHUNK];
                    let mut got = [None; FOLD_CHUNK];
                    let mut cells = slice.iter();
                    cols.for_each_chunk(py, &mut buf, |chunk| {
                        let got = &mut got[..chunk.len()];
                        s.update_slice(chunk, got);
                        for v in got.iter() {
                            if let Some(cell) = cells.next() {
                                cell.set(v.unwrap_or(Real::NAN));
                            }
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
                    let mut buf = [0.0; FOLD_CHUNK];
                    let mut got = [None; FOLD_CHUNK];
                    let mut cells = slice.iter();
                    xs.for_each_chunk(py, &mut buf, |chunk| {
                        let got = &mut got[..chunk.len()];
                        s.update_slice(chunk, got);
                        for v in got.iter() {
                            if let Some(cell) = cells.next() {
                                cell.set(v.unwrap_or(Real::NAN));
                            }
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
/// # The field is a type parameter, and that was measured
///
/// It was briefly an enum with the field chosen at run time, so that fusing
/// (see [`PendingRoot`]) would need one monomorphisation per constructor
/// instead of seven. The branch is not free:
///
/// | instr/sample | typed | runtime enum |
/// |---|---:|---:|
/// | `ta.close().feed(frame)` | **53.2** | 58.2 |
/// | `ta.sma(ta.close(), 14).feed(frame)` | 94.2 | 92.2 |
///
/// ~5 instructions/sample, paid on *every* bar-field read. Unfused — a bare
/// `ta.close()` — that is pure loss, and it made the commonest root in the API
/// 9% worse. Fused it ate most of the 8 that fusing saves. Seven
/// instantiations is the cheaper side of that trade; see [`PendingRoot`] for
/// what it costs in binary size.
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

/// Which bar field a [`PendingRoot::Field`] root reads. A tag, not a reader:
/// `map_rooted!` matches it to pick the typed [`BarField`] to fuse over.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum BarFieldKind {
    Open,
    High,
    Low,
    Close,
    Volume,
    Typical,
    Median,
}

/// A chain root kept in its concrete type, so a wrapping constructor can be
/// monomorphised over it instead of over a `Box<dyn …>`.
///
/// # What this is for
///
/// `ta.sma(ta.close(), 14)` used to erase twice: `close()` handed back an
/// already-boxed source, so by the time `sma()` saw it the leaf was opaque and
/// could only be boxed again. Carrying the leaf's concrete type alongside its
/// erased form lets `map_rooted!` build `Sma<BarFieldDyn>` — **one** erased level
/// rather than two.
///
/// Measured (`benches/icount.rs`, `sma_scalar_*`, net of a control, every variant
/// storing an `Option<Real>` per sample):
///
/// | erased levels | | instr/sample |
/// |---:|---|---:|
/// | 2 | as it was | 45.0 |
/// | 1 | fused, this | **37.0** |
/// | 0 | unreachable — the carrier must erase something | 16.0 |
///
/// So this is worth **8.0 instructions/sample**. The lopsided split is the
/// useful part: the *outer* level costs 21, because dropping the last erasure is
/// what lets the chain inline into the driving loop and hold its state in
/// registers. Only a fully monomorphised carrier could take that one, which is a
/// much larger change and deliberately not attempted here.
///
/// # Why this lives on `PyIndicator` and not in `AnySource`
///
/// The first attempt added `RootReal`/`RootField` variants to [`AnySource`].
/// That works, and it is worse: `AnySource` is the vocabulary *every* consumer
/// matches on, so two extra variants meant either an arm in ~15 unrelated
/// matches or a `settle()` normaliser plus six `unreachable!()` arms the compiler
/// could not verify away. Neither is a good trade for 8 instructions.
///
/// Here the root is **metadata on the carrier**. `AnySource` keeps exactly the
/// five domains it always had, every existing match is untouched, and the only
/// code that knows roots exist is the handful of constructors that fuse. The
/// cost is that a rooted leaf holds its root twice — once erased in `src`, once
/// concrete here — which is a few dozen bytes at construction and nothing per
/// sample.
///
/// # The invariant
///
/// `PyIndicator::src` **must** be the erased form of `PyIndicator::root`
/// whenever `root` is `Some`. Fusing swaps one for the other, so if they ever
/// disagree the fused chain computes something different from the unfused one.
/// Only [`PyIndicator::rooted`] sets them, and only from the same value.
#[derive(Debug, Clone)]
pub(crate) enum PendingRoot {
    /// `ta.identity()` — a raw value stream.
    Real(Identity<Real>),
    /// `ta.close()`, `ta.high()`, … — which bar field, as a tag. `map_rooted!`
    /// turns it back into a typed [`BarField`] so the fused wrapper gets a
    /// direct field load rather than a branch.
    Field(BarFieldKind),
}

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

    /// Fold the frame into one `bool` per bar, a chunk at a time.
    ///
    /// `SignalBox` wraps a `Chain<I, bool>`, so it inherits
    /// `DynIndicator::update_slice` and with it the local-state trick — the same
    /// ~21 instructions/sample the scalar and multi-output paths get. Warm-up
    /// `None` flattens to `false` here, which is the `SignalBox` contract.
    pub(crate) fn feed_rows(&mut self, data: &Bound<'_, PyAny>) -> PyResult<Vec<bool>> {
        let py = data.py();
        let mut out: Vec<bool> = Vec::new();
        let mut got = [None; FOLD_CHUNK];
        macro_rules! flush {
            ($n:expr) => {
                out.extend(got[..$n].iter().map(|v| v.unwrap_or(false)))
            };
        }
        match self {
            // The bar-only arm: candles go straight in. No `Atom` is built,
            // moved or dropped — which is the whole point of the domain.
            AnySignal::Candle(s) => {
                let cols = columns_from_frame(data)?;
                out.reserve(cols.len(py));
                let mut buf = [ZERO_BAR; FOLD_CHUNK];
                cols.for_each_chunk(py, &mut buf, |chunk| {
                    s.0.update_slice(chunk, &mut got[..chunk.len()]);
                    flush!(chunk.len());
                });
            }
            AnySignal::Atom(s) => {
                let cols = columns_from_frame(data)?;
                out.reserve(cols.len(py));
                let mut buf = [ZERO_BAR; FOLD_CHUNK];
                let mut atoms: Vec<Atom> = Vec::with_capacity(FOLD_CHUNK);
                cols.for_each_chunk(py, &mut buf, |chunk| {
                    atoms.clear();
                    atoms.extend(chunk.iter().map(|c| Atom::from(*c)));
                    s.0.update_slice(&atoms, &mut got[..chunk.len()]);
                    flush!(chunk.len());
                });
            }
            AnySignal::Real(s) => {
                let xs = reals_from_series(data)?;
                out.reserve(xs.len(py));
                let mut buf = [0.0; FOLD_CHUNK];
                xs.for_each_chunk(py, &mut buf, |chunk| {
                    s.0.update_slice(chunk, &mut got[..chunk.len()]);
                    flush!(chunk.len());
                });
            }
            AnySignal::Snapshot(s) => {
                let snaps = snapshots_from_sequence(data)?;
                out.reserve(snaps.len());
                for chunk in snaps.chunks(FOLD_CHUNK) {
                    s.0.update_slice(chunk, &mut got[..chunk.len()]);
                    flush!(chunk.len());
                }
            }
        }
        Ok(out)
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
    /// Reads the **bar only** — 40 bytes, `Copy`, no drop glue. Where every
    /// bar-rooted multi belongs, and the multi-output half of P1.
    ///
    /// Without it, `ta.dmi(14)` rooted at `CurrentBar<Identity<Atom>>` spent
    /// **97 instructions/sample** — `Identity<Atom>` 42, `CurrentBar` 55 — doing
    /// nothing but storing an 88-byte `Atom` and reading a 40-byte `Candle` back
    /// out of it. `AnySource` got this treatment early; this is the same fix,
    /// for the path `dmi`, `adx`, `aroon`, `macd` and `bollinger` run on.
    Candle(MultiBox<Candle>),
    Atom(MultiBox<Atom>),
    Real(MultiBox<Real>),
    Snapshot(MultiBox<Snapshot<Symbol>>),
}

impl AnyMulti {
    pub(crate) fn names(&self) -> &'static [&'static str] {
        match self {
            AnyMulti::Candle(m) => m.0.names(),
            AnyMulti::Atom(m) => m.0.names(),
            AnyMulti::Real(m) => m.0.names(),
            AnyMulti::Snapshot(m) => m.0.names(),
        }
    }
    pub(crate) fn value(&self) -> Option<Vec<Real>> {
        match self {
            AnyMulti::Candle(m) => m.0.value(),
            AnyMulti::Atom(m) => m.0.value(),
            AnyMulti::Real(m) => m.0.value(),
            AnyMulti::Snapshot(m) => m.0.value(),
        }
    }
    pub(crate) fn warm_up_bars(&self) -> usize {
        match self {
            AnyMulti::Candle(m) => m.0.warm_up_bars(),
            AnyMulti::Atom(m) => m.0.warm_up_bars(),
            AnyMulti::Real(m) => m.0.warm_up_bars(),
            AnyMulti::Snapshot(m) => m.0.warm_up_bars(),
        }
    }
    pub(crate) fn unstable_bars(&self) -> usize {
        match self {
            AnyMulti::Candle(m) => m.0.unstable_bars(),
            AnyMulti::Atom(m) => m.0.unstable_bars(),
            AnyMulti::Real(m) => m.0.unstable_bars(),
            AnyMulti::Snapshot(m) => m.0.unstable_bars(),
        }
    }
    pub(crate) fn reset(&mut self) {
        match self {
            AnyMulti::Candle(m) => m.0.reset(),
            AnyMulti::Atom(m) => m.0.reset(),
            AnyMulti::Real(m) => m.0.reset(),
            AnyMulti::Snapshot(m) => m.0.reset(),
        }
    }

    /// Dispatch a frame of samples through the domain the multi lives in,
    /// producing one `Option<Vec<Real>>` per bar (`None` while warming up).
    ///
    /// **A new `AnyMulti` variant needs an arm here *and* in
    /// [`feed_into_columns`](Self::feed_into_columns)** — same standing
    /// duplication as `AnySource`, same reason, and this one likewise exists only
    /// for the no-NumPy fallback.
    pub(crate) fn feed_rows(&mut self, data: &Bound<'_, PyAny>) -> PyResult<Vec<Option<Vec<Real>>>> {
        let py = data.py();
        // One scratch buffer, cloned per produced row. The clone is unavoidable
        // here because the caller wants owned rows; the *repeated allocation
        // inside the multi* is what `update_into` removes.
        let mut scratch: Vec<Real> = Vec::new();
        let mut rows = Vec::new();
        macro_rules! drive {
            ($m:expr, $input:expr) => {{
                if $m.update_into($input, &mut scratch) {
                    rows.push(Some(scratch.clone()));
                } else {
                    rows.push(None);
                }
            }};
        }
        match self {
            AnyMulti::Candle(m) => {
                let cols = columns_from_frame(data)?;
                rows.reserve(cols.len(py));
                cols.for_each(py, |c| drive!(m.0, c));
            }
            AnyMulti::Atom(m) => {
                let cols = columns_from_frame(data)?;
                rows.reserve(cols.len(py));
                cols.for_each(py, |c| drive!(m.0, c.into()));
            }
            AnyMulti::Real(m) => {
                let xs = reals_from_series(data)?;
                rows.reserve(xs.len(py));
                xs.for_each(py, |x| drive!(m.0, x));
            }
            AnyMulti::Snapshot(m) => {
                for snap in snapshots_from_sequence(data)? {
                    drive!(m.0, snap);
                }
            }
        }
        Ok(rows)
    }

    /// Fold the frame straight into **one NumPy array per output line**.
    ///
    /// The multi-output twin of `AnySource::feed_into_numpy`, and it removes two
    /// costs rather than one:
    ///
    /// * **A heap allocation per bar.** Each `update` used to return a fresh
    ///   `Vec<Real>` of two or three `f64` — 200 000 tiny allocations for a
    ///   200 000-bar frame. See [`MultiOutput`].
    /// * **The transpose.** `build_multi` collected row-major
    ///   `Vec<Option<Vec<Real>>>` and then rebuilt it column-major, so the whole
    ///   result was materialised twice before NumPy saw any of it. Writing
    ///   column-major as values are produced skips both copies.
    ///
    /// Warm-up rows become `NaN` in every column, which is what `build_multi`
    /// did with its `None`s.
    pub(crate) fn feed_into_columns<'py>(
        &mut self,
        py: Python<'py>,
        data: &Bound<'py, PyAny>,
    ) -> PyResult<Vec<Bound<'py, PyAny>>> {
        let lines = self.names().len();
        // `update_slice_flat` stages one row on the stack; a value struct wider
        // than that would silently truncate, so refuse instead. Nothing in the
        // crate is close — three is the widest — and this is the one place a new
        // one would arrive.
        if lines > MAX_LINES {
            return Err(PyTypeError::new_err(format!(
                "multi-output indicator emits {lines} lines; the fold stages at \
                 most {MAX_LINES} (raise MAX_LINES in python/src/carriers.rs)"
            )));
        }

        // Allocate every column first, then borrow all of their buffers at once.
        // The buffers must outlive the slices, hence the two bindings.
        let n = self.row_count(py, data)?;
        let arrays = (0..lines)
            .map(|_| empty_f64_array(py, n))
            .collect::<PyResult<Vec<_>>>()?;
        let buffers = arrays
            .iter()
            .map(pyo3::buffer::PyBuffer::<f64>::get)
            .collect::<PyResult<Vec<_>>>()?;
        let columns = buffers
            .iter()
            .map(|b| {
                b.as_mut_slice(py)
                    .ok_or_else(|| PyTypeError::new_err("numpy array is not writable"))
            })
            .collect::<PyResult<Vec<_>>>()?;

        // Folded a chunk at a time with the indicator's state held in a local —
        // the same reason `DynIndicator::update_slice` exists, and worth ~21
        // instructions/sample here too. `flat` takes the chunk row-major and is
        // scattered into the column buffers after; over 128 rows that transpose
        // stays in L1.
        let mut flat = vec![0.0; FOLD_CHUNK * lines.max(1)];
        let mut row = 0usize;
        // `flat` arrives **column-major** (see `update_slice_flat`), so each
        // line is already a contiguous run of `n` values and this is a memcpy
        // per column per chunk. There is no per-element work left here at all:
        // no bounds check, no stride, no transpose. That transpose was most of
        // the multi-output boundary — callgrind put the scalar path at ~19
        // instructions/sample and this one at 185-340.
        //
        // `Cell<Real>` has the same layout as `Real`, so the destination can be
        // viewed as a plain slice for the copy. That is what makes it one
        // `memcpy` instead of `n` stores through `Cell::set`.
        let scatter = |flat: &[Real], n: usize, row: &mut usize| {
            for (j, col) in columns.iter().enumerate() {
                // Clamp once per column per chunk. A caller-supplied frame whose
                // length disagrees with `row_count` is the only way this bites,
                // and it truncates rather than panicking, as before.
                let end = (*row + n).min(col.len());
                let start = (*row).min(end);
                let dst = &col[start..end];
                let src = &flat[j * n..j * n + dst.len()];
                for (cell, v) in dst.iter().zip(src) {
                    cell.set(*v);
                }
            }
            *row += n;
        };
        match self {
            // The bar arm: candles go straight to the multi, so no `Atom` is
            // built, moved or dropped — 97 instructions/sample of P1 gone.
            AnyMulti::Candle(m) => {
                let cols = columns_from_frame(data)?;
                let mut buf = [ZERO_BAR; FOLD_CHUNK];
                cols.for_each_chunk(py, &mut buf, |chunk| {
                    let flat = &mut flat[..chunk.len() * lines];
                    m.0.update_slice_flat(chunk, flat, lines);
                    scatter(flat, chunk.len(), &mut row);
                });
            }
            AnyMulti::Atom(m) => {
                let cols = columns_from_frame(data)?;
                let mut buf = [ZERO_BAR; FOLD_CHUNK];
                // `AnyMulti` has no bar-only domain, so each candle still lifts
                // to an 88-byte `Atom` (step 3 of the Python plan, still open).
                let mut atoms: Vec<Atom> = Vec::with_capacity(FOLD_CHUNK);
                cols.for_each_chunk(py, &mut buf, |chunk| {
                    atoms.clear();
                    atoms.extend(chunk.iter().map(|c| Atom::from(*c)));
                    let flat = &mut flat[..chunk.len() * lines];
                    m.0.update_slice_flat(&atoms, flat, lines);
                    scatter(flat, chunk.len(), &mut row);
                });
            }
            AnyMulti::Real(m) => {
                let xs = reals_from_series(data)?;
                let mut buf = [0.0; FOLD_CHUNK];
                xs.for_each_chunk(py, &mut buf, |chunk| {
                    let flat = &mut flat[..chunk.len() * lines];
                    m.0.update_slice_flat(chunk, flat, lines);
                    scatter(flat, chunk.len(), &mut row);
                });
            }
            AnyMulti::Snapshot(m) => {
                let snaps = snapshots_from_sequence(data)?;
                for chunk in snaps.chunks(FOLD_CHUNK) {
                    let flat = &mut flat[..chunk.len() * lines];
                    m.0.update_slice_flat(chunk, flat, lines);
                    scatter(flat, chunk.len(), &mut row);
                }
            }
        }
        Ok(arrays)
    }

    /// How many rows `data` holds, in whichever shape this multi consumes it.
    /// Cheap: the column readers borrow rather than copy (see `Column`).
    fn row_count(&self, py: Python<'_>, data: &Bound<'_, PyAny>) -> PyResult<usize> {
        Ok(match self {
            AnyMulti::Candle(_) => columns_from_frame(data)?.len(py),
            AnyMulti::Atom(_) => columns_from_frame(data)?.len(py),
            AnyMulti::Real(_) => reals_from_series(data)?.len(py),
            AnyMulti::Snapshot(_) => snapshots_from_sequence(data)?.len(),
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
