//! Standalone performance metrics — one function per metric — reducing a
//! backtest's [`equity_curve`](crate::RunReport::equity_curve) and
//! [`fills`](crate::RunReport::fills) to the numbers a reader cares about
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
//!
//! # Closed system
//!
//! **Every function here assumes the equity curve is a closed system** — that
//! all of its movement is P&L, and that no value entered or left the account
//! from outside. That holds by construction for a backtest, which is what these
//! functions were written to reduce.
//!
//! It does *not* hold for an account a human can pay into, and the failure is
//! silent: a withdrawal is shaped exactly like a trading loss in an equity
//! curve, and a deposit exactly like a gain. There is nothing in a bare curve
//! to distinguish them, so an unrecorded external flow corrupts
//! [`total_return`], [`cagr`], [`sharpe`], [`sortino`] and [`max_drawdown`]
//! alike, and the corrupting bar stays in the series permanently.
//!
//! Tracking flows is portfolio accounting, and this module deliberately does
//! not do it: the flow series would have to be threaded through
//! [`per_bar_returns`], the one intermediate every other metric consumes.
//! **A caller whose account takes external flows must neutralize them before
//! calling** — the standard treatment is a chain-linked time-weighted return,
//! `r_i = (E_i − F_i) / E_{i−1} − 1` for a flow `F_i` landing in period `i`,
//! which yields a flow-neutral curve the functions here then reduce correctly.
//! (Attributing `F_i` to period start rather than end gives
//! `r_i = E_i / (E_{i−1} + F_i) − 1`; pick one and hold to it.)
//!
//! [`Wallet::adjust_funds`](crate::Wallet::adjust_funds) is the operation that
//! most often introduces such a flow.

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
///
/// **This formula inverts sign below zero** — with `prev < 0`, a further loss
/// comes back as a *positive* return. It is not guarded here, because the guard
/// belongs one layer down: [`run`](crate::backtest::run) pins a ruined curve at
/// `0.0` from [`ruin_bar`](crate::RunReport::ruin_bar) on, so no curve it
/// produces ever goes negative, and a ruined run's series is one `-1.0`
/// followed by zeros. A hand-built curve that does go negative gets the
/// arithmetic it asked for.
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

/// Walk `fills` **per symbol**, each with its own signed position and
/// volume-weighted entry price, producing one [`Trade`] per closed leg.
///
/// Same-side fills add to that symbol's open leg with a volume-weighted new
/// entry. An opposite-side fill in the *same symbol* closes (partially or
/// fully) and — if it crosses zero — re-opens the remainder at the same fill
/// price as a fresh trade. So one reversal (`set(Buy, all-in)` while short)
/// yields one closed short plus one open long, matching how a
/// [`SingleAssetStrategy`](crate::strategies::SingleAssetStrategy) reasons
/// about its position.
///
/// **Legs never cross instruments.** A blotter from a multi-symbol shape
/// (`pairs`, `basket`, `multi`, `portfolio`) interleaves symbols, and every
/// emitted [`Trade`] draws its `entry_price` and `exit_price` from fills of one
/// symbol. Before 0.63.2 this walked the whole blotter with a single position,
/// so an opposite-side fill in a *different* instrument closed the open leg and
/// P&L subtracted one asset's price from another's.
///
/// **Ordering.** Each trade is emitted as its closing fill is read, so the
/// result is in non-decreasing `exit_bar` order — the arrival order the
/// consecutive-win/loss streak metrics read as a time series. Trades closing on
/// the same bar keep blotter order. Positions still open at the end of the
/// blotter are not emitted (a run wanting them counted should flatten first —
/// see [`flatten_open_positions`](crate::backtest::flatten_open_positions)).
///
/// `Sym` needs only [`PartialEq`]: the open legs live in a small
/// insertion-ordered list, keyed by borrowed symbol. Grouping is therefore
/// deterministic by construction — no hash iteration order enters the result.
pub fn reconstruct_trades<Sym: PartialEq>(fills: &[Fill<Sym>]) -> Vec<Trade> {
    struct Open {
        signed_units: Real,
        entry_price: Real,
        entry_bar: usize,
    }

    let mut trades = Vec::new();
    // One open leg per symbol seen so far, in first-appearance order. A linear
    // scan beats hashing here: the common blotter carries one or two symbols,
    // and even a wide basket stays far short of the string hash it would pay
    // on every fill.
    let mut open: Vec<(&Sym, Open)> = Vec::new();

    for f in fills {
        let delta = f.order.signed_units();
        let bar = f.bar;
        let price = f.order.price;
        let sym = &f.order.symbol;

        let Some(slot) = open.iter().position(|(s, _)| *s == sym) else {
            open.push((
                sym,
                Open {
                    signed_units: delta,
                    entry_price: price,
                    entry_bar: bar,
                },
            ));
            continue;
        };
        let pos = &mut open[slot].1;

        if pos.signed_units.signum() == delta.signum() {
            // Adding to the position: volume-weighted new entry.
            let new_units = pos.signed_units + delta;
            let notional = pos.signed_units.abs() * pos.entry_price + delta.abs() * price;
            pos.entry_price = notional / new_units.abs();
            pos.signed_units = new_units;
            continue;
        }

        // Opposite side, same symbol: reducing, closing, or reversing.
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
            // Flat: drop the slot so the next fill in this symbol opens fresh.
            open.swap_remove(slot);
        } else {
            // Reversed: the remainder is a fresh position at this fill.
            *pos = Open {
                signed_units: remaining,
                entry_price: price,
                entry_bar: bar,
            };
        }
    }

    trades
}

/// Build the drawdown segments of `equity_curve` — one entry per peak → trough
/// → recovery-or-end stretch. A monotone-non-decreasing curve produces an
/// empty vector.
///
/// Every emitted `depth_ratio` is at most `1.0`, and a debug build asserts it.
/// A deeper-than-100% drawdown means the curve went below zero, which
/// [`run`](crate::backtest::run) cannot produce — so on a real report its
/// appearance is a bug in the driver, not a property of the strategy. (The
/// assertion is `debug_assert!` rather than a guard because this is a public
/// function and a caller is free to hand it any series it likes.)
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
                let depth = depth_ratio(peak, trough);
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
        let depth = depth_ratio(peak, trough);
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
    median_of_sorted(&sorted_asc(returns))
}

/// [`median_return`] over an already-sorted series. See [`sorted_returns`] for
/// why this split exists.
pub(crate) fn median_of_sorted(sorted: &[Real]) -> Real {
    if sorted.is_empty() {
        return 0.0;
    }
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
    value_at_risk_of_sorted(&sorted_asc(returns), confidence)
}

/// [`value_at_risk`] over an already-sorted series. See [`sorted_returns`].
pub(crate) fn value_at_risk_of_sorted(sorted: &[Real], confidence: Real) -> Real {
    if sorted.is_empty() {
        return 0.0;
    }
    -percentile(sorted, 1.0 - confidence)
}

/// Historical Conditional VaR (Expected Shortfall) of `returns` at
/// `confidence`: mean of the bottom-`(1 - confidence)` return tail, expressed
/// as a positive loss fraction. `0.0` on empty input.
pub fn conditional_value_at_risk(returns: &[Real], confidence: Real) -> Real {
    conditional_value_at_risk_of_sorted(&sorted_asc(returns), confidence)
}

/// [`conditional_value_at_risk`] over an already-sorted series. See
/// [`sorted_returns`].
pub(crate) fn conditional_value_at_risk_of_sorted(sorted: &[Real], confidence: Real) -> Real {
    if sorted.is_empty() {
        return 0.0;
    }
    -tail_mean(sorted, 1.0 - confidence)
}

/// `|P95(returns)| / |P5(returns)|` (with 5th/95th percentiles), a coarse
/// symmetry check on the tails. `None` when the 5th-percentile magnitude is
/// zero.
pub fn tail_ratio(returns: &[Real]) -> Option<Real> {
    tail_ratio_of_sorted(&sorted_asc(returns))
}

/// [`tail_ratio`] over an already-sorted series. See [`sorted_returns`].
pub(crate) fn tail_ratio_of_sorted(sorted: &[Real]) -> Option<Real> {
    if sorted.is_empty() {
        return None;
    }
    let p95 = percentile(sorted, 0.95).abs();
    let p5 = percentile(sorted, 0.05).abs();
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
    calmar_with_max_drawdown(
        equity_curve,
        initial_equity,
        bars_per_year,
        max_drawdown(&drawdown_segments(equity_curve)),
    )
}

/// [`calmar`] against a max drawdown the caller already has. See
/// [`sorted_returns`] for why these split forms exist.
pub(crate) fn calmar_with_max_drawdown(
    equity_curve: &[Real],
    initial_equity: Real,
    bars_per_year: Real,
    max_dd: Real,
) -> Option<Real> {
    let c = cagr(equity_curve, initial_equity, bars_per_year)?;
    safe_div(c, max_dd)
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
    ulcer_performance_index_with_ulcer(
        equity_curve,
        initial_equity,
        risk_free_rate,
        bars_per_year,
        ulcer_index(equity_curve),
    )
}

/// [`ulcer_performance_index`] against an Ulcer Index the caller already has.
///
/// A report emits both `ulcer_index` and `ulcer_performance_index`, and the
/// second recomputing the first is a second full walk of the equity curve —
/// ~250 µs at 200 000 bars. Same split, and the same reason, as
/// [`calmar_with_max_drawdown`] and [`recovery_factor_with_max_drawdown`].
pub(crate) fn ulcer_performance_index_with_ulcer(
    equity_curve: &[Real],
    initial_equity: Real,
    risk_free_rate: Real,
    bars_per_year: Real,
    ulcer: Real,
) -> Option<Real> {
    let c = cagr(equity_curve, initial_equity, bars_per_year)?;
    safe_div(c - risk_free_rate, ulcer)
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
/// pre-aggregated (e.g. the `optimize` grid where every row's `Metrics`
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

/// The **longest** time spent below a prior peak, in bars — the worst recovery
/// wait, independent of how deep the drawdown that caused it was. `0` on empty
/// input.
///
/// Reads each segment's `underwater_bars` (peak → recovery), not its
/// `duration_bars` (peak → trough): a drawdown is not over when it stops
/// falling, it is over when the curve gets back to where it started. The two
/// coincide only for a drawdown that never recovers.
///
/// Deepest-and-longest are different drawdowns in general, and this is the
/// longest. A shallow drift that takes 200 bars to work off is the one that
/// exhausts an allocator's patience; a sharp 30% dip recovered in 5 bars is
/// not, and `drawdown.max` already reports that one's severity.
pub fn max_drawdown_duration(segments: &[DrawdownSegment]) -> usize {
    segments
        .iter()
        .map(|s| s.underwater_bars)
        .max()
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

/// Mean time spent below a prior peak, in bars, across all segments; `None` for
/// empty input.
///
/// Reads `underwater_bars` (peak → recovery), matching
/// [`max_drawdown_duration`]. The two are siblings in `drawdown.*` and must
/// measure the same span, or `avg_duration_bars > max_duration_bars` becomes
/// reachable on ordinary input and neither number means what its name says.
pub fn average_drawdown_duration(segments: &[DrawdownSegment]) -> Option<Real> {
    if segments.is_empty() {
        None
    } else {
        Some(
            segments
                .iter()
                .map(|s| s.underwater_bars as Real)
                .sum::<Real>()
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
    recovery_factor_with_max_drawdown(
        equity_curve,
        initial_equity,
        max_drawdown(&drawdown_segments(equity_curve)),
    )
}

/// [`recovery_factor`] against a max drawdown the caller already has. See
/// [`sorted_returns`].
pub(crate) fn recovery_factor_with_max_drawdown(
    equity_curve: &[Real],
    initial_equity: Real,
    max_dd: Real,
) -> Option<Real> {
    safe_div(total_return(equity_curve, initial_equity), max_dd)
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

/// One drawdown segment's depth: `(peak - trough) / peak`, or `0.0` when the
/// peak is not strictly positive (nothing to be down *from*).
///
/// Bounded by `1.0` in debug builds — see [`drawdown_segments`] for why that is
/// an assertion about the driver rather than a clamp.
fn depth_ratio(peak: Real, trough: Real) -> Real {
    if peak <= 0.0 {
        return 0.0;
    }
    let depth = (peak - trough) / peak;
    debug_assert!(
        depth <= 1.0,
        "drawdown deeper than 100% ({depth}): equity went below zero, which \
         `backtest::run` cannot produce — peak {peak}, trough {trough}"
    );
    depth
}

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
///
/// `pub(crate)` so a caller that is about to ask *several* quantile questions of
/// one series can sort it once and pass the result to the `*_of_sorted` backs.
///
/// Four public metrics here are quantile reads — [`median_return`],
/// [`value_at_risk`], [`conditional_value_at_risk`], [`tail_ratio`] — and each
/// independently sorts its input. Taken one at a time that is the right API;
/// taken together, as
/// [`spec::metrics::from_report`](crate::spec::metrics::from_report) takes them,
/// it was four sorts and four full copies of the same series. Measured on a
/// 200 000-bar run that was ~16.8 ms of a 22.8 ms reduction — which `optimize`
/// pays once per grid row per fold.
///
/// So each of the four splits into a public front (sorts, then delegates) and a
/// `*_of_sorted` back. The same shape as the existing public
/// [`probabilistic_sharpe_from_stats`] / [`deflated_sharpe_from_stats`] pairs,
/// but kept crate-private: these are an internal reuse mechanism, not new
/// user-facing metrics, and every `pub fn` in this module must be mirrored on
/// the Python `fugazi.metrics` module (enforced by
/// `tests/hand_maintained_mirrors.rs`).
///
/// **The reducer no longer takes this path at all** — [`quantile_reads`] answers
/// all four without a total order, for a further 3.76 ms → 0.70 ms at 200 000
/// bars. The split survives because it is still what the four public fronts are
/// built from, and because it is the reference `quantile_reads` is pinned
/// against.
pub(crate) fn sorted_asc(xs: &[Real]) -> Vec<Real> {
    let mut v = xs.to_vec();
    v.sort_by(crate::indicators::stats::cmp_asc);
    v
}

/// Linearly-interpolated `p`-quantile of a sorted-ascending slice (R's type-7,
/// `numpy`'s default). `p` in `[0, 1]`.
///
/// Delegates to the shared core so the report-level percentiles here and the
/// rolling [`Percentile`](crate::indicators::Percentile) indicator cannot drift
/// apart.
fn percentile(sorted: &[Real], p: Real) -> Real {
    crate::indicators::stats::quantile_of_sorted(sorted, p)
}

/// Mean of the bottom-`p` tail of a sorted-ascending slice: the elements up to
/// and including the `p`-quantile's lower order statistic, `floor(p·(n−1)) + 1`
/// of them.
///
/// **Indexed off `p·(n − 1)`, the same base [`percentile`] uses**, not off
/// `ceil(n·p)`. Two reasons, and the first is a bug this had:
///
/// * `ceil(n·p)` sits on a knife edge whenever `n·p` lands near an integer, and
///   at the 95% confidence every report asks for, it does. `value_at_risk` and
///   `conditional_value_at_risk` take a *confidence* and derive the tail as
///   `1.0 - confidence`, which for `0.95` is `0.050000000000000044` rather than
///   the `0.05` [`tail_ratio`] writes as a literal — the two are 4.4e-17 apart,
///   and `1.0 - 0.95` is not even a rounding error in the subtraction (by
///   Sterbenz's lemma it is exact; the error is already in `fl(0.95)`). Under
///   `ceil(n·p)` that gap flipped the tail between 500 and 501 elements at
///   10 000 samples — a ~2e-5 difference in a metric, off 4e-17 of input.
///   Indexed off `floor(p·(n−1))`, both spellings land on 499 and the tail is
///   500 either way.
/// * It is the formula the reference implements. `empyrical`'s
///   `conditional_value_at_risk` is `int((n - 1) * cutoff)` then the mean of the
///   first `cutoff_index + 1`, which this now matches exactly rather than
///   approximately — see `tools/gen_metrics_fixtures.py`.
///
/// The knife edge is why the committed fixture never caught this: it is 252
/// returns, and at `n = 252` both formulas and both spellings agree on 13.
fn tail_mean(sorted: &[Real], p: Real) -> Real {
    if sorted.is_empty() {
        return 0.0;
    }
    let cutoff = tail_cutoff(sorted.len(), p);
    sorted[..cutoff].iter().sum::<Real>() / cutoff as Real
}

/// How many of the smallest samples the bottom-`p` tail covers: the lower order
/// statistic of the `p`-quantile, plus one for inclusivity. At least 1, at most
/// `n`. See [`tail_mean`] for why it is indexed this way.
pub(crate) fn tail_cutoff(n: usize, p: Real) -> usize {
    if n == 0 {
        return 0;
    }
    // A negative `p` saturates to 0 in the cast, so the `+ 1` floors this at 1.
    ((p * (n - 1) as Real).floor() as usize + 1).min(n)
}

/// CAGR helper: `(final / initial)^(bars_per_year / bars) − 1`.
///
/// A final equity of exactly zero is **ruin**, not an undefined ratio: the
/// formula evaluates to `-1` there (`0^x = 0` for any positive `x`), so it is
/// reported as `-100%` rather than as an absent value. That distinction is the
/// point — a blank CAGR cell used to mean both "the account was wiped out" and
/// "the window was too short to annualize", and a search ranking by CAGR read
/// the first as the second. Only a *negative* final equity is undefined, and
/// [`run`](crate::backtest::run) no longer produces one.
fn cagr_fraction(
    initial: Real,
    final_equity: Real,
    bars: usize,
    bars_per_year: Real,
) -> Option<Real> {
    if initial <= 0.0 || final_equity < 0.0 || bars == 0 || bars_per_year <= 0.0 {
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

// ---------------------------------------------------------------------------
// Shared reduction cores
// ---------------------------------------------------------------------------
//
// Everything below is `pub(crate)`, and for the same reason `sorted_asc` is:
// these are an internal reuse mechanism for a caller that wants *all* of the
// numbers at once, not new user-facing metrics — and every `pub fn` in this
// module owes a mirror on Python's `fugazi.metrics` (enforced by
// `tests/hand_maintained_mirrors.rs`).
//
// The public per-metric functions above stay the definition of each metric.
// The cores here re-derive them from shared accumulators, so the two *could*
// drift; `tests::reduction_cores_match_public_metrics` walks a spread of
// series (including the degenerate ones) and pins every derivation to its
// public twin, bit for bit.
//
// Delegating the other way — having `mean_return` and friends read off a core —
// would remove the duplication, and is the wrong trade: a standalone
// `mean_return` would then pay for a two-pass gather of moments, a downside
// deviation and two Omega integrals it does not want, and `fugazi.metrics`
// exposes every one of those functions to Python as a single call.
//
// Gated on `spec` because `spec::metrics::from_report` is the only caller, and
// the feature matrix builds `--no-default-features --lib` (where `spec` is
// off) with `-D warnings` — so an ungated core is a dead-code error in five of
// the matrix jobs. If a second caller appears outside `spec`, widen the gate
// rather than reaching for `#[allow(dead_code)]`.

#[cfg(feature = "spec")]
/// The seed `impl Sum for f64` folds from.
///
/// `-0.0` is the additive identity f64 actually has — `x + -0.0 == x` for every
/// `x`, including `x = -0.0`, whereas `-0.0 + 0.0` is `+0.0`. That matters here
/// only because `sum()` over an **empty** iterator returns the seed verbatim,
/// and three of the trade aggregates below sum a filtered subset that is empty
/// whenever a run has no winner (or no loser): `profit_factor` on a run where
/// every trade lost is `Some(-0.0)`, and an accumulator seeded `0.0` would
/// answer `Some(0.0)`. Same number, different bits, and `metrics.yml`
/// serializes the sign.
const SUM_SEED: Real = -0.0;

#[cfg(feature = "spec")]
/// Every accumulator the return series is asked for, gathered in two passes.
///
/// [`from_report`](crate::spec::metrics::from_report) needs fourteen numbers
/// off one return series, and taken one public function at a time that was
/// ~30 walks of it: `sharpe` derives the mean and then the mean and stddev
/// again inside `annualized_volatility`, `sortino` derives the mean twice more,
/// `skewness` and `kurtosis` each re-derive the mean *and* `Σ(x − mean)²`
/// before their own moment, and `probabilistic_sharpe` then calls all three of
/// `sharpe` / `skewness` / `kurtosis` a second time from scratch. Measured on a
/// 200 000-bar run that was 4.1 ms of a 9.6 ms reduction — 43% of it, and the
/// largest single cost. Gathering once takes the same fourteen numbers to
/// 0.57 ms.
///
/// Two passes rather than one, because the centred moments need the mean and
/// this crate does not take the `E[X²] − E[X]²` shortcut — it cancels away the
/// leading digits, and it was wrong at crypto price scale (see
/// [`WindowStats`](crate::indicators::stats)).
///
/// `threshold` is the Minimum Acceptable Return shared by the downside
/// deviation ([`sortino`]) and the Omega integrals ([`omega`]); pass the
/// per-bar risk-free rate.
#[derive(Debug, Clone, Copy)]
pub(crate) struct ReturnStats {
    n: usize,
    mean: Real,
    best: Real,
    worst: Real,
    positive: usize,
    /// `Σ (x − mean)²`
    sum_sq: Real,
    /// `Σ (x − mean)³`
    sum_cu: Real,
    /// `Σ (x − mean)⁴`
    sum_qu: Real,
    /// `Σ min(0, x − threshold)²`
    downside_sq: Real,
    /// `Σ max(x − threshold, 0)`
    omega_gains: Real,
    /// `Σ max(threshold − x, 0)`
    omega_losses: Real,
}

#[cfg(feature = "spec")]
impl ReturnStats {
#[inline]
    pub(crate) fn of(returns: &[Real], threshold: Real) -> Self {
        let n = returns.len();
        if n == 0 {
            return Self {
                n: 0,
                mean: 0.0,
                best: 0.0,
                worst: 0.0,
                positive: 0,
                sum_sq: 0.0,
                sum_cu: 0.0,
                sum_qu: 0.0,
                downside_sq: 0.0,
                omega_gains: 0.0,
                omega_losses: 0.0,
            };
        }

        // Pass 1 — the mean, the extrema, the sign count. `SUM_SEED`, not
        // `0.0`: this stands in for `returns.iter().sum::<Real>()`.
        let mut sum = SUM_SEED;
        let mut best = returns[0];
        let mut worst = returns[0];
        let mut positive = 0usize;
        for &r in returns {
            sum += r;
            best = best.max(r);
            worst = worst.min(r);
            if r > 0.0 {
                positive += 1;
            }
        }
        let mean = sum / n as Real;

        // Pass 2 — the centred moments, the downside deviation, the Omega
        // integrals. `d2 * d` and `d2 * d2` are the same multiplication trees
        // `powi(3)` / `powi(4)` expand to, so each accumulator sees the same
        // addends in the same order as the public functions' own folds.
        let mut sum_sq = SUM_SEED;
        let mut sum_cu = SUM_SEED;
        let mut sum_qu = SUM_SEED;
        let mut downside_sq = SUM_SEED;
        // `omega` is a hand-written loop seeded `0.0`, not a `sum()` — so these
        // two are seeded `0.0`. The seed is copied from the public function,
        // not chosen.
        let mut omega_gains = 0.0;
        let mut omega_losses = 0.0;
        for &r in returns {
            let d = r - mean;
            let d2 = d * d;
            sum_sq += d2;
            sum_cu += d2 * d;
            sum_qu += d2 * d2;

            let diff = r - threshold;
            let below = diff.min(0.0);
            downside_sq += below * below;
            if diff >= 0.0 {
                omega_gains += diff;
            } else {
                omega_losses += -diff;
            }
        }

        Self {
            n,
            mean,
            best,
            worst,
            positive,
            sum_sq,
            sum_cu,
            sum_qu,
            downside_sq,
            omega_gains,
            omega_losses,
        }
    }

    /// The observation count — [`probabilistic_sharpe_from_stats`]'s `n_returns`.
    pub(crate) fn len(&self) -> usize {
        self.n
    }

    /// [`mean_return`]
    pub(crate) fn mean(&self) -> Real {
        self.mean
    }

    /// [`best_return`]
    pub(crate) fn best(&self) -> Real {
        self.best
    }

    /// [`worst_return`]
    pub(crate) fn worst(&self) -> Real {
        self.worst
    }

    /// [`positive_bars_ratio`]
    pub(crate) fn positive_ratio(&self) -> Real {
        if self.n == 0 {
            0.0
        } else {
            self.positive as Real / self.n as Real
        }
    }

    /// [`stddev_return`] — sample (`ddof=1`).
    pub(crate) fn stddev(&self) -> Real {
        if self.n < 2 {
            0.0
        } else {
            (self.sum_sq / (self.n - 1) as Real).sqrt()
        }
    }

    /// The biased central second moment, the [`skewness`] / [`kurtosis`]
    /// denominator. `None` when it vanishes, matching both.
    fn m2(&self) -> Option<Real> {
        if self.n == 0 {
            return None;
        }
        let m2 = self.sum_sq / self.n as Real;
        (m2 != 0.0).then_some(m2)
    }

    /// [`skewness`]
    pub(crate) fn skewness(&self) -> Option<Real> {
        let m2 = self.m2()?;
        Some((self.sum_cu / self.n as Real) / m2.powf(1.5))
    }

    /// [`kurtosis`]
    pub(crate) fn kurtosis(&self) -> Option<Real> {
        let m2 = self.m2()?;
        Some((self.sum_qu / self.n as Real) / m2.powi(2) - 3.0)
    }

    /// [`annualized_return`]
    pub(crate) fn annualized_mean(&self, bars_per_year: Real) -> Real {
        self.mean * bars_per_year
    }

    /// [`annualized_volatility`]
    pub(crate) fn annualized_volatility(&self, bars_per_year: Real) -> Real {
        self.stddev() * bars_per_year.max(0.0).sqrt()
    }

    /// [`sharpe`]
    pub(crate) fn sharpe(&self, risk_free_rate: Real, bars_per_year: Real) -> Option<Real> {
        safe_div(
            self.annualized_mean(bars_per_year) - risk_free_rate,
            self.annualized_volatility(bars_per_year),
        )
    }

    /// [`sortino`] — valid only when `self` was gathered with the per-bar
    /// risk-free rate as its `threshold`, which is that metric's MAR.
    pub(crate) fn sortino(&self, risk_free_rate: Real, bars_per_year: Real) -> Option<Real> {
        let downside = if self.n == 0 {
            0.0
        } else {
            (self.downside_sq / self.n as Real).sqrt()
        };
        safe_div(
            self.annualized_mean(bars_per_year) - risk_free_rate,
            downside * bars_per_year.max(0.0).sqrt(),
        )
    }

    /// [`omega`] at the `threshold` `self` was gathered with.
    pub(crate) fn omega(&self) -> Option<Real> {
        safe_div(self.omega_gains, self.omega_losses)
    }
}

#[cfg(feature = "spec")]
/// The four quantile answers a report needs off one return distribution.
pub(crate) struct QuantileReads {
    /// [`median_return`]
    pub median: Real,
    /// [`value_at_risk`]
    pub var: Real,
    /// [`conditional_value_at_risk`]
    pub cvar: Real,
    /// [`tail_ratio`]
    pub tail_ratio: Option<Real>,
}

#[cfg(feature = "spec")]
/// All four quantile reads without sorting the series.
///
/// Between them the four need six order statistics and the mean of one tail —
/// not a total order. [`sorted_asc`] gave the reducer one sort instead of four,
/// which was the right first move; this replaces the remaining sort with
/// introselect. On a 200 000-bar run the four reads go from 3.76 ms (39% of the
/// whole reduction) to 0.70 ms.
///
/// Bit-identical to the sorted path, deliberately, down to two details:
///
/// * [`value_at_risk`] and [`conditional_value_at_risk`] take a *confidence*
///   and derive the tail as `1.0 - confidence`, while [`tail_ratio`] writes
///   `0.05` as a literal, and `1.0 - 0.95` is `0.050000000000000044`. Both
///   spellings are carried because they are 4.4e-17 apart and that is not
///   recoverable — the error is in `fl(0.95)`, so no amount of care in the
///   subtraction gets `0.05` back.
///
///   What it costs is now bounded: both floor to the *same* order statistic
///   (499 at 10 000 samples), so `var_95` and `tail_ratio`'s 5th percentile
///   differ only in the interpolation weight, by about one ULP. It used to cost
///   more — [`tail_mean`] indexed the CVaR tail off `ceil(n·p)`, which is a
///   knife edge there, and the two spellings disagreed by a whole element.
/// * the CVaR tail is sorted before it is averaged, so its mean sums ascending
///   exactly as [`tail_mean`] over a fully-sorted copy would. Summing it in
///   partition order is ~15% faster again and lands ~1 ULP away; not worth it.
///
/// `NaN` tolerance is the same weak promise [`sorted_asc`] makes — no panic,
/// via [`cmp_asc`](crate::indicators::stats::cmp_asc) — but *not* the same
/// arbitrary placement: with a `NaN` in the series the comparator is not a
/// total order, and where introselect strands it need not be where a sort
/// would. A `NaN` here means the equity curve already carried one.
///
/// **`returns` is left permuted.** Introselect works in place, and the caller
/// that wants all four reads is reducing a report, where the return series is
/// dead the moment its moments and its quantiles have been taken — so it hands
/// over its own buffer rather than paying 1.6 MB of copy (at 200 000 bars) to
/// protect an order nothing will look at again. Gather any moments *first*.
#[inline]
pub(crate) fn quantile_reads(returns: &mut [Real], confidence: Real) -> QuantileReads {
    let n = returns.len();
    if n == 0 {
        return QuantileReads {
            median: 0.0,
            var: 0.0,
            cvar: 0.0,
            tail_ratio: None,
        };
    }

    let p_tail = 1.0 - confidence;
    let straddle = |p: Real| -> (usize, usize) {
        let idx = p * (n - 1) as Real;
        let lo = idx.floor() as usize;
        (lo, (lo + 1).min(n - 1))
    };
    let (lo_var, hi_var) = straddle(p_tail);
    let (lo05, hi05) = straddle(0.05);
    let (lo95, hi95) = straddle(0.95);
    let cutoff = tail_cutoff(n, p_tail);

    let mut ks = vec![lo_var, hi_var, lo05, hi05, lo95, hi95, cutoff - 1];
    if n.is_multiple_of(2) {
        ks.push(n / 2 - 1);
        ks.push(n / 2);
    } else {
        ks.push(n / 2);
    }
    ks.sort_unstable();
    ks.dedup();

    let mut found: Vec<(usize, Real)> = Vec::with_capacity(ks.len());
    select_each(returns, &ks, 0, &mut found);
    let at = |k: usize| -> Real {
        found
            .iter()
            .find(|(i, _)| *i == k)
            .expect("every requested index was selected")
            .1
    };

    // R type-7, on the two order statistics the quantile straddles — the same
    // arithmetic `quantile_of_sorted` does, off selected elements instead of a
    // sorted slice.
    let quantile = |p: Real, lo: usize, hi: usize| -> Real {
        if n == 1 {
            return at(0);
        }
        let frac = p * (n - 1) as Real - lo as Real;
        at(lo) * (1.0 - frac) + at(hi) * frac
    };

    let median = if n.is_multiple_of(2) {
        (at(n / 2 - 1) + at(n / 2)) / 2.0
    } else {
        at(n / 2)
    };

    // Selecting index `cutoff - 1` left `returns[..cutoff]` holding exactly the
    // `cutoff` smallest elements, as a multiset.
    let tail = &mut returns[..cutoff];
    tail.sort_unstable_by(crate::indicators::stats::cmp_asc);

    QuantileReads {
        median,
        var: -quantile(p_tail, lo_var, hi_var),
        cvar: -(tail.iter().sum::<Real>() / cutoff as Real),
        tail_ratio: safe_div(
            quantile(0.95, lo95, hi95).abs(),
            quantile(0.05, lo05, hi05).abs(),
        ),
    }
}

#[cfg(feature = "spec")]
/// Place every index in `ks` (ascending, deduped, absolute) at its final
/// position in `v`, recording the order statistic found there.
///
/// `base` is the absolute index of `v[0]`. Selecting the middle wanted index
/// first and recursing into the two partitions costs `O(len)` per level over
/// halving lengths, so the six-to-nine indices a report asks for come to ~2n
/// comparisons against the ~n·log₂n a sort pays.
///
/// The partition invariant survives the recursion: every later select
/// rearranges a contiguous sub-slice whose element *set* is already fixed. So
/// on return `v[k]` is the k-th order statistic for each `k` in `ks`, and
/// `v[..k]` holds exactly the k smallest elements.
fn select_each(v: &mut [Real], ks: &[usize], base: usize, out: &mut Vec<(usize, Real)>) {
    if ks.is_empty() {
        return;
    }
    let mid = ks.len() / 2;
    let k = ks[mid];
    let (lo, at, hi) = v.select_nth_unstable_by(k - base, crate::indicators::stats::cmp_asc);
    out.push((k, *at));
    select_each(lo, &ks[..mid], base, out);
    select_each(hi, &ks[mid + 1..], k + 1, out);
}

#[cfg(feature = "spec")]
/// Every aggregate the reconstructed trades are asked for, in one walk.
///
/// The same argument as [`ReturnStats`], at a different scale: taken one public
/// function at a time the trade section is ~20 walks of the vector, two of them
/// (`average_win`, `average_loss`) collecting the filtered PnLs into a fresh
/// `Vec` before averaging. That is 23 µs against 5 µs on a 100-trade run —
/// invisible next to the return series there, and the dominant trade-side cost
/// on a high-turnover one, where the vector is the long input.
pub(crate) struct TradeStats {
    pub total: usize,
    pub wins: usize,
    pub losses: usize,
    pub flat: usize,
    pub longs: usize,
    pub shorts: usize,
    pub max_consec_wins: usize,
    pub max_consec_losses: usize,
    pub largest_win: Option<Real>,
    pub largest_loss: Option<Real>,
    pub min_bars: Option<usize>,
    pub max_bars: Option<usize>,
    sum_pnl: Real,
    sum_win_pnl: Real,
    sum_loss_pnl: Real,
    sum_return_ratio: Real,
    sum_bars: Real,
}

#[cfg(feature = "spec")]
impl TradeStats {
#[inline]
    pub(crate) fn of(trades: &[Trade]) -> Self {
        let mut s = Self {
            total: trades.len(),
            wins: 0,
            losses: 0,
            flat: 0,
            longs: 0,
            shorts: 0,
            max_consec_wins: 0,
            max_consec_losses: 0,
            largest_win: None,
            largest_loss: None,
            min_bars: None,
            max_bars: None,
            // All five stand in for an `Iterator::sum::<Real>()`. See `SUM_SEED`.
            sum_pnl: SUM_SEED,
            sum_win_pnl: SUM_SEED,
            sum_loss_pnl: SUM_SEED,
            sum_return_ratio: SUM_SEED,
            sum_bars: SUM_SEED,
        };
        let mut win_run = 0usize;
        let mut loss_run = 0usize;

        for t in trades {
            s.sum_pnl += t.pnl;
            s.sum_return_ratio += t.return_ratio;
            let bars = t.bars_held();
            s.sum_bars += bars as Real;
            s.min_bars = Some(s.min_bars.map_or(bars, |m| m.min(bars)));
            s.max_bars = Some(s.max_bars.map_or(bars, |m| m.max(bars)));

            match t.side {
                Side::Buy => s.longs += 1,
                Side::Sell => s.shorts += 1,
            }

            if t.pnl > 0.0 {
                s.wins += 1;
                s.sum_win_pnl += t.pnl;
                s.largest_win = Some(s.largest_win.map_or(t.pnl, |m: Real| m.max(t.pnl)));
                win_run += 1;
                s.max_consec_wins = s.max_consec_wins.max(win_run);
            } else {
                win_run = 0;
            }

            if t.pnl < 0.0 {
                s.losses += 1;
                s.sum_loss_pnl += t.pnl;
                s.largest_loss = Some(s.largest_loss.map_or(t.pnl, |m: Real| m.min(t.pnl)));
                loss_run += 1;
                s.max_consec_losses = s.max_consec_losses.max(loss_run);
            } else {
                loss_run = 0;
            }

            if t.pnl == 0.0 {
                s.flat += 1;
            }
        }
        s
    }

    /// [`win_rate`]
    pub(crate) fn win_rate(&self) -> Option<Real> {
        (self.total > 0).then(|| self.wins as Real / self.total as Real)
    }

    /// [`profit_factor`]
    pub(crate) fn profit_factor(&self) -> Option<Real> {
        safe_div(self.sum_win_pnl, -self.sum_loss_pnl)
    }

    /// [`average_win`]
    pub(crate) fn average_win(&self) -> Option<Real> {
        (self.wins > 0).then(|| self.sum_win_pnl / self.wins as Real)
    }

    /// [`average_loss`]
    pub(crate) fn average_loss(&self) -> Option<Real> {
        (self.losses > 0).then(|| self.sum_loss_pnl / self.losses as Real)
    }

    /// [`payoff_ratio`]
    pub(crate) fn payoff_ratio(&self) -> Option<Real> {
        match (self.average_win(), self.average_loss()) {
            (Some(w), Some(l)) if l < 0.0 => Some(w / -l),
            _ => None,
        }
    }

    /// [`expectancy`]
    pub(crate) fn expectancy(&self) -> Option<Real> {
        (self.total > 0).then(|| self.sum_pnl / self.total as Real)
    }

    /// [`kelly_fraction`]
    pub(crate) fn kelly_fraction(&self) -> Option<Real> {
        match (self.win_rate(), self.payoff_ratio()) {
            (Some(p), Some(b)) if b > 0.0 => Some(p - (1.0 - p) / b),
            _ => None,
        }
    }

    /// [`average_trade_return`]
    pub(crate) fn average_return(&self) -> Option<Real> {
        (self.total > 0).then(|| self.sum_return_ratio / self.total as Real)
    }

    /// [`average_bars_held`]
    pub(crate) fn average_bars(&self) -> Option<Real> {
        (self.total > 0).then(|| self.sum_bars / self.total as Real)
    }
}

#[cfg(test)]
mod tests {
    use crate::types::Symbol;
    use super::*;
    use crate::{Order, OrderId, OrderKind};

    fn order(side: Side, units: Real, price: Real) -> Order<Symbol> {
        Order::new(
            crate::types::symbol("BTC"),
            side,
            units,
            price,
            OrderKind::Market,
            OrderId(0),
        )
    }

    fn tagged_fills(orders: Vec<Order<Symbol>>) -> Vec<Fill<Symbol>> {
        orders
            .into_iter()
            .enumerate()
            .map(|(bar, order)| Fill { bar, order })
            .collect()
    }

    /// The 5% tail must not depend on whether its probability was spelled
    /// `1.0 - 0.95` or `0.05`, and it must be the tail the reference takes.
    ///
    /// Both matter and neither was covered. `value_at_risk` /
    /// `conditional_value_at_risk` derive the tail from a *confidence*, so they
    /// pass `1.0 - 0.95` = `0.050000000000000044`; `tail_ratio` writes `0.05`.
    /// The old `ceil(n·p)` cutoff sat on a knife edge at exactly the sizes that
    /// matter and split those two spellings by a whole element — while the
    /// committed fixture, at 252 returns, is a size where every candidate
    /// formula agrees on 13, so nothing went red.
    ///
    /// The sizes below are chosen the other way: each is one where `ceil(n·p)`
    /// *did* differ between the two spellings.
    #[test]
    fn the_tail_cutoff_is_spelling_independent_and_matches_empyrical() {
        for n in [1_000usize, 2_000, 10_000, 20_000, 100_000, 200_000] {
            let from_confidence = tail_cutoff(n, 1.0 - 0.95);
            let from_literal = tail_cutoff(n, 0.05);
            assert_eq!(
                from_confidence, from_literal,
                "n={n}: tail size depends on how 5% was spelled"
            );

            // `empyrical.conditional_value_at_risk`: `int((n - 1) * cutoff)`,
            // then the mean of the first `cutoff_index + 1` samples.
            let empyrical = (((n - 1) as Real * 0.05) as usize) + 1;
            assert_eq!(
                from_literal, empyrical,
                "n={n}: diverges from the reference implementation"
            );

            // The old formula, kept here as the thing that must stay fixed.
            let old_ceil = |p: Real| ((n as Real * p).ceil() as usize).max(1);
            assert_ne!(
                old_ceil(1.0 - 0.95),
                old_ceil(0.05),
                "n={n} no longer exercises the bug; pick a size that does"
            );
        }
    }

    /// The whole point of the cutoff change: a report's VaR and CVaR now read
    /// the same 5% tail `tail_ratio` does, at a size where they used not to.
    #[test]
    fn var_and_cvar_agree_with_tail_ratio_on_which_tail_is_the_tail() {
        let mut s: u64 = 0xfeed_face_dead_beef;
        let returns: Vec<Real> = (0..10_000)
            .map(|_| {
                s = s.wrapping_mul(6364136223846793005).wrapping_add(1);
                ((s >> 33) as Real / u32::MAX as Real - 0.5) * 0.04
            })
            .collect();
        let sorted = sorted_asc(&returns);

        // VaR's 5th percentile and `tail_ratio`'s share an order statistic, so
        // they agree to within the interpolation weight — one ULP of each
        // other, not a whole sample apart.
        let var_p5 = -value_at_risk(&returns, 0.95);
        let ratio_p5 = percentile(&sorted, 0.05);
        assert!(
            (var_p5 - ratio_p5).abs() <= var_p5.abs() * 1e-15,
            "VaR's 5th percentile {var_p5} and tail_ratio's {ratio_p5} disagree by more than rounding"
        );

        // And the CVaR tail is the samples at or below that percentile.
        let cutoff = tail_cutoff(returns.len(), 1.0 - 0.95);
        let expected = -(sorted[..cutoff].iter().sum::<Real>() / cutoff as Real);
        assert_eq!(
            conditional_value_at_risk(&returns, 0.95).to_bits(),
            expected.to_bits()
        );
        assert!(sorted[cutoff - 1] <= ratio_p5.max(var_p5));
    }

    #[cfg(feature = "spec")]
    /// A spread of return series covering the shapes the cores have to agree
    /// with the public functions on: empty, one and two samples, zero variance,
    /// one-sided, and both parities of a long noisy series.
    fn sample_series() -> Vec<(&'static str, Vec<Real>)> {
        let noisy = |n: usize| -> Vec<Real> {
            let mut s: u64 = 0x1234_5678_9abc_def0;
            (0..n)
                .map(|_| {
                    s = s.wrapping_mul(6364136223846793005).wrapping_add(1);
                    ((s >> 33) as Real / u32::MAX as Real - 0.5) * 0.04
                })
                .collect()
        };
        vec![
            ("empty", vec![]),
            ("one", vec![0.01]),
            ("two", vec![0.01, -0.02]),
            ("flat_zero", vec![0.0; 7]),
            ("flat_nonzero", vec![0.003; 7]),
            ("all_gains", vec![0.01, 0.02, 0.003, 0.05, 0.001]),
            ("all_losses", vec![-0.01, -0.02, -0.003, -0.05]),
            ("odd_noisy", noisy(1_001)),
            ("even_noisy", noisy(1_000)),
        ]
    }

    #[cfg(feature = "spec")]
    fn trade(side: Side, pnl: Real, return_ratio: Real, entry_bar: usize, exit_bar: usize) -> Trade {
        Trade {
            entry_bar,
            exit_bar,
            side,
            units: 1.0,
            entry_price: 100.0,
            exit_price: 100.0 + pnl,
            pnl,
            return_ratio,
        }
    }

    #[cfg(feature = "spec")]
    /// The same, for the trade vector — including the two subsets that make an
    /// `Iterator::sum` seed observable (no winner, no loser).
    fn sample_trades() -> Vec<(&'static str, Vec<Trade>)> {
        let w = |p: Real, b: usize| trade(Side::Buy, p, p / 100.0, 0, b);
        let l = |p: Real, b: usize| trade(Side::Sell, -p, -p / 100.0, 0, b);
        vec![
            ("empty", vec![]),
            ("one_win", vec![w(5.0, 3)]),
            ("all_wins", vec![w(5.0, 3), w(1.0, 1), w(9.0, 12)]),
            ("all_losses", vec![l(5.0, 3), l(1.0, 1), l(9.0, 12)]),
            ("all_flat", vec![w(0.0, 2), w(0.0, 5)]),
            (
                "mixed",
                vec![w(5.0, 3), l(2.0, 1), l(7.0, 8), w(1.0, 0), w(4.0, 6), l(3.0, 2)],
            ),
        ]
    }

    #[cfg(feature = "spec")]
    /// Bit-equality, so a reordered accumulation fails rather than rounding into
    /// agreement. `-0.0` and `0.0` are different values here, deliberately —
    /// see `SUM_SEED`.
    #[track_caller]
    fn same(label: &str, want: Real, got: Real) {
        assert_eq!(
            want.to_bits(),
            got.to_bits(),
            "{label}: public {want:?}, core {got:?}"
        );
    }

    #[track_caller]
    #[cfg(feature = "spec")]
    fn same_opt(label: &str, want: Option<Real>, got: Option<Real>) {
        assert_eq!(
            want.map(Real::to_bits),
            got.map(Real::to_bits),
            "{label}: public {want:?}, core {got:?}"
        );
    }

    #[cfg(feature = "spec")]
    /// The cores in *Shared reduction cores* duplicate the derivations of the
    /// public per-metric functions rather than delegating to them — delegation
    /// would make a standalone `mean_return` pay for a two-pass gather it does
    /// not need. This is what keeps the duplicates honest: every number
    /// `from_report` now reads off a core is pinned, bit for bit, to the public
    /// function it replaced.
    #[test]
    fn reduction_cores_match_public_metrics() {
        const RF: Real = 0.045;
        const BPY: Real = 365.0;
        let rf_bar = RF / BPY;

        for (name, r) in sample_series() {
            let stats = ReturnStats::of(&r, rf_bar);
            same(&format!("{name}/mean"), mean_return(&r), stats.mean());
            same(&format!("{name}/best"), best_return(&r), stats.best());
            same(&format!("{name}/worst"), worst_return(&r), stats.worst());
            same(
                &format!("{name}/positive_ratio"),
                positive_bars_ratio(&r),
                stats.positive_ratio(),
            );
            same(&format!("{name}/stddev"), stddev_return(&r), stats.stddev());
            same(
                &format!("{name}/ann_mean"),
                annualized_return(&r, BPY),
                stats.annualized_mean(BPY),
            );
            same(
                &format!("{name}/ann_vol"),
                annualized_volatility(&r, BPY),
                stats.annualized_volatility(BPY),
            );
            same_opt(&format!("{name}/skewness"), skewness(&r), stats.skewness());
            same_opt(&format!("{name}/kurtosis"), kurtosis(&r), stats.kurtosis());
            same_opt(
                &format!("{name}/sharpe"),
                sharpe(&r, RF, BPY),
                stats.sharpe(RF, BPY),
            );
            same_opt(
                &format!("{name}/sortino"),
                sortino(&r, RF, BPY),
                stats.sortino(RF, BPY),
            );
            same_opt(&format!("{name}/omega"), omega(&r, rf_bar), stats.omega());
            assert_eq!(r.len(), stats.len(), "{name}/len");

            // A scratch copy, because `quantile_reads` permutes what it is
            // given and the public fronts below must see the original order.
            let mut scratch = r.clone();
            let q = quantile_reads(&mut scratch, 0.95);
            same(&format!("{name}/median"), median_return(&r), q.median);
            same(&format!("{name}/var"), value_at_risk(&r, 0.95), q.var);
            same(
                &format!("{name}/cvar"),
                conditional_value_at_risk(&r, 0.95),
                q.cvar,
            );
            same_opt(&format!("{name}/tail_ratio"), tail_ratio(&r), q.tail_ratio);
        }

        for (name, tr) in sample_trades() {
            let t = TradeStats::of(&tr);
            assert_eq!(total_trades(&tr), t.total, "{name}/total");
            assert_eq!(winning_trades(&tr), t.wins, "{name}/wins");
            assert_eq!(losing_trades(&tr), t.losses, "{name}/losses");
            assert_eq!(flat_trades(&tr), t.flat, "{name}/flat");
            assert_eq!(long_trades(&tr), t.longs, "{name}/longs");
            assert_eq!(short_trades(&tr), t.shorts, "{name}/shorts");
            assert_eq!(
                max_consecutive_wins(&tr),
                t.max_consec_wins,
                "{name}/consec_wins"
            );
            assert_eq!(
                max_consecutive_losses(&tr),
                t.max_consec_losses,
                "{name}/consec_losses"
            );
            assert_eq!(min_bars_held(&tr), t.min_bars, "{name}/min_bars");
            assert_eq!(max_bars_held(&tr), t.max_bars, "{name}/max_bars");
            same_opt(&format!("{name}/win_rate"), win_rate(&tr), t.win_rate());
            same_opt(
                &format!("{name}/profit_factor"),
                profit_factor(&tr),
                t.profit_factor(),
            );
            same_opt(
                &format!("{name}/payoff_ratio"),
                payoff_ratio(&tr),
                t.payoff_ratio(),
            );
            same_opt(
                &format!("{name}/expectancy"),
                expectancy(&tr),
                t.expectancy(),
            );
            same_opt(
                &format!("{name}/kelly"),
                kelly_fraction(&tr),
                t.kelly_fraction(),
            );
            same_opt(&format!("{name}/avg_win"), average_win(&tr), t.average_win());
            same_opt(
                &format!("{name}/avg_loss"),
                average_loss(&tr),
                t.average_loss(),
            );
            same_opt(
                &format!("{name}/largest_win"),
                largest_win(&tr),
                t.largest_win,
            );
            same_opt(
                &format!("{name}/largest_loss"),
                largest_loss(&tr),
                t.largest_loss,
            );
            same_opt(
                &format!("{name}/avg_return"),
                average_trade_return(&tr),
                t.average_return(),
            );
            same_opt(
                &format!("{name}/avg_bars"),
                average_bars_held(&tr),
                t.average_bars(),
            );
        }
    }

    fn indexed_fills(pairs: Vec<(usize, Order<Symbol>)>) -> Vec<Fill<Symbol>> {
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

    fn order_of(sym: &str, side: Side, units: Real, price: Real) -> Order<Symbol> {
        Order::new(
            crate::types::symbol(sym),
            side,
            units,
            price,
            OrderKind::Market,
            OrderId(0),
        )
    }

    /// The blotter of the two-asset repro in the 0.63.1 report: AAA long
    /// 100 → 110, BBB short 10 → 9, both opened on bar 1 and closed on bar 5.
    /// Walked with one shared position this produced *three* trades — pairing
    /// AAA's entry (100) with BBB's exit (10) for a −4500 loss that never
    /// happened, and BBB's 9 with AAA's 110 for a +5050 gain to match.
    #[test]
    fn interleaved_symbols_do_not_close_each_other() {
        let fills = indexed_fills(vec![
            (1, order_of("AAA", Side::Buy, 50.0, 100.0)),
            (1, order_of("BBB", Side::Sell, 500.0, 10.0)),
            (5, order_of("BBB", Side::Buy, 500.0, 9.0)),
            (5, order_of("AAA", Side::Sell, 50.0, 110.0)),
        ]);
        let trades = reconstruct_trades(&fills);

        assert_eq!(trades.len(), 2, "one round trip per symbol, not three legs");
        // Emitted in closing-fill order, so BBB (closed first) leads.
        let (bbb, aaa) = (&trades[0], &trades[1]);

        assert!(matches!(bbb.side, Side::Sell));
        assert!((bbb.entry_price - 10.0).abs() < 1e-9);
        assert!((bbb.exit_price - 9.0).abs() < 1e-9);
        assert!((bbb.pnl - 500.0).abs() < 1e-9);

        assert!(matches!(aaa.side, Side::Buy));
        assert!((aaa.entry_price - 100.0).abs() < 1e-9);
        assert!((aaa.exit_price - 110.0).abs() < 1e-9);
        assert!((aaa.pnl - 500.0).abs() < 1e-9);

        // Both legs are winners: the fabricated −4500 is gone, and so is the
        // +5050 that used to offset it into a plausible-looking total.
        assert!(trades.iter().all(|t| t.pnl > 0.0));
        assert_eq!(win_rate(&trades), Some(1.0));
        // Σpnl matched the true total even while every leg was wrong, so it is
        // deliberately *not* the assertion this test rests on.
        let total: Real = trades.iter().map(|t| t.pnl).sum();
        assert!((total - 1000.0).abs() < 1e-9);
    }

    /// Each symbol carries its own signed position, so a symbol's fills reduce
    /// only that symbol's leg no matter how the blotter interleaves them.
    #[test]
    fn each_symbol_keeps_its_own_running_position() {
        // AAA: +2 @100, +2 @110 (vwap 105) → close 4 @120.
        // BBB: -1 @50 → reverse +3 @40, closing the short and opening +2 @40,
        //      then close 2 @45.
        let fills = indexed_fills(vec![
            (0, order_of("AAA", Side::Buy, 2.0, 100.0)),
            (0, order_of("BBB", Side::Sell, 1.0, 50.0)),
            (1, order_of("AAA", Side::Buy, 2.0, 110.0)),
            (1, order_of("BBB", Side::Buy, 3.0, 40.0)),
            (2, order_of("AAA", Side::Sell, 4.0, 120.0)),
            (3, order_of("BBB", Side::Sell, 2.0, 45.0)),
        ]);
        let trades = reconstruct_trades(&fills);
        assert_eq!(trades.len(), 3);

        // BBB's short closes first (bar 1), then AAA (bar 2), then BBB's long.
        assert!(matches!(trades[0].side, Side::Sell));
        assert!((trades[0].pnl - 10.0).abs() < 1e-9); // (50 - 40) * 1

        assert!(matches!(trades[1].side, Side::Buy));
        assert!((trades[1].entry_price - 105.0).abs() < 1e-9); // vwap survives
        assert!((trades[1].pnl - 60.0).abs() < 1e-9); // (120 - 105) * 4

        assert!(matches!(trades[2].side, Side::Buy));
        assert!((trades[2].entry_price - 40.0).abs() < 1e-9); // reversal remainder
        assert!((trades[2].pnl - 10.0).abs() < 1e-9); // (45 - 40) * 2
    }

    /// The structural invariant behind both tests above: no emitted trade may
    /// draw its entry and exit prices from different instruments. Checked by
    /// replaying the walk per symbol in isolation — the union of the per-symbol
    /// reconstructions must be exactly what the interleaved blotter produced.
    #[test]
    fn no_trade_mixes_prices_across_symbols() {
        let fills = indexed_fills(vec![
            (0, order_of("AAA", Side::Buy, 1.0, 100.0)),
            (0, order_of("BBB", Side::Buy, 1.0, 7.0)),
            (1, order_of("CCC", Side::Sell, 4.0, 55.0)),
            (2, order_of("BBB", Side::Sell, 1.0, 9.0)),
            (3, order_of("AAA", Side::Sell, 1.0, 90.0)),
            (3, order_of("CCC", Side::Buy, 4.0, 50.0)),
            (4, order_of("AAA", Side::Buy, 2.0, 80.0)),
            (5, order_of("AAA", Side::Sell, 2.0, 85.0)),
        ]);
        let mixed = reconstruct_trades(&fills);

        let mut per_symbol = Vec::new();
        for sym in ["AAA", "BBB", "CCC"] {
            let only: Vec<Fill<Symbol>> = fills
                .iter()
                .filter(|f| f.order.symbol.as_ref() == sym)
                .cloned()
                .collect();
            per_symbol.extend(reconstruct_trades(&only));
        }

        assert_eq!(mixed.len(), per_symbol.len());
        // Same multiset of legs: interleaving changes only the emission order.
        for want in &per_symbol {
            assert!(
                mixed.iter().any(|got| got.entry_bar == want.entry_bar
                    && got.exit_bar == want.exit_bar
                    && (got.entry_price - want.entry_price).abs() < 1e-9
                    && (got.exit_price - want.exit_price).abs() < 1e-9
                    && (got.pnl - want.pnl).abs() < 1e-9),
                "leg {want:?} has no isolated-walk counterpart — prices crossed symbols",
            );
        }

        // And the emission order is non-decreasing in `exit_bar`, which is what
        // makes the consecutive win/loss streaks a time series rather than a
        // per-symbol artifact.
        assert!(mixed.windows(2).all(|w| w[0].exit_bar <= w[1].exit_bar));
    }

    /// A single-symbol blotter must reconstruct exactly as it did before the
    /// per-symbol split — the fix may not perturb the single-asset path.
    #[test]
    fn single_symbol_blotter_is_unchanged_by_grouping() {
        let fills = tagged_fills(vec![
            order(Side::Buy, 1.0, 100.0),
            order(Side::Buy, 1.0, 120.0),
            order(Side::Sell, 2.0, 130.0),
            order(Side::Sell, 1.0, 130.0),
            order(Side::Buy, 1.0, 125.0),
        ]);
        let trades = reconstruct_trades(&fills);
        assert_eq!(trades.len(), 2);
        assert!((trades[0].entry_price - 110.0).abs() < 1e-9); // vwap of 100/120
        assert!((trades[0].pnl - 40.0).abs() < 1e-9);
        assert!(matches!(trades[1].side, Side::Sell));
        assert!((trades[1].pnl - 5.0).abs() < 1e-9);
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
        // The longest *recovery*, not the deepest drop's fall: segment 0 spends
        // bars 2, 3 and 4 below its peak, segment 1 only bar 6. Peak-to-trough
        // for segment 0 is 2 bars, so this deliberately reads 3 rather than 2 —
        // a drawdown ends when the curve recovers, not when it stops falling.
        assert_eq!(max_drawdown_duration(&segs), 3);
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
