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
use crate::constructors::*;
#[allow(unused_imports)]
use crate::sources::*;
#[allow(unused_imports)]
use crate::metrics::*;

// ---------------------------------------------------------------------------
// Spec-driven surface: YAML strategies, evaluate, optimize.
//
// The library-side `fugazi::spec` module exposes typed spec trees plus the
// pure evaluate / optimize kernel; this section wraps them into a small
// Python surface: `ta.load_spec(text)` returns a `StrategySpec` whose
// `.run()` / `.evaluate()` drive the same measurement pipeline the CLI uses,
// and `ta.optimize(text, snaps, ...)` enumerates a parameter grid and
// returns a ranked `Sweep`.
//
// Design notes:
// * One `PyStrategySpec` pyclass with an internal 5-variant enum
//   ([`CoreStrategySpec`]) covering single / pairs / basket / multi / portfolio —
//   Python's dispatch on `kind` is a plain match on the variant.
// * Costs accept a Python dict, a `PyCostConfig` instance, or `None`: the
//   dict path serialises to `serde_json::Value` and typed-parses through
//   `CostConfig`'s serde impl, so the Python surface mirrors the CLI's YAML
//   shape (nested `default:` / `by_symbol:` / `by_interval:` all work).
// * Walkforward is wired: `optimize(walkforward=(is, oos[, embargo]))`
//   returns a `WalkForwardResult` (mutually exclusive with `windowed=`).
//   The library kernel does grid-wide readiness pre-scan, per-row full-run
//   backtest, per-fold IS/OOS selection, and composite OOS stitching.
// ---------------------------------------------------------------------------

use serde_json::Value as JsonValue;

/// Convert a Python object into a `serde_json::Value`. Handles the common
/// leaf types (`None`/`bool`/`int`/`float`/`str`) and containers (`list`/
/// `tuple`/`dict`). A dict key must be a string.
pub(crate) fn py_to_json(v: &Bound<'_, PyAny>) -> PyResult<JsonValue> {
    if v.is_none() {
        return Ok(JsonValue::Null);
    }
    // Booleans are a subclass of int in Python — check them first.
    if is_python_bool(v) {
        return Ok(JsonValue::Bool(v.extract::<bool>()?));
    }
    if let Ok(x) = v.extract::<i64>() {
        return Ok(JsonValue::from(x));
    }
    if let Ok(x) = v.extract::<f64>() {
        return Ok(serde_json::Number::from_f64(x)
            .map(JsonValue::Number)
            .unwrap_or(JsonValue::Null));
    }
    if let Ok(s) = v.extract::<String>() {
        return Ok(JsonValue::String(s));
    }
    if let Ok(d) = v.cast::<pyo3::types::PyDict>() {
        let mut m = serde_json::Map::new();
        for (k, val) in d.iter() {
            let key = k.extract::<String>().map_err(|_| {
                PyTypeError::new_err("dict keys must be strings when converting to JSON")
            })?;
            m.insert(key, py_to_json(&val)?);
        }
        return Ok(JsonValue::Object(m));
    }
    if let Ok(l) = v.cast::<pyo3::types::PyList>() {
        let mut arr = Vec::with_capacity(l.len());
        for item in l.iter() {
            arr.push(py_to_json(&item)?);
        }
        return Ok(JsonValue::Array(arr));
    }
    if let Ok(t) = v.cast::<pyo3::types::PyTuple>() {
        let mut arr = Vec::with_capacity(t.len());
        for item in t.iter() {
            arr.push(py_to_json(&item)?);
        }
        return Ok(JsonValue::Array(arr));
    }
    // Fallback: try to iterate.
    if let Ok(iter) = v.try_iter() {
        let mut arr = Vec::new();
        for item in iter {
            arr.push(py_to_json(&item?)?);
        }
        return Ok(JsonValue::Array(arr));
    }
    Err(PyTypeError::new_err(format!(
        "cannot convert Python value to JSON: {}",
        v.get_type().name()?
    )))
}

/// Convert a `serde_json::Value` into a Python object.
pub(crate) fn json_to_py(py: Python<'_>, v: &JsonValue) -> PyResult<Py<PyAny>> {
    match v {
        JsonValue::Null => Ok(py.None()),
        JsonValue::Bool(b) => Ok(b.into_pyobject(py)?.to_owned().into_any().unbind()),
        JsonValue::Number(n) => {
            if let Some(i) = n.as_i64() {
                Ok(i.into_pyobject(py)?.to_owned().into_any().unbind())
            } else if let Some(u) = n.as_u64() {
                Ok(u.into_pyobject(py)?.to_owned().into_any().unbind())
            } else if let Some(f) = n.as_f64() {
                Ok(f.into_pyobject(py)?.to_owned().into_any().unbind())
            } else {
                Ok(py.None())
            }
        }
        JsonValue::String(s) => Ok(s.into_pyobject(py)?.to_owned().into_any().unbind()),
        JsonValue::Array(arr) => {
            let items: PyResult<Vec<Py<PyAny>>> =
                arr.iter().map(|v| json_to_py(py, v)).collect();
            let list = pyo3::types::PyList::new(py, items?)?;
            Ok(list.into_any().unbind())
        }
        JsonValue::Object(obj) => {
            let d = pyo3::types::PyDict::new(py);
            for (k, val) in obj {
                d.set_item(k, json_to_py(py, val)?)?;
            }
            Ok(d.into_any().unbind())
        }
    }
}

/// Extract a `HashMap<String, serde_json::Value>` from an optional Python
/// dict, for the `!param` substitution table. `None` is treated as an empty
/// table.
pub(crate) fn extract_params(
    obj: Option<&Bound<'_, PyAny>>,
) -> PyResult<std::collections::HashMap<String, JsonValue>> {
    let Some(o) = obj else {
        return Ok(std::collections::HashMap::new());
    };
    if o.is_none() {
        return Ok(std::collections::HashMap::new());
    }
    let dict = o.cast::<pyo3::types::PyDict>().map_err(|_| {
        PyTypeError::new_err("`params` must be a dict[str, Any] (or None)")
    })?;
    let mut out = std::collections::HashMap::new();
    for (k, v) in dict.iter() {
        let key = k
            .extract::<String>()
            .map_err(|_| PyTypeError::new_err("`params` keys must be strings"))?;
        out.insert(key, py_to_json(&v)?);
    }
    Ok(out)
}

/// A wrapper around the library's `CostConfig` that Python callers use to
/// build cost bundles from a dict once and reuse across many runs / grid
/// points. Stores the resolved JSON tree internally so it can rebuild the
/// (non-Cloneable) `CostConfig` per use — the cost of a serde round-trip
/// is negligible next to any run.
#[pyclass(name = "TradingCostsConfig", module = "fugazi")]
pub(crate) struct PyCostConfig {
    pub(crate) tree: JsonValue,
}

impl PyCostConfig {
    pub(crate) fn build(&self) -> PyResult<CostConfig> {
        serde_json::from_value(self.tree.clone())
            .map_err(|e| PyValueError::new_err(format!("invalid TradingCostsConfig: {e}")))
    }

    pub(crate) fn build_view(tree: &JsonValue) -> PyResult<CostConfig> {
        serde_json::from_value(tree.clone())
            .map_err(|e| PyValueError::new_err(format!("invalid TradingCostsConfig: {e}")))
    }
}

/// Monte Carlo significance configuration for
/// `StrategySpec.evaluate(montecarlo=...)`. Mirrors the CLI's `--mc-*` flags:
/// bootstrap confidence intervals plus empirical-null p-values, over a chosen
/// resampling scheme. `scheme` is one of `iid` / `moving-block` / `stationary`
/// (default), `block` its (expected) block length, `null` one of
/// `none` / `rerun` (default; which empirical null to test), and
/// `metrics` an optional list of metric names (default: a headline set).
#[pyclass(name = "MonteCarloConfig", module = "fugazi", from_py_object)]
#[derive(Clone)]
pub(crate) struct PyMonteCarloConfig {
    pub(crate) inner: McConfig,
}

#[pymethods]
impl PyMonteCarloConfig {
    #[new]
    #[pyo3(signature = (
        permutations = 1000,
        scheme = "stationary",
        block = 10.0,
        seed = 0,
        ci_level = 0.95,
        null = "rerun",
        metrics = None,
    ))]
    fn new(
        permutations: usize,
        scheme: &str,
        block: f64,
        seed: u64,
        ci_level: f64,
        null: &str,
        metrics: Option<Vec<String>>,
    ) -> PyResult<Self> {
        let scheme = match scheme {
            "iid" => ResampleScheme::Iid,
            "moving-block" | "moving_block" => ResampleScheme::MovingBlock {
                block: block.max(1.0) as usize,
            },
            "stationary" => ResampleScheme::Stationary { mean_block: block },
            other => {
                return Err(PyValueError::new_err(format!(
                    "unknown scheme `{other}` (expected iid | moving-block | stationary)"
                )));
            }
        };
        let rerun_null = match null {
            "none" => false,
            "rerun" => true,
            other => {
                return Err(PyValueError::new_err(format!(
                    "unknown null `{other}` (expected none | rerun)"
                )));
            }
        };
        Ok(Self {
            inner: McConfig {
                permutations,
                scheme,
                seed,
                ci_level,
                rerun_null,
                metrics: metrics.unwrap_or_default(),
            },
        })
    }

    fn __repr__(&self) -> String {
        format!(
            "MonteCarloConfig(permutations={}, scheme={}, seed={}, ci_level={})",
            self.inner.permutations,
            self.inner.scheme.label(),
            self.inner.seed,
            self.inner.ci_level
        )
    }
}

#[pymethods]
impl PyCostConfig {
    /// Build a config from a Python dict mirroring the CLI's YAML shape:
    /// `{"commission": {"percentage": {"rate": 0.001}}, ...}` for the
    /// simple flat form or `{"commission": {"default": {...}, "by_symbol":
    /// {"BTC": {...}}}}` for scoped overrides. Passing no argument or `{}`
    /// yields a zero-cost bundle.
    #[new]
    #[pyo3(signature = (mapping = None))]
    pub(crate) fn new(mapping: Option<&Bound<'_, PyAny>>) -> PyResult<Self> {
        let value = match mapping {
            None => JsonValue::Object(serde_json::Map::new()),
            Some(o) if o.is_none() => JsonValue::Object(serde_json::Map::new()),
            Some(o) => py_to_json(o)?,
        };
        let tree = normalize_cost_dict(value)?;
        // Validate now so the caller sees the error at construction rather than
        // at .run().
        let _cfg = PyCostConfig::build_view(&tree)?;
        Ok(PyCostConfig { tree })
    }

    pub(crate) fn __repr__(&self) -> String {
        match PyCostConfig::build_view(&self.tree) {
            Ok(inner) if inner.is_none() => "TradingCostsConfig(<zero-cost>)".to_string(),
            Ok(inner) => format!(
                "TradingCostsConfig(scoped={}, defaults={})",
                inner.scoped_count(),
                inner.has_any_default(),
            ),
            Err(_) => "TradingCostsConfig(<invalid>)".to_string(),
        }
    }
}

/// Turn a user-facing Python cost dict into the canonical structured shape
/// `CostConfig`'s serde impl expects: hoist a flat model into `default:`.
/// Non-cost keys pass through so a caller can also hand us an
/// already-structured dict.
pub(crate) fn normalize_cost_dict(value: JsonValue) -> PyResult<JsonValue> {
    let JsonValue::Object(map) = value else {
        return Err(PyValueError::new_err(
            "TradingCostsConfig expects a dict mapping (e.g. {'commission': {'percentage': ...}})",
        ));
    };
    let mut out = serde_json::Map::new();
    for (leg, node) in map {
        if !matches!(leg.as_str(), "commission" | "spread" | "slippage") {
            return Err(PyValueError::new_err(format!(
                "TradingCostsConfig: unknown leg `{leg}` (expected commission/spread/slippage)"
            )));
        }
        let JsonValue::Object(obj) = node else {
            out.insert(leg, node);
            continue;
        };
        let structured = obj.contains_key("default")
            || obj.contains_key("by_symbol")
            || obj.contains_key("by_interval")
            || obj.contains_key("scoped");
        // A flat singleton like {percentage: {rate: 0.001}} — hoist to default.
        let is_model_shape = obj.len() == 1
            && obj.keys().next().is_some_and(|k| {
                matches!(
                    k.as_str(),
                    "none"
                        | "fixed"
                        | "percentage"
                        | "per_unit"
                        | "composite"
                        | "max"
                        | "bps"
                        | "absolute"
                        | "volume_participation"
                )
            });
        if is_model_shape && !structured {
            let mut wrapper = serde_json::Map::new();
            wrapper.insert("default".to_string(), JsonValue::Object(obj));
            out.insert(leg, JsonValue::Object(wrapper));
        } else {
            out.insert(leg, JsonValue::Object(obj));
        }
    }
    Ok(JsonValue::Object(out))
}

/// Coerce the `costs=` argument on `.run()` / `.evaluate()` / `ta.optimize()`
/// into an owned `CostConfig`. Accepts `None`, a `PyCostConfig`, or a raw
/// Python dict.
pub(crate) fn coerce_cost_config(obj: Option<&Bound<'_, PyAny>>) -> PyResult<CostConfig> {
    let Some(o) = obj else {
        return Ok(default_cost_config());
    };
    if o.is_none() {
        return Ok(default_cost_config());
    }
    if let Ok(cc) = o.cast::<PyCostConfig>() {
        return cc.borrow().build();
    }
    // Dict path.
    let value = py_to_json(o)?;
    let normalized = normalize_cost_dict(value)?;
    serde_json::from_value(normalized)
        .map_err(|e| PyValueError::new_err(format!("invalid `costs=` dict: {e}")))
}

/// A zero-cost `CostConfig` — parsed from `{}` (deserialization sets every
/// leg to its default, i.e. the no-op model).
pub(crate) fn default_cost_config() -> CostConfig {
    serde_json::from_value(JsonValue::Object(serde_json::Map::new()))
        .expect("empty CostConfig deserialization is infallible")
}

/// Detect the strategy kind from a resolved (post `!import`/`!param`) YAML
/// value. Mirrors the CLI's shape-based routing rules.
pub(crate) fn detect_kind(v: &JsonValue) -> &'static str {
    // Presets like !buy_and_hold arrive as either a top-level tagged value
    // (serde_norway path) or a single-key mapping (JSON path). In both cases
    // the tag key is one of the preset names — route them as `single`.
    const PRESET_TAGS: &[&str] = &[
        "buy_and_hold",
        "ma_crossover",
        "rsi_reversal",
        "donchian_breakout",
        "keltner_breakout",
    ];
    if let JsonValue::Object(m) = v
        && m.len() == 1
        && let Some(k) = m.keys().next()
        && PRESET_TAGS.contains(&k.as_str())
    {
        return "single";
    }
    let Some(map) = v.as_object() else {
        return "single"; // fallback; typed parse will surface the error
    };
    if map.contains_key("children") {
        return "portfolio";
    }
    if map.contains_key("left") && map.contains_key("right") {
        return "pairs";
    }
    if map.contains_key("selection") {
        return "basket";
    }
    if map.contains_key("symbol") {
        return "single";
    }
    // Bare per-side factories or a lone `long:` / `short:` mapping — that's
    // a multi-asset shape.
    "multi"
}

/// Load a strategy YAML doc from text, auto-detecting kind (or using the
/// caller's `kind` override). Returns the typed `CoreStrategySpec`.
pub(crate) fn load_loaded_spec(
    text: &str,
    params: &std::collections::HashMap<String, JsonValue>,
    base_dir: &std::path::Path,
    kind: &str,
) -> PyResult<CoreStrategySpec> {
    let value = fugazi_core::spec::load_value(text, params, base_dir, "(inline)")
        .map_err(|e| PyValueError::new_err(format!("loading strategy: {e:#}")))?;
    let kind = if kind == "auto" { detect_kind(&value) } else { kind };
    macro_rules! parse {
        ($variant:ident, $ty:ty, $label:literal) => {{
            let s: $ty = serde_json::from_value(value)
                .map_err(|e| PyValueError::new_err(format!("parsing {} strategy: {e}", $label)))?;
            Ok(CoreStrategySpec::$variant(Box::new(s)))
        }};
    }
    match kind {
        "single" => parse!(Single, StrategyRef, "single"),
        "pairs" => parse!(Pairs, PairsStrategySpec, "pairs"),
        "basket" => parse!(Basket, BasketStrategySpec, "basket"),
        "multi" => parse!(Multi, MultiAssetStrategySpec, "multi"),
        "portfolio" => parse!(Portfolio, PortfolioSpec, "portfolio"),
        other => Err(PyValueError::new_err(format!(
            "unknown strategy kind `{other}` (expected auto/single/pairs/basket/multi/portfolio)"
        ))),
    }
}

/// The pyclass surface for a loaded strategy spec: one for every YAML shape,
/// dispatched off the inner enum's variant.
#[pyclass(name = "StrategySpec", module = "fugazi")]
pub(crate) struct PyStrategySpec {
    pub(crate) inner: CoreStrategySpec,
}

/// Drive one already-loaded spec through the user-supplied `PaperWallet` and
/// return the run report. Same shape as [`PyStrategy::run`] and its siblings —
/// the wallet's `equity()` seeds the strategy, and any costs the caller
/// pre-installed via `wallet.set_costs_for(sym, ...)` apply naturally.
///
/// The one non-conformant variant is `Portfolio`: `Portfolio::trade` ignores
/// the external wallet by design (a composite needs one sub-wallet per child
/// and the `Strategy` trait offers one, so it drives its own composite
/// `PortfolioWallet` internally — see `fugazi::portfolio` in the Rust
/// library). The external wallet's `equity()` is still used as the cash seed,
/// but its installed costs are *not* propagated to the portfolio's
/// sub-wallets; the caller must install portfolio-wide costs via the spec's
/// own facilities (currently `None` — a documented follow-up).
///
/// This is safe here because the portfolio arm below drives through the
/// portfolio's own wallet rather than the one passed in; handing a portfolio
/// some other wallet is caught at runtime on the Rust side.
pub(crate) fn run_spec(
    loaded: &CoreStrategySpec,
    snapshots: &[Snapshot<String>],
    wallet: &mut PaperWallet<String>,
) -> PyResult<RunReport<String>> {
    let cash = <PaperWallet<String> as Wallet<String>>::equity(wallet).0;
    let schema = spec_backtest::schema_from_snapshots(snapshots);
    let mut built = loaded
        .try_build(cash, &schema, None)
        .map_err(build_err)?;
    // Portfolio drives its own composite wallet and ignores the one passed
    // here (see `RunnableStrategy::drive`); the other shapes trade into the
    // caller's, keeping any costs it was primed with.
    if matches!(loaded, CoreStrategySpec::Portfolio(_)) {
        return Ok(built.drive(snapshots, cash, &[]));
    }
    // `&mut *built` rather than `&mut built`: `run` takes `S: Strategy + ?Sized`,
    // and it is `dyn RunnableStrategy` that carries the `Strategy` supertrait,
    // not the `Box` around it.
    Ok(fugazi_core::backtest::run(
        &mut *built,
        wallet,
        snapshots.iter().cloned(),
    ))
}

/// The resumable superset of [`run_spec`]: optionally restore `resume` state
/// before the run, optionally finalize open positions with `flatten`, and
/// return the run's final [`RunState`](fugazi_core::spec::RunState) alongside the
/// report so Python can persist it and resume later.
pub(crate) fn run_spec_resumable(
    loaded: &CoreStrategySpec,
    snapshots: &[Snapshot<String>],
    wallet: &mut PaperWallet<String>,
    resume: Option<&fugazi_core::spec::RunState>,
    flatten: bool,
) -> PyResult<(RunReport<String>, fugazi_core::spec::RunState)> {
    use fugazi_core::spec::{RUN_STATE_FORMAT_VERSION, RunState};
    let cash = <PaperWallet<String> as Wallet<String>>::equity(wallet).0;
    let schema = spec_backtest::schema_from_snapshots(snapshots);
    let mut built = loaded.try_build(cash, &schema, None).map_err(build_err)?;

    // Portfolio owns its composite wallet, so delegate wholesale to
    // `drive_resumable` (which restores/saves internally and finalizes if asked).
    if matches!(loaded, CoreStrategySpec::Portfolio(_)) {
        return built
            .drive_resumable(snapshots, cash, &[], resume, flatten)
            .map_err(build_err);
    }

    if let Some(rs) = resume {
        if rs.format_version != RUN_STATE_FORMAT_VERSION {
            return Err(PyValueError::new_err(format!(
                "resume: state format version {} does not match this build's {}",
                rs.format_version, RUN_STATE_FORMAT_VERSION
            )));
        }
        if rs.kind != loaded.kind() {
            return Err(PyValueError::new_err(format!(
                "resume: state is for a `{}` strategy but this document is `{}`",
                rs.kind,
                loaded.kind()
            )));
        }
        built.restore_state(&rs.strategy).map_err(build_err)?;
        wallet.restore_state(&rs.wallet).map_err(build_err)?;
    }

    let mut report = fugazi_core::backtest::run(&mut *built, wallet, snapshots.iter().cloned());
    if flatten {
        fugazi_core::backtest::flatten_open_positions(&mut *built, wallet, snapshots, &mut report);
    }
    let last_bar = snapshots
        .last()
        .and_then(|s| s.iter().find_map(|(_, _, a)| a.time))
        .map(|t| t.0);
    let final_state = RunState {
        format_version: RUN_STATE_FORMAT_VERSION,
        kind: loaded.kind().to_string(),
        last_bar,
        bars_seen: resume.map(|r| r.bars_seen).unwrap_or(0) + snapshots.len(),
        strategy: fugazi_core::spec::RunnableStrategy::save_state(&*built),
        wallet: wallet.snapshot_state(),
    };
    Ok((report, final_state))
}

/// Typed-parse an already-`!param`-substituted document as `kind`.
///
/// The optimize kernel substitutes a fresh params table per grid row, so the
/// typed parse has to happen per row; this is the shape-routing half of
/// [`load_loaded_spec`] without the load passes.
pub(crate) fn spec_from_value(value: JsonValue, kind: &str) -> anyhow::Result<CoreStrategySpec> {
    macro_rules! parse {
        ($variant:ident, $ty:ty) => {
            CoreStrategySpec::$variant(Box::new(serde_json::from_value::<$ty>(value)?))
        };
    }
    Ok(match kind {
        "single" => parse!(Single, StrategyRef),
        "pairs" => parse!(Pairs, PairsStrategySpec),
        "basket" => parse!(Basket, BasketStrategySpec),
        "multi" => parse!(Multi, MultiAssetStrategySpec),
        "portfolio" => parse!(Portfolio, PortfolioSpec),
        other => anyhow::bail!("unknown strategy kind `{other}`"),
    })
}

/// Map a spec-build failure to a Python `ValueError`, splitting the crate's
/// `!tag > ` breadcrumb onto its own line so the message reads the way the
/// CLI renders it.
pub(crate) fn build_err(e: String) -> PyErr {
    let (trail, message) = fugazi_core::spec::diagnostics::split_trail(&e);
    if trail.is_empty() {
        PyValueError::new_err(message.to_string())
    } else {
        PyValueError::new_err(format!("{message}\n  at: {}", trail.join(" > ")))
    }
}

/// Serialize a `SpecMetrics` document into a Python dict via serde_json.
pub(crate) fn metrics_to_py(py: Python<'_>, m: &SpecMetrics) -> PyResult<Py<PyAny>> {
    let value = serde_json::to_value(m)
        .map_err(|e| PyValueError::new_err(format!("serializing metrics: {e}")))?;
    json_to_py(py, &value)
}

/// The raw per-resample values behind a Monte Carlo summary — the same shape
/// the CLI writes to `montecarlo.csv`, one entry per estimator (`bootstrap_ci`,
/// `null_rerun`).
fn mc_samples_to_json(samples: &fugazi_core::spec::montecarlo::McSamples) -> JsonValue {
    serde_json::json!({
        "metric_names": samples.metric_names,
        "sets": samples.sets.iter().map(|s| serde_json::json!({
            "estimator": s.estimator,
            "rows": s.rows,
        })).collect::<Vec<_>>(),
    })
}

#[pymethods]
impl PyStrategySpec {
    /// The strategy kind, one of `single`, `pairs`, `basket`, `multi`,
    /// `portfolio`.
    #[getter]
    pub(crate) fn kind(&self) -> &'static str {
        self.inner.kind()
    }

    /// Drive the spec over `snapshots` against `wallet`, returning the full
    /// run report. Matches the [`PyStrategy::run`] shape: the wallet's
    /// `equity()` seeds the strategy, and any costs the caller pre-installed
    /// via `wallet.set_costs_for(sym, ...)` apply naturally (except for
    /// portfolio, whose composite wallet is owned internally — see
    /// [`run_spec`]).
    pub(crate) fn run(
        &self,
        mut wallet: PyRefMut<'_, PyWallet>,
        snapshots: &Bound<'_, PyAny>,
    ) -> PyResult<PyRunReport> {
        let snaps = snapshots_from_sequence(snapshots)?;
        let report = run_spec(&self.inner, &snaps, &mut wallet.inner)?;
        Ok(PyRunReport { inner: report })
    }

    /// Drive the spec with **run resuming**: optionally restore `resume` (a JSON
    /// string previously returned here) before the run, optionally finalize open
    /// positions with `flatten`, and return `(report, state_json)` — the
    /// run report plus the final state to persist and resume from later.
    ///
    /// `resume` and `flatten=True` are mutually exclusive in spirit (a
    /// flattened run is finalized); passing a flattened run's state to a later
    /// `resume` simply continues from a flat book. PaperWallet only, like
    /// [`Self::run`].
    #[pyo3(signature = (wallet, snapshots, resume = None, flatten = false))]
    pub(crate) fn run_resumable(
        &self,
        mut wallet: PyRefMut<'_, PyWallet>,
        snapshots: &Bound<'_, PyAny>,
        resume: Option<String>,
        flatten: bool,
    ) -> PyResult<(PyRunReport, String)> {
        let snaps = snapshots_from_sequence(snapshots)?;
        let resume_state = match resume {
            Some(text) => Some(
                serde_json::from_str::<fugazi_core::spec::RunState>(&text)
                    .map_err(|e| PyValueError::new_err(format!("parsing resume state: {e}")))?,
            ),
            None => None,
        };
        let (report, state) = run_spec_resumable(
            &self.inner,
            &snaps,
            &mut wallet.inner,
            resume_state.as_ref(),
            flatten,
        )?;
        let state_json = serde_json::to_string(&state)
            .map_err(|e| PyValueError::new_err(format!("serializing run state: {e}")))?;
        Ok((PyRunReport { inner: report }, state_json))
    }

    /// Drive the spec over `snapshots` against `wallet`, reduce the run
    /// report to a metrics document, and return it as a nested dict (mirroring
    /// `metrics.yml`). Convenience over calling `.run(...)` then feeding the
    /// report to `fugazi.metrics.*` — same wallet-first shape as [`Self::run`].
    ///
    /// Passing `windowed=N` additionally slices the run into `N`-bar spans —
    /// exactly `run -w N`'s `metrics.csv`/`rolling.csv` — and embeds them as
    /// `windowed`/`rolling` list-of-dict keys in the returned dict, each entry
    /// `{"start_bar", "end_bar", "metrics": {...}}`. `windowed` is
    /// non-overlapping (independent spans, for cross-window statistics);
    /// `rolling` advances one bar at a time (heavily autocorrelated, for a
    /// continuous rolling-Sharpe-style curve). Unlike the CLI's `-w`, this
    /// takes a plain bar count — no duration/asset-class resolution.
    ///
    /// Passing `montecarlo=MonteCarloConfig(...)` additionally runs the Monte
    /// Carlo significance pass and embeds its result under a `montecarlo` key
    /// in the returned dict (bootstrap CIs + empirical-null p-values, exactly
    /// as `metrics.yml`'s `montecarlo:` block), plus the raw per-resample
    /// values under `montecarlo["samples"]` — the same data the CLI writes to
    /// `montecarlo.csv` (`{"metric_names": [...], "sets": [{"estimator":
    /// "bootstrap_ci"|"null_rerun", "rows": [[...], ...]}, ...]}`), for
    /// plotting the sampling/null distributions directly in Python. The
    /// synthetic re-run-null paths are driven frictionlessly (the wallet's
    /// costs stay on the observed run and its CIs, not on the resampled
    /// re-drives).
    #[allow(clippy::too_many_arguments)]
    #[pyo3(signature = (
        wallet,
        snapshots,
        bars_per_year = 252.0,
        risk_free_rate = 0.0,
        seconds_per_bar = None,
        windowed = None,
        montecarlo = None,
    ))]
    pub(crate) fn evaluate(
        &self,
        py: Python<'_>,
        mut wallet: PyRefMut<'_, PyWallet>,
        snapshots: &Bound<'_, PyAny>,
        bars_per_year: Real,
        risk_free_rate: Real,
        seconds_per_bar: Option<Real>,
        windowed: Option<usize>,
        montecarlo: Option<PyMonteCarloConfig>,
    ) -> PyResult<Py<PyAny>> {
        let snaps = snapshots_from_sequence(snapshots)?;
        // Drive once for the observed report, reduce to metrics.
        let report = run_spec(&self.inner, &snaps, &mut wallet.inner)?;
        let mut metrics =
            spec_metrics::from_report(&report, bars_per_year, risk_free_rate, seconds_per_bar);
        if let Some(0) = windowed {
            return Err(PyValueError::new_err("`windowed` must be positive"));
        }
        let mut samples = None;
        if let Some(mc) = montecarlo {
            // The Python surface installs costs on the wallet, not via a
            // `CostConfig`, so the re-run null's synthetic re-drives are
            // frictionless; the observed run (and its CIs) still reflects them.
            let empty_costs: CostConfig = serde_json::from_str("{}")
                .map_err(|e| PyValueError::new_err(format!("cost config: {e}")))?;
            let ctx = spec_backtest::EvalContext {
                cash: report.initial_equity,
                bars_per_year,
                risk_free_rate,
                cost_config: &empty_costs,
                effective_freq: None,
                windowed: None,
                seconds_per_bar,
                mc: None,
            };
            let outcome = py
                .detach(|| run_montecarlo(&self.inner, &snaps, &ctx, &report, &mc.inner))
                .map_err(build_err)?;
            metrics.montecarlo = Some(outcome.section);
            samples = Some(outcome.samples);
        }
        let mut value = serde_json::to_value(&metrics)
            .map_err(|e| PyValueError::new_err(format!("serializing metrics: {e}")))?;
        if let Some(samples) = samples
            && let Some(obj) = value.get_mut("montecarlo").and_then(JsonValue::as_object_mut)
        {
            obj.insert("samples".to_string(), mc_samples_to_json(&samples));
        }
        if let Some(w) = windowed {
            let win_rows =
                spec_metrics::windowed_from_report(&report, w, bars_per_year, risk_free_rate, seconds_per_bar);
            let roll_rows =
                spec_metrics::rolling_from_report(&report, w, bars_per_year, risk_free_rate, seconds_per_bar);
            let obj = value.as_object_mut().expect("metrics document serializes to an object");
            obj.insert(
                "windowed".to_string(),
                serde_json::to_value(&win_rows)
                    .map_err(|e| PyValueError::new_err(format!("serializing windowed metrics: {e}")))?,
            );
            obj.insert(
                "rolling".to_string(),
                serde_json::to_value(&roll_rows)
                    .map_err(|e| PyValueError::new_err(format!("serializing rolling metrics: {e}")))?,
            );
        }
        json_to_py(py, &value)
    }

    pub(crate) fn __repr__(&self) -> String {
        format!("StrategySpec(kind='{}')", self.inner.kind())
    }
}

/// The complete, machine-readable **grammar descriptor** — one JSON record per
/// YAML tag, reflected straight off serde's variant definitions. The single
/// authority for the spec's presentation metadata: names, groups, kinds,
/// shapes, fields (with types, required-ness, defaults, and prose), outputs,
/// and `since`.
///
/// Returns `{ "schema_version": <int>, "tags": [ {tag}, ... ] }`. Each tag:
///
/// ```text
/// name         variant name, no leading `!` (a stable public contract)
/// group        "node" | "selection"
/// kind         "source" | "indicator" | "operator" | "predicate" | "function" | "selection"
/// shape        "unit" | "newtype" | "seq" | "map"  (how it's written in YAML)
/// fields       [ {name, type, required, default, doc} ]  (map tags only)
/// output       what it evaluates to: "scalar" | "bool" | "str" | ...
/// projections  struct-output accessors (empty for fugazi's flattened tags)
/// payload      positional payload type of a newtype/seq tag ("node" |
///              "literal" | "node_list"); null for unit/map tags
/// doc          the variant's `///`
/// since        release it first shipped in
/// ```
///
/// Same anti-drift guarantee as [`spec_tags`], one level deeper: everything
/// flows from the serde definitions via `#[derive(SpecGrammar)]`, so downstream
/// consumers (these very Python constructors, editor tooling, docs, external
/// grammar tables) generate from one artifact rather than re-encoding by hand.
/// Guard on `schema_version` for shape changes (`payload` and its `"literal"`
/// field type were added in schema_version 2).
#[pyfunction]
pub(crate) fn spec_grammar(py: Python<'_>) -> PyResult<Py<PyAny>> {
    let doc = fugazi_core::spec::grammar::spec_grammar_document();
    json_to_py(py, &doc)
}

/// A JSON Schema (draft 2020-12) for the spec's expression grammar, derived from
/// the same serde definitions as [`spec_grammar`] — the one artifact `load_spec`
/// validation, an editor form, and external tooling can all key off.
///
/// Validates the **JSON bridge form** of an expression: the single-key
/// `{ "<tag>": { <fields> } }` objects the dict path accepts, plus the
/// bare-literal shorthands (`70`, `"close"`). The root `$ref`s `#/$defs/node`;
/// `#/$defs/selection` covers the `basket:` `selection:` vocabulary. It checks
/// *structure* — the Real/Bool/Str type discipline stays in the build-time type
/// checker. (Phase 1: the expression grammar; the whole-document envelope is a
/// planned follow-up.)
#[pyfunction]
pub(crate) fn spec_json_schema(py: Python<'_>) -> PyResult<Py<PyAny>> {
    let schema = fugazi_core::spec::grammar::spec_json_schema();
    json_to_py(py, &schema)
}

/// A JSON Schema (draft 2020-12) for a whole **spec document** — the five
/// strategy shapes (single / pairs / basket / multi / portfolio) and their
/// slots — `$ref`-ing the same expression grammar as [`spec_json_schema`] for
/// every signal / level / score / sizing / weight slot.
///
/// The root is a `oneOf` over the five shapes (disjoint by their required keys).
/// Same caveats: validates the JSON bridge form, checks *structure*, and
/// complements `fugazi check` rather than replacing it. Nested portfolio-child
/// strategies are validated only as non-empty mappings.
#[pyfunction]
pub(crate) fn spec_document_json_schema(py: Python<'_>) -> PyResult<Py<PyAny>> {
    let schema = fugazi_core::spec::grammar::spec_document_json_schema();
    json_to_py(py, &schema)
}

/// Every tag the YAML spec layer accepts, grouped by the vocabulary it belongs
/// to: `"node"` (the one composable expression enum — numeric sources, boolean
/// predicates, and string comparisons together) and `"selection"` (a `basket:`
/// document's `selection:` rules). Names come back without the leading `!`.
///
/// A thin projection of [`spec_grammar`] — the names of each group — kept as a
/// convenience for discovery (`"sma" in ta.spec_tags()["node"]`) and the parity
/// test. Reach for [`spec_grammar`] when you need fields, defaults, or prose.
#[pyfunction]
pub(crate) fn spec_tags(py: Python<'_>) -> PyResult<Py<PyAny>> {
    let grammar = fugazi_core::spec::grammar::spec_grammar();
    let out = pyo3::types::PyDict::new(py);
    for group in ["node", "selection"] {
        let names: Vec<&str> = grammar
            .iter()
            .filter(|t| t.group == group)
            .map(|t| t.name.as_str())
            .collect();
        out.set_item(group, names)?;
    }
    Ok(out.into_any().unbind())
}

/// Load a strategy YAML doc from text into a `StrategySpec`.
///
/// `params` is a dict of `!param` substitutions; `base_dir` is the directory
/// `!import` paths resolve against. Auto-detects the strategy kind unless
/// `kind` is one of `single`/`pairs`/`basket`/`multi`/`portfolio`.
#[pyfunction]
#[pyo3(signature = (text, params = None, base_dir = None, kind = "auto"))]
pub(crate) fn load_spec(
    text: &str,
    params: Option<&Bound<'_, PyAny>>,
    base_dir: Option<&str>,
    kind: &str,
) -> PyResult<PyStrategySpec> {
    let params = extract_params(params)?;
    let base = std::path::PathBuf::from(base_dir.unwrap_or("."));
    let inner = load_loaded_spec(text, &params, &base, kind)?;
    Ok(PyStrategySpec { inner })
}

// ---------------------------------------------------------------------------
// Optimize
// ---------------------------------------------------------------------------

#[pyclass(name = "SweepRow", module = "fugazi")]
pub(crate) struct PySweepRow {
    // Axis-name → value (None for sparse cells).
    pub(crate) axis_columns: Vec<String>,
    pub(crate) axis_values: Vec<Option<JsonValue>>,
    // Metric-name (user-facing) → resolved value.
    pub(crate) metric_columns: Vec<(String, String)>,
    pub(crate) metric_values: Vec<Option<Real>>,
    // If windowed, one Metrics dict per window.
    pub(crate) windowed_metrics: Option<Vec<SpecMetrics>>,
}

#[pymethods]
impl PySweepRow {
    #[getter]
    pub(crate) fn values(&self, py: Python<'_>) -> PyResult<Py<pyo3::types::PyDict>> {
        let d = pyo3::types::PyDict::new(py);
        for (name, v) in self.axis_columns.iter().zip(&self.axis_values) {
            match v {
                Some(val) => d.set_item(name, json_to_py(py, val)?)?,
                None => d.set_item(name, py.None())?,
            }
        }
        Ok(d.into())
    }

    #[getter]
    pub(crate) fn metrics(&self, py: Python<'_>) -> PyResult<Py<pyo3::types::PyDict>> {
        let d = pyo3::types::PyDict::new(py);
        for ((user, _resolved), v) in self.metric_columns.iter().zip(&self.metric_values) {
            match v {
                Some(x) => d.set_item(user, x)?,
                None => d.set_item(user, py.None())?,
            }
        }
        Ok(d.into())
    }

    #[getter]
    pub(crate) fn metrics_windowed(&self, py: Python<'_>) -> PyResult<Py<PyAny>> {
        match &self.windowed_metrics {
            None => Ok(py.None()),
            Some(v) => {
                let list = pyo3::types::PyList::empty(py);
                for m in v {
                    list.append(metrics_to_py(py, m)?)?;
                }
                Ok(list.into_any().unbind())
            }
        }
    }

    pub(crate) fn __repr__(&self) -> String {
        format!(
            "SweepRow(axes={}, metrics={})",
            self.axis_columns.len(),
            self.metric_columns.len(),
        )
    }
}

#[pyclass(name = "Sweep", module = "fugazi")]
pub(crate) struct PySweep {
    pub(crate) columns: Vec<String>,
    pub(crate) metric_columns: Vec<(String, String)>,
    pub(crate) rows: Vec<Py<PySweepRow>>,
    pub(crate) best_idx: Option<usize>,
}

#[pymethods]
impl PySweep {
    #[getter]
    pub(crate) fn columns(&self) -> Vec<String> {
        self.columns.clone()
    }

    #[getter]
    pub(crate) fn metric_columns(&self) -> Vec<(String, String)> {
        self.metric_columns.clone()
    }

    #[getter]
    pub(crate) fn rows(&self, py: Python<'_>) -> Vec<Py<PySweepRow>> {
        self.rows.iter().map(|r| r.clone_ref(py)).collect()
    }

    #[getter]
    pub(crate) fn best(&self, py: Python<'_>) -> Option<Py<PySweepRow>> {
        self.best_idx.map(|i| self.rows[i].clone_ref(py))
    }

    pub(crate) fn __repr__(&self) -> String {
        format!("Sweep(rows={}, columns={})", self.rows.len(), self.columns.len())
    }
}

/// Fold the Python `grid` list-of-dicts into `Vec<Subgrid>`, layering the
/// baseline scalar `params` under each subgrid. Values that are lists become
/// axes; `"start..end[:step]"` strings become ranges.
pub(crate) fn build_subgrids(
    baseline: &std::collections::HashMap<String, JsonValue>,
    grid_py: &Bound<'_, PyAny>,
) -> PyResult<Vec<spec_optimize::Subgrid>> {
    if grid_py.is_none() {
        return Err(PyValueError::new_err(
            "`grid` must be a list of dicts (at least one)",
        ));
    }
    let list = grid_py.cast::<pyo3::types::PyList>().map_err(|_| {
        PyTypeError::new_err("`grid` must be a list of dicts (each dict maps NAME -> value)")
    })?;
    if list.is_empty() {
        return Err(PyValueError::new_err("`grid` must contain at least one subgrid"));
    }
    let mut subgrids = Vec::with_capacity(list.len());
    for (idx, item) in list.iter().enumerate() {
        let dict = item.cast::<pyo3::types::PyDict>().map_err(|_| {
            PyTypeError::new_err(format!("`grid[{idx}]` must be a dict"))
        })?;
        let mut merged: std::collections::HashMap<String, JsonValue> = baseline.clone();
        for (k, v) in dict.iter() {
            let key: String = k.extract().map_err(|_| {
                PyTypeError::new_err(format!("`grid[{idx}]` keys must be strings"))
            })?;
            merged.insert(key, py_to_json(&v)?);
        }
        let (fixed, axes) = spec_optimize::split_axes(&merged)
            .map_err(|e| PyValueError::new_err(format!("--grid #{}: {e}", idx + 1)))?;
        let combos = spec_optimize::cartesian(&axes);
        subgrids.push(spec_optimize::Subgrid { fixed, axes, combos });
    }
    Ok(subgrids)
}

/// Run a parameter-grid sweep over a strategy YAML document. Returns a
/// `Sweep` with one row per grid point, ranked by `best_by` when set.
///
/// Pass `walkforward=(is, oos)` or `walkforward=(is, oos, embargo)` to run
/// walk-forward validation instead — mutually exclusive with `windowed=`;
/// returns a [`WalkForwardResult`] with per-fold IS/OOS metrics and the
/// stitched composite OOS equity curve.
#[pyfunction]
#[pyo3(signature = (
    text,
    snapshots,
    cash = 1.0,
    params = None,
    grid = None,
    kind = "auto",
    metric_names = None,
    best_by = None,
    windowed = None,
    walkforward = None,
    risk_aversion = 0.0,
    jobs = None,
    bars_per_year = 252.0,
    risk_free_rate = 0.0,
    costs = None,
    seconds_per_bar = None,
    base_dir = None,
))]
#[allow(clippy::too_many_arguments)]
pub(crate) fn optimize(
    py: Python<'_>,
    text: &str,
    snapshots: &Bound<'_, PyAny>,
    cash: Real,
    params: Option<&Bound<'_, PyAny>>,
    grid: Option<&Bound<'_, PyAny>>,
    kind: &str,
    metric_names: Option<Vec<String>>,
    best_by: Option<String>,
    windowed: Option<usize>,
    walkforward: Option<&Bound<'_, PyAny>>,
    risk_aversion: Real,
    jobs: Option<usize>,
    bars_per_year: Real,
    risk_free_rate: Real,
    costs: Option<&Bound<'_, PyAny>>,
    seconds_per_bar: Option<Real>,
    base_dir: Option<&str>,
) -> PyResult<Py<PyAny>> {
    // Walkforward and windowed are mutually exclusive (same as the CLI).
    let walkforward_tuple = extract_walkforward(walkforward)?;
    if walkforward_tuple.is_some() && windowed.is_some() {
        return Err(PyValueError::new_err(
            "`walkforward=` and `windowed=` are mutually exclusive",
        ));
    }
    let snaps = snapshots_from_sequence(snapshots)?;
    let params_table = extract_params(params)?;
    spec_optimize::reject_axes_in_params(&params_table)
        .map_err(|e| PyValueError::new_err(format!("`params`: {e}")))?;
    let base = std::path::PathBuf::from(base_dir.unwrap_or("."));
    // Load and !import-splice the base value once — every grid point substitutes
    // over this same value.
    let base_value = fugazi_core::spec::input::parse_value_at(text, "(inline)")
        .map_err(|e| PyValueError::new_err(format!("parsing strategy YAML: {e:#}")))?;
    let base_value = fugazi_core::spec::imports::resolve(base_value, &base)
        .map_err(|e| PyValueError::new_err(format!("resolving imports: {e:#}")))?;
    // Detect the kind from the raw (pre-`!param`) base value. Kind is fixed by
    // top-level shape, not by any parameter — running `!param` here would fail
    // for grid-only names.
    let detected = if kind == "auto" { detect_kind(&base_value) } else { kind };
    let grid_py = grid.ok_or_else(|| {
        PyValueError::new_err("`grid` is required (list of dicts, one per subgrid)")
    })?;
    let subgrids = build_subgrids(&params_table, grid_py)?;
    // Cost config resolved once — cloning it per row isn't needed because the
    // resolve() call inside evaluate_* takes &CostConfig.
    let cost_config = coerce_cost_config(costs)?;
    // Snapshots ref borrowed by the evaluate closure. `windowed = Some(n)`
    // switches the closure into windowed mode.

    let metric_names_vec: Vec<String> = metric_names.unwrap_or_default();
    let best_by_str = best_by.clone();

    // ----- Walkforward path (mutually exclusive with `windowed=`) -----
    if let Some((is_bars, oos_bars, embargo_bars)) = walkforward_tuple {
        return run_walkforward(
            py,
            detected,
            &base_value,
            &snaps,
            &cost_config,
            subgrids,
            is_bars,
            oos_bars,
            embargo_bars,
            &metric_names_vec,
            best_by_str.as_deref(),
            jobs,
            cash,
            bars_per_year,
            risk_free_rate,
            seconds_per_bar,
        );
    }

    let sweep = py.detach(|| -> anyhow::Result<spec_optimize::Sweep> {
        let ctx = spec_backtest::EvalContext {
            cash,
            bars_per_year,
            risk_free_rate,
            cost_config: &cost_config,
            // The Python surface takes snapshots, not a dated series, so
            // there's no bar cadence to resolve cost scopes against.
            effective_freq: None,
            windowed: windowed.and_then(std::num::NonZeroUsize::new),
            seconds_per_bar,
            mc: None,
        };
        let ctx_ref = &ctx;
        let evaluate_row = |params: &std::collections::HashMap<String, JsonValue>|
            -> anyhow::Result<spec_optimize::Evaluation>
        {
            let value = fugazi_core::spec::params::substitute(base_value.clone(), params)?;
            let spec = spec_from_value(value, detected)?;
            Ok(match windowed {
                None => spec_optimize::Evaluation::Whole(Box::new(
                    spec_backtest::evaluate_any(&spec, &snaps, ctx_ref)
                        .map_err(spec_backtest::build_error)?,
                )),
                Some(w) => spec_optimize::Evaluation::Windowed(
                    spec_backtest::evaluate_windowed_any(&spec, &snaps, ctx_ref, w)
                        .map_err(spec_backtest::build_error)?,
                ),
            })
        };

        spec_optimize::optimize(
            subgrids,
            windowed,
            &metric_names_vec,
            best_by_str.as_deref(),
            risk_aversion,
            jobs,
            evaluate_row,
        )
    })
    .map_err(|e| PyValueError::new_err(format!("optimize: {e:#}")))?;

    // Turn the kernel's Sweep into pyclass-facing rows. We serialize windowed
    // per-window metrics on demand only when `windowed` is set, matching the
    // API contract.
    let columns = sweep.union_columns.clone();
    let metric_columns = sweep.metric_columns.clone();

    let mut row_objs: Vec<Py<PySweepRow>> = Vec::with_capacity(sweep.rows.len());
    for row in &sweep.rows {
        // Extract metric values by resolved-column path.
        let metric_values: Vec<Option<Real>> = metric_columns
            .iter()
            .map(|(_user, resolved)| match &row.eval {
                spec_optimize::Evaluation::Whole(m) => spec_optimize::lookup(m.as_ref(), resolved),
                spec_optimize::Evaluation::Windowed(ws) => {
                    spec_optimize::lookup_windowed(ws.as_slice(), resolved).map(|(mean, _)| mean)
                }
            })
            .collect();
        let windowed_metrics = match &row.eval {
            spec_optimize::Evaluation::Whole(_) => None,
            spec_optimize::Evaluation::Windowed(ws) => {
                Some(ws.iter().map(|w| w.metrics.clone()).collect())
            }
        };
        let py_row = Py::new(
            py,
            PySweepRow {
                axis_columns: columns.clone(),
                axis_values: row.values.clone(),
                metric_columns: metric_columns.clone(),
                metric_values,
                windowed_metrics,
            },
        )?;
        row_objs.push(py_row);
    }
    // best is row 0 iff best_by was set; the kernel already sorted `rows`.
    let best_idx = if sweep.best_by.is_some() && !row_objs.is_empty() {
        Some(0)
    } else {
        None
    };
    let py_sweep = Py::new(
        py,
        PySweep {
            columns,
            metric_columns,
            rows: row_objs,
            best_idx,
        },
    )?;
    Ok(py_sweep.into_any())
}

/// Extract `(is, oos, embargo)` from a Python `walkforward=` argument. `None`
/// / a Python `None` returns `Ok(None)`; a 2- or 3-tuple / list of positive
/// ints returns `Ok(Some((is, oos, embargo)))` (embargo defaults to 0).
pub(crate) fn extract_walkforward(
    arg: Option<&Bound<'_, PyAny>>,
) -> PyResult<Option<(usize, usize, usize)>> {
    let Some(obj) = arg else { return Ok(None) };
    if obj.is_none() {
        return Ok(None);
    }
    let items: Vec<usize> = obj.try_iter()?
        .map(|it| it?.extract::<usize>())
        .collect::<PyResult<Vec<_>>>()
        .map_err(|_| PyValueError::new_err(
            "`walkforward` must be a 2- or 3-tuple of positive ints: (is, oos) or (is, oos, embargo)",
        ))?;
    match items.len() {
        2 => Ok(Some((items[0], items[1], 0))),
        3 => Ok(Some((items[0], items[1], items[2]))),
        n => Err(PyValueError::new_err(format!(
            "`walkforward` expects 2 or 3 ints (got {n})"
        ))),
    }
}

/// One fold's row in [`PyWalkForwardResult`]: the winning param combo, the
/// IS / OOS bar ranges, and both metrics documents.
#[pyclass(name = "WalkForwardFold", module = "fugazi")]
pub(crate) struct PyWalkForwardFold {
    pub(crate) fold: usize,
    pub(crate) is_range: (usize, usize),
    pub(crate) oos_range: (usize, usize),
    pub(crate) axis_columns: Vec<String>,
    pub(crate) axis_values: Vec<Option<JsonValue>>,
    pub(crate) is_metrics: SpecMetrics,
    pub(crate) oos_metrics: SpecMetrics,
}

#[pymethods]
impl PyWalkForwardFold {
    #[getter]
    pub(crate) fn fold(&self) -> usize {
        self.fold
    }
    #[getter]
    pub(crate) fn is_range(&self) -> (usize, usize) {
        self.is_range
    }
    #[getter]
    pub(crate) fn oos_range(&self) -> (usize, usize) {
        self.oos_range
    }
    #[getter]
    pub(crate) fn values(&self, py: Python<'_>) -> PyResult<Py<pyo3::types::PyDict>> {
        let d = pyo3::types::PyDict::new(py);
        for (name, v) in self.axis_columns.iter().zip(&self.axis_values) {
            match v {
                Some(val) => d.set_item(name, json_to_py(py, val)?)?,
                None => d.set_item(name, py.None())?,
            }
        }
        Ok(d.into())
    }
    #[getter]
    pub(crate) fn is_metrics(&self, py: Python<'_>) -> PyResult<Py<PyAny>> {
        metrics_to_py(py, &self.is_metrics)
    }
    #[getter]
    pub(crate) fn oos_metrics(&self, py: Python<'_>) -> PyResult<Py<PyAny>> {
        metrics_to_py(py, &self.oos_metrics)
    }
    pub(crate) fn __repr__(&self) -> String {
        format!(
            "WalkForwardFold(fold={}, is={:?}, oos={:?})",
            self.fold, self.is_range, self.oos_range
        )
    }
}

/// The result of a walk-forward run (`ta.optimize(..., walkforward=(is,
/// oos))`). Carries per-fold winner rows, the stitched composite OOS equity
/// curve, and the composite metrics document.
#[pyclass(name = "WalkForwardResult", module = "fugazi")]
pub(crate) struct PyWalkForwardResult {
    pub(crate) is_bars: usize,
    pub(crate) oos_bars: usize,
    pub(crate) embargo_bars: usize,
    pub(crate) prefix_skip: usize,
    pub(crate) folds: Vec<Py<PyWalkForwardFold>>,
    pub(crate) composite_equity: Vec<Real>,
    pub(crate) composite_fills: Vec<fugazi_core::Fill<String>>,
    pub(crate) composite_metrics: SpecMetrics,
    pub(crate) columns: Vec<String>,
    pub(crate) metric_columns: Vec<(String, String)>,
    pub(crate) cash: Real,
}

#[pymethods]
impl PyWalkForwardResult {
    #[getter]
    pub(crate) fn is_bars(&self) -> usize {
        self.is_bars
    }
    #[getter]
    pub(crate) fn oos_bars(&self) -> usize {
        self.oos_bars
    }
    #[getter]
    pub(crate) fn embargo_bars(&self) -> usize {
        self.embargo_bars
    }
    #[getter]
    pub(crate) fn prefix_skip(&self) -> usize {
        self.prefix_skip
    }
    #[getter]
    pub(crate) fn columns(&self) -> Vec<String> {
        self.columns.clone()
    }
    #[getter]
    pub(crate) fn metric_columns(&self) -> Vec<(String, String)> {
        self.metric_columns.clone()
    }
    #[getter]
    pub(crate) fn folds(&self, py: Python<'_>) -> Vec<Py<PyWalkForwardFold>> {
        self.folds.iter().map(|f| f.clone_ref(py)).collect()
    }
    #[getter]
    pub(crate) fn composite_equity(&self) -> Vec<Real> {
        self.composite_equity.clone()
    }
    #[getter]
    pub(crate) fn composite_fills(&self) -> Vec<PyFill> {
        self.composite_fills
            .iter()
            .cloned()
            .map(|inner| PyFill { inner })
            .collect()
    }
    #[getter]
    pub(crate) fn composite_metrics(&self, py: Python<'_>) -> PyResult<Py<PyAny>> {
        metrics_to_py(py, &self.composite_metrics)
    }
    #[getter]
    pub(crate) fn cash(&self) -> Real {
        self.cash
    }
    pub(crate) fn __repr__(&self) -> String {
        format!(
            "WalkForwardResult(folds={}, is={}, oos={}, embargo={})",
            self.folds.len(),
            self.is_bars,
            self.oos_bars,
            self.embargo_bars,
        )
    }
}

/// Drive the walk-forward kernel — same argument shape as [`optimize`] for
/// the plain sweep path, minus the mode toggles (walkforward is a distinct
/// mode). Wraps every strategy shape's `stable_period` + full-run backtest
/// in the two closures the library's [`spec_optimize::walkforward`] takes.
#[allow(clippy::too_many_arguments)]
pub(crate) fn run_walkforward(
    py: Python<'_>,
    detected: &str,
    base_value: &JsonValue,
    snaps: &[Snapshot<String>],
    cost_config: &fugazi_core::spec::costs::CostConfig,
    subgrids: Vec<spec_optimize::Subgrid>,
    is_bars: usize,
    oos_bars: usize,
    embargo_bars: usize,
    metric_names: &[String],
    best_by: Option<&str>,
    jobs: Option<usize>,
    cash: Real,
    bars_per_year: Real,
    risk_free_rate: Real,
    seconds_per_bar: Option<Real>,
) -> PyResult<Py<PyAny>> {
    // Bar count: single-asset walks the atom-per-symbol stream; every other
    // shape uses the snapshot count (already time-aligned upstream).
    let n_bars = match detected {
        "single" => snaps.len(),
        _ => snaps.len(),
    };

    let result = py
        .detach(|| -> anyhow::Result<spec_optimize::WalkForwardResult> {
            // Basket and multi build their per-symbol chains lazily, so their
            // periods only read true once a snapshot has gone through. The
            // eager shapes must not be fed one — a pairs leaf that didn't name
            // its asset would hit the sole-atom guard on a multi-symbol bar.
            let needs_probe_feed = matches!(detected, "basket" | "multi");
            let probe_snapshot = snaps.first().cloned().unwrap_or_default();
            let wf_ctx = spec_backtest::EvalContext {
                cash,
                bars_per_year,
                risk_free_rate,
                cost_config,
                effective_freq: None,
                windowed: None,
                seconds_per_bar,
                mc: None,
            };
            let wf_ctx_ref = &wf_ctx;
            let wf_schema = spec_backtest::schema_from_snapshots(snaps);

            let probe_readiness = |params: &std::collections::HashMap<String, JsonValue>|
                -> anyhow::Result<usize>
            {
                let value = fugazi_core::spec::params::substitute(base_value.clone(), params)?;
                let spec = spec_from_value(value, detected)?;
                let mut built = spec
                    .try_build(cash, &wf_schema, None)
                    .map_err(spec_backtest::build_error)?;
                if needs_probe_feed {
                    built.update(probe_snapshot.clone());
                }
                Ok(built.stable_period())
            };

            let run_backtest = |params: &std::collections::HashMap<String, JsonValue>|
                -> anyhow::Result<fugazi_core::RunReport<String>>
            {
                let value = fugazi_core::spec::params::substitute(base_value.clone(), params)?;
                let spec = spec_from_value(value, detected)?;
                spec_backtest::measured_report_any(&spec, snaps, wf_ctx_ref)
                    .map_err(spec_backtest::build_error)
            };

            spec_optimize::walkforward(
                subgrids,
                n_bars,
                probe_readiness,
                run_backtest,
                bars_per_year,
                risk_free_rate,
                seconds_per_bar,
                is_bars,
                oos_bars,
                embargo_bars,
                metric_names,
                best_by,
                jobs,
                cash,
            )
        })
        .map_err(|e| PyValueError::new_err(format!("walkforward: {e:#}")))?;

    // Convert into pyclass objects.
    let columns = result.union_columns.clone();
    let metric_columns = result.metric_columns.clone();
    let mut fold_objs: Vec<Py<PyWalkForwardFold>> = Vec::with_capacity(result.fold_rows.len());
    for row in &result.fold_rows {
        let fold = Py::new(
            py,
            PyWalkForwardFold {
                fold: row.fold,
                is_range: (row.is_start, row.is_end),
                oos_range: (row.oos_start, row.oos_end),
                axis_columns: columns.clone(),
                axis_values: row.values.clone(),
                is_metrics: row.is_metrics.clone(),
                oos_metrics: row.oos_metrics.clone(),
            },
        )?;
        fold_objs.push(fold);
    }
    let py_result = Py::new(
        py,
        PyWalkForwardResult {
            is_bars: result.is_bars,
            oos_bars: result.oos_bars,
            embargo_bars: result.embargo_bars,
            prefix_skip: result.prefix_skip,
            folds: fold_objs,
            composite_equity: result.composite_equity,
            composite_fills: result.composite_fills,
            composite_metrics: result.composite_metrics,
            columns,
            metric_columns,
            cash: result.cash,
        },
    )?;
    Ok(py_result.into_any())
}

