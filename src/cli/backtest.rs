//! Pure per-iteration evaluation. No IO, no console output, no clock.
//!
//! This module owns the "run one backtest, reduce it to a metrics
//! document" work — [`crate::run::run`] wraps it with IO. Writing the
//! results (CSV files, YAML summaries, console banners) is deliberately
//! kept out of here; that's a concern of the `run` subcommand driver, and
//! this module never returns a `Path`, opens a file, or calls `println!`.
//!
//! ## The three pure entry points
//!
//! * [`run_iteration`] — the "full" pure evaluation: drives one backtest
//!   over `atoms` through a paper wallet, produces the whole-run
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

/// Drive `spec` over `atoms` through a fresh paper wallet with `cash`
/// starting funds and the given trading `costs`, returning the full
/// [`fugazi::RunReport`]. The shared core of [`evaluate`] and
/// [`evaluate_windowed`].
fn measured_report(
    spec: &StrategySpec,
    atoms: &[(String, Atom)],
    cash: Real,
    costs: TradingCosts,
) -> fugazi::RunReport<String> {
    let symbol = spec.symbol.clone();
    let schema = schema_from_atoms(atoms);
    let mut strategy = spec.build(&schema);
    let mut wallet = PaperWallet::with_costs(cash, costs);
    fugazi::backtest::run(
        &mut strategy,
        &mut wallet,
        atoms
            .iter()
            .map(|(_, a)| fugazi::types::Snapshot::single(symbol.clone(), a.clone())),
    )
}

/// Extract the shared overlay schema from an atom stream — every atom is built
/// against the same [`Schema`] `Arc` in the loader ([`crate::data`]), so any
/// atom that carries `overlays` gives us it. Falls back to
/// [`Schema::empty()`] when the stream is empty or none of the atoms are
/// overlay-bearing (i.e. no side channel), so a `!get { key }` in the spec
/// panics with a helpful "unknown key" against the empty registered-keys list.
fn schema_from_atoms(atoms: &[(String, Atom)]) -> std::sync::Arc<Schema> {
    atoms
        .iter()
        .find_map(|(_, a)| a.overlays.as_ref())
        .map(|ov| ov.schema().clone())
        .unwrap_or_else(Schema::empty)
}

/// Pure metrics-only evaluation: drive `spec` over `atoms` through a paper
/// wallet with `cash` starting funds, the given `cost_config` resolved for
/// (spec's symbol, `frequency`), and reduce the run to a [`metrics::Metrics`]
/// document. The shape `optimize` calls per grid combination.
#[allow(clippy::too_many_arguments)]
pub fn evaluate(
    spec: &StrategySpec,
    atoms: &[(String, Atom)],
    cash: Real,
    bars_per_year: Real,
    risk_free_rate: Real,
    cost_config: &CostConfig,
    frequency: Option<Frequency>,
    seconds_per_bar: Option<Real>,
) -> metrics::Metrics {
    let costs = cost_config.resolve(&spec.symbol, frequency);
    let measured = measured_report(spec, atoms, cash, costs);
    metrics::from_report(&measured, bars_per_year, risk_free_rate, seconds_per_bar)
}

/// The windowed twin of [`evaluate`]: reduce the same measured run to one
/// [`metrics::Metrics`] per non-overlapping `window`-bar span — what
/// `optimize -w/--windowed` calls per grid combination.
#[allow(clippy::too_many_arguments)]
pub fn evaluate_windowed(
    spec: &StrategySpec,
    atoms: &[(String, Atom)],
    cash: Real,
    bars_per_year: Real,
    risk_free_rate: Real,
    cost_config: &CostConfig,
    frequency: Option<Frequency>,
    window: usize,
    seconds_per_bar: Option<Real>,
) -> Vec<metrics::WindowMetrics> {
    let costs = cost_config.resolve(&spec.symbol, frequency);
    let measured = measured_report(spec, atoms, cash, costs);
    metrics::windowed_from_report(
        &measured,
        window,
        bars_per_year,
        risk_free_rate,
        seconds_per_bar,
    )
}

/// Everything one iteration of a backtest produces — consumed by
/// [`crate::run::run`]. Deliberately owns no IO — the driver decides how
/// (and whether) to persist the payload.
pub struct IterationResult {
    /// One time label per bar, borrowed from the input atoms' time column
    /// and cloned so the result is `Send + 'static`.
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
/// the driver's option struct (see [`crate::run::RunOptions`]) so the
/// pure-work layer doesn't carry `out_dir`, `strategy_label`, etc. — the
/// knobs that only make sense to the IO layer.
pub struct IterationInputs<'a> {
    pub cash: Real,
    pub bars_per_year: Real,
    pub risk_free_rate: Real,
    pub cost_config: &'a CostConfig,
    pub effective_freq: Option<Frequency>,
    pub windowed: Option<NonZeroUsize>,
    /// Trading seconds a bar of `effective_freq` spans on the run's calendar
    /// — populates the `trades.*_seconds` fields on the metrics document.
    /// `None` when the caller doesn't know both the asset class and the bar
    /// cadence; the fields are omitted from the YAML then and stay empty in
    /// the windowed CSV.
    pub seconds_per_bar: Option<Real>,
}

/// The pure-work half of a run: drive the strategy over `atoms`, reduce
/// the report to `Metrics`, and hand back an [`IterationResult`]. Does no
/// IO and no console printing — that's the driver's responsibility.
pub fn run_iteration(
    spec: &StrategySpec,
    atoms: &[(String, Atom)],
    inputs: &IterationInputs,
) -> IterationResult {
    let symbol = spec.symbol.clone();
    let costs = inputs.cost_config.resolve(&symbol, inputs.effective_freq);
    let costs_active = !costs.is_none();
    let schema = schema_from_atoms(atoms);
    let mut strategy = spec.build(&schema);
    let mut wallet = PaperWallet::with_costs(inputs.cash, costs);
    let snapshots = || {
        atoms
            .iter()
            .map(|(_, a)| fugazi::types::Snapshot::single(symbol.clone(), a.clone()))
    };
    let report = fugazi::backtest::run(&mut strategy, &mut wallet, snapshots());
    // Gross twin under active costs: same strategy/atoms/cash, no cost
    // model, so any difference is attributable to costs alone.
    let gross_report = if costs_active {
        let mut gs = spec.build(&schema);
        let mut gw = PaperWallet::new(inputs.cash);
        Some(fugazi::backtest::run(&mut gs, &mut gw, snapshots()))
    } else {
        None
    };
    let mut whole = metrics::from_report(
        &report,
        inputs.bars_per_year,
        inputs.risk_free_rate,
        inputs.seconds_per_bar,
    );
    if costs_active {
        whole.costs = Some(metrics::costs_section(
            &report,
            gross_report.as_ref(),
            inputs.bars_per_year,
        ));
    }
    let gross_metrics = gross_report.as_ref().map(|g| {
        metrics::from_report(
            g,
            inputs.bars_per_year,
            inputs.risk_free_rate,
            inputs.seconds_per_bar,
        )
    });
    let (windowed, rolling) = match inputs.windowed {
        Some(n) => {
            let w = metrics::windowed_from_report(
                &report,
                n.get(),
                inputs.bars_per_year,
                inputs.risk_free_rate,
                inputs.seconds_per_bar,
            );
            let r = metrics::rolling_from_report(
                &report,
                n.get(),
                inputs.bars_per_year,
                inputs.risk_free_rate,
                inputs.seconds_per_bar,
            );
            (Some(w), Some(r))
        }
        None => (None, None),
    };
    let bars: Vec<String> = atoms.iter().map(|(t, _)| t.clone()).collect();
    let final_equity = report.equity_curve.last().copied().unwrap_or(inputs.cash);
    let summary = SummaryRow {
        final_equity,
        trades: report.fills.len(),
        bars: report.equity_curve.len(),
    };
    IterationResult {
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
