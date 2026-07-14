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
use crate::spec::{BasketStrategySpec, PairsStrategySpec, SingleStrategySpec, StrategyRef};

/// Drive `spec` over `atoms` through a fresh paper wallet with `cash`
/// starting funds and the given trading `costs`, returning the full
/// [`fugazi::RunReport`]. The shared core of [`evaluate`] and
/// [`evaluate_windowed`].
fn measured_report(
    spec: &SingleStrategySpec,
    atoms: &[(String, Atom)],
    cash: Real,
    costs: TradingCosts,
) -> fugazi::RunReport<String> {
    let symbol = spec.symbol.clone();
    let schema = schema_from_atoms(atoms);
    let mut strategy = spec.build(cash, &schema);
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
    spec: &SingleStrategySpec,
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
    spec: &SingleStrategySpec,
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
    /// emission in `fills.csv` and gross/net console rows.
    pub costs_active: bool,
}

/// Precomputed inside [`run_iteration`] so IO callers don't reduce the
/// report to these numbers twice.
pub struct SummaryRow {
    pub final_equity: Real,
    /// Count of booked fills (`report.fills.len()`). One per wallet order.
    /// Distinct from the round-trip trade count in
    /// [`metrics::Metrics::trades`]`.total`, which counts closed legs.
    pub fills: usize,
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
    strategy: &StrategyRef,
    atoms: &[(String, Atom)],
    inputs: &IterationInputs,
) -> IterationResult {
    let symbol = strategy.symbol();
    let costs = inputs.cost_config.resolve(symbol, inputs.effective_freq);
    let schema = schema_from_atoms(atoms);
    let bars: Vec<String> = atoms.iter().map(|(t, _)| t.clone()).collect();
    let snapshots: Vec<fugazi::types::Snapshot<String>> = atoms
        .iter()
        .map(|(_, a)| fugazi::types::Snapshot::single(symbol.to_string(), a.clone()))
        .collect();
    run_iteration_core(
        || strategy.build(inputs.cash, &schema),
        &snapshots,
        bars,
        vec![(symbol.to_string(), costs)],
        inputs,
    )
}

/// The pairs twin of [`run_iteration`]. Drives a
/// [`PairsStrategy`](fugazi::strategies::PairsStrategy) over a time-aligned
/// pair of atom streams (both symbols packed into one
/// [`Snapshot<String>`](fugazi::types::Snapshot) per bar).
///
/// `left`/`right` are the parallel atom streams — the caller is responsible
/// for joining them on `time`; each index corresponds to the same time
/// label in `bars`. The wallet is priced for both legs per bar so both
/// mark to market and can produce fills. Cost models are resolved **per
/// leg** and installed as per-symbol overrides on the wallet: the
/// `--costs` spec is asked for `(left, freq)` and `(right, freq)`
/// independently, so a scoped entry like `BTC: 0.001, ETH: 0.0005` prices
/// each side on its own commission / spread / slippage model, and an
/// unscoped default applies uniformly to both. The two symbols are treated
/// symmetrically — the wallet's fallback is zero-cost and every traded leg
/// enters via [`PaperWallet::set_costs_for`], so this shape lifts
/// unchanged to a future N-symbol `BasketStrategy` (loop over basket
/// symbols instead of `[left, right]`).
pub fn run_iteration_pairs(
    spec: &PairsStrategySpec,
    bars: &[String],
    left: &[Atom],
    right: &[Atom],
    inputs: &IterationInputs,
) -> IterationResult {
    assert_eq!(
        left.len(),
        right.len(),
        "pairs run: `left`/`right` streams must be time-aligned"
    );
    assert_eq!(
        bars.len(),
        left.len(),
        "pairs run: `bars` labels must match the aligned stream length"
    );
    let per_symbol_costs = vec![
        (
            spec.left.clone(),
            inputs.cost_config.resolve(&spec.left, inputs.effective_freq),
        ),
        (
            spec.right.clone(),
            inputs.cost_config.resolve(&spec.right, inputs.effective_freq),
        ),
    ];
    let schema = left
        .iter()
        .chain(right.iter())
        .find_map(|a| a.overlays.as_ref())
        .map(|ov| ov.schema().clone())
        .unwrap_or_else(Schema::empty);
    let snapshots: Vec<fugazi::types::Snapshot<String>> = left
        .iter()
        .zip(right.iter())
        .map(|(l, r)| {
            let mut s = fugazi::types::Snapshot::<String>::new();
            s.push(Some(spec.left.clone()), None, l.clone());
            s.push(Some(spec.right.clone()), None, r.clone());
            s
        })
        .collect();
    run_iteration_core(
        || spec.build(inputs.cash, &schema),
        &snapshots,
        bars.to_vec(),
        per_symbol_costs,
        inputs,
    )
}

/// The basket twin of [`run_iteration`] / [`run_iteration_pairs`]. Drives a
/// [`BasketStrategy`](fugazi::strategies::BasketStrategy) over pre-aligned
/// snapshots — each snapshot carries the symbol-tagged atoms for one bar of
/// the shared timeline, ordered by `bars` (each `snapshots[i]` corresponds
/// to `bars[i]`).
///
/// The caller (`crate::run::run_basket`) is responsible for the multi-way
/// time alignment: unioning per-symbol atom streams into a shared bar
/// sequence and packing the atoms present at each bar into a
/// [`Snapshot<String>`](fugazi::types::Snapshot). Symbols that are missing
/// on a bar simply don't appear in that snapshot — the strategy's inner
/// `Pick`s read `None` and the score/sizing chains propagate that up.
///
/// Cost models are resolved **per symbol** and installed as per-symbol
/// overrides on the wallet (via [`PaperWallet::set_costs_for`]), same as
/// the pairs case scaled to N. `universe` names every symbol the strategy
/// could trade — one per-symbol cost bundle is resolved from
/// `inputs.cost_config` for each — and the wallet's fallback is
/// [`TradingCosts::none`], so a symbol the strategy trades that isn't in
/// `universe` fills at zero cost (a minor safety net; the driver passes
/// the full symbol set discovered in the frame).
///
/// [`PaperWallet::set_costs_for`]: fugazi::PaperWallet::set_costs_for
/// [`TradingCosts::none`]: fugazi::TradingCosts::none
pub fn run_iteration_basket(
    spec: &BasketStrategySpec,
    bars: &[String],
    snapshots: &[fugazi::types::Snapshot<String>],
    universe: &[String],
    inputs: &IterationInputs,
) -> IterationResult {
    assert_eq!(
        bars.len(),
        snapshots.len(),
        "basket run: `bars` and `snapshots` must be the same length"
    );
    let per_symbol_costs: Vec<(String, TradingCosts)> = universe
        .iter()
        .map(|s| {
            (
                s.clone(),
                inputs.cost_config.resolve(s, inputs.effective_freq),
            )
        })
        .collect();
    // The shared overlay schema — first atom carrying an OverlayInfo wins;
    // if none of them do, `Schema::empty` (matches the single-asset path).
    let schema = snapshots
        .iter()
        .flat_map(|s| s.iter())
        .find_map(|(_sym, _freq, a)| a.overlays.as_ref())
        .map(|ov| ov.schema().clone())
        .unwrap_or_else(Schema::empty);
    run_iteration_core(
        || spec.build(inputs.cash, &schema),
        snapshots,
        bars.to_vec(),
        per_symbol_costs,
        inputs,
    )
}

/// The shared reduction from "pre-built strategy + snapshot stream" to a
/// full [`IterationResult`]. Runs the strategy through a paper wallet,
/// optionally re-runs it against a zero-cost twin for the gross diff, and
/// assembles the whole-run / windowed / rolling metrics.
///
/// `per_symbol_costs` is the flat list of per-symbol cost bundles the
/// wallet is primed with — installed via
/// [`PaperWallet::set_costs_for`](fugazi::PaperWallet::set_costs_for) so
/// each traded symbol carries its own commission / spread / slippage
/// pipeline. The wallet's fallback is always [`TradingCosts::none`], so a
/// symbol the strategy trades that isn't in the list fills at zero cost.
/// Single-asset callers pass one entry; pairs pass two; a future N-symbol
/// basket driver passes N — no other reshuffling needed. The gross twin
/// (built only when the priced run has non-none costs) uses a plain
/// zero-cost wallet.
///
/// `build_strategy` is called at most twice (once for the priced run, once
/// for the gross twin when costs are active), so any per-leaf state a spec
/// carries reads freshly on each call.
fn run_iteration_core<S>(
    mut build_strategy: impl FnMut() -> S,
    snapshots: &[fugazi::types::Snapshot<String>],
    bars: Vec<String>,
    per_symbol_costs: Vec<(String, TradingCosts)>,
    inputs: &IterationInputs,
) -> IterationResult
where
    S: fugazi::Strategy<Input = fugazi::types::Snapshot<String>, Symbol = String>,
{
    let costs_active = per_symbol_costs.iter().any(|(_, c)| !c.is_none());
    let mut strategy = build_strategy();
    let mut wallet = PaperWallet::new(inputs.cash);
    for (sym, c) in per_symbol_costs {
        wallet.set_costs_for(sym, c);
    }
    let report = fugazi::backtest::run(&mut strategy, &mut wallet, snapshots.iter().cloned());
    // Gross twin under active costs: same strategy/snapshots/cash, no cost
    // model, so any difference is attributable to costs alone.
    let gross_report = if costs_active {
        let mut gs = build_strategy();
        let mut gw = PaperWallet::new(inputs.cash);
        Some(fugazi::backtest::run(
            &mut gs,
            &mut gw,
            snapshots.iter().cloned(),
        ))
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
    }
}
