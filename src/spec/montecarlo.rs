//! Monte Carlo significance analysis over a completed backtest.
//!
//! Three estimators, all built on the seeded resampling core in
//! [`crate::montecarlo`]:
//!
//! 1. **Bootstrap confidence intervals** — resample the run's realized per-bar
//!    returns, rebuild the equity path, recompute each metric, and take a
//!    percentile CI + bootstrap standard error. Shape-agnostic; covers any
//!    metric recomputable from a return path (Sharpe, Sortino, Calmar, total
//!    return, drawdown, …). Trade-level metrics have no meaning on a resampled
//!    return path and are left `None`.
//! 2. **Cheap empirical-null p-values** (single-asset only) — hold the run's
//!    realized per-bar exposure fixed and re-pair it with block-resampled
//!    *market* returns. Small p = the strategy was positioned in step with
//!    genuine return structure rather than by luck. No strategy re-run.
//! 3. **Re-run empirical-null p-values** (all shapes) — block-resample the
//!    input price paths (chaining each symbol's own returns so intrabar OHLC
//!    geometry survives) and re-trade the strategy on each synthetic path.
//!    Small p = the edge survives when the exploitable serial structure is
//!    randomized away. This is the honest but expensive null; the resamples run
//!    in parallel.
//!
//! The resampling *scheme* (IID / moving-block / stationary) is orthogonal to
//! which estimator runs — see [`crate::montecarlo::ResampleScheme`]. A single
//! seed drives every estimator (drawn in a fixed order), so a reported block of
//! CIs and p-values reproduces exactly.

use crate::market::Real;
use crate::montecarlo::ResampleScheme;
use crate::spec::metrics::McSection;

// The `rand`/rayon-backed compute half — only compiled with the feature.
#[cfg(feature = "montecarlo")]
use std::collections::HashMap;
#[cfg(feature = "montecarlo")]
use rayon::prelude::*;
#[cfg(feature = "montecarlo")]
use crate::Timestamp;
#[cfg(feature = "montecarlo")]
use crate::market::Candle;
#[cfg(feature = "montecarlo")]
use crate::montecarlo::{
    McRng, percentile, resample_indices, resample_slice, rng_from_seed, std_dev,
};
#[cfg(feature = "montecarlo")]
use crate::spec::backtest::{EvalContext, measured_report_any};
#[cfg(feature = "montecarlo")]
use crate::spec::metrics::{McMetric, MetricKey, Metrics};
#[cfg(feature = "montecarlo")]
use crate::spec::runnable::StrategySpec;
#[cfg(feature = "montecarlo")]
use crate::types::Snapshot;

/// The headline metrics analyzed when the caller doesn't narrow the set. Each
/// is recomputable from a return path (so it gets a CI) and has an unambiguous
/// "better" direction (so it gets a p-value).
pub fn default_metrics() -> Vec<String> {
    [
        "risk_adjusted.sharpe",
        "risk_adjusted.sortino",
        "risk_adjusted.calmar",
        "returns.total_pct",
        "returns.annualized_mean_pct",
        "drawdown.max_pct",
    ]
    .iter()
    .map(|s| s.to_string())
    .collect()
}

/// The run's Monte Carlo configuration.
#[derive(Debug, Clone)]
pub struct McConfig {
    /// Resamples per estimator.
    pub permutations: usize,
    /// Resampling scheme (default [`ResampleScheme::Stationary`]).
    pub scheme: ResampleScheme,
    /// RNG seed — reproducible across platforms.
    pub seed: u64,
    /// Two-sided confidence level for the bootstrap CIs (e.g. `0.95`).
    pub ci_level: Real,
    /// Compute the cheap (positions-held-fixed) null p-values. Single-asset
    /// only; silently skipped for multi-symbol runs.
    pub cheap_null: bool,
    /// Compute the re-run (re-trade on synthetic paths) null p-values.
    pub rerun_null: bool,
    /// Metric names to analyze (short or dotted; resolved against the run's
    /// metrics). Empty ⇒ [`default_metrics`].
    pub metrics: Vec<String>,
}

impl Default for McConfig {
    fn default() -> Self {
        Self {
            permutations: 1000,
            scheme: ResampleScheme::Stationary { mean_block: 10.0 },
            seed: 0,
            ci_level: 0.95,
            cheap_null: true,
            rerun_null: false,
            metrics: Vec::new(),
        }
    }
}

/// The per-permutation resampled metric values, for `montecarlo.csv`.
pub struct McSamples {
    /// Column order (canonical dotted metric names).
    pub metric_names: Vec<String>,
    pub sets: Vec<McSampleSet>,
}

/// One estimator's resamples: `rows[p][m]` is metric `m`'s value on
/// permutation `p` (aligned to [`McSamples::metric_names`]).
pub struct McSampleSet {
    pub estimator: &'static str,
    pub rows: Vec<Vec<Option<Real>>>,
}

/// Everything a run produces for its Monte Carlo block: the summary section
/// that lands in `metrics.yml` and the raw resamples for `montecarlo.csv`.
pub struct McOutcome {
    pub section: McSection,
    pub samples: McSamples,
}

/// Run the requested Monte Carlo estimators over a completed backtest.
///
/// `observed` is the actual run's report; `spec` / `snapshots` / `ctx` are what
/// [`measured_report_any`] needs to re-drive the strategy for the re-run null.
#[cfg(feature = "montecarlo")]
pub fn run_montecarlo(
    spec: &StrategySpec,
    snapshots: &[Snapshot<String>],
    ctx: &EvalContext,
    observed: &crate::RunReport<String>,
    config: &McConfig,
) -> Result<McOutcome, String> {
    let names = if config.metrics.is_empty() {
        default_metrics()
    } else {
        config.metrics.clone()
    };

    // Resolve every requested name once against the actual run's metrics.
    let observed_metrics = ctx.reduce(observed);
    let mut keys: Vec<(String, MetricKey, bool)> = Vec::with_capacity(names.len());
    for name in &names {
        let key = MetricKey::from_name(name, &observed_metrics).map_err(|e| e.to_string())?;
        let dotted = key.dotted();
        let maximize = metric_is_maximize(&dotted);
        keys.push((dotted, key, maximize));
    }
    let observed_values: Vec<Option<Real>> = keys
        .iter()
        .map(|(_, k, _)| k.resolve(&observed_metrics).ok().flatten())
        .collect();
    let metric_names: Vec<String> = keys.iter().map(|(n, _, _)| n.clone()).collect();

    let n = config.permutations;
    let scheme = config.scheme;
    // One RNG stream, drawn in a fixed order so the whole block reproduces.
    let mut rng = rng_from_seed(config.seed);
    let mut sample_sets: Vec<McSampleSet> = Vec::new();

    // --- (1) Bootstrap CIs over realized per-bar returns ---------------------
    let observed_returns =
        crate::metrics::per_bar_returns(&observed.equity_curve, observed.initial_equity);
    let ci_rows = resampled_metric_rows(n, &mut rng, scheme, &keys, |rng| {
        let rs = resample_slice(&observed_returns, scheme, rng);
        metrics_from_returns(&rs, observed.initial_equity, ctx)
    });
    let ci_stats = summarize_ci(&ci_rows, keys.len(), config.ci_level);
    sample_sets.push(McSampleSet {
        estimator: "bootstrap_ci",
        rows: ci_rows,
    });

    // --- (2) Cheap null (single-asset) ---------------------------------------
    let universe = spec.universe(snapshots);
    let mut p_cheap: Vec<Option<Real>> = vec![None; keys.len()];
    if config.cheap_null
        && universe.len() == 1
        && let Some((exposure, market)) = exposure_and_market(observed, snapshots, &universe[0])
    {
        let rows = resampled_metric_rows(n, &mut rng, scheme, &keys, |rng| {
            let m_star = resample_slice(&market, scheme, rng);
            let r_star: Vec<Real> = exposure
                .iter()
                .zip(m_star.iter())
                .map(|(e, m)| e * m)
                .collect();
            metrics_from_returns(&r_star, observed.initial_equity, ctx)
        });
        p_cheap = column_pvalues(&rows, &keys, &observed_values);
        sample_sets.push(McSampleSet {
            estimator: "null_cheap",
            rows,
        });
    }

    // --- (3) Re-run null (all shapes, parallel) ------------------------------
    let mut p_rerun: Vec<Option<Real>> = vec![None; keys.len()];
    if config.rerun_null {
        // Draw all index sequences up front (sequential = deterministic), then
        // re-drive in parallel — each drive builds a fresh strategy.
        let bars = observed.equity_curve.len();
        let plan = precompute_rebuild(snapshots);
        let indices: Vec<Vec<usize>> =
            (0..n).map(|_| resample_indices(bars, scheme, &mut rng)).collect();
        let rows: Vec<Vec<Option<Real>>> = indices
            .par_iter()
            .map(|idx| {
                let rebuilt = plan.rebuild(snapshots, idx);
                match measured_report_any(spec, &rebuilt, ctx) {
                    Ok(report) => {
                        let m = ctx.reduce(&report);
                        keys.iter().map(|(_, k, _)| k.resolve(&m).ok().flatten()).collect()
                    }
                    Err(_) => vec![None; keys.len()],
                }
            })
            .collect();
        p_rerun = column_pvalues(&rows, &keys, &observed_values);
        sample_sets.push(McSampleSet {
            estimator: "null_rerun",
            rows,
        });
    }

    // --- Assemble the metrics.yml section ------------------------------------
    let metrics = keys
        .iter()
        .enumerate()
        .map(|(i, (name, _, _))| McMetric {
            name: name.clone(),
            observed: observed_values[i],
            ci_lower: ci_stats[i].0,
            ci_upper: ci_stats[i].1,
            std_error: ci_stats[i].2,
            p_value_cheap: p_cheap[i],
            p_value_rerun: p_rerun[i],
        })
        .collect();

    Ok(McOutcome {
        section: McSection {
            permutations: n,
            scheme: scheme.label(),
            seed: config.seed,
            ci_level: config.ci_level,
            metrics,
        },
        samples: McSamples {
            metric_names,
            sets: sample_sets,
        },
    })
}

/// Draw `n` resamples, reduce each to `Metrics` via `make`, and resolve every
/// key against it — one row of `Option<Real>` per resample.
#[cfg(feature = "montecarlo")]
fn resampled_metric_rows(
    n: usize,
    rng: &mut McRng,
    _scheme: ResampleScheme,
    keys: &[(String, MetricKey, bool)],
    make: impl Fn(&mut McRng) -> Metrics,
) -> Vec<Vec<Option<Real>>> {
    (0..n)
        .map(|_| {
            let m = make(rng);
            keys.iter().map(|(_, k, _)| k.resolve(&m).ok().flatten()).collect()
        })
        .collect()
}

/// Per-metric (ci_lower, ci_upper, std_error) from the bootstrap rows.
#[cfg(feature = "montecarlo")]
fn summarize_ci(
    rows: &[Vec<Option<Real>>],
    n_metrics: usize,
    ci_level: Real,
) -> Vec<(Option<Real>, Option<Real>, Option<Real>)> {
    let alpha = (1.0 - ci_level.clamp(0.0, 1.0)) / 2.0;
    (0..n_metrics)
        .map(|m| {
            let col: Vec<Real> = rows
                .iter()
                .filter_map(|r| r[m])
                .filter(|v| v.is_finite())
                .collect();
            let lower = percentile(&col, alpha);
            let upper = percentile(&col, 1.0 - alpha);
            let se = std_dev(&col);
            (lower, upper, se)
        })
        .collect()
}

/// Per-metric one-sided empirical p-values from a null's rows.
#[cfg(feature = "montecarlo")]
fn column_pvalues(
    rows: &[Vec<Option<Real>>],
    keys: &[(String, MetricKey, bool)],
    observed: &[Option<Real>],
) -> Vec<Option<Real>> {
    (0..keys.len())
        .map(|m| {
            let obs = observed[m]?;
            let maximize = keys[m].2;
            let col: Vec<Real> = rows.iter().filter_map(|r| r[m]).collect();
            if col.is_empty() {
                return None;
            }
            let extreme = col
                .iter()
                .filter(|&&v| if maximize { v >= obs } else { v <= obs })
                .count();
            // (1 + #extreme) / (1 + N) — the standard bias-corrected estimator
            // that never reports p == 0.
            Some((1.0 + extreme as Real) / (1.0 + col.len() as Real))
        })
        .collect()
}

/// Rebuild an equity path from a return series and reduce it to `Metrics`. The
/// synthetic report carries no fills, so trade-level metrics come back `None`.
#[cfg(feature = "montecarlo")]
fn metrics_from_returns(returns: &[Real], initial: Real, ctx: &EvalContext) -> Metrics {
    let mut prev = initial;
    let equity: Vec<Real> = returns
        .iter()
        .map(|r| {
            prev *= 1.0 + r;
            prev
        })
        .collect();
    let report = crate::RunReport {
        equity_curve: equity,
        fills: Vec::new(),
        rejections: Vec::new(),
        initial_equity: initial,
    };
    ctx.reduce(&report)
}

/// Whether a metric's "better" direction is *larger* (Sharpe, return) vs
/// *smaller* (drawdown, volatility, tail loss). Drives the p-value tail.
#[cfg(feature = "montecarlo")]
fn metric_is_maximize(dotted: &str) -> bool {
    const MINIMIZE: [&str; 6] = [
        "drawdown",
        "volatility",
        "var_95",
        "cvar_95",
        "ulcer_index",
        "worst",
    ];
    !MINIMIZE.iter().any(|k| dotted.contains(k))
}

/// Reconstruct the single-asset run's per-bar exposure fraction (held entering
/// each bar) and the symbol's per-bar market return. `None` if the symbol's
/// price series can't be recovered from the snapshots.
#[cfg(feature = "montecarlo")]
fn exposure_and_market(
    observed: &crate::RunReport<String>,
    snapshots: &[Snapshot<String>],
    symbol: &str,
) -> Option<(Vec<Real>, Vec<Real>)> {
    let n = observed.equity_curve.len();
    if n == 0 || snapshots.len() < n {
        return None;
    }
    // Symbol close per bar.
    let mut closes = Vec::with_capacity(n);
    for snap in &snapshots[..n] {
        let close = snap.iter().find_map(|(s, _f, atom)| {
            if s.map(|s| s.as_str() == symbol).unwrap_or(true) {
                atom.candle.map(|c| c.close)
            } else {
                None
            }
        })?;
        closes.push(close);
    }
    // Signed units established *before* each bar (decided last bar, held this
    // bar), from the fill blotter.
    let mut delta = vec![0.0; n];
    for fill in &observed.fills {
        if fill.bar < n {
            let signed = match fill.order.side {
                crate::wallet::Side::Buy => fill.order.units,
                crate::wallet::Side::Sell => -fill.order.units,
            };
            delta[fill.bar] += signed;
        }
    }
    let mut exposure = vec![0.0; n];
    let mut market = vec![0.0; n];
    let mut units_entering = 0.0;
    for t in 0..n {
        let equity_prev = if t == 0 {
            observed.initial_equity
        } else {
            observed.equity_curve[t - 1]
        };
        let mark_prev = if t == 0 { closes[0] } else { closes[t - 1] };
        exposure[t] = if equity_prev != 0.0 {
            units_entering * mark_prev / equity_prev
        } else {
            0.0
        };
        market[t] = if t == 0 || closes[t - 1] == 0.0 {
            0.0
        } else {
            closes[t] / closes[t - 1] - 1.0
        };
        units_entering += delta[t];
    }
    Some((exposure, market))
}

// ---------------------------------------------------------------------------
// Re-run null: block-resample each symbol's price path, chaining returns.
// ---------------------------------------------------------------------------

/// The per-bar geometry of one symbol's candle, relative to its own close, plus
/// the gross return that chains it onto the previous present bar.
#[cfg(feature = "montecarlo")]
#[derive(Clone, Copy)]
struct BarShape {
    gross: Real,
    ratio_open: Real,
    ratio_high: Real,
    ratio_low: Real,
    volume: Real,
}

/// Precomputed reconstruction plan: per-symbol bar shapes + anchor prices +
/// the original per-bar timestamps (so the synthetic path keeps a monotone
/// time axis).
#[cfg(feature = "montecarlo")]
struct RebuildPlan {
    shapes: HashMap<String, HashMap<usize, BarShape>>,
    anchor: HashMap<String, Real>,
    bar_times: Vec<Option<Timestamp>>,
}

#[cfg(feature = "montecarlo")]
impl RebuildPlan {
    /// Rebuild the snapshot stream following the resampled bar order `idx`.
    /// Each output bar takes its cross-section from source bar `idx[k]`, but
    /// every symbol's price is re-chained onto its own running synthetic price;
    /// the output bar's timestamp is the original bar `k`'s (monotone axis).
    fn rebuild(&self, snapshots: &[Snapshot<String>], idx: &[usize]) -> Vec<Snapshot<String>> {
        let mut running = self.anchor.clone();
        let mut out = Vec::with_capacity(idx.len());
        for (k, &source) in idx.iter().enumerate() {
            let time = self.bar_times.get(k).copied().flatten();
            let mut snap = Snapshot::new();
            for (sym, freq, atom) in snapshots[source].iter() {
                let mut new_atom = atom.clone();
                new_atom.time = time;
                if let Some(sym) = sym
                    && atom.candle.is_some()
                    && let Some(shape) = self.shapes.get(sym).and_then(|m| m.get(&source))
                {
                    let price = running.entry(sym.to_string()).or_insert(1.0);
                    *price *= shape.gross;
                    let close = *price;
                    new_atom.candle = Some(Candle {
                        open: close * shape.ratio_open,
                        high: close * shape.ratio_high,
                        low: close * shape.ratio_low,
                        close,
                        volume: shape.volume,
                    });
                }
                snap.push(sym.cloned(), freq, new_atom);
            }
            out.push(snap);
        }
        out
    }
}

/// Scan the snapshots once and build the [`RebuildPlan`].
#[cfg(feature = "montecarlo")]
fn precompute_rebuild(snapshots: &[Snapshot<String>]) -> RebuildPlan {
    // Gather each symbol's (bar, candle) in chronological order.
    let mut series: HashMap<String, Vec<(usize, Candle)>> = HashMap::new();
    for (k, snap) in snapshots.iter().enumerate() {
        for (sym, _freq, atom) in snap.iter() {
            if let (Some(sym), Some(candle)) = (sym, atom.candle) {
                series.entry(sym.to_string()).or_default().push((k, candle));
            }
        }
    }
    let mut shapes: HashMap<String, HashMap<usize, BarShape>> = HashMap::new();
    let mut anchor: HashMap<String, Real> = HashMap::new();
    for (sym, bars) in &series {
        if let Some((_, first)) = bars.first() {
            anchor.insert(sym.clone(), first.close);
        }
        let mut prev_close: Option<Real> = None;
        let entry = shapes.entry(sym.clone()).or_default();
        for (bar, c) in bars {
            let gross = match prev_close {
                Some(pc) if pc != 0.0 => c.close / pc,
                _ => 1.0,
            };
            let denom = if c.close != 0.0 { c.close } else { 1.0 };
            entry.insert(
                *bar,
                BarShape {
                    gross,
                    ratio_open: c.open / denom,
                    ratio_high: c.high / denom,
                    ratio_low: c.low / denom,
                    volume: c.volume,
                },
            );
            prev_close = Some(c.close);
        }
    }
    let bar_times = snapshots
        .iter()
        .map(|s| s.any_atom().and_then(|a| a.time))
        .collect();
    RebuildPlan {
        shapes,
        anchor,
        bar_times,
    }
}
