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
#[macro_use]
pub(crate) mod macros;
pub(crate) mod classes;
// `#[macro_use]`: `over_prepared_wallet!` / `over_any_wallet!` live here and are
// consumed by `spec` below, which is declared after it.
#[macro_use]
pub(crate) mod strategy;
pub(crate) mod constructors;
pub(crate) mod sources;
pub(crate) mod metrics;
pub(crate) mod montecarlo;
pub(crate) mod spec;
pub(crate) mod prelude;


use crate::prelude::*;
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
#[allow(unused_imports)]
use crate::montecarlo::*;
#[allow(unused_imports)]
use crate::spec::*;
// `wrap_pyfunction!` resolves a hidden item pyo3 generates beside each
// `#[pyfunction]`, and a glob import doesn't carry it — so every registered
// function is named explicitly. The list doubles as the module's index.
use crate::constructors::{
    open, high, low, close, volume, typical,
    median, identity, value, value_str, sma, ema,
    rma, wma, hma, rsi, stddev, skewness_indicator,
    kurtosis_indicator, zscore, correlation, percentile, percentile_rank, bars_since,
    bars_since_high, bars_since_low, variance_ratio, stochastic, cci, log,
    atr, parkinson, garman_klass, rogers_satchell, mfi, williams_r,
    obv, vwap, ad, true_range, adx, dmi,
    aroon, sar, macd, bollinger, keltner, donchian,
    stoch_rsi, resample, latch, unstable, if_else, get,
    get_real, get_bool, get_str, compute_overlays, str_eq, str_ne,
    year, month, day, hour, minute, second,
    day_of_week, day_of_year, week_of_year, quarter, unix_seconds, unix_millis,
    is_weekday, is_weekend, every, pick,
};
use crate::strategy::{
    everything, top_bottom, threshold, quantile, buy_and_hold, ma_crossover,
    rsi_reversal, donchian_breakout, keltner_breakout, sharpe_of, sortino_of, volatility_of,
    max_drawdown_of, calmar_of,
};
use crate::sources::{
    fetch,
};
use crate::spec::{
    load_spec, optimize, spec_document_json_schema, spec_grammar, spec_json_schema, spec_tags,
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
    m.add_class::<PyOrder>()?;
    m.add_class::<PySize>()?;
    m.add_class::<PyStrategy>()?;
    m.add_class::<PyPairsStrategy>()?;
    m.add_class::<PyMultiAssetStrategy>()?;
    m.add_class::<PyPortfolio>()?;
    m.add_class::<PyBasketStrategy>()?;
    m.add_class::<PySelection>()?;
    m.add_class::<PyRunReport>()?;
    m.add_class::<PyRejected>()?;
    m.add_class::<PyFill>()?;
    m.add_class::<PyBinance>()?;
    m.add_class::<PyOkx>()?;
    m.add_class::<PyCoinbase>()?;
    m.add_class::<PyYahoo>()?;
    m.add_class::<PyCoinGecko>()?;
    m.add_class::<PyBinanceVision>()?;
    m.add_class::<PyCostConfig>()?;
    m.add_class::<PyMonteCarloConfig>()?;
    m.add_class::<PyStrategySpec>()?;
    m.add_class::<PySweep>()?;
    m.add_class::<PySweepRow>()?;
    m.add_class::<PyWalkForwardResult>()?;
    m.add_class::<PyWalkForwardFold>()?;

    // The default comparison tolerance is hybrid (absolute floor + relative
    // term), so it is exposed as its two components rather than one scalar. An
    // `epsilon=` argument stays absolute — a deadband the caller means literally.
    m.add("DEFAULT_TOLERANCE_ABS", DEFAULT_TOLERANCE.abs)?;
    m.add("DEFAULT_TOLERANCE_REL", DEFAULT_TOLERANCE.rel)?;

    macro_rules! reg {
        ($($f:ident),* $(,)?) => { $( m.add_function(wrap_pyfunction!($f, m)?)?; )* };
    }
    reg!(
        _bench_feed_stage,
        _bench_feed_built,
        open, high, low, close, volume, typical,
        median, identity, value, value_str, sma, ema,
        rma, wma, hma, rsi, stddev, skewness_indicator,
        kurtosis_indicator, zscore, correlation, percentile, percentile_rank, bars_since,
        bars_since_high, bars_since_low, variance_ratio, stochastic, cci, log,
        atr, parkinson, garman_klass, rogers_satchell, mfi, williams_r,
        obv, vwap, ad, true_range, adx, dmi,
        aroon, sar, macd, bollinger, keltner, donchian,
        stoch_rsi, resample, latch, unstable, if_else, get,
        get_real, get_bool, get_str, compute_overlays, str_eq, str_ne,
        year, month, day, hour, minute, second,
        day_of_week, day_of_year, week_of_year, quarter, unix_seconds, unix_millis,
        is_weekday, is_weekend, pick, everything, every, top_bottom, threshold,
        quantile, buy_and_hold, ma_crossover, rsi_reversal, donchian_breakout, keltner_breakout,
        sharpe_of, sortino_of, volatility_of, max_drawdown_of, calmar_of, fetch,
        load_spec, optimize, spec_document_json_schema, spec_grammar, spec_json_schema,
        spec_tags, evaluate_report,
    );

    // `fugazi.metrics` — mirror of `fugazi::metrics::*`. Registered as a
    // submodule *and* injected into `sys.modules` so `from fugazi.metrics
    // import sharpe` works (pyo3 submodules aren't visible to Python's import
    // machinery by default).
    let metrics = PyModule::new(m.py(), "metrics")?;
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
    let montecarlo = PyModule::new(m.py(), "montecarlo")?;
    register_montecarlo_module(&montecarlo)?;
    m.add_submodule(&montecarlo)?;
    m.py()
        .import("sys")?
        .getattr("modules")?
        .set_item("fugazi.montecarlo", &montecarlo)?;

    Ok(())
}
