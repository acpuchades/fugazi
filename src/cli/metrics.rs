//! Post-run backtest evaluation metrics, written to `metrics.yml`.
//!
//! Derived from the two artefacts every run already produces (equity curve +
//! fill blotter), via the pure per-metric functions in [`fugazi::metrics`].
//! This module is the CLI's aggregation layer: it holds the serde-decorated
//! [`Metrics`] document that lands on disk as YAML, glues the library's
//! standalone metric functions into that document via [`from_report`], and
//! exposes a name-based lookup ([`resolve_metric`]) for the `optimize`
//! subcommand.
//!
//! The output is YAML, grouped by theme: `run`, `returns`, `risk_adjusted`,
//! `drawdown`, `trades`. Ratios and averages whose denominator is degenerate
//! (no trades, zero variance, no losing trade for a profit factor, …) are
//! omitted rather than emitted as `NaN`/`Infinity` so the file stays a clean
//! YAML scalar map a downstream tool can trust.

use std::path::Path;

use anyhow::{Context, Result, anyhow, bail};
use fugazi::Fill;
use fugazi::backtest::RunReport;
use fugazi::prelude::*;
use serde::Serialize;

/// The metrics document written to `metrics.yml`, grouped by theme so the file
/// reads top-down as "run inputs → returns → risk-adjusted → drawdown → trades".
///
/// `Clone` so a grid-search caller can retain one full [`Metrics`] per param
/// combination without re-running the backtest.
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

/// Reduce a [`RunReport`] to a [`Metrics`] document by calling one library
/// function per field. `bars_per_year` scales per-bar return moments to annual
/// figures; `risk_free_rate` is the annualized rf as a fraction (`0.045` =
/// 4.5% p.a.) subtracted from annualized returns before Sharpe/Sortino/UPI and
/// used as the per-bar threshold for Omega.
pub fn from_report<Sym>(
    report: &RunReport<Sym>,
    bars_per_year: Real,
    risk_free_rate: Real,
) -> Metrics {
    let equity = report.equity_curve.as_slice();
    let bars = equity.len();
    let initial = report.initial_equity;
    let final_equity = equity.last().copied().unwrap_or(initial);

    // Build each intermediate once, hand it to every metric that consumes it.
    let returns = fugazi::metrics::per_bar_returns(equity, initial);
    let trades = fugazi::metrics::reconstruct_trades(&report.fills);
    let segments = fugazi::metrics::drawdown_segments(equity);
    let rf_per_bar = if bars_per_year > 0.0 {
        risk_free_rate / bars_per_year
    } else {
        0.0
    };

    // Return-section values are computed as fractions in the library and
    // scaled to percent here at the presentation boundary.
    let total = fugazi::metrics::total_return(equity, initial);
    let cagr = fugazi::metrics::cagr(equity, initial, bars_per_year);
    let ann_mean = fugazi::metrics::annualized_return(&returns, bars_per_year);
    let ann_vol = fugazi::metrics::annualized_volatility(&returns, bars_per_year);
    let max_dd = fugazi::metrics::max_drawdown(&segments);
    let avg_dd = fugazi::metrics::average_drawdown(&segments);
    let win_rate = fugazi::metrics::win_rate(&trades);
    let avg_trade_return = fugazi::metrics::average_trade_return(&trades);
    let exposure = fugazi::metrics::exposure_ratio(&report.fills, bars);

    Metrics {
        run: RunSection {
            bars,
            initial_equity: initial,
            final_equity,
            bars_per_year,
            risk_free_rate,
        },
        returns: ReturnSection {
            total,
            total_pct: total * 100.0,
            cagr_pct: cagr.map(|c| c * 100.0),
            mean_bar: fugazi::metrics::mean_return(&returns),
            median_bar: fugazi::metrics::median_return(&returns),
            stddev_bar: fugazi::metrics::stddev_return(&returns),
            best_bar: fugazi::metrics::best_return(&returns),
            worst_bar: fugazi::metrics::worst_return(&returns),
            positive_bars_pct: fugazi::metrics::positive_bars_ratio(&returns) * 100.0,
            skewness: fugazi::metrics::skewness(&returns),
            kurtosis: fugazi::metrics::kurtosis(&returns),
            var_95: fugazi::metrics::value_at_risk(&returns, 0.95),
            cvar_95: fugazi::metrics::conditional_value_at_risk(&returns, 0.95),
            tail_ratio: fugazi::metrics::tail_ratio(&returns),
            annualized_mean_pct: ann_mean * 100.0,
            annualized_volatility_pct: ann_vol * 100.0,
        },
        risk_adjusted: RiskAdjustedSection {
            sharpe: fugazi::metrics::sharpe(&returns, risk_free_rate, bars_per_year),
            sortino: fugazi::metrics::sortino(&returns, risk_free_rate, bars_per_year),
            calmar: fugazi::metrics::calmar(equity, initial, bars_per_year),
            omega: fugazi::metrics::omega(&returns, rf_per_bar),
            ulcer_index: fugazi::metrics::ulcer_index(equity),
            ulcer_performance_index: fugazi::metrics::ulcer_performance_index(
                equity,
                initial,
                risk_free_rate,
                bars_per_year,
            ),
        },
        drawdown: DrawdownSection {
            max: max_dd,
            max_pct: max_dd * 100.0,
            max_duration_bars: fugazi::metrics::max_drawdown_duration(&segments),
            avg: avg_dd,
            avg_pct: avg_dd.map(|a| a * 100.0),
            avg_duration_bars: fugazi::metrics::average_drawdown_duration(&segments),
            count: fugazi::metrics::drawdown_count(&segments),
            time_in_drawdown_pct: fugazi::metrics::time_in_drawdown_ratio(&segments, bars) * 100.0,
            recovery_factor: fugazi::metrics::recovery_factor(equity, initial),
        },
        trades: TradeSection {
            total: fugazi::metrics::total_trades(&trades),
            wins: fugazi::metrics::winning_trades(&trades),
            losses: fugazi::metrics::losing_trades(&trades),
            flat: fugazi::metrics::flat_trades(&trades),
            long_trades: fugazi::metrics::long_trades(&trades),
            short_trades: fugazi::metrics::short_trades(&trades),
            total_fills: report.fills.len(),
            max_consecutive_wins: fugazi::metrics::max_consecutive_wins(&trades),
            max_consecutive_losses: fugazi::metrics::max_consecutive_losses(&trades),
            exposure_pct: exposure * 100.0,
            win_rate_pct: win_rate.map(|w| w * 100.0),
            profit_factor: fugazi::metrics::profit_factor(&trades),
            payoff_ratio: fugazi::metrics::payoff_ratio(&trades),
            expectancy: fugazi::metrics::expectancy(&trades),
            kelly_fraction: fugazi::metrics::kelly_fraction(&trades),
            average_win: fugazi::metrics::average_win(&trades),
            average_loss: fugazi::metrics::average_loss(&trades),
            largest_win: fugazi::metrics::largest_win(&trades),
            largest_loss: fugazi::metrics::largest_loss(&trades),
            average_return_pct: avg_trade_return.map(|r| r * 100.0),
            average_bars: fugazi::metrics::average_bars_held(&trades),
            min_bars: fugazi::metrics::min_bars_held(&trades),
            max_bars: fugazi::metrics::max_bars_held(&trades),
        },
    }
}

/// The [`Metrics`] document of one non-overlapping window of a run, tagged with
/// the window's bar span so a caller can map it back to times.
pub struct WindowMetrics {
    /// Zero-based bar index of the window's first bar.
    pub start_bar: usize,
    /// Zero-based bar index of the window's last bar (inclusive; the last
    /// window may be shorter than the requested length).
    pub end_bar: usize,
    pub metrics: Metrics,
}

/// The sub-run covering the half-open `bars` range of `report`: the equity
/// slice, the fills booked inside it (rebased to the range's own bar axis, so
/// bar-relative metrics read against it), and, as initial equity, the equity
/// marked on the bar before the range (the run's own `initial_equity` when the
/// range starts at bar 0).
///
/// This is the one measurement primitive the CLI's metric reductions share:
/// the first-fill anchor slices off the flat warm-up prefix, and each
/// `--windowed` window is a slice of its own.
pub fn report_slice<Sym: Clone>(
    report: &RunReport<Sym>,
    bars: std::ops::Range<usize>,
) -> RunReport<Sym> {
    let fills: Vec<Fill<Sym>> = report
        .fills
        .iter()
        .filter(|f| bars.contains(&f.bar))
        .map(|f| Fill {
            bar: f.bar - bars.start,
            order: f.order.clone(),
        })
        .collect();
    RunReport {
        equity_curve: report.equity_curve[bars.clone()].to_vec(),
        fills,
        initial_equity: if bars.start == 0 {
            report.initial_equity
        } else {
            report.equity_curve[bars.start - 1]
        },
    }
}

/// Reduce a [`RunReport`] to one [`Metrics`] document per non-overlapping
/// `window`-bar span (the last window keeps whatever bars remain). Each window
/// is evaluated as a run of its own — a [`report_slice`]: its initial equity
/// is the equity marked on the bar before it, and only the fills booked inside
/// it count — a position carried across a boundary shows up in the entering
/// window as an unmatched closing fill, the usual windowed-analysis convention.
pub fn windowed_from_report<Sym: Clone>(
    report: &RunReport<Sym>,
    window: usize,
    bars_per_year: Real,
    risk_free_rate: Real,
) -> Vec<WindowMetrics> {
    assert!(window > 0, "window length must be positive");
    let bars = report.equity_curve.len();
    let mut out = Vec::new();
    let mut start = 0;
    while start < bars {
        let end = (start + window).min(bars);
        out.push(WindowMetrics {
            start_bar: start,
            end_bar: end - 1,
            metrics: from_report(
                &report_slice(report, start..end),
                bars_per_year,
                risk_free_rate,
            ),
        });
        start = end;
    }
    out
}

/// Mean and population standard deviation of `values`, or `None` when empty —
/// the cross-window aggregation used by `run -w`'s console block and
/// `optimize -w`'s `_mean`/`_std` columns (population, not sample: the windows
/// are the whole set being described, not a draw from one).
pub fn mean_std(values: impl Iterator<Item = Real>) -> Option<(Real, Real)> {
    let v: Vec<Real> = values.collect();
    if v.is_empty() {
        return None;
    }
    let n = v.len() as Real;
    let mean = v.iter().sum::<Real>() / n;
    let variance = v.iter().map(|x| (x - mean) * (x - mean)).sum::<Real>() / n;
    Some((mean, variance.sqrt()))
}

/// Flatten a [`Metrics`] document into `(dotted name, value)` pairs — one entry
/// per leaf, in document order, under the same dotted names [`resolve_metric`]
/// accepts. Unlike the YAML serialization, degenerate metrics are *kept* (as
/// `None`), so every document flattens to the same fixed column set — what a
/// CSV with one row per window needs. Counts flatten to `Real`.
pub fn flatten(m: &Metrics) -> Vec<(&'static str, Option<Real>)> {
    let real = |v: Real| Some(v);
    let count = |v: usize| Some(v as Real);
    vec![
        ("run.bars", count(m.run.bars)),
        ("run.initial_equity", real(m.run.initial_equity)),
        ("run.final_equity", real(m.run.final_equity)),
        ("run.bars_per_year", real(m.run.bars_per_year)),
        ("run.risk_free_rate", real(m.run.risk_free_rate)),
        ("returns.total", real(m.returns.total)),
        ("returns.total_pct", real(m.returns.total_pct)),
        ("returns.cagr_pct", m.returns.cagr_pct),
        ("returns.mean_bar", real(m.returns.mean_bar)),
        ("returns.median_bar", real(m.returns.median_bar)),
        ("returns.stddev_bar", real(m.returns.stddev_bar)),
        ("returns.best_bar", real(m.returns.best_bar)),
        ("returns.worst_bar", real(m.returns.worst_bar)),
        ("returns.positive_bars_pct", real(m.returns.positive_bars_pct)),
        ("returns.skewness", m.returns.skewness),
        ("returns.kurtosis", m.returns.kurtosis),
        ("returns.var_95", real(m.returns.var_95)),
        ("returns.cvar_95", real(m.returns.cvar_95)),
        ("returns.tail_ratio", m.returns.tail_ratio),
        (
            "returns.annualized_mean_pct",
            real(m.returns.annualized_mean_pct),
        ),
        (
            "returns.annualized_volatility_pct",
            real(m.returns.annualized_volatility_pct),
        ),
        ("risk_adjusted.sharpe", m.risk_adjusted.sharpe),
        ("risk_adjusted.sortino", m.risk_adjusted.sortino),
        ("risk_adjusted.calmar", m.risk_adjusted.calmar),
        ("risk_adjusted.omega", m.risk_adjusted.omega),
        ("risk_adjusted.ulcer_index", real(m.risk_adjusted.ulcer_index)),
        (
            "risk_adjusted.ulcer_performance_index",
            m.risk_adjusted.ulcer_performance_index,
        ),
        ("drawdown.max", real(m.drawdown.max)),
        ("drawdown.max_pct", real(m.drawdown.max_pct)),
        ("drawdown.max_duration_bars", count(m.drawdown.max_duration_bars)),
        ("drawdown.avg", m.drawdown.avg),
        ("drawdown.avg_pct", m.drawdown.avg_pct),
        ("drawdown.avg_duration_bars", m.drawdown.avg_duration_bars),
        ("drawdown.count", count(m.drawdown.count)),
        (
            "drawdown.time_in_drawdown_pct",
            real(m.drawdown.time_in_drawdown_pct),
        ),
        ("drawdown.recovery_factor", m.drawdown.recovery_factor),
        ("trades.total", count(m.trades.total)),
        ("trades.wins", count(m.trades.wins)),
        ("trades.losses", count(m.trades.losses)),
        ("trades.flat", count(m.trades.flat)),
        ("trades.long_trades", count(m.trades.long_trades)),
        ("trades.short_trades", count(m.trades.short_trades)),
        ("trades.total_fills", count(m.trades.total_fills)),
        (
            "trades.max_consecutive_wins",
            count(m.trades.max_consecutive_wins),
        ),
        (
            "trades.max_consecutive_losses",
            count(m.trades.max_consecutive_losses),
        ),
        ("trades.exposure_pct", real(m.trades.exposure_pct)),
        ("trades.win_rate_pct", m.trades.win_rate_pct),
        ("trades.profit_factor", m.trades.profit_factor),
        ("trades.payoff_ratio", m.trades.payoff_ratio),
        ("trades.expectancy", m.trades.expectancy),
        ("trades.kelly_fraction", m.trades.kelly_fraction),
        ("trades.average_win", m.trades.average_win),
        ("trades.average_loss", m.trades.average_loss),
        ("trades.largest_win", m.trades.largest_win),
        ("trades.largest_loss", m.trades.largest_loss),
        ("trades.average_return_pct", m.trades.average_return_pct),
        ("trades.average_bars", m.trades.average_bars),
        ("trades.min_bars", m.trades.min_bars.map(|v| v as Real)),
        ("trades.max_bars", m.trades.max_bars.map(|v| v as Real)),
    ]
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

#[cfg(test)]
mod tests {
    use super::*;
    use fugazi::Fill;

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

    fn sample_metrics() -> Metrics {
        // Two long round trips, +10 then -5, with a non-degenerate variance so
        // Sharpe/Sortino are populated (not `None`).
        let orders = vec![
            order(Side::Buy, 1.0, 100.0),
            order(Side::Sell, 1.0, 110.0),
            order(Side::Buy, 1.0, 108.0),
            order(Side::Sell, 1.0, 103.0),
        ];
        let fills: Vec<Fill<String>> = orders
            .into_iter()
            .enumerate()
            .map(|(bar, order)| Fill { bar, order })
            .collect();
        let report = RunReport {
            equity_curve: vec![100.0, 105.0, 110.0, 108.0, 103.0],
            fills,
            initial_equity: 100.0,
        };
        from_report(&report, 252.0, 0.0)
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

    /// Record every numeric leaf of `v` as a dotted path, in document order.
    fn numeric_leaves(v: &serde_json::Value, cur: &mut Vec<String>, out: &mut Vec<(String, Real)>) {
        if let serde_json::Value::Object(map) = v {
            for (k, child) in map {
                cur.push(k.clone());
                if let Some(n) = child.as_f64() {
                    out.push((cur.join("."), n));
                }
                numeric_leaves(child, cur, out);
                cur.pop();
            }
        }
    }

    /// `flatten` is the CSV's column catalogue; keep it in lock-step with the
    /// serialized document both ways — every serialized leaf must appear in
    /// `flatten` with the same value, and every populated `flatten` entry must
    /// resolve in the document.
    #[test]
    fn flatten_matches_serialized_document() {
        let m = sample_metrics();
        let flat = flatten(&m);
        let doc = serde_json::to_value(&m).unwrap();

        let mut leaves = Vec::new();
        numeric_leaves(&doc, &mut Vec::new(), &mut leaves);
        for (path, value) in &leaves {
            let (_, got) = flat
                .iter()
                .find(|(name, _)| name == path)
                .unwrap_or_else(|| panic!("serialized leaf `{path}` missing from flatten"));
            assert_eq!(got.unwrap(), *value, "value mismatch at `{path}`");
        }
        for (name, value) in &flat {
            if value.is_some() {
                assert!(
                    leaves.iter().any(|(path, _)| path == name),
                    "flatten entry `{name}` is populated but absent from the document"
                );
            }
        }
    }

    #[test]
    fn windowed_from_report_splits_and_rebases() {
        // 5 bars, window 2 → windows [0,1], [2,3], [4,4]; one fill on bar 3.
        let report = RunReport {
            equity_curve: vec![100.0, 105.0, 110.0, 108.0, 103.0],
            fills: vec![Fill {
                bar: 3,
                order: order(Side::Buy, 1.0, 108.0),
            }],
            initial_equity: 100.0,
        };
        let windows = windowed_from_report(&report, 2, 252.0, 0.0);
        assert_eq!(windows.len(), 3);
        assert_eq!(
            windows
                .iter()
                .map(|w| (w.start_bar, w.end_bar))
                .collect::<Vec<_>>(),
            vec![(0, 1), (2, 3), (4, 4)]
        );
        // Each window's initial equity is the equity marked on the bar before it.
        assert_eq!(windows[0].metrics.run.initial_equity, 100.0);
        assert_eq!(windows[1].metrics.run.initial_equity, 105.0);
        assert_eq!(windows[2].metrics.run.initial_equity, 108.0);
        assert_eq!(windows[2].metrics.run.bars, 1); // partial last window
        // The bar-3 fill lands in the second window only.
        assert_eq!(windows[0].metrics.trades.total_fills, 0);
        assert_eq!(windows[1].metrics.trades.total_fills, 1);
        assert_eq!(windows[2].metrics.trades.total_fills, 0);
        // Windowed total returns compound back to the whole-run total return.
        let whole = from_report(&report, 252.0, 0.0).returns.total;
        let compounded: Real = windows
            .iter()
            .map(|w| 1.0 + w.metrics.returns.total)
            .product::<Real>()
            - 1.0;
        assert!((whole - compounded).abs() < 1e-12);
    }
}
