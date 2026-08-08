use crate::prelude::*;
// The binding modules were one flat namespace before the split and still read
// as one: each pulls in its siblings, so a cross-module reference needs no path.
#[allow(unused_imports)]
use crate::carriers::*;
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
// Python classes
// ---------------------------------------------------------------------------

/// A single OHLCV bar.
#[pyclass(name = "Candle", frozen, skip_from_py_object)]
#[derive(Clone, Copy)]
pub(crate) struct PyCandle {
    pub(crate) inner: Candle,
}

#[pymethods]
impl PyCandle {
    #[new]
    pub(crate) fn new(open: f64, high: f64, low: f64, close: f64, volume: f64) -> Self {
        PyCandle {
            inner: Candle::new(open, high, low, close, volume),
        }
    }

    #[getter]
    pub(crate) fn open(&self) -> f64 {
        self.inner.open
    }
    #[getter]
    pub(crate) fn high(&self) -> f64 {
        self.inner.high
    }
    #[getter]
    pub(crate) fn low(&self) -> f64 {
        self.inner.low
    }
    #[getter]
    pub(crate) fn close(&self) -> f64 {
        self.inner.close
    }
    #[getter]
    pub(crate) fn volume(&self) -> f64 {
        self.inner.volume
    }

    /// Typical price, `(high + low + close) / 3`.
    pub(crate) fn typical(&self) -> f64 {
        self.inner.typical()
    }

    /// Median price, `(high + low) / 2`.
    pub(crate) fn median(&self) -> f64 {
        self.inner.median()
    }

    pub(crate) fn __repr__(&self) -> String {
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
pub(crate) struct PySchema {
    pub(crate) inner: Arc<Schema>,
}

#[pymethods]
impl PySchema {
    pub(crate) fn __len__(&self) -> usize {
        self.inner.len()
    }

    pub(crate) fn __contains__(&self, key: &str) -> bool {
        self.inner.contains(key)
    }

    /// The zero-based column index of `key`, or `None` if unregistered.
    pub(crate) fn index_of(&self, key: &str) -> Option<usize> {
        self.inner.index_of(key)
    }

    /// The declared type of the column at `index` — one of `"real"`,
    /// `"bool"`, `"str"` — or `None` if the index is out of range.
    pub(crate) fn type_of(&self, index: usize) -> Option<&'static str> {
        self.inner.type_of(index).map(overlay_type_name)
    }

    /// The declared type of column `key` — one of `"real"`, `"bool"`, `"str"`
    /// — or `None` if `key` is not registered.
    pub(crate) fn type_of_key(&self, key: &str) -> Option<&'static str> {
        self.inner.type_of_key(key).map(overlay_type_name)
    }

    /// All registered column names, in insertion order.
    pub(crate) fn keys(&self) -> Vec<String> {
        self.inner.keys().map(str::to_string).collect()
    }

    pub(crate) fn __repr__(&self) -> String {
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

pub(crate) fn overlay_type_name(ty: OverlayType) -> &'static str {
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
pub(crate) struct PySchemaBuilder {
    pub(crate) inner: Option<SchemaBuilder>,
}

#[pymethods]
impl PySchemaBuilder {
    #[new]
    pub(crate) fn new() -> Self {
        Self {
            inner: Some(SchemaBuilder::default()),
        }
    }

    /// Register `key` as a `Real` column. Returns the assigned column index; a
    /// repeated key returns the previously-assigned index without adding a slot.
    /// Re-registering with a different type raises `ValueError`.
    pub(crate) fn add_real(&mut self, key: &str) -> PyResult<usize> {
        self.with_builder(|b| b.add_real(key.to_string()))
    }

    /// Register `key` as a `Bool` column. A `Bool` overlay reads as a signal
    /// directly — no `str_eq true` needed.
    pub(crate) fn add_bool(&mut self, key: &str) -> PyResult<usize> {
        self.with_builder(|b| b.add_bool(key.to_string()))
    }

    /// Register `key` as a `Str` column. Consumed via `get_str(...).eq("...")`
    /// (or the underlying `str_eq(...)`).
    pub(crate) fn add_str(&mut self, key: &str) -> PyResult<usize> {
        self.with_builder(|b| b.add_str(key.to_string()))
    }

    /// Back-compat alias for [`add_real`](Self::add_real). Prefer the typed
    /// method in new code.
    pub(crate) fn add(&mut self, key: &str) -> PyResult<usize> {
        self.add_real(key)
    }

    pub(crate) fn __len__(&self) -> PyResult<usize> {
        self.inner
            .as_ref()
            .ok_or_else(|| {
                PyValueError::new_err("SchemaBuilder has already been finished")
            })
            .map(|b| b.len())
    }

    /// Freeze into an immutable [`Schema`]. The builder is consumed — further
    /// calls raise `ValueError`.
    pub(crate) fn finish(&mut self) -> PyResult<PySchema> {
        let builder = self.inner.take().ok_or_else(|| {
            PyValueError::new_err("SchemaBuilder has already been finished")
        })?;
        Ok(PySchema {
            inner: builder.finish(),
        })
    }

    pub(crate) fn __repr__(&self) -> String {
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
    pub(crate) fn with_builder<F>(&mut self, f: F) -> PyResult<usize>
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
pub(crate) fn panic_message(payload: &Box<dyn std::any::Any + Send + 'static>) -> String {
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
/// the column's declared type, or `None` to mark that column absent for this
/// bar (a warming computed column, or a missing cell). Reading a value with
/// `get()` returns the native Python type (or `None` for an absent slot); the
/// typed accessors (`get_real` / `get_bool` / `get_str`) return `None` on an
/// absent slot or a type mismatch.
///
/// The internal `Rc<[OverlayValue]>` (per-atom, non-atomic refcount) makes
/// this class `unsendable` — it's confined to the Python thread that created
/// it. This is fine under the GIL and keeps overlay clones cheap in the hot
/// per-bar loop.
#[pyclass(name = "OverlayInfo", frozen, unsendable, skip_from_py_object)]
#[derive(Clone)]
pub(crate) struct PyOverlayInfo {
    pub(crate) inner: OverlayInfo,
}

#[pymethods]
impl PyOverlayInfo {
    #[new]
    pub(crate) fn new(schema: &PySchema, values: Vec<Py<PyAny>>, py: Python<'_>) -> PyResult<Self> {
        if values.len() != schema.inner.len() {
            return Err(PyValueError::new_err(format!(
                "values length ({}) must match schema length ({})",
                values.len(),
                schema.inner.len(),
            )));
        }
        let mut typed: Vec<Option<OverlayValue>> = Vec::with_capacity(values.len());
        for (i, v) in values.into_iter().enumerate() {
            let declared = schema.inner.type_of(i).expect("schema index in range");
            let bound = v.bind(py);
            if bound.is_none() {
                // Python `None` marks a slot absent for this bar (a warming
                // computed column, or a genuinely missing cell).
                typed.push(None);
            } else {
                typed.push(Some(python_to_overlay_value(bound, declared, i)?));
            }
        }
        Ok(Self {
            inner: OverlayInfo::sparse(schema.inner.clone(), typed),
        })
    }

    pub(crate) fn __len__(&self) -> usize {
        self.inner.values().len()
    }

    /// Read the value at a resolved column index as its native Python type
    /// (`float` for `Real`, `bool` for `Bool`, `str` for `Str`), or `None` if
    /// the index is out of bounds.
    pub(crate) fn get(&self, py: Python<'_>, index: usize) -> Option<Py<PyAny>> {
        self.inner.get(index).and_then(|v| overlay_to_python(py, v).ok())
    }

    /// Read the value by column name (`None` if the key isn't registered).
    pub(crate) fn get_by_key(&self, py: Python<'_>, key: &str) -> Option<Py<PyAny>> {
        self.inner
            .schema()
            .index_of(key)
            .and_then(|i| self.get(py, i))
    }

    /// Typed reader: `Real` value at `index`, or `None` on out-of-bounds or a
    /// type mismatch (the schema declares a different type at this index).
    pub(crate) fn get_real(&self, index: usize) -> Option<Real> {
        self.inner.get_real(index)
    }

    /// Typed reader: `Bool` value at `index`, or `None` on out-of-bounds or a
    /// type mismatch.
    pub(crate) fn get_bool(&self, index: usize) -> Option<bool> {
        self.inner.get_bool(index)
    }

    /// Typed reader: `Str` value at `index`, or `None` on out-of-bounds or a
    /// type mismatch.
    pub(crate) fn get_str(&self, index: usize) -> Option<String> {
        self.inner.get_str(index).map(|s| s.to_string())
    }

    pub(crate) fn __repr__(&self) -> String {
        format!("OverlayInfo(values={:?})", self.inner.values())
    }
}

/// Convert one Python value into an [`OverlayValue`] of the declared type. On
/// mismatch, raises a `ValueError` mentioning the slot index + expected type
/// so a caller can locate the bad value in a large list.
pub(crate) fn python_to_overlay_value(
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
pub(crate) fn is_python_bool(bound: &Bound<'_, PyAny>) -> bool {
    bound
        .get_type()
        .name()
        .ok()
        .map(|n| n == "bool")
        .unwrap_or(false)
}

pub(crate) fn slot_type_error(slot: usize, declared: OverlayType, bound: &Bound<'_, PyAny>) -> PyErr {
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
pub(crate) fn overlay_to_python(py: Python<'_>, v: &OverlayValue) -> PyResult<Py<PyAny>> {
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
pub(crate) struct PyAtom {
    pub(crate) inner: Atom,
}

#[pymethods]
impl PyAtom {
    /// `time` is the bar-open UTC millisecond stamp (an `int`) — passed through
    /// to any calendar indicator (`year()`, `month()`, `day_of_week()`, …) in
    /// the chain. `None` on synthetic bars leaves those calendar reads at
    /// `None` (matching a not-yet-warm result).
    ///
    /// `candle` may be `None`, which builds an **overlay-only** atom: a series
    /// that is not a price — a funding rate, an open interest, a market
    /// capitalisation. Such an atom is stacked into a `Snapshot` beside the
    /// price series and read with `pick()` + `get()`; it never reaches
    /// mark-to-market, and `sole_atom` skips it, so attaching one cannot
    /// disturb a strategy that never asked for it. An overlay-only atom with
    /// no overlays at all is rejected — it would carry nothing.
    #[new]
    #[pyo3(signature = (candle = None, overlays = None, time = None))]
    pub(crate) fn new(
        candle: Option<&PyCandle>,
        overlays: Option<&PyOverlayInfo>,
        time: Option<i64>,
    ) -> PyResult<Self> {
        if candle.is_none() && overlays.is_none() {
            return Err(PyValueError::new_err(
                "an Atom needs a candle, overlay values, or both — one with neither \
                 carries no data",
            ));
        }
        let inner = Atom {
            candle: candle.map(|c| c.inner),
            time: time.map(Timestamp),
            overlays: overlays.map(|ov| ov.inner.clone()),
        };
        Ok(Self { inner })
    }

    /// The bar, or `None` for an overlay-only atom — a series that is not a
    /// price (a funding rate, an open interest). Reading it is how a caller
    /// tells the two apart; everything that prices a bar must handle the
    /// `None`.
    #[getter]
    pub(crate) fn candle(&self) -> Option<PyCandle> {
        self.inner.candle.map(|inner| PyCandle { inner })
    }

    /// Whether this atom carries a bar and can therefore be priced.
    #[getter]
    pub(crate) fn is_priceable(&self) -> bool {
        self.inner.is_priceable()
    }

    #[getter]
    pub(crate) fn overlays(&self) -> Option<PyOverlayInfo> {
        self.inner
            .overlays
            .as_ref()
            .cloned()
            .map(|ov| PyOverlayInfo { inner: ov })
    }

    /// The bar-open time as a UTC millisecond epoch (an `int`), or `None`
    /// if the atom was constructed without one.
    #[getter]
    pub(crate) fn time(&self) -> Option<i64> {
        self.inner.time.map(|t| t.0)
    }

    pub(crate) fn __repr__(&self) -> String {
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
    pub(crate) fn __eq__(&self, other: &Bound<'_, PyAny>) -> PyResult<bool> {
        match other.cast::<PyAtom>() {
            Ok(o) => Ok(self.inner == o.borrow().inner),
            Err(_) => Ok(false),
        }
    }

    pub(crate) fn __ne__(&self, other: &Bound<'_, PyAny>) -> PyResult<bool> {
        Ok(!self.__eq__(other)?)
    }

    pub(crate) fn __hash__(&self) -> u64 {
        // `Timestamp: Hash` already; `None` hashes to a distinct sentinel.
        use std::collections::hash_map::DefaultHasher;
        use std::hash::{Hash, Hasher};
        let mut h = DefaultHasher::new();
        self.inner.time.map(|t| t.0).hash(&mut h);
        h.finish()
    }

    pub(crate) fn __lt__(&self, other: PyRef<'_, PyAtom>) -> bool {
        self.inner < other.inner
    }
    pub(crate) fn __le__(&self, other: PyRef<'_, PyAtom>) -> bool {
        self.inner <= other.inner
    }
    pub(crate) fn __gt__(&self, other: PyRef<'_, PyAtom>) -> bool {
        self.inner > other.inner
    }
    pub(crate) fn __ge__(&self, other: PyRef<'_, PyAtom>) -> bool {
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
pub(crate) struct PyFrequency {
    pub(crate) inner: Frequency,
}

#[pymethods]
impl PyFrequency {
    /// Parse an `N<unit>` token (`"1m"`, `"5m"`, `"1h"`, `"4h"`, `"1d"`,
    /// `"1w"`, `"1M"`, …). Raises `ValueError` on any other shape.
    #[new]
    pub(crate) fn new(token: &str) -> PyResult<Self> {
        use std::str::FromStr;
        let inner = Frequency::from_str(token).map_err(PyValueError::new_err)?;
        Ok(Self { inner })
    }

    /// The canonical token — the round-trip of the constructor.
    pub(crate) fn __str__(&self) -> String {
        self.inner.as_token()
    }

    pub(crate) fn __repr__(&self) -> String {
        format!("Frequency({:?})", self.inner.as_token())
    }

    pub(crate) fn __hash__(&self) -> u64 {
        use std::collections::hash_map::DefaultHasher;
        use std::hash::{Hash, Hasher};
        let mut h = DefaultHasher::new();
        self.inner.hash(&mut h);
        h.finish()
    }

    pub(crate) fn __eq__(&self, other: &Bound<'_, PyAny>) -> bool {
        match other.cast::<PyFrequency>() {
            Ok(o) => self.inner == o.borrow().inner,
            Err(_) => false,
        }
    }

    pub(crate) fn __ne__(&self, other: &Bound<'_, PyAny>) -> bool {
        !self.__eq__(other)
    }

    pub(crate) fn __lt__(&self, other: PyRef<'_, PyFrequency>) -> bool {
        self.inner < other.inner
    }
    pub(crate) fn __le__(&self, other: PyRef<'_, PyFrequency>) -> bool {
        self.inner <= other.inner
    }
    pub(crate) fn __gt__(&self, other: PyRef<'_, PyFrequency>) -> bool {
        self.inner > other.inner
    }
    pub(crate) fn __ge__(&self, other: PyRef<'_, PyFrequency>) -> bool {
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
pub(crate) struct PySelector {
    pub(crate) inner: Selector<String>,
}

#[pymethods]
impl PySelector {
    /// Build a selector. Both fields are optional and default to `None`; an
    /// empty selector is legal and drives the [`Pick`] single-entry-unpack path.
    /// `freq` accepts a `Frequency` instance or a token string (`"1h"`, `"1d"`).
    #[new]
    #[pyo3(signature = (symbol = None, freq = None))]
    pub(crate) fn new(symbol: Option<String>, freq: Option<&Bound<'_, PyAny>>) -> PyResult<Self> {
        let freq = freq.map(coerce_frequency).transpose()?;
        Ok(Self {
            inner: Selector::<String>::new(symbol, freq),
        })
    }

    #[getter]
    pub(crate) fn symbol(&self) -> Option<String> {
        self.inner.symbol.clone()
    }

    #[getter]
    pub(crate) fn freq(&self) -> Option<PyFrequency> {
        self.inner.freq.map(|inner| PyFrequency { inner })
    }

    /// True when both fields are `None` — the `Pick` no-query case.
    pub(crate) fn is_empty(&self) -> bool {
        self.inner.is_empty()
    }

    /// Match this selector as a **query** against a `(symbol, freq)` entry
    /// tag: each `None` field on the query is a wildcard; a `Some` field
    /// must equal the entry's value. `entry` accepts a `Selector` (its
    /// fields are used as the tag) or a `(str | None, Frequency | str |
    /// None)` tuple.
    pub(crate) fn matches(&self, entry: &Bound<'_, PyAny>) -> PyResult<bool> {
        let entry_sel = coerce_selector(entry)?;
        Ok(self
            .inner
            .matches(entry_sel.symbol.as_ref(), entry_sel.freq))
    }

    pub(crate) fn __hash__(&self) -> u64 {
        use std::collections::hash_map::DefaultHasher;
        use std::hash::{Hash, Hasher};
        let mut h = DefaultHasher::new();
        // Hash the two fields directly — `Selector` itself no longer derives
        // `Hash` (it's a query predicate, not a HashMap key), but a Python-
        // side stable hash based on its fields is still useful for `in`
        // checks / set membership.
        self.inner.symbol.hash(&mut h);
        self.inner.freq.hash(&mut h);
        h.finish()
    }

    pub(crate) fn __eq__(&self, other: &Bound<'_, PyAny>) -> bool {
        match coerce_selector(other) {
            Ok(sel) => self.inner == sel,
            Err(_) => false,
        }
    }

    pub(crate) fn __ne__(&self, other: &Bound<'_, PyAny>) -> bool {
        !self.__eq__(other)
    }

    pub(crate) fn __repr__(&self) -> String {
        match (&self.inner.symbol, &self.inner.freq) {
            (Some(s), Some(f)) => format!("Selector(symbol={:?}, freq={:?})", s, f.as_token()),
            (Some(s), None) => format!("Selector(symbol={s:?})"),
            (None, Some(f)) => format!("Selector(freq={:?})", f.as_token()),
            (None, None) => "Selector()".to_string(),
        }
    }
}

/// Extract a [`Frequency`] from a Python `PyFrequency` or a token `str`.
pub(crate) fn coerce_frequency(obj: &Bound<'_, PyAny>) -> PyResult<Frequency> {
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
pub(crate) fn coerce_selector(obj: &Bound<'_, PyAny>) -> PyResult<Selector<String>> {
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
pub(crate) struct PySnapshot {
    pub(crate) inner: Snapshot<String>,
}

#[pymethods]
impl PySnapshot {
    #[new]
    #[pyo3(signature = (mapping = None))]
    pub(crate) fn new(mapping: Option<&Bound<'_, PyAny>>) -> PyResult<Self> {
        let inner = match mapping {
            None => Snapshot::<String>::new(),
            Some(m) => extract_snapshot(m)?,
        };
        Ok(Self { inner })
    }

    /// Read the atom matching `key`; raises `KeyError` if no entry matches
    /// (dict semantics). `key` is coerced to a [`Selector`] and matched
    /// wildcard-aware via [`Snapshot::find`] — a symbol-only key finds any
    /// entry with that symbol regardless of frequency.
    pub(crate) fn __getitem__(&self, key: &Bound<'_, PyAny>) -> PyResult<PyAtom> {
        let sel = coerce_selector(key)?;
        self.inner
            .find(&sel)
            .cloned()
            .map(|inner| PyAtom { inner })
            .ok_or_else(|| pyo3::exceptions::PyKeyError::new_err(format!("{sel:?}")))
    }

    /// Insert-or-overwrite: any entry whose tag matches `key` (under
    /// [`Selector::matches`]) is removed first, then a new entry is pushed
    /// with `key`'s `(symbol, freq)` tag and `atom` as the value. Matches
    /// Python's expectation that assignment overwrites the entry rather
    /// than accumulating duplicates.
    pub(crate) fn __setitem__(&mut self, key: &Bound<'_, PyAny>, atom: PyRef<'_, PyAtom>) -> PyResult<()> {
        let sel = coerce_selector(key)?;
        // Remove exact matches on the key's tag pattern, then push.
        self.inner.remove_matching(&sel);
        self.inner
            .push(sel.symbol, sel.freq, atom.inner.clone());
        Ok(())
    }

    pub(crate) fn __contains__(&self, key: &Bound<'_, PyAny>) -> PyResult<bool> {
        let sel = coerce_selector(key)?;
        Ok(self.inner.find(&sel).is_some())
    }

    pub(crate) fn __len__(&self) -> usize {
        self.inner.len()
    }

    /// Non-raising variant of `snap[key]` — returns `None` on a miss.
    pub(crate) fn get(&self, key: &Bound<'_, PyAny>) -> PyResult<Option<PyAtom>> {
        let sel = coerce_selector(key)?;
        Ok(self
            .inner
            .find(&sel)
            .cloned()
            .map(|inner| PyAtom { inner }))
    }

    /// Append a tagged atom to the snapshot. `key` supplies the `(symbol,
    /// freq)` tag (any [`Selector`]-coercible value); duplicates are
    /// allowed and future `snap[query]` reads return the first-inserted
    /// match. Rust's `push` semantics — for the dedup-on-write behaviour
    /// use `__setitem__` (i.e. `snap[key] = atom`).
    pub(crate) fn push(
        &mut self,
        key: &Bound<'_, PyAny>,
        atom: PyRef<'_, PyAtom>,
    ) -> PyResult<()> {
        let sel = coerce_selector(key)?;
        self.inner
            .push(sel.symbol, sel.freq, atom.inner.clone());
        Ok(())
    }

    /// Non-raising find: returns the first atom whose tag matches `query`,
    /// or `None`.
    pub(crate) fn find(&self, query: &Bound<'_, PyAny>) -> PyResult<Option<PyAtom>> {
        let sel = coerce_selector(query)?;
        Ok(self
            .inner
            .find(&sel)
            .cloned()
            .map(|inner| PyAtom { inner }))
    }

    /// The list of `(symbol, freq)` selectors present in this snapshot, in
    /// insertion order. Duplicates on the same tag surface as multiple
    /// selectors with the same fields.
    pub(crate) fn keys(&self) -> Vec<PySelector> {
        self.inner
            .iter()
            .map(|(sym, freq, _)| PySelector {
                inner: Selector::new(sym.cloned(), freq),
            })
            .collect()
    }

    /// True if this snapshot carries no assets.
    pub(crate) fn is_empty(&self) -> bool {
        self.inner.is_empty()
    }

    /// The sole atom in a single-entry snapshot, if there is exactly one.
    /// Returns `None` on an empty snapshot; raises `RuntimeError` (translated
    /// from a Rust panic) on 2+ entries — the same loud failure the no-query
    /// `pick()` uses.
    pub(crate) fn sole_atom(&self) -> Option<PyAtom> {
        self.inner.sole_atom().cloned().map(|inner| PyAtom { inner })
    }

    pub(crate) fn __repr__(&self) -> String {
        let keys: Vec<String> = self
            .inner
            .iter()
            .map(|(sym, freq, _)| {
                PySelector {
                    inner: Selector::new(sym.cloned(), freq),
                }
                .__repr__()
            })
            .collect();
        format!("Snapshot(keys=[{}])", keys.join(", "))
    }
}

// ---------------------------------------------------------------------------
// Atom-emitting source — the box behind the `pick()` leaf and the `.of(source)`
// method on every atom-input leaf constructor (close, high, year, ...).
// ---------------------------------------------------------------------------

/// A boxed `I -> Atom` indicator — the atom-emitting twin of `Source<I>`.
/// Now a type alias over the shared [`TypedSource`] carrier; the dedicated
/// `DynAtomIndicator<I>` trait + blanket impl it used to have collapsed
/// into [`runtime::Adapter`]'s coverage.
pub(crate) type AtomBox<I> = TypedSource<I, Atom>;

/// An atom-emitting source erased to one of the two input domains it can be
/// rooted in on the Python side: `Atom` (the identity passthrough) or
/// `Snapshot<String>` (a `Pick`). Feeds the optional `source=` argument every
/// atom-input leaf pyfunction accepts (`close(source=...)`, `year(source=...)`, …).
#[derive(Clone)]
pub(crate) enum AnyAtomSource {
    /// The trivial atom passthrough — `Identity<Atom>`, so the caller can build
    /// an atom-input leaf explicitly rooted on the atom stream itself. Kept in
    /// the enum for surface completeness; not currently produced by any leaf
    /// pyfunction (the raw-atom shape is already what a zero-arg `close()`
    /// returns).
    #[allow(dead_code)]
    Atom(AtomBox<Atom>),
    Snapshot(AtomBox<Snapshot<String>>),
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
pub(crate) struct PyAtomSource {
    pub(crate) inner: AnyAtomSource,
}

#[pymethods]
impl PyAtomSource {
    /// The most recent atom, without advancing state.
    pub(crate) fn value(&self) -> Option<PyAtom> {
        let out = match &self.inner {
            AnyAtomSource::Atom(s) => Indicator::value(s),
            AnyAtomSource::Snapshot(s) => Indicator::value(s),
        };
        out.map(|inner| PyAtom { inner })
    }

    /// Feed the next sample. Pass an `Atom` for an atom-rooted source (the
    /// trivial identity), a `Snapshot` for a `pick()`-rooted one.
    pub(crate) fn update(&mut self, sample: &Bound<'_, PyAny>) -> PyResult<Option<PyAtom>> {
        let out = match &mut self.inner {
            AnyAtomSource::Atom(s) => Indicator::update(s, extract_atom(sample)?),
            AnyAtomSource::Snapshot(s) => Indicator::update(s, extract_snapshot(sample)?),
        };
        Ok(out.map(|inner| PyAtom { inner }))
    }

    pub(crate) fn warm_up_period(&self) -> usize {
        match &self.inner {
            AnyAtomSource::Atom(s) => Indicator::warm_up_period(s),
            AnyAtomSource::Snapshot(s) => Indicator::warm_up_period(s),
        }
    }

    pub(crate) fn unstable_period(&self) -> usize {
        match &self.inner {
            AnyAtomSource::Atom(s) => Indicator::unstable_period(s),
            AnyAtomSource::Snapshot(s) => Indicator::unstable_period(s),
        }
    }

    pub(crate) fn stable_period(&self) -> usize {
        self.warm_up_period() + self.unstable_period()
    }

    pub(crate) fn reset(&mut self) {
        match &mut self.inner {
            AnyAtomSource::Atom(s) => Indicator::reset(s),
            AnyAtomSource::Snapshot(s) => Indicator::reset(s),
        }
    }

    pub(crate) fn __repr__(&self) -> String {
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
pub(crate) struct PyIndicator {
    pub(crate) src: AnySource,
}

impl PyIndicator {
    pub(crate) fn wrap(src: AnySource) -> Self {
        PyIndicator { src }
    }
}

#[pymethods]
impl PyIndicator {
    /// Feed the next sample; returns the current value, or `None` while warming
    /// up. Pass a `Candle` for a candle-rooted indicator, a `float` for an
    /// identity-rooted one.
    pub(crate) fn update(&mut self, sample: &Bound<'_, PyAny>) -> PyResult<Option<f64>> {
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
    pub(crate) fn feed(&mut self, py: Python<'_>, data: &Bound<'_, PyAny>) -> PyResult<Py<PyAny>> {
        let kind = OutputKind::detect(data)?;
        let values = self.src.feed_rows(data)?;
        build_floats(py, &kind, values)
    }

    /// The most recent value, without advancing state.
    pub(crate) fn value(&self) -> Option<f64> {
        self.src.value()
    }

    /// Whether enough samples have been seen to produce a value.
    pub(crate) fn is_ready(&self) -> bool {
        self.src.value().is_some()
    }

    /// The number of samples needed before the first value can appear.
    pub(crate) fn warm_up_period(&self) -> usize {
        self.src.warm_up_period()
    }

    /// Extra samples after warm-up before a recursive indicator (EMA, RSI, …)
    /// has effectively converged; `0` for windowed indicators.
    pub(crate) fn unstable_period(&self) -> usize {
        self.src.unstable_period()
    }

    /// `warm_up_period() + unstable_period()`: how much history to feed before
    /// trusting the output.
    pub(crate) fn stable_period(&self) -> usize {
        self.src.warm_up_period() + self.src.unstable_period()
    }

    /// Reset all internal state to freshly-constructed.
    pub(crate) fn reset(&mut self) {
        self.src.reset()
    }

    // --- comparisons -> Signal -------------------------------------------------
    #[pyo3(signature = (other, epsilon = None))]
    pub(crate) fn gt(&self, other: &Bound<'_, PyAny>, epsilon: Option<f64>) -> PyResult<PySignal> {
        let rhs = coerce_operand(other)?;
        let eps = epsilon.unwrap_or(DEFAULT_EPSILON);
        Ok(PySignal::wrap(sources_to_signal!(
            self.src.clone(),
            rhs,
            |l, r| Combine::<_, _, GtOp>::with_epsilon(l, r, eps)
        )?))
    }

    #[pyo3(signature = (other, epsilon = None))]
    pub(crate) fn lt(&self, other: &Bound<'_, PyAny>, epsilon: Option<f64>) -> PyResult<PySignal> {
        let rhs = coerce_operand(other)?;
        let eps = epsilon.unwrap_or(DEFAULT_EPSILON);
        Ok(PySignal::wrap(sources_to_signal!(
            self.src.clone(),
            rhs,
            |l, r| Combine::<_, _, LtOp>::with_epsilon(l, r, eps)
        )?))
    }

    #[pyo3(signature = (other, epsilon = None))]
    pub(crate) fn ge(&self, other: &Bound<'_, PyAny>, epsilon: Option<f64>) -> PyResult<PySignal> {
        let rhs = coerce_operand(other)?;
        let eps = epsilon.unwrap_or(DEFAULT_EPSILON);
        Ok(PySignal::wrap(sources_to_signal!(
            self.src.clone(),
            rhs,
            |l, r| Combine::<_, _, GeOp>::with_epsilon(l, r, eps)
        )?))
    }

    #[pyo3(signature = (other, epsilon = None))]
    pub(crate) fn le(&self, other: &Bound<'_, PyAny>, epsilon: Option<f64>) -> PyResult<PySignal> {
        let rhs = coerce_operand(other)?;
        let eps = epsilon.unwrap_or(DEFAULT_EPSILON);
        Ok(PySignal::wrap(sources_to_signal!(
            self.src.clone(),
            rhs,
            |l, r| Combine::<_, _, LeOp>::with_epsilon(l, r, eps)
        )?))
    }

    #[pyo3(signature = (other, epsilon = None))]
    pub(crate) fn eq(&self, other: &Bound<'_, PyAny>, epsilon: Option<f64>) -> PyResult<PySignal> {
        let rhs = coerce_operand(other)?;
        let eps = epsilon.unwrap_or(DEFAULT_EPSILON);
        Ok(PySignal::wrap(sources_to_signal!(
            self.src.clone(),
            rhs,
            |l, r| Combine::<_, _, EqOp>::with_epsilon(l, r, eps)
        )?))
    }

    #[pyo3(signature = (other, epsilon = None))]
    pub(crate) fn ne(&self, other: &Bound<'_, PyAny>, epsilon: Option<f64>) -> PyResult<PySignal> {
        let rhs = coerce_operand(other)?;
        let eps = epsilon.unwrap_or(DEFAULT_EPSILON);
        Ok(PySignal::wrap(sources_to_signal!(
            self.src.clone(),
            rhs,
            |l, r| Combine::<_, _, NeOp>::with_epsilon(l, r, eps)
        )?))
    }

    /// `self > level` for a constant level.
    pub(crate) fn above(&self, level: f64) -> PySignal {
        PySignal::wrap(source_to_signal!(self.src.clone(), |s| s.above(level)))
    }

    /// `self < level` for a constant level.
    pub(crate) fn below(&self, level: f64) -> PySignal {
        PySignal::wrap(source_to_signal!(self.src.clone(), |s| s.below(level)))
    }

    /// `self` rises above `other` on this step.
    pub(crate) fn crosses_above(&self, other: &Bound<'_, PyAny>) -> PyResult<PySignal> {
        let rhs = coerce_operand(other)?;
        Ok(PySignal::wrap(sources_to_signal!(
            self.src.clone(),
            rhs,
            |l, r| l.crosses_above(r)
        )?))
    }

    /// `self` falls below `other` on this step.
    pub(crate) fn crosses_below(&self, other: &Bound<'_, PyAny>) -> PyResult<PySignal> {
        let rhs = coerce_operand(other)?;
        Ok(PySignal::wrap(sources_to_signal!(
            self.src.clone(),
            rhs,
            |l, r| l.crosses_below(r)
        )?))
    }

    // --- arithmetic -> Indicator ----------------------------------------------
    /// Pointwise `self + other` (`other` may be an Indicator or a number).
    pub(crate) fn add(&self, other: &Bound<'_, PyAny>) -> PyResult<PyIndicator> {
        let rhs = coerce_operand(other)?;
        Ok(PyIndicator::wrap(combine_sources!(
            self.src.clone(),
            rhs,
            |l, r| l.add(r)
        )?))
    }
    /// Pointwise `self - other`.
    pub(crate) fn sub(&self, other: &Bound<'_, PyAny>) -> PyResult<PyIndicator> {
        let rhs = coerce_operand(other)?;
        Ok(PyIndicator::wrap(combine_sources!(
            self.src.clone(),
            rhs,
            |l, r| l.sub(r)
        )?))
    }
    /// Pointwise `self * other`.
    pub(crate) fn mul(&self, other: &Bound<'_, PyAny>) -> PyResult<PyIndicator> {
        let rhs = coerce_operand(other)?;
        Ok(PyIndicator::wrap(combine_sources!(
            self.src.clone(),
            rhs,
            |l, r| l.mul(r)
        )?))
    }
    /// Pointwise `self / other` (`None` on divide-by-zero).
    pub(crate) fn div(&self, other: &Bound<'_, PyAny>) -> PyResult<PyIndicator> {
        let rhs = coerce_operand(other)?;
        Ok(PyIndicator::wrap(combine_sources!(
            self.src.clone(),
            rhs,
            |l, r| l.div(r)
        )?))
    }

    pub(crate) fn __add__(&self, other: &Bound<'_, PyAny>) -> PyResult<PyIndicator> {
        self.add(other)
    }
    pub(crate) fn __sub__(&self, other: &Bound<'_, PyAny>) -> PyResult<PyIndicator> {
        self.sub(other)
    }
    pub(crate) fn __mul__(&self, other: &Bound<'_, PyAny>) -> PyResult<PyIndicator> {
        self.mul(other)
    }
    pub(crate) fn __truediv__(&self, other: &Bound<'_, PyAny>) -> PyResult<PyIndicator> {
        self.div(other)
    }
    pub(crate) fn __radd__(&self, other: &Bound<'_, PyAny>) -> PyResult<PyIndicator> {
        let lhs = coerce_operand(other)?;
        Ok(PyIndicator::wrap(combine_sources!(
            lhs,
            self.src.clone(),
            |l, r| l.add(r)
        )?))
    }
    pub(crate) fn __rsub__(&self, other: &Bound<'_, PyAny>) -> PyResult<PyIndicator> {
        let lhs = coerce_operand(other)?;
        Ok(PyIndicator::wrap(combine_sources!(
            lhs,
            self.src.clone(),
            |l, r| l.sub(r)
        )?))
    }
    pub(crate) fn __rmul__(&self, other: &Bound<'_, PyAny>) -> PyResult<PyIndicator> {
        let lhs = coerce_operand(other)?;
        Ok(PyIndicator::wrap(combine_sources!(
            lhs,
            self.src.clone(),
            |l, r| l.mul(r)
        )?))
    }
    pub(crate) fn __rtruediv__(&self, other: &Bound<'_, PyAny>) -> PyResult<PyIndicator> {
        let lhs = coerce_operand(other)?;
        Ok(PyIndicator::wrap(combine_sources!(
            lhs,
            self.src.clone(),
            |l, r| l.div(r)
        )?))
    }

    // --- lookback / rolling -> Indicator --------------------------------------
    /// `self` delayed by `period` steps.
    pub(crate) fn lag(&self, period: usize) -> PyIndicator {
        PyIndicator::wrap(map_source!(self.src.clone(), |s| s.lag(period)))
    }
    /// Discrete difference over `period` steps (`x[t] - x[t-n]`).
    pub(crate) fn diff(&self, period: usize) -> PyIndicator {
        PyIndicator::wrap(map_source!(self.src.clone(), |s| s.diff(period)))
    }
    /// Ratio to the value `period` steps ago (`x[t] / x[t-n]`).
    pub(crate) fn ratio(&self, period: usize) -> PyIndicator {
        PyIndicator::wrap(map_source!(self.src.clone(), |s| s.ratio(period)))
    }
    /// Percentage rate of change over `period` steps.
    pub(crate) fn roc(&self, period: usize) -> PyIndicator {
        PyIndicator::wrap(map_source!(self.src.clone(), |s| s.roc(period)))
    }
    /// Rolling maximum over `period` steps.
    pub(crate) fn rolling_max(&self, period: usize) -> PyResult<PyIndicator> {
        ensure_period(period)?;
        Ok(PyIndicator::wrap(
            map_source!(self.src.clone(), |s| s.rolling_max(period))
        ))
    }
    /// Rolling minimum over `period` steps.
    pub(crate) fn rolling_min(&self, period: usize) -> PyResult<PyIndicator> {
        ensure_period(period)?;
        Ok(PyIndicator::wrap(
            map_source!(self.src.clone(), |s| s.rolling_min(period))
        ))
    }

    /// Logarithm of `self` in `base` (default: natural log, `e`). Emits `None`
    /// on samples where the input is non-positive.
    #[pyo3(signature = (base = std::f64::consts::E))]
    pub(crate) fn log(&self, base: f64) -> PyResult<PyIndicator> {
        ensure_log_base(base)?;
        Ok(PyIndicator::wrap(
            map_source!(self.src.clone(), |s| Log::new(s, base))
        ))
    }

    /// Passthrough that forces this indicator's reported `unstable_period()` to
    /// `0`. Output and `warm_up_period()` are unchanged; a downstream reader of
    /// `stable_period()` (a strategy readiness gate, an overlay trim) no longer
    /// waits for this subtree's IIR settling tail. Use to explicitly opt out of
    /// the safe default that waits for it.
    pub(crate) fn unstable(&self) -> PyIndicator {
        PyIndicator::wrap(map_source!(self.src.clone(), |s| s.unstable()))
    }

    pub(crate) fn __repr__(&self) -> String {
        match self.src.value() {
            Some(v) => format!("Indicator(value={v})"),
            None => "Indicator(value=None)".to_string(),
        }
    }
}

/// A boolean signal. Combine signals with `&` / `|` / `^` / `~` (or the named
/// `and_` / `or_` / `xor_` / `not_` / `changed` methods).
#[pyclass(name = "Signal")]
pub(crate) struct PySignal {
    pub(crate) sig: AnySignal,
}

impl PySignal {
    pub(crate) fn wrap(sig: AnySignal) -> Self {
        PySignal { sig }
    }
}

#[pymethods]
impl PySignal {
    /// Feed the next sample; returns the current boolean state. Pass a `Candle`
    /// for a candle-rooted signal, a `float` for an identity-rooted one.
    pub(crate) fn update(&mut self, sample: &Bound<'_, PyAny>) -> PyResult<bool> {
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
    pub(crate) fn feed(&mut self, py: Python<'_>, data: &Bound<'_, PyAny>) -> PyResult<Py<PyAny>> {
        let kind = OutputKind::detect(data)?;
        let values = self.sig.feed_rows(data)?;
        build_bools(py, &kind, values)
    }

    /// The most recent boolean state, without advancing.
    pub(crate) fn is_true(&self) -> bool {
        self.sig.is_true()
    }

    /// The number of samples needed before the signal can produce a real state
    /// (it reads `False` while warming up).
    pub(crate) fn warm_up_period(&self) -> usize {
        self.sig.warm_up_period()
    }

    /// Extra samples after warm-up before any recursive sources inside the
    /// signal have effectively converged; `0` for windowed ones.
    pub(crate) fn unstable_period(&self) -> usize {
        self.sig.unstable_period()
    }

    /// `warm_up_period() + unstable_period()`: how much history to feed before
    /// trusting the signal.
    pub(crate) fn stable_period(&self) -> usize {
        self.sig.warm_up_period() + self.sig.unstable_period()
    }

    /// Reset all internal state.
    pub(crate) fn reset(&mut self) {
        self.sig.reset()
    }

    /// Logical AND.
    pub(crate) fn and_(&self, other: PyRef<'_, PySignal>) -> PyResult<PySignal> {
        Ok(PySignal::wrap(combine_signals!(
            self.sig.clone(),
            other.sig.clone(),
            |a, b| a.and(b)
        )?))
    }
    /// Logical OR.
    pub(crate) fn or_(&self, other: PyRef<'_, PySignal>) -> PyResult<PySignal> {
        Ok(PySignal::wrap(combine_signals!(
            self.sig.clone(),
            other.sig.clone(),
            |a, b| a.or(b)
        )?))
    }
    /// Logical XOR.
    pub(crate) fn xor_(&self, other: PyRef<'_, PySignal>) -> PyResult<PySignal> {
        Ok(PySignal::wrap(combine_signals!(
            self.sig.clone(),
            other.sig.clone(),
            |a, b| a.xor(b)
        )?))
    }
    /// Logical NOT.
    pub(crate) fn not_(&self) -> PySignal {
        PySignal::wrap(map_signal!(self.sig.clone(), |s| s.not()))
    }
    /// Fires on the single step where this signal toggles (either direction).
    pub(crate) fn changed(&self) -> PySignal {
        PySignal::wrap(map_signal!(self.sig.clone(), |s| s.changed()))
    }

    /// Passthrough that forces this signal's reported `unstable_period()` to
    /// `0`. Output and `warm_up_period()` are unchanged; a downstream reader of
    /// `stable_period()` (a strategy readiness gate) no longer waits for this
    /// subtree's IIR settling tail. Mirrors the free `unstable(x)` function; use
    /// to explicitly opt out of the safe default that waits for the tail.
    pub(crate) fn unstable(&self) -> PySignal {
        PySignal::wrap(map_signal!(self.sig.clone(), |s| s.unstable()))
    }

    pub(crate) fn __and__(&self, other: PyRef<'_, PySignal>) -> PyResult<PySignal> {
        self.and_(other)
    }
    pub(crate) fn __or__(&self, other: PyRef<'_, PySignal>) -> PyResult<PySignal> {
        self.or_(other)
    }
    pub(crate) fn __xor__(&self, other: PyRef<'_, PySignal>) -> PyResult<PySignal> {
        self.xor_(other)
    }
    pub(crate) fn __invert__(&self) -> PySignal {
        self.not_()
    }

    pub(crate) fn __repr__(&self) -> String {
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
pub(crate) struct PyStrSource {
    pub(crate) src: AnyStrSource,
}

impl PyStrSource {
    pub(crate) fn wrap(src: AnyStrSource) -> Self {
        PyStrSource { src }
    }
}

#[pymethods]
impl PyStrSource {
    /// Feed the next sample; returns the current string, or `None` while
    /// warming up. Always accepts an `Atom` (or a `Candle`, lifted to an
    /// overlay-free atom — which makes an overlay-reading source yield
    /// `None`).
    pub(crate) fn update(&mut self, sample: &Bound<'_, PyAny>) -> PyResult<Option<String>> {
        let out = match &mut self.src {
            AnyStrSource::Candle(s) => Indicator::update(s, extract_atom(sample)?),
            AnyStrSource::Snapshot(s) => Indicator::update(s, extract_snapshot(sample)?),
            AnyStrSource::Const(c) => {
                // Still validate the sample so a constant behaves like any
                // other source when handed nonsense.
                extract_atom(sample)?;
                Some(c.clone())
            }
        };
        Ok(out.map(|s| s.to_string()))
    }

    /// The most recent value, without advancing state.
    pub(crate) fn value(&self) -> Option<String> {
        self.src.value().map(|s| s.to_string())
    }

    pub(crate) fn is_ready(&self) -> bool {
        self.src.value().is_some()
    }

    pub(crate) fn warm_up_period(&self) -> usize {
        self.src.warm_up_period()
    }

    pub(crate) fn unstable_period(&self) -> usize {
        self.src.unstable_period()
    }

    pub(crate) fn stable_period(&self) -> usize {
        self.src.warm_up_period() + self.src.unstable_period()
    }

    pub(crate) fn reset(&mut self) {
        self.src.reset()
    }

    /// `self == other` — build a boolean signal that fires when both string
    /// sources agree. `other` may be another `StrSource` or a Python `str`
    /// (lifted to a `ValueStr` constant).
    pub(crate) fn eq(&self, other: &Bound<'_, PyAny>) -> PyResult<PySignal> {
        let rhs = coerce_str_operand(other)?;
        Ok(match str_pair(self.src.clone(), rhs)? {
            StrPair::Candle(l, r) => PySignal::wrap(AnySignal::Candle(SignalBox::new(
                Combine::<_, _, StrEqOp>::new(l, r),
            ))),
            StrPair::Snapshot(l, r) => PySignal::wrap(AnySignal::Snapshot(SignalBox::new(
                Combine::<_, _, StrEqOp>::new(l, r),
            ))),
        })
    }

    /// `self != other` — the string counterpart to [`eq`](Self::eq).
    pub(crate) fn ne(&self, other: &Bound<'_, PyAny>) -> PyResult<PySignal> {
        let rhs = coerce_str_operand(other)?;
        Ok(match str_pair(self.src.clone(), rhs)? {
            StrPair::Candle(l, r) => PySignal::wrap(AnySignal::Candle(SignalBox::new(
                Combine::<_, _, StrNeOp>::new(l, r),
            ))),
            StrPair::Snapshot(l, r) => PySignal::wrap(AnySignal::Snapshot(SignalBox::new(
                Combine::<_, _, StrNeOp>::new(l, r),
            ))),
        })
    }

    pub(crate) fn __repr__(&self) -> String {
        match self.src.value() {
            Some(v) => format!("StrSource(value={v:?})"),
            None => "StrSource(value=None)".to_string(),
        }
    }
}

/// Coerce a Python operand for a string-comparison RHS: accepts either a
/// `PyStrSource` or a Python `str` (lifted to `AnyStrSource::Const`).
pub(crate) fn coerce_str_operand(other: &Bound<'_, PyAny>) -> PyResult<AnyStrSource> {
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
pub(crate) struct PyMulti {
    pub(crate) inner: AnyMulti,
}

#[pymethods]
impl PyMulti {
    /// Feed the next sample; returns a dict of output lines, or `None` while
    /// warming up. Pass a `Candle` for a candle-rooted indicator, a `float` for
    /// an identity-rooted one.
    pub(crate) fn update<'py>(
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
    pub(crate) fn feed(&mut self, py: Python<'_>, data: &Bound<'_, PyAny>) -> PyResult<Py<PyAny>> {
        let kind = OutputKind::detect(data)?;
        let names = self.inner.names();
        let rows = self.inner.feed_rows(data)?;
        build_multi(py, &kind, names, rows)
    }

    /// The most recent output dict, without advancing.
    pub(crate) fn value<'py>(&self, py: Python<'py>) -> PyResult<Option<Bound<'py, PyDict>>> {
        let names = self.inner.names();
        match self.inner.value() {
            Some(values) => Ok(Some(values_to_dict(py, names, &values)?)),
            None => Ok(None),
        }
    }

    /// Whether enough samples have been seen to produce a value.
    pub(crate) fn is_ready(&self) -> bool {
        self.inner.value().is_some()
    }

    /// The number of samples needed before the first value can appear (for the
    /// slowest output line).
    pub(crate) fn warm_up_period(&self) -> usize {
        self.inner.warm_up_period()
    }

    /// Extra samples after warm-up before a recursive indicator (MACD, ADX, …)
    /// has effectively converged; `0` for windowed indicators.
    pub(crate) fn unstable_period(&self) -> usize {
        self.inner.unstable_period()
    }

    /// `warm_up_period() + unstable_period()`: how much history to feed before
    /// trusting the output.
    pub(crate) fn stable_period(&self) -> usize {
        self.inner.warm_up_period() + self.inner.unstable_period()
    }

    /// Reset all internal state.
    pub(crate) fn reset(&mut self) {
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
    pub(crate) fn shared(&self) -> PySharedMulti {
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
pub(crate) struct PySharedMulti {
    pub(crate) inner: AnySharedMulti,
}

/// Emit the accessor list on `PySharedMulti`. Each generated method resolves
/// the name against the underlying multi's field list (declared once per
/// concrete `MultiOutput` impl); an accessor whose name doesn't match a field
/// of *this* particular multi errors clearly at call time.
#[pymethods]
impl PySharedMulti {
    /// MACD line (fast EMA − slow EMA) as a standalone indicator.
    pub(crate) fn macd(&self) -> PyResult<PyIndicator> {
        self.inner.project("macd")
    }
    /// MACD line — alias for [`macd`](Self::macd), matching Rust's
    /// `Macd::line()` accessor.
    pub(crate) fn line(&self) -> PyResult<PyIndicator> {
        self.inner.project("macd")
    }
    /// MACD signal line (EMA of the MACD line).
    pub(crate) fn signal(&self) -> PyResult<PyIndicator> {
        self.inner.project("signal")
    }
    /// MACD histogram (line − signal).
    pub(crate) fn histogram(&self) -> PyResult<PyIndicator> {
        self.inner.project("histogram")
    }
    /// Bollinger / Keltner / Donchian upper band.
    pub(crate) fn upper(&self) -> PyResult<PyIndicator> {
        self.inner.project("upper")
    }
    /// Bollinger / Keltner / Donchian middle band.
    pub(crate) fn middle(&self) -> PyResult<PyIndicator> {
        self.inner.project("middle")
    }
    /// Bollinger / Keltner / Donchian lower band.
    pub(crate) fn lower(&self) -> PyResult<PyIndicator> {
        self.inner.project("lower")
    }
    /// ADX / DMI positive directional indicator, `+DI`.
    pub(crate) fn plus_di(&self) -> PyResult<PyIndicator> {
        self.inner.project("plus_di")
    }
    /// ADX / DMI negative directional indicator, `−DI`.
    pub(crate) fn minus_di(&self) -> PyResult<PyIndicator> {
        self.inner.project("minus_di")
    }
    /// ADX line (the trend-strength value).
    pub(crate) fn adx(&self) -> PyResult<PyIndicator> {
        self.inner.project("adx")
    }
    /// Aroon Up.
    pub(crate) fn up(&self) -> PyResult<PyIndicator> {
        self.inner.project("up")
    }
    /// Aroon Down.
    pub(crate) fn down(&self) -> PyResult<PyIndicator> {
        self.inner.project("down")
    }
    /// Aroon oscillator (up − down).
    pub(crate) fn oscillator(&self) -> PyResult<PyIndicator> {
        self.inner.project("oscillator")
    }

    /// The output field names available on the underlying multi.
    pub(crate) fn names(&self) -> Vec<String> {
        self.inner.names().iter().map(|s| s.to_string()).collect()
    }

    /// Project the component named `name` (one of [`names`](Self::names)) as a
    /// standalone [`Indicator`]. Prefer the named accessors when one matches;
    /// this is the fallback for programmatic lookup.
    pub(crate) fn component(&self, name: &str) -> PyResult<PyIndicator> {
        self.inner.project(name)
    }

    pub(crate) fn __repr__(&self) -> String {
        let names = self.inner.names();
        format!("SharedMultiIndicator(fields={names:?})")
    }
}

