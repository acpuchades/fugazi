//! Python bindings for the `fugazi` incremental technical-analysis library.
//!
//! The Rust library is built around "composition is construction": a
//! price-series indicator owns its input source and is generic over it, so an
//! "EMA of an SMA of the close" is just `Ema::new(Sma::new(Current::close(),
//! 10), 20)`. Those generics are monomorphised at compile time and cannot cross
//! the Python boundary directly, so this crate erases them behind
//! [`fugazi::runtime::Chain`](fugazi_core::runtime::Chain) — a
//! `Box<dyn DynIndicator<In, Out>>` that keeps the input and output types while
//! dropping the concrete one, so the Rust API's per-input, per-output typing
//! survives the boundary at no per-sample cost. `Source<I>` / `StrSource<I>` /
//! `AtomBox<I>` are output-specialised aliases of it; `SignalBox<I>` is the one
//! newtype, and only because it flattens a warming-up `None` to `false`. Only
//! [`MultiBox`] keeps its own trait, because its `Vec<Real>` +
//! `&'static [&'static str]` shape needs the line *names* alongside the values.
//!
//! Erasing this way rather than through the older payload vocabulary is what
//! makes the bindings fast: see the Phase 6 section of `docs/PERFORMANCE.md`,
//! and the note at the top of `carriers.rs` before changing it.
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

// The bindings are split by concern; `lib.rs` keeps only the module
// wiring and the `#[pymodule]` registration. Every module's items are
// re-exported here so cross-module references stay path-free, which is
// how they read when this was one file.
pub(crate) mod carriers;
pub(crate) mod errors;
#[macro_use]
pub(crate) mod macros;
pub(crate) mod classes;
// `#[macro_use]`: `over_prepared_wallet!` / `over_any_wallet!` live here and are
// consumed by `spec` below, which is declared after it.
#[macro_use]
pub(crate) mod strategy;
pub(crate) mod constructors;
pub(crate) mod metrics;
pub(crate) mod montecarlo;
pub(crate) mod prelude;
pub(crate) mod sources;
pub(crate) mod spec;

#[allow(unused_imports)]
use crate::carriers::*;
#[allow(unused_imports)]
use crate::classes::*;
#[allow(unused_imports)]
use crate::constructors::*;
#[allow(unused_imports)]
use crate::metrics::*;
#[allow(unused_imports)]
use crate::montecarlo::*;
use crate::prelude::*;
#[allow(unused_imports)]
use crate::sources::*;
#[allow(unused_imports)]
use crate::spec::*;
#[allow(unused_imports)]
use crate::strategy::*;
// `wrap_pyfunction!` resolves a hidden item pyo3 generates beside each
// `#[pyfunction]`, and a glob import doesn't carry it — so every registered
// function is named explicitly. The list doubles as the module's index.
use crate::constructors::{
    ad, adx, aroon, atr, bars_since, bars_since_high, bars_since_low, beta, bollinger, cci, close,
    compute_overlays, correlation, covariance, day, day_of_week, day_of_year, dmi, dollar_bars,
    donchian, ema, every, exp, garman_klass, get, get_bool, get_real, get_str, high, hma, hour,
    identity, if_else, is_weekday, is_weekend, keltner, kurtosis_indicator, latch, linreg, log,
    low, macd, median, mfi, minute, month, obv, open, parkinson, percentile, percentile_rank, pick,
    quarter, resample, rma, rogers_satchell, rsi, sar, second, skewness_indicator, sma, stddev,
    stoch_rsi, stochastic, true_range, typical, unix_millis, unix_seconds, unstable, value,
    value_str, variance_ratio, volume, volume_bars, vwap, week_of_year, williams_r, wma, year,
    zscore,
};
// Unpickling entry points. Not surface — but `__reduce__` names its callable by
// `module.qualname`, so each has to be a real, importable module member.
use crate::classes::{_rebuild_schema, _rebuild_snapshot};
use crate::sources::{fetch, tickers};
use crate::spec::{
    check_spec, load_spec, optimize, slot_demand, slot_demands, spec_document_json_schema,
    spec_grammar, spec_json_schema, spec_tags,
};
use crate::strategy::{_rebuild_order, _rebuild_run_report, _rebuild_size};
use crate::strategy::{
    buy_and_hold, calmar_of, donchian_breakout, everything, keltner_breakout, ma_crossover,
    max_drawdown_of, quantile, rsi_reversal, sharpe_of, sortino_of, threshold, top_bottom,
    volatility_of,
};

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
    m.add_class::<PyOkxWallet>()?;
    m.add_class::<PyCoinbaseWallet>()?;
    m.add_class::<PyKrakenWallet>()?;
    m.add_class::<PyOrder>()?;
    m.add_class::<PySize>()?;
    m.add_class::<PyStrategy>()?;
    m.add_class::<PyPairsStrategy>()?;
    m.add_class::<PyMultiAssetStrategy>()?;
    m.add_class::<PyPortfolio>()?;
    m.add_class::<PyBasketStrategy>()?;
    m.add_class::<PySelection>()?;
    m.add_class::<PyRunReport>()?;
    m.add_class::<PyRunState>()?;
    m.add_class::<PyRejected>()?;
    m.add_class::<PyFill>()?;
    m.add_class::<PyChildFill>()?;
    m.add_class::<PyAttribution>()?;
    m.add_class::<PyBinance>()?;
    m.add_class::<PyOkx>()?;
    m.add_class::<PyKraken>()?;
    m.add_class::<PyCoinbase>()?;
    m.add_class::<PyYahoo>()?;
    m.add_class::<PyCoinGecko>()?;
    m.add_class::<PyBinanceVision>()?;
    m.add_class::<PyBinanceFutures>()?;
    m.add_class::<PyCostConfig>()?;
    m.add_class::<PyMonteCarloConfig>()?;
    m.add_class::<PyStrategySpec>()?;
    m.add_class::<PySweep>()?;
    m.add_class::<PySweepRow>()?;
    m.add_class::<PyWalkForwardResult>()?;
    m.add_class::<PyWalkForwardFold>()?;
    m.add_class::<PyPanelWalkForwardResult>()?;
    m.add_class::<PyPanelFold>()?;
    m.add_class::<PyPanelShrinkage>()?;
    m.add_class::<PyPanelDecomposition>()?;
    m.add_class::<PyScoreTable>()?;
    m.add_class::<PyPanelBreadth>()?;
    m.add_class::<PyDemeanedScore>()?;
    m.add_class::<PyMemberComposite>()?;
    m.add_class::<PySpecCheck>()?;
    m.add_class::<PySpecHole>()?;

    // `fugazi.Wallet` — an ABC with the three concrete wallets registered as
    // virtual subclasses, so `isinstance(w, ta.Wallet)` works. See there.
    crate::strategy::register_wallet_protocol(m)?;

    // `fugazi.__version__`. Taken from `CARGO_PKG_VERSION` at compile time, so
    // it is `python/Cargo.toml`'s version — already one of the seven places a
    // release bump touches, which is why this adds no eighth. Reading it from
    // `importlib.metadata` instead would cost an import and only work for an
    // installed wheel, not a `maturin develop` tree.
    m.add("__version__", env!("CARGO_PKG_VERSION"))?;

    // The exception hierarchy (`errors.rs`). Added by explicit `get_type`
    // because `create_exception!` produces a plain Rust type, not a
    // `#[pyclass]` that `add_class` would pick up.
    m.add(
        "FugaziError",
        m.py().get_type::<crate::errors::FugaziError>(),
    )?;
    m.add("SpecError", m.py().get_type::<crate::errors::SpecError>())?;
    m.add(
        "WalletError",
        m.py().get_type::<crate::errors::WalletError>(),
    )?;
    m.add("FetchError", m.py().get_type::<crate::errors::FetchError>())?;

    // The default comparison tolerance is hybrid (absolute floor + relative
    // term), so it is exposed as its two components rather than one scalar. An
    // `epsilon=` argument stays absolute — a deadband the caller means literally.
    m.add("DEFAULT_TOLERANCE_ABS", DEFAULT_TOLERANCE.abs)?;
    m.add("DEFAULT_TOLERANCE_REL", DEFAULT_TOLERANCE.rel)?;

    // The one threshold `Order.is_materially_fitted` and
    // `RunReport.materially_fitted` apply, exposed so a caller reproducing the
    // predicate over its own fills gets the same answer the CLI banner does
    // rather than picking a second number.
    m.add("MATERIALLY_FITTED", fugazi_core::wallet::MATERIALLY_FITTED)?;

    macro_rules! reg {
        ($($f:ident),* $(,)?) => { $( m.add_function(wrap_pyfunction!($f, m)?)?; )* };
    }
    reg!(
        open,
        high,
        low,
        close,
        volume,
        typical,
        median,
        identity,
        value,
        value_str,
        sma,
        ema,
        rma,
        wma,
        hma,
        rsi,
        stddev,
        skewness_indicator,
        kurtosis_indicator,
        zscore,
        correlation,
        covariance,
        beta,
        percentile,
        percentile_rank,
        bars_since,
        bars_since_high,
        bars_since_low,
        variance_ratio,
        stochastic,
        cci,
        log,
        exp,
        atr,
        parkinson,
        garman_klass,
        rogers_satchell,
        mfi,
        williams_r,
        obv,
        vwap,
        ad,
        true_range,
        adx,
        dmi,
        aroon,
        sar,
        macd,
        bollinger,
        keltner,
        donchian,
        linreg,
        stoch_rsi,
        resample,
        volume_bars,
        dollar_bars,
        latch,
        unstable,
        if_else,
        get,
        get_real,
        get_bool,
        get_str,
        compute_overlays,
        year,
        month,
        day,
        hour,
        minute,
        second,
        day_of_week,
        day_of_year,
        week_of_year,
        quarter,
        unix_seconds,
        unix_millis,
        is_weekday,
        is_weekend,
        pick,
        everything,
        every,
        top_bottom,
        threshold,
        quantile,
        buy_and_hold,
        ma_crossover,
        rsi_reversal,
        donchian_breakout,
        keltner_breakout,
        sharpe_of,
        sortino_of,
        volatility_of,
        max_drawdown_of,
        calmar_of,
        fetch,
        tickers,
        load_spec,
        check_spec,
        optimize,
        slot_demand,
        slot_demands,
        spec_document_json_schema,
        spec_grammar,
        spec_json_schema,
        spec_tags,
        evaluate_report,
        // Unpickling entry points. These deliberately stay in the module's
        // generated `__all__`, underscore prefix and all: maturin's shim
        // populates the `fugazi` package with `from .fugazi import *`, which
        // honours `__all__` — so filtering them out for tidiness would take
        // them off the package and break every `__reduce__` that resolves
        // `fugazi._rebuild_*`. Exported is a requirement here, not an oversight.
        _rebuild_schema,
        _rebuild_snapshot,
        _rebuild_size,
        _rebuild_order,
        _rebuild_run_report,
    );

    // `fugazi.metrics` — mirror of `fugazi::metrics::*`. Registered as a
    // submodule *and* injected into `sys.modules` so `from fugazi.metrics
    // import sharpe` works (pyo3 submodules aren't visible to Python's import
    // machinery by default).
    // Named `fugazi.metrics`, not `metrics`: `PyModule::new` sets `__name__`
    // verbatim, and pickle resolves a function by `__module__` + import —
    // so a bare `metrics` makes every `__reduce__` pointing into this
    // submodule fail with "import of module 'metrics' failed".
    let metrics = PyModule::new(m.py(), "fugazi.metrics")?;
    register_metrics_module(&metrics)?;
    m.add_submodule(&metrics)?;
    m.py()
        .import("sys")?
        .getattr("modules")?
        .set_item("fugazi.metrics", &metrics)?;

    // `fugazi.montecarlo` — the deterministic resampling primitive behind the
    // significance layer, exposed so consumers can rebuild resampled paths (e.g.
    // an equity fan chart) themselves. Same submodule + `sys.modules` dance as
    // `fugazi.metrics` so `from fugazi.montecarlo import resample_index_matrix`
    // works.
    // Named `fugazi.montecarlo`, not `montecarlo`: `PyModule::new` sets `__name__`
    // verbatim, and pickle resolves a function by `__module__` + import —
    // so a bare `montecarlo` makes every `__reduce__` pointing into this
    // submodule fail with "import of module 'montecarlo' failed".
    let montecarlo = PyModule::new(m.py(), "fugazi.montecarlo")?;
    register_montecarlo_module(&montecarlo)?;
    m.add_submodule(&montecarlo)?;
    m.py()
        .import("sys")?
        .getattr("modules")?
        .set_item("fugazi.montecarlo", &montecarlo)?;

    Ok(())
}
