# Metrics

Every `fugazi` subcommand that measures a run produces the same catalogue of
metrics, grouped into six sections. This document lists each metric, states
what it means, and calls out the caveats that shape how it should be read.

## Where metrics show up

- **`metrics.yml`** (`fugazi run`) — the whole-run summary. Degenerate
  (undefined) entries are *omitted* from the YAML rather than emitted as
  `NaN` / `Infinity`, so the file stays a clean scalar map. Read a missing
  key as "not defined for this run" (usually a divide-by-zero).
- **`metrics.csv`** (`fugazi run -w LEN`) — one row per non-overlapping
  window. All columns are always present; a degenerate cell is empty.
- **`rolling.csv`** (`fugazi run -w LEN`) — same columns as `metrics.csv`
  *except* the trailing `selection.deflated_sharpe`, one row per stride-1
  window (heavy autocorrelation, meant for plotting a curve).
- **Grid CSV** (`fugazi optimize -o …`) — one row per parameter combination;
  columns are `axes… + metrics… (+ selection.deflated_sharpe)`. Under
  `-w LEN`, each requested metric becomes a `<name>_mean` / `<name>_std`
  pair aggregated across the row's own non-overlapping windows.

Everything downstream — CSVs, `optimize`'s ranking, `run -w`'s per-window
CSV — flattens the same `Metrics` document. Metric names in this file use
the canonical dotted path (`risk_adjusted.sharpe`), which is also what
`optimize`'s `-m` / `--best-by` accept.

## Unit conventions

- **Fractions in the library, percent in the document.** `fugazi::metrics`
  returns fractions (`0.15` = +15%). The CLI's `Metrics` document scales
  those to percent at the presentation boundary — so `returns.total` is
  fractional, `returns.total_pct` is 15.0. Both are emitted; pick the one
  your downstream tool expects.
- **Annualized figures depend on `bars_per_year`.** `bars_per_year` is
  resolved from the trading calendar (`--stocks` / `--forex` / `--crypto`)
  and the bar cadence — override via `--bars-per-year`. Annualized numbers
  are meaningful only when the bar cadence matches that resolution.
- **`risk_free_rate` is an annualized fraction** (`0.045` = 4.5% p.a.).
  Sharpe/Sortino/UPI subtract it from the annualized mean; Omega uses the
  per-bar rate as its threshold. Calmar is a raw ratio and does not.
- **Degenerate → `None`.** Any ratio whose denominator can vanish is typed
  `Option<Real>`; the CLI writes an empty cell for a `None`.

## `run.*` — bookkeeping

Not really metrics — echoed at the top of `metrics.yml` so a numbers-only
reader can identify the run.

| Column | Meaning |
|---|---|
| `run.bars` | Number of bars in the run (or window). |
| `run.initial_equity` | Equity at the start of the run (or window). |
| `run.final_equity` | Equity at the last bar (or window). |
| `run.bars_per_year` | Annualization factor threaded through Sharpe / annualized_* / CAGR. |
| `run.risk_free_rate` | Annualized rf fraction used by Sharpe / Sortino / UPI / Omega. |

## `returns.*` — per-bar return distribution

Computed on `per_bar_returns(equity, initial)` — one return per bar, seeded
from `initial_equity`.

| Column | Meaning | Notes |
|---|---|---|
| `returns.total` / `_pct` | End-to-end return over the whole run/window. | Always defined. |
| `returns.cagr_pct` | Compound annual growth rate. | `None` when the equity path is non-positive. |
| `returns.mean_bar` / `median_bar` / `stddev_bar` | Per-bar central moments. | Always defined. |
| `returns.best_bar` / `worst_bar` | Extremes of per-bar returns. | Always defined. |
| `returns.positive_bars_pct` | Share of bars with a strictly positive return. | |
| `returns.skewness` / `kurtosis` | Sample skew / excess kurtosis. | `None` when stddev is zero. Kurtosis is excess (normal = 0). |
| `returns.var_95` / `cvar_95` | Historical 5% VaR / CVaR (Expected Shortfall). | Expressed as a **positive loss fraction** (0.02 = "5% worst case is a −2% return"). Negative when the 5th percentile is itself positive. |
| `returns.tail_ratio` | `|P95| / |P5|`. | `None` when the 5th-percentile magnitude is zero. |
| `returns.annualized_mean_pct` / `annualized_volatility_pct` | Mean × `bars_per_year`; stddev × √`bars_per_year`. | Only meaningful when the bar cadence matches `bars_per_year`. |

## `risk_adjusted.*` — return-per-unit-of-risk ratios

| Column | Meaning | Notes |
|---|---|---|
| `risk_adjusted.sharpe` | `(annualized_return − rf) / annualized_volatility`. | `None` when volatility is zero. |
| `risk_adjusted.sortino` | Same numerator, denominator uses downside deviation with per-bar rf as MAR (n-divisor, matches empyrical). | `None` when no bar dips below the threshold. |
| `risk_adjusted.calmar` | `cagr / max_drawdown`. | `None` when max drawdown is zero or CAGR is undefined. Does *not* subtract rf. |
| `risk_adjusted.omega` | `Σmax(r − τ, 0) / Σmax(τ − r, 0)` at τ = per-bar rf. | `None` when every return clears τ. |
| `risk_adjusted.ulcer_index` | RMS drawdown (fractional). | Always defined (0 for a monotone-non-decreasing curve). |
| `risk_adjusted.ulcer_performance_index` | `(CAGR − rf) / ulcer_index`. | `None` when UI or CAGR is degenerate. |
| `risk_adjusted.probabilistic_sharpe` | See caveat below. | |

### PSR — Probabilistic Sharpe Ratio (Bailey & López de Prado, 2012)

The probability that the *true* per-bar Sharpe of the return-generating
process exceeds a benchmark, given the observed Sharpe, skewness, and
kurtosis of the sample. A probability in `[0, 1]`. `None` when the
underlying Sharpe / skew / kurt is undefined.

- **Single-trial statistic.** PSR corrects a single Sharpe estimate for the
  higher moments of *this* run's return distribution. It does NOT correct
  for having inspected many strategies / windows.
- **Benchmark = 0** in the CLI (the probability that the true Sharpe > 0).
- **In `run -w`**, each window's PSR is against *that window's own*
  returns — still single-trial, still against a zero-Sharpe benchmark.

## `drawdown.*` — peak-to-trough analytics

Computed from `drawdown_segments(equity)` — one segment per drop
(peak → trough → recovery-or-end).

| Column | Meaning | Notes |
|---|---|---|
| `drawdown.max` / `max_pct` | Deepest single drawdown. | Always defined. |
| `drawdown.max_duration_bars` | Peak-to-trough duration of the deepest drop. | Always defined. |
| `drawdown.avg` / `avg_pct` | Mean depth across all segments. | `None` for a monotone-non-decreasing equity curve. |
| `drawdown.avg_duration_bars` | Mean peak-to-trough duration across segments. | `None` when there are no segments. |
| `drawdown.count` | Number of drawdown segments. | |
| `drawdown.time_in_drawdown_pct` | Fraction of bars strictly below the running peak. | |
| `drawdown.recovery_factor` | `total_return / max_drawdown`. | Non-annualized cousin of Calmar. `None` when max DD is zero. |

## `trades.*` — round-trip trade statistics

Trades are reconstructed from the fill blotter by walking a single signed
position with a volume-weighted entry; each closed leg is one trade.

| Column | Meaning | Notes |
|---|---|---|
| `trades.total` / `wins` / `losses` / `flat` | Trade counts. | |
| `trades.long_trades` / `short_trades` | Split by direction at entry. | |
| `trades.total_fills` | Number of fills booked (usually 2 × trades minus reversals). | |
| `trades.max_consecutive_wins` / `max_consecutive_losses` | Longest run. | |
| `trades.exposure_pct` | Percentage of bars the wallet held a non-zero position. | Derived from the fill blotter. |
| `trades.win_rate_pct` | Wins ÷ total. | `None` when there are no trades. |
| `trades.profit_factor` | Σ wins ÷ Σ |losses|. | `None` when there are no losing trades. |
| `trades.payoff_ratio` | `average_win / |average_loss|`. | `None` when either side is empty. |
| `trades.expectancy` | Mean per-trade PnL. | `None` when there are no trades. |
| `trades.kelly_fraction` | `p − (1 − p)/b` under the current win rate `p` and payoff ratio `b`. | Can be negative (unfavourable edge). |
| `trades.average_win` / `average_loss` / `largest_win` / `largest_loss` | | Each `None` when the relevant side is empty. |
| `trades.average_return_pct` | Mean per-trade return as a fraction of the entry notional. | |
| `trades.average_bars` / `min_bars` / `max_bars` | Holding duration in bars. | |
| `trades.average_seconds` / `min_seconds` / `max_seconds` | Same, in trading seconds. | Emitted only when both an `AssetClass` and a resolvable bar cadence are known; empty otherwise. |

### Trade-metric caveats

- **A profit factor of `∞` isn't emitted.** A run with wins and no losses
  gets `profit_factor = None`, not a huge number. Same shape for the payoff
  ratio and Kelly fraction.
- **`kelly_fraction` can be negative.** That means the trade edge is
  unfavourable — do not size a position from it.
- **Exposure is bar-fraction, not trade-fraction.** A strategy that opens
  once and holds for the whole run has `exposure_pct` ≈ 100 and
  `total = 1` (assuming the position is still open at the end, it doesn't
  round-trip and no `Trade` is booked at all — many trade-level metrics
  read `None`).

## `costs.*` — cost-model aggregates

Populated only when `run --costs` set a non-trivial cost model. Omitted
entirely otherwise (a zero-cost `metrics.yml` matches the pre-costs schema
byte-for-byte).

| Column | Meaning |
|---|---|
| `costs.total_commission` | Sum of every fill's `commission`, in reference currency. |
| `costs.total_slippage_cost` | Σ `|net_price − gross_price| × units` — the aggregate spread + slippage the wallet took out of the run. Computed by re-running the same strategy zero-cost and matching fills by `(bar, kind, side, units)`. |
| `costs.cost_drag_pct` | Gross CAGR minus net CAGR, in percentage points. `None` when either endpoint's CAGR is degenerate. |

## `selection.*` — trial-population corrections

Not part of the `Metrics` document — a *trailing column* emitted only
where a meaningful trial population exists. There is currently one metric
in this section:

### `selection.deflated_sharpe` — DSR (Bailey & López de Prado, 2014)

The probability that a Sharpe estimate survives a multiple-testing
correction against the population of trials it belongs to. A probability
in `[0, 1]`. Computed as `deflated_sharpe_from_stats(sharpe, skew, kurt,
n_returns, bpy, n_trials, trial_var)`.

**Emitted in two places, with two different trial populations:**

- **`optimize`'s grid CSV.** `n_trials` = number of grid rows,
  `trial_var` = sample variance of the grid's annualized Sharpes. Answers
  "does *this* cell's Sharpe survive correction for the fact that N cells
  were searched and the best was picked?" — post-selection inference in
  the textbook sense.
- **`run -w`'s `metrics.csv`.** `n_trials` = number of non-overlapping
  windows, `trial_var` = sample variance of the window Sharpes. Answers
  "does *this* window's Sharpe survive correction for the fact that N
  windows were inspected?" — a passive-selection correction rather than
  active grid search.

**Caveats:**

- **Per-row summary stats, grid-wide null.** In whole-run `optimize`, each
  row's summary is its own Sharpe / skew / kurt / bars. In windowed
  `optimize`, each row's summary is the cross-window arithmetic mean of
  window Sharpes / skews / kurts, with `n_returns` = sum of window bar
  counts. Aggregating higher moments by cross-window mean isn't quite the
  pooled-returns skew — it matches how the windowed `_mean` columns
  aggregate, so the DSR cell stays comparable to its neighbours.
- **Emitted only when the null is defined.** Omitted when fewer than two
  rows/windows have a defined Sharpe or when `trial_var` is zero.
- **Not emitted for `rolling.csv`.** Adjacent rolling windows share
  `LEN − 1` bars, so `trial_var` is understated and the DSR would be
  optimistic. Use non-overlapping windows for this correction.
- **Windowing regularises but does not eliminate selection bias.**
  `optimize -w` still emits DSR because the cross-window mean of the
  winning cell is still a max-of-many statistic — the correction is worth
  applying even after the mean smooths the point estimate.

## Cross-cutting caveats

### Warm-up and readiness

The strategy layer's readiness gate holds the first trade until every
consulted source has cleared its warm-up and IIR settling tail. During
the warm-up the equity curve is flat, so:

- `returns.stddev_bar` is smaller than it would be on the "traded" tail
  alone (many zero returns diluting the sample).
- `returns.skewness` / `kurtosis` are pulled toward the zero-return spike.
- `sharpe` / `sortino` are consequently biased *down* relative to a
  warm-up-trimmed evaluation.

For most single-symbol runs the warm-up is a small fraction of the run
and this effect is negligible; on very short backtests or very long
warm-ups it can dominate. There is no automatic warm-up crop — the
strategy's readiness gate is the crop, and everything past it is what's
measured.

### Windowed aggregation in `optimize`

Each `-m NAME` under `-w LEN` becomes `<NAME>_mean` / `<NAME>_std` where
mean / std are taken across the row's non-overlapping windows, over the
windows where the metric is defined. `--best-by` ranks by the mean,
optionally shifted against the row by `k` standard deviations
(`-k/--risk-aversion`), direction-aware (`mean − k·std` for
higher-is-better, `mean + k·std` for lower-is-better) so dispersion is
always penalised.

The aggregation is **strictly per grid row across that row's windows** —
there is no pooling across grid rows. When two rows produce identical
means and stddevs it is because the underlying trades were identical
(common on tiny toy datasets with small parameter ranges), not because
the aggregation is global.

### `median_bar` on flat runs

`median_return` is 0 for any run where at least half the bars have zero
return — which is the typical case (only a minority of bars trade). Prefer
`mean_bar` for a return summary.

### Trade counts vs fill counts

`trades.total` counts closed round-trips. A strategy that only ever opens
one position and never closes it books zero trades but many fills — the
`Position` never returns to flat, so no `Trade` is reconstructed. Most
`trades.*` fields will read `None` on such a run; consult
`trades.total_fills` and `trades.exposure_pct` instead.

### `average_return_pct` vs `total_pct`

`trades.average_return_pct` is the mean per-trade return as a fraction of
each trade's entry notional — a trade-quality statistic, unrelated to the
compounded run return. Do not multiply it by `trades.total` and expect
`returns.total_pct`.

### `PSR` in windowed contexts vs `DSR`

Both surface in `run -w`'s `metrics.csv`, but they answer different
questions.

- **`risk_adjusted.probabilistic_sharpe`** — per-window, single-trial.
  "Given this window's returns and their higher moments, what's the
  probability its true Sharpe > 0?"
- **`selection.deflated_sharpe`** — per-window, corrected against the
  window population. "Given N windows were inspected and their Sharpes
  varied by V, what's the probability this window's Sharpe survives that
  correction?"

Use PSR when you care about a single window's estimation noise. Use DSR
when the fact that you looked at N windows is itself a source of bias.
