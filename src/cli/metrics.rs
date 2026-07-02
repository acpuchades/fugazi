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
}
