use crate::prelude::*;
// The binding modules were one flat namespace before the split and still read
// as one: each pulls in its siblings, so a cross-module reference needs no path.
#[allow(unused_imports)]
use crate::carriers::*;
#[allow(unused_imports)]
use crate::classes::*;
#[allow(unused_imports)]
use crate::strategy::*;
#[allow(unused_imports)]
use crate::sources::*;
#[allow(unused_imports)]
use crate::metrics::*;
#[allow(unused_imports)]
use crate::spec::*;

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

pub(crate) fn ensure_period(period: usize) -> PyResult<()> {
    if period == 0 {
        Err(PyValueError::new_err("period must be greater than 0"))
    } else {
        Ok(())
    }
}

/// Extract a `Candle` for a candle-rooted node's `update`.
pub(crate) fn extract_candle(sample: &Bound<'_, PyAny>) -> PyResult<Candle> {
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
pub(crate) fn extract_atom(sample: &Bound<'_, PyAny>) -> PyResult<Atom> {
    if let Ok(atom) = sample.cast::<PyAtom>() {
        return Ok(atom.borrow().inner.clone());
    }
    let candle = extract_candle(sample)?;
    Ok(candle.into())
}

/// Extract a `float` for an identity-rooted node's `update`.
pub(crate) fn extract_real(sample: &Bound<'_, PyAny>) -> PyResult<Real> {
    sample
        .extract::<f64>()
        .map_err(|_| PyTypeError::new_err("this indicator consumes a value stream; pass a float"))
}

/// A per-conversion symbol cache.
///
/// A Python caller hands the same handful of symbol strings once per bar, so a
/// 20 000-bar × 32-symbol series presents 640 000 `str` keys drawn from 32
/// distinct values. Interning each one independently would allocate 640 000
/// `Arc`s; this allocates 32 and hands out refcount bumps.
///
/// Scoped to one conversion rather than global on purpose: a process-wide
/// intern table would need locking and would never release symbols from a
/// finished run.
#[derive(Default)]
pub(crate) struct SymbolInterner {
    seen: std::collections::HashMap<String, Symbol>,
}

impl SymbolInterner {
    fn get(&mut self, name: &str) -> Symbol {
        if let Some(s) = self.seen.get(name) {
            return s.clone();
        }
        let s = intern(name);
        self.seen.insert(name.to_owned(), s.clone());
        s
    }
}

/// Iterate a Python sequence of snapshots (`list[Snapshot]` or `list[dict]`)
/// into a native `Vec<Snapshot<Symbol>>` for a snapshot-rooted node's `feed`.
///
/// Symbols are interned across the whole sequence — see [`SymbolInterner`].
pub(crate) fn snapshots_from_sequence(obj: &Bound<'_, PyAny>) -> PyResult<Vec<Snapshot<Symbol>>> {
    let mut out = Vec::new();
    let mut interner = SymbolInterner::default();
    let iter = obj.try_iter().map_err(|_| {
        PyTypeError::new_err(
            "snapshot-rooted feed(): expected an iterable of Snapshot (or dict) values",
        )
    })?;
    for item in iter {
        out.push(extract_snapshot_interned(&item?, &mut interner)?);
    }
    Ok(out)
}

/// Extract a `Snapshot<Symbol>` for a snapshot-rooted node's `update`.
/// Accepts a `PySnapshot` directly, or a Python `dict` whose keys are coerced
/// via [`coerce_selector`] (str → symbol, Frequency → freq, Selector as-is,
/// (str, freq) tuple → both fields).
pub(crate) fn extract_snapshot(sample: &Bound<'_, PyAny>) -> PyResult<Snapshot<Symbol>> {
    // Deliberately *not* via `SymbolInterner`: for a single snapshot the cache
    // can never hit, so building one would trade a per-symbol string allocation
    // for a whole hash table — measurably worse (it cost +86% on a 32-symbol
    // conversion before this was split out). The interner only pays across a
    // sequence, which is what `snapshots_from_sequence` uses it for.
    extract_snapshot_with(sample, &mut |s: &str| intern(s))
}

/// The body shared by [`extract_snapshot`] and [`extract_snapshot_interned`],
/// parameterised by how a `str` key becomes a [`Symbol`].
fn extract_snapshot_with(
    sample: &Bound<'_, PyAny>,
    to_symbol: &mut dyn FnMut(&str) -> Symbol,
) -> PyResult<Snapshot<Symbol>> {
    if let Ok(snap) = sample.cast::<PySnapshot>() {
        // Already native: cloning is a refcount bump on the entries `Arc`.
        return Ok(snap.borrow().inner.clone());
    }
    if let Ok(dict) = sample.cast::<pyo3::types::PyDict>() {
        let mut out = Snapshot::<Symbol>::new();
        for (k, v) in dict.iter() {
            let atom = extract_atom(&v)?;
            // Fast path: a bare `str` key, which is what a dict-shaped snapshot
            // almost always uses.
            if let Ok(s) = k.cast::<pyo3::types::PyString>()
                && let Ok(s) = s.to_cow()
            {
                out.push(Some(to_symbol(s.as_ref())), None, atom);
                continue;
            }
            let key = coerce_selector(&k)?;
            out.push(key.symbol, key.freq, atom);
        }
        return Ok(out);
    }
    Err(PyTypeError::new_err(
        "this indicator consumes a Snapshot; pass a Snapshot or a dict[str, Atom|Candle]",
    ))
}

/// [`extract_snapshot`] reusing a caller-owned [`SymbolInterner`], so a whole
/// series shares one `Arc` per distinct symbol.
pub(crate) fn extract_snapshot_interned(
    sample: &Bound<'_, PyAny>,
    interner: &mut SymbolInterner,
) -> PyResult<Snapshot<Symbol>> {
    extract_snapshot_with(sample, &mut |s: &str| interner.get(s))
}

/// Collect any 1-D sequence of numbers (`list`, NumPy array, pandas `Series`,
/// …) into a `Vec<f64>`, attributing failures to the named column.
pub(crate) fn column_to_vec(obj: &Bound<'_, PyAny>, name: &str) -> PyResult<Vec<f64>> {
    let err = || {
        PyTypeError::new_err(format!(
            "'{name}' must be a 1-D sequence of numbers (list, NumPy array, or pandas Series)"
        ))
    };

    // Fast path: anything exposing a contiguous 1-D `f64` buffer — a NumPy
    // `float64` array, an `array.array('d')`, a `memoryview` — is copied in one
    // `memcpy`.
    //
    // The general path below walks the object with the Python iterator
    // protocol, materialising one Python `float` per element. For a
    // 200 000-sample array that is 200 000 object round-trips at ~155 ns each,
    // against ~1-13 ns of actual indicator work: without this, `feed()` spends
    // over 90% of its time at the boundary. Needing `PyBuffer` is why the wheel
    // is `abi3-py311` (see `python/Cargo.toml`).
    if let Ok(buf) = pyo3::buffer::PyBuffer::<f64>::get(obj)
        && buf.dimensions() == 1
        && buf.is_c_contiguous()
    {
        return buf.to_vec(obj.py());
    }
    // pandas / polars `Series` are not buffers themselves but hand one over.
    for attr in ["to_numpy", "__array__"] {
        if let Ok(f) = obj.getattr(attr)
            && let Ok(arr) = f.call0()
            && let Ok(buf) = pyo3::buffer::PyBuffer::<f64>::get(&arr)
            && buf.dimensions() == 1
            && buf.is_c_contiguous()
        {
            return buf.to_vec(arr.py());
        }
    }
    // General path: any iterable of numbers.
    let mut values = Vec::new();
    for item in obj.try_iter().map_err(|_| err())? {
        values.push(item?.extract::<f64>().map_err(|_| err())?);
    }
    Ok(values)
}

/// Zip OHLCV columns into candles. `close` is the anchor: omitted `open`/`high`/
/// `low` default to it and omitted `volume` to `0`.
pub(crate) fn assemble_candles(
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

/// A frame's OHLCV columns, held **as columns**.
///
/// PERFORMANCE — the reason this exists instead of a `Vec<Candle>`.
///
/// `feed()` on a candle-rooted indicator used to go
/// columns -> `Vec<Candle>` -> iterate, which for 200 000 bars writes 8 MB of
/// candles and reads them straight back, purely to change the layout. Measured,
/// the frame input path cost **24.3 ns/sample** with a trivial indicator on top
/// of it, against 5.6 for the *entire* 1-D pipeline — so the conversion, not the
/// indicator, was the whole cost of a candle-rooted `feed()`.
///
/// [`Self::for_each`] streams candles instead: built in a register, handed
/// straight to `update`, never stored. It also zips slices rather than indexing,
/// so the five bounds checks per bar go away too.
///
/// Keep the columns owned (not borrowed from Python) — the buffer fast path in
/// [`column_to_vec`] already copied them, and holding Python buffers alive
/// across `update` calls would pin the GIL to the whole loop.
pub(crate) struct CandleColumns {
    close: Vec<f64>,
    open: Option<Vec<f64>>,
    high: Option<Vec<f64>>,
    low: Option<Vec<f64>>,
    volume: Option<Vec<f64>>,
}

impl CandleColumns {
    /// Number of bars — `close`'s length, which every other column is validated
    /// against in [`columns_from_frame`].
    pub(crate) fn len(&self) -> usize {
        self.close.len()
    }

    /// Feed every bar to `f`, without materialising a `Vec<Candle>`.
    ///
    /// An omitted `open`/`high`/`low` reads as `close` — and is *aliased* to the
    /// close slice rather than copied, so a close-only frame costs no extra
    /// memory. An omitted `volume` reads as `0.0`, which needs its own arm
    /// because there is no slice to alias.
    /// **Do not "optimise" the zips into an indexed loop.** That was tried, with
    /// every slice re-cut to `[..n]` first so the bounds checks would provably
    /// fold away, on the theory that four nested `Zip`s each carrying their own
    /// exhaustion check must cost something. It compiles to the same machine
    /// code: `close`, `sma` and `atr` all measured **identical to the hundredth
    /// of an instruction per sample** (38.57 / 79.58 / 81.23) before and after.
    /// LLVM already canonicalises both forms to one induction variable.
    ///
    /// The ~28 instructions/sample this loop does cost are the `Candle` itself —
    /// five loads, five stores, and a 40-byte by-value move through the chain's
    /// vtable — not the iteration.
    #[inline]
    pub(crate) fn for_each(&self, mut f: impl FnMut(Candle)) {
        let c = self.close.as_slice();
        let o = self.open.as_deref().unwrap_or(c);
        let h = self.high.as_deref().unwrap_or(c);
        let l = self.low.as_deref().unwrap_or(c);
        match self.volume.as_deref() {
            Some(v) => {
                for ((((&c, &o), &h), &l), &v) in
                    c.iter().zip(o).zip(h).zip(l).zip(v)
                {
                    f(Candle::new(o, h, l, c, v));
                }
            }
            None => {
                for (((&c, &o), &h), &l) in c.iter().zip(o).zip(h).zip(l) {
                    f(Candle::new(o, h, l, c, 0.0));
                }
            }
        }
    }
}

/// Read a frame's OHLCV columns without zipping them into candles — the
/// streaming counterpart of [`candles_from_frame`]. See [`CandleColumns`].
pub(crate) fn columns_from_frame(data: &Bound<'_, PyAny>) -> PyResult<CandleColumns> {
    if !(data.hasattr("columns")? || data.is_instance_of::<PyDict>()) {
        return Err(PyTypeError::new_err(
            "this indicator consumes candles: pass a DataFrame or dict with OHLCV columns. \
             To compute over a bare numeric series, root the indicator at identity().",
        ));
    }
    let cols = frame_columns(data)?;
    let n = cols.close.len();
    for (name, col) in [
        ("open", &cols.open),
        ("high", &cols.high),
        ("low", &cols.low),
        ("volume", &cols.volume),
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
    Ok(cols)
}

/// Build the candle series a candle-rooted `feed()` consumes from its `data`
/// argument: a pandas/polars `DataFrame` or a `dict` of OHLCV columns. A bare
/// numeric series is rejected — root the indicator at `identity()` for that.
pub(crate) fn candles_from_frame(data: &Bound<'_, PyAny>) -> PyResult<Vec<Candle>> {
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
pub(crate) fn reals_from_series(data: &Bound<'_, PyAny>) -> PyResult<Vec<Real>> {
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
pub(crate) fn frame_to_candles(frame: &Bound<'_, PyAny>) -> PyResult<Vec<Candle>> {
    let cols = frame_columns(frame)?;
    assemble_candles(cols.close, cols.open, cols.high, cols.low, cols.volume)
}

/// Pull the OHLCV columns out of a `DataFrame`/`dict`, matched
/// case-insensitively (`Close`/`CLOSE`/`close`). `close` is required; the rest
/// are optional. Shared by [`frame_to_candles`] and [`columns_from_frame`].
fn frame_columns(frame: &Bound<'_, PyAny>) -> PyResult<CandleColumns> {
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
    Ok(CandleColumns {
        close,
        open: col("open")?,
        high: col("high")?,
        low: col("low")?,
        volume: col("volume")?,
    })
}

/// Turn a Python argument into an [`AnySource`] in the requested domain: either
/// an existing `Indicator` (cloned, so the argument stays usable) or a number
/// lifted to a constant of that domain.
/// Require a candle-rooted source. Some indicators (e.g. Keltner) read OHLC
/// bars internally, so their source must consume `Candle`s too.
pub(crate) fn require_candle_source(src: AnySource) -> PyResult<Source<Atom>> {
    match src {
        // Lifted, not rejected: an indicator that reads OHLC internally is happy
        // with a bar-only source, it just needs the atom-shaped input type.
        AnySource::Candle(s) => Ok(atom_over_candle(s)),
        AnySource::Atom(s) => Ok(s),
        AnySource::Const(c) => Ok(runtime::erase(Value::<Atom>::new(c))),
        AnySource::Real(_) | AnySource::Snapshot(_) => Err(PyTypeError::new_err(
            "this indicator reads OHLC bars internally, so its source must be \
             candle-rooted (e.g. close()), not identity- or snapshot-rooted",
        )),
    }
}

pub(crate) fn coerce_operand(obj: &Bound<'_, PyAny>) -> PyResult<AnySource> {
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

pub(crate) fn values_to_dict<'py>(
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
pub(crate) enum OutputKind {
    /// pandas — carries the input's index to preserve alignment.
    Pandas(Py<PyAny>),
    Polars,
    /// Anything else (lists, dicts, NumPy): a NumPy array, or a plain Python
    /// container if NumPy is not importable.
    Numpy,
}

impl OutputKind {
    pub(crate) fn detect(data: &Bound<'_, PyAny>) -> PyResult<Self> {
        match module_root(data).as_deref() {
            Some("pandas") => Ok(OutputKind::Pandas(data.getattr("index")?.unbind())),
            Some("polars") => Ok(OutputKind::Polars),
            _ => Ok(OutputKind::Numpy),
        }
    }
}

/// The top-level package a Python object's type comes from, e.g. `"pandas"` for
/// a `DataFrame` (whose type lives in `pandas.core.frame`).
pub(crate) fn module_root(obj: &Bound<'_, PyAny>) -> Option<String> {
    let module: String = obj.get_type().getattr("__module__").ok()?.extract().ok()?;
    Some(module.split('.').next().unwrap_or("").to_string())
}

/// Build a numeric output series. Warm-up `None`s become `NaN`.
pub(crate) fn build_floats(
    py: Python<'_>,
    kind: &OutputKind,
    values: Vec<Option<f64>>,
) -> PyResult<Py<PyAny>> {
    match kind {
        OutputKind::Pandas(index) => {
            let series = py.import("pandas")?.getattr("Series")?;
            let kwargs = PyDict::new(py);
            kwargs.set_item("index", index.bind(py))?;
            let data = ndarray_from_values(py, &values)?;
            Ok(series.call((data,), Some(&kwargs))?.unbind())
        }
        OutputKind::Polars => {
            let data = ndarray_from_values(py, &values)?;
            Ok(py.import("polars")?.getattr("Series")?.call1((data,))?.unbind())
        }
        OutputKind::Numpy => match ndarray_from_values(py, &values) {
            Ok(arr) => Ok(arr.unbind()),
            // No NumPy: fall back to a plain list, which preserves the
            // warm-up `None`s rather than flattening them to `NaN`.
            Err(_) => Ok(values.into_pyobject(py)?.into_any().unbind()),
        },
    }
}

/// Build a 1-D `float64` NumPy array of `values`, warm-up `None`s as `NaN`,
/// writing **straight into NumPy's own buffer**.
///
/// Three earlier spellings, each worse:
///
/// * `np.asarray(vec_of_f64)` — goes through pyo3's `Vec<f64> -> list`, so a
///   Python `float` object per element for NumPy to copy back out.
/// * `np.frombuffer(bytearray)` — one `memcpy` into a `bytearray` and a
///   zero-copy wrap, but still an intermediate `Vec<f64>` (to flatten the
///   `Option`s) plus a `bytearray` allocation: three passes over the data and
///   two large allocations. Measured at 19.3 ns/sample.
/// * this — `np.empty` once, then fill its buffer in a single pass.
///
/// The `Option` flattening happens *during* that pass rather than into a
/// temporary, so the data is touched once.
fn ndarray_from_values<'py>(
    py: Python<'py>,
    values: &[Option<f64>],
) -> PyResult<Bound<'py, PyAny>> {
    let np = py.import("numpy")?;
    let kwargs = PyDict::new(py);
    kwargs.set_item("dtype", np.getattr("float64")?)?;
    let arr = np.getattr("empty")?.call((values.len(),), Some(&kwargs))?;

    // `np.empty` hands back an owned, writable, C-contiguous `float64` array,
    // so the buffer request cannot fail for shape reasons.
    let buf = pyo3::buffer::PyBuffer::<f64>::get(&arr)?;
    let slice = buf
        .as_mut_slice(py)
        .ok_or_else(|| PyTypeError::new_err("numpy array is not writable"))?;
    for (cell, v) in slice.iter().zip(values) {
        cell.set(v.unwrap_or(f64::NAN));
    }
    Ok(arr)
}

/// Allocate a 1-D `float64` NumPy array of `len` cells and let `fill` write them.
///
/// The zero-intermediate output path: the indicator's values land in NumPy's own
/// buffer as they are produced, so nothing is staged in between.
/// [`ndarray_from_values`] is the same idea one step later — it still has to read
/// a `Vec<Option<f64>>` that somebody already built and paid for.
///
/// What the `Vec` cost, per sample, is not obvious until it is gone: a 16-byte
/// `Option<f64>` store with a capacity check on the way in, then a re-read,
/// unwrap and 8-byte store on the way out — two passes over 3.2 MB for a
/// 200 000-bar frame, and 3.2 MB of allocation that peaks alongside NumPy's own
/// 1.6 MB.
///
/// `fill` gets `&[Cell<f64>]` rather than `&mut [f64]` because the buffer is
/// aliasable from Python's side; that is pyo3's `PyBuffer` contract, not a
/// choice here. Every cell must be written — `np.empty` does not zero, so a
/// skipped cell reads as whatever was in that page.
pub(crate) fn numpy_filled<'py>(
    py: Python<'py>,
    len: usize,
    fill: impl FnOnce(&[std::cell::Cell<f64>]),
) -> PyResult<Bound<'py, PyAny>> {
    let np = py.import("numpy")?;
    let kwargs = PyDict::new(py);
    kwargs.set_item("dtype", np.getattr("float64")?)?;
    let arr = np.getattr("empty")?.call((len,), Some(&kwargs))?;

    // `np.empty` hands back an owned, writable, C-contiguous `float64` array, so
    // the buffer request cannot fail for shape reasons.
    let buf = pyo3::buffer::PyBuffer::<f64>::get(&arr)?;
    let slice = buf
        .as_mut_slice(py)
        .ok_or_else(|| PyTypeError::new_err("numpy array is not writable"))?;
    fill(slice);
    Ok(arr)
}

/// Wrap an already-built NumPy array in whatever the input library was.
///
/// The tail of the fast path: [`numpy_filled`] has produced the data, and this
/// only decides what Python type it arrives as. Split out from
/// [`build_floats`] so the array can be filled *before* this runs — that
/// ordering is the entire point of the zero-intermediate path.
pub(crate) fn wrap_floats<'py>(
    py: Python<'py>,
    kind: &OutputKind,
    arr: Bound<'py, PyAny>,
) -> PyResult<Py<PyAny>> {
    match kind {
        OutputKind::Pandas(index) => {
            let series = py.import("pandas")?.getattr("Series")?;
            let kwargs = PyDict::new(py);
            kwargs.set_item("index", index.bind(py))?;
            Ok(series.call((arr,), Some(&kwargs))?.unbind())
        }
        OutputKind::Polars => Ok(py
            .import("polars")?
            .getattr("Series")?
            .call1((arr,))?
            .unbind()),
        OutputKind::Numpy => Ok(arr.unbind()),
    }
}

/// Build a boolean output series. Signals never warm up to a missing value.
pub(crate) fn build_bools(py: Python<'_>, kind: &OutputKind, values: Vec<bool>) -> PyResult<Py<PyAny>> {
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
pub(crate) fn build_multi(
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
/// Wrap a **bar**-rooted `Real` source: one that reads only the OHLCV bar.
///
/// The fast domain. `Candle` is 40 bytes, `Copy`, and needs no drop, so a chain
/// over it neither retains nor moves the 88-byte `Atom` an atom-rooted chain
/// does — measured at ~22 instructions per bar against 125.8. Every indicator
/// that reads the bar and nothing else belongs here; only the overlay readers
/// and the calendar leaves need [`atom_source`].
pub(crate) fn bar_source<T>(inner: T) -> PyIndicator
where
    T: Indicator<Input = Candle, Output = Real> + Clone + Send + Sync + 'static,
{
    PyIndicator::wrap(AnySource::Candle(runtime::erase(inner)))
}

/// Wrap an **atom**-rooted `Real` source: one that reads the bar *and* its side
/// channels (`time`, `overlays`).
///
/// Named for the input it takes. The old name, `candle_source`, described what
/// callers meant rather than what the chain consumed — every one of these is fed
/// a whole 88-byte `Atom` even when it only ever looks at the 40-byte candle
/// inside it. See `docs/PERFORMANCE.md`, P1.
pub(crate) fn atom_source<T>(inner: T) -> PyIndicator
where
    T: Indicator<Input = Atom, Output = Real> + Clone + Send + Sync + 'static,
{
    PyIndicator::wrap(AnySource::Atom(runtime::erase(inner)))
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
        pub(crate) fn $name(source: Option<PyRef<'_, PyAtomSource>>) -> PyIndicator {
            match source.map(|s| s.inner.clone()) {
                None => PyIndicator::wrap(AnySource::Atom(runtime::erase($default_ctor))),
                Some(AnyAtomSource::Atom(s)) => {
                    PyIndicator::wrap(AnySource::Atom(runtime::erase($of_ctor(s))))
                }
                Some(AnyAtomSource::Snapshot(s)) => {
                    PyIndicator::wrap(AnySource::Snapshot(runtime::erase($of_ctor(s))))
                }
            }
        }
    };
}

/// Candle-field leaves (`close`, `high`, …), which read the bar and nothing else.
///
/// Identical to [`atom_leaf_source!`] except for the `source=`-omitted case: that
/// roots on the **bar** domain rather than lifting every candle into an 88-byte
/// `Atom` first. `ta.close()` is the most common root in the API, so this is the
/// one that matters most. An explicit `source=` still selects core's atom- or
/// snapshot-rooted `Field`, because then the atom is what the caller has.
macro_rules! bar_leaf_source {
    ($name:ident, $field:ty, $of_ctor:path, $doc:literal) => {
        #[doc = $doc]
        #[pyfunction]
        #[pyo3(signature = (source = None))]
        pub(crate) fn $name(source: Option<PyRef<'_, PyAtomSource>>) -> PyIndicator {
            match source.map(|s| s.inner.clone()) {
                None => PyIndicator::wrap(AnySource::Candle(runtime::erase(
                    BarField::<$field>::new(),
                ))),
                Some(AnyAtomSource::Atom(s)) => {
                    PyIndicator::wrap(AnySource::Atom(runtime::erase($of_ctor(s))))
                }
                Some(AnyAtomSource::Snapshot(s)) => {
                    PyIndicator::wrap(AnySource::Snapshot(runtime::erase($of_ctor(s))))
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
        pub(crate) fn $name(source: Option<PyRef<'_, PyAtomSource>>) -> PySignal {
            match source.map(|s| s.inner.clone()) {
                None => PySignal::wrap(AnySignal::Atom(SignalBox::new($default_ctor))),
                Some(AnyAtomSource::Atom(s)) => {
                    PySignal::wrap(AnySignal::Atom(SignalBox::new($of_ctor(s))))
                }
                Some(AnyAtomSource::Snapshot(s)) => {
                    PySignal::wrap(AnySignal::Snapshot(SignalBox::new($of_ctor(s))))
                }
            }
        }
    };
}

bar_leaf_source!(open, BarOpen, Open::of, "Source: the bar's open price.");
bar_leaf_source!(high, BarHigh, High::of, "Source: the bar's high price.");
bar_leaf_source!(low, BarLow, Low::of, "Source: the bar's low price.");
bar_leaf_source!(
    close,
    BarClose,
    Close::of,
    "Source: the bar's close price. Pass `source=ta.pick(key)` to read a specific asset's close out of a `Snapshot`."
);
bar_leaf_source!(
    volume,
    BarVolume,
    Volume::of,
    "Source: the bar's volume."
);
bar_leaf_source!(
    typical,
    BarTypical,
    Typical::of,
    "Source: the bar's typical price, (high + low + close) / 3."
);
bar_leaf_source!(
    median,
    BarMedian,
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
pub(crate) fn pick(symbol: Option<&Bound<'_, PyAny>>, freq: Option<&Bound<'_, PyAny>>) -> PyResult<PyAtomSource> {
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
        inner: AnyAtomSource::Snapshot(runtime::erase(pick)),
    })
}

/// Source: the raw value stream, passed straight through. Root an indicator
/// here (instead of a candle accessor) to consume a bare 1-D series of numbers
/// — `update(float)` and `feed([...])` rather than candles.
#[pyfunction]
pub(crate) fn identity() -> PyIndicator {
    PyIndicator::wrap(AnySource::Real(runtime::erase(Identity::new())))
}

/// Source: a constant value, ignoring the input. Mirrors Rust's `Value`, which
/// is generic over the input — so this is domain-**neutral**: in an operator it
/// adopts its partner's domain (works in both `rsi(close()).gt(value(70))` and
/// `rsi(identity()).gt(value(70))`). Used entirely on its own it is candle-
/// rooted. A bare Python number works the same way, so `gt(70)` == `gt(value(70))`.
#[pyfunction]
pub(crate) fn value(value: f64) -> PyIndicator {
    PyIndicator::wrap(AnySource::Const(value))
}

/// Signal: a periodic pulse that fires every `period` bars — the canonical
/// `rebalance_on` gate. Mirrors Rust's `Every` and the spec's `!every N`.
///
/// The first fire is **delayed**, not immediate: `every(5)` fires on bar 4
/// (0-indexed) and every 5th bar after, so each pulse closes a full 5-bar block
/// rather than firing on the first bar and then again 5 bars later. `every(1)`
/// is every bar. Candle-rooted, so it lifts into any snapshot-rooted slot:
///
/// ```python
/// strat.rebalance_on(ta.every(20))     # resize about monthly on daily bars
/// ```
#[pyfunction]
pub(crate) fn every(period: usize) -> PyResult<PySignal> {
    ensure_period(period)?;
    Ok(PySignal::wrap(AnySignal::Atom(SignalBox::new(
        Every::<Atom>::new(period),
    ))))
}

macro_rules! src_period {
    ($name:ident, $ty:ident, $doc:literal) => {
        #[doc = $doc]
        #[pyfunction]
        pub(crate) fn $name(source: PyRef<'_, PyIndicator>, period: usize) -> PyResult<PyIndicator> {
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
// `skewness`/`kurtosis` name-collide with the `fugazi.metrics` functions of the
// same name (which take a returns vector). These are the *indicator* forms
// (a source + window); the Rust idents are suffixed to disambiguate, but the
// Python names stay `skewness`/`kurtosis` — the metric twins live under
// `fugazi.metrics.*`, so the two never clash from Python.
/// Rolling population skewness (standardized 3rd moment) of `source` over `period`.
#[pyfunction(name = "skewness")]
pub(crate) fn skewness_indicator(source: PyRef<'_, PyIndicator>, period: usize) -> PyResult<PyIndicator> {
    ensure_period(period)?;
    Ok(PyIndicator::wrap(map_source!(source.src.clone(), |s| {
        Skewness::new(s, period)
    })))
}
/// Rolling population kurtosis (raw standardized 4th moment; ~3 for normal, not
/// excess) of `source` over `period`.
#[pyfunction(name = "kurtosis")]
pub(crate) fn kurtosis_indicator(source: PyRef<'_, PyIndicator>, period: usize) -> PyResult<PyIndicator> {
    ensure_period(period)?;
    Ok(PyIndicator::wrap(map_source!(source.src.clone(), |s| {
        Kurtosis::new(s, period)
    })))
}
src_period!(
    zscore,
    ZScore,
    "Rolling z-score of `source` over `period`: `(x - mean) / stddev`."
);
src_period!(
    percentile_rank,
    PercentileRank,
    "Where the current reading sits in its own trailing `period`-bar \
     distribution: `count(v <= x) / period`, in `(0, 1]`. A fresh high reads \
     `1.0`, a fresh low `1/period`."
);
src_period!(
    bars_since_high,
    BarsSinceHigh,
    "Bars elapsed since `source` last set a new `period`-bar high — `0` on the \
     bar that sets it, up to `period - 1`. `None` until the window is full."
);
src_period!(
    bars_since_low,
    BarsSinceLow,
    "Bars elapsed since `source` last set a new `period`-bar low."
);

/// The `pct`-quantile of `source` over the trailing `period` bars —
/// `pct=0.5` is the rolling median, `0.8` the 80th percentile.
///
/// Interpolates linearly between the bracketing samples (R type-7, numpy's
/// default), the same convention `fugazi.metrics.value_at_risk` uses. Returns
/// `None` until the window is full.
///
/// The adaptive-threshold primitive — an RSI compared against its own
/// trailing-year 80th percentile rather than a hardcoded 70:
///
/// ```python
/// rsi = ta.rsi(ta.close(), 14)
/// hot = rsi.gt(ta.percentile(ta.rsi(ta.close(), 14), 252, 0.8))
/// ```
///
/// For the extremes prefer `rolling_max` / `rolling_min`, which are O(1).
#[pyfunction]
pub(crate) fn percentile(source: PyRef<'_, PyIndicator>, period: usize, pct: f64) -> PyResult<PyIndicator> {
    ensure_period(period)?;
    if !(0.0..=1.0).contains(&pct) {
        return Err(PyValueError::new_err(format!(
            "percentile pct must lie in [0.0, 1.0], got {pct}"
        )));
    }
    Ok(PyIndicator::wrap(map_source!(source.src.clone(), |s| {
        Percentile::new(s, period, pct)
    })))
}

/// Bars elapsed since `source` (a **signal**) last read true — `0` on the
/// firing bar, `1` on the next, and so on.
///
/// Returns `None` until the signal has fired at least once. That makes every
/// threshold against it read false until then, which is the conservative
/// answer in both directions: a never-fired signal can't gate an entry in, and
/// a clock that never started can't time-stop a position out.
///
/// ```python
/// # Only act on a crossover that happened within the last 5 bars.
/// cross = ta.close().crosses_above(ta.sma(ta.close(), 50))
/// fresh = ta.bars_since(cross).lt(ta.value(5.0))
/// ```
#[pyfunction]
pub(crate) fn bars_since(source: PyRef<'_, PySignal>) -> PyResult<PyIndicator> {
    let out = match source.sig.clone() {
        AnySignal::Candle(s) => AnySource::Candle(runtime::erase(BarsSince::new(s))),
        AnySignal::Atom(s) => AnySource::Atom(runtime::erase(BarsSince::new(s))),
        AnySignal::Real(s) => AnySource::Real(runtime::erase(BarsSince::new(s))),
        AnySignal::Snapshot(s) => AnySource::Snapshot(runtime::erase(BarsSince::new(s))),
    };
    Ok(PyIndicator::wrap(out))
}

/// Rolling Pearson correlation between two Real sources over `period`, in
/// `[-1, 1]`. Both operands must share an input domain (both candle-rooted,
/// both value-rooted, or both snapshot-rooted). A dispersion-free window on
/// either leg reads `0.0`.
#[pyfunction]
pub(crate) fn correlation(
    lhs: PyRef<'_, PyIndicator>,
    rhs: PyRef<'_, PyIndicator>,
    period: usize,
) -> PyResult<PyIndicator> {
    ensure_period(period)?;
    Ok(PyIndicator::wrap(combine_sources!(
        lhs.src.clone(),
        rhs.src.clone(),
        |l, r| Correlation::new(l, r, period)
    )?))
}
/// Lo-MacKinlay variance-ratio regime classifier over `source`'s first
/// differences: reads `1.0` under the random-walk null, `> 1.0` in a trending
/// (positively autocorrelated) regime and `< 1.0` in a mean-reverting one.
///
/// `period` is the retained window and `lag` the aggregation horizon; `lag`
/// must be at least 2 and `period` at least `lag + 2`. Unlike the other
/// indicators this recomputes over the whole window each bar (O(`period`), not
/// O(1)) — see the Rust docs. A dispersion-free window (constant one-period
/// returns) reads `1.0`.
#[pyfunction]
pub(crate) fn variance_ratio(
    source: PyRef<'_, PyIndicator>,
    period: usize,
    lag: usize,
) -> PyResult<PyIndicator> {
    if lag < 2 {
        return Err(PyValueError::new_err("lag must be at least 2"));
    }
    if period < lag + 2 {
        return Err(PyValueError::new_err(
            "period must be at least lag + 2 (need >1 overlapping block)",
        ));
    }
    Ok(PyIndicator::wrap(map_source!(source.src.clone(), |s| {
        VarianceRatio::new(s, period, lag)
    })))
}
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

/// Logarithm of `source` in `base` (defaults to natural log, `e`). Emits
/// `None` on samples where the source's output is non-positive. Raises
/// `ValueError` if `base` is not a finite positive number distinct from `1.0`.
#[pyfunction]
#[pyo3(signature = (source, base = std::f64::consts::E))]
pub(crate) fn log(source: PyRef<'_, PyIndicator>, base: f64) -> PyResult<PyIndicator> {
    ensure_log_base(base)?;
    Ok(PyIndicator::wrap(map_source!(source.src.clone(), |s| {
        Log::new(s, base)
    })))
}

pub(crate) fn ensure_log_base(base: f64) -> PyResult<()> {
    if base.is_finite() && base > 0.0 && base != 1.0 {
        Ok(())
    } else {
        Err(PyValueError::new_err(format!(
            "log base must be a finite positive number distinct from 1.0, got {base}"
        )))
    }
}

macro_rules! bar_period {
    ($name:ident, $ty:ident, $doc:literal) => {
        #[doc = $doc]
        #[pyfunction]
        pub(crate) fn $name(period: usize) -> PyResult<PyIndicator> {
            ensure_period(period)?;
            // `Identity::<Candle>` rather than `CurrentBar` (which is
            // `CurrentBar<Identity<Atom>>`): these read the bar and nothing else,
            // so they belong in the bar domain. See `bar_source`.
            Ok(bar_source($ty::new(Identity::<Candle>::new(), period)))
        }
    };
}

bar_period!(
    atr,
    Atr,
    "Average true range over `period` (consumes the full bar)."
);
bar_period!(
    parkinson,
    Parkinson,
    "Parkinson high/low range volatility estimator over `period`."
);
bar_period!(
    garman_klass,
    GarmanKlass,
    "Garman-Klass OHLC volatility estimator over `period`."
);
bar_period!(
    rogers_satchell,
    RogersSatchell,
    "Rogers-Satchell drift-independent OHLC volatility estimator over `period`."
);
bar_period!(
    mfi,
    Mfi,
    "Money-flow index over `period` (consumes the full bar)."
);
bar_period!(
    vwap,
    Vwap,
    "Volume-weighted average price over `period` (rolling)."
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
        pub(crate) fn $name() -> PyIndicator {
            bar_source($ty::new(Identity::<Candle>::new()))
        }
    };
}

bar_noarg!(
    obv,
    Obv,
    "On-balance volume (cumulative; reset to re-anchor)."
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
        pub(crate) fn $name(period: usize) -> PyResult<PyMulti> {
            ensure_period(period)?;
            Ok(PyMulti {
                inner: AnyMulti::Atom(MultiBox::new($ty::new(CurrentBar::new(), period))),
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
pub(crate) fn sar(step: f64, max: f64) -> PyIndicator {
    atom_source(Sar::new(CurrentBar::new(), step, max))
}

/// MACD of `source`: {macd, signal, histogram}.
#[pyfunction]
#[pyo3(signature = (source, fast_period = 12, slow_period = 26, signal_period = 9))]
pub(crate) fn macd(
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
pub(crate) fn bollinger(source: PyRef<'_, PyIndicator>, period: usize, k: f64) -> PyResult<PyMulti> {
    ensure_period(period)?;
    Ok(PyMulti {
        inner: map_multi!(source.src.clone(), |s| Bollinger::new(s, period, k)),
    })
}

/// Keltner channels around an EMA of `source`: {upper, middle, lower}.
#[pyfunction]
#[pyo3(signature = (source, ema_period = 20, atr_period = 10, multiplier = 2.0))]
pub(crate) fn keltner(
    source: PyRef<'_, PyIndicator>,
    ema_period: usize,
    atr_period: usize,
    multiplier: f64,
) -> PyResult<PyMulti> {
    ensure_period(ema_period)?;
    ensure_period(atr_period)?;
    let s = require_candle_source(source.src.clone())?;
    Ok(PyMulti {
        inner: AnyMulti::Atom(MultiBox::new(Keltner::new(
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
pub(crate) fn donchian(
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
pub(crate) fn stoch_rsi(
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
pub(crate) fn resample(every: usize, inner: PyRef<'_, PyIndicator>) -> PyResult<PyIndicator> {
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
    Ok(PyIndicator::wrap(AnySource::Atom(runtime::erase(
        ResampleThen::new(every, inner_candle),
    ))))
}

/// Hold the last `Some` output of an indicator or signal, re-emitting it on
/// ticks where the source returns `None`. Domain-preserving: `latch()` of a
/// candle-rooted source is candle-rooted, of an identity-rooted signal is
/// identity-rooted, and so on. Pair with `resample()` so per-base-tick reads
/// see the finished HTF value between boundaries.
#[pyfunction]
pub(crate) fn latch<'py>(py: Python<'py>, source: &Bound<'py, PyAny>) -> PyResult<Py<PyAny>> {
    if let Ok(ind) = source.cast::<PyIndicator>() {
        let out = match ind.borrow().src.clone() {
            AnySource::Candle(s) => AnySource::Candle(runtime::erase(Latch::new(s))),
            AnySource::Atom(s) => AnySource::Atom(runtime::erase(Latch::new(s))),
            AnySource::Real(s) => AnySource::Real(runtime::erase(Latch::new(s))),
            AnySource::Snapshot(s) => AnySource::Snapshot(runtime::erase(Latch::new(s))),
            // A latched constant is still that constant — the source never
            // emits `None`, so the latch never fires. Return as-is.
            other @ AnySource::Const(_) => other,
        };
        return Ok(PyIndicator::wrap(out).into_pyobject(py)?.into_any().unbind());
    }
    if let Ok(sig) = source.cast::<PySignal>() {
        let out = match sig.borrow().sig.clone() {
            AnySignal::Candle(s) => AnySignal::Candle(SignalBox::new(Latch::new(s))),
            AnySignal::Atom(s) => AnySignal::Atom(SignalBox::new(Latch::new(s))),
            AnySignal::Real(s) => AnySignal::Real(SignalBox::new(Latch::new(s))),
            AnySignal::Snapshot(s) => AnySignal::Snapshot(SignalBox::new(Latch::new(s))),
        };
        return Ok(PySignal::wrap(out).into_pyobject(py)?.into_any().unbind());
    }
    Err(PyTypeError::new_err(
        "latch() expects an fugazi Indicator or Signal",
    ))
}

/// Passthrough wrapper that forces the argument's reported `unstable_bars()`
/// to `0`. Same output, same `warm_up_bars()`; a downstream reader of
/// `stable_bars()` no longer waits for this subtree's IIR settling tail.
/// Accepts either an `Indicator` or a `Signal` and returns the same kind. The
/// explicit opt-out of the safe default that waits for the tail:
///
/// ```python
/// # Skip the Ema's unstable tail when computing readiness.
/// src = ta.unstable(ta.ema(ta.close(), 20))
/// ```
#[pyfunction]
pub(crate) fn unstable(py: Python<'_>, arg: &Bound<'_, PyAny>) -> PyResult<Py<PyAny>> {
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

/// Three-source ternary: reads `cond` each bar and returns `then`'s value
/// when the condition is true, `otherwise`'s when false. Returns `None` while
/// `cond` reads `None` or any of the three sources is still warming.
///
/// All three sources are advanced every bar (never short-circuited), so a
/// branch that doesn't fire this bar keeps its warm-up progressing. Reads
/// naturally as an English sentence:
///
/// ```python
/// # ADX-gated momentum: 20-bar ROC when ADX > 25, else 0.
/// cond = ta.adx(ta.current_bar(), 14).adx().gt(25.0)
/// score = ta.if_else(cond, ta.close().roc(20), ta.value(0.0))
/// ```
///
/// All three inputs must share the same input domain (candle / real /
/// snapshot). A neutral constant (`ta.value(...)`) adopts its partner's
/// domain, so a bare `otherwise=ta.value(0.0)` composes with a candle-rooted
/// condition without extra ceremony.
#[pyfunction]
pub(crate) fn if_else(cond: PyRef<'_, PySignal>, then: PyRef<'_, PyIndicator>, otherwise: PyRef<'_, PyIndicator>) -> PyResult<PyIndicator> {
    // Resolve constants against each other first (via `pair`), then match
    // the pair's domain against the condition's.
    let branches = pair(then.src.clone(), otherwise.src.clone())?;
    let cond_sig = cond.sig.clone();
    let out = match (cond_sig, branches) {
        (AnySignal::Candle(c), Pair::Candle(t, f)) => {
            AnySource::Candle(runtime::erase(IfElse::new(c, t, f)))
        }
        (AnySignal::Atom(c), Pair::Atom(t, f)) => {
            AnySource::Atom(runtime::erase(IfElse::new(c, t, f)))
        }
        // The condition and the branches can land in different domains — a
        // bar-rooted `close()` test over atom-rooted `get()` branches, say. Lift
        // whichever side is bar-only.
        (AnySignal::Candle(c), Pair::Atom(t, f)) => AnySource::Atom(runtime::erase(
            IfElse::new(atom_signal_over_candle(c), t, f),
        )),
        (AnySignal::Atom(c), Pair::Candle(t, f)) => AnySource::Atom(runtime::erase(
            IfElse::new(c, atom_over_candle(t), atom_over_candle(f)),
        )),
        (AnySignal::Real(c), Pair::Real(t, f)) => {
            AnySource::Real(runtime::erase(IfElse::new(c, t, f)))
        }
        (AnySignal::Snapshot(c), Pair::Snapshot(t, f)) => {
            AnySource::Snapshot(runtime::erase(IfElse::new(c, t, f)))
        }
        _ => return Err(domain_mismatch()),
    };
    Ok(PyIndicator::wrap(out))
}

/// Read a per-atom overlay column by its `key` in `schema`. Rooted at the
/// atom stream by default, so it slots into the same candle-rooted pipelines
/// as `close()`/`atr()`/etc. When fed a bare `Candle` (no overlays), the
/// reader yields `None` — pass an `Atom` carrying an `OverlayInfo` bound to
/// the same schema to see values.
///
/// **Polymorphic on the column's declared type**: a `Real` column yields an
/// `Indicator`, a `Bool` column yields a `Signal`, and a `Str` column yields
/// a `StrSource`. Use `get_real()` / `get_bool()` / `get_str()` if you want
/// to assert the returned type at the call site.
///
/// `source` re-roots the reader onto a selected atom, exactly as it does on
/// `close()` and the other atom leaves — `get(schema, "funding", source=pick("M"))`
/// reads M's overlay column out of a multi-symbol snapshot while the strategy
/// trades something else. A source that yields an atom carrying no overlays,
/// or none bound to this schema, reads `None` rather than raising.
///
/// Raises `ValueError` if `key` isn't registered in `schema`.
#[pyfunction]
#[pyo3(signature = (schema, key, source = None))]
pub(crate) fn get<'py>(
    py: Python<'py>,
    schema: &PySchema,
    key: &str,
    source: Option<PyRef<'_, PyAtomSource>>,
) -> PyResult<Py<PyAny>> {
    let source = source.map(|s| s.inner.clone());
    match schema.inner.type_of_key(key) {
        Some(OverlayType::Real) => {
            let ind = build_get_real(schema, key, source)?;
            Ok(ind.into_pyobject(py)?.into_any().unbind())
        }
        Some(OverlayType::Bool) => {
            let sig = build_get_bool(schema, key, source)?;
            Ok(sig.into_pyobject(py)?.into_any().unbind())
        }
        Some(OverlayType::Str) => {
            let src = build_get_str(schema, key, source)?;
            Ok(src.into_pyobject(py)?.into_any().unbind())
        }
        None => Err(unknown_key_error(schema, key)),
    }
}

/// Read a `Real`-typed overlay column. Always returns an `Indicator`; raises
/// `ValueError` if the column is missing or its declared type isn't `Real`.
#[pyfunction]
#[pyo3(signature = (schema, key, source = None))]
pub(crate) fn get_real(
    schema: &PySchema,
    key: &str,
    source: Option<PyRef<'_, PyAtomSource>>,
) -> PyResult<PyIndicator> {
    if !schema.inner.contains(key) {
        return Err(unknown_key_error(schema, key));
    }
    build_get_real(schema, key, source.map(|s| s.inner.clone()))
}

/// Read a `Bool`-typed overlay column. Always returns a `Signal`; raises
/// `ValueError` if the column is missing or its declared type isn't `Bool`.
#[pyfunction]
#[pyo3(signature = (schema, key, source = None))]
pub(crate) fn get_bool(
    schema: &PySchema,
    key: &str,
    source: Option<PyRef<'_, PyAtomSource>>,
) -> PyResult<PySignal> {
    if !schema.inner.contains(key) {
        return Err(unknown_key_error(schema, key));
    }
    build_get_bool(schema, key, source.map(|s| s.inner.clone()))
}

/// Read a `Str`-typed overlay column. Always returns a `StrSource`; raises
/// `ValueError` if the column is missing or its declared type isn't `Str`.
#[pyfunction]
#[pyo3(signature = (schema, key, source = None))]
pub(crate) fn get_str(
    schema: &PySchema,
    key: &str,
    source: Option<PyRef<'_, PyAtomSource>>,
) -> PyResult<PyStrSource> {
    if !schema.inner.contains(key) {
        return Err(unknown_key_error(schema, key));
    }
    build_get_str(schema, key, source.map(|s| s.inner.clone()))
}

pub(crate) fn build_get_real(
    schema: &PySchema,
    key: &str,
    source: Option<AnyAtomSource>,
) -> PyResult<PyIndicator> {
    // Validate through `try_new`, which owns the key/type diagnostics — `of`
    // is infallible and would skip them. Then rebuild with the source.
    let checked =
        GetReal::try_new(&schema.inner, key).map_err(|e| PyValueError::new_err(e.to_string()))?;
    Ok(match source {
        None => PyIndicator::wrap(AnySource::Atom(runtime::erase(checked))),
        Some(AnyAtomSource::Atom(s)) => PyIndicator::wrap(AnySource::Atom(runtime::erase(
            GetReal::of(&schema.inner, key, s),
        ))),
        Some(AnyAtomSource::Snapshot(s)) => PyIndicator::wrap(AnySource::Snapshot(runtime::erase(
            GetReal::of(&schema.inner, key, s),
        ))),
    })
}

pub(crate) fn build_get_bool(
    schema: &PySchema,
    key: &str,
    source: Option<AnyAtomSource>,
) -> PyResult<PySignal> {
    let checked =
        GetBool::try_new(&schema.inner, key).map_err(|e| PyValueError::new_err(e.to_string()))?;
    Ok(match source {
        None => PySignal::wrap(AnySignal::Atom(SignalBox::new(checked))),
        Some(AnyAtomSource::Atom(s)) => PySignal::wrap(AnySignal::Atom(SignalBox::new(
            GetBool::of(&schema.inner, key, s),
        ))),
        Some(AnyAtomSource::Snapshot(s)) => PySignal::wrap(AnySignal::Snapshot(SignalBox::new(
            GetBool::of(&schema.inner, key, s),
        ))),
    })
}

pub(crate) fn build_get_str(
    schema: &PySchema,
    key: &str,
    source: Option<AnyAtomSource>,
) -> PyResult<PyStrSource> {
    let checked =
        GetStr::try_new(&schema.inner, key).map_err(|e| PyValueError::new_err(e.to_string()))?;
    Ok(match source {
        None => PyStrSource::wrap(AnyStrSource::Atom(runtime::erase(checked))),
        Some(AnyAtomSource::Atom(s)) => PyStrSource::wrap(AnyStrSource::Atom(runtime::erase(
            GetStr::of(&schema.inner, key, s),
        ))),
        Some(AnyAtomSource::Snapshot(s)) => PyStrSource::wrap(AnyStrSource::Snapshot(
            runtime::erase(GetStr::of(&schema.inner, key, s)),
        )),
    })
}

/// The "unknown overlay key" error message used by `get*()` — lists the
/// registered keys so a typo is easy to spot, or hints that the caller
/// forgot to bind a schema.
pub(crate) fn unknown_key_error(schema: &PySchema, key: &str) -> PyErr {
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

// ---------------------------------------------------------------------------
// compute_overlays: derive overlay columns from indicator specs and attach
// them onto a series of Atoms / Snapshots — the dataset "overlays" step.
// ---------------------------------------------------------------------------

/// Pull the erased indicator handle + its overlay column type out of a Python
/// carrier (`Indicator` → Real, `Signal` → Bool, `StrSource` → Str). Returns
/// `None` if `v` is none of those. The handle is deep-cloned so the caller's
/// carrier is untouched; a domain-neutral `Const` carrier synthesises a
/// constant leaf. The raw inner chain (not the `SignalBox` wrapper) is taken
/// on purpose — a warming bool overlay then reads `None`, not `false`.
///
/// This is the one place the bindings still hand a *payload* box out: the
/// overlay-column API takes a heterogeneous list whose members differ in input
/// domain (atom-rooted, value-rooted, snapshot-rooted), which a domain-typed
/// `Chain` cannot express in one `Vec`. Re-wrapping costs one `Box` per column
/// at build time and nothing per bar, so it stays until
/// `spec::overlay` itself moves over.
pub(crate) fn carrier_inner_indicator(
    v: &Bound<'_, PyAny>,
) -> PyResult<Option<(Box<dyn runtime::PayloadIndicator>, OverlayType)>> {
    if let Ok(ind) = v.cast::<PyIndicator>() {
        let ind = ind.borrow();
        let inner: Box<dyn runtime::PayloadIndicator> = match &ind.src {
            // Overlay columns ride the payload vocabulary; a bar-only chain is
            // lifted so the column set stays one homogeneous list.
            AnySource::Candle(s) => runtime::wrap(atom_over_candle(s.clone())),
            AnySource::Atom(s) => runtime::wrap(s.clone()),
            AnySource::Real(s) => runtime::wrap(s.clone()),
            AnySource::Snapshot(s) => runtime::wrap(s.clone()),
            AnySource::Const(c) => runtime::wrap(Value::<Atom>::new(*c)),
        };
        return Ok(Some((inner, OverlayType::Real)));
    }
    if let Ok(sig) = v.cast::<PySignal>() {
        let sig = sig.borrow();
        let inner: Box<dyn runtime::PayloadIndicator> = match &sig.sig {
            AnySignal::Candle(s) => runtime::wrap(atom_over_candle(s.0.clone())),
            AnySignal::Atom(s) => runtime::wrap(s.0.clone()),
            AnySignal::Real(s) => runtime::wrap(s.0.clone()),
            AnySignal::Snapshot(s) => runtime::wrap(s.0.clone()),
        };
        return Ok(Some((inner, OverlayType::Bool)));
    }
    if let Ok(ss) = v.cast::<PyStrSource>() {
        let ss = ss.borrow();
        let inner: Box<dyn runtime::PayloadIndicator> = match &ss.src {
            AnyStrSource::Atom(s) => runtime::wrap(s.clone()),
            AnyStrSource::Snapshot(s) => runtime::wrap(s.clone()),
            AnyStrSource::Const(c) => runtime::wrap(ValueStr::<Atom>::new(c.clone())),
        };
        return Ok(Some((inner, OverlayType::Str)));
    }
    Ok(None)
}

/// The two accepted `overlays` shapes, normalised.
pub(crate) enum OverlayInput {
    /// A `name: NodeSpec` YAML doc (built per series against the input schema).
    Spec(Vec<fugazi_core::spec::overlay::OverlayColumn>),
    /// A dict of pre-built carriers (deep-cloned per series).
    Built(Vec<(String, Box<dyn runtime::PayloadIndicator>)>),
}

/// Parse the `overlays` argument: a YAML string (the fugazi-web overlay-file
/// shape `rsi_14: !rsi { period: 14 }`) or a dict `{name: Indicator|Signal|
/// StrSource}`. `params` resolves `!param` inside the YAML form only.
pub(crate) fn parse_overlay_input(
    overlays: &Bound<'_, PyAny>,
    params: Option<&Bound<'_, PyAny>>,
) -> PyResult<OverlayInput> {
    if let Ok(text) = overlays.extract::<String>() {
        let table = extract_params(params)?;
        let cols = fugazi_core::spec::overlay::columns_from_yaml(
            &text,
            &table,
            std::path::Path::new("."),
            "(overlays)",
        )
        .map_err(|e| PyValueError::new_err(format!("{e:#}")))?;
        return Ok(OverlayInput::Spec(cols));
    }
    if let Ok(dict) = overlays.cast::<pyo3::types::PyDict>() {
        let mut named = Vec::new();
        for (k, v) in dict.iter() {
            let name = k.extract::<String>().map_err(|_| {
                PyTypeError::new_err("overlay column names (dict keys) must be strings")
            })?;
            match carrier_inner_indicator(&v)? {
                Some((ind, _ty)) => named.push((name, ind)),
                None => {
                    return Err(PyTypeError::new_err(format!(
                        "overlay column {name:?} must be an Indicator, Signal, or StrSource \
                         (or pass the whole overlay set as a YAML string)"
                    )));
                }
            }
        }
        return Ok(OverlayInput::Built(named));
    }
    Err(PyTypeError::new_err(
        "`overlays` must be a YAML string or a dict of {name: Indicator|Signal|StrSource}",
    ))
}

/// Build the output schema + a fresh (never-fed) prepared column set for a
/// series with input schema `existing`.
pub(crate) fn build_overlay_prepared(
    existing: &std::sync::Arc<Schema>,
    input: &OverlayInput,
) -> PyResult<(std::sync::Arc<Schema>, Vec<fugazi_core::spec::overlay::PreparedColumn>)> {
    use fugazi_core::spec::overlay as ov;
    let result = match input {
        OverlayInput::Spec(cols) => ov::prepare(existing, cols),
        OverlayInput::Built(named) => {
            let cloned: Vec<(String, Box<dyn runtime::PayloadIndicator>)> =
                named.iter().map(|(n, i)| (n.clone(), i.clone())).collect();
            ov::prepare_built(existing, cloned)
        }
    };
    result.map_err(|e| PyValueError::new_err(format!("{e:#}")))
}

/// Derive the one input overlay schema shared by an atom stream — the schema of
/// the first atom carrying overlays, validated identical (by `Arc` identity)
/// across every other overlay-bearing atom. Bare atoms are ignored; an empty
/// stream (or an all-bare one) yields the empty schema.
pub(crate) fn existing_schema_from_atoms<'a>(
    atoms: impl Iterator<Item = &'a Atom>,
) -> PyResult<std::sync::Arc<Schema>> {
    let mut existing: Option<std::sync::Arc<Schema>> = None;
    for a in atoms {
        if let Some(ov) = &a.overlays {
            match &existing {
                None => existing = Some(ov.schema().clone()),
                Some(e) => {
                    if !std::sync::Arc::ptr_eq(e, ov.schema()) {
                        return Err(PyValueError::new_err(
                            "input atoms carry overlays bound to different schemas; \
                             compute_overlays needs one shared input schema",
                        ));
                    }
                }
            }
        }
    }
    Ok(existing.unwrap_or_else(Schema::empty))
}

/// Extract a `Vec<Atom>` from a Python sequence of `Atom` (or `Candle`).
pub(crate) fn atoms_from_sequence(obj: &Bound<'_, PyAny>) -> PyResult<Vec<Atom>> {
    let iter = obj
        .try_iter()
        .map_err(|_| PyTypeError::new_err("`series` must be an iterable of Atom"))?;
    let mut out = Vec::new();
    for item in iter {
        out.push(extract_atom(&item?)?);
    }
    Ok(out)
}

/// Compute derived overlay columns from indicator specs and attach them onto a
/// series of `Atom`s (single series) or `Snapshot`s (multi-symbol), returning
/// `(schema, augmented)`.
///
/// `overlays` is a YAML doc (`name: !expr { ... }`) or a dict `{name:
/// Indicator|Signal|StrSource}`. Existing overlay columns are preserved (same
/// indexes) and the new columns appended; a computed column reads `None` while
/// it warms up. **Use the returned schema for downstream `get(...)`** — the
/// augmented atoms are bound to it by `Arc` identity.
#[pyfunction]
#[pyo3(signature = (series, overlays, params = None))]
pub(crate) fn compute_overlays<'py>(
    py: Python<'py>,
    series: &Bound<'py, PyAny>,
    overlays: &Bound<'py, PyAny>,
    params: Option<&Bound<'py, PyAny>>,
) -> PyResult<(PySchema, Py<PyAny>)> {
    let input = parse_overlay_input(overlays, params)?;

    // Sniff the first element to pick the atoms-vs-snapshots path.
    let mut iter = series
        .try_iter()
        .map_err(|_| PyTypeError::new_err("`series` must be a sequence of Atom or Snapshot"))?;
    let Some(first) = iter.next() else {
        // Empty series: still return a meaningful (possibly extended) schema.
        let (schema, _) = build_overlay_prepared(&Schema::empty(), &input)?;
        let empty = pyo3::types::PyList::empty(py).into_any().unbind();
        return Ok((PySchema { inner: schema }, empty));
    };
    let first = first?;

    if first.cast::<PyAtom>().is_ok() {
        compute_overlays_atoms(py, series, &input)
    } else if first.cast::<PySnapshot>().is_ok() || first.cast::<pyo3::types::PyDict>().is_ok() {
        compute_overlays_snapshots(py, series, &input)
    } else {
        Err(PyTypeError::new_err(
            "`series` must be a sequence of Atom or Snapshot",
        ))
    }
}

pub(crate) fn compute_overlays_atoms<'py>(
    py: Python<'py>,
    series: &Bound<'py, PyAny>,
    input: &OverlayInput,
) -> PyResult<(PySchema, Py<PyAny>)> {
    use fugazi_core::spec::overlay as ov;
    let atoms = atoms_from_sequence(series)?;
    let existing = existing_schema_from_atoms(atoms.iter())?;
    let (out_schema, mut prepared) = build_overlay_prepared(&existing, input)?;
    let augmented = ov::compute_series(None, &atoms, &out_schema, existing.len(), &mut prepared);

    let list = pyo3::types::PyList::empty(py);
    for a in augmented {
        list.append(PyAtom { inner: a })?;
    }
    Ok((PySchema { inner: out_schema }, list.into_any().unbind()))
}

pub(crate) fn compute_overlays_snapshots<'py>(
    py: Python<'py>,
    series: &Bound<'py, PyAny>,
    input: &OverlayInput,
) -> PyResult<(PySchema, Py<PyAny>)> {
    use fugazi_core::spec::overlay as ov;
    use std::collections::HashMap;

    let snaps = snapshots_from_sequence(series)?;
    let existing =
        existing_schema_from_atoms(snaps.iter().flat_map(|s| s.iter().map(|(_, _, a)| a)))?;

    // Spec-authored columns go through the snapshot-aware engine: it builds one
    // indicator set per (symbol, freq) series rooted on that series, and drives
    // each with the *whole* snapshot — so a bare `!close` reads its own series
    // while `!pick { symbol: SPY }` reaches across to another. This is the path
    // a multi-symbol upload takes.
    if let OverlayInput::Spec(cols) = input {
        let (out_schema, augmented) = ov::compute_snapshots(&existing, cols, &snaps)
            .map_err(|e| PyValueError::new_err(format!("{e:#}")))?;
        let list = pyo3::types::PyList::empty(py);
        for snap in augmented {
            list.append(PySnapshot { inner: snap })?;
        }
        return Ok((PySchema { inner: out_schema }, list.into_any().unbind()));
    }

    // Pre-built Python carriers (the `{name: Indicator}` dict form) have no
    // spec to rebuild, so they can't be re-rooted per series — and a carrier
    // rooted on the sole-atom `Pick` would panic on a multi-symbol snapshot.
    // They keep the per-series drive: each series computed on its own size-1
    // snapshots, cross-symbol references unavailable.
    let (out_schema, template) = build_overlay_prepared(&existing, input)?;
    let existing_len = existing.len();

    // Pass 1: collect each (symbol, freq) series in first-appearance order.
    type Key = (Option<Symbol>, Option<Frequency>);
    let mut order: Vec<Key> = Vec::new();
    let mut index: HashMap<Key, usize> = HashMap::new();
    let mut series_atoms: Vec<Vec<Atom>> = Vec::new();
    for snap in &snaps {
        for (sym, freq, atom) in snap.iter() {
            let key = (sym.cloned(), freq);
            let i = *index.entry(key.clone()).or_insert_with(|| {
                order.push(key.clone());
                series_atoms.push(Vec::new());
                series_atoms.len() - 1
            });
            series_atoms[i].push(atom.clone());
        }
    }

    // Pass 2: compute each series with a fresh indicator set, reusing the one
    // out_schema so every augmented atom binds to the same `Arc`.
    let mut augmented: Vec<Vec<Atom>> = Vec::with_capacity(order.len());
    for (key, atoms) in order.iter().zip(series_atoms.iter()) {
        let mut prepared = template.clone();
        augmented.push(ov::compute_series(
            key.0.as_deref(),
            atoms,
            &out_schema,
            existing_len,
            &mut prepared,
        ));
    }

    // Pass 3: rebuild each snapshot with its atoms replaced by the augmented ones.
    let mut cursor: HashMap<Key, usize> = HashMap::new();
    let list = pyo3::types::PyList::empty(py);
    for snap in &snaps {
        let mut rebuilt = Snapshot::<Symbol>::new();
        for (sym, freq, _) in snap.iter() {
            let key = (sym.cloned(), freq);
            let i = index[&key];
            let c = cursor.entry(key.clone()).or_insert(0);
            let aug = augmented[i][*c].clone();
            *c += 1;
            rebuilt.push(key.0.clone(), key.1, aug);
        }
        list.append(PySnapshot { inner: rebuilt })?;
    }
    Ok((PySchema { inner: out_schema }, list.into_any().unbind()))
}

/// A constant string source — the string twin of `value(x)`. Feeds a
/// [`ValueStr`] leaf as a `StrSource` that ignores its input and always emits
/// `s`. Usually you don't need to build one explicitly: `StrSource.eq("foo")`
/// accepts a raw Python `str` on the right-hand side and lifts internally.
#[pyfunction]
pub(crate) fn value_str(s: &str) -> PyStrSource {
    PyStrSource::wrap(AnyStrSource::Const(Arc::from(s)))
}

/// `lhs == rhs` on two string sources. `lhs` is a `StrSource`; `rhs` may be
/// another `StrSource` or a Python `str` (lifted to a `ValueStr` constant).
/// Returns a `Signal`.
#[pyfunction]
pub(crate) fn str_eq(lhs: &PyStrSource, rhs: &Bound<'_, PyAny>) -> PyResult<PySignal> {
    let rhs = coerce_str_operand(rhs)?;
    Ok(match str_pair(lhs.src.clone(), rhs)? {
        StrPair::Atom(l, r) => PySignal::wrap(AnySignal::Atom(SignalBox::new(
            Combine::<_, _, StrEqOp>::new(l, r),
        ))),
        StrPair::Snapshot(l, r) => PySignal::wrap(AnySignal::Snapshot(SignalBox::new(
            Combine::<_, _, StrEqOp>::new(l, r),
        ))),
    })
}

/// `lhs != rhs` on two string sources. The complement of [`str_eq`].
#[pyfunction]
pub(crate) fn str_ne(lhs: &PyStrSource, rhs: &Bound<'_, PyAny>) -> PyResult<PySignal> {
    let rhs = coerce_str_operand(rhs)?;
    Ok(match str_pair(lhs.src.clone(), rhs)? {
        StrPair::Atom(l, r) => PySignal::wrap(AnySignal::Atom(SignalBox::new(
            Combine::<_, _, StrNeOp>::new(l, r),
        ))),
        StrPair::Snapshot(l, r) => PySignal::wrap(AnySignal::Snapshot(SignalBox::new(
            Combine::<_, _, StrNeOp>::new(l, r),
        ))),
    })
}



// ---------------------------------------------------------------------------
// Diagnostic: which stage of `feed()` costs what
// ---------------------------------------------------------------------------

/// Run one stage of the `feed()` pipeline and return how many samples it
/// touched. **A measurement hook, not API** — the leading underscore keeps it
/// out of `dir(fugazi)`'s useful surface, and nothing in the library calls it.
///
/// `feed()` is three stages: read the Python input into a `Vec<f64>`, fold it
/// through the (erased) indicator, and hand the result back as a NumPy array.
/// Timing the whole thing says only that it is slow; timing each prefix says
/// *which part*. Stages are cumulative:
///
/// * `0` — input conversion only
/// * `1` — input + indicator
/// * `2` — input + indicator + output (i.e. all of `feed`)
/// * `10 + k` — all of `feed`, over a **Rust-built** chain `k` levels deep
///
/// The `10 + k` stages exist to separate two costs that look identical from
/// Python: what an erased level costs, and what the *builders* put around it.
/// They assemble exactly what `sma(ema(…))` assembles, but from Rust, so a
/// difference between `10 + k` and the equivalent Python expression is the
/// binding layer and nothing else.
#[pyfunction]
pub(crate) fn _bench_feed_stage(
    py: Python<'_>,
    data: &Bound<'_, PyAny>,
    stage: usize,
    period: usize,
) -> PyResult<usize> {
    let xs = reals_from_series(data)?;
    if stage == 0 {
        return Ok(xs.len());
    }
    // `#[inline(never)]` for the same reason `benches/erasure.rs` needs it: if
    // the concrete types are visible at the `erase` call, LLVM devirtualises
    // the chain and the measurement stops describing a runtime-built one.
    #[inline(never)]
    fn chain_levels(levels: usize, period: usize) -> crate::carriers::Source<Real> {
        use fugazi_core::indicators::{Ema, Identity, Sma};
        let mut c: crate::carriers::Source<Real> = runtime::erase(Identity::<Real>::new());
        for i in 1..levels {
            c = if i % 2 == 1 {
                runtime::erase(Sma::new(c, period))
            } else {
                runtime::erase(Ema::new(c, period))
            };
        }
        c
    }
    let mut ind = if stage >= 10 {
        chain_levels(stage - 10, period)
    } else {
        chain_levels(2, period)
    };
    let values: Vec<Option<Real>> = xs
        .into_iter()
        .map(|x| fugazi_core::Indicator::update(&mut ind, x))
        .collect();
    if stage == 1 {
        return Ok(values.len());
    }
    let out = build_floats(py, &OutputKind::Numpy, values)?;
    Ok(out.bind(py).len().unwrap_or(0))
}

/// A/B the two candle-frame input paths in one process. **A measurement hook.**
///
/// Stages, cumulative where it makes sense:
/// * `0` — read the frame's columns and stop
/// * `1` — stream candles out of them ([`CandleColumns::for_each`])
/// * `2` — materialise a `Vec<Candle>` instead (the older shape)
/// * `3` — streamed, through an indicator, to a NumPy array (the whole `feed`)
/// * `4` — the same via `Vec<Candle>`
/// * `5` — streamed, but through a `Chain<Atom, Real>` with the per-bar
///   `Candle -> Atom` lift, which is what the real carriers do. 5 against 3
///   isolates the cost of `Atom` being the boundary type.
///
/// 3 against 4 is the comparison that matters, and it has to happen inside one
/// process: two wheels measured minutes apart on a shared machine cannot resolve
/// a 30% difference, which is how a regression here got mistaken for a win.
#[pyfunction]
pub(crate) fn _bench_frame_stage(
    py: Python<'_>,
    data: &Bound<'_, PyAny>,
    stage: usize,
    period: usize,
) -> PyResult<usize> {
    use fugazi_core::indicators::{Atr, Identity};
    let cols = columns_from_frame(data)?;
    if stage == 0 {
        return Ok(cols.len());
    }
    if stage == 1 {
        let mut n = 0usize;
        cols.for_each(|c| n = n.wrapping_add(c.close.to_bits() as usize));
        return Ok(n);
    }
    if stage == 2 {
        let v = frame_to_candles(data)?;
        return Ok(v.len());
    }
    // `Chain<Candle, Real>`, not `Source<Atom>`: this probe measures the input
    // path, so it feeds candles straight in rather than lifting each to an Atom.
    let mut ind: runtime::Chain<Candle, Real> =
        runtime::erase(Atr::new(Identity::<Candle>::new(), period));
    if stage == 5 {
        // What `AnySource::Atom` actually holds: an Atom-rooted chain, so each
        // 40-byte `Candle` is lifted into an 88-byte `Atom` per bar.
        let mut atom_ind: crate::carriers::Source<Atom> =
            runtime::erase(Atr::new(fugazi_core::indicators::CurrentBar::new(), period));
        let mut out = Vec::with_capacity(cols.len());
        cols.for_each(|c| out.push(fugazi_core::Indicator::update(&mut atom_ind, c.into())));
        let arr = build_floats(py, &OutputKind::Numpy, out)?;
        return Ok(arr.bind(py).len().unwrap_or(0));
    }
    let values: Vec<Option<Real>> = if stage == 3 {
        let mut out = Vec::with_capacity(cols.len());
        cols.for_each(|c| out.push(fugazi_core::Indicator::update(&mut ind, c)));
        out
    } else {
        frame_to_candles(data)?
            .into_iter()
            .map(|c| fugazi_core::Indicator::update(&mut ind, c))
            .collect()
    };
    let out = build_floats(py, &OutputKind::Numpy, values)?;
    Ok(out.bind(py).len().unwrap_or(0))
}

/// Drive a **Python-built** indicator through the same loop [`_bench_feed_stage`]
/// uses for its Rust-built one. **A measurement hook, not API.**
///
/// The pair bisects the Python gap: same input conversion, same fold, same
/// output — the only difference is who assembled the chain.
#[pyfunction]
pub(crate) fn _bench_feed_built(
    py: Python<'_>,
    ind: &Bound<'_, PyIndicator>,
    data: &Bound<'_, PyAny>,
) -> PyResult<usize> {
    let xs = reals_from_series(data)?;
    let mut ind = ind.borrow_mut();
    let AnySource::Real(chain) = &mut ind.src else {
        return Err(PyTypeError::new_err("expected a value-rooted indicator"));
    };
    let values: Vec<Option<Real>> = xs
        .into_iter()
        .map(|x| fugazi_core::Indicator::update(chain, x))
        .collect();
    let out = build_floats(py, &OutputKind::Numpy, values)?;
    Ok(out.bind(py).len().unwrap_or(0))
}
