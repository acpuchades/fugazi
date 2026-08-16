//! Pure per-iteration evaluation. No IO, no console output, no clock.
//!
//! This module owns the "run one backtest, reduce it to a metrics
//! document" work — `crate::run::run` wraps it with IO. Writing the
//! results (CSV files, YAML summaries, console banners) is deliberately
//! kept out of here; that's a concern of the `run` subcommand driver, and
//! this module never returns a `Path`, opens a file, or calls `println!`.
//!
//! ## The three pure entry points
//!
//! * `run_iteration` — the "full" pure evaluation: drives one backtest
//!   over `atoms` through a paper wallet, produces the whole-run
//!   [`metrics::Metrics`] document, optionally the gross twin under active
//!   costs, and (when `-w N` is set) the windowed + rolling reductions.
//!   Returns everything the driver needs to write files and print
//!   summaries via the [`IterationResult`] payload.
//! * `evaluate` — a thin metrics-only wrapper for `optimize`'s grid loop.
//! * `evaluate_windowed` — the same shape but with a windowed reduction.
//!
//! ## Warm-up and stability
//!
//! Metrics cover the whole run — the strategy layer is opinion-free about
//! stability. A strategy that wants entries held off until every source it
//! consults has settled composes the check at the entry with `!stable`, i.e.
//! `!all [<entry>, !stable { signal: <entry> }]`.

use std::num::NonZeroUsize;

use crate::prelude::*;

use crate::spec::runnable::StrategySpec;
use crate::spec::calendar::Frequency;
use crate::spec::costs::CostConfig;
use crate::spec::metrics;


/// Build a strategy once and discard it, turning a malformed document into an
/// error before the run machinery — which builds through the infallible `build`
/// shim — ever touches it.
///
/// Pass the shape's own `try_build`:
///
/// ```ignore
/// let schema = schema_from_atoms(&atoms);
/// validated(|| strategy.try_build(cash, &schema))?;
/// ```
///
/// Every driver here already builds its strategy at least twice (the priced run
/// and the zero-cost gross twin), so one more construction costs nothing next to
/// the backtest itself — and it buys a clean diagnostic in place of a panic.
/// This is the same "validate once, then trust" shape the per-symbol factories
/// in [`basket`](crate::spec::basket) and
/// [`multi_asset`](crate::spec::multi_asset) use.
///
/// The `!tag > ` breadcrumb is split off onto its own `at:` line, matching how
/// the CLI renders a parse error.
pub fn validated<T>(build: impl FnOnce() -> std::result::Result<T, String>) -> anyhow::Result<()> {
    build().map(|_| ()).map_err(build_error)
}

/// Render a `try_build` failure as an [`anyhow::Error`], splitting the crate's
/// `!tag > ` breadcrumb off onto its own `at:` line — the same shape the CLI
/// uses for a parse error, so the two are indistinguishable to whoever reads
/// them.
///
/// Use directly (`.map_err(build_error)?`) where the built value is wanted;
/// [`validated`] is the discard-the-value convenience over it.
pub fn build_error(e: String) -> anyhow::Error {
    let (trail, message) = crate::spec::diagnostics::split_trail(&e);
    if trail.is_empty() {
        anyhow::anyhow!("{message}")
    } else {
        anyhow::anyhow!("{message}\n  at: {}", trail.join(" > "))
    }
}

/// Extract the shared overlay schema from an atom stream — every atom is built
/// against the same [`Schema`] `Arc` in the loader (`crate::data`), so any
/// atom that carries `overlays` gives us it. Falls back to
/// [`Schema::empty()`] when the stream is empty or none of the atoms are
/// overlay-bearing (i.e. no side channel), so a `!get { key }` in the spec
/// panics with a helpful "unknown key" against the empty registered-keys list.
pub fn schema_from_atoms(atoms: &[(String, Atom)]) -> std::sync::Arc<Schema> {
    atoms
        .iter()
        .find_map(|(_, a)| a.overlays.as_ref())
        .map(|ov| ov.schema().clone())
        .unwrap_or_else(Schema::empty)
}

/// The snapshot-stream twin of [`schema_from_atoms`] — walks every atom
/// across every snapshot's tagged entries and returns the first shared
/// overlay [`Schema`] `Arc`. Falls back to [`Schema::empty()`] under the
/// same conditions.
pub fn schema_from_snapshots(
    snapshots: &[crate::types::Snapshot<String>],
) -> std::sync::Arc<Schema> {
    snapshots
        .iter()
        .flat_map(|s| s.iter())
        .find_map(|(_sym, _freq, a)| a.overlays.as_ref())
        .map(|ov| ov.schema().clone())
        .unwrap_or_else(Schema::empty)
}



/// Discover the tradeable universe from a snapshot stream — the distinct
/// symbols carried across every bar, sorted so the resulting per-symbol
/// cost install order is deterministic.
pub fn universe_from_snapshots(snapshots: &[crate::types::Snapshot<String>]) -> Vec<String> {
    let mut set = std::collections::HashSet::new();
    for snap in snapshots {
        for (sym, _, _) in snap.iter() {
            if let Some(s) = sym {
                set.insert(s.clone());
            }
        }
    }
    let mut v: Vec<_> = set.into_iter().collect();
    v.sort();
    v
}

/// Everything one iteration of a backtest produces — consumed by
/// `crate::run::run`. Deliberately owns no IO — the driver decides how
/// (and whether) to persist the payload.
pub struct IterationResult {
    /// One time label per bar, borrowed from the input atoms' time column
    /// and cloned so the result is `Send + 'static`.
    pub bars: Vec<String>,
    /// The priced (net) run report from `crate::backtest::run`.
    pub report: crate::RunReport<String>,
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
    /// emission in `fills.csv` and gross/net console rows.
    pub costs_active: bool,
    /// Per-resample Monte Carlo values, when `EvalContext::mc` was set (and the
    /// `montecarlo` feature is on). The CLI writes these to `montecarlo.csv`;
    /// the summary lands in `metrics.montecarlo`.
    pub mc_samples: Option<crate::spec::montecarlo::McSamples>,
}

/// Precomputed inside `run_iteration` so IO callers don't reduce the
/// report to these numbers twice.
pub struct SummaryRow {
    pub final_equity: Real,
    /// Count of booked fills (`report.fills.len()`). One per wallet order.
    /// Distinct from the round-trip trade count in
    /// [`metrics::Metrics::trades`]`.total`, which counts closed legs.
    pub fills: usize,
    pub bars: usize,
}

/// The resolved-once inputs every measurement in this module consumes —
/// `evaluate` and its per-shape twins, and `run_iteration` and its.
///
/// Kept separate from the driver's option struct (see
/// `crate::run::RunOptions`) so the pure-work layer doesn't carry
/// `out_dir`, `strategy_label`, etc. — the knobs that only make sense to the
/// IO layer.
///
/// The fields group into three concerns, which is also how the methods below
/// divide: **execution** (`cash`, `cost_config`, `effective_freq`) seeds the
/// wallet, **measurement** (`bars_per_year`, `risk_free_rate`,
/// `seconds_per_bar`) reduces a report to metrics, and `windowed` picks the
/// reduction. They travel together through every entry point here, which is
/// why they're one struct rather than the seven positional arguments this
/// used to be.
pub struct EvalContext<'a> {
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
    /// Monte Carlo significance analysis to run *after* the backtest: when
    /// `Some`, [`run_iteration_resumable`] resamples the run and attaches a
    /// `montecarlo:` block to the metrics document (plus the raw samples on
    /// [`IterationResult`]). `None` skips it — the default for `optimize`,
    /// where per-grid-cell resampling would be pathological. Requires the
    /// `montecarlo` feature to actually compute; ignored without it.
    pub mc: Option<crate::spec::montecarlo::McConfig>,
}

impl EvalContext<'_> {
    /// Resolve one symbol's [`TradingCosts`] at this run's cadence.
    pub fn costs_for_one(&self, symbol: &str) -> TradingCosts {
        self.cost_config.resolve(symbol, self.effective_freq)
    }

    /// Resolve a per-symbol cost bundle for each of `symbols` — the shape
    /// `measured_report_from_strategy` primes its wallet with. Pairs pass
    /// their two legs, basket / multi their whole universe.
    pub fn costs_for<S: AsRef<str>>(
        &self,
        symbols: impl IntoIterator<Item = S>,
    ) -> Vec<(String, TradingCosts)> {
        symbols
            .into_iter()
            .map(|s| {
                let s = s.as_ref();
                (s.to_string(), self.costs_for_one(s))
            })
            .collect()
    }

    /// Reduce a whole run to one [`metrics::Metrics`] document.
    pub fn reduce(&self, report: &crate::RunReport<String>) -> metrics::Metrics {
        metrics::from_report(
            report,
            self.bars_per_year,
            self.risk_free_rate,
            self.seconds_per_bar,
        )
    }

    /// Reduce a run to one [`metrics::Metrics`] per non-overlapping
    /// `window`-bar span.
    pub fn reduce_windowed(
        &self,
        report: &crate::RunReport<String>,
        window: usize,
    ) -> Vec<metrics::WindowMetrics> {
        metrics::windowed_from_report(
            report,
            window,
            self.bars_per_year,
            self.risk_free_rate,
            self.seconds_per_bar,
        )
    }
}

/// Whole-run measurement for any strategy shape: build it with this run's
/// costs applied the way its shape needs them, drive it, and return the
/// report.
///
/// One function for all five shapes — the wallet difference lives on
/// [`RunnableStrategy::drive`](crate::spec::runnable::RunnableStrategy::drive), and the cost-application difference on
/// [`StrategySpec::try_build_priced`].
pub fn measured_report_any(
    spec: &StrategySpec,
    snapshots: &[crate::types::Snapshot<String>],
    ctx: &EvalContext,
) -> Result<crate::RunReport<String>, String> {
    let schema = schema_from_snapshots(snapshots);
    let universe = spec.universe(snapshots);
    let per_symbol_costs = ctx.costs_for(&universe);
    let mut built = spec.try_build_priced(
        ctx.cash,
        &schema,
        ctx.cost_config,
        ctx.effective_freq,
        &universe,
    )?;
    Ok(built.drive(snapshots, ctx.cash, &per_symbol_costs))
}

/// Reduce a whole-run backtest of `spec` to one [`metrics::Metrics`] document
/// — what `optimize` calls per grid combination, for every shape.
pub fn evaluate_any(
    spec: &StrategySpec,
    snapshots: &[crate::types::Snapshot<String>],
    ctx: &EvalContext,
) -> Result<metrics::Metrics, String> {
    Ok(ctx.reduce(&measured_report_any(spec, snapshots, ctx)?))
}

/// The windowed twin of [`evaluate_any`]: one document per non-overlapping
/// `window`-bar span.
pub fn evaluate_windowed_any(
    spec: &StrategySpec,
    snapshots: &[crate::types::Snapshot<String>],
    ctx: &EvalContext,
    window: usize,
) -> Result<Vec<metrics::WindowMetrics>, String> {
    Ok(ctx.reduce_windowed(&measured_report_any(spec, snapshots, ctx)?, window))
}

/// The pure-work half of a run, for any shape: drive the strategy over
/// `snapshots`, reduce the report to `Metrics`, and hand back an
/// [`IterationResult`]. Does no IO and no console printing — that's the
/// driver's responsibility.
///
/// Under active costs the strategy is built and driven a second time with no
/// cost model at all, so the difference between the two reports is
/// attributable to costs alone.
pub fn run_iteration_any(
    spec: &StrategySpec,
    bars: Vec<String>,
    snapshots: &[crate::types::Snapshot<String>],
    ctx: &EvalContext,
) -> Result<IterationResult, String> {
    Ok(run_iteration_resumable(spec, bars, snapshots, ctx, None, false)?.0)
}

/// The resumable superset of [`run_iteration_any`]: optionally restore `resume`
/// state before the priced run, optionally finalize open positions with
/// `flatten`, and surface the run's final [`RunState`](crate::spec::runnable::RunState) alongside the
/// metrics so the CLI can persist it (`--save-state`).
///
/// The zero-cost gross twin is never *resumed* — it is a costs-attribution
/// shadow of the priced run, not a run in its own right — but it is flattened
/// alongside it. `costs_section` pairs net fills against gross fills, so a
/// flatten leg with no gross counterpart would contribute nothing to
/// `total_slippage_cost` and silently understate the drag, while `cost_drag_pct`
/// compared a flattened curve against an unflattened one.
pub fn run_iteration_resumable(
    spec: &StrategySpec,
    bars: Vec<String>,
    snapshots: &[crate::types::Snapshot<String>],
    ctx: &EvalContext,
    resume: Option<&crate::spec::runnable::RunState>,
    flatten: bool,
) -> Result<(IterationResult, crate::spec::runnable::RunState), String> {
    assert_eq!(
        bars.len(),
        snapshots.len(),
        "run: `bars` labels must match the snapshot stream length"
    );
    if resume.is_some_and(|r| r.kind != spec.kind()) {
        return Err(format!(
            "!resume > state is for a `{}` strategy but this document is `{}`",
            resume.map(|r| r.kind.as_str()).unwrap_or(""),
            spec.kind()
        ));
    }
    let schema = schema_from_snapshots(snapshots);
    let universe = spec.universe(snapshots);
    let per_symbol_costs = ctx.costs_for(&universe);
    // Costs are active when the unscoped default is non-empty *or* any
    // per-symbol scoped bundle is. The default matters on its own for a
    // portfolio, where it is the sub-wallet fallback rather than something
    // primed per symbol.
    let costs_active = !ctx.cost_config.resolve("", ctx.effective_freq).is_none()
        || per_symbol_costs.iter().any(|(_, c)| !c.is_none());

    let mut priced = spec.try_build_priced(
        ctx.cash,
        &schema,
        ctx.cost_config,
        ctx.effective_freq,
        &universe,
    )?;
    let (report, final_state) =
        priced.drive_resumable(snapshots, ctx.cash, &per_symbol_costs, resume, flatten)?;

    let gross_report = if costs_active {
        let mut gross = spec.try_build(ctx.cash, &schema, None)?;
        Some(
            gross
                .drive_resumable(snapshots, ctx.cash, &[], None, flatten)?
                .0,
        )
    } else {
        None
    };

    let iter = reduce_iteration(report, gross_report, bars, costs_active, ctx);
    #[cfg(feature = "montecarlo")]
    let iter = attach_montecarlo(iter, spec, snapshots, ctx)?;
    Ok((iter, final_state))
}

/// Reduce a priced run (plus its zero-cost twin, when costs were active) to a
/// full [`IterationResult`].
///
/// The half of an iteration that has nothing to do with which strategy shape
/// produced the reports: whole-run metrics, the costs section, and the
/// windowed / rolling reductions under `-w`.
fn reduce_iteration(
    report: crate::RunReport<String>,
    gross_report: Option<crate::RunReport<String>>,
    bars: Vec<String>,
    costs_active: bool,
    inputs: &EvalContext,
) -> IterationResult {
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
    let final_equity = report.equity_curve.last().copied().unwrap_or(inputs.cash);
    let summary = SummaryRow {
        final_equity,
        fills: report.fills.len(),
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
        mc_samples: None,
    }
}

/// Run the Monte Carlo analysis (`EvalContext::mc`) over a completed iteration
/// and fold its summary + samples onto the result. Only compiled with the
/// `montecarlo` feature; a no-op when `ctx.mc` is `None`.
#[cfg(feature = "montecarlo")]
fn attach_montecarlo(
    mut iter: IterationResult,
    spec: &StrategySpec,
    snapshots: &[crate::types::Snapshot<String>],
    ctx: &EvalContext,
) -> Result<IterationResult, String> {
    if let Some(config) = &ctx.mc {
        let outcome =
            crate::spec::montecarlo::run_montecarlo(spec, snapshots, ctx, &iter.report, config)?;
        iter.metrics.montecarlo = Some(outcome.section);
        iter.mc_samples = Some(outcome.samples);
    }
    Ok(iter)
}
