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

use crate::spec::calendar::Frequency;
use crate::spec::costs::CostConfig;
use crate::spec::metrics;
use crate::spec::runnable::{RunnableStrategyExt, StrategySpec};
use crate::types::Symbol;

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
    snapshots: &[crate::types::Snapshot<Symbol>],
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
pub fn universe_from_snapshots(snapshots: &[crate::types::Snapshot<Symbol>]) -> Vec<Symbol> {
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

/// Refuse a run whose document names a symbol the snapshot stream never
/// carries — the typo / wrong-dataset / case-mismatch case.
///
/// **Why this is not left to the bars.** A leaf that resolves nothing reads
/// `None`, which is the right answer for a listing gap and exactly the wrong
/// one for a symbol that was never supplied: no signal ever fires, no order is
/// ever sized, and the run completes with zero fills and a full set of
/// metrics. That reads as "the strategy didn't like this period" rather than
/// "the strategy never saw its asset" — the most expensive way for a backtest
/// to be wrong. This is the same argument, and the same disposition, as
/// `cli::run::read_only_series` applies to a `!pick`-named series; the CLI has
/// refused that since it existed. This closes the twin hole for the symbol the
/// document *trades*, on the entry points where the caller builds the
/// snapshots themselves.
///
/// **Absent from the stream, not absent from a bar.** The check is over the
/// whole stream, so a symbol that quotes on even one bar passes. A shorter
/// history, a delisting, a holiday or a half-day is ordinary and must not fail
/// — that case is handled one layer down, where the strategy reads `None` and
/// does not advance (see `strategies::single_asset::extract_self_atom`).
///
/// **What is not checked.** Basket and multi-asset *discover* their universe
/// from the stream, so they declare nothing and cannot disagree with it; see
/// [`StrategySpec::declared_symbols`]. Callers must also skip this on a
/// **resumed** run — a chunk in which a symbol never quotes is legitimate when
/// the state carrying it came from an earlier chunk — and on a **live** feed,
/// where there is no stream to scan. Both are the caller's call because only
/// the caller knows; the batch entry points here make it for them.
pub fn validate_universe(
    spec: &StrategySpec,
    snapshots: &[crate::types::Snapshot<Symbol>],
) -> Result<(), String> {
    let declared = spec.declared_symbols();
    if declared.is_empty() {
        return Ok(());
    }
    let present = universe_from_snapshots(snapshots);
    let missing: Vec<&String> = declared
        .iter()
        .filter(|d| !present.iter().any(|p| p.as_ref() == d.as_str()))
        .collect();
    if missing.is_empty() {
        return Ok(());
    }
    let names = missing
        .iter()
        .map(|s| format!("`{s}`"))
        .collect::<Vec<_>>()
        .join(", ");
    let carried = if present.is_empty() {
        "no symbols at all".to_string()
    } else {
        present
            .iter()
            .map(|s| format!("`{s}`"))
            .collect::<Vec<_>>()
            .join(", ")
    };
    let (subject, verb) = if missing.len() == 1 {
        ("symbol", "is")
    } else {
        ("symbols", "are")
    };
    Err(format!(
        "the document trades {subject} {names}, which {verb} not in the input at all — \
         the stream carries {carried}.\n\
         \n\
         Nothing would ever resolve for {names}, so the run would report a full set of \
         metrics over zero fills rather than an error. Check for a typo or a case \
         mismatch against the series you passed, and note the match is exact.\n\
         \n\
         A symbol that is merely absent from *some* bars — a shorter history, a \
         delisting, a holiday — is fine and does not reach this: the strategy reads \
         `None` on those bars and does not advance."
    ))
}

/// Everything one iteration of a backtest produces — consumed by
/// `crate::run::run`. Deliberately owns no IO — the driver decides how
/// (and whether) to persist the payload.
pub struct IterationResult {
    /// One time label per bar, borrowed from the input atoms' time column
    /// and cloned so the result is `Send + 'static`.
    pub bars: Vec<String>,
    /// The priced (net) run report from `crate::backtest::run`.
    pub report: crate::RunReport<Symbol>,
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
    /// The most gross notional the run's account may hold, as a multiple of
    /// equity — `1.0` (unlevered) unless the caller says otherwise, and handed
    /// straight to [`PaperWallet::with_max_gross`].
    ///
    /// Part of the *account*, like `cash`, rather than of the strategy: it
    /// bounds what any document can carry rather than changing what one asks
    /// for. A `sizing:` above it is fitted to it and the gap recorded on
    /// [`Order::requested_units`](crate::Order::requested_units).
    pub max_gross: Real,
    /// Annualized interest charged on a **negative** cash balance —
    /// [`PaperWallet::with_margin_rate`]. `0.0` charges nothing, which is the
    /// only honest default: a rate is a fact about a broker, not about a run.
    pub margin_rate: Real,
    /// Equity/gross ratio below which the run's account is force-closed, or
    /// `None` for no margin call — [`PaperWallet::with_maintenance_margin`].
    pub maintenance_margin: Option<Real>,
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
    /// Bars fed to the strategy *before* the evaluated range, purely to warm
    /// its chains — the prefix `--from` reads back from the series so a sliced
    /// run's first evaluated bar is measured on settled indicators. Trading is
    /// gated off across it, so it contributes no fills and no equity.
    ///
    /// Echoed into `metrics.yml` as `run.warmup_bars`, and consumed by
    /// [`run_iteration_resumable`] to split the snapshot stream. `None` means
    /// no prefix — every bar handed over is evaluated, which is what an
    /// unsliced run does.
    pub warmup_bars: Option<usize>,
}

impl EvalContext<'_> {
    /// The [`PaperWallet`] this run trades: seeded with `cash`, capped at
    /// `max_gross`, and primed with `per_symbol_costs`.
    ///
    /// One place, so the priced run and its zero-cost twin cannot end up on
    /// differently-configured accounts.
    pub fn account(&self, per_symbol_costs: &[(String, TradingCosts)]) -> PaperWallet<Symbol> {
        let mut wallet = PaperWallet::new(self.cash)
            .with_max_gross(self.max_gross)
            .with_margin_rate(self.margin_rate);
        // Only when the cadence resolved. A time-denominated carry model charges
        // nothing without it — see `with_bar_year_fraction` — which the CLI
        // warns about rather than papering over with an assumed year length.
        if let Some(freq) = self.effective_freq {
            wallet = wallet.with_bar_frequency(freq);
        }
        if let Some(ratio) = self.maintenance_margin {
            wallet = wallet.with_maintenance_margin(ratio);
        }
        for (sym, costs) in per_symbol_costs {
            let _ = wallet.set_costs_for(crate::types::symbol(sym), costs.clone());
        }
        wallet
    }

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
    pub fn reduce(&self, report: &crate::RunReport<Symbol>) -> metrics::Metrics {
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
        report: &crate::RunReport<Symbol>,
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
    snapshots: &[crate::types::Snapshot<Symbol>],
    ctx: &EvalContext,
) -> Result<crate::RunReport<Symbol>, String> {
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
    let mut wallet = ctx.account(&per_symbol_costs);
    let (report, _) = built.drive_warmed_over(
        snapshots,
        ctx.warmup_bars.unwrap_or(0),
        &mut wallet,
        None,
        false,
    )?;
    Ok(report)
}

/// Reduce a whole-run backtest of `spec` to one [`metrics::Metrics`] document
/// — what `optimize` calls per grid combination, for every shape.
pub fn evaluate_any(
    spec: &StrategySpec,
    snapshots: &[crate::types::Snapshot<Symbol>],
    ctx: &EvalContext,
) -> Result<metrics::Metrics, String> {
    Ok(ctx.reduce(&measured_report_any(spec, snapshots, ctx)?))
}

/// The windowed twin of [`evaluate_any`]: one document per non-overlapping
/// `window`-bar span.
pub fn evaluate_windowed_any(
    spec: &StrategySpec,
    snapshots: &[crate::types::Snapshot<Symbol>],
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
    snapshots: &[crate::types::Snapshot<Symbol>],
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
    snapshots: &[crate::types::Snapshot<Symbol>],
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
    // Refuse a declared symbol the stream never carries, once, before any bar
    // is driven — see `validate_universe`. Cold starts only: on a resume, a
    // chunk in which a symbol never quotes is legitimate, because the state
    // that carries it was restored from an earlier one.
    if resume.is_none() {
        validate_universe(spec, snapshots)?;
    }
    // The warm-up prefix is fed to the strategy but not measured, so it is
    // charged against `bars` here — everything downstream (the equity curve,
    // `returns.csv`, the windowed reductions, the stamped period) is indexed
    // off the evaluated range alone.
    let warmup = ctx.warmup_bars.unwrap_or(0).min(snapshots.len());
    let bars: Vec<String> = bars[warmup..].to_vec();

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
    let mut wallet = ctx.account(&per_symbol_costs);
    let (report, final_state) =
        priced.drive_warmed_over(snapshots, warmup, &mut wallet, resume, flatten)?;

    let gross_report = if costs_active {
        let mut gross = spec.try_build(ctx.cash, &schema, None)?;
        // The zero-cost twin has to carry the *same* leverage cap, or the two
        // curves would differ by more than the costs they were run to isolate.
        let mut gross_wallet = ctx.account(&[]);
        Some(
            gross
                .drive_warmed_over(snapshots, warmup, &mut gross_wallet, None, flatten)?
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
    report: crate::RunReport<Symbol>,
    gross_report: Option<crate::RunReport<Symbol>>,
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
    // `bars` labels the *evaluated* range (the caller split any warm-up prefix
    // off both streams together), so it names the period end to end.
    metrics::stamp_period(&mut whole, &bars, inputs.warmup_bars);
    if costs_active {
        whole.costs = Some(metrics::costs_section(
            &report,
            gross_report.as_ref(),
            inputs.bars_per_year,
        ));
    }
    let gross_metrics = gross_report.as_ref().map(|g| {
        let mut m = metrics::from_report(
            g,
            inputs.bars_per_year,
            inputs.risk_free_rate,
            inputs.seconds_per_bar,
        );
        // The gross twin is the same bars with the cost model removed, so it
        // covers the same period.
        metrics::stamp_period(&mut m, &bars, inputs.warmup_bars);
        m
    });
    // Each window is a slice of the *evaluated* range, so it carries its own
    // period but no warm-up of its own — the prefix was consumed once, before
    // the first window began.
    let stamp_windows = |ws: &mut Vec<metrics::WindowMetrics>| {
        for w in ws.iter_mut() {
            let span = bars.get(w.start_bar..=w.end_bar).unwrap_or(&[]);
            metrics::stamp_period(&mut w.metrics, span, None);
        }
    };
    let (windowed, rolling) = match inputs.windowed {
        Some(n) => {
            let mut w = metrics::windowed_from_report(
                &report,
                n.get(),
                inputs.bars_per_year,
                inputs.risk_free_rate,
                inputs.seconds_per_bar,
            );
            let mut r = metrics::rolling_from_report(
                &report,
                n.get(),
                inputs.bars_per_year,
                inputs.risk_free_rate,
                inputs.seconds_per_bar,
            );
            stamp_windows(&mut w);
            stamp_windows(&mut r);
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
    snapshots: &[crate::types::Snapshot<Symbol>],
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
