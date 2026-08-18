//! Candidate rewrites of the run-metrics reduction — `RunReport` → `Metrics`.
//!
//! `benches/metrics.rs` measures the *shipped* reduction. This file measures
//! what it could be. Nothing here is wired into the library: each variant is a
//! self-contained local reimplementation, so the numbers can be read before
//! anyone commits to the refactor.
//!
//! **Where the remaining 9.4 ms (200k bars) goes.** After F8 (`docs/PERFORMANCE.md`
//! — one sort instead of four, `max_drawdown` reused by `calmar` /
//! `recovery_factor`), the reduction still:
//!
//! 1. walks the equity curve **three** times — `per_bar_returns`,
//!    `drawdown_segments`, `ulcer_index` — each streaming 1.6 MB;
//! 2. walks the *return* series **~30** times. `sharpe` recomputes the mean and
//!    the stddev; `sortino` recomputes the mean; `skewness` and `kurtosis` each
//!    recompute the mean *and* the centred second moment; and
//!    `probabilistic_sharpe` recomputes all three of `sharpe` / `skewness` /
//!    `kurtosis` from scratch, for nine passes on its own;
//! 3. **sorts** the return series once (`sorted_asc`, a *stable* `sort_by`) to
//!    answer four quantile questions that between them need six order
//!    statistics and one tail mean;
//! 4. walks the trade vector ~20 times, two of those (`average_win`,
//!    `average_loss`) allocating a `Vec` to hold the filtered PnLs, and asks
//!    `average_bars_held` / `min_bars_held` / `max_bars_held` **twice** each —
//!    once for the `_bars` field, once for the `_seconds` twin.
//!
//! **The variants are cumulative**, so the group reads as a waterfall:
//!
//! | variant | change |
//! |---|---|
//! | `v0_shipped` | `fugazi::spec::metrics::from_report`, as released |
//! | `v1_local` | the same reduction, reimplemented here — the **control** |
//! | `v2_fused_returns` | + one two-pass `ReturnStats` bundle feeding every return metric |
//! | `v3_select_quantiles` | + `select_nth_unstable` order statistics instead of the sort |
//! | `v4_fused_trades` | + one pass over the trades, no filtered `Vec`s |
//! | `v5_fused_equity` | + one pass over the equity curve for all three of its consumers |
//!
//! `v1_local` exists to keep the waterfall honest: if it does not match
//! `v0_shipped` in both output and time, the rest of the column is measuring
//! the reimplementation rather than the optimisation.
//!
//! **Every variant is checked for bit-identity against `v0_shipped` before any
//! timing runs** (`check_equivalence`, from the first bench group). Fusing a pass does
//! not perturb floating-point results here — each accumulator sees the same
//! addends in the same order — so *exact* equality is the assertion, not a
//! tolerance. The one deliberate exception is `select_quantiles` with an
//! unsorted tail (`QuantileMode::SelectRawTail`): the CVaR tail mean then sums
//! in partition order rather than ascending order, which is a real (if ~1 ULP)
//! difference. It is benched but not adopted by the cumulative variants, and
//! `check_equivalence` prints its deviation instead of asserting on it.

use std::cmp::Ordering;
use std::hint::black_box;

use criterion::{BenchmarkId, Criterion, criterion_group, criterion_main};

use fugazi::backtest::{Fill, RunReport};
use fugazi::metrics::{DrawdownSegment, Trade};
use fugazi::prelude::*;
use fugazi::spec::metrics::{
    DrawdownSection, Metrics, ReturnSection, RiskAdjustedSection, RunSection, TradeSection,
};
use fugazi::wallet::{Order, OrderId, OrderKind};

mod common;
use common::synth_candles;

const SIZES: [usize; 3] = [10_000, 100_000, 200_000];

/// The same synthetic report `benches/metrics.rs` reduces, so the two files'
/// numbers are directly comparable: an equity curve from the price walk, plus
/// an alternating fill every 50 bars to give the trade metrics real input.
fn report(bars: usize) -> RunReport<Symbol> {
    let candles = synth_candles(bars);
    let equity_curve: Vec<Real> = candles.iter().map(|c| c.close * 100.0).collect();
    let fills: Vec<Fill<Symbol>> = (0..bars)
        .step_by(50)
        .enumerate()
        .map(|(i, bar)| Fill {
            bar,
            order: Order {
                id: OrderId(i as u64),
                symbol: fugazi::types::symbol("X"),
                side: if i % 2 == 0 { Side::Buy } else { Side::Sell },
                units: 1.0,
                price: candles[bar].close,
                kind: OrderKind::Market,
                commission: 0.0,
            },
        })
        .collect();
    RunReport {
        equity_curve,
        fills,
        rejections: Vec::new(),
        initial_equity: candles[0].close * 100.0,
    }
}

const BARS_PER_YEAR: Real = 365.0;
const RISK_FREE: Real = 0.045;

// ---------------------------------------------------------------------------
// The crate-private helpers the variants need, copied verbatim
// ---------------------------------------------------------------------------
//
// `sorted_asc`, `median_of_sorted`, `percentile`, `tail_mean` and `safe_div`
// are `pub(crate)` in the library (deliberately — see the `sorted_asc` doc
// comment: every `pub fn` in `metrics` owes a Python mirror). A bench is a
// separate crate, so they are reproduced here. Any drift between these and the
// library versions shows up immediately as a `check_equivalence` failure.

fn cmp_asc(a: &Real, b: &Real) -> Ordering {
    a.partial_cmp(b).unwrap_or(Ordering::Equal)
}

fn sorted_asc(xs: &[Real]) -> Vec<Real> {
    let mut v = xs.to_vec();
    v.sort_by(cmp_asc);
    v
}

fn median_of_sorted(sorted: &[Real]) -> Real {
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

/// R type-7, the crate's single quantile convention.
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

fn tail_mean(sorted: &[Real], p: Real) -> Real {
    if sorted.is_empty() {
        return 0.0;
    }
    let cutoff = ((sorted.len() as Real * p).ceil() as usize).max(1);
    sorted[..cutoff].iter().sum::<Real>() / cutoff as Real
}

fn safe_div(num: Real, denom: Real) -> Option<Real> {
    if denom > 0.0 && denom.is_finite() {
        Some(num / denom)
    } else {
        None
    }
}

/// The seed `impl Sum for f64` folds from.
///
/// Not a curiosity: `sum()` over an **empty** iterator returns this verbatim,
/// and three of the shipped trade metrics sum a filtered subset that is empty
/// whenever a run has no winner (or no loser). `profit_factor` on a run where
/// every trade lost is `Some(-0.0)`, and a fused accumulator seeded `0.0`
/// answers `Some(0.0)` — same number, different bits, and `metrics.yml`
/// serializes the sign.
const SIGNED_ZERO: Real = -0.0;

fn rf_per_bar(risk_free_rate: Real, bars_per_year: Real) -> Real {
    if bars_per_year > 0.0 {
        risk_free_rate / bars_per_year
    } else {
        0.0
    }
}

// ---------------------------------------------------------------------------
// Piece 1 — the equity-curve walk
// ---------------------------------------------------------------------------

/// The three things every reduction needs off the raw equity curve. Two of the
/// three are `O(bars)` streams of the same 1.6 MB at 200k bars.
struct EquityPass {
    returns: Vec<Real>,
    segments: Vec<DrawdownSegment>,
    ulcer: Real,
}

/// As shipped: three independent walks.
fn equity_pass_separate(equity: &[Real], initial: Real) -> EquityPass {
    EquityPass {
        returns: fugazi::metrics::per_bar_returns(equity, initial),
        segments: fugazi::metrics::drawdown_segments(equity),
        ulcer: fugazi::metrics::ulcer_index(equity),
    }
}

/// One walk feeding all three.
///
/// The two peak trackers are **not** redundant: `drawdown_segments` seeds its
/// peak from `equity[0]`, `ulcer_index` seeds from `0.0` and skips any bar
/// whose running peak is still non-positive. They agree on a positive curve and
/// disagree on one that starts at or below zero, so both are carried.
fn equity_pass_fused(equity: &[Real], initial: Real) -> EquityPass {
    let mut returns = Vec::with_capacity(equity.len());
    let mut segments = Vec::new();
    if equity.is_empty() {
        return EquityPass {
            returns,
            segments,
            ulcer: 0.0,
        };
    }

    let mut prev = initial;

    // `drawdown_segments` state.
    let mut peak = equity[0];
    let mut peak_idx = 0usize;
    let mut in_dd = false;
    let mut trough = peak;
    let mut trough_idx = 0usize;
    let mut underwater = 0usize;

    // `ulcer_index` state.
    let mut u_peak = 0.0_f64;
    let mut sum_sq = 0.0_f64;

    for (i, &e) in equity.iter().enumerate() {
        returns.push(if prev != 0.0 { (e - prev) / prev } else { 0.0 });
        prev = e;

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

        if e > u_peak {
            u_peak = e;
        }
        if u_peak > 0.0 {
            let d = (e - u_peak) / u_peak; // ≤ 0
            sum_sq += d * d;
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

    EquityPass {
        returns,
        segments,
        ulcer: (sum_sq / equity.len() as Real).sqrt(),
    }
}

// ---------------------------------------------------------------------------
// Piece 2 — the return-series moments
// ---------------------------------------------------------------------------

/// Everything the return series is asked for, other than its quantiles.
///
/// Held as raw accumulators rather than finished metrics so the derivations
/// below divide by exactly the divisor each metric wants (`n` for the central
/// moments, `n − 1` for the sample stddev) off one shared `Σ(x − mean)²`.
#[derive(Clone, Copy)]
struct ReturnStats {
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
    /// `Σ min(0, x − rf_per_bar)²`
    downside_sq: Real,
    /// `Σ max(x − rf_per_bar, 0)`
    omega_gains: Real,
    /// `Σ max(rf_per_bar − x, 0)`
    omega_losses: Real,
}

impl ReturnStats {
    fn empty() -> Self {
        Self {
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
        }
    }

    fn stddev(&self) -> Real {
        if self.n < 2 {
            0.0
        } else {
            (self.sum_sq / (self.n - 1) as Real).sqrt()
        }
    }

    /// Biased central second moment — the `skewness` / `kurtosis` denominator.
    /// `None` when it vanishes, matching both.
    fn m2(&self) -> Option<Real> {
        if self.n == 0 {
            return None;
        }
        let m2 = self.sum_sq / self.n as Real;
        (m2 != 0.0).then_some(m2)
    }

    fn skewness(&self) -> Option<Real> {
        let m2 = self.m2()?;
        Some((self.sum_cu / self.n as Real) / m2.powf(1.5))
    }

    fn kurtosis(&self) -> Option<Real> {
        let m2 = self.m2()?;
        Some((self.sum_qu / self.n as Real) / m2.powi(2) - 3.0)
    }

    fn positive_ratio(&self) -> Real {
        if self.n == 0 {
            0.0
        } else {
            self.positive as Real / self.n as Real
        }
    }

    fn downside_stddev(&self) -> Real {
        if self.n == 0 {
            0.0
        } else {
            (self.downside_sq / self.n as Real).sqrt()
        }
    }
}

/// The accumulators as the shipped code gathers them: one library call per
/// metric, each with its own walk (and `skewness` / `kurtosis` / `sharpe` /
/// `sortino` re-deriving the mean, or the mean *and* the second moment, on the
/// way).
///
/// Reconstructed into a [`ReturnStats`] so both halves of the A/B feed the
/// identical downstream code — the point of measurement is the gathering.
fn return_stats_separate(returns: &[Real], threshold: Real) -> ReturnStats {
    let n = returns.len();
    if n == 0 {
        return ReturnStats::empty();
    }
    let mean = fugazi::metrics::mean_return(returns);
    let sum_sq = returns.iter().map(|x| (x - mean).powi(2)).sum::<Real>();
    let sum_cu = returns.iter().map(|x| (x - mean).powi(3)).sum::<Real>();
    let sum_qu = returns.iter().map(|x| (x - mean).powi(4)).sum::<Real>();
    let downside_sq = returns
        .iter()
        .map(|x| (x - threshold).min(0.0).powi(2))
        .sum::<Real>();
    let mut omega_gains = 0.0;
    let mut omega_losses = 0.0;
    for &r in returns {
        let diff = r - threshold;
        if diff >= 0.0 {
            omega_gains += diff;
        } else {
            omega_losses += -diff;
        }
    }
    ReturnStats {
        n,
        mean,
        best: fugazi::metrics::best_return(returns),
        worst: fugazi::metrics::worst_return(returns),
        positive: returns.iter().filter(|&&r| r > 0.0).count(),
        sum_sq,
        sum_cu,
        sum_qu,
        downside_sq,
        omega_gains,
        omega_losses,
    }
}

/// Two passes, because the centred moments need the mean and the crate does not
/// take the `E[X²] − E[X]²` shortcut (see `WindowStats` in `src/indicators/stats.rs`
/// — it cancels away the leading digits and was wrong at crypto price scale).
///
/// Bit-identical to [`return_stats_separate`]: every accumulator still sees the
/// same addends in the same order, and `d2 * d` / `d2 * d2` are the same
/// multiplication trees `powi(3)` / `powi(4)` expand to.
fn return_stats_fused(returns: &[Real], threshold: Real) -> ReturnStats {
    let n = returns.len();
    if n == 0 {
        return ReturnStats::empty();
    }

    // Pass 1 — mean, extrema, sign count.
    //
    // Every accumulator that stands in for an `Iterator::sum::<f64>()` is
    // seeded `-0.0`, not `0.0`. `-0.0` is the additive identity f64 actually
    // has (`x + -0.0 == x` for every `x`, including `x = -0.0`, whereas
    // `-0.0 + 0.0 == +0.0`), so that is what `impl Sum for f64` folds from —
    // and the seed survives verbatim when the subset being summed is empty.
    // Seeding `0.0` here silently changes `profit_factor` from `-0.0` to `0.0`
    // on a run with no winning trade. See `SIGNED_ZERO` below.
    let mut sum = SIGNED_ZERO;
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

    // Pass 2 — centred moments, downside deviation, the Omega integrals.
    let mut sum_sq = SIGNED_ZERO;
    let mut sum_cu = SIGNED_ZERO;
    let mut sum_qu = SIGNED_ZERO;
    let mut downside_sq = SIGNED_ZERO;
    // `omega` is a hand-written loop in the library, seeded `0.0` — so this one
    // is `0.0` too. The seed is copied from whatever the shipped code does, not
    // chosen.
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

    ReturnStats {
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

// ---------------------------------------------------------------------------
// Piece 3 — the quantile reads
// ---------------------------------------------------------------------------

/// The four quantile answers the document needs off the return distribution.
#[derive(Clone, Copy, PartialEq, Debug)]
struct QuantileReads {
    median: Real,
    var_95: Real,
    cvar_95: Real,
    tail_ratio: Option<Real>,
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum QuantileMode {
    /// As shipped: one stable `sort_by` of a full copy.
    Sort,
    /// The same sort, unstable — a one-token change, no allocation of the
    /// merge scratch buffer.
    SortUnstable,
    /// `select_nth_unstable` for the six order statistics, then sort only the
    /// ~5% CVaR tail so its mean sums in ascending order (bit-identical).
    Select,
    /// The same, summing the tail in partition order. Faster, and **not**
    /// bit-identical — kept as the measurement of what the ordering costs.
    SelectRawTail,
}

/// The confidence `from_report` asks VaR and CVaR for.
const CONFIDENCE: Real = 0.95;

/// **Not `0.05`.** `value_at_risk` / `conditional_value_at_risk` take a
/// *confidence* and derive the tail as `1.0 - confidence`, and `1.0 - 0.95` is
/// `0.050000000000000044`, not `0.05`. `tail_ratio` writes its `0.05` as a
/// literal instead.
///
/// The 4.4e-17 gap between the two is not cosmetic: at 10 000 bars,
/// `p·(n − 1)` floors to index **500** for one and **499** for the other, and
/// `ceil(n·p)` gives a 501-element CVaR tail rather than 500. So the two spellings
/// are carried separately here — reproducing the shipped document means
/// reproducing which order statistic each read lands on.
const P_TAIL: Real = 1.0 - CONFIDENCE;

fn quantile_reads(returns: &[Real], mode: QuantileMode) -> QuantileReads {
    match mode {
        QuantileMode::Sort | QuantileMode::SortUnstable => {
            let sorted = if mode == QuantileMode::Sort {
                sorted_asc(returns)
            } else {
                let mut v = returns.to_vec();
                v.sort_unstable_by(cmp_asc);
                v
            };
            QuantileReads {
                median: median_of_sorted(&sorted),
                var_95: if sorted.is_empty() {
                    0.0
                } else {
                    -percentile(&sorted, P_TAIL)
                },
                cvar_95: if sorted.is_empty() {
                    0.0
                } else {
                    -tail_mean(&sorted, P_TAIL)
                },
                tail_ratio: if sorted.is_empty() {
                    None
                } else {
                    safe_div(
                        percentile(&sorted, 0.95).abs(),
                        percentile(&sorted, 0.05).abs(),
                    )
                },
            }
        }
        QuantileMode::Select | QuantileMode::SelectRawTail => {
            quantile_reads_by_selection(returns, mode == QuantileMode::Select)
        }
    }
}

/// Place `ks` (ascending, deduped, absolute indices) at their final positions
/// in `v` with introselect, recursing into the two partitions rather than
/// re-scanning the whole slice per index.
///
/// `base` is the absolute index of `v[0]`. Each `select_nth_unstable_by` costs
/// `O(len)` expected, and the recursion halves the length, so the six indices
/// this file asks for cost ~2n comparisons against the ~n·log₂n a sort pays.
///
/// The partition invariant survives the recursion — every later select
/// rearranges a contiguous sub-slice whose *element set* is already fixed — so
/// at the end `v[k]` is the k-th order statistic for every `k` in `ks`, and
/// `v[..k]` holds exactly the k smallest elements.
fn multi_select(v: &mut [Real], ks: &[usize], base: usize, out: &mut Vec<(usize, Real)>) {
    if ks.is_empty() {
        return;
    }
    let mid = ks.len() / 2;
    let k = ks[mid];
    let (lo, at, hi) = v.select_nth_unstable_by(k - base, cmp_asc);
    out.push((k, *at));
    multi_select(lo, &ks[..mid], base, out);
    multi_select(hi, &ks[mid + 1..], k + 1, out);
}

fn quantile_reads_by_selection(returns: &[Real], sort_tail: bool) -> QuantileReads {
    let n = returns.len();
    if n == 0 {
        return QuantileReads {
            median: 0.0,
            var_95: 0.0,
            cvar_95: 0.0,
            tail_ratio: None,
        };
    }

    // The order statistics the four reads bottom out at. Three straddling
    // pairs, not two: VaR's tail sits at `P_TAIL` and `tail_ratio`'s lower leg
    // at the literal `0.05`, which are different indices — see `P_TAIL`.
    let quantile_pair = |p: Real| -> (usize, usize) {
        let idx = p * (n - 1) as Real;
        let lo = idx.floor() as usize;
        (lo, (lo + 1).min(n - 1))
    };
    let (lo_var, hi_var) = quantile_pair(P_TAIL);
    let (lo05, hi05) = quantile_pair(0.05);
    let (lo95, hi95) = quantile_pair(0.95);
    let cutoff = ((n as Real * P_TAIL).ceil() as usize).max(1);

    let mut ks = vec![lo_var, hi_var, lo05, hi05, lo95, hi95, cutoff - 1];
    if n.is_multiple_of(2) {
        ks.push(n / 2 - 1);
        ks.push(n / 2);
    } else {
        ks.push(n / 2);
    }
    ks.sort_unstable();
    ks.dedup();

    let mut v = returns.to_vec();
    let mut found: Vec<(usize, Real)> = Vec::with_capacity(ks.len());
    multi_select(&mut v, &ks, 0, &mut found);
    let at = |k: usize| -> Real {
        found
            .iter()
            .find(|(i, _)| *i == k)
            .expect("every requested index was selected")
            .1
    };

    // R type-7 interpolation, on the two order statistics it straddles.
    let quantile = |p: Real, lo: usize, hi: usize| -> Real {
        if n == 1 {
            return at(0);
        }
        let frac = p * (n - 1) as Real - lo as Real;
        at(lo) * (1.0 - frac) + at(hi) * frac
    };
    let p_var = quantile(P_TAIL, lo_var, hi_var);
    let p05 = quantile(0.05, lo05, hi05);
    let p95 = quantile(0.95, lo95, hi95);

    let median = if n.is_multiple_of(2) {
        (at(n / 2 - 1) + at(n / 2)) / 2.0
    } else {
        at(n / 2)
    };

    // `v[..cutoff]` is the bottom-`cutoff` multiset (the `cutoff - 1` select
    // partitioned it), but in arbitrary order. Sorting it makes the mean sum
    // ascending, exactly as `tail_mean` over a fully-sorted copy would.
    let tail = &mut v[..cutoff];
    if sort_tail {
        tail.sort_unstable_by(cmp_asc);
    }
    let cvar = -(tail.iter().sum::<Real>() / cutoff as Real);

    QuantileReads {
        median,
        var_95: -p_var,
        cvar_95: cvar,
        tail_ratio: safe_div(p95.abs(), p05.abs()),
    }
}

// ---------------------------------------------------------------------------
// Piece 4 — the trade-level aggregates
// ---------------------------------------------------------------------------

/// Everything `TradeSection` needs off the reconstructed trades.
struct TradeStats {
    total: usize,
    wins: usize,
    losses: usize,
    flat: usize,
    longs: usize,
    shorts: usize,
    max_consec_wins: usize,
    max_consec_losses: usize,
    sum_pnl: Real,
    sum_win_pnl: Real,
    sum_loss_pnl: Real,
    largest_win: Option<Real>,
    largest_loss: Option<Real>,
    sum_return_ratio: Real,
    sum_bars: Real,
    min_bars: Option<usize>,
    max_bars: Option<usize>,
}

impl TradeStats {
    fn win_rate(&self) -> Option<Real> {
        (self.total > 0).then(|| self.wins as Real / self.total as Real)
    }
    fn profit_factor(&self) -> Option<Real> {
        safe_div(self.sum_win_pnl, -self.sum_loss_pnl)
    }
    fn average_win(&self) -> Option<Real> {
        (self.wins > 0).then(|| self.sum_win_pnl / self.wins as Real)
    }
    fn average_loss(&self) -> Option<Real> {
        (self.losses > 0).then(|| self.sum_loss_pnl / self.losses as Real)
    }
    fn payoff_ratio(&self) -> Option<Real> {
        match (self.average_win(), self.average_loss()) {
            (Some(w), Some(l)) if l < 0.0 => Some(w / -l),
            _ => None,
        }
    }
    fn expectancy(&self) -> Option<Real> {
        (self.total > 0).then(|| self.sum_pnl / self.total as Real)
    }
    fn kelly(&self) -> Option<Real> {
        match (self.win_rate(), self.payoff_ratio()) {
            (Some(p), Some(b)) if b > 0.0 => Some(p - (1.0 - p) / b),
            _ => None,
        }
    }
    fn average_return(&self) -> Option<Real> {
        (self.total > 0).then(|| self.sum_return_ratio / self.total as Real)
    }
    fn average_bars(&self) -> Option<Real> {
        (self.total > 0).then(|| self.sum_bars / self.total as Real)
    }
}

/// As shipped: ~20 walks, two of which (`average_win` / `average_loss`) collect
/// the filtered PnLs into a fresh `Vec` before averaging them.
fn trade_stats_separate(trades: &[Trade]) -> TradeStats {
    TradeStats {
        total: fugazi::metrics::total_trades(trades),
        wins: fugazi::metrics::winning_trades(trades),
        losses: fugazi::metrics::losing_trades(trades),
        flat: fugazi::metrics::flat_trades(trades),
        longs: fugazi::metrics::long_trades(trades),
        shorts: fugazi::metrics::short_trades(trades),
        max_consec_wins: fugazi::metrics::max_consecutive_wins(trades),
        max_consec_losses: fugazi::metrics::max_consecutive_losses(trades),
        sum_pnl: trades.iter().map(|t| t.pnl).sum(),
        sum_win_pnl: trades.iter().map(|t| t.pnl).filter(|&p| p > 0.0).sum(),
        sum_loss_pnl: trades.iter().map(|t| t.pnl).filter(|&p| p < 0.0).sum(),
        largest_win: fugazi::metrics::largest_win(trades),
        largest_loss: fugazi::metrics::largest_loss(trades),
        sum_return_ratio: trades.iter().map(|t| t.return_ratio).sum(),
        sum_bars: trades.iter().map(|t| t.bars_held() as Real).sum(),
        min_bars: fugazi::metrics::min_bars_held(trades),
        max_bars: fugazi::metrics::max_bars_held(trades),
    }
}

/// One walk, no allocation. Bit-identical: each accumulator still sees its
/// addends in trade order, and the streak counters are the same state machine
/// `metrics::longest_streak` runs behind `max_consecutive_wins`.
fn trade_stats_fused(trades: &[Trade]) -> TradeStats {
    let mut s = TradeStats {
        total: trades.len(),
        wins: 0,
        losses: 0,
        flat: 0,
        longs: 0,
        shorts: 0,
        max_consec_wins: 0,
        max_consec_losses: 0,
        // All five mirror an `Iterator::sum::<f64>()`. See `SIGNED_ZERO`.
        sum_pnl: SIGNED_ZERO,
        sum_win_pnl: SIGNED_ZERO,
        sum_loss_pnl: SIGNED_ZERO,
        largest_win: None,
        largest_loss: None,
        sum_return_ratio: SIGNED_ZERO,
        sum_bars: SIGNED_ZERO,
        min_bars: None,
        max_bars: None,
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

// ---------------------------------------------------------------------------
// The variants
// ---------------------------------------------------------------------------

/// Which of the four candidate changes a run of [`reduce`] applies. The
/// cumulative variants turn them on left to right.
#[derive(Clone, Copy)]
struct Opts {
    fused_equity: bool,
    fused_returns: bool,
    quantiles: QuantileMode,
    fused_trades: bool,
}

impl Opts {
    /// `v1_local` — the shipped reduction's exact shape.
    const SHIPPED: Self = Self {
        fused_equity: false,
        fused_returns: false,
        quantiles: QuantileMode::Sort,
        fused_trades: false,
    };
    const FUSED_RETURNS: Self = Self {
        fused_returns: true,
        ..Self::SHIPPED
    };
    const SELECT_QUANTILES: Self = Self {
        quantiles: QuantileMode::Select,
        ..Self::FUSED_RETURNS
    };
    const FUSED_TRADES: Self = Self {
        fused_trades: true,
        ..Self::SELECT_QUANTILES
    };
    const FUSED_EQUITY: Self = Self {
        fused_equity: true,
        ..Self::FUSED_TRADES
    };
}

/// `RunReport` → `Metrics`, assembled from whichever pieces `opts` selects.
///
/// The *assembly* is identical across variants — same fields, same presentation
/// scaling, same `Option` propagation — so a timing difference between two
/// `Opts` is the piece that changed and nothing else.
fn reduce<Sym>(
    report: &RunReport<Sym>,
    bars_per_year: Real,
    risk_free_rate: Real,
    seconds_per_bar: Option<Real>,
    opts: Opts,
) -> Metrics {
    let equity = report.equity_curve.as_slice();
    let bars = equity.len();
    let initial = report.initial_equity;
    let final_equity = equity.last().copied().unwrap_or(initial);
    let rf_bar = rf_per_bar(risk_free_rate, bars_per_year);

    let EquityPass {
        returns,
        segments,
        ulcer,
    } = if opts.fused_equity {
        equity_pass_fused(equity, initial)
    } else {
        equity_pass_separate(equity, initial)
    };

    let stats = if opts.fused_returns {
        return_stats_fused(&returns, rf_bar)
    } else {
        return_stats_separate(&returns, rf_bar)
    };
    let quantiles = quantile_reads(&returns, opts.quantiles);

    let trades = fugazi::metrics::reconstruct_trades(&report.fills);
    let t = if opts.fused_trades {
        trade_stats_fused(&trades)
    } else {
        trade_stats_separate(&trades)
    };

    let total = fugazi::metrics::total_return(equity, initial);
    let cagr = fugazi::metrics::cagr(equity, initial, bars_per_year);
    let stddev = stats.stddev();
    let ann_scale = bars_per_year.max(0.0).sqrt();
    let ann_mean = stats.mean * bars_per_year;
    let ann_vol = stddev * ann_scale;
    let ann_excess = ann_mean - risk_free_rate;
    let max_dd = fugazi::metrics::max_drawdown(&segments);
    let avg_dd = fugazi::metrics::average_drawdown(&segments);
    let sharpe = safe_div(ann_excess, ann_vol);
    let skewness = stats.skewness();
    let kurtosis = stats.kurtosis();

    // Asked once, spent twice — the `_bars` field and its `_seconds` twin. The
    // shipped reduction calls each of these three a second time inside the
    // `seconds_per_bar.and_then(..)`.
    let average_bars = t.average_bars();
    let min_bars = t.min_bars;
    let max_bars = t.max_bars;

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
            mean_bar: stats.mean,
            median_bar: quantiles.median,
            stddev_bar: stddev,
            best_bar: stats.best,
            worst_bar: stats.worst,
            positive_bars_pct: stats.positive_ratio() * 100.0,
            skewness,
            kurtosis,
            var_95: quantiles.var_95,
            cvar_95: quantiles.cvar_95,
            tail_ratio: quantiles.tail_ratio,
            annualized_mean_pct: ann_mean * 100.0,
            annualized_volatility_pct: ann_vol * 100.0,
        },
        risk_adjusted: RiskAdjustedSection {
            sharpe,
            sortino: safe_div(ann_excess, stats.downside_stddev() * ann_scale),
            calmar: cagr.and_then(|c| safe_div(c, max_dd)),
            omega: safe_div(stats.omega_gains, stats.omega_losses),
            ulcer_index: ulcer,
            ulcer_performance_index: cagr.and_then(|c| safe_div(c - risk_free_rate, ulcer)),
            probabilistic_sharpe: fugazi::metrics::probabilistic_sharpe_from_stats(
                sharpe,
                skewness,
                kurtosis,
                stats.n,
                bars_per_year,
                0.0,
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
            recovery_factor: safe_div(total, max_dd),
        },
        costs: None,
        montecarlo: None,
        trades: TradeSection {
            total: t.total,
            wins: t.wins,
            losses: t.losses,
            flat: t.flat,
            long_trades: t.longs,
            short_trades: t.shorts,
            total_fills: report.fills.len(),
            max_consecutive_wins: t.max_consec_wins,
            max_consecutive_losses: t.max_consec_losses,
            exposure_pct: fugazi::metrics::exposure_ratio(&report.fills, bars) * 100.0,
            win_rate_pct: t.win_rate().map(|w| w * 100.0),
            profit_factor: t.profit_factor(),
            payoff_ratio: t.payoff_ratio(),
            expectancy: t.expectancy(),
            kelly_fraction: t.kelly(),
            average_win: t.average_win(),
            average_loss: t.average_loss(),
            largest_win: t.largest_win,
            largest_loss: t.largest_loss,
            average_return_pct: t.average_return().map(|r| r * 100.0),
            average_bars,
            min_bars,
            max_bars,
            average_seconds: seconds_per_bar.and_then(|s| average_bars.map(|b| b * s)),
            min_seconds: seconds_per_bar.and_then(|s| min_bars.map(|b| b as Real * s)),
            max_seconds: seconds_per_bar.and_then(|s| max_bars.map(|b| b as Real * s)),
        },
    }
}

const VARIANTS: [(&str, Opts); 5] = [
    ("v1_local", Opts::SHIPPED),
    ("v2_fused_returns", Opts::FUSED_RETURNS),
    ("v3_select_quantiles", Opts::SELECT_QUANTILES),
    ("v4_fused_trades", Opts::FUSED_TRADES),
    ("v5_fused_equity", Opts::FUSED_EQUITY),
];

// ---------------------------------------------------------------------------
// Equivalence — run before any timing
// ---------------------------------------------------------------------------

/// Assert every variant reproduces the shipped document **exactly**, and report
/// the one place a deviation is expected.
///
/// Exact equality is the right bar here: none of the four changes reorders an
/// accumulation. A variant that lands with a `1e-16` deviation has reordered
/// something by accident, and a benchmark of the wrong arithmetic is worth
/// nothing — so this fails the run rather than warning.
fn check_equivalence() {
    for n in SIZES {
        let rep = report(n);
        let want =
            fugazi::spec::metrics::from_report(&rep, BARS_PER_YEAR, RISK_FREE, Some(86_400.0));
        let want = fugazi::spec::metrics::flatten(&want);

        for (name, opts) in VARIANTS {
            let got = reduce(&rep, BARS_PER_YEAR, RISK_FREE, Some(86_400.0), opts);
            let got = fugazi::spec::metrics::flatten(&got);
            assert_eq!(want.len(), got.len(), "{name}/{n}: field count differs");
            for ((wk, wv), (gk, gv)) in want.iter().zip(got.iter()) {
                assert_eq!(wk, gk, "{name}/{n}: field order differs");
                assert_eq!(
                    wv.map(Real::to_bits),
                    gv.map(Real::to_bits),
                    "{name}/{n}: {wk} — shipped {wv:?}, variant {gv:?}",
                );
            }
        }
    }
    println!("metrics_variants: all {} variants bit-identical", VARIANTS.len());

    // The one variant held out of the waterfall, reported rather than asserted.
    let rep = report(200_000);
    let returns = fugazi::metrics::per_bar_returns(&rep.equity_curve, rep.initial_equity);
    let exact = quantile_reads(&returns, QuantileMode::Sort);
    let raw = quantile_reads(&returns, QuantileMode::SelectRawTail);
    let rel = |a: Real, b: Real| if a == 0.0 { 0.0 } else { ((b - a) / a).abs() };
    println!(
        "metrics_variants: SelectRawTail cvar_95 relative deviation {:e} (median {:e}, var_95 {:e})",
        rel(exact.cvar_95, raw.cvar_95),
        rel(exact.median, raw.median),
        rel(exact.var_95, raw.var_95),
    );
}

// ---------------------------------------------------------------------------
// Benchmarks
// ---------------------------------------------------------------------------

/// The waterfall: shipped, the control, then one change at a time.
fn bench_variants(c: &mut Criterion) {
    // Before the first timing, not from `main`: `criterion_main!` owns `main`,
    // and a variant that computes the wrong numbers must not be timed at all.
    static CHECKED: std::sync::Once = std::sync::Once::new();
    CHECKED.call_once(check_equivalence);

    let mut g = c.benchmark_group("metrics_variants/from_report");
    for n in SIZES {
        let rep = report(n);
        g.bench_with_input(BenchmarkId::new("v0_shipped", n), &n, |b, _| {
            b.iter(|| {
                black_box(fugazi::spec::metrics::from_report(
                    &rep,
                    BARS_PER_YEAR,
                    RISK_FREE,
                    None,
                ))
            });
        });
        for (name, opts) in VARIANTS {
            g.bench_with_input(BenchmarkId::new(name, n), &n, |b, _| {
                b.iter(|| black_box(reduce(&rep, BARS_PER_YEAR, RISK_FREE, None, opts)));
            });
        }
    }
    g.finish();
}

/// Each piece on its own, at the size where the reduction hurts, so a variant's
/// share of the waterfall is attributable to a mechanism.
fn bench_pieces(c: &mut Criterion) {
    let rep = report(200_000);
    let equity = rep.equity_curve.as_slice();
    let returns = fugazi::metrics::per_bar_returns(equity, rep.initial_equity);
    let trades = fugazi::metrics::reconstruct_trades(&rep.fills);
    let rf_bar = rf_per_bar(RISK_FREE, BARS_PER_YEAR);

    let mut g = c.benchmark_group("metrics_variants/equity_pass");
    g.bench_function("separate_three_walks", |b| {
        b.iter(|| black_box(equity_pass_separate(equity, rep.initial_equity)));
    });
    g.bench_function("fused_one_walk", |b| {
        b.iter(|| black_box(equity_pass_fused(equity, rep.initial_equity)));
    });
    g.finish();

    let mut g = c.benchmark_group("metrics_variants/return_stats");
    g.bench_function("separate_passes", |b| {
        b.iter(|| black_box(return_stats_separate(&returns, rf_bar)));
    });
    g.bench_function("fused_two_pass", |b| {
        b.iter(|| black_box(return_stats_fused(&returns, rf_bar)));
    });
    g.finish();

    let mut g = c.benchmark_group("metrics_variants/quantiles");
    for (name, mode) in [
        ("sort_stable", QuantileMode::Sort),
        ("sort_unstable", QuantileMode::SortUnstable),
        ("select", QuantileMode::Select),
        ("select_raw_tail", QuantileMode::SelectRawTail),
    ] {
        g.bench_function(name, |b| {
            b.iter(|| black_box(quantile_reads(&returns, mode)));
        });
    }
    g.finish();

    let mut g = c.benchmark_group("metrics_variants/trade_stats");
    g.bench_function("separate_passes", |b| {
        b.iter(|| black_box(trade_stats_separate(&trades)));
    });
    g.bench_function("fused_one_pass", |b| {
        b.iter(|| black_box(trade_stats_fused(&trades)));
    });
    g.finish();
}

criterion_group!(benches, bench_variants, bench_pieces);
criterion_main!(benches);
