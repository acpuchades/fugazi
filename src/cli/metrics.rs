//! Post-run backtest evaluation metrics, written to `metrics.yml`.
//!
//! Derived from the two artefacts the run already produces in memory: the
//! **equity curve** (one point per bar, from the wallet's mark-to-market) and
//! the **fill blotter** (one entry per booked order, tagged with its bar
//! index). Annualized figures scale per-bar moments by `bars_per_year`
//! (arithmetic for the mean, `sqrt` for the stddev, geometric for CAGR),
//! and Sharpe/Sortino/UPI subtract the annualized `risk_free_rate` (a
//! fraction — `0.045` for 4.5% p.a.) before dividing — so a non-daily bar
//! cadence or a non-zero rf needs the right flag to keep those numbers
//! meaningful.
//!
//! Round-trip trades are reconstructed by walking the blotter and tracking one
//! signed position with a volume-weighted average entry: an opposite-side fill
//! closes (partially or fully), realizes its PnL, and — if it crosses zero —
//! re-opens the remainder at the same fill price as a fresh trade. So one
//! reversal (`set(Buy, all-in)` while short) counts as one closed short plus
//! one open long, matching how [`SingleAssetStrategy`] reasons about them.
//!
//! The output is YAML, grouped by theme: `run`, `returns`, `risk_adjusted`,
//! `drawdown`, `trades`. Ratios and averages whose denominator is degenerate
//! (no trades, zero variance, no losing trade for a profit factor, …) are
//! omitted rather than emitted as `NaN`/`Infinity` so the file stays a clean
//! YAML scalar map a downstream tool can trust.
//!
//! # Reuse from other commands
//!
//! [`compute`] is a pure function of the equity curve, the fill blotter, the
//! starting cash, the annualization factor, and the (fractional) risk-free rate — no
//! filesystem, no logging. A future `optimize` subcommand doing a grid search
//! over params can run the backtest for each combination, call `compute` on
//! each result, and pick a winner by any field on [`Metrics`]
//! (Sharpe/Sortino/CAGR/Omega/max drawdown/…) — then optionally call
//! [`write_yaml`] once for the retained combination. [`Metrics`] is `Clone`
//! so a grid-search caller can keep the full metrics per combination in
//! memory without re-running the backtest to inspect them.

use std::path::Path;

use anyhow::{Context, Result, anyhow, bail};
use fugazi::prelude::*;
use serde::Serialize;

/// Below this magnitude, a residual position after a reducing fill is treated
/// as fully flat — the same 1e-8 threshold the wallet uses for zero-delta
/// orders, kept local so the metrics module doesn't lean on a re-export.
const EPSILON: Real = 1e-8;

/// A closed round-trip position reconstructed from the blotter.
struct ClosedTrade {
    /// Realized PnL in reference (quote) currency.
    pnl: Real,
    /// Fractional return on the notional at entry (`pnl / (entry_price *
    /// close_units)`), `0.0` if the entry notional is degenerate.
    return_frac: Real,
    /// Bars from opening fill to closing fill (0 if same-bar open+close).
    bars: usize,
    /// `true` if the closed leg was long, `false` if short.
    long: bool,
}

/// The metrics document written to `metrics.yml`, grouped by theme so the file
/// reads top-down as "run inputs → returns → risk-adjusted → drawdown → trades".
///
/// `Clone` so a grid-search caller (see the module docs) can retain one full
/// [`Metrics`] per param combination without re-running the backtest.
#[derive(Clone, Debug, Serialize)]
pub struct Metrics {
    pub run: RunSection,
    pub returns: ReturnSection,
    pub risk_adjusted: RiskAdjustedSection,
    pub drawdown: DrawdownSection,
    pub trades: TradeSection,
}

/// Non-metric context echoed at the top of `metrics.yml` so a numbers-only
/// reader can still identify the run.
#[derive(Clone, Debug, Serialize)]
pub struct RunSection {
    pub bars: usize,
    pub initial_equity: Real,
    pub final_equity: Real,
    pub bars_per_year: Real,
    /// Annualized risk-free rate as a fraction (e.g. `0.045` = 4.5% p.a.).
    /// Subtracted from the annualized mean return before Sharpe/Sortino/UPI,
    /// and used as the per-bar threshold for Omega.
    pub risk_free_rate: Real,
}

/// Return metrics: total return, its per-bar moments, distribution shape and
/// tail statistics. The `annualized_*` figures scale the per-bar mean by
/// `bars_per_year` and the stddev by `sqrt(bars_per_year)`, so they only make
/// sense when the bar cadence matches `bars_per_year`.
#[derive(Clone, Debug, Serialize)]
pub struct ReturnSection {
    pub total: Real,
    pub total_pct: Real,
    /// Compound annual growth rate (CAGR), or `None` for a non-positive equity path.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub cagr_pct: Option<Real>,
    pub mean_bar: Real,
    pub median_bar: Real,
    pub stddev_bar: Real,
    pub best_bar: Real,
    pub worst_bar: Real,
    /// Percentage of bars with a strictly positive return.
    pub positive_bars_pct: Real,
    /// Sample skewness of per-bar returns; `None` when stddev is zero.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub skewness: Option<Real>,
    /// Excess kurtosis of per-bar returns (kurtosis − 3, so a normal
    /// distribution reads 0); `None` when stddev is zero.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub kurtosis: Option<Real>,
    /// Historical 5%-VaR of per-bar returns, expressed as a positive loss
    /// fraction (0.02 = "5% worst case is a 2% loss"). Negative if the 5th
    /// percentile of returns is positive (no meaningful downside).
    pub var_95: Real,
    /// Historical 5%-CVaR (Expected Shortfall): mean of the bottom-5% return
    /// tail, expressed as a positive loss fraction.
    pub cvar_95: Real,
    /// `|P95(returns)| / |P5(returns)|`; `None` when the 5th-percentile
    /// magnitude is zero.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub tail_ratio: Option<Real>,
    pub annualized_mean_pct: Real,
    pub annualized_volatility_pct: Real,
}

/// Risk-adjusted ratios. Sharpe/Sortino/UPI subtract the annualized
/// risk-free rate from the annualized mean; Calmar and Omega do not
/// (Calmar is a raw return/max-DD, Omega already uses the rate as its
/// threshold). Each is `None` when its denominator is degenerate.
#[derive(Clone, Debug, Serialize)]
pub struct RiskAdjustedSection {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub sharpe: Option<Real>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub sortino: Option<Real>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub calmar: Option<Real>,
    /// Omega ratio at threshold = per-bar risk-free rate.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub omega: Option<Real>,
    /// Root-mean-squared drawdown as a fraction (Peter Martin's Ulcer Index,
    /// stored fractional; multiply by 100 for the classic percent form).
    pub ulcer_index: Real,
    /// Ulcer Performance Index: `(CAGR − rf) / ulcer_index`. `None` when UI
    /// or CAGR is degenerate.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub ulcer_performance_index: Option<Real>,
}

/// Drawdown analytics: the worst peak-to-trough drop, the shape of the full
/// drawdown-segment set (each segment = peak → trough → recovery-or-end),
/// and the fraction of bars the equity curve was below a prior peak.
#[derive(Clone, Debug, Serialize)]
pub struct DrawdownSection {
    pub max: Real,
    pub max_pct: Real,
    pub max_duration_bars: usize,
    /// Mean drawdown depth across all segments; `None` for a monotonically
    /// non-decreasing equity curve.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub avg: Option<Real>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub avg_pct: Option<Real>,
    /// Mean peak-to-trough bars across all segments; `None` when there are no
    /// segments.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub avg_duration_bars: Option<Real>,
    pub count: usize,
    pub time_in_drawdown_pct: Real,
    /// `total_return / max_drawdown` — the non-annualized cousin of Calmar.
    /// `None` when `max_drawdown` is zero.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub recovery_factor: Option<Real>,
}

/// Round-trip trade statistics. Averages/ratios are omitted when there are no
/// trades of the required kind (e.g. `profit_factor` needs a losing trade).
#[derive(Clone, Debug, Serialize)]
pub struct TradeSection {
    pub total: usize,
    pub wins: usize,
    pub losses: usize,
    pub flat: usize,
    pub long_trades: usize,
    pub short_trades: usize,
    pub total_fills: usize,
    pub max_consecutive_wins: usize,
    pub max_consecutive_losses: usize,
    /// Percentage of bars during which the wallet held a non-zero position
    /// (time in market), derived from the fill blotter.
    pub exposure_pct: Real,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub win_rate_pct: Option<Real>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub profit_factor: Option<Real>,
    /// `average_win / |average_loss|` (count-agnostic, magnitude-weighted).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub payoff_ratio: Option<Real>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub expectancy: Option<Real>,
    /// Kelly-optimal fraction of bankroll per trade under the current win rate
    /// and payoff ratio (`p − (1 − p)/b`). `None` when either input is
    /// undefined; can be negative (unfavourable edge).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub kelly_fraction: Option<Real>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub average_win: Option<Real>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub average_loss: Option<Real>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub largest_win: Option<Real>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub largest_loss: Option<Real>,
    /// Mean per-trade return as a fraction of the entry notional.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub average_return_pct: Option<Real>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub average_bars: Option<Real>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub min_bars: Option<usize>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub max_bars: Option<usize>,
}

/// Compute the metrics for one run.
///
/// `equity_curve` is one point per bar (post mark-to-market). `fills` is the
/// blotter as `(bar_index, order)` pairs — the shape a run driver naturally
/// produces (see `backtest.rs`), and the shape a future `optimize` grid-search
/// caller will produce for each param combination. `cash` is the initial
/// funds echoed in the `run` block; `bars_per_year` scales per-bar return
/// moments to annual figures; `risk_free_rate` is the annualized rf as a
/// fraction (`0.045` = 4.5% p.a.) subtracted from annualized returns before
/// Sharpe/Sortino/UPI and used as the per-bar threshold for Omega.
pub fn compute(
    equity_curve: &[Real],
    fills: &[(usize, Order<String>)],
    cash: Real,
    bars_per_year: Real,
    risk_free_rate: Real,
) -> Metrics {
    let bars = equity_curve.len();
    let final_equity = equity_curve.last().copied().unwrap_or(cash);

    let bar_returns = per_bar_returns(equity_curve, cash);
    let sorted_returns = sorted_asc(&bar_returns);
    let rf_per_bar = if bars_per_year > 0.0 {
        risk_free_rate / bars_per_year
    } else {
        0.0
    };
    let (mean, stddev) = mean_stddev(&bar_returns);
    let downside = downside_stddev(&bar_returns, rf_per_bar);
    let skewness = skewness(&bar_returns, mean);
    let kurtosis = excess_kurtosis(&bar_returns, mean);

    let total = if cash != 0.0 {
        (final_equity - cash) / cash
    } else {
        0.0
    };
    let cagr_frac = cagr_fraction(cash, final_equity, bars, bars_per_year);

    let ann_mean = mean * bars_per_year;
    let ann_vol = stddev * bars_per_year.sqrt();
    let ann_downside = downside * bars_per_year.sqrt();
    let ann_excess = ann_mean - risk_free_rate;

    let sharpe = safe_div(ann_excess, ann_vol);
    let sortino = safe_div(ann_excess, ann_downside);

    let dd = drawdown_stats(equity_curve);
    let calmar = cagr_frac.and_then(|c| safe_div(c, dd.max));
    let recovery_factor = safe_div(total, dd.max);

    let omega = compute_omega(&bar_returns, rf_per_bar);
    let ulcer = ulcer_index(equity_curve);
    let upi = cagr_frac.and_then(|c| safe_div(c - risk_free_rate, ulcer));

    let best = bar_returns.iter().copied().reduce(Real::max).unwrap_or(0.0);
    let worst = bar_returns.iter().copied().reduce(Real::min).unwrap_or(0.0);
    let positive_bars_pct = if !bar_returns.is_empty() {
        let n = bar_returns.iter().filter(|&&r| r > 0.0).count() as Real;
        n / bar_returns.len() as Real * 100.0
    } else {
        0.0
    };
    let median_bar = median(&sorted_returns);
    let var_95 = -percentile(&sorted_returns, 0.05);
    let cvar_95 = -tail_mean(&sorted_returns, 0.05);
    let p95_abs = percentile(&sorted_returns, 0.95).abs();
    let p5_abs = percentile(&sorted_returns, 0.05).abs();
    let tail_ratio = safe_div(p95_abs, p5_abs);

    let trades = reconstruct_trades(fills);
    let exposed = exposed_bars(fills, bars);
    let exposure_pct = if bars > 0 {
        exposed as Real / bars as Real * 100.0
    } else {
        0.0
    };
    let trade_section = build_trade_section(&trades, fills.len(), exposure_pct);

    Metrics {
        run: RunSection {
            bars,
            initial_equity: cash,
            final_equity,
            bars_per_year,
            risk_free_rate,
        },
        returns: ReturnSection {
            total,
            total_pct: total * 100.0,
            cagr_pct: cagr_frac.map(|c| c * 100.0),
            mean_bar: mean,
            median_bar,
            stddev_bar: stddev,
            best_bar: best,
            worst_bar: worst,
            positive_bars_pct,
            skewness,
            kurtosis,
            var_95,
            cvar_95,
            tail_ratio,
            annualized_mean_pct: ann_mean * 100.0,
            annualized_volatility_pct: ann_vol * 100.0,
        },
        risk_adjusted: RiskAdjustedSection {
            sharpe,
            sortino,
            calmar,
            omega,
            ulcer_index: ulcer,
            ulcer_performance_index: upi,
        },
        drawdown: DrawdownSection {
            max: dd.max,
            max_pct: dd.max * 100.0,
            max_duration_bars: dd.max_duration_bars,
            avg: dd.avg,
            avg_pct: dd.avg.map(|a| a * 100.0),
            avg_duration_bars: dd.avg_duration_bars,
            count: dd.count,
            time_in_drawdown_pct: dd.time_in_drawdown_pct,
            recovery_factor,
        },
        trades: trade_section,
    }
}

/// Serialize the metrics document to `path` as YAML.
pub fn write_yaml(metrics: &Metrics, path: &Path) -> Result<()> {
    let yaml = serde_norway::to_string(metrics).context("serializing metrics")?;
    std::fs::write(path, yaml).with_context(|| format!("writing `{}`", path.display()))?;
    Ok(())
}

// ---------------------------------------------------------------------------
// Name-based metric lookup (for the `optimize` subcommand)
// ---------------------------------------------------------------------------

/// Resolve a user-typed metric `name` against `metrics` — either a short leaf
/// name (`sharpe`, `max_dd_pct`) when it's the only leaf with that key, or a
/// dotted path (`risk_adjusted.sharpe`, `drawdown.max_pct`) otherwise.
///
/// Returns the canonical dotted path plus the numeric value. `Ok(None)` when
/// the metric was omitted from the serialized document (a degenerate ratio,
/// e.g. `sharpe` with zero variance): its column in the optimize CSV becomes
/// an empty cell.
pub fn resolve_metric(name: &str, metrics: &Metrics) -> Result<(String, Option<Real>)> {
    let value = serde_json::to_value(metrics).context("serializing metrics")?;
    let path = resolve_metric_path(&value, name)?;
    let dotted = path.join(".");
    Ok((dotted, lookup_number(&value, &path)))
}

/// Resolve one metric name to a canonical dotted path against the shape of `root`.
/// A `.`-containing name is taken as a path verbatim; a short name walks the tree
/// and errors if it matches zero (unknown) or several leaves (ambiguous).
fn resolve_metric_path(root: &serde_json::Value, name: &str) -> Result<Vec<String>> {
    if name.contains('.') {
        // Verify the path exists at a numeric leaf, but tolerate a missing
        // omitted-because-degenerate metric by only failing when the path's
        // *intermediate* segments miss.
        let path: Vec<String> = name.split('.').map(String::from).collect();
        verify_path(root, &path).with_context(|| format!("unknown metric `{name}`"))?;
        return Ok(path);
    }
    let mut hits: Vec<Vec<String>> = Vec::new();
    walk_leaves(root, &mut Vec::new(), name, &mut hits);
    match hits.len() {
        0 => Err(anyhow!(
            "unknown metric `{name}` (see `metrics.yml` for available names)"
        )),
        1 => Ok(hits.pop().unwrap()),
        _ => {
            let paths: Vec<String> = hits.iter().map(|p| p.join(".")).collect();
            bail!(
                "metric `{name}` is ambiguous ({} matches: {}). Use a dotted path.",
                hits.len(),
                paths.join(", ")
            )
        }
    }
}

/// Walk `v`, recording each numeric leaf whose immediate key matches `target`.
fn walk_leaves(
    v: &serde_json::Value,
    cur: &mut Vec<String>,
    target: &str,
    hits: &mut Vec<Vec<String>>,
) {
    if let serde_json::Value::Object(map) = v {
        for (k, child) in map {
            cur.push(k.clone());
            if k == target && child.is_number() {
                hits.push(cur.clone());
            }
            walk_leaves(child, cur, target, hits);
            cur.pop();
        }
    }
}

/// Follow `path` through the object tree; return `Ok(())` if every non-final
/// segment resolves to an object (the final segment is allowed to be missing,
/// which is how an omitted `skip_serializing_if` metric reads).
fn verify_path(root: &serde_json::Value, path: &[String]) -> Result<()> {
    let mut cur = root;
    for (i, key) in path.iter().enumerate() {
        let obj = cur.as_object().ok_or_else(|| {
            anyhow!(
                "path segment `{}` at position {i} isn't an object",
                path[..i].join(".")
            )
        })?;
        match obj.get(key) {
            Some(next) => cur = next,
            None if i + 1 == path.len() => return Ok(()),
            None => bail!("path segment `{key}` at position {i} doesn't exist"),
        }
    }
    Ok(())
}

/// Look up a dotted path against `root` and return its numeric value if all
/// segments exist and the leaf is a number; `None` when any segment is missing
/// (an omitted metric) or the leaf isn't numeric.
fn lookup_number(root: &serde_json::Value, path: &[String]) -> Option<Real> {
    let mut cur = root;
    for key in path {
        cur = cur.as_object()?.get(key)?;
    }
    cur.as_f64()
}

// ---------------------------------------------------------------------------
// Return moments and distribution shape
// ---------------------------------------------------------------------------

/// Per-bar fractional return series: `(equity[i] - prev) / prev`, seeded from
/// `cash` for the first bar. Zero-denominator bars contribute `0.0`.
fn per_bar_returns(equity: &[Real], cash: Real) -> Vec<Real> {
    let mut out = Vec::with_capacity(equity.len());
    let mut prev = cash;
    for &e in equity {
        let r = if prev != 0.0 { (e - prev) / prev } else { 0.0 };
        out.push(r);
        prev = e;
    }
    out
}

/// Sample mean and sample (Bessel-corrected, `ddof=1`) stddev of `xs`. The
/// `ddof=1` divisor matches the industry convention used by empyrical /
/// pyfolio / quantstats and Excel `STDEV`, so `stddev_bar`, annualized
/// volatility and Sharpe read identically to those references. Empty input
/// → `(0, 0)`; a single-sample sample stddev is undefined and returned as
/// `0`.
fn mean_stddev(xs: &[Real]) -> (Real, Real) {
    let n = xs.len();
    if n == 0 {
        return (0.0, 0.0);
    }
    let mean = xs.iter().sum::<Real>() / n as Real;
    if n < 2 {
        return (mean, 0.0);
    }
    let var = xs.iter().map(|x| (x - mean).powi(2)).sum::<Real>() / (n - 1) as Real;
    (mean, var.sqrt())
}

/// Downside stddev (Sortino denominator) with `threshold` as the Minimum
/// Acceptable Return: `sqrt(mean(min(0, r − threshold)^2))`. Uses an `n`
/// divisor (not `n − 1`) so it matches empyrical's `downside_risk` exactly
/// — a run with every bar clearing the threshold returns `0`.
fn downside_stddev(xs: &[Real], threshold: Real) -> Real {
    if xs.is_empty() {
        return 0.0;
    }
    let n = xs.len() as Real;
    let sum_sq = xs
        .iter()
        .map(|x| (x - threshold).min(0.0).powi(2))
        .sum::<Real>();
    (sum_sq / n).sqrt()
}

/// Biased (population) skewness — the classical `g1 = m3 / m2^(3/2)` over
/// central moments with an `n` divisor. Matches `scipy.stats.skew(bias=True)`.
/// `None` when the second moment is zero.
fn skewness(xs: &[Real], mean: Real) -> Option<Real> {
    if xs.is_empty() {
        return None;
    }
    let n = xs.len() as Real;
    let m2 = xs.iter().map(|x| (x - mean).powi(2)).sum::<Real>() / n;
    if m2 == 0.0 {
        return None;
    }
    let m3 = xs.iter().map(|x| (x - mean).powi(3)).sum::<Real>() / n;
    Some(m3 / m2.powf(1.5))
}

/// Biased excess kurtosis — `g2 = m4 / m2^2 − 3`, so a normal distribution
/// reads 0. Matches `scipy.stats.kurtosis(bias=True, fisher=True)`. `None`
/// when the second moment is zero.
fn excess_kurtosis(xs: &[Real], mean: Real) -> Option<Real> {
    if xs.is_empty() {
        return None;
    }
    let n = xs.len() as Real;
    let m2 = xs.iter().map(|x| (x - mean).powi(2)).sum::<Real>() / n;
    if m2 == 0.0 {
        return None;
    }
    let m4 = xs.iter().map(|x| (x - mean).powi(4)).sum::<Real>() / n;
    Some(m4 / m2.powi(2) - 3.0)
}

/// Sorted-ascending copy, `NaN`-tolerant (`NaN`s are treated as equal so the
/// ordering doesn't panic; the metrics module doesn't emit `NaN` anywhere).
fn sorted_asc(xs: &[Real]) -> Vec<Real> {
    let mut v = xs.to_vec();
    v.sort_by(|a, b| a.partial_cmp(b).unwrap_or(std::cmp::Ordering::Equal));
    v
}

/// Median of a sorted slice.
fn median(sorted: &[Real]) -> Real {
    if sorted.is_empty() {
        return 0.0;
    }
    let n = sorted.len();
    if n % 2 == 1 {
        sorted[n / 2]
    } else {
        (sorted[n / 2 - 1] + sorted[n / 2]) / 2.0
    }
}

/// Linearly-interpolated `p`-quantile of a sorted-ascending slice (R's type-7,
/// `numpy`'s default). `p` in `[0, 1]`.
fn percentile(sorted: &[Real], p: Real) -> Real {
    if sorted.is_empty() {
        return 0.0;
    }
    let n = sorted.len();
    if n == 1 {
        return sorted[0];
    }
    let idx = p * (n - 1) as Real;
    let lo = idx.floor() as usize;
    let hi = (lo + 1).min(n - 1);
    let frac = idx - lo as Real;
    sorted[lo] * (1.0 - frac) + sorted[hi] * frac
}

/// Mean of the bottom-`p` fraction of a sorted-ascending slice (CVaR / ES
/// numerator when `p = 0.05`).
fn tail_mean(sorted: &[Real], p: Real) -> Real {
    if sorted.is_empty() {
        return 0.0;
    }
    let cutoff = ((sorted.len() as Real * p).ceil() as usize).max(1);
    sorted[..cutoff].iter().sum::<Real>() / cutoff as Real
}

// ---------------------------------------------------------------------------
// CAGR, Sharpe helper, Omega, Ulcer
// ---------------------------------------------------------------------------

/// Compound annual growth rate as a fraction (e.g. `0.15` for +15% p.a.).
///
/// Returns `None` when the equity path is non-positive at either endpoint (the
/// ratio would be undefined) or the run is empty.
fn cagr_fraction(cash: Real, final_equity: Real, bars: usize, bars_per_year: Real) -> Option<Real> {
    if cash <= 0.0 || final_equity <= 0.0 || bars == 0 || bars_per_year <= 0.0 {
        return None;
    }
    let years = bars as Real / bars_per_year;
    if years <= 0.0 {
        return None;
    }
    Some((final_equity / cash).powf(1.0 / years) - 1.0)
}

/// `Some(numerator / denominator)`, or `None` when the denominator is not
/// strictly positive (so ratios don't leak `NaN`/`Infinity` into the YAML).
fn safe_div(num: Real, denom: Real) -> Option<Real> {
    if denom > 0.0 && denom.is_finite() {
        Some(num / denom)
    } else {
        None
    }
}

/// Omega ratio at `threshold`: `Σ max(r − τ, 0) / Σ max(τ − r, 0)`. `None`
/// when all returns clear the threshold (no downside integral).
fn compute_omega(returns: &[Real], threshold: Real) -> Option<Real> {
    let mut gains = 0.0;
    let mut losses = 0.0;
    for &r in returns {
        let diff = r - threshold;
        if diff >= 0.0 {
            gains += diff;
        } else {
            losses += -diff;
        }
    }
    safe_div(gains, losses)
}

/// Peter Martin's Ulcer Index in fractional form: root-mean-squared drawdown
/// (each `(equity[i] - running_peak[i]) / running_peak[i]`). Bars at or above
/// the running peak contribute zero, so a monotone-non-decreasing curve gives
/// `0`.
fn ulcer_index(equity: &[Real]) -> Real {
    if equity.is_empty() {
        return 0.0;
    }
    let mut peak = 0.0_f64;
    let mut sum_sq = 0.0;
    for &e in equity {
        if e > peak {
            peak = e;
        }
        if peak > 0.0 {
            let d = (e - peak) / peak; // ≤ 0
            sum_sq += d * d;
        }
    }
    (sum_sq / equity.len() as Real).sqrt()
}

// ---------------------------------------------------------------------------
// Drawdown segments
// ---------------------------------------------------------------------------

/// Aggregate drawdown statistics across every segment of the equity curve.
///
/// A **segment** is a `peak → trough → recovery-or-end` triple: bars where the
/// curve is below a running peak. The last segment can be open-ended if the
/// curve never recovers before the run ends. Bars at or above the running peak
/// (equity == peak) don't count as underwater.
struct DrawdownStats {
    max: Real,
    max_duration_bars: usize,
    avg: Option<Real>,
    avg_duration_bars: Option<Real>,
    count: usize,
    time_in_drawdown_pct: Real,
}

fn drawdown_stats(equity: &[Real]) -> DrawdownStats {
    if equity.is_empty() {
        return DrawdownStats {
            max: 0.0,
            max_duration_bars: 0,
            avg: None,
            avg_duration_bars: None,
            count: 0,
            time_in_drawdown_pct: 0.0,
        };
    }

    let mut peak = equity[0];
    let mut peak_idx = 0;
    let mut in_dd = false;
    let mut segment_trough = peak;
    let mut segment_trough_idx = 0;

    // (depth, peak-to-trough bars) per closed or unrecovered segment.
    let mut segments: Vec<(Real, usize)> = Vec::new();
    let mut bars_in_dd = 0usize;

    for (i, &e) in equity.iter().enumerate() {
        if e > peak {
            if in_dd {
                let depth = if peak > 0.0 {
                    (peak - segment_trough) / peak
                } else {
                    0.0
                };
                segments.push((depth, segment_trough_idx - peak_idx));
                in_dd = false;
            }
            peak = e;
            peak_idx = i;
        } else if e < peak {
            bars_in_dd += 1;
            if !in_dd {
                in_dd = true;
                segment_trough = e;
                segment_trough_idx = i;
            } else if e < segment_trough {
                segment_trough = e;
                segment_trough_idx = i;
            }
        }
    }
    if in_dd {
        let depth = if peak > 0.0 {
            (peak - segment_trough) / peak
        } else {
            0.0
        };
        segments.push((depth, segment_trough_idx - peak_idx));
    }

    let (max, max_duration_bars) = segments
        .iter()
        .copied()
        .max_by(|a, b| a.0.partial_cmp(&b.0).unwrap_or(std::cmp::Ordering::Equal))
        .unwrap_or((0.0, 0));

    let count = segments.len();
    let avg = if count > 0 {
        Some(segments.iter().map(|(d, _)| *d).sum::<Real>() / count as Real)
    } else {
        None
    };
    let avg_duration_bars = if count > 0 {
        Some(segments.iter().map(|(_, d)| *d as Real).sum::<Real>() / count as Real)
    } else {
        None
    };
    let time_in_drawdown_pct = bars_in_dd as Real / equity.len() as Real * 100.0;

    DrawdownStats {
        max,
        max_duration_bars,
        avg,
        avg_duration_bars,
        count,
        time_in_drawdown_pct,
    }
}

// ---------------------------------------------------------------------------
// Trade reconstruction and blotter-derived stats
// ---------------------------------------------------------------------------

/// Walk the blotter with a single signed position and a volume-weighted
/// entry price, closing (or reversing) on any opposite-side fill and
/// recording each closed round-trip.
fn reconstruct_trades(fills: &[(usize, Order<String>)]) -> Vec<ClosedTrade> {
    struct Open {
        signed_units: Real,
        entry_price: Real,
        entry_bar: usize,
    }

    let mut trades = Vec::new();
    let mut open: Option<Open> = None;

    for (bar, order) in fills {
        let delta = order.signed_units();
        let bar = *bar;
        let price = order.price;

        match open.as_mut() {
            None => {
                open = Some(Open {
                    signed_units: delta,
                    entry_price: price,
                    entry_bar: bar,
                });
            }
            Some(pos) if pos.signed_units.signum() == delta.signum() => {
                // Adding to the position: volume-weighted new entry.
                let new_units = pos.signed_units + delta;
                let notional =
                    pos.signed_units.abs() * pos.entry_price + delta.abs() * price;
                pos.entry_price = notional / new_units.abs();
                pos.signed_units = new_units;
            }
            Some(pos) => {
                // Opposite side: reducing, closing, or reversing.
                let close_units = pos.signed_units.abs().min(delta.abs());
                let long = pos.signed_units > 0.0;
                let pnl_per_unit = if long {
                    price - pos.entry_price
                } else {
                    pos.entry_price - price
                };
                let pnl = pnl_per_unit * close_units;
                let entry_notional = pos.entry_price * close_units;
                let return_frac = if entry_notional > 0.0 {
                    pnl / entry_notional
                } else {
                    0.0
                };
                trades.push(ClosedTrade {
                    pnl,
                    return_frac,
                    bars: bar - pos.entry_bar,
                    long,
                });
                let remaining = pos.signed_units + delta;
                if remaining.abs() <= EPSILON {
                    open = None;
                } else {
                    // Reversed: the remainder is a fresh position at this fill.
                    open = Some(Open {
                        signed_units: remaining,
                        entry_price: price,
                        entry_bar: bar,
                    });
                }
            }
        }
    }

    trades
}

/// Count of bars during which the wallet held a non-zero position, derived
/// from the fills alone: a fill at bar `B` applies at that bar's open, so the
/// position it produces is what's held from bar `B` onward until the next
/// fill (or the end of the run).
fn exposed_bars(fills: &[(usize, Order<String>)], total_bars: usize) -> usize {
    let mut position: Real = 0.0;
    let mut prev_bar = 0;
    let mut exposed = 0;
    for (bar, order) in fills {
        if position.abs() > EPSILON {
            exposed += bar.saturating_sub(prev_bar);
        }
        position += order.signed_units();
        prev_bar = *bar;
    }
    if position.abs() > EPSILON {
        exposed += total_bars.saturating_sub(prev_bar);
    }
    exposed
}

/// Longest run of trades satisfying `predicate` in the closed-trade sequence.
fn longest_streak(trades: &[ClosedTrade], predicate: impl Fn(&ClosedTrade) -> bool) -> usize {
    let mut max = 0usize;
    let mut cur = 0usize;
    for t in trades {
        if predicate(t) {
            cur += 1;
            if cur > max {
                max = cur;
            }
        } else {
            cur = 0;
        }
    }
    max
}

fn build_trade_section(
    trades: &[ClosedTrade],
    total_fills: usize,
    exposure_pct: Real,
) -> TradeSection {
    let total = trades.len();
    let wins = trades.iter().filter(|t| t.pnl > 0.0).count();
    let losses = trades.iter().filter(|t| t.pnl < 0.0).count();
    let flat = total - wins - losses;
    let long_trades = trades.iter().filter(|t| t.long).count();
    let short_trades = total - long_trades;

    let win_pnls: Vec<Real> = trades.iter().map(|t| t.pnl).filter(|&p| p > 0.0).collect();
    let loss_pnls: Vec<Real> = trades.iter().map(|t| t.pnl).filter(|&p| p < 0.0).collect();

    let sum_wins: Real = win_pnls.iter().sum();
    let sum_losses: Real = loss_pnls.iter().sum();

    let win_rate_pct = if total > 0 {
        Some((wins as Real / total as Real) * 100.0)
    } else {
        None
    };
    let profit_factor = safe_div(sum_wins, -sum_losses);
    let expectancy = if total > 0 {
        Some(trades.iter().map(|t| t.pnl).sum::<Real>() / total as Real)
    } else {
        None
    };
    let average_win = if !win_pnls.is_empty() {
        Some(sum_wins / win_pnls.len() as Real)
    } else {
        None
    };
    let average_loss = if !loss_pnls.is_empty() {
        Some(sum_losses / loss_pnls.len() as Real)
    } else {
        None
    };
    let largest_win = win_pnls.iter().copied().reduce(Real::max);
    let largest_loss = loss_pnls.iter().copied().reduce(Real::min);
    let payoff_ratio = match (average_win, average_loss) {
        (Some(w), Some(l)) if l < 0.0 => Some(w / -l),
        _ => None,
    };
    let kelly_fraction = match (win_rate_pct, payoff_ratio) {
        (Some(wr), Some(b)) if b > 0.0 => {
            let p = wr / 100.0;
            Some(p - (1.0 - p) / b)
        }
        _ => None,
    };
    let average_return_pct = if total > 0 {
        Some(trades.iter().map(|t| t.return_frac).sum::<Real>() / total as Real * 100.0)
    } else {
        None
    };
    let average_bars = if total > 0 {
        Some(trades.iter().map(|t| t.bars as Real).sum::<Real>() / total as Real)
    } else {
        None
    };
    let min_bars = trades.iter().map(|t| t.bars).min();
    let max_bars = trades.iter().map(|t| t.bars).max();

    let max_consecutive_wins = longest_streak(trades, |t| t.pnl > 0.0);
    let max_consecutive_losses = longest_streak(trades, |t| t.pnl < 0.0);

    TradeSection {
        total,
        wins,
        losses,
        flat,
        long_trades,
        short_trades,
        total_fills,
        max_consecutive_wins,
        max_consecutive_losses,
        exposure_pct,
        win_rate_pct,
        profit_factor,
        payoff_ratio,
        expectancy,
        kelly_fraction,
        average_win,
        average_loss,
        largest_win,
        largest_loss,
        average_return_pct,
        average_bars,
        min_bars,
        max_bars,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn order(side: Side, units: Real, price: Real) -> Order<String> {
        Order::new(
            "BTC".to_string(),
            side,
            units,
            price,
            OrderKind::Market,
            OrderId(0),
        )
    }

    fn tagged(orders: Vec<Order<String>>) -> Vec<(usize, Order<String>)> {
        orders.into_iter().enumerate().collect()
    }

    #[test]
    fn round_trip_long_realizes_pnl() {
        let fills = tagged(vec![
            order(Side::Buy, 1.0, 100.0),
            order(Side::Sell, 1.0, 110.0),
        ]);
        let trades = reconstruct_trades(&fills);
        assert_eq!(trades.len(), 1);
        assert!((trades[0].pnl - 10.0).abs() < 1e-9);
        assert_eq!(trades[0].bars, 1);
        assert!(trades[0].long);
        assert!((trades[0].return_frac - 0.1).abs() < 1e-9);
    }

    #[test]
    fn reversal_closes_short_and_opens_long() {
        // Short 1 @ 100, then buy 2 @ 90: closes short (+10) and opens long 1.
        // A final sell 1 @ 95 closes the long (+5).
        let fills = tagged(vec![
            order(Side::Sell, 1.0, 100.0),
            order(Side::Buy, 2.0, 90.0),
            order(Side::Sell, 1.0, 95.0),
        ]);
        let trades = reconstruct_trades(&fills);
        assert_eq!(trades.len(), 2);
        assert!((trades[0].pnl - 10.0).abs() < 1e-9);
        assert!(!trades[0].long);
        assert!((trades[1].pnl - 5.0).abs() < 1e-9);
        assert!(trades[1].long);
    }

    #[test]
    fn drawdown_stats_covers_multiple_segments() {
        // 100 → 110 (peak) → 90 (trough, dd=20/110) → 120 (recovery, closes seg 1)
        //     → 100 (in dd, depth 20/120) → run ends (open seg 2).
        let stats = drawdown_stats(&[100.0, 110.0, 105.0, 90.0, 95.0, 120.0, 100.0]);
        assert_eq!(stats.count, 2);
        assert!((stats.max - (110.0 - 90.0) / 110.0).abs() < 1e-9);
        assert_eq!(stats.max_duration_bars, 2); // peak idx 1 → trough idx 3
        let avg = stats.avg.unwrap();
        let expected_avg = ((110.0 - 90.0) / 110.0 + (120.0 - 100.0) / 120.0) / 2.0;
        assert!((avg - expected_avg).abs() < 1e-9);
        // Underwater bars: idx 2, 3, 4, 6 → 4/7 ≈ 57.14%.
        assert!((stats.time_in_drawdown_pct - 4.0 / 7.0 * 100.0).abs() < 1e-9);
    }

    #[test]
    fn drawdown_stats_flat_curve_reports_no_segments() {
        let stats = drawdown_stats(&[100.0, 100.0, 100.0]);
        assert_eq!(stats.count, 0);
        assert_eq!(stats.max, 0.0);
        assert!(stats.avg.is_none());
    }

    #[test]
    fn safe_div_guards_zero_denominator() {
        assert_eq!(safe_div(1.0, 0.0), None);
        assert_eq!(safe_div(1.0, -1.0), None);
        assert_eq!(safe_div(1.0, 2.0), Some(0.5));
    }

    #[test]
    fn percentile_and_tail_stats() {
        let xs = sorted_asc(&[-5.0, -3.0, -1.0, 0.0, 1.0, 2.0, 3.0, 4.0, 5.0, 6.0]);
        // With N=10, the type-7 5th percentile lies at idx 0.45 → interpolate
        // between -5 and -3 → -5 * 0.55 + -3 * 0.45 = -4.1.
        assert!((percentile(&xs, 0.05) - (-4.1)).abs() < 1e-9);
        // Bottom-5% tail = ceil(0.05 * 10) = 1 element = -5.
        assert!((tail_mean(&xs, 0.05) - (-5.0)).abs() < 1e-9);
        // Median of an even-length sample = mean of the two middle values.
        assert!((median(&xs) - 1.5).abs() < 1e-9);
    }

    #[test]
    fn skew_kurt_normalish_returns() {
        let xs = vec![-1.0, 0.0, 1.0]; // symmetric → skew ≈ 0
        let (mean, _) = mean_stddev(&xs);
        assert!(skewness(&xs, mean).unwrap().abs() < 1e-9);
        // Constant sample → m2 = 0 → skew/kurt undefined.
        let flat = vec![1.0; 5];
        let (m, _) = mean_stddev(&flat);
        assert!(skewness(&flat, m).is_none());
        assert!(excess_kurtosis(&flat, m).is_none());
    }

    #[test]
    fn omega_at_zero_threshold() {
        // Gains total 1+2=3, losses total 1+2=3 → Ω = 1.
        let ret = vec![1.0, -1.0, 2.0, -2.0];
        assert!((compute_omega(&ret, 0.0).unwrap() - 1.0).abs() < 1e-9);
        // No downside → None.
        assert!(compute_omega(&[1.0, 2.0, 3.0], 0.0).is_none());
    }

    #[test]
    fn ulcer_index_zero_on_monotone_curve() {
        assert_eq!(ulcer_index(&[100.0, 110.0, 120.0, 130.0]), 0.0);
        // Fall from 100 to 90 on bar 1, then hold at 100 (recovers) — the
        // underwater bar contributes (10/100)^2 = 0.01, others contribute 0.
        let ui = ulcer_index(&[100.0, 90.0, 100.0]);
        assert!((ui - (0.01_f64 / 3.0).sqrt()).abs() < 1e-9);
    }

    #[test]
    fn exposure_from_fills() {
        // Long from bar 3 to bar 6 (bar 7 closes at open) = 4 bars out of 10.
        let fills = vec![
            (3, order(Side::Buy, 1.0, 100.0)),
            (7, order(Side::Sell, 1.0, 110.0)),
        ];
        assert_eq!(exposed_bars(&fills, 10), 4);
    }

    fn sample_metrics() -> Metrics {
        // Two long round trips, +10 then -5, with a non-degenerate variance so
        // Sharpe/Sortino are populated (not `None`).
        let fills = tagged(vec![
            order(Side::Buy, 1.0, 100.0),
            order(Side::Sell, 1.0, 110.0),
            order(Side::Buy, 1.0, 108.0),
            order(Side::Sell, 1.0, 103.0),
        ]);
        let equity = vec![100.0, 105.0, 110.0, 108.0, 103.0];
        compute(&equity, &fills, 100.0, 252.0, 0.0)
    }

    #[test]
    fn resolve_metric_by_short_name() {
        let m = sample_metrics();
        let (path, value) = resolve_metric("sharpe", &m).unwrap();
        assert_eq!(path, "risk_adjusted.sharpe");
        assert!(value.is_some());
    }

    #[test]
    fn resolve_metric_by_dotted_path() {
        let m = sample_metrics();
        let (path, value) = resolve_metric("drawdown.max_pct", &m).unwrap();
        assert_eq!(path, "drawdown.max_pct");
        assert!(value.is_some());
    }

    #[test]
    fn resolve_metric_unknown_errors() {
        let m = sample_metrics();
        let err = resolve_metric("does_not_exist", &m).unwrap_err();
        assert!(err.to_string().contains("unknown metric"));
    }

    #[test]
    fn streaks_track_longest_run() {
        // W W L W L L L W W W → longest wins 3, longest losses 3.
        let trades: Vec<ClosedTrade> = [1.0, 2.0, -1.0, 3.0, -1.0, -2.0, -3.0, 4.0, 5.0, 6.0]
            .into_iter()
            .map(|p| ClosedTrade {
                pnl: p,
                return_frac: 0.0,
                bars: 1,
                long: true,
            })
            .collect();
        assert_eq!(longest_streak(&trades, |t| t.pnl > 0.0), 3);
        assert_eq!(longest_streak(&trades, |t| t.pnl < 0.0), 3);
    }
}
