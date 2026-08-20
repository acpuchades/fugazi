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
use crate::spec::*;

// ---------------------------------------------------------------------------
// Metrics: mirror `fugazi::metrics::*` as the `fugazi.metrics` submodule
//
// One `#[pyfunction]` per library metric, plus lightweight pyclasses over the
// public `Fill`, `Trade`, `DrawdownSegment` intermediates. Ratios that return
// `Option<Real>` in Rust map to `Optional[float]` in Python (`None` on the
// degenerate case); metrics that always return a `Real` map to plain `float`.
// Bar counts stay `int` (`usize` in Rust). Values are natural units — `0.15`
// is +15%, not `15.0`.
// ---------------------------------------------------------------------------

/// A bar-tagged order: an [`Order`] paired with the bar index at which it
/// filled. `PaperWallet.update()` returns bare `Order`s (no bar); a user
/// driving the loop tags each with its bar index to build the list that
/// `metrics.reconstruct_trades` / `metrics.exposure_ratio` consume:
///
/// ```python
/// fills = []
/// for i, candle in enumerate(candles):
///     for order in wallet.update("BTC", candle):
///         fills.append(fugazi.Fill(bar=i, order=order))
/// ```
#[pyclass(name = "Fill", module = "fugazi", frozen, from_py_object)]
#[derive(Clone)]
pub(crate) struct PyFill {
    pub(crate) inner: Fill<Symbol>,
}

#[pymethods]
impl PyFill {
    #[new]
    pub(crate) fn new(bar: usize, order: &PyOrder) -> Self {
        PyFill {
            inner: Fill {
                bar,
                order: order.inner.clone(),
            },
        }
    }

    /// The bar index at which this order filled.
    #[getter]
    pub(crate) fn bar(&self) -> usize {
        self.inner.bar
    }

    /// The filled [`Order`].
    #[getter]
    pub(crate) fn order(&self) -> PyOrder {
        PyOrder {
            inner: self.inner.order.clone(),
        }
    }

    pub(crate) fn __reduce__(&self, py: Python<'_>) -> PyResult<Py<PyAny>> {
        crate::classes::reduce_with(py, py.get_type::<PyFill>(), (self.bar(), self.order()))
    }

    pub(crate) fn __repr__(&self) -> String {
        format!(
            "Fill(bar={}, order=Order(symbol='{}', side='{}', units={}, price={}, kind='{}'))",
            self.inner.bar,
            self.inner.order.symbol,
            side_str(self.inner.order.side),
            self.inner.order.units,
            self.inner.order.price,
            kind_str(self.inner.order.kind),
        )
    }
}

/// A closed round-trip trade reconstructed from the fill blotter by
/// [`reconstruct_trades`](core_metrics::reconstruct_trades). Frozen; all fields
/// are read-only.
#[pyclass(name = "Trade", module = "fugazi.metrics", frozen, from_py_object)]
#[derive(Clone, Copy)]
pub(crate) struct PyTrade {
    pub(crate) inner: Trade,
}

#[pymethods]
impl PyTrade {
    /// Bar index at which the leg was opened (or last re-averaged).
    #[getter]
    pub(crate) fn entry_bar(&self) -> usize {
        self.inner.entry_bar
    }
    /// Bar index at which the leg was closed.
    #[getter]
    pub(crate) fn exit_bar(&self) -> usize {
        self.inner.exit_bar
    }
    /// `"buy"` (long) or `"sell"` (short).
    #[getter]
    pub(crate) fn side(&self) -> &'static str {
        side_str(self.inner.side)
    }
    /// The magnitude of the closed leg, in instrument units.
    #[getter]
    pub(crate) fn units(&self) -> f64 {
        self.inner.units
    }
    /// Volume-weighted average price of the opening leg.
    #[getter]
    pub(crate) fn entry_price(&self) -> f64 {
        self.inner.entry_price
    }
    /// Fill price of the closing leg.
    #[getter]
    pub(crate) fn exit_price(&self) -> f64 {
        self.inner.exit_price
    }
    /// Realized PnL in reference (quote) currency.
    #[getter]
    pub(crate) fn pnl(&self) -> f64 {
        self.inner.pnl
    }
    /// PnL as a fraction of the entry notional (`pnl / (entry_price * units)`).
    #[getter]
    pub(crate) fn return_ratio(&self) -> f64 {
        self.inner.return_ratio
    }
    /// Bar count from entry to exit — `exit_bar - entry_bar`.
    #[getter]
    pub(crate) fn bars_held(&self) -> usize {
        self.inner.bars_held()
    }

    pub(crate) fn __reduce__(&self, py: Python<'_>) -> PyResult<Py<PyAny>> {
        crate::classes::reduce_with(
            py,
            py.import("fugazi.metrics")?.getattr("_rebuild_trade")?,
            (
                self.entry_bar(),
                self.exit_bar(),
                self.side(),
                self.units(),
                self.entry_price(),
                self.exit_price(),
                self.pnl(),
                self.return_ratio(),
            ),
        )
    }

    pub(crate) fn __repr__(&self) -> String {
        format!(
            "Trade(entry_bar={}, exit_bar={}, side='{}', units={}, entry_price={}, \
             exit_price={}, pnl={}, return_ratio={})",
            self.inner.entry_bar,
            self.inner.exit_bar,
            side_str(self.inner.side),
            self.inner.units,
            self.inner.entry_price,
            self.inner.exit_price,
            self.inner.pnl,
            self.inner.return_ratio,
        )
    }
}

/// One drawdown segment: a peak → trough → recovery-or-end stretch where the
/// equity curve was below a prior peak. Built by
/// [`drawdown_segments`](core_metrics::drawdown_segments). Frozen.
#[pyclass(name = "DrawdownSegment", module = "fugazi.metrics", frozen, from_py_object)]
#[derive(Clone, Copy)]
pub(crate) struct PyDrawdownSegment {
    pub(crate) inner: DrawdownSegment,
}

#[pymethods]
impl PyDrawdownSegment {
    /// Bar index of the pre-drawdown peak.
    #[getter]
    pub(crate) fn peak_bar(&self) -> usize {
        self.inner.peak_bar
    }
    /// Bar index of the deepest point.
    #[getter]
    pub(crate) fn trough_bar(&self) -> usize {
        self.inner.trough_bar
    }
    /// `(peak - trough) / peak`, in fractional form; always non-negative.
    #[getter]
    pub(crate) fn depth_ratio(&self) -> f64 {
        self.inner.depth_ratio
    }
    /// Peak-to-trough distance in bars.
    #[getter]
    pub(crate) fn duration_bars(&self) -> usize {
        self.inner.duration_bars
    }
    /// Bars strictly below the peak in this segment.
    #[getter]
    pub(crate) fn underwater_bars(&self) -> usize {
        self.inner.underwater_bars
    }

    pub(crate) fn __reduce__(&self, py: Python<'_>) -> PyResult<Py<PyAny>> {
        crate::classes::reduce_with(
            py,
            py.import("fugazi.metrics")?.getattr("_rebuild_drawdown_segment")?,
            (
                self.peak_bar(),
                self.trough_bar(),
                self.depth_ratio(),
                self.duration_bars(),
                self.underwater_bars(),
            ),
        )
    }

    pub(crate) fn __repr__(&self) -> String {
        format!(
            "DrawdownSegment(peak_bar={}, trough_bar={}, depth_ratio={}, \
             duration_bars={}, underwater_bars={})",
            self.inner.peak_bar,
            self.inner.trough_bar,
            self.inner.depth_ratio,
            self.inner.duration_bars,
            self.inner.underwater_bars,
        )
    }
}

// -- Intermediate builders --------------------------------------------------

/// Per-bar fractional return series: `(equity[i] - prev) / prev`, seeded from
/// `initial_equity`. Zero-denominator bars contribute `0.0`. The returned list
/// has the same length as `equity_curve`.
#[pyfunction]
pub(crate) fn per_bar_returns(equity_curve: Series, initial_equity: Real) -> Vec<Real> {
    core_metrics::per_bar_returns(&equity_curve, initial_equity)
}

/// Walk `fills` **per symbol**, each with its own signed position and a
/// volume-weighted entry price, producing one `Trade` per closed leg. A
/// reversal fill closes the current leg and reopens the remainder at the same
/// fill price as a fresh trade.
///
/// Legs never cross instruments: an opposite-side fill in a different symbol
/// opens its own leg rather than closing this one. Trades come back in the
/// order they closed.
#[pyfunction]
pub(crate) fn reconstruct_trades(fills: Vec<PyFill>) -> Vec<PyTrade> {
    let native: Vec<Fill<Symbol>> = fills.into_iter().map(|f| f.inner).collect();
    core_metrics::reconstruct_trades(&native)
        .into_iter()
        .map(|inner| PyTrade { inner })
        .collect()
}

/// Build the drawdown segments of `equity_curve` — one entry per peak →
/// trough → recovery-or-end stretch. A monotone-non-decreasing curve produces
/// an empty list.
#[pyfunction]
pub(crate) fn drawdown_segments(equity_curve: Series) -> Vec<PyDrawdownSegment> {
    core_metrics::drawdown_segments(&equity_curve)
        .into_iter()
        .map(|inner| PyDrawdownSegment { inner })
        .collect()
}

// -- Return moments and distribution shape ----------------------------------

/// Arithmetic mean of `returns`. `0.0` on empty input.
#[pyfunction]
pub(crate) fn mean_return(returns: Series) -> Real {
    core_metrics::mean_return(&returns)
}

/// Median of `returns`. `0.0` on empty input; the mean of the two middle
/// values on even-length input.
#[pyfunction]
pub(crate) fn median_return(returns: Series) -> Real {
    core_metrics::median_return(&returns)
}

/// Sample (Bessel-corrected, `ddof=1`) standard deviation of `returns`. `0.0`
/// on empty or single-sample input.
#[pyfunction]
pub(crate) fn stddev_return(returns: Series) -> Real {
    core_metrics::stddev_return(&returns)
}

/// Largest single-bar return, or `0.0` on empty input.
#[pyfunction]
pub(crate) fn best_return(returns: Series) -> Real {
    core_metrics::best_return(&returns)
}

/// Smallest single-bar return, or `0.0` on empty input.
#[pyfunction]
pub(crate) fn worst_return(returns: Series) -> Real {
    core_metrics::worst_return(&returns)
}

/// Fraction of bars with a strictly positive return. `0.0` on empty input.
#[pyfunction]
pub(crate) fn positive_bars_ratio(returns: Series) -> Real {
    core_metrics::positive_bars_ratio(&returns)
}

/// Biased (population) skewness `g1 = m3 / m2^(3/2)`. Matches
/// `scipy.stats.skew(bias=True)`. `None` when the second moment is zero.
#[pyfunction]
pub(crate) fn skewness(returns: Series) -> Option<Real> {
    core_metrics::skewness(&returns)
}

/// Biased excess kurtosis `g2 = m4 / m2^2 − 3`. Matches
/// `scipy.stats.kurtosis(bias=True, fisher=True)`. `None` when the second
/// moment is zero.
#[pyfunction]
pub(crate) fn kurtosis(returns: Series) -> Option<Real> {
    core_metrics::kurtosis(&returns)
}

/// Historical VaR at `confidence` (e.g. `0.95`) as a positive loss fraction.
#[pyfunction]
pub(crate) fn value_at_risk(returns: Series, confidence: Real) -> Real {
    core_metrics::value_at_risk(&returns, confidence)
}

/// Historical Conditional VaR (Expected Shortfall) at `confidence` as a
/// positive loss fraction.
#[pyfunction]
pub(crate) fn conditional_value_at_risk(returns: Series, confidence: Real) -> Real {
    core_metrics::conditional_value_at_risk(&returns, confidence)
}

/// `|P95| / |P5|` — a coarse symmetry check on the tails. `None` when the
/// P5-magnitude is zero.
#[pyfunction]
pub(crate) fn tail_ratio(returns: Series) -> Option<Real> {
    core_metrics::tail_ratio(&returns)
}

// -- Compound-return metrics ------------------------------------------------

/// Total return as a fraction: `(final - initial) / initial`. `0.0` when the
/// initial equity is zero.
#[pyfunction]
pub(crate) fn total_return(equity_curve: Series, initial_equity: Real) -> Real {
    core_metrics::total_return(&equity_curve, initial_equity)
}

/// Compound annual growth rate as a fraction. `None` when the equity path is
/// non-positive at either endpoint, the run is empty, or `bars_per_year <= 0`.
#[pyfunction]
pub(crate) fn cagr(equity_curve: Series, initial_equity: Real, bars_per_year: Real) -> Option<Real> {
    core_metrics::cagr(&equity_curve, initial_equity, bars_per_year)
}

/// Arithmetic mean of `returns` scaled by `bars_per_year`.
#[pyfunction]
pub(crate) fn annualized_return(returns: Series, bars_per_year: Real) -> Real {
    core_metrics::annualized_return(&returns, bars_per_year)
}

/// Sample stddev of `returns` scaled by `sqrt(bars_per_year)`.
#[pyfunction]
pub(crate) fn annualized_volatility(returns: Series, bars_per_year: Real) -> Real {
    core_metrics::annualized_volatility(&returns, bars_per_year)
}

// -- Risk-adjusted ratios ---------------------------------------------------

/// Annualized Sharpe ratio. `risk_free_rate` is the annualized rf as a
/// fraction. `None` when the annualized volatility is zero.
#[pyfunction]
pub(crate) fn sharpe(returns: Series, risk_free_rate: Real, bars_per_year: Real) -> Option<Real> {
    core_metrics::sharpe(&returns, risk_free_rate, bars_per_year)
}

/// Annualized Sortino ratio (downside deviation, `n` divisor). `None` when
/// every bar clears the threshold or `returns` is empty.
#[pyfunction]
pub(crate) fn sortino(returns: Series, risk_free_rate: Real, bars_per_year: Real) -> Option<Real> {
    core_metrics::sortino(&returns, risk_free_rate, bars_per_year)
}

/// Calmar ratio: `cagr / max_drawdown`. `None` when either is undefined.
#[pyfunction]
pub(crate) fn calmar(equity_curve: Series, initial_equity: Real, bars_per_year: Real) -> Option<Real> {
    core_metrics::calmar(&equity_curve, initial_equity, bars_per_year)
}

/// Omega ratio at `threshold`. For an annualized rf comparison, pass the
/// per-bar rate (`rf / bars_per_year`) as `threshold`. `None` when every
/// return clears the threshold (no downside).
#[pyfunction]
pub(crate) fn omega(returns: Series, threshold: Real) -> Option<Real> {
    core_metrics::omega(&returns, threshold)
}

/// Peter Martin's Ulcer Index, in fractional form. `0.0` on a monotone-
/// non-decreasing curve.
#[pyfunction]
pub(crate) fn ulcer_index(equity_curve: Series) -> Real {
    core_metrics::ulcer_index(&equity_curve)
}

/// Ulcer Performance Index: `(cagr − risk_free_rate) / ulcer_index`. `None`
/// when either input is degenerate.
#[pyfunction]
pub(crate) fn ulcer_performance_index(
    equity_curve: Series,
    initial_equity: Real,
    risk_free_rate: Real,
    bars_per_year: Real,
) -> Option<Real> {
    core_metrics::ulcer_performance_index(
        &equity_curve,
        initial_equity,
        risk_free_rate,
        bars_per_year,
    )
}

// -- Higher-moment / multiple-testing Sharpe corrections --------------------

/// Probabilistic Sharpe Ratio (Bailey & López de Prado, 2012): probability
/// that the true Sharpe of the return-generating process exceeds
/// `benchmark_sharpe` (annualized), given the observed Sharpe over `returns`
/// and the empirical skewness + kurtosis. `None` when the underlying Sharpe /
/// skew / kurtosis is undefined.
#[pyfunction]
pub(crate) fn probabilistic_sharpe(
    returns: Series,
    risk_free_rate: Real,
    bars_per_year: Real,
    benchmark_sharpe: Real,
) -> Option<Real> {
    core_metrics::probabilistic_sharpe(&returns, risk_free_rate, bars_per_year, benchmark_sharpe)
}

/// The Probabilistic Sharpe Ratio computed from pre-aggregated statistics —
/// use when the Sharpe / skew / kurtosis are already known (e.g. a summary
/// row from a grid) and re-scanning the returns vector would be wasted work.
/// `None` propagates from any `None` input.
#[pyfunction]
pub(crate) fn probabilistic_sharpe_from_stats(
    sharpe_annualized: Option<Real>,
    skewness_biased: Option<Real>,
    excess_kurtosis: Option<Real>,
    n_returns: usize,
    bars_per_year: Real,
    benchmark_sharpe: Real,
) -> Option<Real> {
    core_metrics::probabilistic_sharpe_from_stats(
        sharpe_annualized,
        skewness_biased,
        excess_kurtosis,
        n_returns,
        bars_per_year,
        benchmark_sharpe,
    )
}

/// Deflated Sharpe Ratio (Bailey & López de Prado, 2014): PSR against the
/// selection-bias-adjusted benchmark `E[max SR]` across `n_trials` candidates.
/// `trial_sharpe_variance` is the variance of the annualized Sharpe estimates
/// across the trials. `None` when `n_trials < 2`, the trial variance is not
/// strictly positive, or the underlying PSR is undefined.
#[pyfunction]
pub(crate) fn deflated_sharpe(
    returns: Series,
    risk_free_rate: Real,
    bars_per_year: Real,
    n_trials: usize,
    trial_sharpe_variance: Real,
) -> Option<Real> {
    core_metrics::deflated_sharpe(
        &returns,
        risk_free_rate,
        bars_per_year,
        n_trials,
        trial_sharpe_variance,
    )
}

/// The Deflated Sharpe Ratio computed from pre-aggregated statistics — the
/// stats-only twin of `deflated_sharpe`. `None` when `n_trials < 2`, the trial
/// variance is not strictly positive, or the underlying PSR is undefined.
#[pyfunction]
#[allow(clippy::too_many_arguments)]
pub(crate) fn deflated_sharpe_from_stats(
    sharpe_annualized: Option<Real>,
    skewness_biased: Option<Real>,
    excess_kurtosis: Option<Real>,
    n_returns: usize,
    bars_per_year: Real,
    n_trials: usize,
    trial_sharpe_variance: Real,
) -> Option<Real> {
    core_metrics::deflated_sharpe_from_stats(
        sharpe_annualized,
        skewness_biased,
        excess_kurtosis,
        n_returns,
        bars_per_year,
        n_trials,
        trial_sharpe_variance,
    )
}

// -- Drawdown metrics -------------------------------------------------------

/// Deepest drawdown in `segments`, as a fraction. `0.0` on empty input.
#[pyfunction]
pub(crate) fn max_drawdown(segments: Vec<PyDrawdownSegment>) -> Real {
    let native: Vec<DrawdownSegment> = segments.iter().map(|s| s.inner).collect();
    core_metrics::max_drawdown(&native)
}

/// The **longest** time spent below a prior peak, in bars — the worst recovery
/// wait, independent of the depth of the drawdown that caused it. `0` on empty
/// input.
#[pyfunction]
pub(crate) fn max_drawdown_duration(segments: Vec<PyDrawdownSegment>) -> usize {
    let native: Vec<DrawdownSegment> = segments.iter().map(|s| s.inner).collect();
    core_metrics::max_drawdown_duration(&native)
}

/// Mean drawdown depth across all segments; `None` on empty input.
#[pyfunction]
pub(crate) fn average_drawdown(segments: Vec<PyDrawdownSegment>) -> Option<Real> {
    let native: Vec<DrawdownSegment> = segments.iter().map(|s| s.inner).collect();
    core_metrics::average_drawdown(&native)
}

/// Mean time spent below a prior peak, in bars, across all segments; `None` on
/// empty input. Measures peak → recovery, matching `max_drawdown_duration` —
/// not peak → trough.
#[pyfunction]
pub(crate) fn average_drawdown_duration(segments: Vec<PyDrawdownSegment>) -> Option<Real> {
    let native: Vec<DrawdownSegment> = segments.iter().map(|s| s.inner).collect();
    core_metrics::average_drawdown_duration(&native)
}

/// Number of drawdown segments.
#[pyfunction]
pub(crate) fn drawdown_count(segments: Vec<PyDrawdownSegment>) -> usize {
    let native: Vec<DrawdownSegment> = segments.iter().map(|s| s.inner).collect();
    core_metrics::drawdown_count(&native)
}

/// Fraction of bars spent below a prior peak. `0.0` when `total_bars == 0`.
#[pyfunction]
pub(crate) fn time_in_drawdown_ratio(segments: Vec<PyDrawdownSegment>, total_bars: usize) -> Real {
    let native: Vec<DrawdownSegment> = segments.iter().map(|s| s.inner).collect();
    core_metrics::time_in_drawdown_ratio(&native, total_bars)
}

/// `total_return / max_drawdown` — the non-annualized cousin of Calmar.
/// `None` when the max drawdown is zero.
#[pyfunction]
pub(crate) fn recovery_factor(equity_curve: Series, initial_equity: Real) -> Option<Real> {
    core_metrics::recovery_factor(&equity_curve, initial_equity)
}

// -- Trade metrics ----------------------------------------------------------

pub(crate) fn to_native_trades(trades: Vec<PyTrade>) -> Vec<Trade> {
    trades.into_iter().map(|t| t.inner).collect()
}

/// Count of closed round-trip trades.
#[pyfunction]
pub(crate) fn total_trades(trades: Vec<PyTrade>) -> usize {
    core_metrics::total_trades(&to_native_trades(trades))
}

/// Count of trades with strictly positive PnL.
#[pyfunction]
pub(crate) fn winning_trades(trades: Vec<PyTrade>) -> usize {
    core_metrics::winning_trades(&to_native_trades(trades))
}

/// Count of trades with strictly negative PnL.
#[pyfunction]
pub(crate) fn losing_trades(trades: Vec<PyTrade>) -> usize {
    core_metrics::losing_trades(&to_native_trades(trades))
}

/// Count of trades with exactly zero PnL.
#[pyfunction]
pub(crate) fn flat_trades(trades: Vec<PyTrade>) -> usize {
    core_metrics::flat_trades(&to_native_trades(trades))
}

/// Count of trades entered on the long side.
#[pyfunction]
pub(crate) fn long_trades(trades: Vec<PyTrade>) -> usize {
    core_metrics::long_trades(&to_native_trades(trades))
}

/// Count of trades entered on the short side.
#[pyfunction]
pub(crate) fn short_trades(trades: Vec<PyTrade>) -> usize {
    core_metrics::short_trades(&to_native_trades(trades))
}

/// Longest consecutive run of winning trades. `0` on empty input.
#[pyfunction]
pub(crate) fn max_consecutive_wins(trades: Vec<PyTrade>) -> usize {
    core_metrics::max_consecutive_wins(&to_native_trades(trades))
}

/// Longest consecutive run of losing trades. `0` on empty input.
#[pyfunction]
pub(crate) fn max_consecutive_losses(trades: Vec<PyTrade>) -> usize {
    core_metrics::max_consecutive_losses(&to_native_trades(trades))
}

/// Fraction of trades with strictly positive PnL. `None` on empty input.
#[pyfunction]
pub(crate) fn win_rate(trades: Vec<PyTrade>) -> Option<Real> {
    core_metrics::win_rate(&to_native_trades(trades))
}

/// `Σ winning_pnl / |Σ losing_pnl|`. `None` when there are no losing trades.
#[pyfunction]
pub(crate) fn profit_factor(trades: Vec<PyTrade>) -> Option<Real> {
    core_metrics::profit_factor(&to_native_trades(trades))
}

/// `average_win / |average_loss|`. `None` when either input is undefined.
#[pyfunction]
pub(crate) fn payoff_ratio(trades: Vec<PyTrade>) -> Option<Real> {
    core_metrics::payoff_ratio(&to_native_trades(trades))
}

/// Mean PnL per trade. `None` on empty input.
#[pyfunction]
pub(crate) fn expectancy(trades: Vec<PyTrade>) -> Option<Real> {
    core_metrics::expectancy(&to_native_trades(trades))
}

/// Kelly-optimal fraction of bankroll per trade under the current win rate
/// and payoff ratio (`p − (1 − p)/b`). Can be negative. `None` when either
/// input is undefined or the payoff ratio is non-positive.
#[pyfunction]
pub(crate) fn kelly_fraction(trades: Vec<PyTrade>) -> Option<Real> {
    core_metrics::kelly_fraction(&to_native_trades(trades))
}

/// Mean PnL across winning trades. `None` when there are no winners.
#[pyfunction]
pub(crate) fn average_win(trades: Vec<PyTrade>) -> Option<Real> {
    core_metrics::average_win(&to_native_trades(trades))
}

/// Mean PnL across losing trades (a negative number). `None` when there are
/// no losers.
#[pyfunction]
pub(crate) fn average_loss(trades: Vec<PyTrade>) -> Option<Real> {
    core_metrics::average_loss(&to_native_trades(trades))
}

/// Largest single-trade PnL. `None` on empty input.
#[pyfunction]
pub(crate) fn largest_win(trades: Vec<PyTrade>) -> Option<Real> {
    core_metrics::largest_win(&to_native_trades(trades))
}

/// Most-negative single-trade PnL. `None` on empty input.
#[pyfunction]
pub(crate) fn largest_loss(trades: Vec<PyTrade>) -> Option<Real> {
    core_metrics::largest_loss(&to_native_trades(trades))
}

/// Mean per-trade return as a fraction of the entry notional. `None` on empty
/// input.
#[pyfunction]
pub(crate) fn average_trade_return(trades: Vec<PyTrade>) -> Option<Real> {
    core_metrics::average_trade_return(&to_native_trades(trades))
}

/// Mean bars-held across trades. `None` on empty input.
#[pyfunction]
pub(crate) fn average_bars_held(trades: Vec<PyTrade>) -> Option<Real> {
    core_metrics::average_bars_held(&to_native_trades(trades))
}

/// Shortest bars-held across trades. `None` on empty input.
#[pyfunction]
pub(crate) fn min_bars_held(trades: Vec<PyTrade>) -> Option<usize> {
    core_metrics::min_bars_held(&to_native_trades(trades))
}

/// Longest bars-held across trades. `None` on empty input.
#[pyfunction]
pub(crate) fn max_bars_held(trades: Vec<PyTrade>) -> Option<usize> {
    core_metrics::max_bars_held(&to_native_trades(trades))
}

/// Fraction of bars during which the wallet held a non-zero position. `0.0`
/// when `total_bars == 0`.
#[pyfunction]
pub(crate) fn exposure_ratio(fills: Vec<PyFill>, total_bars: usize) -> Real {
    let native: Vec<Fill<Symbol>> = fills.into_iter().map(|f| f.inner).collect();
    core_metrics::exposure_ratio(&native, total_bars)
}

// ---------------------------------------------------------------------------
// Unpickling entry points — see the note in `classes.rs`.
//
// `Trade` and `DrawdownSegment` are pure results: the library computes them and
// Python has no constructor for either. These give pickle something importable
// to call, without opening a public constructor for a type nobody should be
// hand-building.
// ---------------------------------------------------------------------------

/// Rebuild a [`Trade`](PyTrade) from its eight fields.
#[pyfunction]
#[allow(clippy::too_many_arguments)]
pub(crate) fn _rebuild_trade(
    entry_bar: usize,
    exit_bar: usize,
    side: &str,
    units: f64,
    entry_price: f64,
    exit_price: f64,
    pnl: f64,
    return_ratio: f64,
) -> PyResult<PyTrade> {
    Ok(PyTrade {
        inner: Trade {
            entry_bar,
            exit_bar,
            side: parse_side(side)?,
            units,
            entry_price,
            exit_price,
            pnl,
            return_ratio,
        },
    })
}

/// Rebuild a [`DrawdownSegment`](PyDrawdownSegment) from its five fields.
#[pyfunction]
pub(crate) fn _rebuild_drawdown_segment(
    peak_bar: usize,
    trough_bar: usize,
    depth_ratio: f64,
    duration_bars: usize,
    underwater_bars: usize,
) -> PyDrawdownSegment {
    PyDrawdownSegment {
        inner: DrawdownSegment {
            peak_bar,
            trough_bar,
            depth_ratio,
            duration_bars,
            underwater_bars,
        },
    }
}

/// Register every metric function on the `fugazi.metrics` submodule.
pub(crate) fn register_metrics_module(m: &Bound<'_, PyModule>) -> PyResult<()> {
    // Mirrors the *Closed system* note on the Rust `metrics` module — the one
    // assumption a caller can violate without any function here complaining.
    m.setattr(
        "__doc__",
        "Standalone performance metrics, one function per metric.\n\n\
         Every function assumes the equity curve is a **closed system**: that all\n\
         of its movement is P&L, and that no value entered or left the account\n\
         from outside. That holds for a backtest by construction. It does not\n\
         hold for an account a human can pay into, and the failure is silent — a\n\
         withdrawal is shaped exactly like a trading loss in an equity curve, and\n\
         a deposit exactly like a gain, so an unrecorded flow corrupts\n\
         total_return, cagr, sharpe, sortino and max_drawdown alike.\n\n\
         Tracking flows is portfolio accounting, which this module deliberately\n\
         does not do. A caller whose account takes external flows must neutralize\n\
         them first — the standard treatment is a chain-linked time-weighted\n\
         return, r_i = (E_i - F_i) / E_{i-1} - 1 for a flow F_i landing in period\n\
         i, yielding a flow-neutral curve these functions then reduce correctly.",
    )?;

    m.add_class::<PyTrade>()?;
    m.add_class::<PyDrawdownSegment>()?;

    macro_rules! reg {
        ($($f:ident),* $(,)?) => { $( m.add_function(wrap_pyfunction!($f, m)?)?; )* };
    }
    reg!(
        _rebuild_trade,
        _rebuild_drawdown_segment,
        per_bar_returns,
        reconstruct_trades,
        drawdown_segments,
        mean_return,
        median_return,
        stddev_return,
        best_return,
        worst_return,
        positive_bars_ratio,
        skewness,
        kurtosis,
        value_at_risk,
        conditional_value_at_risk,
        tail_ratio,
        total_return,
        cagr,
        annualized_return,
        annualized_volatility,
        sharpe,
        sortino,
        calmar,
        omega,
        ulcer_index,
        ulcer_performance_index,
        probabilistic_sharpe,
        probabilistic_sharpe_from_stats,
        deflated_sharpe,
        deflated_sharpe_from_stats,
        max_drawdown,
        max_drawdown_duration,
        average_drawdown,
        average_drawdown_duration,
        drawdown_count,
        time_in_drawdown_ratio,
        recovery_factor,
        total_trades,
        winning_trades,
        losing_trades,
        flat_trades,
        long_trades,
        short_trades,
        max_consecutive_wins,
        max_consecutive_losses,
        win_rate,
        profit_factor,
        payoff_ratio,
        expectancy,
        kelly_fraction,
        average_win,
        average_loss,
        largest_win,
        largest_loss,
        average_trade_return,
        average_bars_held,
        min_bars_held,
        max_bars_held,
        exposure_ratio,
    );
    Ok(())
}

