use crate::prelude::*;
// The binding modules were one flat namespace before the split and still read
// as one: each pulls in its siblings, so a cross-module reference needs no path.
#[allow(unused_imports)]
use crate::carriers::*;
#[allow(unused_imports)]
use crate::classes::*;
#[allow(unused_imports)]
use crate::constructors::*;
#[allow(unused_imports)]
use crate::metrics::*;
#[allow(unused_imports)]
use crate::sources::*;
#[allow(unused_imports)]
use crate::strategy::*;
// See `errors.rs`: a document that will not load or build is a `SpecError`,
// which subclasses `ValueError`. Argument validation of this module's own
// kwargs (`grid=`, `smooth=`, `windowed=`) stays a bare `ValueError`.
use crate::errors::SpecError;

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
            let items: PyResult<Vec<Py<PyAny>>> = arr.iter().map(|v| json_to_py(py, v)).collect();
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
    let dict = o
        .cast::<pyo3::types::PyDict>()
        .map_err(|_| PyTypeError::new_err("`params` must be a dict[str, Any] (or None)"))?;
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
            .map_err(|e| SpecError::new_err(format!("invalid TradingCostsConfig: {e}")))
    }

    pub(crate) fn build_view(tree: &JsonValue) -> PyResult<CostConfig> {
        serde_json::from_value(tree.clone())
            .map_err(|e| SpecError::new_err(format!("invalid TradingCostsConfig: {e}")))
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
    // Seven independent knobs with no natural order — keyword-only, so
    // `MonteCarloConfig(1000, "iid", 10.0, 7)` can never become the spelling
    // anyone has to keep working.
    #[pyo3(signature = (
        *,
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
        if !matches!(leg.as_str(), "commission" | "spread" | "slippage" | "carry") {
            return Err(PyValueError::new_err(format!(
                "TradingCostsConfig: unknown leg `{leg}` \
                 (expected commission/spread/slippage/carry)"
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
        .map_err(|e| SpecError::new_err(format!("invalid `costs=` dict: {e}")))
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
    if map.contains_key("root") {
        return "single";
    }
    // Bare per-side factories or a lone `long:` / `short:` mapping — that's
    // a multi-asset shape.
    "multi"
}

/// Load a strategy YAML doc from text, auto-detecting kind (or using the
/// caller's `kind` override). Returns the typed `CoreStrategySpec`.
///
/// `imports = false` disables `!import` entirely (see
/// [`fugazi_core::spec::load_value_no_imports`]) rather than merely confining
/// it to `base_dir` — the right choice for a caller that wants zero
/// filesystem coupling to a user-authored document. `root_dir` is the
/// `!import` confinement boundary — `base_dir` unless widened.
///
/// The kind is settled **before** `!param` substitution now, because a
/// single-asset document's default `root:` is spliced in between the two (see
/// [`fugazi_core::spec::root::apply_default`]) and only the single-asset shape
/// accepts that key. One consequence worth knowing about `kind="auto"`: `root:`
/// is what tells a `single:` document from a `multi:` one, and a document that
/// omits it is genuinely ambiguous — both shapes are then a bare `long:` /
/// `short:` map — so `auto` reads it as `multi`. Pass `kind="single"` to say
/// otherwise.
/// The typed shape a `kind=` string names — the same five the CLI's shape
/// prefix picks, so `spec::root::apply_default` sees the identical vocabulary
/// on both surfaces rather than a stringly-typed copy of it.
fn kind_of(kind: &str) -> PyResult<fugazi_core::spec::input::StrategyKind> {
    use fugazi_core::spec::input::StrategyKind as K;
    Ok(match kind {
        "single" => K::Single,
        "pairs" => K::Pairs,
        "basket" => K::Basket,
        "multi" => K::Multi,
        "portfolio" => K::Portfolio,
        other => {
            return Err(PyValueError::new_err(format!(
                "unknown strategy kind `{other}` (expected auto/single/pairs/basket/multi/portfolio)"
            )));
        }
    })
}

pub(crate) fn load_loaded_spec(
    text: &str,
    params: &std::collections::HashMap<String, JsonValue>,
    base_dir: &std::path::Path,
    root_dir: &std::path::Path,
    kind: &str,
    imports: bool,
) -> PyResult<CoreStrategySpec> {
    let pre = if imports {
        fugazi_core::spec::load_value_pre_params(text, base_dir, root_dir, "(inline)")
    } else {
        fugazi_core::spec::load_value_refusing_imports(text, "(inline)")
    }
    .map_err(|e| SpecError::new_err(format!("loading strategy: {e:#}")))?;
    let kind = if kind == "auto" {
        detect_kind(&pre)
    } else {
        kind
    };
    let pre = fugazi_core::spec::root::apply_default(pre, kind_of(kind)?);
    let value = fugazi_core::spec::params::substitute(pre, params)
        .map_err(|e| SpecError::new_err(format!("loading strategy: {e:#}")))?;
    macro_rules! parse {
        ($variant:ident, $ty:ty, $label:literal) => {{
            let s: $ty = serde_json::from_value(value)
                .map_err(|e| SpecError::new_err(format!("parsing {} strategy: {e}", $label)))?;
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
    /// Symbols the document reads through an explicit `!pick { symbol: … }`,
    /// captured from the loaded document — see the `reads` getter.
    pub(crate) reads: Vec<String>,
}

/// Drive one already-loaded spec against `wallet` and return the run report.
///
/// Same shape as [`PyStrategy::run`] and its siblings — the wallet's `equity()`
/// seeds the strategy, and any costs the caller pre-installed via
/// `wallet.set_costs_for(sym, ...)` apply naturally. Every shape trades the
/// wallet it is handed, portfolio included: a portfolio is an ordinary
/// `Strategy` that nets its children onto one account.
// `+ Send` is required only because the drive below is detached, and it is
// asked for **here** rather than on the `Wallet` trait: a third-party wallet
// impl that never crosses a thread stays unconstrained. All three concrete
// pyclass wallets satisfy it, and `over_any_wallet!` monomorphises per arm, so
// the compiler checks each one.
pub(crate) fn run_spec<W: Wallet<Symbol> + Send>(
    py: Python<'_>,
    loaded: &CoreStrategySpec,
    snapshots: &[Snapshot<Symbol>],
    wallet: &mut W,
) -> PyResult<RunReport<Symbol>> {
    // Refuse a declared symbol the stream never carries, before any bar is
    // driven. The CLI gets this from the frame; here the snapshots are the
    // caller's to construct, so this is where it lands.
    spec_backtest::validate_universe(loaded, snapshots).map_err(build_err)?;
    let cash = wallet.equity().0;
    let schema = spec_backtest::schema_from_snapshots(snapshots);
    let mut built = loaded.try_build(cash, &schema, None).map_err(build_err)?;
    // `&mut *built` rather than `&mut built`: `run` takes `S: Strategy + ?Sized`,
    // and it is `dyn RunnableStrategy` that carries the `Strategy` supertrait,
    // not the `Box` around it.
    //
    // The build above needs the GIL (a per-symbol factory may be a Python
    // callable); the drive does not, so it runs detached. `interruptible`
    // re-attaches every few thousand bars to poll for Ctrl-C.
    let interrupt = std::sync::Mutex::new(None);
    let report = py.detach(|| {
        fugazi_core::backtest::run(
            &mut *built,
            wallet,
            crate::classes::interruptible(snapshots.iter().cloned(), &interrupt),
        )
    });
    crate::classes::raise_if_interrupted(&interrupt, report)
}

/// The resumable superset of [`run_spec`]: optionally restore `resume` state
/// before the run, optionally flatten open positions at the end, and return the
/// run's final [`RunState`](fugazi_core::spec::RunState) alongside the report so
/// Python can persist it and resume later.
///
/// A thin adapter over the library's `drive_over` rather than a second
/// implementation of it — the version and kind checks, the flatten path and the
/// state capture all live in one place, so the Python and CLI surfaces cannot
/// drift apart.
///
/// # Interruptible?
///
/// No, and deliberately: unlike [`run_spec`] this **does not** thread the
/// snapshots through `interruptible`. `drive_over` needs the slice itself — for
/// `flatten`, for `last_bar`, for `bars_seen` — so there is no iterator seam to
/// wrap without handing it the same data twice and hoping the two agree.
///
/// It costs nothing here, because a resumable run is *already chunked by the
/// caller*: that is the whole point of it. The interrupt point is between
/// chunks, where the caller already stands. The GIL is still released for each
/// chunk, which is the part that a long warm-up actually needs.
pub(crate) fn run_spec_resumable<W: Wallet<Symbol> + Send>(
    py: Python<'_>,
    loaded: &CoreStrategySpec,
    snapshots: &[Snapshot<Symbol>],
    wallet: &mut W,
    resume: Option<&fugazi_core::spec::RunState>,
    flatten: bool,
) -> PyResult<(RunReport<Symbol>, fugazi_core::spec::RunState)> {
    // Cold starts only: on a resume, a chunk in which a symbol never quotes is
    // legitimate — the state carrying it came from an earlier chunk.
    if resume.is_none() {
        spec_backtest::validate_universe(loaded, snapshots).map_err(build_err)?;
    }
    let cash = wallet.equity().0;
    let schema = spec_backtest::schema_from_snapshots(snapshots);
    // Built with the GIL held — per-symbol factories may be Python callables.
    let mut built = loaded.try_build(cash, &schema, None).map_err(build_err)?;
    py.detach(|| fugazi_core::spec::drive_over(&mut *built, snapshots, wallet, resume, flatten))
        .map_err(build_err)
}

/// Parse the JSON string Python hands back as a resume state.
fn parse_resume(resume: Option<String>) -> PyResult<Option<fugazi_core::spec::RunState>> {
    resume
        .map(|text| {
            serde_json::from_str::<fugazi_core::spec::RunState>(&text)
                .map_err(|e| PyValueError::new_err(format!("parsing resume state: {e}")))
        })
        .transpose()
}

/// Serialize a state back to the JSON string Python persists.
fn state_json(state: &fugazi_core::spec::RunState) -> PyResult<String> {
    serde_json::to_string(state)
        .map_err(|e| PyValueError::new_err(format!("serializing run state: {e}")))
}

/// Advance a spec over `snapshots` without trading, returning the state to
/// resume from. See `StrategySpec.warm_up` for what it is for.
pub(crate) fn warm_up_spec<W: Wallet<Symbol> + Send>(
    py: Python<'_>,
    loaded: &CoreStrategySpec,
    snapshots: &[Snapshot<Symbol>],
    wallet: &mut W,
    resume: Option<&fugazi_core::spec::RunState>,
) -> PyResult<fugazi_core::spec::RunState> {
    use fugazi_core::spec::RunnableStrategyExt;
    let cash = wallet.equity().0;
    let schema = spec_backtest::schema_from_snapshots(snapshots);
    let mut built = loaded.try_build(cash, &schema, None).map_err(build_err)?;
    // Priming over months of history is the longest-blocking call on the
    // surface, so this is the one that most wanted the GIL dropped.
    py.detach(|| built.warm_up_over(snapshots, wallet, resume))
        .map_err(build_err)
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

/// Map a spec-build failure to a Python [`SpecError`], splitting the crate's
/// `!tag > ` breadcrumb onto its own line so the message reads the way the
/// CLI renders it.
pub(crate) fn build_err(e: String) -> PyErr {
    let (trail, message) = fugazi_core::spec::diagnostics::split_trail(&e);
    if trail.is_empty() {
        SpecError::new_err(message.to_string())
    } else {
        SpecError::new_err(format!("{message}\n  at: {}", trail.join(" > ")))
    }
}

/// Serialize a `SpecMetrics` document into a Python dict via serde_json.
pub(crate) fn metrics_to_py(py: Python<'_>, m: &SpecMetrics) -> PyResult<Py<PyAny>> {
    let value = serde_json::to_value(m)
        .map_err(|e| PyValueError::new_err(format!("serializing metrics: {e}")))?;
    json_to_py(py, &value)
}

/// Reduce a [`RunReport`](PyRunReport) to the full metric document — the same
/// nested dict `Strategy.evaluate` / `StrategySpec.evaluate` return, under the
/// same dotted key names, but without running anything.
///
/// This is the entry point for metrics over a curve that no `run()` in this
/// process produced: a live account's accrued equity, a resumed run, an
/// externally-computed series. Build the report, reduce it:
///
/// ```python
/// report = fugazi.RunReport(equity_curve=curve, initial_equity=10_000.0)
/// metrics = fugazi.evaluate_report(report, bars_per_year=252.0)
/// metrics["risk_adjusted"]["sharpe"]
/// ```
///
/// `bars_per_year` scales per-bar return moments to annual figures;
/// `risk_free_rate` is the annualized rf as a fraction (`0.045` = 4.5% p.a.);
/// `seconds_per_bar`, when given, populates the trades' `*_seconds` twins of the
/// `*_bars` fields.
///
/// The `trades.*` section is reconstructed from the report's fills, so a report
/// built from a bare curve reads there as a run that never traded — pass `fills`
/// to `RunReport` for the whole tree. The `costs.*` section is absent either
/// way: it is a property of the wallet that executed the run, not of the report.
///
/// Metrics assume a **closed system** — see the note on `fugazi.metrics`.
#[pyfunction]
#[pyo3(signature = (report, *, bars_per_year = 252.0, risk_free_rate = 0.0, seconds_per_bar = None))]
pub(crate) fn evaluate_report(
    py: Python<'_>,
    report: &PyRunReport,
    bars_per_year: Real,
    risk_free_rate: Real,
    seconds_per_bar: Option<Real>,
) -> PyResult<Py<PyAny>> {
    let metrics = spec_metrics::from_report(
        &report.inner,
        bars_per_year,
        risk_free_rate,
        seconds_per_bar,
    );
    metrics_to_py(py, &metrics)
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

    /// The document's free-form `meta:`, as plain Python data (`dict` / `list`
    /// / scalar), or `None` when the document set none.
    ///
    /// fugazi never interprets it — it is the open-schema slot for whatever
    /// service produced or stores this strategy. Mirrors Rust's
    /// `StrategySpec::meta`.
    #[getter]
    pub(crate) fn meta(&self, py: Python<'_>) -> PyResult<Py<PyAny>> {
        match self.inner.meta() {
            Some(v) => json_to_py(py, v),
            None => Ok(py.None()),
        }
    }

    /// The symbols this document **reads but does not trade** — every asset
    /// named by an explicit `!pick { symbol: … }` anywhere in the tree, sorted.
    ///
    /// A cross-asset expression (a regime gate on another asset, a spread leg)
    /// only resolves if that symbol is an entry in the snapshots you pass to
    /// `run`. It is not an error for it to be missing — `Pick` reads `None` on
    /// a bar it doesn't match, which is right for a listing gap — so a spec
    /// whose reads you never supplied simply never fires. Check this against
    /// what you're building snapshots from:
    ///
    /// ```python
    /// spec = ta.load_spec(open("gate.yml").read())
    /// assert set(spec.reads) <= set(df["symbol"].unique())
    /// ```
    ///
    /// The CLI does this check for you against `--series` and refuses the run;
    /// here the snapshots are yours to construct, so the check is yours too.
    /// Mirrors Rust's `spec::reads::picked_symbols`.
    #[getter]
    pub(crate) fn reads(&self) -> Vec<String> {
        self.reads.clone()
    }

    /// Drive the spec over `snapshots` against `wallet`, returning the full
    /// run report.
    ///
    /// `wallet` is a `PaperWallet`, an `OkxWallet` or a `CoinbaseWallet` —
    /// the same three `Strategy.run` accepts. Every shape trades the wallet it
    /// is handed, portfolio included, so the wallet's `equity()` seeds the
    /// strategy and any costs pre-installed via `wallet.set_costs_for(sym, ...)`
    /// apply naturally. Positions the account already holds are treated as the
    /// user's own and left untouched (the run trades a sleeve on top).
    pub(crate) fn run(
        &self,
        wallet: &Bound<'_, PyAny>,
        snapshots: &Bound<'_, PyAny>,
    ) -> PyResult<PyRunReport> {
        let snaps = snapshots_from_sequence(snapshots)?;
        over_any_wallet!(wallet, py, _seed, w => {
            let report = run_spec(py, &self.inner, &snaps, w)?;
            Ok(PyRunReport { inner: report })
        })
    }

    /// Drive the spec with **run resuming**: optionally restore `resume` (a JSON
    /// string previously returned here) before the run, optionally close out
    /// open positions with `flatten`, and return `(report, state_json)` — the
    /// run report plus the final state to persist and resume from later.
    ///
    /// `flatten=True` closes every open position **in the account**, through
    /// the normal cost pipeline, so the returned state holds a genuinely flat
    /// book and passing it to a later `resume` continues from flat.
    ///
    /// Takes the same three wallet types as [`Self::run`]. Against a live
    /// wallet the returned state's `wallet` field is `null`: the venue owns the
    /// positions and cash, so only the strategy's own state is carried and the
    /// account is re-read on resume.
    #[pyo3(signature = (wallet, snapshots, *, resume = None, flatten = false))]
    pub(crate) fn run_resumable(
        &self,
        wallet: &Bound<'_, PyAny>,
        snapshots: &Bound<'_, PyAny>,
        resume: Option<String>,
        flatten: bool,
    ) -> PyResult<(PyRunReport, String)> {
        let snaps = snapshots_from_sequence(snapshots)?;
        let resume_state = parse_resume(resume)?;
        over_any_wallet!(wallet, py, _seed, w => {
            let (report, state) =
                run_spec_resumable(py, &self.inner, &snaps, w, resume_state.as_ref(), flatten)?;
            Ok((PyRunReport { inner: report }, state_json(&state)?))
        })
    }

    /// Advance the spec over `snapshots` **without trading**, returning the
    /// state to resume from.
    ///
    /// Indicators warm and the account is marked to market exactly as in a real
    /// run, but no order is ever submitted. That is what closes a *pause gap*:
    /// bars that elapsed while a deployment was stopped should warm the
    /// strategy without booking trades at prices nobody could have traded at.
    /// Replay the gap here, hand the returned state to
    /// [`run_resumable`](Self::run_resumable), and go live — instead of
    /// dropping the state and re-serving a long-period indicator's whole
    /// warm-up after every pause.
    ///
    /// Returns the state JSON only; there is no report, because no run
    /// happened. A fill that arrives anyway (a resting order left from before
    /// the pause) still reaches the strategy, so its position cannot drift from
    /// the account's.
    #[pyo3(signature = (wallet, snapshots, resume = None))]
    pub(crate) fn warm_up(
        &self,
        wallet: &Bound<'_, PyAny>,
        snapshots: &Bound<'_, PyAny>,
        resume: Option<String>,
    ) -> PyResult<String> {
        let snaps = snapshots_from_sequence(snapshots)?;
        let resume_state = parse_resume(resume)?;
        over_any_wallet!(wallet, py, _seed, w => {
            let state = warm_up_spec(py, &self.inner, &snaps, w, resume_state.as_ref())?;
            state_json(&state)
        })
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
        // Evaluation knobs, not data — keyword-only, as on `optimize`.
        *,
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
        let report = run_spec(py, &self.inner, &snaps, &mut wallet.inner)?;
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
                .map_err(|e| SpecError::new_err(format!("cost config: {e}")))?;
            let ctx = spec_backtest::EvalContext {
                cash: report.initial_equity,
                // No wallet is built here — this reduces a report that already
                // exists — so none of the account settings have anything to act on.
                max_gross: None,
                leverage: 1.0,
                margin_rate: 0.0,
                maintenance_margin: None,
                bars_per_year,
                risk_free_rate,
                cost_config: &empty_costs,
                effective_freq: None,
                stream: None,
                windowed: None,
                seconds_per_bar,
                mc: None,
                warmup_bars: None,
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
            && let Some(obj) = value
                .get_mut("montecarlo")
                .and_then(JsonValue::as_object_mut)
        {
            obj.insert("samples".to_string(), mc_samples_to_json(&samples));
        }
        if let Some(w) = windowed {
            let win_rows = spec_metrics::windowed_from_report(
                &report,
                w,
                bars_per_year,
                risk_free_rate,
                seconds_per_bar,
            );
            let roll_rows = spec_metrics::rolling_from_report(
                &report,
                w,
                bars_per_year,
                risk_free_rate,
                seconds_per_bar,
            );
            let obj = value
                .as_object_mut()
                .expect("metrics document serializes to an object");
            obj.insert(
                "windowed".to_string(),
                serde_json::to_value(&win_rows).map_err(|e| {
                    PyValueError::new_err(format!("serializing windowed metrics: {e}"))
                })?,
            );
            obj.insert(
                "rolling".to_string(),
                serde_json::to_value(&roll_rows).map_err(|e| {
                    PyValueError::new_err(format!("serializing rolling metrics: {e}"))
                })?,
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
/// group        "node" | "selection" | "universe" | "weighting" | "document"
/// kind         "source" | "indicator" | "operator" | "predicate" | "function" |
///              "selection" | "universe" | "weighting" | "document"
/// forms        every spelling the tag accepts, canonical first (never empty)
/// output       what it evaluates to: "scalar" | "bool" | "str" | ... | "none"
/// projections  struct-output accessors (empty for fugazi's flattened tags)
/// category     fine conceptual sub-group ("moving averages", "oscillators",
///              "bands", …) — one rung finer than kind, for curated grouping
/// doc          the variant's `///`, as clean presentation prose
/// since        release it first shipped in
/// host_affecting  true only for tags whose resolution touches the host
///              (today, only "import" — a filesystem read); false otherwise
/// ```
///
/// Each entry of `forms`:
///
/// ```text
/// shape        "unit" | "newtype" | "seq" | "map"  (how it's written in YAML)
/// fields       [ {name, type, required, default, node_output?, doc} ]
///              (map forms only)
/// payload      positional payload type of a newtype/seq form ("node" |
///              "literal" | "node_list" | "str_list" | "number_list"); null for
///              unit/map forms
/// payload_output  node_output for that positional payload
/// scope        where this spelling is legal, when narrower than its group:
///              "template" | "portfolio_weights" | "internal"; absent = anywhere
/// doc          what this spelling does that the canonical one cannot
///              (present on every non-canonical form)
/// ```
///
/// **`forms` — a tag is a *set* of spellings, not one.** `!param NAME` and
/// `!param {key, default, type}` are the same tag written two ways, and only
/// the second can carry a default or a declared type; `!changed <node>` and `!changed {source: <node>}`
/// likewise. `forms[0]` is canonical — emit that. If you *accept* documents
/// (validate, complete, scaffold), iterate all of `forms`: eight tags have more
/// than one, and reading only the first is how a generator ends up unable to
/// scaffold `!param`'s `default`.
///
/// **`scope` — `group` is provenance, not position.** All four `document` tags
/// are resolved by a load- or build-time `Value` pass, but they are not
/// interchangeable in placement. `!param` and `!import` are genuinely
/// position-free: their passes rewrite *any* value position, an expression slot
/// or a scalar field like `period:` or a string field like `symbol:` alike.
/// `!arg` is `scope: "template"` — it is substituted only inside a deferred
/// template body (a basket's `score:` / `sizing:`, a multi-asset side's
/// `enter:`, a portfolio's `weights:`), and one written anywhere else is a hard
/// parse error, `check` included. `!undefined` is `scope: "internal"` and
/// should never be offered at all.
///
/// **`default` — what omitting a key gets you.** A **tagged** value with three
/// states, so a consumer never has to infer which it holds:
///
/// ```text
/// {"literal": 12}       a scalar key's JSON default  (34 fields)
/// {"expr": "!close"}    a YAML fragment              (69 fields)
/// null                  no expressible default
/// ```
///
/// `{"expr": …}` is the fragment a key whose default is a *node* falls back to:
/// `!ema`'s `source` is `!close`, `!atr`'s `!current`, `!donchian_upper`'s
/// `high` / `low` are `!high` / `!low`, a selection rule's `of` is
/// `!everything`. It parses in the slot it describes, so a completion menu can
/// both show it (`!macd_line · source=!close, fast=12, …`) and insert it.
/// `!ema {period: 10}` and `!ema {source: !close, period: 10}` are the same
/// expression, and a test settles that against the parser rather than leaving
/// it to prose. Don't scrape the `doc` for any of this.
///
/// A fragment is always a **root floor**: a bare leaf, which reads the series
/// its enclosing document blesses, so it never nests (`!close`, not
/// `!close {source: …}`). That is also why `null` is a real answer and not a
/// gap — a *leaf's own* `source:` defaults to the strategy's own series, which
/// no tag names and the floor already implies.
///
/// **`node_output` — what a slot must be filled *with*.** `type: "node"` says a
/// field holds a nested expression; `node_output` says which expressions are
/// admissible there, as `output` values you can match by string equality:
/// `!and`'s `lhs` is `["bool"]`, `!sma`'s `source` `["scalar"]`, `!changed`'s
/// payload `["bool", "scalar"]`. Three states — **absent** when the field holds
/// no free expression (a scalar field, or a book selector like `!drawdown`'s
/// `source`, which takes only `!strategy_book` / `!portfolio_book`), `[]` for a
/// passthrough that demands nothing (`!unstable`, `!resample`'s `inner`), else
/// the admitted set. This is the one part of the descriptor **not** reflected
/// off serde: it comes from the same table `check` enforces, so it is the type
/// discipline the JSON Schema deliberately omits.
///
/// **What this covers.** Every tag in five vocabularies, keyed by `group`:
/// `node` and `selection` are the slot-fillable *expression* grammars; `universe`
/// (`!all_of`/`!any_of`), `weighting` (portfolio `weights:` sugar
/// `!fixed`/`!equal_weight`), and `document` (load-time
/// `!import`/`!param`/`!arg`/`!undefined`) are document-level directives.
/// Consumers that filtered on `group == "node"` keep working unchanged.
///
/// **What it does not cover**, by design: the nested config *sub-documents* —
/// `costs:` (`TradingCostsConfig`) and a portfolio child's embedded strategy —
/// which are whole documents, not slot-level tags.
///
/// Same anti-drift guarantee as [`spec_tags`], one level deeper. The `node` /
/// `selection` / `universe` groups flow from the serde definitions via
/// `#[derive(SpecGrammar)]`; the load-time `weighting` / `document` tags aren't
/// serde variants (a `Value` pass rewrites them before the typed parse), so
/// their records are hand-authored with their *name set* pinned to the parser's
/// own rewrite list by a test. Either way downstream consumers (these very
/// Python constructors, editor tooling, docs, external grammar tables) generate
/// from one artifact rather than re-encoding by hand. Guard on `schema_version`
/// for *shape* changes: `payload` + its `"literal"` field type landed in v2,
/// `category` in v3 (0.51), `node_output` / `payload_output` in v4 (0.61),
/// v5 (0.67) moved `shape` / `fields` / `payload` / `payload_output` off the tag
/// and onto `forms`, v6 (0.68) added `host_affecting`, and v7 retagged a
/// field's `default`. The 0.50 group additions did
/// **not** bump it — new groups and legend values leave the record shape
/// unchanged, only a new *field* does. Two breaking changes so far:
/// `tag["shape"]` became `tag["forms"][0]["shape"]` in v5, and a field's bare
/// `default` value became `default["literal"]` in v7.
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
///
/// A tag that accepts more than one spelling (see [`spec_grammar`]'s `forms`)
/// is emitted as an `anyOf` over them, so `{"unstable": "close"}` and
/// `{"unstable": {"source": "close"}}` both validate — as they both always
/// parsed.
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

/// Every tag the YAML spec layer accepts, keyed by the vocabulary it belongs to.
/// Five groups: `"node"` (the one composable expression enum — numeric sources,
/// boolean predicates, and string comparisons together), `"selection"` (a
/// `basket:` document's `selection:` rules), `"universe"` (`!all_of`/`!any_of`),
/// `"weighting"` (portfolio `weights:` sugar `!fixed`/`!equal_weight`), and
/// `"document"` (load-time `!import`/`!param`/`!arg`/`!undefined`). Only `node`
/// and `selection` are slot-fillable expressions; the rest are document-level
/// directives. Names come back without the leading `!`.
///
/// A thin projection of [`spec_grammar`] — the names of each group — kept as a
/// convenience for discovery (`"sma" in ta.spec_tags()["node"]`) and the parity
/// test. Reach for [`spec_grammar`] when you need fields, defaults, or prose.
/// The group set is derived from the descriptor, so a new group flows in here
/// with no edit; a consumer that switched exhaustively on the old two-key dict
/// should treat unknown keys as inert.
#[pyfunction]
pub(crate) fn spec_tags(py: Python<'_>) -> PyResult<Py<PyAny>> {
    let grammar = fugazi_core::spec::grammar::spec_grammar();
    // Distinct groups in first-seen order (node, selection, universe, …).
    let mut groups: Vec<&str> = Vec::new();
    for tag in &grammar {
        if !groups.contains(&tag.group.as_str()) {
            groups.push(tag.group.as_str());
        }
    }
    let out = pyo3::types::PyDict::new(py);
    for group in groups {
        let names: Vec<&str> = grammar
            .iter()
            .filter(|t| t.group == group)
            .map(|t| t.name.as_str())
            .collect();
        out.set_item(group, names)?;
    }
    Ok(out.into_any().unbind())
}

/// What `tag` requires the expression in `slot` to **produce**.
///
/// The tag-keyed face of the type discipline `fugazi check` enforces — the
/// answer to "`!and`'s `lhs:` has to be *what*?" without a spec in hand. The
/// same datum the [`spec_grammar`] descriptor carries as a field's
/// `node_output`, reachable directly when you have a tag and a slot name rather
/// than a whole record.
///
/// `tag` may be written with or without its leading `!`. `slot` is the YAML key
/// (`source`, `lhs`, `high`, …), or the pseudo-slot a tag with no named fields
/// uses for its positional payload — `source` for `!not` / `!changed`, `item`
/// for `!all` / `!any`, `case value` for `!match`'s cases.
///
/// Three distinct answers:
///
/// * `None` — no such expression slot: an unknown tag, a scalar field like
///   `period:`, or a *book selector* like `!drawdown`'s `source`, which admits
///   only `!strategy_book` / `!portfolio_book`.
/// * `[]` — an expression slot that demands nothing of its output.
///   `!unstable`'s `source` and `!resample`'s `inner` are passthroughs.
/// * a non-empty list — the admitted `output` values, in the same vocabulary
///   [`spec_grammar`] uses, so they compare to a candidate tag's `output` by
///   string equality.
///
/// ```python
/// >>> ta.slot_demand("and", "lhs")
/// ['bool']
/// >>> ta.slot_demand("atr", "source")
/// ['candle']
/// >>> ta.slot_demand("changed", "source")     # either is accepted
/// ['bool', 'scalar']
/// >>> ta.slot_demand("unstable", "source")    # passthrough
/// []
/// >>> ta.slot_demand("sma", "period") is None
/// True
///
/// >>> # every tag that could legally fill !and's lhs
/// >>> want = ta.slot_demand("and", "lhs")
/// >>> [t["name"] for t in ta.spec_grammar()["tags"]
/// ...  if t["group"] == "node" and t["output"] in want][:3]
/// ['gt', 'lt', 'ge']
/// ```
#[pyfunction]
pub(crate) fn slot_demand(tag: &str, slot: &str) -> Option<Vec<&'static str>> {
    fugazi_core::spec::typecheck::slot_demand(tag, slot).map(|types| {
        types
            .into_iter()
            .map(fugazi_core::spec::grammar::output_label)
            .collect()
    })
}

/// Every expression slot `tag` has, with each one's demand — the whole-tag form
/// of [`slot_demand`], as a `{slot: [output, ...]}` dict.
///
/// Empty for a tag with no expression slots (`!entry`, `!is_weekday`) and for an
/// unknown tag. A slot present here with an empty list is a passthrough; a slot
/// *absent* from it holds no free expression. Iteration order is the order the
/// type checker reports the slots in.
///
/// ```python
/// >>> ta.slot_demands("if_else")
/// {'cond': ['bool'], 'then': ['scalar'], 'otherwise': ['scalar']}
/// >>> ta.slot_demands("is_weekday")
/// {}
/// ```
#[pyfunction]
pub(crate) fn slot_demands(py: Python<'_>, tag: &str) -> PyResult<Py<PyAny>> {
    let out = pyo3::types::PyDict::new(py);
    for (slot, types) in fugazi_core::spec::typecheck::slot_demands(tag) {
        let labels: Vec<&str> = types
            .into_iter()
            .map(fugazi_core::spec::grammar::output_label)
            .collect();
        out.set_item(slot, labels)?;
    }
    Ok(out.into_any().unbind())
}

/// Load a strategy YAML doc from text into a `StrategySpec`.
///
/// `params` is a dict of `!param` substitutions. `base_dir` is the directory
/// `!import` paths resolve against — it defaults to the **process's current
/// working directory**, not the caller's own location, so an embedder that
/// doesn't set it explicitly is granting whatever `!import` access that
/// directory allows. Imports resolve **before** the typed parse (`parse ->
/// !import -> !param -> typed parse`), so a malformed or unreachable import is
/// reported as an ordinary load error, not a later build error. Every import,
/// however deeply nested, is confined to `base_dir` — an absolute path or a
/// `..` that walks past it is refused, not followed.
///
/// `imports = False` disables `!import` entirely: any use of the tag anywhere
/// in the document (including inside a deferred template body such as a
/// basket's `score:` or a portfolio's `weights:`) is a load error, and
/// `base_dir` is not touched. Use this when `text` is authored by someone
/// other than the process owner and no filesystem access should be granted
/// at all — `base_dir`'s confinement narrows *where* an import can read from,
/// but only `imports=False` removes read access altogether.
///
/// Auto-detects the strategy kind unless `kind` is one of
/// `single`/`pairs`/`basket`/`multi`/`portfolio`.
///
/// `import_root` widens the `!import` confinement boundary beyond `base_dir`
/// — the directory a nested import may not resolve outside of, however
/// deeply nested. Defaults to `base_dir` itself (confining a document to its
/// own directory, same as the CLI without `--import-root`). Must contain (or
/// equal) `base_dir`, or `base_dir`'s own relative imports stop resolving.
#[pyfunction]
#[pyo3(signature = (text, *, params = None, base_dir = None, kind = "auto", imports = true, import_root = None))]
#[allow(clippy::too_many_arguments)]
pub(crate) fn load_spec(
    text: &str,
    params: Option<&Bound<'_, PyAny>>,
    base_dir: Option<&str>,
    kind: &str,
    imports: bool,
    import_root: Option<&str>,
) -> PyResult<PyStrategySpec> {
    let params = extract_params(params)?;
    let base = std::path::PathBuf::from(base_dir.unwrap_or("."));
    let root = import_root.map_or_else(|| base.clone(), std::path::PathBuf::from);
    let inner = load_loaded_spec(text, &params, &base, &root, kind, imports)?;
    // Collected from the same document `load_loaded_spec` parses, so a caller
    // assembling snapshots by hand can see which series the spec will need.
    let reads = if imports {
        fugazi_core::spec::reads::picked_symbols_of(text, &params, &base, &root, "(python)")
    } else {
        fugazi_core::spec::reads::picked_symbols_of_no_imports(text, &params, "(python)")
    }
    .map(|s| s.into_iter().collect())
    .unwrap_or_default();
    Ok(PyStrategySpec { inner, reads })
}

// ---------------------------------------------------------------------------
// Shape-only validation (`check`)
// ---------------------------------------------------------------------------

/// One placeholder a checked document left unresolved, and what its type has
/// to be.
///
/// This is the answer to a question an authoring tool otherwise has to guess
/// at: a strategy is written once and parameterised per run, so at the moment
/// it is saved a `!param` has no value — and no value is exactly what makes its
/// type hard to know. `check_spec` reads it off the *parse* instead, from the
/// slots the placeholder actually sits in: a `period:` demands a number, a
/// `symbol:` a string. No inference from the name, no ladder of fallbacks.
#[pyclass(name = "SpecHole", module = "fugazi", frozen, skip_from_py_object)]
#[derive(Clone)]
pub(crate) struct PySpecHole {
    name: String,
    origin: &'static str,
    declared: Option<&'static str>,
    demanded: Vec<&'static str>,
    used: Vec<&'static str>,
}

#[pymethods]
impl PySpecHole {
    /// The `params=` key for an unset `!param`, or the document path for an
    /// author-written `!undefined` — which has no name to give.
    #[getter]
    pub(crate) fn name(&self) -> &str {
        &self.name
    }

    /// `"param"` or `"undefined"` — whether this is a placeholder a caller is
    /// expected to supply a value for, or a gap the author declared.
    #[getter]
    pub(crate) fn origin(&self) -> &'static str {
        self.origin
    }

    /// The placeholder's own `type:` declaration, if it carried one:
    /// `"string"` / `"numeric"` / `"integer"` / `"bool"`.
    ///
    /// Sharper than anything a position can demand — `integer` and `numeric`
    /// are one demand as far as the document is concerned, but they reject
    /// different values, which is the point of writing one down.
    #[getter]
    pub(crate) fn declared(&self) -> Option<&'static str> {
        self.declared
    }

    /// Every type a *position* required of this placeholder, sorted:
    /// `"bool"` / `"number"` / `"string"` / `"list"` / `"table"`.
    ///
    /// At most one entry for a `!param` — two would mean no single value could
    /// satisfy the document, which `check_spec` refuses outright. Empty when
    /// the placeholder stands where a whole *expression* goes and nothing
    /// narrowed it further.
    #[getter]
    pub(crate) fn demanded(&self) -> Vec<&'static str> {
        self.demanded.clone()
    }

    /// [`demanded`](Self::demanded) plus `"expression"`, which is not a demand:
    /// it says the placeholder stands where a whole expression goes, and every
    /// scalar is one. Here for a caller that wants to tell "unconstrained" from
    /// "never observed"; prefer `required_type`.
    #[getter]
    pub(crate) fn used(&self) -> Vec<&'static str> {
        self.used.clone()
    }

    /// The one type to show a user, or `None` when nothing determined one.
    ///
    /// The declaration outranks the parse — the author said what the value is.
    /// Otherwise the single demanded type, and failing that `"expression"` for
    /// a placeholder standing where any scalar would do.
    #[getter]
    pub(crate) fn required_type(&self) -> Option<&'static str> {
        let sole = |types: &[&'static str]| match types {
            [one] => Some(*one),
            _ => None,
        };
        self.declared
            .or_else(|| sole(&self.demanded))
            .or_else(|| sole(&self.used))
    }

    pub(crate) fn __repr__(&self) -> String {
        format!(
            "SpecHole(name={:?}, origin={:?}, required_type={:?})",
            self.name,
            self.origin,
            self.required_type()
        )
    }
}

/// What `check_spec` learned about a document: that it is well-formed, and what
/// it still needs before it can run.
///
/// Deliberately **not** a `StrategySpec`. A checked document parses with every
/// unset placeholder standing as a typed zero — `period` 1, `symbol` `""` — so
/// a spec handed back from here would run, and silently backtest a strategy
/// nobody wrote. Loading a document you intend to *run* is `load_spec`, which
/// is the path that refuses a placeholder it cannot resolve.
#[pyclass(name = "SpecCheck", module = "fugazi", frozen)]
pub(crate) struct PySpecCheck {
    kind: &'static str,
    holes: Vec<PySpecHole>,
    reads: Vec<String>,
    built: bool,
}

#[pymethods]
impl PySpecCheck {
    /// The shape the document was validated as: `single` / `pairs` / `basket` /
    /// `multi` / `portfolio`. The detected one when `kind="auto"`.
    #[getter]
    pub(crate) fn kind(&self) -> &'static str {
        self.kind
    }

    /// Every placeholder left unresolved, sorted by `(origin, name)`. Empty for
    /// a document that is fully determined as written.
    #[getter]
    pub(crate) fn holes(&self) -> Vec<PySpecHole> {
        self.holes.clone()
    }

    /// `{name: required_type}` over the `!param` holes only — the dict form of
    /// the one question an authoring UI asks: what does each knob have to be?
    ///
    /// A value of `None` means nothing in the document narrowed it (the
    /// placeholder stands where any expression goes). `!undefined` holes are
    /// left out: they are keyed by document path, not by a name anyone passes a
    /// value for — read `holes` for those.
    ///
    /// **The unresolved placeholders only.** A `!param` carrying a `default:`,
    /// or bound through `params=`, is *resolved* — it is not a hole and does
    /// not appear here, whatever `type:` it declared. That is the right set for
    /// the question this answers (what does the caller still owe, and of what
    /// type), and it is exactly the set with no default to read a type off. A
    /// form that types **every** knob reads the defaulted ones from their
    /// defaults and only the required ones from here.
    #[getter]
    pub(crate) fn param_types(&self, py: Python<'_>) -> PyResult<Py<PyAny>> {
        let d = pyo3::types::PyDict::new(py);
        for hole in self.holes.iter().filter(|h| h.origin == "param") {
            d.set_item(&hole.name, hole.required_type())?;
        }
        Ok(d.into_any().unbind())
    }

    /// Symbols the document names through an explicit `!pick`, sorted. Same
    /// walk, and same meaning, as `StrategySpec.reads` — with no data in hand
    /// `check` cannot say these are *present*, only that they are *required*.
    #[getter]
    pub(crate) fn reads(&self) -> Vec<String> {
        self.reads.clone()
    }

    /// Whether the document was **built** as well as parsed.
    ///
    /// The build catches the one class of error a typed parse structurally
    /// cannot — a leaf with no asset to read in a shape that holds more than
    /// one — so it runs whenever the document is fully determined. It is
    /// skipped, and this reads `False`, where building would report a
    /// *document* error for a document whose only gap is an input nobody
    /// supplied: an `!undefined`, a placeholder standing in for a whole
    /// expression, a `!get` (whose build needs an overlay schema only real data
    /// supplies), or a single-asset `root:` left to the input.
    ///
    /// `False` is not a weaker verdict on the parse — everything the parse
    /// decides was decided either way.
    ///
    /// ## Which claim this is
    ///
    /// `True` says the document is **constructible as written**. It does *not*
    /// say "this will load once you supply values": the build ran with a typed
    /// zero standing in each hole, so it never saw the values a caller will
    /// eventually pass, and those are validated when they are passed. A
    /// `built=True` document with holes can still fail `load_spec` on a value
    /// its slot rejects — a `period` of `0`, a `type: integer` given `2.5`, a
    /// `freq` that is not a bar cadence.
    ///
    /// So for a caller deciding whether to reject a submission: an exception is
    /// the document being wrong, and that is the 4xx. `built` is a note on how
    /// much of the document could be checked without inputs — not a second
    /// verdict, and neither value of it means "reject".
    #[getter]
    pub(crate) fn built(&self) -> bool {
        self.built
    }

    pub(crate) fn __repr__(&self) -> String {
        format!(
            "SpecCheck(kind={:?}, holes={}, built={})",
            self.kind,
            self.holes.len(),
            self.built
        )
    }
}

/// Validate a strategy YAML document **without** binding values for its
/// `!param` placeholders, and report what each one has to be.
///
/// The same pass as `fugazi check strategy`, and the counterpart to
/// [`load_spec`] for a caller that has a document but not yet a run: an
/// authoring tool storing a strategy, a form validating on submit, a linter.
/// `load_spec` refuses a required `!param` with no value — correctly, since it
/// is about to hand back something runnable — and that refusal covers every
/// strategy whose author wrote a knob they intend to supply per run — including
/// the default `root:` written out longhand,
/// `!pick { symbol: !param SYMBOL, freq: !param FREQ }`, whose two placeholders
/// are both required in that spelling.
///
/// ```python
/// >>> doc = """
/// ... root: !pick { symbol: !param SYMBOL, freq: !param FREQ }
/// ... long:
/// ...   enter: !gt
/// ...     lhs: !sma { period: !param FAST }
/// ...     rhs: !value 0
/// ... """
/// >>> ta.check_spec(doc).param_types
/// {'FAST': 'number', 'FREQ': 'frequency', 'SYMBOL': 'symbol'}
/// >>> ta.check_spec(doc).built            # a root left to the input is not built
/// False
/// ```
///
/// ## The two spellings of that root are different documents
///
/// *Omitting* `root:` is not the same as writing the line above. The spliced
/// default's placeholders carry `default: null`, so they are **optional**: they
/// resolve to null, the `!pick` collapses to the sole-atom selector a
/// single-series input fills in, and `load_spec(text, params={})` has always
/// taken that document. It reports no holes here either — so a form built on
/// `param_types` is not offered a symbol box for it. Write the root out in the
/// bare spelling to make the two knobs required, and therefore reportable.
///
/// And a `root:`-less document is only single-asset if the caller says so: it
/// is structurally a `multi:` one, `kind="auto"` reads it that way, and only
/// `kind="single"` splices a root at all. Both surfaces agree — `load_spec`
/// detects the same shape — so the risk is a caller that meant single-asset and
/// did not say, not a disagreement between checking and running.
///
/// ## What it does *not* relax
///
/// Only the requirement that a placeholder have a value. Everything else is
/// validated exactly as `load_spec` validates it, on the same document: an
/// unknown tag, a misspelled field, a slot handed the wrong type, a malformed
/// `!pick`, a `!portfolio_book` outside a portfolio, a portfolio whose
/// `weights:` could never be read. On top of that it adds two checks
/// `load_spec` has no way to make — a placeholder whose positions contradict
/// each other (no single value could ever satisfy the document), and, when
/// nothing is left undetermined, a full build (see `SpecCheck.built`).
///
/// An unset placeholder is filled with a typed zero to let the parse proceed,
/// so a check cannot say anything about a document's *values* — that a period
/// is sensible, that a symbol exists. It says the document is well-formed and
/// what it still needs.
///
/// `params` may bind some placeholders and not others; a bound one is
/// substituted and type-checked normally, and only the rest become holes. The
/// `base_dir` / `imports` / `import_root` arguments mean exactly what they mean
/// on [`load_spec`], including `imports=False` refusing `!import` outright.
///
/// Raises `SpecError` for a document that is genuinely bad.
#[pyfunction]
#[pyo3(signature = (text, *, params = None, base_dir = None, kind = "auto", imports = true, import_root = None))]
pub(crate) fn check_spec(
    text: &str,
    params: Option<&Bound<'_, PyAny>>,
    base_dir: Option<&str>,
    kind: &str,
    imports: bool,
    import_root: Option<&str>,
) -> PyResult<PySpecCheck> {
    use fugazi_core::spec::undefined::UndefinedOrigin;

    let params = extract_params(params)?;
    let base = std::path::PathBuf::from(base_dir.unwrap_or("."));
    let root = import_root.map_or_else(|| base.clone(), std::path::PathBuf::from);
    let pre = if imports {
        fugazi_core::spec::load_value_pre_params(text, &base, &root, "(inline)")
    } else {
        fugazi_core::spec::load_value_refusing_imports(text, "(inline)")
    }
    .map_err(|e| SpecError::new_err(format!("loading strategy: {e:#}")))?;
    // Detected before `check_value` applies the shape's defaulted keys — a
    // `root:`-less document is structurally indistinguishable from a `multi:`
    // one, and splicing a default `root:` in first would decide it wrongly.
    let kind = if kind == "auto" {
        detect_kind(&pre)
    } else {
        kind
    };
    let checked = fugazi_core::spec::check::check_value(pre, kind_of(kind)?, &params)
        .map_err(|e| SpecError::new_err(format!("checking strategy: {e:#}")))?;

    let holes = checked
        .holes
        .iter()
        .map(|h| PySpecHole {
            name: h.name.clone(),
            origin: match h.origin {
                UndefinedOrigin::Param => "param",
                UndefinedOrigin::Undefined => "undefined",
            },
            declared: h.declared.map(|t| t.label()),
            demanded: h.demanded().iter().map(|t| t.label()).collect(),
            used: h.used.iter().map(|t| t.label()).collect(),
        })
        .collect();
    Ok(PySpecCheck {
        kind: checked.spec.kind(),
        holes,
        reads: checked.reads,
        built: checked.built,
    })
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
    // If pooled, one Metrics dict per panel member, in panel order.
    pub(crate) panel_metrics: Option<Vec<(String, SpecMetrics)>>,
    // If pooled *and* windowed, each member's per-window documents — the
    // replicates the shrinkage estimator actually fits on.
    pub(crate) panel_windowed: Option<Vec<(String, Vec<SpecMetrics>)>>,
    // If pooled, `(defined, members)` per metric column — the support behind
    // each pooled mean.
    pub(crate) panel_support: Option<Vec<Option<(usize, usize)>>>,
    // Under `smooth=`, the neighbourhood average this row was ranked by and the
    // support behind it. `None` when smoothing didn't run.
    pub(crate) smoothed: Option<Real>,
    pub(crate) support: Option<Real>,
    // The bar this row's account was ruined on, if it was.
    pub(crate) ruin_bar: Option<usize>,
    // (mean, std, defined, members) of this row's member-demeaned score.
    pub(crate) demeaned: Option<(Real, Real, usize, usize)>,
}

#[pymethods]
impl PySweepRow {
    /// The `smooth=` neighbourhood average of this row's ranking key, in the
    /// metric's native orientation. `None` when smoothing didn't run, when the
    /// row's own metric was undefined, or when `smooth_min_support` rejected it.
    #[getter]
    pub(crate) fn smoothed(&self) -> Option<Real> {
        self.smoothed
    }

    /// The bar this row's account was **ruined** on — the first bar close at
    /// which equity reached zero — or `None` for a row that stayed solvent.
    ///
    /// A ruined row is **not a candidate**: `best_by` never returns one, and
    /// under `smooth=` it contributes no weight to its neighbours and lowers
    /// their `support`. Its metrics are still here, because a pre-ruin Sharpe
    /// is a true description of the strategy while it was alive — this is the
    /// property that says it is only that. See `fugazi optimize`'s ruin
    /// warning, which reports the same thing on the console.
    #[getter]
    pub(crate) fn ruin_bar(&self) -> Option<usize> {
        self.ruin_bar
    }

    /// Whether this row's account was wiped out. Sugar for
    /// `row.ruin_bar is not None`.
    #[getter]
    pub(crate) fn ruined(&self) -> bool {
        self.ruin_bar.is_some()
    }

    /// The neighbourhood weight actually found, as a fraction of the weight a
    /// point in the interior of a *regular* axis of the same median spacing
    /// would find. `1.0` = as much evidence as a regular grid of that spacing;
    /// not clamped, so a denser-than-median stretch of an irregular axis reads
    /// above it. A numeric axis with only one value doesn't count against it —
    /// it isn't a swept dimension. `None` when smoothing didn't run.
    #[getter]
    pub(crate) fn support(&self) -> Option<Real> {
        self.support
    }

    /// This row's cross-member score with the **member level removed**, as
    /// `(mean, std, defined, members)` — the key `shrink=` ranks on.
    ///
    /// The raw pooled mean conflates "this parameter set is unstable across
    /// members" with "these instruments have different achievable Sharpe". The
    /// second is identical for every row, so it cannot separate them, yet it
    /// still inflates the spread — and unequally, since rows differ in which
    /// members they are defined on. Comparing this ordering against the raw one
    /// is how you see whether a pooled ranking was resting on the panel's
    /// composition.
    ///
    /// Present for any pooled sweep whose table could be fitted, whether or not
    /// `shrink=` was passed; `None` otherwise. Same 4-tuple layout as
    /// `PanelWalkForwardResult.breadth`.
    #[getter]
    pub(crate) fn demeaned(&self) -> Option<PyDemeanedScore> {
        self.demeaned
            .map(|(mean, std, defined, members)| PyDemeanedScore {
                mean,
                std,
                defined,
                members,
            })
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

    /// One metrics document per **panel member**, keyed by member name — the
    /// pooled twin of `metrics_windowed`. `None` when the sweep wasn't pooled.
    ///
    /// `row.metrics` holds the pooled *means* over these; this is what they
    /// were pooled from, so a member that dragged the mean down is findable
    /// rather than merely implied.
    #[getter]
    pub(crate) fn metrics_panel(&self, py: Python<'_>) -> PyResult<Py<PyAny>> {
        match &self.panel_metrics {
            None => Ok(py.None()),
            Some(members) => {
                let d = pyo3::types::PyDict::new(py);
                for (name, m) in members {
                    d.set_item(name, metrics_to_py(py, m)?)?;
                }
                Ok(d.into_any().unbind())
            }
        }
    }

    /// `{member: [metrics per window]}` under `panel=` **and** `windowed=` —
    /// each member's run cut into windows.
    ///
    /// These are the *replicates*: the readings the shrinkage estimator fits
    /// on, and the reason `windowed=` is what makes `disagreement` estimable in
    /// a sweep at all. Exposed so a caller can rebuild the same `ScoreTable`
    /// the sweep built — reproduce its fit, or extend it — rather than having
    /// to re-measure. `None` when the sweep was not both pooled and windowed.
    #[getter]
    pub(crate) fn metrics_panel_windowed(&self, py: Python<'_>) -> PyResult<Py<PyAny>> {
        match &self.panel_windowed {
            None => Ok(py.None()),
            Some(members) => {
                let d = pyo3::types::PyDict::new(py);
                for (name, windows) in members {
                    let items: Vec<Py<PyAny>> = windows
                        .iter()
                        .map(|m| metrics_to_py(py, m))
                        .collect::<PyResult<_>>()?;
                    d.set_item(name, items)?;
                }
                Ok(d.into_any().unbind())
            }
        }
    }

    /// How many panel members each pooled metric actually rests on:
    /// `{name: (defined, members)}`. `None` when the sweep wasn't pooled.
    ///
    /// An undefined metric stays undefined rather than becoming zero, so a
    /// pooled mean is taken over the members that *reported* it. Without this,
    /// a mean over 2 of 30 survivors and a mean over 30 read identically.
    #[getter]
    pub(crate) fn metrics_support(&self, py: Python<'_>) -> PyResult<Py<PyAny>> {
        match &self.panel_support {
            None => Ok(py.None()),
            Some(support) => {
                let d = pyo3::types::PyDict::new(py);
                for ((user, _resolved), v) in self.metric_columns.iter().zip(support) {
                    match v {
                        Some((defined, members)) => d.set_item(user, (*defined, *members))?,
                        None => d.set_item(user, py.None())?,
                    }
                }
                Ok(d.into_any().unbind())
            }
        }
    }

    /// One metrics document per **window** (`-w` / `windowed=`), in bar order.
    /// `None` when the sweep wasn't windowed.
    ///
    /// The windows tile the evaluated bars and each takes its initial equity
    /// from the bar before it, so a window's return series is the same slice of
    /// the whole run's. Each window's `(run.bars, returns.mean_bar,
    /// returns.stddev_bar)` is therefore a **sufficient statistic** for that
    /// slice: an exact mean / volatility / Sharpe over *any* union of windows —
    /// non-contiguous ones included, as a CSCV / PBO pass needs — follows from
    /// these three numbers per window, with no per-point return series kept.
    ///
    /// `returns.stddev_bar` is the `ddof = 1` estimator, so the window's
    /// centred second moment is `(n - 1) * stddev_bar ** 2`. Pool pairwise
    /// rather than by accumulating a sum of squares; the full recipe, and why,
    /// is in `docs/METRICS.md` under *Pooling windows*.
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
    // Under `shrink=`, each member's own pick: `(member, axis values)` sparse
    // across `columns`. The CLI writes these to a sibling CSV; a Python caller
    // has no file, so this getter is the only way to reach the N parameter sets
    // partial pooling exists to produce.
    pub(crate) member_winners: Vec<(String, Vec<Option<JsonValue>>)>,
    // Under `shrink=`, how many independent searches over the grid those
    // selections amounted to — the factor the deflated Sharpe's trial count was
    // scaled by.
    pub(crate) independent_searches: Option<Real>,
    // The sweep's two-way decomposition, for any pooled sweep.
    pub(crate) shrinkage: Option<fugazi_core::spec::shrinkage::Summary>,
    // Whether the rows are ordered by the member-demeaned score.
    pub(crate) shrunk: bool,
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

    /// Under `shrink=`, each member's own parameters as `{member: {axis: value}}`.
    ///
    /// **Always every member**, including the ones that landed on the pooled
    /// winner — so an empty dict means the sweep was not shrunk (or `λ` was
    /// unavailable, leaving no surface to select off) and *never* that the panel
    /// agreed. For "did anyone depart", read
    /// [`departed`](Self::departed), which does carry empty-means-agreed.
    #[getter]
    pub(crate) fn member_winners(&self, py: Python<'_>) -> PyResult<Py<pyo3::types::PyDict>> {
        winners_to_py(py, &self.columns, &self.member_winners)
    }

    /// The sweep's [`PanelShrinkage`] — how much of the spread between members
    /// is real disagreement, and whether the parameter moves the metric at all.
    ///
    /// Reported for **any** pooled sweep, not only a shrunk one: it is the
    /// number that says whether aggregating the members was the right thing to
    /// do, which a caller wants before deciding to pass `shrink=`. `None` when
    /// the sweep was not pooled, or the score table was too sparse to fit.
    #[getter]
    pub(crate) fn shrinkage(&self, py: Python<'_>) -> PyResult<Option<Py<PyPanelShrinkage>>> {
        PyPanelShrinkage::wrap(py, self.shrinkage)
    }

    /// Members whose pick differed from the pooled winner.
    ///
    /// [`member_winners`](Self::member_winners) lists **every** member,
    /// including those that landed on the pooled winner, so an empty dict there
    /// means "not shrunk" and never "the panel agreed". This is the derived
    /// question, and it carries empty-means-agreed the way
    /// `PanelFold.departed` does — so the two grains answer it the same way
    /// rather than one of them making the caller compare dicts.
    #[getter]
    pub(crate) fn departed(&self) -> Vec<String> {
        let Some(best) = self.best_idx else {
            return Vec::new();
        };
        let winner = &self.rows[best];
        Python::attach(|py| {
            let winner_values = winner.borrow(py).axis_values.clone();
            self.member_winners
                .iter()
                .filter(|(_, values)| *values != winner_values)
                .map(|(member, _)| member.clone())
                .collect()
        })
    }

    /// Whether `shrink=` ranked this sweep — i.e. whether `rows` is ordered by
    /// the member-demeaned score rather than the raw pooled reduction.
    ///
    /// Worth reading back rather than remembering what you passed: it is the
    /// difference between two orderings of the same rows, and nothing else in
    /// the result says which one you are looking at.
    #[getter]
    pub(crate) fn shrunk(&self) -> bool {
        self.shrunk
    }

    /// Under `shrink=`, how many *independent* searches over the grid the
    /// per-member selections amounted to — `1.0` when the members agreed (one
    /// shared surface), up to the member count when they share nothing.
    ///
    /// This is the factor the deflated Sharpe's trial count was scaled by, so a
    /// caller comparing DSR across runs can see why it moved. `None` when the
    /// sweep was not shrunk or `λ` was unavailable.
    #[getter]
    pub(crate) fn independent_searches(&self) -> Option<Real> {
        self.independent_searches
    }

    /// The number of grid points evaluated — `len(sweep)` == `len(sweep.rows)`.
    pub(crate) fn __len__(&self) -> usize {
        self.rows.len()
    }

    /// Iterate the rows, so `for row in sweep` needs no `.rows` detour.
    pub(crate) fn __iter__(&self, py: Python<'_>) -> PyResult<Py<PyAny>> {
        crate::classes::iter_over(py, self.rows(py))
    }

    /// Index or slice the rows — `sweep[0]`, `sweep[-1]`, `sweep[:10]`.
    ///
    /// Delegating the whole thing to the materialised `list` is what makes
    /// slices, negative indices and the `IndexError` message come out exactly as
    /// a caller expects, rather than three hand-rolled approximations of them.
    pub(crate) fn __getitem__(
        &self,
        py: Python<'_>,
        index: &Bound<'_, PyAny>,
    ) -> PyResult<Py<PyAny>> {
        let list = pyo3::types::PyList::new(py, self.rows(py))?;
        Ok(list.as_any().get_item(index)?.unbind())
    }

    pub(crate) fn __repr__(&self) -> String {
        format!(
            "Sweep(rows={}, columns={})",
            self.rows.len(),
            self.columns.len()
        )
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
        return Err(PyValueError::new_err(
            "`grid` must contain at least one subgrid",
        ));
    }
    let mut subgrids = Vec::with_capacity(list.len());
    for (idx, item) in list.iter().enumerate() {
        let dict = item
            .cast::<pyo3::types::PyDict>()
            .map_err(|_| PyTypeError::new_err(format!("`grid[{idx}]` must be a dict")))?;
        let mut merged: std::collections::HashMap<String, JsonValue> = baseline.clone();
        for (k, v) in dict.iter() {
            let key: String = k
                .extract()
                .map_err(|_| PyTypeError::new_err(format!("`grid[{idx}]` keys must be strings")))?;
            merged.insert(key, py_to_json(&v)?);
        }
        let (fixed, axes) = spec_optimize::split_axes(&merged)
            .map_err(|e| PyValueError::new_err(format!("--grid #{}: {e}", idx + 1)))?;
        let combos = spec_optimize::cartesian(&axes);
        subgrids.push(spec_optimize::Subgrid {
            fixed,
            axes,
            combos,
        });
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
///
/// `base_dir`, `imports` and `import_root` behave exactly as on
/// [`load_spec`]: `base_dir` confines (and defaults to the process cwd),
/// `imports = False` disables `!import` outright, and `import_root` widens
/// the confinement boundary beyond `base_dir`.
#[pyfunction]
#[pyo3(signature = (
    text,
    snapshots = None,
    // Everything below is configuration, not data: the order is an
    // implementation detail and no caller means `optimize(doc, snaps, 1.0, None,
    // None, "auto", ...)`. Keyword-only pins that down before the order becomes
    // API by accident.
    *,
    cash = 1.0,
    max_gross = None,
    leverage = 1.0,
    margin_rate = 0.0,
    maintenance_margin = None,
    params = None,
    grid = None,
    kind = "auto",
    metric_names = None,
    best_by = None,
    windowed = None,
    walkforward = None,
    panel = None,
    panel_axis = None,
    risk_aversion = 0.0,
    smooth = None,
    smooth_min_support = 0.0,
    smooth_scale = None,
    shrink = false,
    jobs = None,
    bars_per_year = 252.0,
    risk_free_rate = 0.0,
    costs = None,
    seconds_per_bar = None,
    base_dir = None,
    imports = true,
    import_root = None,
))]
#[allow(clippy::too_many_arguments)]
pub(crate) fn optimize(
    py: Python<'_>,
    text: &str,
    snapshots: Option<&Bound<'_, PyAny>>,
    cash: Real,
    max_gross: Option<Real>,
    leverage: Real,
    margin_rate: Real,
    maintenance_margin: Option<Real>,
    params: Option<&Bound<'_, PyAny>>,
    grid: Option<&Bound<'_, PyAny>>,
    kind: &str,
    metric_names: Option<Vec<String>>,
    best_by: Option<String>,
    windowed: Option<usize>,
    walkforward: Option<&Bound<'_, PyAny>>,
    panel: Option<&Bound<'_, PyAny>>,
    panel_axis: Option<String>,
    risk_aversion: Real,
    smooth: Option<&str>,
    smooth_min_support: Real,
    smooth_scale: Option<&str>,
    shrink: bool,
    jobs: Option<usize>,
    bars_per_year: Real,
    risk_free_rate: Real,
    costs: Option<&Bound<'_, PyAny>>,
    seconds_per_bar: Option<Real>,
    base_dir: Option<&str>,
    imports: bool,
    import_root: Option<&str>,
) -> PyResult<Py<PyAny>> {
    // Walkforward and windowed are mutually exclusive (same as the CLI).
    let walkforward_tuple = extract_walkforward(walkforward)?;
    if walkforward_tuple.is_some() && windowed.is_some() {
        return Err(PyValueError::new_err(
            "`walkforward=` and `windowed=` are mutually exclusive",
        ));
    }
    // `panel=` and `windowed=` **compose**, and the composition is not a nested
    // reduction. Under `panel=`, `windowed=` does not change the pooled numbers
    // at all: each member is measured once and reduced twice, so the whole-run
    // document every pooled column reads is untouched, and the per-window
    // documents ride beside it as within-cell *replicates*.
    //
    // That replication is the whole point — it is the only thing that lets
    // `shrink=` separate "the members disagree" from "the backtests are noisy",
    // which with one observation per member are the same quantity. Refusing the
    // pair left `shrink=` unable to estimate anything in a sweep.
    if shrink && panel.is_none() {
        return Err(PyValueError::new_err(
            "`shrink=True` needs `panel=` — partial pooling lets each panel member depart \
             from the pooled answer, and without a panel there is nothing to pool",
        ));
    }
    if shrink && best_by.is_none() {
        return Err(PyValueError::new_err(
            "`shrink=True` needs `best_by=` — partial pooling shrinks a ranking key toward \
             the panel's consensus, and without one there is no surface to select off",
        ));
    }
    // The kernel refuses this pair too, but in the CLI's own vocabulary
    // (`--shrink`, `-k`). Catching it here keeps a Python caller from being
    // told about flags they never typed — the same reason the two checks above
    // are spelled in kwargs rather than left to the shared message.
    if shrink && risk_aversion > 0.0 {
        return Err(PyValueError::new_err(
            "`shrink=` and `risk_aversion=` are rival answers to the same question. \
             `risk_aversion=` charges a parameter set for the spread between panel members; \
             `shrink=` models that spread and lets each member move by however much of it is \
             real. Applying both pays for the same disagreement twice. Pick one",
        ));
    }
    // Exactly one source of data. `snapshots` stays positional for the single
    // stream case; a panel names its members, so it comes in by keyword.
    let panel_members = panel.map(extract_panel).transpose()?;
    if panel_axis.is_some() && panel_members.is_none() {
        return Err(PyValueError::new_err(
            "`panel_axis=` needs `panel=` — there is no member name to substitute without one",
        ));
    }
    match (snapshots.is_some(), panel_members.is_some()) {
        (false, false) => {
            return Err(PyValueError::new_err(
                "pass either `snapshots` (one stream) or `panel=` (several, pooled)",
            ));
        }
        (true, true) => {
            return Err(PyValueError::new_err(
                "pass either `snapshots` or `panel=`, not both — a pooled sweep reduces \
                 across the panel's members and has no separate single stream to rank against",
            ));
        }
        _ => {}
    }
    // `smooth=` mirrors the CLI's `--smooth` grammar: "box:1", "triangle:2",
    // "gaussian:1.5". Presence is what turns smoothing on; `smooth_min_support`
    // only tunes it, exactly as `--smooth-min-support` does.
    // `smooth_scale=` mirrors `--smooth-scale`: "index", "PERIOD:log",
    // "linear,PERIOD:log". `None` leaves every axis on the automatic choice.
    let scales = smooth_scale
        .map(|spec| {
            spec.parse::<spec_optimize::SmoothScales>()
                .map_err(|e| PyValueError::new_err(format!("`smooth_scale`: {e}")))
        })
        .transpose()?;
    if scales.is_some() && smooth.is_none() {
        return Err(PyValueError::new_err(
            "`smooth_scale=` needs `smooth=` — there is no neighbourhood to measure without a kernel",
        ));
    }
    let smoothing = smooth
        .map(|spec| {
            let kernel: spec_optimize::SmoothKernel = spec
                .parse()
                .map_err(|e| PyValueError::new_err(format!("`smooth`: {e}")))?;
            let sm = spec_optimize::Smoothing::new(kernel, smooth_min_support)
                .map_err(|e| PyValueError::new_err(format!("`smooth_min_support`: {e}")))?;
            Ok::<_, PyErr>(match scales.clone() {
                Some(scales) => sm.with_scales(scales),
                None => sm,
            })
        })
        .transpose()?;
    let snaps = match snapshots {
        Some(obj) => snapshots_from_sequence(obj)?,
        None => Vec::new(),
    };
    let params_table = extract_params(params)?;
    spec_optimize::reject_axes_in_params(&params_table)
        .map_err(|e| PyValueError::new_err(format!("`params`: {e}")))?;
    let base = std::path::PathBuf::from(base_dir.unwrap_or("."));
    let root = import_root.map_or_else(|| base.clone(), std::path::PathBuf::from);
    // Load and !import-splice the base value once — every grid point substitutes
    // over this same value.
    let base_value = fugazi_core::spec::input::parse_value_at(text, "(inline)")
        .map_err(|e| SpecError::new_err(format!("parsing strategy YAML: {e:#}")))?;
    let base_value = if imports {
        fugazi_core::spec::imports::resolve(base_value, &base, &root)
            .map_err(|e| SpecError::new_err(format!("resolving imports: {e:#}")))?
    } else {
        fugazi_core::spec::imports::refuse(&base_value)
            .map_err(|e| SpecError::new_err(format!("resolving imports: {e:#}")))?;
        base_value
    };
    // Detect the kind from the raw (pre-`!param`) base value. Kind is fixed by
    // top-level shape, not by any parameter — running `!param` here would fail
    // for grid-only names.
    let detected = if kind == "auto" {
        detect_kind(&base_value)
    } else {
        kind
    };
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

    // ----- Pooled walkforward: one winner per fold, on the pooled IS score -----
    if let (Some((is_bars, oos_bars, embargo_bars)), Some(members)) =
        (walkforward_tuple, panel_members.as_ref())
    {
        return run_panel_walkforward(
            py,
            detected,
            &base_value,
            members,
            panel_axis.as_deref(),
            &cost_config,
            subgrids,
            is_bars,
            oos_bars,
            embargo_bars,
            &metric_names_vec,
            best_by_str.as_deref(),
            risk_aversion,
            smoothing.as_ref(),
            shrink,
            jobs,
            cash,
            max_gross,
            leverage,
            margin_rate,
            maintenance_margin,
            bars_per_year,
            risk_free_rate,
            seconds_per_bar,
        );
    }

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
            smoothing.as_ref(),
            jobs,
            cash,
            max_gross,
            leverage,
            margin_rate,
            maintenance_margin,
            bars_per_year,
            risk_free_rate,
            seconds_per_bar,
        );
    }

    // Polled once per row so Ctrl-C ends a long grid. See `SweepInterrupt` for
    // why only the main thread asks Python and the workers read an atomic.
    let interrupt = crate::classes::SweepInterrupt::new();
    let sweep = crate::classes::run_watched(
        py,
        &interrupt,
        || -> anyhow::Result<spec_optimize::Sweep> {
            let ctx = spec_backtest::EvalContext {
                cash,
                max_gross,
                leverage,
                margin_rate,
                maintenance_margin,
                bars_per_year,
                risk_free_rate,
                cost_config: &cost_config,
                // The Python surface takes snapshots, not a dated series, so
                // there's no bar cadence to resolve cost scopes against.
                effective_freq: None,
                stream: None,
                windowed: windowed.and_then(std::num::NonZeroUsize::new),
                seconds_per_bar,
                mc: None,
                warmup_bars: None,
            };
            let ctx_ref = &ctx;
            // Borrowed as slices once, so the per-row closure re-borrows rather
            // than cloning every member's stream on every grid point.
            let panel_axis_ref = panel_axis.as_deref();
            let panel_ref: Option<PanelSlices<'_>> = panel_members.as_ref().map(|ms| {
                ms.iter()
                    .map(|m| (m.name.clone(), m.snaps.as_slice()))
                    .collect()
            });
            let panel_ref = panel_ref.as_deref();
            // The value each member substitutes for `panel_axis=`, parallel to
            // `panel_ref`. Carried separately because the pooled evaluators take
            // (label, stream) pairs and the axis value is neither.
            let axis_values: Option<Vec<JsonValue>> = panel_members
                .as_ref()
                .map(|ms| ms.iter().map(|m| m.axis.clone()).collect());
            let axis_values = axis_values.as_deref();
            let evaluate_row = |params: &std::collections::HashMap<String, JsonValue>|
            -> anyhow::Result<spec_optimize::Evaluation>
        {
            if interrupt.should_stop() {
                anyhow::bail!("interrupted");
            }
            // Under `panel_axis=`, the document is only complete *per member* —
            // the axis is exactly the parameter the members differ in, so
            // substituting the row's params alone would leave it unset and fail
            // the load. So that arm never builds a shared spec; every other
            // path does, once.
            let build_shared = || -> anyhow::Result<_> {
                let value = fugazi_core::spec::params::substitute(base_value.clone(), params)?;
                spec_from_value(value, detected)
            };
            Ok(match (panel_ref, windowed) {
                // Pooled: one document per member, reduced across them.
                //
                // Under `panel_axis=`, the member's *name* is substituted for
                // that parameter first, so each member is rooted on its own
                // series — the same thing the CLI's `--pooled` does. Built
                // per member rather than once, because the spec differs.
                (Some(members), _) => spec_optimize::Evaluation::Panel(match panel_axis_ref {
                    None => spec_backtest::evaluate_panel_any(&build_shared()?, members, ctx_ref)
                        .map_err(spec_backtest::build_error)?,
                    Some(axis) => members
                        .iter()
                        .zip(axis_values.expect("panel_ref implies axis_values"))
                        .map(|((name, snaps), axis_value)| {
                            let mut p = params.clone();
                            p.insert(axis.to_string(), axis_value.clone());
                            let value =
                                fugazi_core::spec::params::substitute(base_value.clone(), &p)?;
                            let member_spec = spec_from_value(value, detected)?;
                            let one = [(name.clone(), *snaps)];
                            let mut out =
                                spec_backtest::evaluate_panel_any(&member_spec, &one, ctx_ref)
                                    .map_err(spec_backtest::build_error)?;
                            Ok::<_, anyhow::Error>(out.remove(0))
                        })
                        .collect::<anyhow::Result<Vec<_>>>()?,
                }),
                (None, None) => spec_optimize::Evaluation::Whole(Box::new(
                    spec_backtest::evaluate_any(&build_shared()?, &snaps, ctx_ref)
                        .map_err(spec_backtest::build_error)?,
                )),
                (None, Some(w)) => spec_optimize::Evaluation::Windowed(
                    spec_backtest::evaluate_windowed_any(&build_shared()?, &snaps, ctx_ref, w)
                        .map_err(spec_backtest::build_error)?,
                ),
            })
        };

            spec_optimize::optimize(
                subgrids,
                &metric_names_vec,
                best_by_str.as_deref(),
                risk_aversion,
                smoothing.as_ref(),
                shrink,
                jobs,
                evaluate_row,
            )
        },
    )
    .map_err(|e| SpecError::new_err(format!("optimize: {e:#}")));
    // A row that saw the signal aborts the sweep with an ordinary error; the
    // parked `KeyboardInterrupt` is the one the caller asked for.
    let sweep = interrupt.raise_over(sweep)?;

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
                spec_optimize::Evaluation::Panel(ms) => {
                    fugazi_core::spec::panel::pool_metric(ms, resolved).map(|p| p.mean)
                }
            })
            .collect();
        let windowed_metrics = match &row.eval {
            spec_optimize::Evaluation::Windowed(ws) => {
                Some(ws.iter().map(|w| w.metrics.clone()).collect())
            }
            _ => None,
        };
        // Per-member documents, keyed by member — the pooled twin of
        // `metrics_windowed`, plus the support each pooled cell rests on.
        let (panel_metrics, panel_support) = match &row.eval {
            spec_optimize::Evaluation::Panel(ms) => {
                let members: Vec<(String, SpecMetrics)> = ms
                    .iter()
                    .map(|m| (m.member.clone(), m.metrics.clone()))
                    .collect();
                let support: Vec<Option<(usize, usize)>> = metric_columns
                    .iter()
                    .map(|(_user, resolved)| {
                        fugazi_core::spec::panel::pool_metric(ms, resolved)
                            .map(|p| (p.defined, p.members))
                    })
                    .collect();
                (Some(members), Some(support))
            }
            _ => (None, None),
        };
        // The replicates behind the pooled cells, when `windowed=` supplied
        // them. Empty `windows` on every member means the sweep was pooled but
        // not windowed, which is the unreplicated case — reported as absent
        // rather than as a member-keyed dict of empty lists.
        let panel_windowed = match &row.eval {
            spec_optimize::Evaluation::Panel(ms) if ms.iter().any(|m| !m.windows.is_empty()) => {
                Some(
                    ms.iter()
                        .map(|m| {
                            (
                                m.member.clone(),
                                m.windows.iter().map(|w| w.metrics.clone()).collect(),
                            )
                        })
                        .collect(),
                )
            }
            _ => None,
        };
        let py_row = Py::new(
            py,
            PySweepRow {
                axis_columns: columns.clone(),
                axis_values: row.values.clone(),
                metric_columns: metric_columns.clone(),
                metric_values,
                windowed_metrics,
                panel_metrics,
                panel_support,
                panel_windowed,
                smoothed: row.smoothed.and_then(|s| s.value),
                support: row.smoothed.and_then(|s| s.support),
                ruin_bar: row.eval.ruin_bar(),
                demeaned: row.demeaned.map(|p| (p.mean, p.std, p.defined, p.members)),
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
            member_winners: sweep
                .member_winners
                .iter()
                .map(|w| (w.member.clone(), w.values.clone()))
                .collect(),
            independent_searches: sweep.selection.map(|b| b.effective),
            shrinkage: sweep.shrinkage,
            shrunk: sweep.shrunk,
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
    pub(crate) is_smoothed: Option<Real>,
    pub(crate) is_support: Option<Real>,
}

#[pymethods]
impl PyWalkForwardFold {
    /// Under `smooth=`, the neighbourhood average of the winning row's IS
    /// ranking key — the value this fold was actually selected on.
    #[getter]
    pub(crate) fn is_smoothed(&self) -> Option<Real> {
        self.is_smoothed
    }

    /// The neighbourhood support behind [`Self::is_smoothed`].
    #[getter]
    pub(crate) fn is_support(&self) -> Option<Real> {
        self.is_support
    }

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

/// A panel's members as the pooled evaluators take them: name plus a borrowed
/// snapshot stream.
pub(crate) type PanelSlices<'a> = Vec<(String, &'a [Snapshot<Symbol>])>;

/// One `panel=` member: its label, the value `panel_axis=` substitutes for it,
/// and its own snapshot stream.
pub(crate) struct PanelMember {
    /// What the pooled support counts, `metrics_panel` keys, and the
    /// walk-forward composites are labelled by.
    pub(crate) name: String,
    /// What `panel_axis=` substitutes for its `!param`, **in the mapping key's
    /// own JSON type**. See [`extract_panel`].
    pub(crate) axis: JsonValue,
    pub(crate) snaps: Vec<Snapshot<Symbol>>,
}

/// Coerce the `panel=` argument into named snapshot streams.
///
/// Accepts a mapping (`{"BTC": snaps, "ETH": snaps}`) or a plain sequence of
/// streams (auto-named `member[0]`, `member[1]`, …). The mapping form is worth
/// preferring: pooled cells report *which* members reported a metric, and
/// `member[7]` is a poor answer to that.
///
/// **A mapping key carries its own type through to `panel_axis=`.** A `str`
/// key substitutes as a JSON string, an `int`/`float` as a JSON number, a
/// `bool` as a JSON boolean — so `{5: snaps, 10: snaps}` with
/// `panel_axis="FAST"` reaches a `period:` slot as a number and pools over a
/// *parameter*, exactly as the CLI's `--pooled 'FAST=[5,10]'`
/// does. Nothing is parsed out of the label: `{"5": …}` is the string `"5"`
/// and `{5: …}` is the number `5`, so a member genuinely named `"5"` stays
/// unambiguous. The label is the key rendered without JSON quoting, so the
/// string case is byte-identical to what it always was.
///
/// Each stream is a separate feed on purpose. Handing one merged
/// multi-instrument stream instead would make every member run over the
/// **union** timeline and see bars on which it has no quote — which is exactly
/// what the CLI's per-root stream preparation exists to avoid, and Python has
/// no equivalent to inherit. Pooling over a parameter is the case where that
/// bites: every member is the same series, so it is passed (and copied) once
/// per member.
pub(crate) fn extract_panel(panel: &Bound<'_, PyAny>) -> PyResult<Vec<PanelMember>> {
    let mut out: Vec<PanelMember> = Vec::new();
    if let Ok(map) = panel.cast::<pyo3::types::PyDict>() {
        for (k, v) in map.iter() {
            let axis = panel_key_value(&k)?;
            out.push(PanelMember {
                name: panel_key_label(&axis),
                axis,
                snaps: snapshots_from_sequence(&v)?,
            });
        }
    } else {
        let iter = panel.try_iter().map_err(|_| {
            PyTypeError::new_err(
                "`panel=` expected a mapping of name -> snapshots, or a sequence of \
                 snapshot streams",
            )
        })?;
        for (i, item) in iter.enumerate() {
            let name = format!("member[{i}]");
            out.push(PanelMember {
                axis: JsonValue::String(name.clone()),
                name,
                snaps: snapshots_from_sequence(&item?)?,
            });
        }
    }
    if out.is_empty() {
        return Err(PyValueError::new_err(
            "`panel=` is empty — pooling needs at least one member",
        ));
    }
    let mut seen = std::collections::HashSet::new();
    for m in &out {
        if !seen.insert(m.name.clone()) {
            return Err(PyValueError::new_err(format!(
                "`panel=` has two members named `{}` — member names label the \
                 pooled support counts, so they have to be distinct",
                m.name
            )));
        }
    }
    Ok(out)
}

/// One `panel=` mapping key, as the JSON scalar `panel_axis=` will substitute.
///
/// `bool` is tested before `int` because Python's `bool` *is* an `int`, and a
/// `!param` slot expecting a boolean would otherwise be handed `1`.
fn panel_key_value(key: &Bound<'_, PyAny>) -> PyResult<JsonValue> {
    if let Ok(s) = key.extract::<String>() {
        return Ok(JsonValue::String(s));
    }
    if let Ok(b) = key.cast::<pyo3::types::PyBool>() {
        return Ok(JsonValue::Bool(b.is_true()));
    }
    if let Ok(i) = key.extract::<i64>() {
        return Ok(JsonValue::from(i));
    }
    if let Ok(f) = key.extract::<f64>()
        && let Some(n) = serde_json::Number::from_f64(f)
    {
        return Ok(JsonValue::Number(n));
    }
    Err(PyTypeError::new_err(
        "`panel=` mapping keys must be str, int, float or bool — the key labels the member \
         *and* is what `panel_axis=` substitutes for its `!param`, so it has to be a value a \
         document can hold",
    ))
}

/// A member's display label: the key rendered without JSON quoting, so a `str`
/// key reads exactly as it was written.
fn panel_key_label(axis: &JsonValue) -> String {
    match axis {
        JsonValue::String(s) => s.clone(),
        other => other.to_string(),
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
    pub(crate) composite_fills: Vec<fugazi_core::Fill<Symbol>>,
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

    /// The number of folds — `len(result)` == `len(result.folds)`.
    pub(crate) fn __len__(&self) -> usize {
        self.folds.len()
    }

    /// Iterate the folds, so `for fold in result` needs no `.folds` detour.
    pub(crate) fn __iter__(&self, py: Python<'_>) -> PyResult<Py<PyAny>> {
        crate::classes::iter_over(py, self.folds(py))
    }

    /// Index or slice the folds — `result[0]`, `result[-1]`, `result[:2]`.
    pub(crate) fn __getitem__(
        &self,
        py: Python<'_>,
        index: &Bound<'_, PyAny>,
    ) -> PyResult<Py<PyAny>> {
        let list = pyo3::types::PyList::new(py, self.folds(py))?;
        Ok(list.as_any().get_item(index)?.unbind())
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
/// mode). Wraps every strategy shape's `stable_bars` + full-run backtest
/// in the two closures the library's [`spec_optimize::walkforward`] takes.
#[allow(clippy::too_many_arguments)]
pub(crate) fn run_walkforward(
    py: Python<'_>,
    detected: &str,
    base_value: &JsonValue,
    snaps: &[Snapshot<Symbol>],
    cost_config: &fugazi_core::spec::costs::CostConfig,
    subgrids: Vec<spec_optimize::Subgrid>,
    is_bars: usize,
    oos_bars: usize,
    embargo_bars: usize,
    metric_names: &[String],
    best_by: Option<&str>,
    smoothing: Option<&spec_optimize::Smoothing>,
    jobs: Option<usize>,
    cash: Real,
    max_gross: Option<Real>,
    leverage: Real,
    margin_rate: Real,
    maintenance_margin: Option<Real>,
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

    // Same per-row polling as the grid sweep — a walk-forward runs the whole
    // grid once per fold, so it is the *longer* of the two.
    let interrupt = crate::classes::SweepInterrupt::new();
    let result = crate::classes::run_watched(
        py,
        &interrupt,
        || -> anyhow::Result<spec_optimize::WalkForwardResult> {
            // Basket and multi build their per-symbol chains lazily, so their
            // periods only read true once a snapshot has gone through. The
            // eager shapes must not be fed one — a pairs leaf that didn't name
            // its asset would hit the sole-atom guard on a multi-symbol bar.
            let needs_probe_feed = matches!(detected, "basket" | "multi");
            let probe_snapshot = snaps.first().cloned().unwrap_or_default();
            let wf_ctx = spec_backtest::EvalContext {
                cash,
                max_gross,
                leverage,
                margin_rate,
                maintenance_margin,
                bars_per_year,
                risk_free_rate,
                cost_config,
                effective_freq: None,
                stream: None,
                windowed: None,
                seconds_per_bar,
                mc: None,
                warmup_bars: None,
            };
            let wf_ctx_ref = &wf_ctx;
            let wf_schema = spec_backtest::schema_from_snapshots(snaps);

            let probe_readiness =
                |params: &std::collections::HashMap<String, JsonValue>| -> anyhow::Result<usize> {
                    let value = fugazi_core::spec::params::substitute(base_value.clone(), params)?;
                    let spec = spec_from_value(value, detected)?;
                    let mut built = spec
                        .try_build(cash, &wf_schema, None)
                        .map_err(spec_backtest::build_error)?;
                    if needs_probe_feed {
                        built.update(probe_snapshot.clone());
                    }
                    Ok(built.stable_bars())
                };

            let run_backtest = |params: &std::collections::HashMap<String, JsonValue>|
                -> anyhow::Result<fugazi_core::RunReport<Symbol>>
            {
                if interrupt.should_stop() {
                    anyhow::bail!("interrupted");
                }
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
                smoothing,
                jobs,
                cash,
            )
        },
    )
    .map_err(|e| SpecError::new_err(format!("walkforward: {e:#}")));
    let result = interrupt.raise_over(result)?;

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
                is_smoothed: row.is_smoothed.and_then(|s| s.value),
                is_support: row.is_smoothed.and_then(|s| s.support),
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

// ---------------------------------------------------------------------------
// Pooled walk-forward
// ---------------------------------------------------------------------------

/// What a panel is worth as evidence: `(effective, mean_correlation, members,
/// pairs)`, with names.
///
/// Returned by both `PanelWalkForwardResult.effective_breadth` (correlation
/// between member *returns*) and `PanelDecomposition.selection_breadth`
/// (correlation between member *ranking surfaces*). Same quantity, same field
/// order, two things to correlate.
///
/// **Still a tuple where it counts.** It iterates, indexes and compares as
/// `(effective, mean_correlation, members, pairs)`, so
/// `eff, rho, n, pairs = result.effective_breadth` keeps working; `.members` is
/// simply the spelling that cannot be transposed.
// `skip_from_py_object`: these are results, never arguments — nothing
// accepts a PanelBreadth. The `Clone` is for returning by value.
#[pyclass(name = "PanelBreadth", module = "fugazi", skip_from_py_object)]
#[derive(Clone, Copy)]
pub(crate) struct PyPanelBreadth {
    pub(crate) effective: Real,
    pub(crate) mean_correlation: Real,
    pub(crate) members: usize,
    pub(crate) pairs: usize,
}

#[pymethods]
impl PyPanelBreadth {
    /// How many *independent* members the panel is worth —
    /// `members / (1 + (members - 1) * mean_correlation)`.
    #[getter]
    pub(crate) fn effective(&self) -> Real {
        self.effective
    }

    /// Mean pairwise correlation over the pairs that could be measured.
    /// Negative values are reported but floored at zero inside `effective`.
    #[getter]
    pub(crate) fn mean_correlation(&self) -> Real {
        self.mean_correlation
    }

    /// Members with enough data to be correlated against anything — the `M` in
    /// the formula, not the panel's declared size.
    #[getter]
    pub(crate) fn members(&self) -> usize {
        self.members
    }

    /// Pairs actually measured.
    #[getter]
    pub(crate) fn pairs(&self) -> usize {
        self.pairs
    }

    pub(crate) fn __iter__(&self, py: Python<'_>) -> PyResult<Py<PyAny>> {
        Ok(self.as_tuple(py)?.try_iter()?.into_any().unbind())
    }

    pub(crate) fn __len__(&self) -> usize {
        4
    }

    pub(crate) fn __getitem__(
        &self,
        py: Python<'_>,
        index: &Bound<'_, PyAny>,
    ) -> PyResult<Py<PyAny>> {
        // Through the object protocol, not `PyTuple::get_item`: that takes a
        // `usize`, which would silently drop negative indices and slices — two
        // things a caller reasonably expects of something that destructures.
        Ok(self.as_tuple(py)?.as_any().get_item(index)?.unbind())
    }

    pub(crate) fn __eq__(&self, py: Python<'_>, other: &Bound<'_, PyAny>) -> PyResult<bool> {
        self.as_tuple(py)?.eq(other)
    }

    pub(crate) fn __hash__(&self, py: Python<'_>) -> PyResult<isize> {
        self.as_tuple(py)?.hash()
    }

    pub(crate) fn __repr__(&self) -> String {
        format!(
            "PanelBreadth(effective={:.4}, mean_correlation={:.4}, members={}, pairs={})",
            self.effective, self.mean_correlation, self.members, self.pairs,
        )
    }
}

impl PyPanelBreadth {
    fn as_tuple<'py>(&self, py: Python<'py>) -> PyResult<Bound<'py, pyo3::types::PyTuple>> {
        use pyo3::IntoPyObject;
        (
            self.effective,
            self.mean_correlation,
            self.members,
            self.pairs,
        )
            .into_pyobject(py)
    }

    pub(crate) fn from_breadth(b: fugazi_core::spec::panel::Breadth) -> Self {
        Self {
            effective: b.effective,
            mean_correlation: b.mean_correlation,
            members: b.members,
            pairs: b.pairs,
        }
    }
}

/// A row's cross-member score with the member level removed:
/// `(mean, std, defined, members)`, with names.
///
/// `defined` and `members` are the coverage behind `mean` — a mean over 2 of 30
/// members and one over 30 of 30 are not the same evidence, and positionally
/// they render as "2 of 30" and "30 of 2" with equal plausibility. That is what
/// the names are for.
///
/// **Still a tuple where it counts** — it iterates, indexes and compares as
/// `(mean, std, defined, members)`.
// `skip_from_py_object`: these are results, never arguments — nothing
// accepts a DemeanedScore. The `Clone` is for returning by value.
#[pyclass(name = "DemeanedScore", module = "fugazi", skip_from_py_object)]
#[derive(Clone, Copy)]
pub(crate) struct PyDemeanedScore {
    pub(crate) mean: Real,
    pub(crate) std: Real,
    pub(crate) defined: usize,
    pub(crate) members: usize,
}

#[pymethods]
impl PyDemeanedScore {
    /// Cross-member mean of the demeaned cells — the key `shrink=` ranks on.
    #[getter]
    pub(crate) fn mean(&self) -> Real {
        self.mean
    }

    /// Population standard deviation across those same members.
    #[getter]
    pub(crate) fn std(&self) -> Real {
        self.std
    }

    /// Members that **reported** a value for this row.
    #[getter]
    pub(crate) fn defined(&self) -> usize {
        self.defined
    }

    /// Members in the panel. `defined <= members` always; the gap is members
    /// that ran but could not compute the metric.
    #[getter]
    pub(crate) fn members(&self) -> usize {
        self.members
    }

    pub(crate) fn __iter__(&self, py: Python<'_>) -> PyResult<Py<PyAny>> {
        Ok(self.as_tuple(py)?.try_iter()?.into_any().unbind())
    }

    pub(crate) fn __len__(&self) -> usize {
        4
    }

    pub(crate) fn __getitem__(
        &self,
        py: Python<'_>,
        index: &Bound<'_, PyAny>,
    ) -> PyResult<Py<PyAny>> {
        // Through the object protocol, not `PyTuple::get_item`: that takes a
        // `usize`, which would silently drop negative indices and slices — two
        // things a caller reasonably expects of something that destructures.
        Ok(self.as_tuple(py)?.as_any().get_item(index)?.unbind())
    }

    pub(crate) fn __eq__(&self, py: Python<'_>, other: &Bound<'_, PyAny>) -> PyResult<bool> {
        self.as_tuple(py)?.eq(other)
    }

    pub(crate) fn __hash__(&self, py: Python<'_>) -> PyResult<isize> {
        self.as_tuple(py)?.hash()
    }

    pub(crate) fn __repr__(&self) -> String {
        format!(
            "DemeanedScore(mean={:.4}, std={:.4}, defined={}, members={})",
            self.mean, self.std, self.defined, self.members,
        )
    }
}

impl PyDemeanedScore {
    fn as_tuple<'py>(&self, py: Python<'py>) -> PyResult<Bound<'py, pyo3::types::PyTuple>> {
        use pyo3::IntoPyObject;
        (self.mean, self.std, self.defined, self.members).into_pyobject(py)
    }
}

/// A row × member table of scores — the estimator's input, for callers that
/// pool a panel themselves.
///
/// `optimize(panel=…, shrink=True)` builds one of these internally and never
/// shows it. That is no use to a caller who reduces across members with its own
/// machinery: there is nothing for `shrink=` to be plumbed into, and reaching it
/// would mean giving up whatever made the caller's own pooling worth having.
/// This is the same estimator with the sweep taken off the front.
///
/// Rows are parameter points, members are whatever you pooled over. Each cell
/// holds that pair's **replicate** observations — measure the same
/// `(row, member)` over several sub-spans and push each one, because with a
/// single observation per cell "the members disagree" and "the backtests are
/// noisy" are the same sum of squares and no split exists.
///
/// **Ragged is expected.** A pair you never measured is simply an empty cell.
/// Never push a zero to stand in for one: a substituted zero is
/// indistinguishable from a measurement, and every statistic downstream would
/// rest on it.
///
/// ```py
/// t = ta.ScoreTable(rows=len(grid), members=len(panel))
/// for r, params in enumerate(grid):
///     for m, member in enumerate(panel):
///         t.extend(r, m, sharpe_per_window(params, member))   # the replicates
/// d = t.decompose()                    # None if too sparse to fit
/// d.summary.disagreement               # lambda
/// d.shrunk                             # per-member surface to select off
/// ```
#[pyclass(name = "ScoreTable", module = "fugazi")]
pub(crate) struct PyScoreTable {
    pub(crate) inner: fugazi_core::spec::shrinkage::ScoreTable,
}

#[pymethods]
impl PyScoreTable {
    /// An empty `rows × members` table.
    #[new]
    #[pyo3(signature = (rows, members))]
    pub(crate) fn new(rows: usize, members: usize) -> Self {
        Self {
            inner: fugazi_core::spec::shrinkage::ScoreTable::new(rows, members),
        }
    }

    /// Build from a nested `cells[row][member] -> sequence of replicates`.
    ///
    /// The shape is taken from the outer lengths, so a short row is an error
    /// rather than a silently narrower table — a ragged *input* is a bug, while
    /// a ragged *table* (empty cells) is ordinary and is spelled by passing an
    /// empty sequence.
    #[staticmethod]
    pub(crate) fn from_cells(cells: Vec<Vec<Vec<Real>>>) -> PyResult<Self> {
        let rows = cells.len();
        let members = cells.first().map_or(0, Vec::len);
        for (r, row) in cells.iter().enumerate() {
            if row.len() != members {
                return Err(PyValueError::new_err(format!(
                    "ScoreTable.from_cells: row {r} has {} members but row 0 has {members} — \
                     every row must span the same members; an unmeasured pair is an empty \
                     sequence, not a missing column",
                    row.len(),
                )));
            }
        }
        let mut inner = fugazi_core::spec::shrinkage::ScoreTable::new(rows, members);
        for (r, row) in cells.into_iter().enumerate() {
            for (m, replicates) in row.into_iter().enumerate() {
                inner.extend(r, m, replicates);
            }
        }
        Ok(Self { inner })
    }

    /// Record one observation. Out-of-range indices and non-finite values are
    /// dropped rather than raising — a `NaN` in one cell would otherwise take
    /// every sum of squares with it.
    pub(crate) fn push(&mut self, row: usize, member: usize, value: Real) {
        self.inner.push(row, member, value);
    }

    /// Record a cell's replicates in one call.
    pub(crate) fn extend(&mut self, row: usize, member: usize, values: Vec<Real>) {
        self.inner.extend(row, member, values);
    }

    #[getter]
    pub(crate) fn rows(&self) -> usize {
        self.inner.rows()
    }

    #[getter]
    pub(crate) fn members(&self) -> usize {
        self.inner.members()
    }

    /// One cell's replicates; empty when the pair was never measured.
    pub(crate) fn cell(&self, row: usize, member: usize) -> Vec<Real> {
        self.inner.cell(row, member).to_vec()
    }

    /// A cell's mean, or `None` when it holds nothing.
    pub(crate) fn cell_mean(&self, row: usize, member: usize) -> Option<Real> {
        self.inner.cell_mean(row, member)
    }

    /// Cells holding at least one observation.
    #[getter]
    pub(crate) fn populated(&self) -> usize {
        self.inner.populated()
    }

    /// Total observations across every cell.
    #[getter]
    pub(crate) fn observations(&self) -> usize {
        self.inner.observations()
    }

    /// Cells carrying enough replicates to speak to within-cell spread. Zero
    /// here is why `decompose().summary.disagreement` comes back `None`.
    #[getter]
    pub(crate) fn replicated_cells(&self) -> usize {
        self.inner.replicated_cells()
    }

    /// Fit the two-way layout, or `None` when the table cannot carry it.
    ///
    /// `None` on three conditions, all of them "there is not enough table",
    /// never "the answer is zero": fewer than 6 populated cells, fewer than two
    /// live rows or members, or no degrees of freedom left for an interaction
    /// once both margins are spent (`cells - rows - members + 1 <= 0`).
    /// [`populated`](Self::populated) and
    /// [`replicated_cells`](Self::replicated_cells) are how you tell which.
    pub(crate) fn decompose(&self, py: Python<'_>) -> PyResult<Option<Py<PyPanelDecomposition>>> {
        self.inner
            .decompose()
            .map(|fit| {
                Py::new(
                    py,
                    PyPanelDecomposition {
                        table: self.inner.clone(),
                        fit,
                    },
                )
            })
            .transpose()
    }

    pub(crate) fn __repr__(&self) -> String {
        format!(
            "ScoreTable(rows={}, members={}, populated={}, observations={})",
            self.inner.rows(),
            self.inner.members(),
            self.inner.populated(),
            self.inner.observations(),
        )
    }
}

/// A fitted two-way layout: the summary, and the two surfaces you act on.
///
/// Holds a copy of the table it was fitted from, so every reading below is a
/// plain attribute rather than a call you have to pass the table back into.
#[pyclass(name = "PanelDecomposition", module = "fugazi")]
pub(crate) struct PyPanelDecomposition {
    pub(crate) table: fugazi_core::spec::shrinkage::ScoreTable,
    pub(crate) fit: fugazi_core::spec::shrinkage::Decomposition,
}

#[pymethods]
impl PyPanelDecomposition {
    /// The headline reading — `disagreement` (λ), `parameter_matters`,
    /// `verdict`, and the variance components. Same class
    /// `Sweep.shrinkage` returns.
    #[getter]
    pub(crate) fn summary(&self, py: Python<'_>) -> PyResult<Py<PyPanelShrinkage>> {
        Py::new(
            py,
            PyPanelShrinkage {
                inner: self.fit.summary(&self.table),
            },
        )
    }

    /// Cell means with the **member level removed**, as `rows × members`.
    ///
    /// This is what to rank on when you want a cross-member spread to mean
    /// "this parameter set ranks consistently well" rather than "these members
    /// are alike": the member effect is identical for every row, so it carries
    /// no ranking information, yet it still inflates the spread — and
    /// unequally, since rows differ in which members they are defined on.
    ///
    /// `None` where the cell is unpopulated, so your own support counts stay
    /// honest. Needs no replication — only the table.
    #[getter]
    pub(crate) fn demeaned(&self) -> Vec<Vec<Option<Real>>> {
        Self::nest(self.fit.demeaned(&self.table), self.table.members())
    }

    /// The surface each member selects its own parameters off under partial
    /// pooling: `mu + alpha_r + lambda * gamma_rm`, as `rows × members`.
    ///
    /// At `lambda = 0` every column is identical and every member picks the
    /// pooled winner; at `lambda = 1` each gets its own cell means back. In
    /// between, a member whose column is noisy is pulled toward the consensus
    /// while one that genuinely disagrees keeps disagreeing. Take an argmax
    /// down each column.
    ///
    /// **`None` when `disagreement` is** — without a lambda there is no
    /// defensible surface, and falling back to either pooling extreme would be
    /// choosing a pooling policy by accident.
    #[getter]
    pub(crate) fn shrunk(&self) -> Option<Vec<Vec<Option<Real>>>> {
        self.fit
            .shrunk(&self.table)
            .map(|s| Self::nest(s, self.table.members()))
    }

    /// The shared parameter effect per row, as a deviation from the grand mean.
    /// `None` for a row with no populated cell.
    #[getter]
    pub(crate) fn row_effects(&self) -> Vec<Option<Real>> {
        self.fit.row_effects.clone()
    }

    /// The member level per member — the nuisance term `demeaned` removes.
    #[getter]
    pub(crate) fn member_effects(&self) -> Vec<Option<Real>> {
        self.fit.member_effects.clone()
    }

    /// What the additive part misses, as `rows × members` — the disagreement
    /// itself, before it is shrunk.
    #[getter]
    pub(crate) fn interactions(&self) -> Vec<Vec<Option<Real>>> {
        Self::nest(self.fit.interactions.clone(), self.table.members())
    }

    /// The grand mean the effects are deviations from.
    #[getter]
    pub(crate) fn grand_mean(&self) -> Real {
        self.fit.grand_mean
    }

    /// How many **independent searches over the grid** per-member selection
    /// amounts to, as `(effective, mean_correlation, members, pairs)`.
    ///
    /// Multiply your candidate count by `effective` before deflating: letting
    /// every member select for itself takes the maximum over more draws than
    /// the candidate count alone admits. `1.0` when the members agree (one
    /// shared surface, so one search), up to the member count when they share
    /// nothing.
    ///
    /// `None` alongside a `None` `shrunk` — with no surface there is nothing to
    /// correlate.
    #[getter]
    pub(crate) fn selection_breadth(&self) -> Option<PyPanelBreadth> {
        fugazi_core::spec::panel::selection_breadth(&self.fit, &self.table)
            .map(PyPanelBreadth::from_breadth)
    }

    pub(crate) fn __repr__(&self) -> String {
        let s = self.fit.summary(&self.table);
        let lambda = s
            .lambda
            .map_or_else(|| "None".to_string(), |l| format!("{l:.3}"));
        format!(
            "PanelDecomposition(disagreement={lambda}, cells={}, rows={}, members={})",
            s.cells, s.live_rows, s.live_members,
        )
    }
}

impl PyPanelDecomposition {
    /// Row-major flat vector to `rows × members` nesting.
    ///
    /// The Rust side is flat because it indexes hot loops; a Python caller
    /// wants `surface[row][member]` and should not be doing the arithmetic —
    /// getting that stride wrong is silent, not loud.
    fn nest(flat: Vec<Option<Real>>, members: usize) -> Vec<Vec<Option<Real>>> {
        if members == 0 {
            return Vec::new();
        }
        flat.chunks(members).map(<[_]>::to_vec).collect()
    }
}

/// How much of the spread between panel members is real disagreement rather
/// than backtest noise — the reading `shrink=` acts on.
///
/// A pooled sweep ranks one parameter set across every member. That is the
/// right thing to do only when the members *share* an optimum; when they do
/// not, the pooled winner is a compromise that can be worse on every member
/// than that member's own answer. This is the number that says which case you
/// are in.
///
/// The headline is [`disagreement`](Self::disagreement) — written `λ` in the
/// docs and the CSVs, and spelled out here because `lambda` is a Python
/// keyword and `sweep.shrinkage.lambda` would be a `SyntaxError`.
#[pyclass(name = "PanelShrinkage", module = "fugazi")]
pub(crate) struct PyPanelShrinkage {
    pub(crate) inner: fugazi_core::spec::shrinkage::Summary,
}

#[pymethods]
impl PyPanelShrinkage {
    /// `λ` in `0..=1`: the share of the spread between members that is genuine
    /// disagreement about the optimum rather than estimation noise.
    ///
    /// `0.0` — the members agree; pooling is buying variance reduction.
    /// `1.0` — they are separate problems; the pooled winner suits nobody.
    ///
    /// **`None` is not zero.** It means the table carried no within-cell
    /// replication, so disagreement and noise are literally the same sum of
    /// squares and no split exists to report — a different statement from "the
    /// members agree perfectly". Pass `windowed=` in a sweep to supply the
    /// replication; under `walkforward=` each fold splits its own in-sample
    /// window and needs no extra argument. Every other component below is
    /// still defined and still reported.
    #[getter]
    pub(crate) fn disagreement(&self) -> Option<Real> {
        self.inner.lambda
    }

    /// Whether the swept parameter moves this metric at all.
    ///
    /// Read it *with* [`disagreement`](Self::disagreement), never instead of
    /// it. `λ` compares disagreement against noise and says nothing about
    /// whether there was a signal to disagree over: on a grid that barely moves
    /// the metric, a high `λ` means the members disagree about which of several
    /// equivalent parameter sets is marginally best, which is not the finding
    /// it looks like. [`verdict`](Self::verdict) folds this in so the prose
    /// cannot be read without it.
    #[getter]
    pub(crate) fn parameter_matters(&self) -> bool {
        self.inner.parameter_matters()
    }

    /// The one-line reading, caveat included.
    ///
    /// Carries the same words the CLI prints, and appends the
    /// grid-barely-moves-this-metric warning when
    /// [`parameter_matters`](Self::parameter_matters) is false — so a caller
    /// who reports only this cannot report a misleading `λ`.
    #[getter]
    pub(crate) fn verdict(&self) -> String {
        let base = fugazi_core::spec::shrinkage::verdict(self.inner.lambda);
        if self.inner.lambda.is_some() && !self.inner.parameter_matters() {
            format!("{base} — but the grid barely moves this metric")
        } else {
            base.to_string()
        }
    }

    /// Replicated cells over populated cells, in `0..=1` — how much of the
    /// table actually backs `disagreement`. A `λ` resting on three cells of
    /// ninety and one resting on all ninety are not the same evidence.
    #[getter]
    pub(crate) fn support(&self) -> Real {
        self.inner.support
    }

    /// Populated `(row, member)` cells the fit rests on.
    #[getter]
    pub(crate) fn cells(&self) -> usize {
        self.inner.cells
    }

    /// Grid rows with at least one populated cell.
    #[getter]
    pub(crate) fn live_rows(&self) -> usize {
        self.inner.live_rows
    }

    /// Members with at least one populated cell.
    #[getter]
    pub(crate) fn live_members(&self) -> usize {
        self.inner.live_members
    }

    /// Variance of the shared parameter effect — how much the parameter moves
    /// the metric at all, before any member-specific structure.
    #[getter]
    pub(crate) fn row_variance(&self) -> Real {
        self.inner.row_variance
    }

    /// Variance of the per-member level. This is the nuisance term: identical
    /// for every row, so it carries no ranking information, which is why
    /// `shrink=` ranks on the member-demeaned score instead.
    #[getter]
    pub(crate) fn member_variance(&self) -> Real {
        self.inner.member_variance
    }

    /// `τ²_γ` — the parameter × member interaction, bias-corrected for the
    /// sampling noise its cell means carry and floored at zero.
    #[getter]
    pub(crate) fn interaction_variance(&self) -> Real {
        self.inner.interaction_variance
    }

    /// `σ²_ε` — pooled within-cell variance, or `None` on an unreplicated
    /// table where it cannot be told apart from the interaction.
    #[getter]
    pub(crate) fn residual_variance(&self) -> Option<Real> {
        self.inner.residual_variance
    }

    /// Harmonic mean replicate count over the replicated cells.
    #[getter]
    pub(crate) fn mean_replicates(&self) -> Real {
        self.inner.mean_replicates
    }

    /// Whether every live `(row, member)` pair was populated. An unbalanced
    /// table is fitted all the same, but its components are method-of-moments
    /// rather than exact.
    #[getter]
    pub(crate) fn balanced(&self) -> bool {
        self.inner.balanced
    }

    /// Both halves of the reading in one line, so a bare `print()` cannot show
    /// `λ` without its caveat.
    pub(crate) fn __repr__(&self) -> String {
        let lambda = self
            .inner
            .lambda
            .map_or_else(|| "None".to_string(), |l| format!("{l:.3}"));
        format!(
            "PanelShrinkage(disagreement={lambda}, support={:.2}, cells={}, \
             parameter_matters={}, verdict={:?})",
            self.inner.support,
            self.inner.cells,
            if self.inner.parameter_matters() {
                "True"
            } else {
                "False"
            },
            self.verdict(),
        )
    }
}

impl PyPanelShrinkage {
    fn wrap(
        py: Python<'_>,
        summary: Option<fugazi_core::spec::shrinkage::Summary>,
    ) -> PyResult<Option<Py<Self>>> {
        summary.map(|inner| Py::new(py, Self { inner })).transpose()
    }
}

/// One fold of a pooled walk-forward: the parameter set that won this fold on
/// the **pooled** in-sample score, and the per-member documents behind it.
///
/// `is_range` / `oos_range` are indices into the panel's shared clock — the
/// sorted union of every member's bar times — not into any one member's bars.
/// That is what makes fold *k* the same span for every member of a ragged
/// panel; see `PanelWalkForwardResult.axis_len`.
#[pyclass(name = "PanelFold", module = "fugazi")]
pub(crate) struct PyPanelFold {
    pub(crate) fold: usize,
    pub(crate) is_range: (usize, usize),
    pub(crate) oos_range: (usize, usize),
    pub(crate) axis_columns: Vec<String>,
    pub(crate) axis_values: Vec<Option<JsonValue>>,
    pub(crate) is_members: Vec<(String, SpecMetrics)>,
    pub(crate) oos_members: Vec<(String, SpecMetrics)>,
    pub(crate) is_smoothed: Option<Real>,
    pub(crate) is_support: Option<Real>,
    /// Under `shrink=`, this fold's own decomposition — estimated from
    /// sub-spans of *this fold's* in-sample window, so it rests only on data
    /// the fold could see.
    pub(crate) shrinkage: Option<fugazi_core::spec::shrinkage::Summary>,
    /// Under `shrink=`, `(member, axis values)` for each member's own pick.
    pub(crate) member_winners: Vec<(String, Vec<Option<JsonValue>>)>,
    /// Members whose pick differed from the pooled winner in this fold.
    pub(crate) departed: Vec<String>,
}

#[pymethods]
impl PyPanelFold {
    #[getter]
    pub(crate) fn fold(&self) -> usize {
        self.fold
    }
    /// In-sample range on the panel's shared clock, `(start, end)`.
    #[getter]
    pub(crate) fn is_range(&self) -> (usize, usize) {
        self.is_range
    }
    /// Post-embargo out-of-sample range on the panel's shared clock.
    #[getter]
    pub(crate) fn oos_range(&self) -> (usize, usize) {
        self.oos_range
    }
    /// The winning parameter set, as `{axis: value}`.
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
    /// Per-member in-sample documents for the winning row, keyed by member.
    ///
    /// Only members with bars in this fold's window appear. A member that had
    /// not listed yet is **absent**, never present-and-zero — which is what
    /// makes `len(fold.metrics_is)` a usable support count.
    #[getter]
    pub(crate) fn metrics_is(&self, py: Python<'_>) -> PyResult<Py<PyAny>> {
        members_to_py(py, &self.is_members)
    }
    /// Per-member out-of-sample documents for the winning row.
    #[getter]
    pub(crate) fn metrics_oos(&self, py: Python<'_>) -> PyResult<Py<PyAny>> {
        members_to_py(py, &self.oos_members)
    }
    /// Members with bars in this fold's in-sample window.
    #[getter]
    pub(crate) fn is_support_members(&self) -> usize {
        self.is_members.len()
    }
    /// Members with bars in this fold's out-of-sample window.
    #[getter]
    pub(crate) fn oos_support_members(&self) -> usize {
        self.oos_members.len()
    }
    /// Under `smooth=`, the neighbourhood average of the winning row's pooled
    /// IS ranking key — the value this fold was actually selected on.
    #[getter]
    pub(crate) fn is_smoothed(&self) -> Option<Real> {
        self.is_smoothed
    }
    /// The neighbourhood support behind `is_smoothed`.
    #[getter]
    pub(crate) fn is_support(&self) -> Option<Real> {
        self.is_support
    }

    /// Under `shrink=`, this fold's own [`PanelShrinkage`] — or `None` when the
    /// sweep was not shrunk, or the fold's in-sample window was too short to
    /// split into replicates.
    ///
    /// Per fold rather than once for the run, because a panel that agreed early
    /// and split later is a different story from one that never agreed, and a
    /// single number tells neither.
    ///
    /// **Deliberately conservative, and lower than the run-wide reading.** A
    /// fold estimates from sub-spans of its own in-sample window — which is
    /// what keeps it lookahead-free — but a metric measured over a short span
    /// is itself noisy, and that noise lands in the denominator. Per-fold
    /// `disagreement` of 0.275 / 0.0 / 0.0 against 0.815 for
    /// `PanelWalkForwardResult.shrinkage` is an ordinary spread, not a
    /// contradiction: it is the fold saying it cannot yet separate disagreement
    /// from noise on its own evidence. Label which is which if you render both;
    /// `docs/CLI.md` carries the longer version.
    #[getter]
    pub(crate) fn shrinkage(&self, py: Python<'_>) -> PyResult<Option<Py<PyPanelShrinkage>>> {
        PyPanelShrinkage::wrap(py, self.shrinkage)
    }

    /// Under `shrink=`, each member's own parameters for this fold as
    /// `{member: {axis: value}}` — the same shape as
    /// [`values`](Self::values), which is the pooled winner they are being
    /// compared against.
    ///
    /// Empty when the sweep was not shrunk. At `disagreement == 0` every entry
    /// equals `values`, which is complete pooling spelled out.
    #[getter]
    pub(crate) fn member_winners(&self, py: Python<'_>) -> PyResult<Py<pyo3::types::PyDict>> {
        winners_to_py(py, &self.axis_columns, &self.member_winners)
    }

    /// Members whose pick differed from the pooled winner in this fold.
    ///
    /// The useful half of [`member_winners`](Self::member_winners) when you
    /// only want to know *whether* the panel split and who split: "one member
    /// went its own way" and "every member did" are different findings that a
    /// mean `λ` renders identically.
    #[getter]
    pub(crate) fn departed(&self) -> Vec<String> {
        self.departed.clone()
    }

    pub(crate) fn __repr__(&self) -> String {
        format!(
            "PanelFold(fold={}, is={:?}, oos={:?}, members={}/{})",
            self.fold,
            self.is_range,
            self.oos_range,
            self.oos_members.len(),
            self.is_members.len(),
        )
    }
}

/// `{member: {axis: value}}` from per-member axis rows sparse across `columns`.
///
/// Shared by `Sweep.member_winners` and `PanelFold.member_winners` so the two
/// cannot drift into different shapes for the same idea — and shaped as a dict
/// of dicts rather than a list of records to match `PanelFold.values`, which is
/// the pooled winner a caller compares these against.
fn winners_to_py(
    py: Python<'_>,
    columns: &[String],
    winners: &[(String, Vec<Option<JsonValue>>)],
) -> PyResult<Py<pyo3::types::PyDict>> {
    let out = pyo3::types::PyDict::new(py);
    for (member, values) in winners {
        let d = pyo3::types::PyDict::new(py);
        for (name, v) in columns.iter().zip(values) {
            match v {
                Some(val) => d.set_item(name, json_to_py(py, val)?)?,
                None => d.set_item(name, py.None())?,
            }
        }
        out.set_item(member, d)?;
    }
    Ok(out.into())
}

fn members_to_py(py: Python<'_>, members: &[(String, SpecMetrics)]) -> PyResult<Py<PyAny>> {
    let d = pyo3::types::PyDict::new(py);
    for (name, m) in members {
        d.set_item(name, metrics_to_py(py, m)?)?;
    }
    Ok(d.into_any().unbind())
}

/// One panel member's stitched out-of-sample composite.
#[pyclass(name = "MemberComposite", module = "fugazi")]
pub(crate) struct PyMemberComposite {
    pub(crate) member: String,
    pub(crate) equity: Vec<Real>,
    pub(crate) fills: Vec<fugazi_core::Fill<Symbol>>,
    pub(crate) metrics: SpecMetrics,
}

#[pymethods]
impl PyMemberComposite {
    #[getter]
    pub(crate) fn member(&self) -> String {
        self.member.clone()
    }
    #[getter]
    pub(crate) fn equity(&self) -> Vec<Real> {
        self.equity.clone()
    }
    #[getter]
    pub(crate) fn fills(&self) -> Vec<PyFill> {
        self.fills
            .iter()
            .map(|f| PyFill { inner: f.clone() })
            .collect()
    }
    #[getter]
    pub(crate) fn metrics(&self, py: Python<'_>) -> PyResult<Py<PyAny>> {
        metrics_to_py(py, &self.metrics)
    }
    pub(crate) fn __repr__(&self) -> String {
        format!(
            "MemberComposite(member={:?}, bars={})",
            self.member,
            self.equity.len()
        )
    }
}

/// The result of a pooled walk-forward (`ta.optimize(..., panel=...,
/// walkforward=(is, oos))`).
///
/// One parameter set is chosen per fold on the **pooled** in-sample score and
/// applied out-of-sample to every member, so all the composites switch
/// parameters on the same dates.
///
/// There is deliberately no single netted composite curve: netting `M` members
/// into one account needs a weighting and a rebalance cadence, which is an
/// allocation policy fugazi expresses explicitly with `portfolio:` rather than
/// inventing inside `optimize`. Use `pooled(metric)` for the cross-member
/// headline, and `composites` for the per-instrument curves.
#[pyclass(name = "PanelWalkForwardResult", module = "fugazi")]
pub(crate) struct PyPanelWalkForwardResult {
    pub(crate) is_bars: usize,
    pub(crate) oos_bars: usize,
    pub(crate) embargo_bars: usize,
    pub(crate) prefix_skip: usize,
    pub(crate) axis_len: usize,
    pub(crate) members: Vec<String>,
    pub(crate) folds: Vec<Py<PyPanelFold>>,
    pub(crate) composites: Vec<Py<PyMemberComposite>>,
    pub(crate) composite_members: Vec<fugazi_core::spec::panel::PanelMetrics>,
    pub(crate) columns: Vec<String>,
    pub(crate) metric_columns: Vec<(String, String)>,
    pub(crate) cash: Real,
    /// `(effective, mean_correlation, members, pairs)`, computed once at
    /// construction — it is a scalar property of the finished panel rather than
    /// of any row, so recomputing it per access would re-correlate every pair
    /// to arrive at the same number.
    pub(crate) breadth: Option<(Real, Real, usize, usize)>,
    /// `(member, fold count)` for every member that departed at least once,
    /// most-frequent first — the order `PanelWalkForward::departures` sorts in.
    pub(crate) departures: Vec<(String, usize)>,
    /// The run-wide decomposition, folds as replicates.
    pub(crate) shrinkage: Option<fugazi_core::spec::shrinkage::Summary>,
}

#[pymethods]
impl PyPanelWalkForwardResult {
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
    /// Bars trimmed off the head of the shared clock for grid-wide readiness —
    /// the pooled analogue of `WalkForwardResult.prefix_skip`.
    ///
    /// Measured from the point the **first** member becomes ready, not the
    /// last: waiting for every member would truncate the panel's history to its
    /// most recent listing. Early folds therefore rest on fewer members, which
    /// `PanelFold.is_support_members` reports rather than hides.
    #[getter]
    pub(crate) fn prefix_skip(&self) -> usize {
        self.prefix_skip
    }
    /// Length of the panel's shared clock — the union of every member's bar
    /// times. Fold ranges index into this, not into any one member's bars.
    #[getter]
    pub(crate) fn axis_len(&self) -> usize {
        self.axis_len
    }
    /// The panel's member names, in order.
    #[getter]
    pub(crate) fn members(&self) -> Vec<String> {
        self.members.clone()
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
    pub(crate) fn cash(&self) -> Real {
        self.cash
    }
    /// How many *independent* members this panel's results are worth:
    /// `(effective, mean_correlation, members, pairs)`, or `None` when fewer
    /// than two members shared enough history to be correlated at all.
    ///
    /// A pooled row reports `N` hypotheses rather than `N x M`, which is the
    /// honest count — and it invites the reading that `M` members are `M`
    /// pieces of evidence. For a panel drawn from one market's worth of
    /// instruments they are not: at an average pairwise correlation of 0.8,
    /// thirty members are worth about 1.2, and a pooled Sharpe over them
    /// deserves roughly the confidence of a single backtest. The reading is
    /// `M / (1 + (M - 1) * rho_bar)`.
    ///
    /// Measured on the **composites' own returns**, not on the members' price
    /// series: what a pooled figure rests on is how much the results co-moved,
    /// and a strategy trading two correlated markets at different times earns
    /// more independence than their prices would suggest.
    ///
    /// Reported, never applied. What to do with it — deflate against it, widen
    /// an interval, or go and find less correlated members — is a decision the
    /// caller has the context to make and this crate does not.
    #[getter]
    pub(crate) fn effective_breadth(&self) -> Option<PyPanelBreadth> {
        self.breadth.map(
            |(effective, mean_correlation, members, pairs)| PyPanelBreadth {
                effective,
                mean_correlation,
                members,
                pairs,
            },
        )
    }

    /// Members that departed from the pooled winner at least once, and in how
    /// many folds — `{member: folds}`, most-frequent first.
    ///
    /// Empty when the run was not shrunk, and **also** when the panel agreed
    /// throughout. That second case is a real result, not an absence: complete
    /// pooling was already each member's own answer.
    ///
    /// This is the reading a run-level `λ` flattens. "One member went its own
    /// way in every fold" and "everyone drifted once" can produce the same mean
    /// disagreement and mean very different things.
    #[getter]
    pub(crate) fn departures(&self, py: Python<'_>) -> PyResult<Py<pyo3::types::PyDict>> {
        let d = pyo3::types::PyDict::new(py);
        for (member, folds) in &self.departures {
            d.set_item(member, folds)?;
        }
        Ok(d.into())
    }

    /// The panel's `λ` over the **whole run**, with folds as the replicate axis.
    ///
    /// Free, so reported without `shrink=`: every fold already measures every
    /// `(row, member)` in-sample to rank the grid.
    ///
    /// Deliberately *not* what any fold selected on — a component estimated
    /// over every fold and applied inside fold 1 would let fold 10's data pick
    /// fold 1's winner. Use `PanelFold.shrinkage` for the lookahead-free
    /// per-fold estimate each fold acted on; this one describes the run after
    /// the fact and is better powered.
    ///
    /// Better powered means it will read **higher** than the per-fold numbers,
    /// which rest on a handful of short sub-spans and are conservative as a
    /// result. Expect the two to differ; it is not a bug. See
    /// `PanelFold.shrinkage`.
    #[getter]
    pub(crate) fn shrinkage(&self, py: Python<'_>) -> PyResult<Option<Py<PyPanelShrinkage>>> {
        PyPanelShrinkage::wrap(py, self.shrinkage)
    }

    #[getter]
    pub(crate) fn folds(&self, py: Python<'_>) -> Vec<Py<PyPanelFold>> {
        self.folds.iter().map(|f| f.clone_ref(py)).collect()
    }
    /// One stitched out-of-sample composite per member, in panel order.
    #[getter]
    pub(crate) fn composites(&self, py: Python<'_>) -> Vec<Py<PyMemberComposite>> {
        self.composites.iter().map(|c| c.clone_ref(py)).collect()
    }

    /// Pool one metric across the per-member composites:
    /// `(mean, std, defined, members)`, or `None` when no member reported it.
    ///
    /// The mean is over the members that reported — a member with no trades has
    /// no win rate and is dropped rather than counted as zero — so `defined` is
    /// what separates a well-supported number from a mean over two survivors.
    pub(crate) fn pooled(&self, metric: &str) -> PyResult<Option<(Real, Real, usize, usize)>> {
        let sample = self
            .composite_members
            .first()
            .ok_or_else(|| PyValueError::new_err("pooled(): the panel has no members"))?;
        let (path, _) = fugazi_core::spec::metrics::resolve_metric(metric, &sample.metrics)
            .map_err(|e| PyValueError::new_err(format!("pooled(): {e:#}")))?;
        Ok(
            fugazi_core::spec::panel::pool_metric(&self.composite_members, &path)
                .map(|p| (p.mean, p.std, p.defined, p.members)),
        )
    }

    /// The number of folds — `len(result)` == `len(result.folds)`.
    pub(crate) fn __len__(&self) -> usize {
        self.folds.len()
    }
    /// Iterate the folds, so `for fold in result` needs no `.folds` detour.
    pub(crate) fn __iter__(&self, py: Python<'_>) -> PyResult<Py<PyAny>> {
        crate::classes::iter_over(py, self.folds(py))
    }
    /// Index or slice the folds — `result[0]`, `result[-1]`, `result[:2]`.
    pub(crate) fn __getitem__(
        &self,
        py: Python<'_>,
        index: &Bound<'_, PyAny>,
    ) -> PyResult<Py<PyAny>> {
        let list = pyo3::types::PyList::new(py, self.folds(py))?;
        Ok(list.as_any().get_item(index)?.unbind())
    }
    pub(crate) fn __repr__(&self) -> String {
        format!(
            "PanelWalkForwardResult(folds={}, members={}, is={}, oos={}, embargo={})",
            self.folds.len(),
            self.members.len(),
            self.is_bars,
            self.oos_bars,
            self.embargo_bars,
        )
    }
}

/// The pooled walk-forward driver — the `panel=` peer of [`run_walkforward`].
#[allow(clippy::too_many_arguments)]
pub(crate) fn run_panel_walkforward(
    py: Python<'_>,
    detected: &str,
    base_value: &JsonValue,
    members: &[PanelMember],
    // When set, each member's key is substituted for this `!param` before its
    // spec is built — as a JSON string for a `str` key, as a number for an
    // `int`/`float` one. Rooting every member on its own series is the usual
    // use; pooling over a numeric parameter is the other. The Python twin of
    // the CLI's `--pooled`.
    panel_axis: Option<&str>,
    cost_config: &fugazi_core::spec::costs::CostConfig,
    subgrids: Vec<spec_optimize::Subgrid>,
    is_bars: usize,
    oos_bars: usize,
    embargo_bars: usize,
    metric_names: &[String],
    best_by: Option<&str>,
    risk_aversion: Real,
    smoothing: Option<&spec_optimize::Smoothing>,
    // Partial pooling — see `fugazi_core::spec::shrinkage`. Each fold estimates
    // its own `λ` from sub-spans of its in-sample window and lets each member
    // depart from the pooled winner by that much.
    shrink: bool,
    jobs: Option<usize>,
    cash: Real,
    max_gross: Option<Real>,
    leverage: Real,
    margin_rate: Real,
    maintenance_margin: Option<Real>,
    bars_per_year: Real,
    risk_free_rate: Real,
    seconds_per_bar: Option<Real>,
) -> PyResult<Py<PyAny>> {
    use fugazi_core::spec::panel;

    // Each member's own bar clock, read off its snapshots. Refused here rather
    // than deep in the kernel so a stream with no `time` names the member.
    let axes: Vec<panel::MemberAxis> = members
        .iter()
        .map(|m| panel::MemberAxis::from_snapshots(&m.name, &m.snaps))
        .collect::<anyhow::Result<Vec<_>>>()
        .map_err(|e| SpecError::new_err(format!("pooled walkforward: {e:#}")))?;

    let interrupt = crate::classes::SweepInterrupt::new();
    let result = crate::classes::run_watched(
        py,
        &interrupt,
        || -> anyhow::Result<panel::PanelWalkForward> {
            let needs_probe_feed = matches!(detected, "basket" | "multi");
            let ctx = spec_backtest::EvalContext {
                cash,
                max_gross,
                leverage,
                margin_rate,
                maintenance_margin,
                bars_per_year,
                risk_free_rate,
                cost_config,
                effective_freq: None,
                stream: None,
                windowed: None,
                seconds_per_bar,
                mc: None,
                warmup_bars: None,
            };
            let ctx_ref = &ctx;
            // One schema per member: members are different instruments, so
            // their overlay columns need not agree.
            let schemas: Vec<_> = members
                .iter()
                .map(|m| spec_backtest::schema_from_snapshots(&m.snaps))
                .collect();

            let member_params = |params: &std::collections::HashMap<String, JsonValue>,
                                 m: usize|
             -> std::collections::HashMap<String, JsonValue> {
                let mut p = params.clone();
                if let Some(axis) = panel_axis {
                    p.insert(axis.to_string(), members[m].axis.clone());
                }
                p
            };
            let probe_readiness = |params: &std::collections::HashMap<String, JsonValue>,
                                   m: usize|
             -> anyhow::Result<usize> {
                let params = member_params(params, m);
                let value = fugazi_core::spec::params::substitute(base_value.clone(), &params)?;
                let spec = spec_from_value(value, detected)?;
                let mut built = spec
                    .try_build(cash, &schemas[m], None)
                    .map_err(spec_backtest::build_error)?;
                if needs_probe_feed && let Some(first) = members[m].snaps.first() {
                    built.update(first.clone());
                }
                Ok(built.stable_bars())
            };

            let run_backtest = |params: &std::collections::HashMap<String, JsonValue>,
                                m: usize|
             -> anyhow::Result<fugazi_core::RunReport<Symbol>> {
                if interrupt.should_stop() {
                    anyhow::bail!("interrupted");
                }
                let params = member_params(params, m);
                let value = fugazi_core::spec::params::substitute(base_value.clone(), &params)?;
                let spec = spec_from_value(value, detected)?;
                spec_backtest::check_member_universe_pub(
                    &spec,
                    &members[m].name,
                    &members[m].snaps,
                )
                .map_err(spec_backtest::build_error)?;
                spec_backtest::measured_report_any(&spec, &members[m].snaps, ctx_ref)
                    .map_err(spec_backtest::build_error)
            };

            panel::panel_walkforward(
                subgrids,
                axes,
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
                risk_aversion,
                smoothing,
                shrink,
                jobs,
                cash,
            )
        },
    )
    .map_err(|e| SpecError::new_err(format!("pooled walkforward: {e:#}")));
    let result = interrupt.raise_over(result)?;

    let columns = result.union_columns.clone();
    let metric_columns = result.metric_columns.clone();
    let mut fold_objs: Vec<Py<PyPanelFold>> = Vec::with_capacity(result.fold_rows.len());
    for row in &result.fold_rows {
        let to_pairs = |ms: &[panel::PanelMetrics]| -> Vec<(String, SpecMetrics)> {
            ms.iter()
                .map(|m| (m.member.clone(), m.metrics.clone()))
                .collect()
        };
        fold_objs.push(Py::new(
            py,
            PyPanelFold {
                fold: row.fold,
                is_range: (row.is.start, row.is.end),
                oos_range: (row.oos.start, row.oos.end),
                axis_columns: columns.clone(),
                axis_values: row.values.clone(),
                is_members: to_pairs(&row.is_members),
                oos_members: to_pairs(&row.oos_members),
                is_smoothed: row.is_smoothed.and_then(|s| s.value),
                is_support: row.is_smoothed.and_then(|s| s.support),
                shrinkage: row.shrinkage,
                member_winners: row
                    .member_winners
                    .iter()
                    .map(|w| (w.member.clone(), w.values.clone()))
                    .collect(),
                departed: row
                    .member_winners
                    .iter()
                    .filter(|w| w.departed)
                    .map(|w| w.member.clone())
                    .collect(),
            },
        )?);
    }
    let composite_members = result.composite_members();
    let mut composite_objs: Vec<Py<PyMemberComposite>> =
        Vec::with_capacity(result.composites.len());
    for c in &result.composites {
        composite_objs.push(Py::new(
            py,
            PyMemberComposite {
                member: c.member.clone(),
                equity: c.equity.clone(),
                fills: c.fills.clone(),
                metrics: c.metrics.clone(),
            },
        )?);
    }
    let py_result = Py::new(
        py,
        PyPanelWalkForwardResult {
            is_bars: result.is_bars,
            oos_bars: result.oos_bars,
            embargo_bars: result.embargo_bars,
            prefix_skip: result.axis.prefix_skip,
            axis_len: result.axis.len(),
            members: result.axis.members.iter().map(|m| m.name.clone()).collect(),
            folds: fold_objs,
            composites: composite_objs,
            composite_members,
            columns,
            metric_columns,
            cash: result.cash,
            breadth: result
                .effective_breadth()
                .map(|b| (b.effective, b.mean_correlation, b.members, b.pairs)),
            departures: result.departures(),
            shrinkage: result.run_shrinkage,
        },
    )?;
    Ok(py_result.into_any())
}
