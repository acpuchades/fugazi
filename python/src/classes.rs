use crate::prelude::*;
// The binding modules were one flat namespace before the split and still read
// as one: each pulls in its siblings, so a cross-module reference needs no path.
#[allow(unused_imports)]
use crate::carriers::*;
#[allow(unused_imports)]
use crate::constructors::*;
#[allow(unused_imports)]
use crate::metrics::*;
#[allow(unused_imports)]
use crate::sources::*;
#[allow(unused_imports)]
use crate::spec::*;
#[allow(unused_imports)]
use crate::strategy::*;

// ---------------------------------------------------------------------------
// Python classes
// ---------------------------------------------------------------------------

/// A single OHLCV bar.
#[pyclass(name = "Candle", module = "fugazi", frozen, skip_from_py_object)]
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
    /// Support `pickle` / `copy.deepcopy` by naming the constructor and the five
    /// fields that rebuild it.
    ///
    /// This is what lets a `Candle` cross a `multiprocessing` / `joblib` /
    /// `ProcessPoolExecutor` boundary — the standard way a Python caller fans a
    /// backtest out over cores. It only works because the class declares
    /// `module = "fugazi"`; pickle stores a type by `module.qualname` and every
    /// pyclass here used to answer `builtins`.
    pub(crate) fn __reduce__(&self, py: Python<'_>) -> PyResult<Py<PyAny>> {
        reduce_with(
            py,
            py.get_type::<PyCandle>(),
            (
                self.open(),
                self.high(),
                self.low(),
                self.close(),
                self.volume(),
            ),
        )
    }

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
#[pyclass(name = "Schema", module = "fugazi", frozen, skip_from_py_object)]
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

    /// Rebuild through [`_rebuild_schema`] — a `Schema` is frozen and has no
    /// `__new__` (a `SchemaBuilder` produces it), so the reconstruction replays
    /// the builder rather than reaching past it.
    pub(crate) fn __reduce__(&self, py: Python<'_>) -> PyResult<Py<PyAny>> {
        let types: Vec<&'static str> = (0..self.inner.len())
            .map(|i| {
                self.inner
                    .type_of(i)
                    .map(overlay_type_name)
                    .unwrap_or("real")
            })
            .collect();
        reduce_with(
            py,
            py.import("fugazi")?.getattr("_rebuild_schema")?,
            (self.keys(), types),
        )
    }

    /// Iterate the column names, in insertion order — so `list(schema)` and
    /// `for key in schema` read the way `len(schema)` and `key in schema`
    /// already promised.
    ///
    /// Without this a `__len__` + `__getitem__`-shaped type falls back to
    /// Python's *legacy* sequence-iteration protocol, which probes integer
    /// indices and surfaces whatever error that produces instead of a plain
    /// `TypeError: not iterable`.
    pub(crate) fn __iter__(&self, py: Python<'_>) -> PyResult<Py<PyAny>> {
        iter_over(py, self.keys())
    }

    pub(crate) fn __repr__(&self) -> String {
        let cols: Vec<String> = self
            .inner
            .keys()
            .map(|k| {
                let ty = self
                    .inner
                    .type_of_key(k)
                    .map(overlay_type_name)
                    .unwrap_or("?");
                format!("{k}:{ty}")
            })
            .collect();
        format!("Schema(columns={cols:?})")
    }
}

/// Build the 2-tuple `__reduce__` return value: `(callable, args)`.
///
/// `callable` is either the class itself (when its `__new__` takes every field
/// positionally) or a module-level `_rebuild_*` function (when it doesn't —
/// keyword-only parameters, or a type Python cannot construct at all, like
/// `Trade`). Either way pickle stores it by `module.qualname` and calls it with
/// `args` on the way back in, so the reconstruction path is ordinary public
/// behaviour rather than a parallel deserializer that can drift.
pub(crate) fn reduce_with<'py, C, A>(py: Python<'py>, callable: C, args: A) -> PyResult<Py<PyAny>>
where
    C: pyo3::IntoPyObject<'py>,
    A: pyo3::IntoPyObject<'py>,
{
    // The tuple `IntoPyObject` impl's `Error` is already `PyErr`, so `?` needs
    // no conversion here.
    Ok((callable, args).into_pyobject(py)?.into_any().unbind())
}

/// Back an `__iter__` with a materialised `list`'s own iterator.
///
/// Every collection bound here is small and already fully realised on the Rust
/// side (a schema's columns, a snapshot's keys, a sweep's rows), so the honest
/// implementation is to hand Python a list iterator rather than a bespoke
/// `__next__` pyclass holding a cursor into borrowed state — which would also
/// have to answer what happens when the collection mutates mid-iteration.
pub(crate) fn iter_over<T>(py: Python<'_>, items: Vec<T>) -> PyResult<Py<PyAny>>
where
    T: for<'py> pyo3::IntoPyObject<'py>,
{
    let list = pyo3::types::PyList::new(py, items)?;
    Ok(list.try_iter()?.into_any().unbind())
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
#[pyclass(name = "SchemaBuilder", module = "fugazi")]
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
            .ok_or_else(|| PyValueError::new_err("SchemaBuilder has already been finished"))
            .map(|b| b.len())
    }

    /// Freeze into an immutable [`Schema`]. The builder is consumed — further
    /// calls raise `ValueError`.
    pub(crate) fn finish(&mut self) -> PyResult<PySchema> {
        let builder = self
            .inner
            .take()
            .ok_or_else(|| PyValueError::new_err("SchemaBuilder has already been finished"))?;
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
        let builder = self
            .inner
            .as_mut()
            .ok_or_else(|| PyValueError::new_err("SchemaBuilder has already been finished"))?;
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
/// Cheap to clone: [`OverlayInfo`] is two `Arc`s (the shared schema and this
/// bar's values), so a clone is two atomic bumps and no allocation.
///
/// Both fields being `Arc` is also what makes this class **sendable**, and so
/// picklable. It carried `unsendable` long after the core moved off `Rc` — and
/// that flag makes pyo3 assert the accessing thread on *every* method call,
/// `__reduce__` included, which is precisely what `multiprocessing` does from
/// its queue feeder thread. Don't reinstate it without re-checking `OverlayInfo`:
/// the marker is what decides whether an `Atom` can leave the process.
#[pyclass(name = "OverlayInfo", module = "fugazi", frozen, skip_from_py_object)]
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

    /// The schema this bundle's slots are declared by.
    #[getter]
    pub(crate) fn schema(&self) -> PySchema {
        PySchema {
            inner: self.inner.schema().clone(),
        }
    }

    /// Every slot, in schema order, as native Python values — `None` for a slot
    /// absent this bar. Exactly the `values` argument `OverlayInfo(schema,
    /// values)` takes, so `OverlayInfo(o.schema, o.values)` reconstructs `o`.
    #[getter]
    pub(crate) fn values(&self, py: Python<'_>) -> PyResult<Vec<Py<PyAny>>> {
        (0..self.inner.values().len())
            .map(|i| match self.inner.get(i) {
                Some(v) => overlay_to_python(py, v),
                None => Ok(py.None()),
            })
            .collect()
    }

    pub(crate) fn __reduce__(&self, py: Python<'_>) -> PyResult<Py<PyAny>> {
        reduce_with(
            py,
            py.get_type::<PyOverlayInfo>(),
            (self.schema(), self.values(py)?),
        )
    }

    pub(crate) fn __len__(&self) -> usize {
        self.inner.values().len()
    }

    /// Read the value at a resolved column index as its native Python type
    /// (`float` for `Real`, `bool` for `Bool`, `str` for `Str`), or `None` if
    /// the index is out of bounds.
    pub(crate) fn get(&self, py: Python<'_>, index: usize) -> Option<Py<PyAny>> {
        self.inner
            .get(index)
            .and_then(|v| overlay_to_python(py, v).ok())
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

pub(crate) fn slot_type_error(
    slot: usize,
    declared: OverlayType,
    bound: &Bound<'_, PyAny>,
) -> PyErr {
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
#[pyclass(name = "Atom", module = "fugazi", frozen, skip_from_py_object)]
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
    /// mark-to-market, and the sole-atom unpack skips it, so attaching one cannot
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

    pub(crate) fn __reduce__(&self, py: Python<'_>) -> PyResult<Py<PyAny>> {
        reduce_with(
            py,
            py.get_type::<PyAtom>(),
            (self.candle(), self.overlays(), self.time()),
        )
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
            (Some(ov), None) => format!("Atom(candle={:?}, overlays={:?})", candle, ov.values(),),
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
#[pyclass(name = "Frequency", module = "fugazi", frozen, skip_from_py_object)]
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

    pub(crate) fn __reduce__(&self, py: Python<'_>) -> PyResult<Py<PyAny>> {
        reduce_with(py, py.get_type::<PyFrequency>(), (self.__str__(),))
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
#[pyclass(name = "Selector", module = "fugazi", frozen, skip_from_py_object)]
#[derive(Clone)]
pub(crate) struct PySelector {
    pub(crate) inner: Selector<Symbol>,
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
            inner: Selector::<Symbol>::new(symbol.map(intern), freq),
        })
    }

    #[getter]
    pub(crate) fn symbol(&self) -> Option<String> {
        self.inner.symbol.as_ref().map(|s| s.to_string())
    }

    #[getter]
    pub(crate) fn freq(&self) -> Option<PyFrequency> {
        self.inner.freq.map(|inner| PyFrequency { inner })
    }

    pub(crate) fn __reduce__(&self, py: Python<'_>) -> PyResult<Py<PyAny>> {
        reduce_with(
            py,
            py.get_type::<PySelector>(),
            (self.symbol(), self.freq()),
        )
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
pub(crate) fn coerce_selector(obj: &Bound<'_, PyAny>) -> PyResult<Selector<Symbol>> {
    if let Ok(sel) = obj.cast::<PySelector>() {
        return Ok(sel.borrow().inner.clone());
    }
    if let Ok(f) = obj.cast::<PyFrequency>() {
        return Ok(Selector::by_freq(f.borrow().inner));
    }
    if let Ok(s) = obj.cast::<pyo3::types::PyString>()
        && let Ok(s) = s.to_cow()
    {
        // Straight to a `Symbol`: extracting a `String` first would allocate
        // twice (once for the `String`, once for the `Arc`) and throw the first
        // away. Callers converting a whole series should go through
        // `SymbolInterner` instead, which allocates once per *distinct* symbol.
        return Ok(Selector::by_symbol(intern(s.as_ref())));
    }
    if let Ok((sym, freq)) = obj.extract::<(String, Option<Py<PyAny>>)>() {
        let freq = match freq {
            None => None,
            Some(f) => Some(coerce_frequency(f.bind(obj.py()))?),
        };
        return Ok(Selector::new(Some(intern(sym)), freq));
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
#[pyclass(name = "Snapshot", module = "fugazi", skip_from_py_object)]
#[derive(Clone)]
pub(crate) struct PySnapshot {
    pub(crate) inner: Snapshot<Symbol>,
}

#[pymethods]
impl PySnapshot {
    #[new]
    #[pyo3(signature = (mapping = None))]
    pub(crate) fn new(mapping: Option<&Bound<'_, PyAny>>) -> PyResult<Self> {
        let inner = match mapping {
            None => Snapshot::<Symbol>::new(),
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
    pub(crate) fn __setitem__(
        &mut self,
        key: &Bound<'_, PyAny>,
        atom: PyRef<'_, PyAtom>,
    ) -> PyResult<()> {
        let sel = coerce_selector(key)?;
        // Remove exact matches on the key's tag pattern, then push.
        self.inner.remove_matching(&sel);
        self.inner.push(sel.symbol, sel.freq, atom.inner.clone());
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
        Ok(self.inner.find(&sel).cloned().map(|inner| PyAtom { inner }))
    }

    /// Append a tagged atom to the snapshot. `key` supplies the `(symbol,
    /// freq)` tag (any [`Selector`]-coercible value); duplicates are
    /// allowed and future `snap[query]` reads return the first-inserted
    /// match. Rust's `push` semantics — for the dedup-on-write behaviour
    /// use `__setitem__` (i.e. `snap[key] = atom`).
    pub(crate) fn push(&mut self, key: &Bound<'_, PyAny>, atom: PyRef<'_, PyAtom>) -> PyResult<()> {
        let sel = coerce_selector(key)?;
        self.inner.push(sel.symbol, sel.freq, atom.inner.clone());
        Ok(())
    }

    /// Non-raising find: returns the first atom whose tag matches `query`,
    /// or `None`.
    pub(crate) fn find(&self, query: &Bound<'_, PyAny>) -> PyResult<Option<PyAtom>> {
        let sel = coerce_selector(query)?;
        Ok(self.inner.find(&sel).cloned().map(|inner| PyAtom { inner }))
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

    /// Every atom in this snapshot, in insertion order — the `values()` half of
    /// the mapping shape `keys()` starts.
    pub(crate) fn values(&self) -> Vec<PyAtom> {
        self.inner
            .iter()
            .map(|(_, _, atom)| PyAtom {
                inner: atom.clone(),
            })
            .collect()
    }

    /// `(selector, atom)` pairs, in insertion order — so `dict(snapshot.items())`
    /// and `for sel, atom in snapshot.items()` work.
    pub(crate) fn items(&self) -> Vec<(PySelector, PyAtom)> {
        self.inner
            .iter()
            .map(|(sym, freq, atom)| {
                (
                    PySelector {
                        inner: Selector::new(sym.cloned(), freq),
                    },
                    PyAtom {
                        inner: atom.clone(),
                    },
                )
            })
            .collect()
    }

    /// Iterate the `Selector` keys, in insertion order — matching `keys()`, the
    /// way a `dict` iterates its keys.
    ///
    /// Regression-worthy: `Snapshot` has `__len__` and `__getitem__`, and
    /// without `__iter__` Python falls back to the legacy sequence protocol and
    /// probes `snapshot[0]`. That went through `coerce_selector`, so `list(snap)`
    /// reported *"keys must be a Selector, a str, a Frequency, or a tuple"* —
    /// an error about key types, for an iteration the caller never asked to do
    /// by index.
    pub(crate) fn __iter__(&self, py: Python<'_>) -> PyResult<Py<PyAny>> {
        iter_over(py, self.keys())
    }

    /// Rebuild by replaying [`push`](Self::push) over `items()`.
    ///
    /// Deliberately **not** the `Snapshot(mapping)` constructor: a snapshot may
    /// legitimately carry two entries under one tag — the same symbol at two
    /// cadences — and routing those through a `dict` would silently collapse
    /// them. `push` keeps duplicates and insertion order, so the round-trip is
    /// exact.
    pub(crate) fn __reduce__(&self, py: Python<'_>) -> PyResult<Py<PyAny>> {
        reduce_with(
            py,
            py.import("fugazi")?.getattr("_rebuild_snapshot")?,
            (self.items(),),
        )
    }

    /// True if this snapshot carries no assets.
    pub(crate) fn is_empty(&self) -> bool {
        self.inner.is_empty()
    }

    /// The sole atom in a single-entry snapshot, if there is exactly one.
    /// Returns `None` on an empty snapshot; raises `ValueError` on 2+ entries —
    /// the same ambiguity the no-query `pick()` refuses.
    ///
    /// `ValueError`, not the `PanicException` this used to raise by letting a
    /// Rust panic unwind across the FFI boundary. `PanicException` derives from
    /// `BaseException`, so `except Exception` walked straight past it and a
    /// caller could not handle this at all without knowing to catch
    /// `BaseException` — which would also swallow their own `KeyboardInterrupt`.
    /// The Rust `Snapshot::sole_atom_or_panic` still panics, because it is
    /// called from `Indicator::update`, which has no error channel to return
    /// through; this binding goes through the fallible spelling,
    /// `Snapshot::sole_atom_or_err`.
    pub(crate) fn sole_atom(&self) -> PyResult<Option<PyAtom>> {
        match self.inner.sole_atom_or_err() {
            Ok(atom) => Ok(atom.cloned().map(|inner| PyAtom { inner })),
            Err(n) => Err(PyValueError::new_err(format!(
                "sole_atom: this snapshot carries {n} priceable series, so there is no \
                 single one to unpack. Name the one you want with `pick(symbol)`, or use \
                 `any_atom()` if any entry will do (calendar reads, which only touch the \
                 shared timestamp)."
            ))),
        }
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
pub(crate) type AtomBox<I> = runtime::Chain<I, Atom>;

/// An atom-emitting source erased to one of the two input domains it can be
/// rooted in on the Python side: `Atom` (the identity passthrough) or
/// `Snapshot<Symbol>` (a `Pick`). Feeds the optional `source=` argument every
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
    Snapshot(AtomBox<Snapshot<Symbol>>),
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
#[pyclass(name = "AtomSource", module = "fugazi", skip_from_py_object)]
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

    pub(crate) fn warm_up_bars(&self) -> usize {
        match &self.inner {
            AnyAtomSource::Atom(s) => Indicator::warm_up_bars(s),
            AnyAtomSource::Snapshot(s) => Indicator::warm_up_bars(s),
        }
    }

    pub(crate) fn unstable_bars(&self) -> usize {
        match &self.inner {
            AnyAtomSource::Atom(s) => Indicator::unstable_bars(s),
            AnyAtomSource::Snapshot(s) => Indicator::unstable_bars(s),
        }
    }

    pub(crate) fn stable_bars(&self) -> usize {
        self.warm_up_bars() + self.unstable_bars()
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
///
/// `+ - * /` build a new `Indicator`; `> < >= <=` build a `Signal`. Both accept
/// a number on either side.
///
/// **`==` is the one exception, and it is not elementwise.** `a == b` is
/// Python's ordinary identity comparison, so two separately-built chains over
/// the same source compare `False`. Overloading it would return a `Signal` —
/// truthy, unhashable — and silently break `in`, `dict` and `set` for every
/// indicator. Use `a.eq(b)` (and `a.ne(b)`) for the elementwise form; they take
/// an `epsilon=` too, which is the reason the named `gt`/`lt`/`ge`/`le` twins
/// exist alongside the operators.
#[pyclass(name = "Indicator", module = "fugazi")]
pub(crate) struct PyIndicator {
    pub(crate) src: AnySource,
    /// The chain's root, kept **concrete** when it is a plain leaf, purely so a
    /// wrapping constructor can absorb it. See [`PendingRoot`].
    pub(crate) root: Option<PendingRoot>,
}

impl PyIndicator {
    pub(crate) fn wrap(src: AnySource) -> Self {
        PyIndicator { src, root: None }
    }

    /// A leaf that a wrapper may fuse over. `src` must be the erased form of
    /// `root` and nothing else — that equivalence is what makes fusing invisible.
    pub(crate) fn rooted(src: AnySource, root: PendingRoot) -> Self {
        PyIndicator {
            src,
            root: Some(root),
        }
    }
}

#[pymethods]
impl PyIndicator {
    /// Feed the next sample; returns the current value, or `None` while warming
    /// up. Pass a `Candle` for a candle-rooted indicator, a `float` for an
    /// identity-rooted one.
    pub(crate) fn update(&mut self, sample: &Bound<'_, PyAny>) -> PyResult<Option<f64>> {
        match &mut self.src {
            // A bar-only chain still accepts an `Atom` here — the Python
            // surface takes a Candle/Atom either way — and reads the bar out of
            // it. An overlay-only atom has no bar, so it reads `None`.
            AnySource::Candle(s) => Ok(extract_atom(sample)?
                .candle
                .and_then(|c| Indicator::update(s, c))),
            AnySource::Atom(s) => Ok(Indicator::update(s, extract_atom(sample)?)),
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
        // NumPy present — the normal case — means the values can be produced
        // directly into its buffer, with no `Vec<Option<f64>>` in between. The
        // import is checked up front rather than inside the fill, because the
        // fallback has to feed the data a different way and there is no
        // rewinding a consumed frame.
        if py.import("numpy").is_ok() {
            let arr = self.src.feed_into_numpy(py, data)?;
            return wrap_floats(py, &kind, arr);
        }
        // No NumPy: collect, and hand back a plain list that keeps the warm-up
        // `None`s instead of flattening them to `NaN`.
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
    pub(crate) fn warm_up_bars(&self) -> usize {
        self.src.warm_up_bars()
    }

    /// Extra samples after warm-up before a recursive indicator (EMA, RSI, …)
    /// has effectively converged; `0` for windowed indicators.
    pub(crate) fn unstable_bars(&self) -> usize {
        self.src.unstable_bars()
    }

    /// `warm_up_bars() + unstable_bars()`: how much history to feed before
    /// trusting the output.
    pub(crate) fn stable_bars(&self) -> usize {
        self.src.warm_up_bars() + self.src.unstable_bars()
    }

    /// Reset all internal state to freshly-constructed.
    pub(crate) fn reset(&mut self) {
        self.src.reset()
    }

    // --- comparisons -> Signal -------------------------------------------------
    #[pyo3(signature = (other, epsilon = None))]
    pub(crate) fn gt(&self, other: &Bound<'_, PyAny>, epsilon: Option<f64>) -> PyResult<PySignal> {
        let rhs = coerce_operand(other)?;
        let eps = epsilon.map_or(DEFAULT_TOLERANCE, Tolerance::absolute);
        Ok(PySignal::wrap(sources_to_signal!(
            self.src.clone(),
            rhs,
            |l, r| Combine::<_, _, GtOp>::with_tolerance(l, r, eps)
        )?))
    }

    #[pyo3(signature = (other, epsilon = None))]
    pub(crate) fn lt(&self, other: &Bound<'_, PyAny>, epsilon: Option<f64>) -> PyResult<PySignal> {
        let rhs = coerce_operand(other)?;
        let eps = epsilon.map_or(DEFAULT_TOLERANCE, Tolerance::absolute);
        Ok(PySignal::wrap(sources_to_signal!(
            self.src.clone(),
            rhs,
            |l, r| Combine::<_, _, LtOp>::with_tolerance(l, r, eps)
        )?))
    }

    #[pyo3(signature = (other, epsilon = None))]
    pub(crate) fn ge(&self, other: &Bound<'_, PyAny>, epsilon: Option<f64>) -> PyResult<PySignal> {
        let rhs = coerce_operand(other)?;
        let eps = epsilon.map_or(DEFAULT_TOLERANCE, Tolerance::absolute);
        Ok(PySignal::wrap(sources_to_signal!(
            self.src.clone(),
            rhs,
            |l, r| Combine::<_, _, GeOp>::with_tolerance(l, r, eps)
        )?))
    }

    #[pyo3(signature = (other, epsilon = None))]
    pub(crate) fn le(&self, other: &Bound<'_, PyAny>, epsilon: Option<f64>) -> PyResult<PySignal> {
        let rhs = coerce_operand(other)?;
        let eps = epsilon.map_or(DEFAULT_TOLERANCE, Tolerance::absolute);
        Ok(PySignal::wrap(sources_to_signal!(
            self.src.clone(),
            rhs,
            |l, r| Combine::<_, _, LeOp>::with_tolerance(l, r, eps)
        )?))
    }

    #[pyo3(signature = (other, epsilon = None))]
    pub(crate) fn eq(&self, other: &Bound<'_, PyAny>, epsilon: Option<f64>) -> PyResult<PySignal> {
        let rhs = coerce_operand(other)?;
        let eps = epsilon.map_or(DEFAULT_TOLERANCE, Tolerance::absolute);
        Ok(PySignal::wrap(sources_to_signal!(
            self.src.clone(),
            rhs,
            |l, r| Combine::<_, _, EqOp>::with_tolerance(l, r, eps)
        )?))
    }

    #[pyo3(signature = (other, epsilon = None))]
    pub(crate) fn ne(&self, other: &Bound<'_, PyAny>, epsilon: Option<f64>) -> PyResult<PySignal> {
        let rhs = coerce_operand(other)?;
        let eps = epsilon.map_or(DEFAULT_TOLERANCE, Tolerance::absolute);
        Ok(PySignal::wrap(sources_to_signal!(
            self.src.clone(),
            rhs,
            |l, r| Combine::<_, _, NeOp>::with_tolerance(l, r, eps)
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

    /// Pointwise `self ** other` (`None` where the result is not a finite real —
    /// a negative base at a fractional exponent, `0` to a negative power, or an
    /// overflow).
    pub(crate) fn pow(&self, other: &Bound<'_, PyAny>) -> PyResult<PyIndicator> {
        let rhs = coerce_operand(other)?;
        Ok(PyIndicator::wrap(combine_sources!(
            self.src.clone(),
            rhs,
            |l, r| l.pow(r)
        )?))
    }
    /// The larger of `self` and `other`, **bar by bar**.
    ///
    /// Not `rolling_max`, which maximises this one source over a window; this
    /// compares two sources against each other on the same bar.
    pub(crate) fn max(&self, other: &Bound<'_, PyAny>) -> PyResult<PyIndicator> {
        let rhs = coerce_operand(other)?;
        Ok(PyIndicator::wrap(combine_sources!(
            self.src.clone(),
            rhs,
            |l, r| l.max(r)
        )?))
    }
    /// The smaller of `self` and `other`, bar by bar — the twin of
    /// [`max`](Self::max).
    pub(crate) fn min(&self, other: &Bound<'_, PyAny>) -> PyResult<PyIndicator> {
        let rhs = coerce_operand(other)?;
        Ok(PyIndicator::wrap(combine_sources!(
            self.src.clone(),
            rhs,
            |l, r| l.min(r)
        )?))
    }
    /// `self` held inside `[lower, upper]`.
    ///
    /// Both bounds may be indicators or scalars. Inverted bounds (`lower` above
    /// `upper`) collapse to `upper` — what the `min`-of-`max` form this stands
    /// for does, and the honest answer to a contradictory band.
    pub(crate) fn clamp(
        &self,
        lower: &Bound<'_, PyAny>,
        upper: &Bound<'_, PyAny>,
    ) -> PyResult<PyIndicator> {
        self.max(lower)?.min(upper)
    }
    /// Absolute value of `self`, pointwise.
    pub(crate) fn abs(&self) -> PyIndicator {
        PyIndicator::wrap(map_rooted!(self, |s| s.abs()))
    }
    /// Sign of `self`: `1` above zero, `-1` below, `0` at exactly zero.
    pub(crate) fn sign(&self) -> PyIndicator {
        PyIndicator::wrap(map_rooted!(self, |s| s.sign()))
    }
    /// Square root of `self` (`None` on negative samples).
    pub(crate) fn sqrt(&self) -> PyIndicator {
        PyIndicator::wrap(map_rooted!(self, |s| s.sqrt()))
    }
    /// Hyperbolic tangent of `self`, squashing the real line into `(-1, 1)`.
    pub(crate) fn tanh(&self) -> PyIndicator {
        PyIndicator::wrap(map_rooted!(self, |s| s.tanh()))
    }
    /// Logistic sigmoid of `self`, `1 / (1 + e**-x)`, squashing into `(0, 1)`.
    pub(crate) fn sigmoid(&self) -> PyIndicator {
        PyIndicator::wrap(map_rooted!(self, |s| s.sigmoid()))
    }
    /// Running total of every value `self` has produced, from the first sample
    /// onward. Unbounded — no window.
    pub(crate) fn cum_sum(&self) -> PyIndicator {
        PyIndicator::wrap(map_rooted!(self, |s| s.cum_sum()))
    }
    /// Running maximum since the first sample — the unbounded
    /// [`rolling_max`](Self::rolling_max). `x / x.cum_max() - 1` is the
    /// drawdown of any series.
    pub(crate) fn cum_max(&self) -> PyIndicator {
        PyIndicator::wrap(map_rooted!(self, |s| s.cum_max()))
    }
    /// Running minimum since the first sample.
    pub(crate) fn cum_min(&self) -> PyIndicator {
        PyIndicator::wrap(map_rooted!(self, |s| s.cum_min()))
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
    pub(crate) fn __pow__(
        &self,
        other: &Bound<'_, PyAny>,
        modulo: Option<&Bound<'_, PyAny>>,
    ) -> PyResult<PyIndicator> {
        // Python's three-argument `pow` is integer modular exponentiation.
        // There is no elementwise reading of it over a float series, so it is
        // refused rather than silently ignored.
        if modulo.is_some_and(|m| !m.is_none()) {
            return Err(PyValueError::new_err(
                "three-argument pow() is not supported on an Indicator",
            ));
        }
        self.pow(other)
    }
    /// `abs(ind)` — the operator form of [`abs`](Self::abs).
    pub(crate) fn __abs__(&self) -> PyIndicator {
        self.abs()
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
    pub(crate) fn __rpow__(
        &self,
        other: &Bound<'_, PyAny>,
        modulo: Option<&Bound<'_, PyAny>>,
    ) -> PyResult<PyIndicator> {
        if modulo.is_some_and(|m| !m.is_none()) {
            return Err(PyValueError::new_err(
                "three-argument pow() is not supported on an Indicator",
            ));
        }
        let lhs = coerce_operand(other)?;
        Ok(PyIndicator::wrap(combine_sources!(
            lhs,
            self.src.clone(),
            |l, r| l.pow(r)
        )?))
    }

    // --- comparison dunders -> Signal -----------------------------------------
    //
    // `ind > other` is [`gt`](Self::gt) at the default tolerance — the spelling
    // to reach for, and the one that matches `+`/`-`/`*`/`/` right above. The
    // named methods stay because they are the only way to pass an `epsilon=`.
    //
    // Python reflects an ordering comparison on its own (`2.0 < ind` retries as
    // `ind.__gt__(2.0)` once `float.__lt__` declines), so the four here also
    // cover a scalar on the left with no `__r*__` twins.
    //
    // `__eq__`/`__ne__` are deliberately **absent**. Returning a `Signal` from
    // them would make an `Indicator` unhashable and silently break `in`, `dict`
    // and `set` — so `==` stays Python's identity comparison and the elementwise
    // form is [`eq`](Self::eq) / [`ne`](Self::ne) only. That asymmetry is the
    // price of staying a well-behaved Python object; it is called out in the
    // class docstring.
    /// `self > other`, elementwise, at the default tolerance — the operator form
    /// of [`gt`](Self::gt).
    pub(crate) fn __gt__(&self, other: &Bound<'_, PyAny>) -> PyResult<PySignal> {
        self.gt(other, None)
    }
    /// `self < other`, elementwise, at the default tolerance — the operator form
    /// of [`lt`](Self::lt).
    pub(crate) fn __lt__(&self, other: &Bound<'_, PyAny>) -> PyResult<PySignal> {
        self.lt(other, None)
    }
    /// `self >= other`, elementwise, at the default tolerance — the operator
    /// form of [`ge`](Self::ge).
    pub(crate) fn __ge__(&self, other: &Bound<'_, PyAny>) -> PyResult<PySignal> {
        self.ge(other, None)
    }
    /// `self <= other`, elementwise, at the default tolerance — the operator
    /// form of [`le`](Self::le).
    pub(crate) fn __le__(&self, other: &Bound<'_, PyAny>) -> PyResult<PySignal> {
        self.le(other, None)
    }

    /// Identity hash — the default an object without `__eq__` would have had.
    ///
    /// **Not redundant.** Defining the four orderings above fills `tp_richcompare`,
    /// and CPython nulls `tp_hash` for any type that has the one and not the
    /// other — so without this line adding `>` would silently make every
    /// `Indicator` unhashable and break `set`/`dict` use that worked before.
    /// `__eq__` is still Python's identity comparison, so hashing by address
    /// keeps the hash/eq contract exactly as it was.
    pub(crate) fn __hash__(&self) -> u64 {
        std::ptr::from_ref(self) as u64
    }

    // --- lookback / rolling -> Indicator --------------------------------------
    /// `self` delayed by `period` steps (one, the previous bar, by default).
    #[pyo3(signature = (period = 1))]
    pub(crate) fn lag(&self, period: usize) -> PyIndicator {
        PyIndicator::wrap(map_rooted!(self, |s| s.lag(period)))
    }
    /// Discrete difference over `period` steps (`x[t] - x[t-n]`); `period`
    /// defaults to one, the first difference.
    #[pyo3(signature = (period = 1))]
    pub(crate) fn diff(&self, period: usize) -> PyIndicator {
        PyIndicator::wrap(map_rooted!(self, |s| s.diff(period)))
    }
    /// Ratio to the value `period` steps ago (`x[t] / x[t-n]`); `period`
    /// defaults to one bar.
    #[pyo3(signature = (period = 1))]
    pub(crate) fn ratio(&self, period: usize) -> PyIndicator {
        PyIndicator::wrap(map_rooted!(self, |s| s.ratio(period)))
    }
    /// Percentage rate of change over `period` steps; `period` defaults to
    /// one, so `roc()` is the per-bar return.
    #[pyo3(signature = (period = 1))]
    pub(crate) fn roc(&self, period: usize) -> PyIndicator {
        PyIndicator::wrap(map_rooted!(self, |s| s.roc(period)))
    }
    /// Rolling maximum over `period` steps.
    pub(crate) fn rolling_max(&self, period: usize) -> PyResult<PyIndicator> {
        ensure_period(period)?;
        Ok(PyIndicator::wrap(
            map_rooted!(self, |s| s.rolling_max(period))
        ))
    }
    /// Rolling minimum over `period` steps.
    pub(crate) fn rolling_min(&self, period: usize) -> PyResult<PyIndicator> {
        ensure_period(period)?;
        Ok(PyIndicator::wrap(
            map_rooted!(self, |s| s.rolling_min(period))
        ))
    }

    /// Logarithm of `self` in `base` (default: natural log, `e`). Emits `None`
    /// on samples where the input is non-positive.
    #[pyo3(signature = (base = std::f64::consts::E))]
    pub(crate) fn log(&self, base: f64) -> PyResult<PyIndicator> {
        ensure_log_base(base)?;
        Ok(PyIndicator::wrap(map_rooted!(self, |s| Log::new(s, base))))
    }

    /// Exponential of `self` in `base` — `base^x`, the inverse of `log`
    /// (default: the natural exponential, `e`). Emits `None` on samples whose
    /// result overflows the finite range.
    #[pyo3(signature = (base = std::f64::consts::E))]
    pub(crate) fn exp(&self, base: f64) -> PyResult<PyIndicator> {
        ensure_exp_base(base)?;
        Ok(PyIndicator::wrap(map_rooted!(self, |s| Exp::new(s, base))))
    }

    /// Passthrough that forces this indicator's reported `unstable_bars()` to
    /// `0`. Output and `warm_up_bars()` are unchanged; a downstream reader of
    /// `stable_bars()` (a strategy readiness gate, an overlay trim) no longer
    /// waits for this subtree's IIR settling tail. Use to explicitly opt out of
    /// the safe default that waits for it.
    pub(crate) fn unstable(&self) -> PyIndicator {
        PyIndicator::wrap(map_rooted!(self, |s| s.unstable()))
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
#[pyclass(name = "Signal", module = "fugazi")]
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
            AnySignal::Candle(s) => Ok(extract_atom(sample)?
                .candle
                .and_then(|c| Indicator::update(s, c))
                .unwrap_or(false)),
            AnySignal::Atom(s) => Ok(Indicator::update(s, extract_atom(sample)?).unwrap_or(false)),
            AnySignal::Real(s) => Ok(Indicator::update(s, extract_real(sample)?).unwrap_or(false)),
            AnySignal::Snapshot(s) => {
                Ok(Indicator::update(s, extract_snapshot(sample)?).unwrap_or(false))
            }
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
    pub(crate) fn warm_up_bars(&self) -> usize {
        self.sig.warm_up_bars()
    }

    /// Extra samples after warm-up before any recursive sources inside the
    /// signal have effectively converged; `0` for windowed ones.
    pub(crate) fn unstable_bars(&self) -> usize {
        self.sig.unstable_bars()
    }

    /// `warm_up_bars() + unstable_bars()`: how much history to feed before
    /// trusting the signal.
    pub(crate) fn stable_bars(&self) -> usize {
        self.sig.warm_up_bars() + self.sig.unstable_bars()
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

    /// Passthrough that forces this signal's reported `unstable_bars()` to
    /// `0`. Output and `warm_up_bars()` are unchanged; a downstream reader of
    /// `stable_bars()` (a strategy readiness gate) no longer waits for this
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
#[pyclass(name = "StrSource", module = "fugazi")]
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
            AnyStrSource::Atom(s) => Indicator::update(s, extract_atom(sample)?),
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

    pub(crate) fn warm_up_bars(&self) -> usize {
        self.src.warm_up_bars()
    }

    pub(crate) fn unstable_bars(&self) -> usize {
        self.src.unstable_bars()
    }

    pub(crate) fn stable_bars(&self) -> usize {
        self.src.warm_up_bars() + self.src.unstable_bars()
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
            StrPair::Atom(l, r) => {
                PySignal::wrap(AnySignal::Atom(SignalBox::new(
                    Combine::<_, _, StrEqOp>::new(l, r),
                )))
            }
            StrPair::Snapshot(l, r) => PySignal::wrap(AnySignal::Snapshot(SignalBox::new(
                Combine::<_, _, StrEqOp>::new(l, r),
            ))),
        })
    }

    /// `self != other` — the string counterpart to [`eq`](Self::eq).
    pub(crate) fn ne(&self, other: &Bound<'_, PyAny>) -> PyResult<PySignal> {
        let rhs = coerce_str_operand(other)?;
        Ok(match str_pair(self.src.clone(), rhs)? {
            StrPair::Atom(l, r) => {
                PySignal::wrap(AnySignal::Atom(SignalBox::new(
                    Combine::<_, _, StrNeOp>::new(l, r),
                )))
            }
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
#[pyclass(name = "MultiIndicator", module = "fugazi")]
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
        // One `Vec` per call, which is a per-sample API and not a bulk path.
        let mut values: Vec<Real> = Vec::new();
        let ok = match &mut self.inner {
            // The Python surface takes a Candle or an Atom either way; a bar
            // chain reads the bar out of whichever arrived.
            AnyMulti::Candle(m) => match extract_atom(sample)?.candle {
                Some(c) => m.0.update_into(c, &mut values),
                None => false,
            },
            AnyMulti::Atom(m) => m.0.update_into(extract_atom(sample)?, &mut values),
            AnyMulti::Real(m) => m.0.update_into(extract_real(sample)?, &mut values),
            AnyMulti::Snapshot(m) => m.0.update_into(extract_snapshot(sample)?, &mut values),
        };
        match ok.then_some(values) {
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
    ///
    /// **The NumPy columns are views over one shared buffer.** Every line of a
    /// call comes out of a single `(lines, n)` allocation, because allocating
    /// them separately costs 10.70 ns/sample against 1.61 — NumPy's allocator
    /// caches one buffer of a stable size and thrashes on several. The
    /// consequence is that they share ownership: keeping one column keeps the
    /// whole buffer alive, so holding just `adx` out of a three-line result
    /// retains 3x what its own `nbytes` reports, and `sys.getsizeof` will say
    /// 112 bytes while it does. `arr.base.nbytes` is what tells the truth.
    ///
    /// It is bounded — at most the line count — and everything frees when the
    /// last column is dropped. If you are keeping one line out of many across a
    /// large universe, `.copy()` it and the rest is released; that copy costs
    /// about a tenth of what the sharing saves. The pandas path is unaffected:
    /// pandas consolidates into its own blocks.
    pub(crate) fn feed(&mut self, py: Python<'_>, data: &Bound<'_, PyAny>) -> PyResult<Py<PyAny>> {
        let kind = OutputKind::detect(data)?;
        let names = self.inner.names();
        // NumPy present — the normal case — means each output line can be folded
        // straight into its own array: no `Vec` per bar, and no transpose.
        if py.import("numpy").is_ok() {
            let arrays = self.inner.feed_into_columns(py, data)?;
            return wrap_multi(py, &kind, names, arrays);
        }
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
    pub(crate) fn warm_up_bars(&self) -> usize {
        self.inner.warm_up_bars()
    }

    /// Extra samples after warm-up before a recursive indicator (MACD, ADX, …)
    /// has effectively converged; `0` for windowed indicators.
    pub(crate) fn unstable_bars(&self) -> usize {
        self.inner.unstable_bars()
    }

    /// `warm_up_bars() + unstable_bars()`: how much history to feed before
    /// trusting the output.
    pub(crate) fn stable_bars(&self) -> usize {
        self.inner.warm_up_bars() + self.inner.unstable_bars()
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
                last_output: Vec::new(),
                last_valid: false,
            }))),
            AnyMulti::Atom(m) => AnySharedMulti::Atom(Arc::new(Mutex::new(SharedMultiCell {
                names: m.0.names(),
                multi: m.0.clone_box(),
                generation: 0,
                last_output: Vec::new(),
                last_valid: false,
            }))),
            AnyMulti::Real(m) => AnySharedMulti::Real(Arc::new(Mutex::new(SharedMultiCell {
                names: m.0.names(),
                multi: m.0.clone_box(),
                generation: 0,
                last_output: Vec::new(),
                last_valid: false,
            }))),
            AnyMulti::Snapshot(m) => {
                AnySharedMulti::Snapshot(Arc::new(Mutex::new(SharedMultiCell {
                    names: m.0.names(),
                    multi: m.0.clone_box(),
                    generation: 0,
                    last_output: Vec::new(),
                    last_valid: false,
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
#[pyclass(name = "SharedMultiIndicator", module = "fugazi")]
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

    /// Linear-regression slope, in source units per bar.
    pub(crate) fn slope(&self) -> PyResult<PyIndicator> {
        self.inner.project("slope")
    }
    /// Linear-regression fit at the oldest bar of the window.
    pub(crate) fn intercept(&self) -> PyResult<PyIndicator> {
        self.inner.project("intercept")
    }
    /// Linear-regression fit at the newest bar of the window.
    pub(crate) fn value(&self) -> PyResult<PyIndicator> {
        self.inner.project("value")
    }
    /// Linear-regression coefficient of determination, in `[0, 1]`.
    pub(crate) fn r2(&self) -> PyResult<PyIndicator> {
        self.inner.project("r2")
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

    /// `handle["signal"]` — [`component`](Self::component) by subscript.
    ///
    /// Worth having because this class is the one place the Rust type system
    /// does *not* survive the boundary: Rust has a distinct type per multi, each
    /// exposing only its own accessors, while Python has one class carrying the
    /// union of all of them. So `bollinger(...).shared().adx()` looks fine and
    /// fails at call time, and `names()` is the only honest source of truth.
    /// Subscripting reads as the lookup it is, rather than as a method that
    /// might have been checked.
    pub(crate) fn __getitem__(&self, name: &str) -> PyResult<PyIndicator> {
        self.inner.project(name)
    }

    /// The number of output lines — `len(handle)` == `len(handle.names())`.
    pub(crate) fn __len__(&self) -> usize {
        self.inner.names().len()
    }

    /// Iterate the field names, so `for line in handle` and `list(handle)`
    /// match [`names`](Self::names).
    pub(crate) fn __iter__(&self, py: Python<'_>) -> PyResult<Py<PyAny>> {
        iter_over(py, self.names())
    }

    /// `"signal" in handle` — whether this particular multi has that line.
    pub(crate) fn __contains__(&self, name: &str) -> bool {
        self.inner.names().contains(&name)
    }

    pub(crate) fn __repr__(&self) -> String {
        let names = self.inner.names();
        format!("SharedMultiIndicator(fields={names:?})")
    }
}

// ---------------------------------------------------------------------------
// Unpickling entry points
//
// `__reduce__` names a callable, and pickle stores that callable by
// `module.qualname` — so anything a `__reduce__` points at has to be a real,
// importable module member. These are that, for the two types whose public
// constructor cannot express their full state: `Schema` has no `__new__` at all
// (a `SchemaBuilder` produces it), and `Snapshot(mapping)` would collapse two
// entries sharing one tag.
//
// Underscore-prefixed because they are a serialization detail, not surface —
// but public members all the same, since pickle has to be able to find them.
// ---------------------------------------------------------------------------

/// Rebuild a [`Snapshot`](PySnapshot) from `(selector, atom)` pairs, preserving
/// duplicates and insertion order.
#[pyfunction]
pub(crate) fn _rebuild_snapshot(items: &Bound<'_, PyAny>) -> PyResult<PySnapshot> {
    let mut snap = PySnapshot::new(None)?;
    for item in items.try_iter()? {
        let pair = item?;
        snap.push(&pair.get_item(0)?, pair.get_item(1)?.extract()?)?;
    }
    Ok(snap)
}

/// Rebuild a [`Schema`](PySchema) by replaying its columns through a
/// `SchemaBuilder`. `types` are the strings [`PySchema::type_of`] returns.
#[pyfunction]
pub(crate) fn _rebuild_schema(keys: Vec<String>, types: Vec<String>) -> PyResult<PySchema> {
    if keys.len() != types.len() {
        return Err(PyValueError::new_err(
            "_rebuild_schema: keys and types must be the same length",
        ));
    }
    let mut builder = PySchemaBuilder::new();
    for (key, ty) in keys.iter().zip(&types) {
        match ty.as_str() {
            "real" => builder.add_real(key)?,
            "bool" => builder.add_bool(key)?,
            "str" => builder.add_str(key)?,
            other => {
                return Err(PyValueError::new_err(format!(
                    "_rebuild_schema: unknown column type {other:?} (expected real/bool/str)"
                )));
            }
        };
    }
    builder.finish()
}

// ---------------------------------------------------------------------------
// Interruptible drives
// ---------------------------------------------------------------------------

/// How many samples pass between signal polls.
///
/// The run is *detached*, so each poll is a GIL reacquire plus a `check_signals`,
/// and a backtest's inner loop is 1-13 ns/bar — polling every bar would dominate
/// it. At 4096 an 800 k-bar run pays ~195 acquisitions total, below the
/// run-to-run variance of the drive itself, while the worst-case latency between
/// Ctrl-C and the run stopping stays well under a millisecond.
///
/// It is also the granularity at which other Python threads get a turn, which is
/// the *other* reason not to raise it much.
const SIGNAL_CHECK_STRIDE: usize = 4096;

/// Wrap a snapshot/sample iterator so a pending `KeyboardInterrupt` ends the
/// drive, **without** holding the GIL between checks.
///
/// `backtest::drive` takes an `IntoIterator`, which is the only seam the Python
/// side needs: this yields the same items and, every [`SIGNAL_CHECK_STRIDE`] of
/// them, re-attaches just long enough to run Python's signal handlers. On a
/// pending signal it parks the error and reports exhaustion, so the drive
/// returns normally and the caller re-raises — `Iterator::next` has nowhere to
/// put an error, and this keeps the core loop free of any Python awareness.
///
/// # Why a `Mutex` and not a `Cell`
///
/// The whole iterator is handed to a [`Python::detach`] closure, which demands
/// `Send`. A `&Cell<_>` is not `Send` (`Cell` is not `Sync`), so the obvious
/// single-threaded parking slot is exactly the thing that will not compile here.
/// `PyErr` is `Send + Sync`, so a `Mutex` is; it is uncontended by construction —
/// one writer, read once after the run — and touched at most once per stride.
pub(crate) fn interruptible<'a, T: 'a>(
    items: impl IntoIterator<Item = T> + 'a,
    interrupt: &'a std::sync::Mutex<Option<PyErr>>,
) -> impl Iterator<Item = T> + 'a {
    let mut seen = 0usize;
    items.into_iter().map_while(move |item| {
        if seen.is_multiple_of(SIGNAL_CHECK_STRIDE)
            && let Err(e) = Python::attach(|py| py.check_signals())
        {
            *interrupt.lock().expect("interrupt slot poisoned") = Some(e);
            return None;
        }
        seen += 1;
        Some(item)
    })
}

/// Re-raise whatever [`interruptible`] parked, if anything.
///
/// Call immediately after a drive that used it: a stopped run's partial report
/// is meaningless, so the error wins over the value.
pub(crate) fn raise_if_interrupted<T>(
    interrupt: &std::sync::Mutex<Option<PyErr>>,
    value: T,
) -> PyResult<T> {
    match interrupt.lock().expect("interrupt slot poisoned").take() {
        Some(e) => Err(e),
        None => Ok(value),
    }
}

/// Signal polling for a **parallel** detached region — the grid sweep and the
/// walk-forward pass.
///
/// [`interruptible`] cannot serve here: those run rows across a rayon pool, so
/// there is no single iterator to wrap.
///
/// # Why the work moves off the main thread
///
/// The obvious design — poll from inside the per-row closure — does not work,
/// and fails *silently*:
///
/// * CPython runs signal handlers on the **main thread only**; `check_signals`
///   returns 0 immediately anywhere else. A worker could poll every row and
///   never see a `KeyboardInterrupt`.
/// * The main thread is not available to poll either, because
///   `rayon::ThreadPool::install` **blocks** the caller on a custom pool rather
///   than letting it steal work. (`par_iter` on the *global* pool does let the
///   caller participate; the sweep does not use the global pool.) So the main
///   thread evaluates no rows and reaches no poll.
///
/// Measured: with per-row polling and no inversion, a 600-row sweep took the
/// interrupt at 3.00 s against a 2.93 s uninterrupted run — i.e. not at all.
///
/// So [`run_watched`] inverts it. The sweep runs on a scoped thread and the main
/// thread becomes a watchdog, polling Python every [`WATCHDOG_INTERVAL`]. Workers
/// only ever read an `AtomicBool` — never the GIL, which would serialise
/// `jobs=N` back down to one.
pub(crate) struct SweepInterrupt {
    stop: std::sync::atomic::AtomicBool,
    parked: std::sync::Mutex<Option<PyErr>>,
}

/// How often the watchdog asks Python whether a signal is pending.
///
/// Small enough that Ctrl-C feels immediate, long enough that the GIL
/// acquisition is irrelevant beside the rows running in parallel underneath.
const WATCHDOG_INTERVAL: std::time::Duration = std::time::Duration::from_millis(20);

impl SweepInterrupt {
    pub(crate) fn new() -> Self {
        Self {
            stop: std::sync::atomic::AtomicBool::new(false),
            parked: std::sync::Mutex::new(None),
        }
    }

    /// True once an interrupt is pending — call at the top of each row.
    ///
    /// A relaxed atomic load and nothing else: this runs on rayon workers, and
    /// anything touching the GIL here would undo the parallelism.
    pub(crate) fn should_stop(&self) -> bool {
        self.stop.load(std::sync::atomic::Ordering::Relaxed)
    }

    /// Ask Python whether a signal is pending. **Main thread only** — see the
    /// type docs for why that is not a suggestion.
    fn poll(&self) {
        if let Err(e) = Python::attach(|py| py.check_signals()) {
            *self.parked.lock().expect("interrupt slot poisoned") = Some(e);
            self.stop.store(true, std::sync::atomic::Ordering::Relaxed);
        }
    }

    /// Re-raise the parked interrupt, if any, in preference to `value`.
    ///
    /// The rows abort the sweep with an ordinary error once the flag is set, so
    /// without this the caller would see "sweep failed" rather than the
    /// `KeyboardInterrupt` they asked for.
    pub(crate) fn raise_over<T>(&self, value: PyResult<T>) -> PyResult<T> {
        match self.parked.lock().expect("interrupt slot poisoned").take() {
            Some(e) => Err(e),
            None => value,
        }
    }
}

/// Run `work` with the GIL released, on a thread that is *not* this one, while
/// this thread watches for `KeyboardInterrupt`.
///
/// The inversion is the point: see [`SweepInterrupt`]. `work` gets a borrowed
/// environment (`std::thread::scope`, not `spawn`), so nothing has to become
/// `'static` to be swept.
pub(crate) fn run_watched<T: Send>(
    py: Python<'_>,
    interrupt: &SweepInterrupt,
    work: impl FnOnce() -> T + Send,
) -> T {
    py.detach(|| {
        std::thread::scope(|scope| {
            let handle = scope.spawn(work);
            while !handle.is_finished() {
                interrupt.poll();
                std::thread::sleep(WATCHDOG_INTERVAL);
            }
            // Propagate a panic from the sweep rather than swallowing it into a
            // join error — the caller's `catch_unwind` boundary is pyo3's.
            match handle.join() {
                Ok(value) => value,
                Err(payload) => std::panic::resume_unwind(payload),
            }
        })
    })
}
