//! Standalone performance metrics — one function per metric — reducing a
//! backtest's [`equity_curve`](crate::BacktestReport::equity_curve) and
//! [`fills`](crate::BacktestReport::fills) to the numbers a reader cares about
//! (return moments, Sharpe/Sortino/Calmar, drawdown analytics, round-trip trade
//! statistics).
//!
//! **No aggregate `compute` — every metric is its own [`pub fn`]**. A caller
//! picks whichever numbers matter and calls those directly; a caller that wants
//! all of them calls all of them. Metrics that share an expensive intermediate
//! (per-bar returns, reconstructed round-trip trades, drawdown segments) take
//! that intermediate as their input, and each intermediate is built by its own
//! public function — so a caller reducing an equity curve to a report builds
//! each intermediate once and hands it to every downstream metric.
//!
//! # Units
//!
//! Metrics are returned in their **natural units** (fractions, ratios, bar
//! counts) — no `_pct` scaling. `0.15` from [`total_return`] is a +15% run;
//! multiply by `100.0` at the presentation layer if you want percent. Return
//! moments are per-bar; use [`annualized_return`] / [`annualized_volatility`]
//! to scale by `bars_per_year`.
//!
//! # Degenerate cases
//!
//! Ratios whose denominator can vanish return `Option<Real>` and read `None`
//! in that case (zero variance for Sharpe, no losing trade for a profit factor,
//! non-positive endpoints for CAGR, …). Metrics that are always well-defined
//! (total return, max drawdown, positive-bars fraction, …) return `Real` and
//! read `0.0` on empty input.

use crate::backtest::Fill;
use crate::{Real, Side};

// ---------------------------------------------------------------------------
// Intermediate types
// ---------------------------------------------------------------------------

/// A closed round-trip trade reconstructed from the fill blotter by
/// [`reconstruct_trades`]. Same-side fills extend the open leg with a
/// volume-weighted entry; opposite-side fills close (or reverse) it, producing
/// one [`Trade`] per closed leg.
#[derive(Debug, Clone, Copy)]
pub struct Trade {
    /// Bar index at which the leg was opened (or last re-averaged).
    pub entry_bar: usize,
    /// Bar index at which the leg was closed.
    pub exit_bar: usize,
    /// Whether the opening side was long ([`Side::Buy`]) or short ([`Side::Sell`]).
    pub side: Side,
    /// The magnitude of the closed leg, in instrument units.
    pub units: Real,
    /// Volume-weighted average price of the opening leg.
    pub entry_price: Real,
    /// Fill price of the closing leg.
    pub exit_price: Real,
    /// Realized PnL in reference (quote) currency.
    pub pnl: Real,
    /// PnL as a fraction of the entry notional (`pnl / (entry_price * units)`);
    /// `0.0` when the entry notional is degenerate.
    pub return_ratio: Real,
}

impl Trade {
    /// Bar count from entry to exit — `exit_bar - entry_bar` (`0` on a same-bar
    /// open+close).
    pub fn bars_held(&self) -> usize {
        self.exit_bar - self.entry_bar
    }
}

/// One drawdown segment: a peak → trough → recovery-or-end stretch where the
/// equity curve was below a prior peak. Built by [`drawdown_segments`].
#[derive(Debug, Clone, Copy)]
pub struct DrawdownSegment {
    /// Bar index of the pre-drawdown peak.
    pub peak_bar: usize,
    /// Bar index of the deepest point in the segment.
    pub trough_bar: usize,
    /// `(peak - trough) / peak`, in fractional form; always non-negative.
    pub depth_ratio: Real,
    /// Peak-to-trough distance in bars (`trough_bar - peak_bar`).
    pub duration_bars: usize,
    /// Bars strictly below the peak in this segment (excluding the peak and
    /// any recovery bar). Used by [`time_in_drawdown_ratio`].
    pub underwater_bars: usize,
}

// ---------------------------------------------------------------------------
// Intermediate builders
// ---------------------------------------------------------------------------

/// Per-bar fractional return series: `(equity[i] - prev) / prev`, seeded from
/// `initial_equity` for the first bar. Zero-denominator bars contribute `0.0`.
/// The returned vector has the same length as `equity_curve`.
pub fn per_bar_returns(equity_curve: &[Real], initial_equity: Real) -> Vec<Real> {
    let mut out = Vec::with_capacity(equity_curve.len());
    let mut prev = initial_equity;
    for &e in equity_curve {
        let r = if prev != 0.0 { (e - prev) / prev } else { 0.0 };
        out.push(r);
        prev = e;
    }
    out
}

/// Walk `fills` with a single signed position and a volume-weighted entry
/// price, producing one [`Trade`] per closed leg.
///
/// Same-side fills add to the open leg with a volume-weighted new entry. An
/// opposite-side fill closes (partially or fully) and — if it crosses zero —
/// re-opens the remainder at the same fill price as a fresh trade. So one
/// reversal (`set(Buy, all-in)` while short) yields one closed short plus one
/// open long, matching how a
/// [`SingleAssetStrategy`](crate::strategies::SingleAssetStrategy) reasons
/// about its position.
pub fn reconstruct_trades<Sym>(fills: &[Fill<Sym>]) -> Vec<Trade> {
    struct Open {
        signed_units: Real,
        entry_price: Real,
        entry_bar: usize,
    }

    let mut trades = Vec::new();
    let mut open: Option<Open> = None;

    for f in fills {
        let delta = f.order.signed_units();
        let bar = f.bar;
        let price = f.order.price;

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
                let notional = pos.signed_units.abs() * pos.entry_price + delta.abs() * price;
                pos.entry_price = notional / new_units.abs();
                pos.signed_units = new_units;
            }
            Some(pos) => {
                // Opposite side: reducing, closing, or reversing.
                let close_units = pos.signed_units.abs().min(delta.abs());
                let long = pos.signed_units > 0.0;
                let side = if long { Side::Buy } else { Side::Sell };
                let pnl_per_unit = if long {
                    price - pos.entry_price
                } else {
                    pos.entry_price - price
                };
                let pnl = pnl_per_unit * close_units;
                let entry_notional = pos.entry_price * close_units;
                let return_ratio = if entry_notional > 0.0 {
                    pnl / entry_notional
                } else {
                    0.0
                };
                trades.push(Trade {
                    entry_bar: pos.entry_bar,
                    exit_bar: bar,
                    side,
                    units: close_units,
                    entry_price: pos.entry_price,
                    exit_price: price,
                    pnl,
                    return_ratio,
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

/// Build the drawdown segments of `equity_curve` — one entry per peak → trough
/// → recovery-or-end stretch. A monotone-non-decreasing curve produces an
/// empty vector.
pub fn drawdown_segments(equity_curve: &[Real]) -> Vec<DrawdownSegment> {
    if equity_curve.is_empty() {
        return Vec::new();
    }

    let mut peak = equity_curve[0];
    let mut peak_idx = 0;
    let mut in_dd = false;
    let mut trough = peak;
    let mut trough_idx = 0;
    let mut underwater = 0usize;
    let mut segments = Vec::new();

    for (i, &e) in equity_curve.iter().enumerate() {
        if e > peak {
            if in_dd {
                let depth = if peak > 0.0 {
                    (peak - trough) / peak
                } else {
                    0.0
                };
                segments.push(DrawdownSegment {
                    peak_bar: peak_idx,
                    trough_bar: trough_idx,
                    depth_ratio: depth,
                    duration_bars: trough_idx - peak_idx,
                    underwater_bars: underwater,
                });
                in_dd = false;
                underwater = 0;
            }
            peak = e;
            peak_idx = i;
        } else if e < peak {
            underwater += 1;
            if !in_dd {
                in_dd = true;
                trough = e;
                trough_idx = i;
            } else if e < trough {
                trough = e;
                trough_idx = i;
            }
        }
    }
    if in_dd {
        let depth = if peak > 0.0 {
            (peak - trough) / peak
        } else {
            0.0
        };
        segments.push(DrawdownSegment {
            peak_bar: peak_idx,
            trough_bar: trough_idx,
            depth_ratio: depth,
            duration_bars: trough_idx - peak_idx,
            underwater_bars: underwater,
        });
    }

    segments
}

// ---------------------------------------------------------------------------
// Return moments and distribution shape
// ---------------------------------------------------------------------------

/// Arithmetic mean of `returns`. `0.0` on an empty input.
pub fn mean_return(returns: &[Real]) -> Real {
    if returns.is_empty() {
        0.0
    } else {
        returns.iter().sum::<Real>() / returns.len() as Real
    }
}

/// Median of `returns`. `0.0` on an empty input; the mean of the two middle
/// values on even-length input.
pub fn median_return(returns: &[Real]) -> Real {
    if returns.is_empty() {
        return 0.0;
    }
    let sorted = sorted_asc(returns);
    let n = sorted.len();
    if n.is_multiple_of(2) {
        (sorted[n / 2 - 1] + sorted[n / 2]) / 2.0
    } else {
        sorted[n / 2]
    }
}

/// Sample (Bessel-corrected, `ddof=1`) standard deviation of `returns`. `0.0`
/// on empty or single-sample input.
///
/// The `ddof=1` divisor matches empyrical / pyfolio / quantstats and Excel's
/// `STDEV`, so this reads identically to those references.
pub fn stddev_return(returns: &[Real]) -> Real {
    mean_stddev(returns).1
}

/// Largest single-bar return, or `0.0` on empty input.
pub fn best_return(returns: &[Real]) -> Real {
    returns.iter().copied().reduce(Real::max).unwrap_or(0.0)
}

/// Smallest single-bar return, or `0.0` on empty input.
pub fn worst_return(returns: &[Real]) -> Real {
    returns.iter().copied().reduce(Real::min).unwrap_or(0.0)
}

/// Fraction of bars with a strictly positive return. `0.0` on empty input.
pub fn positive_bars_ratio(returns: &[Real]) -> Real {
    if returns.is_empty() {
        return 0.0;
    }
    let n = returns.iter().filter(|&&r| r > 0.0).count() as Real;
    n / returns.len() as Real
}

/// Biased (population) skewness — the classical `g1 = m3 / m2^(3/2)` over
/// central moments with an `n` divisor. Matches `scipy.stats.skew(bias=True)`.
/// `None` when the second moment is zero.
pub fn skewness(returns: &[Real]) -> Option<Real> {
    if returns.is_empty() {
        return None;
    }
    let mean = mean_return(returns);
    let n = returns.len() as Real;
    let m2 = returns.iter().map(|x| (x - mean).powi(2)).sum::<Real>() / n;
    if m2 == 0.0 {
        return None;
    }
    let m3 = returns.iter().map(|x| (x - mean).powi(3)).sum::<Real>() / n;
    Some(m3 / m2.powf(1.5))
}

/// Biased excess kurtosis — `g2 = m4 / m2^2 − 3`, so a normal distribution
/// reads `0.0`. Matches `scipy.stats.kurtosis(bias=True, fisher=True)`. `None`
/// when the second moment is zero.
pub fn kurtosis(returns: &[Real]) -> Option<Real> {
    if returns.is_empty() {
        return None;
    }
    let mean = mean_return(returns);
    let n = returns.len() as Real;
    let m2 = returns.iter().map(|x| (x - mean).powi(2)).sum::<Real>() / n;
    if m2 == 0.0 {
        return None;
    }
    let m4 = returns.iter().map(|x| (x - mean).powi(4)).sum::<Real>() / n;
    Some(m4 / m2.powi(2) - 3.0)
}

/// Historical Value-at-Risk of `returns` at `confidence` (e.g. `0.95` for the
/// classic 95%-VaR): the magnitude of the `(1 - confidence)`-quantile of the
/// return distribution, expressed as a positive loss fraction. Negative when
/// even the tail quantile is a gain (no meaningful downside).
///
/// `0.0` on empty input.
pub fn value_at_risk(returns: &[Real], confidence: Real) -> Real {
    if returns.is_empty() {
        return 0.0;
    }
    let sorted = sorted_asc(returns);
    -percentile(&sorted, 1.0 - confidence)
}

/// Historical Conditional VaR (Expected Shortfall) of `returns` at
/// `confidence`: mean of the bottom-`(1 - confidence)` return tail, expressed
/// as a positive loss fraction. `0.0` on empty input.
pub fn conditional_value_at_risk(returns: &[Real], confidence: Real) -> Real {
    if returns.is_empty() {
        return 0.0;
    }
    let sorted = sorted_asc(returns);
    -tail_mean(&sorted, 1.0 - confidence)
}

/// `|P95(returns)| / |P5(returns)|` (with 5th/95th percentiles), a coarse
/// symmetry check on the tails. `None` when the 5th-percentile magnitude is
/// zero.
pub fn tail_ratio(returns: &[Real]) -> Option<Real> {
    if returns.is_empty() {
        return None;
    }
    let sorted = sorted_asc(returns);
    let p95 = percentile(&sorted, 0.95).abs();
    let p5 = percentile(&sorted, 0.05).abs();
    safe_div(p95, p5)
}

// ---------------------------------------------------------------------------
// Compound return metrics
// ---------------------------------------------------------------------------

/// Total return as a fraction: `(final - initial) / initial`. `0.0` when the
/// initial equity is zero.
pub fn total_return(equity_curve: &[Real], initial_equity: Real) -> Real {
    let final_equity = equity_curve.last().copied().unwrap_or(initial_equity);
    if initial_equity != 0.0 {
        (final_equity - initial_equity) / initial_equity
    } else {
        0.0
    }
}

/// Compound annual growth rate as a fraction (e.g. `0.15` for +15% p.a.).
///
/// `None` when the equity path is non-positive at either endpoint (the ratio
/// would be undefined), the run is empty, or `bars_per_year <= 0`.
pub fn cagr(equity_curve: &[Real], initial_equity: Real, bars_per_year: Real) -> Option<Real> {
    let bars = equity_curve.len();
    let final_equity = equity_curve.last().copied().unwrap_or(initial_equity);
    cagr_fraction(initial_equity, final_equity, bars, bars_per_year)
}

/// Arithmetic mean of `returns` scaled by `bars_per_year` (the classical
/// annualization convention).
pub fn annualized_return(returns: &[Real], bars_per_year: Real) -> Real {
    mean_return(returns) * bars_per_year
}

/// Sample stddev of `returns` scaled by `sqrt(bars_per_year)` (the classical
/// annualization convention).
pub fn annualized_volatility(returns: &[Real], bars_per_year: Real) -> Real {
    stddev_return(returns) * bars_per_year.max(0.0).sqrt()
}

// ---------------------------------------------------------------------------
// Risk-adjusted ratios
// ---------------------------------------------------------------------------

/// Annualized Sharpe ratio: `(annualized_return - risk_free_rate) /
/// annualized_volatility`. `None` when the annualized volatility is zero.
///
/// `risk_free_rate` is the annualized rf as a fraction (`0.045` = 4.5% p.a.).
pub fn sharpe(returns: &[Real], risk_free_rate: Real, bars_per_year: Real) -> Option<Real> {
    let ann_excess = annualized_return(returns, bars_per_year) - risk_free_rate;
    let ann_vol = annualized_volatility(returns, bars_per_year);
    safe_div(ann_excess, ann_vol)
}

/// Annualized Sortino ratio: `(annualized_return - risk_free_rate) /
/// annualized_downside_deviation`. The downside deviation uses the per-bar rf
/// as its Minimum Acceptable Return and an `n` divisor (matches empyrical's
/// `downside_risk`). `None` when every bar clears the threshold or `returns`
/// is empty.
pub fn sortino(returns: &[Real], risk_free_rate: Real, bars_per_year: Real) -> Option<Real> {
    let rf_per_bar = if bars_per_year > 0.0 {
        risk_free_rate / bars_per_year
    } else {
        0.0
    };
    let ann_excess = annualized_return(returns, bars_per_year) - risk_free_rate;
    let ann_downside = downside_stddev(returns, rf_per_bar) * bars_per_year.max(0.0).sqrt();
    safe_div(ann_excess, ann_downside)
}

/// Calmar ratio: `cagr / max_drawdown`. `None` when the max drawdown is zero
/// or [`cagr`] is undefined.
pub fn calmar(equity_curve: &[Real], initial_equity: Real, bars_per_year: Real) -> Option<Real> {
    let c = cagr(equity_curve, initial_equity, bars_per_year)?;
    let dd = max_drawdown(&drawdown_segments(equity_curve));
    safe_div(c, dd)
}

/// Omega ratio at `threshold`: `Σ max(r − τ, 0) / Σ max(τ − r, 0)`. `None`
/// when every return clears the threshold (no downside integral).
///
/// For an annualized rf comparison, pass the per-bar rate (`rf / bars_per_year`)
/// as `threshold`.
pub fn omega(returns: &[Real], threshold: Real) -> Option<Real> {
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

/// Peter Martin's Ulcer Index in fractional form: the root-mean-squared
/// drawdown, where each bar's drawdown is `(equity[i] − running_peak[i]) /
/// running_peak[i]`. Bars at or above the running peak contribute zero, so a
/// monotone-non-decreasing curve gives `0.0`.
pub fn ulcer_index(equity_curve: &[Real]) -> Real {
    if equity_curve.is_empty() {
        return 0.0;
    }
    let mut peak = 0.0_f64;
    let mut sum_sq = 0.0;
    for &e in equity_curve {
        if e > peak {
            peak = e;
        }
        if peak > 0.0 {
            let d = (e - peak) / peak; // ≤ 0
            sum_sq += d * d;
        }
    }
    (sum_sq / equity_curve.len() as Real).sqrt()
}

/// Ulcer Performance Index: `(cagr − risk_free_rate) / ulcer_index`. `None`
/// when either the CAGR or the UI is degenerate.
pub fn ulcer_performance_index(
    equity_curve: &[Real],
    initial_equity: Real,
    risk_free_rate: Real,
    bars_per_year: Real,
) -> Option<Real> {
    let c = cagr(equity_curve, initial_equity, bars_per_year)?;
    let ui = ulcer_index(equity_curve);
    safe_div(c - risk_free_rate, ui)
}

// ---------------------------------------------------------------------------
// Higher-moment / multiple-testing Sharpe corrections
// ---------------------------------------------------------------------------

/// Euler–Mascheroni constant, used in [`deflated_sharpe`]'s max-Sharpe
/// expectation.
const EULER_MASCHERONI: Real = 0.577_215_664_901_532_9;

/// Probabilistic Sharpe Ratio (Bailey & López de Prado, 2012): the probability
/// that the true per-bar Sharpe of the return-generating process exceeds
/// `benchmark_sharpe`, given the observed Sharpe over `returns` and the
/// higher-moment shape (skewness + kurtosis) of the empirical distribution.
///
/// Answers *"is my whole-run Sharpe reliably above the benchmark given `T`
/// bars and fat tails?"* — the natural companion to a raw [`sharpe`] read.
///
/// # Arguments
///
/// * `returns` — the per-bar return series (built once by [`per_bar_returns`]).
/// * `risk_free_rate`, `bars_per_year` — as in [`sharpe`]; determine the
///   annualization used for both the observed Sharpe and `benchmark_sharpe`.
/// * `benchmark_sharpe` — the reference **annualized** Sharpe to test against.
///   `0.0` is the classical "is it above zero?" test.
///
/// Returns `Some(p)` in `[0.0, 1.0]`; `None` when `returns.len() < 2`, when
/// [`sharpe`] / [`skewness`] / [`kurtosis`] are undefined, or when the
/// higher-moment adjustment denominator vanishes.
///
/// If your caller already has the observed Sharpe, skewness, and kurtosis
/// pre-aggregated (e.g. the `optimize` grid where every row's [`Metrics`]
/// carries them), use [`probabilistic_sharpe_from_stats`] to skip re-scanning
/// the returns vector.
pub fn probabilistic_sharpe(
    returns: &[Real],
    risk_free_rate: Real,
    bars_per_year: Real,
    benchmark_sharpe: Real,
) -> Option<Real> {
    let n = returns.len();
    if n < 2 || bars_per_year <= 0.0 {
        return None;
    }
    probabilistic_sharpe_from_stats(
        sharpe(returns, risk_free_rate, bars_per_year),
        skewness(returns),
        kurtosis(returns),
        n,
        bars_per_year,
        benchmark_sharpe,
    )
}

/// The Probabilistic Sharpe test statistic computed from pre-aggregated
/// inputs — the same formula [`probabilistic_sharpe`] evaluates, but a caller
/// that already has the per-run Sharpe / skewness / excess kurtosis (say, from
/// a [`Metrics`](crate::metrics)-shaped summary) can skip the per-bar rescan.
///
/// All three `_annualized`/moment inputs are `Option`-typed to mirror the
/// upstream `sharpe`/`skewness`/`kurtosis` fns (each is `None` on degenerate
/// input); this fn propagates that: any `None` in → `None` out.
///
/// # Arguments
///
/// * `sharpe_annualized` — the observed annualized Sharpe, as returned by
///   [`sharpe`].
/// * `skewness_biased`, `excess_kurtosis` — biased skewness (`g1`) and *excess*
///   kurtosis (`g2 = γ₄ − 3`), matching [`skewness`] / [`kurtosis`].
/// * `n_returns` — the number of return observations behind those statistics.
/// * `bars_per_year`, `benchmark_sharpe` — as in [`probabilistic_sharpe`].
pub fn probabilistic_sharpe_from_stats(
    sharpe_annualized: Option<Real>,
    skewness_biased: Option<Real>,
    excess_kurtosis: Option<Real>,
    n_returns: usize,
    bars_per_year: Real,
    benchmark_sharpe: Real,
) -> Option<Real> {
    use statrs::distribution::{ContinuousCDF, Normal};

    if n_returns < 2 || bars_per_year <= 0.0 {
        return None;
    }
    let sr_ann = sharpe_annualized?;
    let skew = skewness_biased?;
    let excess_kurt = excess_kurtosis?;

    // The PSR test statistic is in per-bar Sharpe units; un-annualize both
    // sides by √bars_per_year (matches `annualized_volatility`'s convention).
    let scale = bars_per_year.sqrt();
    let sr = sr_ann / scale;
    let bench = benchmark_sharpe / scale;

    // Higher-moment adjustment: 1 − γ₃·SR + (γ₄ − 1)/4 · SR², where γ₄ is raw
    // (Pearson) kurtosis. `kurtosis` returns *excess* kurtosis (γ₄ − 3), so
    // (γ₄ − 1)/4 = (excess_kurt + 2)/4.
    let denom_sq = 1.0 - skew * sr + (excess_kurt + 2.0) / 4.0 * sr * sr;
    if !(denom_sq > 0.0 && denom_sq.is_finite()) {
        return None;
    }
    let z = (sr - bench) * ((n_returns - 1) as Real).sqrt() / denom_sq.sqrt();
    if !z.is_finite() {
        return None;
    }
    Some(Normal::standard().cdf(z))
}

/// Deflated Sharpe Ratio (Bailey & López de Prado, 2014): the probability
/// that the true per-bar Sharpe exceeds the expected maximum Sharpe under a
/// normal null across `n_trials` independent trials — i.e. PSR against the
/// selection-bias-adjusted benchmark `E[max SR]`.
///
/// Answers *"I picked the best of `n_trials` (parameter cells, windows, …);
/// is the winner's Sharpe real or just the peak of the null distribution?"*
///
/// # Arguments
///
/// * `returns` — the **selected** trial's per-bar returns.
/// * `risk_free_rate`, `bars_per_year` — as in [`sharpe`]; the annualization
///   applied to both the observed Sharpe and `trial_sharpe_variance`.
/// * `n_trials` — number of candidate trials the winner was selected from
///   (e.g. size of the parameter grid). Must be `≥ 2`.
/// * `trial_sharpe_variance` — variance of the **annualized** Sharpe estimates
///   across those trials.
///
/// Returns `None` when `n_trials < 2`, the trial variance is non-positive, or
/// the underlying PSR is undefined.
///
/// If the observed Sharpe / skew / kurt are already known, use
/// [`deflated_sharpe_from_stats`] to skip re-scanning `returns`.
pub fn deflated_sharpe(
    returns: &[Real],
    risk_free_rate: Real,
    bars_per_year: Real,
    n_trials: usize,
    trial_sharpe_variance: Real,
) -> Option<Real> {
    let n = returns.len();
    if n < 2 {
        return None;
    }
    deflated_sharpe_from_stats(
        sharpe(returns, risk_free_rate, bars_per_year),
        skewness(returns),
        kurtosis(returns),
        n,
        bars_per_year,
        n_trials,
        trial_sharpe_variance,
    )
}

/// The Deflated Sharpe Ratio from pre-aggregated statistics — the stats-only
/// twin of [`deflated_sharpe`], matching [`probabilistic_sharpe_from_stats`]'s
/// input shape. The expected max Sharpe under the null is approximated by the
/// standard closed form `√V[SR] · [(1 − γ)·Φ⁻¹(1 − 1/N) + γ·Φ⁻¹(1 − 1/(N·e))]`
/// (with `γ` = Euler–Mascheroni) and passed as the benchmark to
/// [`probabilistic_sharpe_from_stats`].
#[allow(clippy::too_many_arguments)]
pub fn deflated_sharpe_from_stats(
    sharpe_annualized: Option<Real>,
    skewness_biased: Option<Real>,
    excess_kurtosis: Option<Real>,
    n_returns: usize,
    bars_per_year: Real,
    n_trials: usize,
    trial_sharpe_variance: Real,
) -> Option<Real> {
    use statrs::distribution::{ContinuousCDF, Normal};

    if n_trials < 2 || !(trial_sharpe_variance > 0.0 && trial_sharpe_variance.is_finite()) {
        return None;
    }
    let normal = Normal::standard();
    let n = n_trials as Real;
    let q1 = normal.inverse_cdf(1.0 - 1.0 / n);
    let q2 = normal.inverse_cdf(1.0 - 1.0 / (n * std::f64::consts::E));
    let sr0_annualized = trial_sharpe_variance.sqrt()
        * ((1.0 - EULER_MASCHERONI) * q1 + EULER_MASCHERONI * q2);
    probabilistic_sharpe_from_stats(
        sharpe_annualized,
        skewness_biased,
        excess_kurtosis,
        n_returns,
        bars_per_year,
        sr0_annualized,
    )
}

// ---------------------------------------------------------------------------
// Drawdown metrics
// ---------------------------------------------------------------------------

/// Deepest drawdown in `segments`, as a fraction. `0.0` on empty input.
pub fn max_drawdown(segments: &[DrawdownSegment]) -> Real {
    segments
        .iter()
        .map(|s| s.depth_ratio)
        .fold(0.0, |a, b| if b > a { b } else { a })
}

/// Peak-to-trough duration of the **deepest** drawdown segment (not the longest
/// duration overall). `0` on empty input.
pub fn max_drawdown_duration(segments: &[DrawdownSegment]) -> usize {
    segments
        .iter()
        .max_by(|a, b| {
            a.depth_ratio
                .partial_cmp(&b.depth_ratio)
                .unwrap_or(std::cmp::Ordering::Equal)
        })
        .map(|s| s.duration_bars)
        .unwrap_or(0)
}

/// Mean drawdown depth across all segments; `None` for an empty input (i.e. a
/// monotone-non-decreasing equity curve).
pub fn average_drawdown(segments: &[DrawdownSegment]) -> Option<Real> {
    if segments.is_empty() {
        None
    } else {
        Some(segments.iter().map(|s| s.depth_ratio).sum::<Real>() / segments.len() as Real)
    }
}

/// Mean peak-to-trough duration across all segments; `None` for empty input.
pub fn average_drawdown_duration(segments: &[DrawdownSegment]) -> Option<Real> {
    if segments.is_empty() {
        None
    } else {
        Some(
            segments.iter().map(|s| s.duration_bars as Real).sum::<Real>()
                / segments.len() as Real,
        )
    }
}

/// Number of drawdown segments (equivalently `segments.len()`).
pub fn drawdown_count(segments: &[DrawdownSegment]) -> usize {
    segments.len()
}

/// Fraction of bars spent below a prior peak (sum of per-segment
/// `underwater_bars`, divided by `total_bars`). `0.0` when `total_bars` is
/// zero.
pub fn time_in_drawdown_ratio(segments: &[DrawdownSegment], total_bars: usize) -> Real {
    if total_bars == 0 {
        return 0.0;
    }
    let underwater: usize = segments.iter().map(|s| s.underwater_bars).sum();
    underwater as Real / total_bars as Real
}

/// `total_return / max_drawdown` — the non-annualized cousin of Calmar. `None`
/// when the max drawdown is zero.
pub fn recovery_factor(equity_curve: &[Real], initial_equity: Real) -> Option<Real> {
    let dd = max_drawdown(&drawdown_segments(equity_curve));
    safe_div(total_return(equity_curve, initial_equity), dd)
}

// ---------------------------------------------------------------------------
// Trade metrics
// ---------------------------------------------------------------------------

/// Count of closed round-trip trades.
pub fn total_trades(trades: &[Trade]) -> usize {
    trades.len()
}

/// Count of trades with strictly positive PnL.
pub fn winning_trades(trades: &[Trade]) -> usize {
    trades.iter().filter(|t| t.pnl > 0.0).count()
}

/// Count of trades with strictly negative PnL.
pub fn losing_trades(trades: &[Trade]) -> usize {
    trades.iter().filter(|t| t.pnl < 0.0).count()
}

/// Count of trades with exactly zero PnL.
pub fn flat_trades(trades: &[Trade]) -> usize {
    trades.iter().filter(|t| t.pnl == 0.0).count()
}

/// Count of trades entered on the long side.
pub fn long_trades(trades: &[Trade]) -> usize {
    trades.iter().filter(|t| matches!(t.side, Side::Buy)).count()
}

/// Count of trades entered on the short side.
pub fn short_trades(trades: &[Trade]) -> usize {
    trades
        .iter()
        .filter(|t| matches!(t.side, Side::Sell))
        .count()
}

/// Longest consecutive run of winning trades. `0` on empty input.
pub fn max_consecutive_wins(trades: &[Trade]) -> usize {
    longest_streak(trades, |t| t.pnl > 0.0)
}

/// Longest consecutive run of losing trades. `0` on empty input.
pub fn max_consecutive_losses(trades: &[Trade]) -> usize {
    longest_streak(trades, |t| t.pnl < 0.0)
}

/// Fraction of trades with strictly positive PnL. `None` on empty input.
pub fn win_rate(trades: &[Trade]) -> Option<Real> {
    if trades.is_empty() {
        None
    } else {
        Some(winning_trades(trades) as Real / trades.len() as Real)
    }
}

/// `Σ winning_pnl / |Σ losing_pnl|` — total profit divided by total loss.
/// `None` when there are no losing trades (no denominator).
pub fn profit_factor(trades: &[Trade]) -> Option<Real> {
    let sum_wins: Real = trades.iter().map(|t| t.pnl).filter(|&p| p > 0.0).sum();
    let sum_losses: Real = trades.iter().map(|t| t.pnl).filter(|&p| p < 0.0).sum();
    safe_div(sum_wins, -sum_losses)
}

/// `average_win / |average_loss|` (count-agnostic, magnitude-weighted). `None`
/// when either input is undefined.
pub fn payoff_ratio(trades: &[Trade]) -> Option<Real> {
    match (average_win(trades), average_loss(trades)) {
        (Some(w), Some(l)) if l < 0.0 => Some(w / -l),
        _ => None,
    }
}

/// Mean PnL per trade (the trade-level expectancy). `None` on empty input.
pub fn expectancy(trades: &[Trade]) -> Option<Real> {
    if trades.is_empty() {
        None
    } else {
        Some(trades.iter().map(|t| t.pnl).sum::<Real>() / trades.len() as Real)
    }
}

/// Kelly-optimal fraction of bankroll per trade under the current win rate
/// and payoff ratio (`p − (1 − p)/b`). Can be negative (unfavourable edge).
/// `None` when either input is undefined or the payoff ratio is non-positive.
pub fn kelly_fraction(trades: &[Trade]) -> Option<Real> {
    match (win_rate(trades), payoff_ratio(trades)) {
        (Some(p), Some(b)) if b > 0.0 => Some(p - (1.0 - p) / b),
        _ => None,
    }
}

/// Mean PnL across winning trades. `None` when there are no winners.
pub fn average_win(trades: &[Trade]) -> Option<Real> {
    let wins: Vec<Real> = trades.iter().map(|t| t.pnl).filter(|&p| p > 0.0).collect();
    if wins.is_empty() {
        None
    } else {
        Some(wins.iter().sum::<Real>() / wins.len() as Real)
    }
}

/// Mean PnL across losing trades (a negative number when defined). `None` when
/// there are no losers.
pub fn average_loss(trades: &[Trade]) -> Option<Real> {
    let losses: Vec<Real> = trades.iter().map(|t| t.pnl).filter(|&p| p < 0.0).collect();
    if losses.is_empty() {
        None
    } else {
        Some(losses.iter().sum::<Real>() / losses.len() as Real)
    }
}

/// Largest single-trade PnL. `None` on empty input.
pub fn largest_win(trades: &[Trade]) -> Option<Real> {
    trades
        .iter()
        .map(|t| t.pnl)
        .filter(|&p| p > 0.0)
        .reduce(Real::max)
}

/// Most-negative single-trade PnL. `None` on empty input.
pub fn largest_loss(trades: &[Trade]) -> Option<Real> {
    trades
        .iter()
        .map(|t| t.pnl)
        .filter(|&p| p < 0.0)
        .reduce(Real::min)
}

/// Mean per-trade return as a fraction of the entry notional. `None` on empty
/// input.
pub fn average_trade_return(trades: &[Trade]) -> Option<Real> {
    if trades.is_empty() {
        None
    } else {
        Some(trades.iter().map(|t| t.return_ratio).sum::<Real>() / trades.len() as Real)
    }
}

/// Mean bars-held across trades. `None` on empty input.
pub fn average_bars_held(trades: &[Trade]) -> Option<Real> {
    if trades.is_empty() {
        None
    } else {
        Some(
            trades.iter().map(|t| t.bars_held() as Real).sum::<Real>() / trades.len() as Real,
        )
    }
}

/// Shortest bars-held across trades. `None` on empty input.
pub fn min_bars_held(trades: &[Trade]) -> Option<usize> {
    trades.iter().map(|t| t.bars_held()).min()
}

/// Longest bars-held across trades. `None` on empty input.
pub fn max_bars_held(trades: &[Trade]) -> Option<usize> {
    trades.iter().map(|t| t.bars_held()).max()
}

/// Fraction of bars during which the wallet held a non-zero position, derived
/// from the fill blotter alone: a fill at bar `B` applies at that bar's open,
/// so the position it produces is what's held from `B` onward until the next
/// fill (or the end of the run). `0.0` when `total_bars` is zero.
pub fn exposure_ratio<Sym>(fills: &[Fill<Sym>], total_bars: usize) -> Real {
    if total_bars == 0 {
        return 0.0;
    }
    let mut position: Real = 0.0;
    let mut prev_bar = 0;
    let mut exposed = 0usize;
    for f in fills {
        if position.abs() > EPSILON {
            exposed += f.bar.saturating_sub(prev_bar);
        }
        position += f.order.signed_units();
        prev_bar = f.bar;
    }
    if position.abs() > EPSILON {
        exposed += total_bars.saturating_sub(prev_bar);
    }
    exposed as Real / total_bars as Real
}

// ---------------------------------------------------------------------------
// Private helpers
// ---------------------------------------------------------------------------

/// Below this magnitude, a residual position after a reducing fill is treated
/// as fully flat — the same 1e-8 threshold the wallet uses for zero-delta
/// orders.
const EPSILON: Real = 1e-8;

/// Sample mean and sample (Bessel-corrected, `ddof=1`) stddev of `xs`.
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

/// Downside stddev with `threshold` as the Minimum Acceptable Return: `sqrt(mean(min(0, r − threshold)^2))`.
/// `n` divisor (not `n − 1`) to match empyrical's `downside_risk`.
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

/// Sorted-ascending copy, `NaN`-tolerant.
fn sorted_asc(xs: &[Real]) -> Vec<Real> {
    let mut v = xs.to_vec();
    v.sort_by(|a, b| a.partial_cmp(b).unwrap_or(std::cmp::Ordering::Equal));
    v
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

/// Mean of the bottom-`p` fraction of a sorted-ascending slice.
fn tail_mean(sorted: &[Real], p: Real) -> Real {
    if sorted.is_empty() {
        return 0.0;
    }
    let cutoff = ((sorted.len() as Real * p).ceil() as usize).max(1);
    sorted[..cutoff].iter().sum::<Real>() / cutoff as Real
}

/// CAGR helper: `(final / initial)^(bars_per_year / bars) − 1`.
fn cagr_fraction(
    initial: Real,
    final_equity: Real,
    bars: usize,
    bars_per_year: Real,
) -> Option<Real> {
    if initial <= 0.0 || final_equity <= 0.0 || bars == 0 || bars_per_year <= 0.0 {
        return None;
    }
    let years = bars as Real / bars_per_year;
    if years <= 0.0 {
        return None;
    }
    Some((final_equity / initial).powf(1.0 / years) - 1.0)
}

/// `Some(numerator / denominator)`, or `None` when the denominator is not
/// strictly positive (so ratios don't leak `NaN`/`Infinity`).
fn safe_div(num: Real, denom: Real) -> Option<Real> {
    if denom > 0.0 && denom.is_finite() {
        Some(num / denom)
    } else {
        None
    }
}

/// Longest run of trades satisfying `predicate`. Zero on empty input.
fn longest_streak(trades: &[Trade], predicate: impl Fn(&Trade) -> bool) -> usize {
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{Order, OrderId, OrderKind};

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

    fn tagged_fills(orders: Vec<Order<String>>) -> Vec<Fill<String>> {
        orders
            .into_iter()
            .enumerate()
            .map(|(bar, order)| Fill { bar, order })
            .collect()
    }

    fn indexed_fills(pairs: Vec<(usize, Order<String>)>) -> Vec<Fill<String>> {
        pairs
            .into_iter()
            .map(|(bar, order)| Fill { bar, order })
            .collect()
    }

    #[test]
    fn round_trip_long_realizes_pnl() {
        let fills = tagged_fills(vec![
            order(Side::Buy, 1.0, 100.0),
            order(Side::Sell, 1.0, 110.0),
        ]);
        let trades = reconstruct_trades(&fills);
        assert_eq!(trades.len(), 1);
        assert!((trades[0].pnl - 10.0).abs() < 1e-9);
        assert_eq!(trades[0].bars_held(), 1);
        assert!(matches!(trades[0].side, Side::Buy));
        assert!((trades[0].return_ratio - 0.1).abs() < 1e-9);
        assert_eq!(trades[0].units, 1.0);
        assert!((trades[0].entry_price - 100.0).abs() < 1e-9);
        assert!((trades[0].exit_price - 110.0).abs() < 1e-9);
    }

    #[test]
    fn reversal_closes_short_and_opens_long() {
        let fills = tagged_fills(vec![
            order(Side::Sell, 1.0, 100.0),
            order(Side::Buy, 2.0, 90.0),
            order(Side::Sell, 1.0, 95.0),
        ]);
        let trades = reconstruct_trades(&fills);
        assert_eq!(trades.len(), 2);
        assert!((trades[0].pnl - 10.0).abs() < 1e-9);
        assert!(matches!(trades[0].side, Side::Sell));
        assert!((trades[1].pnl - 5.0).abs() < 1e-9);
        assert!(matches!(trades[1].side, Side::Buy));
    }

    #[test]
    fn drawdown_segments_cover_multiple_stretches() {
        // 100 → 110 (peak) → 90 (trough, dd=20/110) → 120 (recovery, closes seg 1)
        //     → 100 (in dd, depth 20/120) → run ends (open seg 2).
        let segs = drawdown_segments(&[100.0, 110.0, 105.0, 90.0, 95.0, 120.0, 100.0]);
        assert_eq!(segs.len(), 2);
        assert!((segs[0].depth_ratio - (110.0 - 90.0) / 110.0).abs() < 1e-9);
        assert_eq!(segs[0].duration_bars, 2); // peak idx 1 → trough idx 3
        assert_eq!(segs[0].underwater_bars, 3); // bars 2, 3, 4
        assert!((segs[1].depth_ratio - (120.0 - 100.0) / 120.0).abs() < 1e-9);
        assert_eq!(segs[1].underwater_bars, 1); // bar 6

        assert!((max_drawdown(&segs) - (110.0 - 90.0) / 110.0).abs() < 1e-9);
        assert_eq!(max_drawdown_duration(&segs), 2);
        let avg = average_drawdown(&segs).unwrap();
        let expected = ((110.0 - 90.0) / 110.0 + (120.0 - 100.0) / 120.0) / 2.0;
        assert!((avg - expected).abs() < 1e-9);
        assert!((time_in_drawdown_ratio(&segs, 7) - 4.0 / 7.0).abs() < 1e-9);
    }

    #[test]
    fn drawdown_segments_flat_curve_is_empty() {
        let segs = drawdown_segments(&[100.0, 100.0, 100.0]);
        assert!(segs.is_empty());
        assert_eq!(max_drawdown(&segs), 0.0);
        assert!(average_drawdown(&segs).is_none());
    }

    #[test]
    fn degenerate_ratios_read_none() {
        // A flat zero-return series has zero variance → Sharpe/Sortino divide
        // by zero and must surface as `None`, not `NaN`/`Infinity`.
        let flat = vec![0.0; 20];
        assert!(sharpe(&flat, 0.0, 252.0).is_none());
        assert!(sortino(&flat, 0.0, 252.0).is_none());
        // No losing trade means profit_factor's denominator is zero.
        let trade = Trade {
            entry_bar: 0,
            exit_bar: 1,
            side: Side::Buy,
            units: 1.0,
            entry_price: 100.0,
            exit_price: 110.0,
            pnl: 10.0,
            return_ratio: 0.1,
        };
        assert!(profit_factor(std::slice::from_ref(&trade)).is_none());
    }

    #[test]
    fn median_matches_convention_on_even_and_odd_samples() {
        let even = [-5.0, -3.0, -1.0, 0.0, 1.0, 2.0, 3.0, 4.0, 5.0, 6.0];
        // Median of an even-length sample = mean of the two middle values.
        assert!((median_return(&even) - 1.5).abs() < 1e-9);
        let odd = [-2.0, -1.0, 3.0];
        assert!((median_return(&odd) - (-1.0)).abs() < 1e-9);
        assert_eq!(median_return(&[]), 0.0);
    }

    #[test]
    fn skew_kurt_normalish_returns() {
        // Symmetric sample → skew ≈ 0.
        let xs = vec![-1.0, 0.0, 1.0];
        assert!(skewness(&xs).unwrap().abs() < 1e-9);
        // Constant sample → m2 = 0 → skew/kurt undefined.
        let flat = vec![1.0; 5];
        assert!(skewness(&flat).is_none());
        assert!(kurtosis(&flat).is_none());
    }

    #[test]
    fn omega_at_zero_threshold() {
        let ret = vec![1.0, -1.0, 2.0, -2.0];
        assert!((omega(&ret, 0.0).unwrap() - 1.0).abs() < 1e-9);
        assert!(omega(&[1.0, 2.0, 3.0], 0.0).is_none());
    }

    #[test]
    fn ulcer_index_zero_on_monotone_curve() {
        assert_eq!(ulcer_index(&[100.0, 110.0, 120.0, 130.0]), 0.0);
        let ui = ulcer_index(&[100.0, 90.0, 100.0]);
        assert!((ui - (0.01_f64 / 3.0).sqrt()).abs() < 1e-9);
    }

    #[test]
    fn exposure_from_fills() {
        let fills = indexed_fills(vec![
            (3, order(Side::Buy, 1.0, 100.0)),
            (7, order(Side::Sell, 1.0, 110.0)),
        ]);
        assert!((exposure_ratio(&fills, 10) - 0.4).abs() < 1e-9);
    }

    #[test]
    fn streaks_track_longest_run() {
        // W W L W L L L W W W → longest wins 3, longest losses 3.
        let trades: Vec<Trade> = [1.0, 2.0, -1.0, 3.0, -1.0, -2.0, -3.0, 4.0, 5.0, 6.0]
            .into_iter()
            .map(|p| Trade {
                entry_bar: 0,
                exit_bar: 1,
                side: Side::Buy,
                units: 1.0,
                entry_price: 100.0,
                exit_price: 100.0 + p,
                pnl: p,
                return_ratio: 0.0,
            })
            .collect();
        assert_eq!(max_consecutive_wins(&trades), 3);
        assert_eq!(max_consecutive_losses(&trades), 3);
    }

    #[test]
    fn value_at_risk_matches_percentile_convention() {
        // With N=10, 95%-VaR = -Q(0.05) = -(-4.1) = 4.1 (loss magnitude).
        let ret = [-5.0, -3.0, -1.0, 0.0, 1.0, 2.0, 3.0, 4.0, 5.0, 6.0];
        assert!((value_at_risk(&ret, 0.95) - 4.1).abs() < 1e-9);
        assert!((conditional_value_at_risk(&ret, 0.95) - 5.0).abs() < 1e-9);
    }

    #[test]
    fn total_return_and_cagr_match_expectations() {
        let equity = [100.0, 105.0, 110.0, 121.0];
        assert!((total_return(&equity, 100.0) - 0.21).abs() < 1e-9);
        // 21% over 4 bars @ 252 bars/year is essentially instant → CAGR is huge.
        assert!(cagr(&equity, 100.0, 252.0).unwrap() > 1.0);
    }

    // Deterministic return series with a modest positive mean and low
    // dispersion — SR should register as clearly positive, and the higher-moment
    // correction should behave.
    fn psr_test_returns() -> Vec<Real> {
        // 200 bars alternating tiny positive with a slightly smaller negative
        // → mean > 0, plenty of samples for T-1 to matter.
        (0..200u32)
            .map(|i| if i.is_multiple_of(2) { 0.010 } else { -0.008 })
            .collect()
    }

    #[test]
    fn psr_returns_probability_in_unit_interval() {
        let ret = psr_test_returns();
        let p = probabilistic_sharpe(&ret, 0.0, 252.0, 0.0).unwrap();
        assert!((0.0..=1.0).contains(&p), "PSR must be a probability, got {p}");
    }

    #[test]
    fn psr_at_observed_sharpe_is_one_half() {
        // Passing benchmark = observed annualized Sharpe should put the test
        // statistic at zero → Φ(0) = 0.5.
        let ret = psr_test_returns();
        let observed = sharpe(&ret, 0.0, 252.0).unwrap();
        let p = probabilistic_sharpe(&ret, 0.0, 252.0, observed).unwrap();
        assert!((p - 0.5).abs() < 1e-9, "expected 0.5, got {p}");
    }

    #[test]
    fn psr_monotone_in_benchmark() {
        // A stricter benchmark can only lower the probability of exceeding it.
        let ret = psr_test_returns();
        let p_at_zero = probabilistic_sharpe(&ret, 0.0, 252.0, 0.0).unwrap();
        let p_at_one = probabilistic_sharpe(&ret, 0.0, 252.0, 1.0).unwrap();
        assert!(p_at_zero > p_at_one);
    }

    #[test]
    fn psr_none_on_short_input() {
        assert!(probabilistic_sharpe(&[], 0.0, 252.0, 0.0).is_none());
        assert!(probabilistic_sharpe(&[0.01], 0.0, 252.0, 0.0).is_none());
    }

    #[test]
    fn psr_none_on_zero_variance() {
        // Exact zeros — mean is 0.0, every centered term is 0.0, stddev is
        // 0.0, so [`sharpe`] bails via `safe_div` and PSR inherits the `None`.
        let flat = vec![0.0; 100];
        assert!(probabilistic_sharpe(&flat, 0.0, 252.0, 0.0).is_none());
    }

    #[test]
    fn dsr_deflates_psr_when_selection_matters() {
        // With n_trials > 1 and positive trial variance, SR₀ > 0, so DSR must
        // read strictly below PSR against 0.
        let ret = psr_test_returns();
        let psr0 = probabilistic_sharpe(&ret, 0.0, 252.0, 0.0).unwrap();
        let dsr = deflated_sharpe(&ret, 0.0, 252.0, 50, 0.25).unwrap();
        assert!(dsr < psr0, "DSR ({dsr}) should be < PSR ({psr0})");
        assert!((0.0..=1.0).contains(&dsr));
    }

    #[test]
    fn dsr_none_on_degenerate_inputs() {
        let ret = psr_test_returns();
        // n_trials < 2: no selection, DSR is undefined.
        assert!(deflated_sharpe(&ret, 0.0, 252.0, 1, 0.25).is_none());
        // Non-positive trial variance: SR₀ is undefined.
        assert!(deflated_sharpe(&ret, 0.0, 252.0, 50, 0.0).is_none());
        assert!(deflated_sharpe(&ret, 0.0, 252.0, 50, -0.1).is_none());
    }

    #[test]
    fn dsr_monotone_in_n_trials() {
        // More trials → higher expected max under the null → harder to beat →
        // strictly lower DSR (for a fixed observed Sharpe and trial variance).
        let ret = psr_test_returns();
        let dsr_small = deflated_sharpe(&ret, 0.0, 252.0, 10, 0.25).unwrap();
        let dsr_large = deflated_sharpe(&ret, 0.0, 252.0, 1000, 0.25).unwrap();
        assert!(dsr_large < dsr_small, "{dsr_large} vs {dsr_small}");
    }
}
