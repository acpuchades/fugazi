//! Post-run backtest evaluation metrics, written to `metrics.yml`.
//!
//! Derived from the two artefacts the run already produces in memory: the
//! **equity curve** (one point per bar, from the wallet's mark-to-market) and
//! the **fill blotter** (one entry per booked order, tagged with its bar
//! index). All ratios treat the risk-free rate as zero and annualize per-bar
//! figures with `bars_per_year` (default 252, adjustable via
//! `--bars-per-year`), so a non-daily bar cadence needs the flag to keep the
//! annualized numbers meaningful.
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
//! starting cash, and the annualization factor — no filesystem, no logging.
//! A future `optimize` subcommand doing a grid search over params can run the
//! backtest for each combination, call `compute` on each result, and pick a
//! winner by any field on [`Metrics`] (Sharpe/Sortino/CAGR/max drawdown/…) —
//! then optionally call [`write_yaml`] once for the retained combination.
//! [`Metrics`] is `Clone` so a grid-search caller can keep the full metrics
//! per combination in memory without re-running the backtest to inspect them.

use std::path::Path;

use anyhow::{Context, Result};
use fugazi::prelude::*;
use serde::Serialize;

/// Below this magnitude, a residual position after a reducing fill is treated
/// as fully flat — the same 1e-8 threshold the wallet uses for zero-delta
/// orders, kept local so the metrics module doesn't lean on a re-export.
const EPSILON: Real = 1e-8;

/// A closed round-trip position reconstructed from the blotter: its realized
/// PnL, the fill count and bar span it took, and its direction.
struct ClosedTrade {
    /// Realized PnL in reference (quote) currency.
    pnl: Real,
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
/// reader can still identify the run: bar count, starting/ending equity, and
/// the `bars_per_year` used for annualization.
#[derive(Clone, Debug, Serialize)]
pub struct RunSection {
    pub bars: usize,
    pub initial_equity: Real,
    pub final_equity: Real,
    pub bars_per_year: Real,
}

/// Return metrics. Per-bar figures are exact; the `annualized_*` figures scale
/// the per-bar mean by `bars_per_year` and the stddev by `sqrt(bars_per_year)`,
/// so they only make sense when the bar cadence matches the flag.
#[derive(Clone, Debug, Serialize)]
pub struct ReturnSection {
    pub total: Real,
    pub total_pct: Real,
    /// Compound annual growth rate (CAGR), or `None` for a non-positive equity path.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub cagr_pct: Option<Real>,
    pub mean_bar: Real,
    pub stddev_bar: Real,
    pub annualized_mean_pct: Real,
    pub annualized_volatility_pct: Real,
}

/// Risk-adjusted ratios (all annualized, rf = 0). Each is `None` when its
/// denominator is degenerate — zero stddev, no downside bar, or zero drawdown.
#[derive(Clone, Debug, Serialize)]
pub struct RiskAdjustedSection {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub sharpe: Option<Real>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub sortino: Option<Real>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub calmar: Option<Real>,
}

/// The worst peak-to-trough drop in the equity curve, and how long the curve
/// stayed underwater from that peak until it recovered (or the run ended).
#[derive(Clone, Debug, Serialize)]
pub struct DrawdownSection {
    pub max: Real,
    pub max_pct: Real,
    pub max_duration_bars: usize,
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
    #[serde(skip_serializing_if = "Option::is_none")]
    pub win_rate_pct: Option<Real>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub profit_factor: Option<Real>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub expectancy: Option<Real>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub average_win: Option<Real>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub average_loss: Option<Real>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub largest_win: Option<Real>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub largest_loss: Option<Real>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub average_bars: Option<Real>,
}

/// Compute the metrics for one run from its equity curve and fill blotter.
///
/// `equity_curve` is one point per bar (post mark-to-market). `fills` is the
/// blotter as `(bar_index, order)` pairs — the shape a run driver naturally
/// produces (see `backtest.rs`), and the shape a future `optimize` grid-search
/// caller will produce for each param combination. `cash` is the initial funds
/// echoed in the `run` block; `bars_per_year` scales per-bar return moments to
/// annual figures.
pub fn compute(
    equity_curve: &[Real],
    fills: &[(usize, Order<String>)],
    cash: Real,
    bars_per_year: Real,
) -> Metrics {
    let bars = equity_curve.len();
    let final_equity = equity_curve.last().copied().unwrap_or(cash);

    let bar_returns = per_bar_returns(equity_curve, cash);
    let (mean, stddev) = mean_stddev(&bar_returns);
    let downside = downside_stddev(&bar_returns);

    let total = if cash != 0.0 {
        (final_equity - cash) / cash
    } else {
        0.0
    };
    let cagr = compute_cagr(cash, final_equity, bars, bars_per_year);

    let ann_mean = mean * bars_per_year;
    let ann_vol = stddev * bars_per_year.sqrt();
    let ann_downside = downside * bars_per_year.sqrt();

    let sharpe = safe_div(ann_mean, ann_vol);
    let sortino = safe_div(ann_mean, ann_downside);

    let dd = max_drawdown(equity_curve);
    let calmar = cagr.and_then(|c| safe_div(c / 100.0, dd.max));

    let trades = reconstruct_trades(fills);
    let trade_section = build_trade_section(&trades, fills.len());

    Metrics {
        run: RunSection {
            bars,
            initial_equity: cash,
            final_equity,
            bars_per_year,
        },
        returns: ReturnSection {
            total,
            total_pct: total * 100.0,
            cagr_pct: cagr,
            mean_bar: mean,
            stddev_bar: stddev,
            annualized_mean_pct: ann_mean * 100.0,
            annualized_volatility_pct: ann_vol * 100.0,
        },
        risk_adjusted: RiskAdjustedSection {
            sharpe,
            sortino,
            calmar,
        },
        drawdown: DrawdownSection {
            max: dd.max,
            max_pct: dd.max * 100.0,
            max_duration_bars: dd.duration,
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

/// Population mean and stddev of a sample. Empty input → `(0, 0)`; a
/// single-sample stddev is `0` by construction.
fn mean_stddev(xs: &[Real]) -> (Real, Real) {
    if xs.is_empty() {
        return (0.0, 0.0);
    }
    let n = xs.len() as Real;
    let mean = xs.iter().sum::<Real>() / n;
    let var = xs.iter().map(|x| (x - mean).powi(2)).sum::<Real>() / n;
    (mean, var.sqrt())
}

/// Downside stddev (Sortino denominator): `sqrt(mean(min(0, r)^2))`. Positive
/// bars are floored to zero, so a run with no losing bar returns `0`.
fn downside_stddev(xs: &[Real]) -> Real {
    if xs.is_empty() {
        return 0.0;
    }
    let n = xs.len() as Real;
    let sum_sq = xs.iter().map(|x| x.min(0.0).powi(2)).sum::<Real>();
    (sum_sq / n).sqrt()
}

/// Compound annual growth rate as a percentage.
///
/// Returns `None` when the equity path is non-positive at either endpoint (the
/// ratio would be undefined) or the run is empty.
fn compute_cagr(cash: Real, final_equity: Real, bars: usize, bars_per_year: Real) -> Option<Real> {
    if cash <= 0.0 || final_equity <= 0.0 || bars == 0 || bars_per_year <= 0.0 {
        return None;
    }
    let years = bars as Real / bars_per_year;
    if years <= 0.0 {
        return None;
    }
    let cagr = (final_equity / cash).powf(1.0 / years) - 1.0;
    Some(cagr * 100.0)
}

/// The worst peak-to-trough drawdown of an equity curve, as a positive
/// fraction of the peak, and its underwater duration in bars.
struct Drawdown {
    max: Real,
    duration: usize,
}

fn max_drawdown(equity: &[Real]) -> Drawdown {
    let mut peak = Real::MIN;
    let mut peak_idx = 0;
    let mut worst = 0.0_f64;
    let mut worst_duration = 0;
    for (i, &e) in equity.iter().enumerate() {
        if e > peak {
            peak = e;
            peak_idx = i;
        }
        let dd = if peak > 0.0 { (peak - e) / peak } else { 0.0 };
        if dd > worst {
            worst = dd;
            worst_duration = i - peak_idx;
        }
    }
    Drawdown {
        max: worst,
        duration: worst_duration,
    }
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
                trades.push(ClosedTrade {
                    pnl: pnl_per_unit * close_units,
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

fn build_trade_section(trades: &[ClosedTrade], total_fills: usize) -> TradeSection {
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
    let average_bars = if total > 0 {
        Some(trades.iter().map(|t| t.bars as Real).sum::<Real>() / total as Real)
    } else {
        None
    };

    TradeSection {
        total,
        wins,
        losses,
        flat,
        long_trades,
        short_trades,
        total_fills,
        win_rate_pct,
        profit_factor,
        expectancy,
        average_win,
        average_loss,
        largest_win,
        largest_loss,
        average_bars,
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
    fn drawdown_finds_worst_peak_to_trough() {
        let dd = max_drawdown(&[100.0, 110.0, 105.0, 90.0, 95.0, 120.0]);
        assert!((dd.max - (110.0 - 90.0) / 110.0).abs() < 1e-9);
        assert_eq!(dd.duration, 2); // peak at idx 1, trough at idx 3
    }

    #[test]
    fn safe_div_guards_zero_denominator() {
        assert_eq!(safe_div(1.0, 0.0), None);
        assert_eq!(safe_div(1.0, -1.0), None);
        assert_eq!(safe_div(1.0, 2.0), Some(0.5));
    }
}
