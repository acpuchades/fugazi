//! Pure per-iteration evaluation. No IO, no console output, no clock.
//!
//! This module owns the "run one backtest, reduce it to a metrics
//! document" work — the piece that has to run identically whether it's
//! called once for a single-run ([`crate::run::run`]) or many times in
//! parallel by the batch driver ([`crate::batch`]). Writing the results
//! (CSV files, YAML summaries, console banners) is deliberately kept out
//! of here; that's a concern of the `run` subcommand driver, and this
//! module never returns a `Path`, opens a file, or calls `println!`.
//!
//! ## The three pure entry points
//!
//! * [`run_iteration`] — the "full" pure evaluation: drives one backtest
//!   over `candles` through a paper wallet, produces the whole-run
//!   [`metrics::Metrics`] document, optionally the gross twin under active
//!   costs, and (when `-w N` is set) the windowed + rolling reductions.
//!   Returns everything the driver needs to write files and print
//!   summaries via the [`IterationResult`] payload.
//! * [`evaluate`] — a thin metrics-only wrapper for `optimize`'s grid loop.
//! * [`evaluate_windowed`] — the same shape but with a windowed reduction.
//!
//! ## Warm-up and stability
//!
//! Metrics cover the whole run — the strategy layer is opinion-free about
//! stability. A strategy that wants entries held off until every source it
//! consults has settled composes the check at the entry with `!stable`, i.e.
//! `!all [<entry>, !stable { signal: <entry> }]`.

use std::num::NonZeroUsize;

use fugazi::prelude::*;

use crate::calendar::Frequency;
use crate::costs::CostConfig;
use crate::metrics;
use crate::spec::StrategySpec;

/// Drive `spec` over `candles` through a fresh paper wallet with `cash`
/// starting funds and the given trading `costs`, returning the full
/// [`fugazi::RunReport`]. The shared core of [`evaluate`] and
/// [`evaluate_windowed`].
fn measured_report(
    spec: &StrategySpec,
    candles: &[(String, Candle)],
    cash: Real,
    costs: TradingCosts,
) -> fugazi::RunReport<String> {
    let symbol = spec.symbol.clone();
    let mut strategy = spec.build();
    let mut wallet = PaperWallet::with_costs(cash, costs);
    fugazi::backtest::run(
        &mut strategy,
        &mut wallet,
        symbol,
        candles.iter().map(|(_, c)| *c),
    )
}

/// Pure metrics-only evaluation: drive `spec` over `candles` through a paper
/// wallet with `cash` starting funds, the given `cost_config` resolved for
/// (spec's symbol, `frequency`), and reduce the run to a [`metrics::Metrics`]
/// document. The shape `optimize` calls per grid combination.
pub fn evaluate(
    spec: &StrategySpec,
    candles: &[(String, Candle)],
    cash: Real,
    bars_per_year: Real,
    risk_free_rate: Real,
    cost_config: &CostConfig,
    frequency: Option<Frequency>,
) -> metrics::Metrics {
    let costs = cost_config.resolve(&spec.symbol, frequency);
    let measured = measured_report(spec, candles, cash, costs);
    metrics::from_report(&measured, bars_per_year, risk_free_rate)
}

/// The windowed twin of [`evaluate`]: reduce the same measured run to one
/// [`metrics::Metrics`] per non-overlapping `window`-bar span — what
/// `optimize -w/--windowed` calls per grid combination.
#[allow(clippy::too_many_arguments)]
pub fn evaluate_windowed(
    spec: &StrategySpec,
    candles: &[(String, Candle)],
    cash: Real,
    bars_per_year: Real,
    risk_free_rate: Real,
    cost_config: &CostConfig,
    frequency: Option<Frequency>,
    window: usize,
) -> Vec<metrics::WindowMetrics> {
    let costs = cost_config.resolve(&spec.symbol, frequency);
    let measured = measured_report(spec, candles, cash, costs);
    metrics::windowed_from_report(&measured, window, bars_per_year, risk_free_rate)
}

/// Everything one iteration of a backtest produces — used by both the
/// single-run driver ([`crate::run`]) and the parallel batch driver
/// ([`crate::batch`]). Deliberately owns no IO — the driver decides how
/// (and whether) to persist the payload.
pub struct IterationResult {
    /// The strategy's symbol for this iteration (already post-param
    /// substitution). Under batch mode this matches the iteration's group.
    pub symbol: String,
    /// The effective bar cadence for this iteration (user's `-f` or
    /// auto-detected). `None` when neither was available.
    pub freq: Option<Frequency>,
    /// One time label per bar, borrowed from the input candles column and
    /// cloned so the result is `Send + 'static` for rayon workers.
    pub bars: Vec<String>,
    /// The priced (net) run report from `fugazi::backtest::run`.
    pub report: fugazi::RunReport<String>,
    /// Whole-run metrics document.
    pub metrics: metrics::Metrics,
    /// Whole-run metrics for the gross twin, when it exists.
    pub gross_metrics: Option<metrics::Metrics>,
    /// Non-overlapping N-bar window rows, when `-w N` was set.
    pub windowed: Option<Vec<metrics::WindowMetrics>>,
    /// Rolling N-bar window rows, when `-w N` was set (same N).
    pub rolling: Option<Vec<metrics::WindowMetrics>>,
    /// Precomputed summary numbers so callers don't reduce the report twice.
    pub summary: SummaryRow,
    /// True when a cost model was active — governs `commission` column
    /// emission in the trade CSV and gross/net console rows.
    pub costs_active: bool,
}

/// Precomputed inside [`run_iteration`] so IO callers don't reduce the
/// report to these numbers twice.
pub struct SummaryRow {
    pub final_equity: Real,
    pub trades: usize,
    pub bars: usize,
}

/// The resolved-once inputs [`run_iteration`] consumes. Kept separate from
/// the driver's option struct (see [`crate::run::RunOptions`]) so a batch
/// worker can build one per iteration without carrying `out_dir`,
/// `strategy_label`, etc. — the per-run knobs that don't vary per
/// iteration and belong in the IO layer.
pub struct IterationInputs<'a> {
    pub cash: Real,
    pub bars_per_year: Real,
    pub risk_free_rate: Real,
    pub cost_config: &'a CostConfig,
    pub effective_freq: Option<Frequency>,
    pub windowed: Option<NonZeroUsize>,
}

/// The pure-work half of a run: drive the strategy over `candles`, reduce
/// the report to `Metrics`, and hand back an [`IterationResult`]. Does no
/// IO and no console printing — that's the driver's responsibility, and
/// the batch driver ([`crate::batch`]) calls this in parallel across
/// iterations.
pub fn run_iteration(
    spec: &StrategySpec,
    candles: &[(String, Candle)],
    inputs: &IterationInputs,
) -> IterationResult {
    let symbol = spec.symbol.clone();
    let costs = inputs.cost_config.resolve(&symbol, inputs.effective_freq);
    let costs_active = !costs.is_none();
    let mut strategy = spec.build();
    let mut wallet = PaperWallet::with_costs(inputs.cash, costs);
    let report = fugazi::backtest::run(
        &mut strategy,
        &mut wallet,
        symbol.clone(),
        candles.iter().map(|(_, c)| *c),
    );
    // Gross twin under active costs: same strategy/candles/cash, no cost
    // model, so any difference is attributable to costs alone.
    let gross_report = if costs_active {
        let mut gs = spec.build();
        let mut gw = PaperWallet::new(inputs.cash);
        Some(fugazi::backtest::run(
            &mut gs,
            &mut gw,
            symbol.clone(),
            candles.iter().map(|(_, c)| *c),
        ))
    } else {
        None
    };
    let mut whole = metrics::from_report(&report, inputs.bars_per_year, inputs.risk_free_rate);
    if costs_active {
        whole.costs = Some(metrics::costs_section(
            &report,
            gross_report.as_ref(),
            inputs.bars_per_year,
        ));
    }
    let gross_metrics = gross_report
        .as_ref()
        .map(|g| metrics::from_report(g, inputs.bars_per_year, inputs.risk_free_rate));
    let (windowed, rolling) = match inputs.windowed {
        Some(n) => {
            let w = metrics::windowed_from_report(
                &report,
                n.get(),
                inputs.bars_per_year,
                inputs.risk_free_rate,
            );
            let r = metrics::rolling_from_report(
                &report,
                n.get(),
                inputs.bars_per_year,
                inputs.risk_free_rate,
            );
            (Some(w), Some(r))
        }
        None => (None, None),
    };
    let bars: Vec<String> = candles.iter().map(|(t, _)| t.clone()).collect();
    let final_equity = report.equity_curve.last().copied().unwrap_or(inputs.cash);
    let summary = SummaryRow {
        final_equity,
        trades: report.fills.len(),
        bars: report.equity_curve.len(),
    };
    IterationResult {
        symbol,
        freq: inputs.effective_freq,
        bars,
        report,
        metrics: whole,
        gross_metrics,
        windowed,
        rolling,
        summary,
        costs_active,
    }
}
