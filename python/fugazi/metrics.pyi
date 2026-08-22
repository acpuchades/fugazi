"""Type stubs for `fugazi.metrics`. GENERATED — see tools/gen_python_stubs.py."""

from collections.abc import Sequence
from typing import Any

from . import Fill as Fill

class DrawdownSegment:
    """One drawdown segment: a peak → trough → recovery-or-end stretch where the equity
    curve was below a prior peak. Built by
    [`drawdown_segments`](core_metrics::drawdown_segments). Frozen.
    """
    @property
    def depth_ratio(self) -> float: ...
    @property
    def duration_bars(self) -> int: ...
    @property
    def peak_bar(self) -> int: ...
    @property
    def trough_bar(self) -> int: ...
    @property
    def underwater_bars(self) -> int: ...

class Trade:
    """A closed round-trip trade reconstructed from the fill blotter by
    [`reconstruct_trades`](core_metrics::reconstruct_trades). Frozen; all fields are
    read-only.
    """
    @property
    def bars_held(self) -> int: ...
    @property
    def entry_bar(self) -> int: ...
    @property
    def entry_price(self) -> float: ...
    @property
    def exit_bar(self) -> int: ...
    @property
    def exit_price(self) -> float: ...
    @property
    def pnl(self) -> float: ...
    @property
    def return_ratio(self) -> float: ...
    @property
    def side(self) -> str: ...
    @property
    def units(self) -> float: ...
def annualized_return(returns: Sequence[float] | Any, bars_per_year: float) -> float:
    """Arithmetic mean of `returns` scaled by `bars_per_year`."""
    ...
def annualized_volatility(returns: Sequence[float] | Any, bars_per_year: float) -> float:
    """Sample stddev of `returns` scaled by `sqrt(bars_per_year)`."""
    ...
def average_bars_held(trades: Any) -> float:
    """Mean bars-held across trades. `None` on empty input."""
    ...
def average_drawdown(segments: Any) -> float:
    """Mean drawdown depth across all segments; `None` on empty input."""
    ...
def average_drawdown_duration(segments: Any) -> float:
    """Mean time spent below a prior peak, in bars, across all segments; `None` on
    empty input. Measures peak → recovery, matching `max_drawdown_duration` — not
    peak → trough.
    """
    ...
def average_loss(trades: Any) -> float:
    """Mean PnL across losing trades (a negative number). `None` when there are no
    losers.
    """
    ...
def average_trade_return(trades: Any) -> float:
    """Mean per-trade return as a fraction of the entry notional. `None` on empty
    input.
    """
    ...
def average_win(trades: Any) -> float:
    """Mean PnL across winning trades. `None` when there are no winners."""
    ...
def best_return(returns: Sequence[float] | Any) -> float:
    """Largest single-bar return, or `0.0` on empty input."""
    ...
def cagr(equity_curve: Sequence[float] | Any, initial_equity: float, bars_per_year: float) -> float:
    """Compound annual growth rate as a fraction. `None` when the equity path is non-
    positive at either endpoint, the run is empty, or `bars_per_year <= 0`.
    """
    ...
def calmar(equity_curve: Sequence[float] | Any, initial_equity: float, bars_per_year: float) -> float:
    """Calmar ratio: `cagr / max_drawdown`. `None` when either is undefined."""
    ...
def conditional_value_at_risk(returns: Sequence[float] | Any, confidence: Any) -> float:
    """Historical Conditional VaR (Expected Shortfall) at `confidence` as a positive
    loss fraction.
    """
    ...
def deflated_sharpe(returns: Sequence[float] | Any, risk_free_rate: float, bars_per_year: float, n_trials: Any, trial_sharpe_variance: Any) -> float:
    """Deflated Sharpe Ratio (Bailey & López de Prado, 2014): PSR against the
    selection-bias-adjusted benchmark `E[max SR]` across `n_trials` candidates.
    `trial_sharpe_variance` is the variance of the annualized Sharpe estimates
    across the trials. `None` when `n_trials < 2`, the trial variance is not
    strictly positive, or the underlying PSR is undefined.
    """
    ...
def deflated_sharpe_from_stats(sharpe_annualized: Any, skewness_biased: Any, excess_kurtosis: Any, n_returns: Any, bars_per_year: float, n_trials: Any, trial_sharpe_variance: Any) -> float:
    """The Deflated Sharpe Ratio computed from pre-aggregated statistics — the stats-
    only twin of `deflated_sharpe`. `None` when `n_trials < 2`, the trial variance
    is not strictly positive, or the underlying PSR is undefined.
    """
    ...
def drawdown_count(segments: Any) -> float:
    """Number of drawdown segments."""
    ...
def drawdown_segments(equity_curve: Sequence[float] | Any) -> list[DrawdownSegment]:
    """Build the drawdown segments of `equity_curve` — one entry per peak → trough →
    recovery-or-end stretch. A monotone-non-decreasing curve produces an empty list.
    """
    ...
def expectancy(trades: Any) -> float:
    """Mean PnL per trade. `None` on empty input."""
    ...
def expected_max_sharpe(n_trials: Any, trial_sharpe_variance: Any) -> float:
    """The expected **maximum** annualized Sharpe under a normal null across `n_trials`
    independent trials — the selection-bias-adjusted benchmark `deflated_sharpe`
    measures against, read out on its own: "the best of your 200 trials would be
    expected to score 1.21 by luck alone".
    """
    ...
def exposure_ratio(fills: Sequence[Fill] | None, total_bars: Any) -> float:
    """Fraction of bars during which the wallet held a non-zero position. `0.0` when
    `total_bars == 0`.
    """
    ...
def flat_trades(trades: Any) -> float:
    """Count of trades with exactly zero PnL."""
    ...
def kelly_fraction(trades: Any) -> float:
    """Kelly-optimal fraction of bankroll per trade under the current win rate and
    payoff ratio (`p − (1 − p)/b`). Can be negative. `None` when either input is
    undefined or the payoff ratio is non-positive.
    """
    ...
def kurtosis(returns: Sequence[float] | Any) -> float:
    """Biased excess kurtosis `g2 = m4 / m2^2 − 3`. Matches
    `scipy.stats.kurtosis(bias=True, fisher=True)`. `None` when the second moment is
    zero.
    """
    ...
def largest_loss(trades: Any) -> float:
    """Most-negative single-trade PnL. `None` on empty input."""
    ...
def largest_win(trades: Any) -> float:
    """Largest single-trade PnL. `None` on empty input."""
    ...
def long_trades(trades: Any) -> float:
    """Count of trades entered on the long side."""
    ...
def losing_trades(trades: Any) -> float:
    """Count of trades with strictly negative PnL."""
    ...
def max_bars_held(trades: Any) -> float:
    """Longest bars-held across trades. `None` on empty input."""
    ...
def max_consecutive_losses(trades: Any) -> float:
    """Longest consecutive run of losing trades. `0` on empty input."""
    ...
def max_consecutive_wins(trades: Any) -> float:
    """Longest consecutive run of winning trades. `0` on empty input."""
    ...
def max_drawdown(segments: Any) -> float:
    """Deepest drawdown in `segments`, as a fraction. `0.0` on empty input."""
    ...
def max_drawdown_duration(segments: Any) -> float:
    """The **longest** time spent below a prior peak, in bars — the worst recovery
    wait, independent of the depth of the drawdown that caused it. `0` on empty
    input.
    """
    ...
def mean_return(returns: Sequence[float] | Any) -> float:
    """Arithmetic mean of `returns`. `0.0` on empty input."""
    ...
def median_return(returns: Sequence[float] | Any) -> float:
    """Median of `returns`. `0.0` on empty input; the mean of the two middle values on
    even-length input.
    """
    ...
def min_bars_held(trades: Any) -> float:
    """Shortest bars-held across trades. `None` on empty input."""
    ...
def omega(returns: Sequence[float] | Any, threshold: Any) -> float:
    """Omega ratio at `threshold`. For an annualized rf comparison, pass the per-bar
    rate (`rf / bars_per_year`) as `threshold`. `None` when every return clears the
    threshold (no downside).
    """
    ...
def payoff_ratio(trades: Any) -> float:
    """`average_win / |average_loss|`. `None` when either input is undefined."""
    ...
def per_bar_returns(equity_curve: Sequence[float] | Any, initial_equity: float) -> list[float]:
    """Per-bar fractional return series: `(equity[i] - prev) / prev`, seeded from
    `initial_equity`. Zero-denominator bars contribute `0.0`. The returned list has
    the same length as `equity_curve`.
    """
    ...
def positive_bars_ratio(returns: Sequence[float] | Any) -> float:
    """Fraction of bars with a strictly positive return. `0.0` on empty input."""
    ...
def probabilistic_sharpe(returns: Sequence[float] | Any, risk_free_rate: float, bars_per_year: float, benchmark_sharpe: Any) -> float:
    """Probabilistic Sharpe Ratio (Bailey & López de Prado, 2012): probability that the
    true Sharpe of the return-generating process exceeds `benchmark_sharpe`
    (annualized), given the observed Sharpe over `returns` and the empirical
    skewness + kurtosis. `None` when the underlying Sharpe / skew / kurtosis is
    undefined.
    """
    ...
def probabilistic_sharpe_from_stats(sharpe_annualized: Any, skewness_biased: Any, excess_kurtosis: Any, n_returns: Any, bars_per_year: float, benchmark_sharpe: Any) -> float:
    """The Probabilistic Sharpe Ratio computed from pre-aggregated statistics — use
    when the Sharpe / skew / kurtosis are already known (e.g. a summary row from a
    grid) and re-scanning the returns vector would be wasted work. `None` propagates
    from any `None` input.
    """
    ...
def profit_factor(trades: Any) -> float:
    """`Σ winning_pnl / |Σ losing_pnl|`. `None` when there are no losing trades."""
    ...
def reconstruct_trades(fills: Sequence[Fill] | None) -> list[Trade]:
    """Walk `fills` **per symbol**, each with its own signed position and a volume-
    weighted entry price, producing one `Trade` per closed leg. A reversal fill
    closes the current leg and reopens the remainder at the same fill price as a
    fresh trade.
    """
    ...
def recovery_factor(equity_curve: Sequence[float] | Any, initial_equity: float) -> float:
    """`total_return / max_drawdown` — the non-annualized cousin of Calmar. `None` when
    the max drawdown is zero.
    """
    ...
def sharpe(returns: Sequence[float] | Any, risk_free_rate: float, bars_per_year: float) -> float:
    """Annualized Sharpe ratio. `risk_free_rate` is the annualized rf as a fraction.
    `None` when the annualized volatility is zero.
    """
    ...
def short_trades(trades: Any) -> float:
    """Count of trades entered on the short side."""
    ...
def skewness(returns: Sequence[float] | Any) -> float:
    """Biased (population) skewness `g1 = m3 / m2^(3/2)`. Matches
    `scipy.stats.skew(bias=True)`. `None` when the second moment is zero.
    """
    ...
def sortino(returns: Sequence[float] | Any, risk_free_rate: float, bars_per_year: float) -> float:
    """Annualized Sortino ratio (downside deviation, `n` divisor). `None` when every
    bar clears the threshold or `returns` is empty.
    """
    ...
def stddev_return(returns: Sequence[float] | Any) -> float:
    """Sample (Bessel-corrected, `ddof=1`) standard deviation of `returns`. `0.0` on
    empty or single-sample input.
    """
    ...
def tail_ratio(returns: Sequence[float] | Any) -> float:
    """`|P95| / |P5|` — a coarse symmetry check on the tails. `None` when the
    P5-magnitude is zero.
    """
    ...
def time_in_drawdown_ratio(segments: Any, total_bars: Any) -> float:
    """Fraction of bars spent below a prior peak. `0.0` when `total_bars == 0`."""
    ...
def total_return(equity_curve: Sequence[float] | Any, initial_equity: float) -> float:
    """Total return as a fraction: `(final - initial) / initial`. `0.0` when the
    initial equity is zero.
    """
    ...
def total_trades(trades: Any) -> float:
    """Count of closed round-trip trades."""
    ...
def ulcer_index(equity_curve: Sequence[float] | Any) -> float:
    """Peter Martin's Ulcer Index, in fractional form. `0.0` on a monotone- non-
    decreasing curve.
    """
    ...
def ulcer_performance_index(equity_curve: Sequence[float] | Any, initial_equity: float, risk_free_rate: float, bars_per_year: float) -> float:
    """Ulcer Performance Index: `(cagr − risk_free_rate) / ulcer_index`. `None` when
    either input is degenerate.
    """
    ...
def value_at_risk(returns: Sequence[float] | Any, confidence: Any) -> float:
    """Historical VaR at `confidence` (e.g. `0.95`) as a positive loss fraction."""
    ...
def win_rate(trades: Any) -> float:
    """Fraction of trades with strictly positive PnL. `None` on empty input."""
    ...
def winning_trades(trades: Any) -> float:
    """Count of trades with strictly positive PnL."""
    ...
def worst_return(returns: Sequence[float] | Any) -> float:
    """Smallest single-bar return, or `0.0` on empty input."""
    ...
