//! The `run` subcommand's IO driver.
//!
//! Owns everything user-facing: file writes (`fills.csv`, `trades.csv`,
//! `returns.csv`, `metrics.yml`, and the optional `metrics.csv`/`rolling.csv`
//! under `-w N`), the tiered console banners (**inputs** / **fills** /
//! **result** / **metrics**), and the wall-clock timing. Evaluation is
//! delegated to [`crate::backtest::run_iteration`] — this module never touches
//! the per-bar loop or the metrics reduction itself; it just wraps the pure
//! payload with IO.
//!
//! ## Output shape
//!
//! Per bar: feed the wallet the candle (in [`run_iteration`]); the priced
//! blotter comes back sorted by fill index. Every order is written to
//! `fills.csv` with its bar's `time` and its own fill price — the per-order
//! log of what the wallet actually booked. Closed round-trip legs are
//! reduced from the same blotter by [`fugazi::metrics::reconstruct_trades`]
//! and written to `trades.csv`: one row per closed leg with entry/exit bar,
//! side, units, entry/exit price, realized `pnl`, `return`, and
//! `bars_held`. A buy-and-hold strategy (nothing ever closes) produces a
//! header-only `trades.csv` — matches the metrics layer, which also only
//! counts closed legs.
//!
//! The running equity is emitted to `returns.csv`. Every CSV is
//! `,`-delimited. After the loop the equity curve + blotter reduce to
//! `metrics.yml` (whole-run summary — see [`crate::metrics`]) and, under
//! `-w N`, to `metrics.csv` (non-overlapping N-bar windows) and
//! `rolling.csv` (rolling stride-1 windows). The console prints the
//! whole-run headline block first; under `-w` a second **windowed metrics**
//! block follows it, showing `mean ± std` across the non-overlapping
//! windows for the same headline stats — so the caller sees both the
//! whole-run point estimate and its cross-window dispersion side-by-side.
//!
//! Metrics cover the whole run — the strategy layer is opinion-free about
//! stability. A strategy that wants entries held off until every source it
//! consults has settled composes the check at the entry with `!stable`, i.e.
//! `!all [<entry>, !stable { signal: <entry> }]`.

use fugazi::types::Symbol;
use std::collections::BTreeSet;
use std::path::Path;
use std::str::FromStr;
use std::time::SystemTime;

use anyhow::{Context, Result};
use fugazi::prelude::*;

use crate::backtest::{self, EvalContext, IterationResult};
use crate::calendar::{self, AssetClass, BarsPerYearSpec, ScopedFrequency, WindowSpec};
use crate::costs::CostConfig;
use crate::data::DataFrame;
use crate::daterange::{self, Slice};
use crate::metrics;
use crate::overlap::{self, Overlap};
use crate::spec::{
    BasketStrategySpec, MultiAssetStrategySpec, PairsStrategySpec, PortfolioSpec, StrategyRef,
};
use crate::style;
use fugazi::spec::StrategySpec;

/// Console-logging knobs plus the run's inputs, threaded in from the CLI args.
/// Held by the `run` subcommand's driver; never enters [`crate::backtest`],
/// which stays IO-free.
pub struct RunOptions<'a> {
    /// Initial cash for the paper wallet.
    pub cash: Real,
    /// Most gross notional the account may hold, as a multiple of equity
    /// (`--max-gross`; `1.0` is unlevered). Handed to the run's
    /// [`PaperWallet`](fugazi::PaperWallet) via
    /// [`EvalContext::max_gross`](fugazi::spec::backtest::EvalContext).
    pub max_gross: Real,
    /// Annualized interest on a negative cash balance (`--margin-rate`).
    pub margin_rate: Real,
    /// Maintenance-margin ratio, or `None` for no margin call
    /// (`--maintenance-margin`).
    pub maintenance_margin: Option<Real>,
    /// Directory to write `fills.csv` / `trades.csv` / `returns.csv` into.
    pub out_dir: &'a Path,
    /// A short label for the strategy source (file path or `(inline)`), echoed
    /// in the run block.
    pub strategy_label: &'a str,
    /// A one-line view of the effective params (`NAME=value, …`), echoed in
    /// the run block.
    pub params: &'a str,
    /// `--bars-per-year` entries: each is a plain `N` or a `SYMBOL[FREQ]:N`
    /// override. Resolved per iteration via
    /// [`crate::calendar::pick_bars_per_year`].
    pub bars_per_year: &'a [BarsPerYearSpec],
    /// Trading-calendar shortcut (`--stocks`/`--forex`/`--crypto`). `None`
    /// falls back to [`AssetClass::Stocks`].
    pub asset_class: Option<AssetClass>,
    /// Annualized risk-free rate as a fraction (e.g. `0.045` = 4.5% p.a.).
    pub risk_free_rate: Real,
    /// When set, also emit windowed reductions at this window length: one row
    /// per non-overlapping window in `metrics.csv`, one row per rolling
    /// (stride-1) window in `rolling.csv`. `metrics.yml` (whole-run) is
    /// always written; `None` skips the CSVs. The raw CLI spec — a bar count
    /// or a duration; resolved to a bar count against the trading calendar
    /// inside [`run`]. The duration form requires `asset_class` and a
    /// resolvable bar cadence (`frequency` or auto-detection from
    /// `Atom::time`).
    pub windowed: Option<WindowSpec>,
    /// Configured cost models, resolved into a live [`TradingCosts`] per
    /// (symbol, frequency) at run time. See [`crate::costs`].
    pub cost_config: &'a CostConfig,
    /// `-f/--frequency` entries: plain `CODE` or `SYMBOL:CODE`. Resolved per
    /// iteration via [`crate::calendar::pick_frequency`]; falls through to
    /// detection when no entry matches.
    pub frequency: &'a [ScopedFrequency],
    /// Whether the user passed at least one `--costs` flag (even `--costs
    /// none`). Governs the "no cost model set" warning banner.
    pub costs_supplied: bool,
    /// Suppress all console output (the result files are still written).
    pub quiet: bool,
    /// `--resume`: restore this state before the run (loaded from the file in
    /// `main`). `None` for a cold start.
    pub resume: Option<&'a fugazi::spec::RunState>,
    /// `--save-state`: write the run's final state to this path afterwards.
    pub save_state: Option<&'a Path>,
    /// `--flatten`: finalize open positions into the trade blotter at the
    /// last bar (mutually exclusive with `save_state`).
    pub flatten: bool,
    /// `--montecarlo`: when set, run the significance analysis after the
    /// backtest and attach a `montecarlo:` block to `metrics.yml` plus a
    /// `montecarlo.csv` of the per-resample values. `None` skips it entirely.
    pub montecarlo: Option<&'a fugazi::spec::montecarlo::McConfig>,
    /// `--from` / `--until` / `--strict-from`: which bars this run evaluates.
    /// `None` when neither bound was given, which takes every code path here
    /// back to evaluating the series end to end. See [`crate::daterange`].
    pub range: Option<daterange::DateRange>,
    /// The `--from` value as the user spelled it, for error and warning text
    /// that has to quote the flag back.
    pub from_label: Option<&'a str>,
    /// Symbols the document **reads but does not trade** — every asset named by
    /// an explicit `!pick { symbol: … }` anywhere in the tree, collected at load
    /// by [`fugazi::spec::reads::picked_symbols_of`].
    ///
    /// The runners join these series into the snapshot stream alongside the
    /// traded ones, so a regime gate on another asset resolves, and refuse the
    /// run when one of them is absent from `--series`. Empty for a document
    /// that names none, which is every document that worked before this
    /// existed.
    pub reads: &'a BTreeSet<String>,
}

/// Headline numbers returned from a run.
pub struct Summary {
    pub final_equity: Real,
    pub return_pct: Real,
    /// Number of booked fills. Distinct from the round-trip trade count in
    /// [`crate::metrics::Metrics::trades`]`.total`, which counts closed legs.
    pub fills: usize,
    pub bars: usize,
}

/// Drive one iteration, honoring `--resume` / `--save-state` / `--flatten`.
///
/// When any of those is set it goes through the resumable path (restoring first,
/// finalizing open positions if asked, and writing the run's final state to
/// `--save-state`); otherwise it is the plain cold-start iteration. Shared by
/// every shape's runner so the resume plumbing lives in one place.
/// Every overlay column any atom in `snapshots` carries, deduplicated.
///
/// Read off the snapshots rather than off the frame because that is what the
/// wallet will actually see: a column dropped by a join or absent from one
/// symbol's series is absent *here*, which is the question a carry model's
/// "will my column be there" check is really asking.
fn overlay_columns(snapshots: &[fugazi::types::Snapshot<Symbol>]) -> Vec<String> {
    let mut columns: Vec<String> = Vec::new();
    for snap in snapshots {
        for (_, _, atom) in snap.iter() {
            let Some(overlays) = atom.overlays.as_ref() else {
                continue;
            };
            for key in overlays.schema().keys() {
                if !columns.iter().any(|c| c == key) {
                    columns.push(key.to_string());
                }
            }
        }
    }
    columns
}

fn iterate(
    spec: &fugazi::spec::StrategySpec,
    bars: Vec<String>,
    snapshots: &[fugazi::types::Snapshot<Symbol>],
    inputs: &backtest::EvalContext,
    opts: &RunOptions,
) -> Result<backtest::IterationResult> {
    // Every run shape funnels through here, so this is the one place a carry
    // model that cannot charge gets called out — before the run rather than
    // after, since both failure modes leave an equity curve that looks exactly
    // like carry being free.
    if !opts.quiet {
        style::print_warns(&style::carry_warnings(
            &inputs.cost_config.carry_requirements(),
            &overlay_columns(snapshots),
            inputs.effective_freq.is_some(),
        ));
    }
    if opts.resume.is_none() && opts.save_state.is_none() && !opts.flatten {
        return backtest::run_iteration_any(spec, bars, snapshots, inputs)
            .map_err(backtest::build_error);
    }
    let (iter, state) =
        backtest::run_iteration_resumable(spec, bars, snapshots, inputs, opts.resume, opts.flatten)
            .map_err(backtest::build_error)?;
    if let Some(path) = opts.save_state {
        let json = serde_json::to_string_pretty(&state).context("serializing run state")?;
        std::fs::write(path, json)
            .with_context(|| format!("writing state file {}", path.display()))?;
    }
    Ok(iter)
}

/// Emit the Monte Carlo results the backtest layer already computed (when
/// `--montecarlo` was set on the `EvalContext`): write `montecarlo.csv` and
/// narrate the console block. The computation itself lives in
/// [`backtest::run_iteration_resumable`], so every driver — not just the CLI —
/// gets the `montecarlo:` block on its metrics; this is the CLI's IO half.
fn emit_montecarlo(iter: &backtest::IterationResult, opts: &RunOptions) -> Result<()> {
    if opts.montecarlo.is_none() {
        return Ok(());
    }
    if let Some(samples) = &iter.mc_samples {
        write_montecarlo_csv(samples, &opts.out_dir.join("montecarlo.csv"))?;
    }
    if !opts.quiet
        && let Some(section) = &iter.metrics.montecarlo
    {
        print_montecarlo_block(section);
    }
    Ok(())
}

/// Write the per-resample metric values to `montecarlo.csv`: one row per
/// (estimator, permutation), columns `estimator,permutation,<metric...>`.
fn write_montecarlo_csv(samples: &fugazi::spec::montecarlo::McSamples, path: &Path) -> Result<()> {
    use std::fmt::Write as _;
    let mut out = String::new();
    out.push_str("estimator,permutation");
    for name in &samples.metric_names {
        let _ = write!(out, ",{name}");
    }
    out.push('\n');
    for set in &samples.sets {
        for (p, row) in set.rows.iter().enumerate() {
            let _ = write!(out, "{},{}", set.estimator, p);
            for cell in row {
                match cell {
                    Some(v) => {
                        let _ = write!(out, ",{v}");
                    }
                    None => out.push(','),
                }
            }
            out.push('\n');
        }
    }
    std::fs::write(path, out).with_context(|| format!("writing {}", path.display()))?;
    Ok(())
}

/// Narrate the Monte Carlo block: the resampling config, then one line per
/// metric with its observed value, bootstrap CI, and any p-values.
fn print_montecarlo_block(section: &fugazi::spec::metrics::McSection) {
    use std::fmt::Write as _;
    println!();
    style::print_section("montecarlo");
    println!(
        "  {} resamples · {} · seed {} · {:.0}% CI",
        section.permutations,
        section.scheme,
        section.seed,
        section.ci_level * 100.0
    );
    let fmt = |v: Option<Real>| {
        v.map(|x| format!("{x:.4}"))
            .unwrap_or_else(|| "—".to_string())
    };
    for m in &section.metrics {
        let mut line = format!(
            "  {:<28} obs {:>10}  CI [{}, {}]",
            m.name,
            fmt(m.observed),
            fmt(m.ci_lower),
            fmt(m.ci_upper),
        );
        if let Some(p) = m.p_value_rerun {
            let _ = write!(line, "  p(rerun) {p:.4}");
        }
        println!("{line}");
    }
}

/// Run `spec` over `frame` per `opts` — resolve inputs, delegate the pure
/// work to [`backtest::run_iteration`], and write the result files +
/// narrate the tiered run/trade/result/metrics logs.
pub fn run(strategy: &StrategyRef, frame: &DataFrame, opts: &RunOptions) -> Result<Summary> {
    let started = SystemTime::now();
    let symbol = strategy.symbol().map_err(backtest::build_error)?;
    let series = frame.atoms(&symbol)?;
    let atoms = series.atoms;
    let skipped_overlay_columns = series.skipped_columns;

    std::fs::create_dir_all(opts.out_dir)
        .with_context(|| format!("creating output dir `{}`", opts.out_dir.display()))?;

    // The effective bar cadence for both annualization and cost-scope
    // matching, best evidence first: a symbol-matching `-f/--frequency` entry,
    // then the cadence the document's own `root:` declares, then the input's
    // `freq` column, then the cadence detected from the atoms' `time` field
    // (populated by the loader). The `freq` column outranks arithmetic on the
    // gaps because a provider that told us the cadence beats a median over the
    // bars it sent — a thinly-traded name's gaps can median to the wrong
    // cadence outright — and the document outranks both, because a `root:`
    // that names a cadence is the author saying so in as many words.
    let effective_freq = calendar::pick_frequency(opts.frequency, &symbol)
        .or_else(|| {
            strategy
                .root()
                .declared_freq()
                .and_then(|f| Frequency::from_str(f).ok())
        })
        .or_else(|| frame.declared_frequency(&symbol))
        .or_else(|| calendar::detect_frequency_from_atoms(atoms.iter().map(|(_, a)| a)));
    // Resolve `bars_per_year`: a scope-matching `--bars-per-year` entry wins,
    // else fall through to the class × cadence calendar.
    let bars_per_year = calendar::pick_bars_per_year(opts.bars_per_year, &symbol, effective_freq)
        .unwrap_or_else(|| calendar::resolve(None, opts.asset_class, effective_freq));
    let no_cost_warning = !opts.costs_supplied;
    let mut inputs = eval_context(opts, effective_freq, bars_per_year)?;

    // The unified driver is snapshot-shaped; lift the single-symbol atom
    // stream into one tagged entry per bar.
    //
    // `bars` is taken first and the atoms are then **consumed** into the
    // snapshots. Cloning each `Atom` out of a still-live `atoms` held both
    // representations at once — on a long run that is a second copy of the whole
    // series resident at peak for no reason, and `Atom` is 88 bytes a bar before
    // its overlays.
    let bars: Vec<String> = atoms.iter().map(|(t, _)| t.clone()).collect();
    // Series the document reads but does not trade — resolved (and refused, if
    // absent) before the stream is built, so a missing one is an error rather
    // than a run of `None`s. Nothing to do for a document that names none,
    // which is the overwhelmingly common case.
    let read_only = read_only_series(frame, &[symbol.as_str()], opts.reads)?;
    // Interned once; each bar's tag is then a refcount bump.
    let sym = fugazi::types::symbol(&symbol);
    let mut snapshots: Vec<fugazi::types::Snapshot<Symbol>> = atoms
        .into_iter()
        .map(|(_, a)| fugazi::types::Snapshot::single(sym.clone(), a))
        .collect();
    // Left-joined onto the traded symbol's own bars: a read-only series adds
    // entries to existing snapshots, never snapshots of its own. The traded
    // symbol is therefore present in every snapshot, which is what keeps the
    // strategy's `Position` and `Book` reading its own candle.
    attach_read_series(&bars, &mut snapshots, &read_only);
    let spec = StrategySpec::Single(Box::new(strategy.clone()));
    // Resolved before the inputs block prints, so the `period` line names the
    // range that will be *measured* rather than the range the file covers.
    let sliced = sliced_inputs(&spec, bars, snapshots, &mut inputs, opts)?;

    // Print the inputs block up front so a long-running run still shows the
    // user what they asked for while it's working.
    if !opts.quiet {
        let costs_active = costs_active(opts.cost_config, [symbol.as_str()], effective_freq);
        style::print_header("run", "backtest a strategy over CSV series");
        style::print_warns(&style::collect_warnings(
            &skipped_overlay_columns,
            no_cost_warning,
            "results",
        ));
        print_inputs_block(opts, &sliced, costs_active);
    }

    let iter = iterate(&spec, sliced.bars, &sliced.snapshots, &inputs, opts)?;
    emit_montecarlo(&iter, opts)?;

    // Emit `fills.csv` and echo each fill in the same order the wallet booked
    // them. The console stream matches the CSV row-for-row.
    emit_run(&iter, opts, started, effective_freq)
}

/// The pairs twin of [`run`]: drive a
/// [`PairsStrategy`](fugazi::strategies::PairsStrategy) over the two legs'
/// aligned atom streams. Same output shape (`fills.csv`, `trades.csv`,
/// `returns.csv`, `metrics.yml`, and the windowed CSVs under `-w`), so the
/// caller's downstream analysis pipeline is unchanged.
///
/// Time-alignment is an **inner join** on the `time` column: only bars where
/// both symbols have data are fed to the strategy. A mismatched pair produces
/// a run over the intersecting bars, with the count echoed in the run's
/// `period` line.
pub fn run_pairs(
    spec: &PairsStrategySpec,
    frame: &DataFrame,
    opts: &RunOptions,
) -> Result<Summary> {
    let started = SystemTime::now();
    // Each leg's root resolves to exactly one instrument, or the document is
    // refused before a bar is read.
    let left = spec
        .left
        .sole_symbol("pairs")
        .map_err(backtest::build_error)?;
    let right = spec
        .right
        .sole_symbol("pairs")
        .map_err(backtest::build_error)?;
    let left_series = frame.atoms(&left)?;
    let right_series = frame.atoms(&right)?;
    let (bars, left_atoms, right_atoms) =
        join_pair_by_time(&left_series.atoms, &right_series.atoms);

    std::fs::create_dir_all(opts.out_dir)
        .with_context(|| format!("creating output dir `{}`", opts.out_dir.display()))?;

    // Pick the effective cadence off the left leg (both legs are expected to
    // share one cadence — the inner-join filters to the shared timeline).
    let effective_freq = calendar::pick_frequency(opts.frequency, &left)
        .or_else(|| calendar::pick_frequency(opts.frequency, &right))
        .or_else(|| {
            spec.left
                .declared_freq()
                .or_else(|| spec.right.declared_freq())
                .and_then(|f| Frequency::from_str(f).ok())
        })
        .or_else(|| frame.declared_frequency(&left))
        .or_else(|| frame.declared_frequency(&right))
        .or_else(|| {
            calendar::detect_frequency_from_atoms(left_series.atoms.iter().map(|(_, a)| a))
        });
    let bars_per_year = calendar::pick_bars_per_year(opts.bars_per_year, &left, effective_freq)
        .or_else(|| calendar::pick_bars_per_year(opts.bars_per_year, &right, effective_freq))
        .unwrap_or_else(|| calendar::resolve(None, opts.asset_class, effective_freq));
    let no_cost_warning = !opts.costs_supplied;
    let mut inputs = eval_context(opts, effective_freq, bars_per_year)?;

    // Both leg names interned once; each bar then tags with a refcount bump.
    let left_sym = fugazi::types::symbol(&left);
    let right_sym = fugazi::types::symbol(&right);
    let mut snapshots: Vec<fugazi::types::Snapshot<Symbol>> = left_atoms
        .iter()
        .zip(right_atoms.iter())
        .map(|(l, r)| {
            let mut snap = fugazi::types::Snapshot::new();
            snap.push(Some(left_sym.clone()), None, l.clone());
            snap.push(Some(right_sym.clone()), None, r.clone());
            snap
        })
        .collect();
    // A third series the document reads — a pair hedged against an index level,
    // say — joins onto the legs' *inner-joined* timeline. Neither leg is
    // privileged here, so `!pick` is already mandatory on every leaf; this only
    // widens which assets one can name.
    let read_only = read_only_series(frame, &[left.as_str(), right.as_str()], opts.reads)?;
    attach_read_series(&bars, &mut snapshots, &read_only);
    let any = StrategySpec::Pairs(Box::new(spec.clone()));
    // The slice lands on the *joined* timeline, so two partially-overlapping
    // legs behave the way the dates say rather than the way the files do.
    let sliced = sliced_inputs(&any, bars, snapshots, &mut inputs, opts)?;

    if !opts.quiet {
        let costs_active = costs_active(
            opts.cost_config,
            [left.as_str(), right.as_str()],
            effective_freq,
        );
        style::print_header("run", "pair-trade a two-leg strategy over CSV series");
        style::print_warns(&style::collect_warnings(&[], no_cost_warning, "results"));
        print_pairs_inputs_block(opts, &left, &right, &sliced, costs_active);
    }

    let iter = iterate(&any, sliced.bars, &sliced.snapshots, &inputs, opts)?;
    emit_montecarlo(&iter, opts)?;

    emit_run(&iter, opts, started, effective_freq)
}

/// The shared driver for the three N-symbol shapes.
///
/// Basket, multi-asset and portfolio ran identical bodies: resolve the symbol
/// set, build per-symbol atom streams, outer-join them on `time`, read the
/// calendar off a representative symbol, print the inputs block, drive, and
/// emit. They differed in five spots — the spec variant, two strings, and
/// whether the cost probe includes the unscoped `default:` leg — so those are
/// parameters and the bodies are one.
///
/// `declared` is the sixth: basket and multi-asset **discover** their universe
/// from the frame and pass `None`, a portfolio passes what its children name
/// (see [`portfolio_declared_symbols`]). Either way the universe is the *traded*
/// set; series the document only reads join in through [`read_only_series`]
/// without extending the timeline.
///
/// Cadence is read from the representative symbol: none of these shapes
/// declares per-symbol cadences, and a mixed-cadence universe is a follow-up if
/// it becomes a real need. Typical universes are homogeneous.
fn run_universe(
    any: StrategySpec,
    noun: &str,
    headline: &str,
    probe_default_costs: bool,
    declared: Option<Vec<String>>,
    frame: &DataFrame,
    opts: &RunOptions,
) -> Result<Summary> {
    let started = SystemTime::now();
    // The traded universe: whatever the shape declares, else every symbol the
    // frame carries. Basket and multi-asset genuinely *discover* theirs — the
    // frame is the universe, by design — so they pass `None`. A portfolio does
    // not: its children name what they trade, and a symbol no child mentions
    // was never going to be traded, only carried.
    let traded: Vec<String> = declared.unwrap_or_else(|| frame.symbols());
    // Interned once per distinct symbol for the whole run.
    let universe: Vec<Symbol> = traded.iter().map(fugazi::types::symbol).collect();
    if universe.is_empty() {
        anyhow::bail!(
            "no symbols found in the input series — {noun} needs at least one traded asset"
        );
    }
    // Per-symbol atom streams, sorted by time (DataFrame::atoms walks a
    // BTreeMap so ascending order is guaranteed by construction).
    let per_symbol: Vec<(Symbol, Vec<(String, Atom)>)> = universe
        .iter()
        .map(|sym| Ok::<_, anyhow::Error>((sym.clone(), frame.atoms(sym)?.atoms)))
        .collect::<Result<_>>()?;
    let (bars, mut snapshots) = join_universe_by_time(&per_symbol);
    // Series read but not traded. Empty whenever the universe is the whole
    // frame — every `!pick` target is already in it — so for basket and
    // multi-asset this is purely the "named a symbol that isn't in the input"
    // check, which those shapes need just as much: a typo in a `score:` reads
    // `None` forever and scores nothing, silently.
    let traded_refs: Vec<&str> = traded.iter().map(String::as_str).collect();
    let read_only = read_only_series(frame, &traded_refs, opts.reads)?;
    attach_read_series(&bars, &mut snapshots, &read_only);
    if bars.is_empty() {
        anyhow::bail!(
            "no bars found in the input series across the {} discovered symbol(s)",
            universe.len()
        );
    }
    // How much of the discovered universe ever lands on one bar. A `--series`
    // CSV assembled from differently-timed sessions joins into snapshots that
    // never hold it all, and every other surface here — the symbol list, the
    // bar count, the period — still reads correct. See `crate::overlap`.
    let overlap = overlap::measure_universe(&per_symbol);

    std::fs::create_dir_all(opts.out_dir)
        .with_context(|| format!("creating output dir `{}`", opts.out_dir.display()))?;

    let representative = &universe[0];
    let (effective_freq, bars_per_year) =
        universe_calendar(opts, frame, representative, &per_symbol);
    let no_cost_warning = !opts.costs_supplied;
    let mut inputs = eval_context(opts, effective_freq, bars_per_year)?;
    // Sliced on the joined timeline, after the outer join — so a symbol that
    // only lists partway through the range keeps the same relationship to the
    // others that it has in an unsliced run.
    let sliced = sliced_inputs(&any, bars, snapshots, &mut inputs, opts)?;
    if !opts.quiet {
        // A portfolio also probes the unscoped `default:` leg with `""` — see
        // `costs_active`.
        let probes = probe_default_costs
            .then_some("")
            .into_iter()
            .chain(universe.iter().map(|s| s.as_ref()));
        let costs_active = costs_active(opts.cost_config, probes, effective_freq);
        style::print_header("run", headline);
        style::print_warns(&style::collect_warnings(&[], no_cost_warning, "results"));
        print_basket_inputs_block(opts, &universe, &sliced, costs_active, &overlap);
    }
    // Deliberately outside the `--quiet` guard, and on stderr rather than in
    // the block above: the other warnings here are advisory (no cost model, a
    // dropped column), while this one says the run is about to measure
    // something other than the universe it names. `--quiet` suppresses the
    // summary, not a finding about the data.
    overlap::warn_if_fragmented(&overlap, overlap.at, overlap::RUN_CONSEQUENCE);

    let iter = iterate(&any, sliced.bars, &sliced.snapshots, &inputs, opts)?;
    emit_montecarlo(&iter, opts)?;

    emit_run(&iter, opts, started, effective_freq)
}

/// The basket runner: drive a [`BasketStrategy`](fugazi::strategies::BasketStrategy)
/// over every symbol discovered in `frame`, time-aligning per-symbol atom
/// streams by outer-joining on `time` (a symbol without a bar at some
/// time simply doesn't appear in that bar's snapshot — the strategy's
/// per-symbol `Pick` reads `None` and its score chain propagates it up).
///
/// The tradeable **universe is the set of symbols in the frame** — no
/// explicit declaration in the YAML; the basket rebuilds its
/// score/sizing chains lazily as symbols appear (see
/// [`BasketStrategySpec`] for the `!arg SYM` substitution model). A
/// per-symbol cost bundle is resolved from `--costs` for each symbol
/// (via [`fugazi::PaperWallet::set_costs_for`]) so a scoped rule like
/// `BTC:0.001,ETH:0.0005` applies per leg; symbols not scoped fall back
/// to the wallet's zero-cost default.
pub fn run_basket(
    spec: &BasketStrategySpec,
    frame: &DataFrame,
    opts: &RunOptions,
) -> Result<Summary> {
    run_universe(
        StrategySpec::Basket(Box::new(spec.clone())),
        "a basket",
        "trade a basket across an N-symbol universe",
        false,
        None,
        frame,
        opts,
    )
}

/// The multi-asset runner: drive a
/// [`MultiAssetStrategy`](fugazi::strategies::MultiAssetStrategy) over
/// every symbol discovered in `frame`, time-aligning per-symbol atom
/// streams by outer-joining on `time` — same pipeline as
/// [`run_basket`], since the frame → snapshot bridge is identical. The
/// tradeable universe is either the frame's set of symbols (default) or
/// the intersection with the YAML's declared `!all_of` / `!any_of`.
pub fn run_multi(
    spec: &MultiAssetStrategySpec,
    frame: &DataFrame,
    opts: &RunOptions,
) -> Result<Summary> {
    run_universe(
        StrategySpec::Multi(Box::new(spec.clone())),
        "a multi-asset strategy",
        "trade a multi-asset portfolio across an N-symbol universe",
        false,
        None,
        frame,
        opts,
    )
}

/// The portfolio runner: drive a composite [`Portfolio`](fugazi::portfolio::Portfolio)
/// over every symbol discovered in `frame`, time-aligning per-symbol atom
/// streams by outer-joining on `time` — same pipeline as [`run_basket`] /
/// [`run_multi`]. Children trade notional ledgers over one account; the
/// portfolio nets their intents into that account and reports one unified
/// equity curve and blotter.
///
/// **Costs.** A portfolio is now an ordinary strategy that trades the wallet it
/// is handed, so costs ride on that wallet exactly like the other four shapes:
/// the unscoped `--costs` default and per-symbol `--costs SYM:...` overrides are
/// primed onto the `PaperWallet` by `RunnableStrategy::drive`, and whichever
/// child fills a given symbol books at the wallet's rate for it.
pub fn run_portfolio(
    spec: &PortfolioSpec,
    frame: &DataFrame,
    opts: &RunOptions,
) -> Result<Summary> {
    run_universe(
        StrategySpec::Portfolio(Box::new(spec.clone())),
        "a portfolio",
        "trade a composite portfolio of heterogeneous child strategies",
        true,
        portfolio_declared_symbols(spec),
        frame,
        opts,
    )
}

/// The symbols a portfolio's children *declare* they trade — its traded
/// universe, when every child has one.
///
/// A single-asset child names one symbol, a pairs child names two; both are
/// known before a bar is read. A **basket or multi-asset child discovers its
/// universe from the frame**, so a portfolio containing one has no declared
/// universe either, and `None` sends the runner back to taking the whole frame.
///
/// Restricting matters because the alternative is carrying every symbol in the
/// input through every snapshot of the run: a portfolio of two single-asset
/// children pointed at a twenty-symbol CSV would build twenty-entry snapshots
/// for eighteen assets nothing trades or reads, and outer-join their bars onto
/// a timeline none of its children has. Symbols a child only *reads* come back
/// through [`read_only_series`] instead, which left-joins them rather than
/// letting them extend the timeline.
fn portfolio_declared_symbols(spec: &PortfolioSpec) -> Option<Vec<String>> {
    use fugazi::spec::portfolio::PortfolioChildStrategy as Child;
    let mut out: BTreeSet<String> = BTreeSet::new();
    for child in &spec.children {
        match &child.strategy {
            // A child whose root names anything other than one instrument is
            // refused when the portfolio is built; here it simply contributes
            // whatever it named, and `None` stays reserved for the shapes that
            // genuinely declare nothing.
            Child::Single(s) => out.extend(s.root().named_symbols()),
            Child::Pairs(p) => {
                out.extend(p.left.named_symbols());
                out.extend(p.right.named_symbols());
            }
            // Discovered, not declared — so the portfolio's is too.
            Child::Basket(_) | Child::Multi(_) => return None,
        }
    }
    Some(out.into_iter().collect())
}

/// The bar cadence and annualization factor for an N-symbol run, read off a
/// representative symbol.
///
/// Shared verbatim by the basket, multi-asset and portfolio runners — the three
/// shapes whose universe is discovered from the stream rather than declared, so
/// none of them has a single symbol whose calendar is authoritative.
/// A scope-matching `-f/--frequency` wins, then the representative's own `freq`
/// column, then the cadence detected from its atoms. A universe whose symbols
/// disagree is a warning rather than an error, raised once at load by
/// [`crate::cadence`] — this function is where the consequence lands, since one
/// symbol's answer becomes the whole run's annualization.
fn universe_calendar(
    opts: &RunOptions<'_>,
    frame: &DataFrame,
    representative: &str,
    per_symbol: &[(Symbol, Vec<(String, fugazi::types::Atom)>)],
) -> (Option<Frequency>, Real) {
    let effective_freq = calendar::pick_frequency(opts.frequency, representative)
        .or_else(|| frame.declared_frequency(representative))
        .or_else(|| {
            per_symbol
                .iter()
                .find(|(s, _)| s.as_ref() == representative)
                .and_then(|(_, atoms)| {
                    calendar::detect_frequency_from_atoms(atoms.iter().map(|(_, a)| a))
                })
        });
    let bars_per_year =
        calendar::pick_bars_per_year(opts.bars_per_year, representative, effective_freq)
            .unwrap_or_else(|| calendar::resolve(None, opts.asset_class, effective_freq));
    (effective_freq, bars_per_year)
}

/// Assemble the resolved-once run inputs the driver takes.
///
/// All five runners built this identically — the `-w/--windowed` resolution,
/// the per-bar trading seconds, and the eight-field `EvalContext` literal, 21
/// lines apiece. Only the two arguments differ per shape, because a
/// single-asset run reads its own symbol's cadence, pairs tries both legs, and
/// the N-symbol shapes use a representative.
fn eval_context<'a>(
    opts: &RunOptions<'a>,
    effective_freq: Option<Frequency>,
    bars_per_year: Real,
) -> Result<EvalContext<'a>> {
    let windowed_bars = opts
        .windowed
        .map(|w| {
            w.resolve(effective_freq, opts.asset_class)
                .map_err(anyhow::Error::msg)
        })
        .transpose()
        .context("resolving `-w/--windowed`")?;
    let seconds_per_bar = opts
        .asset_class
        .zip(effective_freq)
        .map(|(class, freq)| class.trading_seconds_per_bar(freq));
    Ok(EvalContext {
        cash: opts.cash,
        max_gross: opts.max_gross,
        margin_rate: opts.margin_rate,
        maintenance_margin: opts.maintenance_margin,
        bars_per_year,
        risk_free_rate: opts.risk_free_rate,
        cost_config: opts.cost_config,
        effective_freq,
        windowed: windowed_bars,
        seconds_per_bar,
        mc: opts.montecarlo.cloned(),
        warmup_bars: None,
    })
}

/// Write every artefact a `run` produces and print its closing console blocks.
///
/// All five runners ended with a byte-identical 47-line block: `fills.csv`,
/// the fill stream, the rejection banner, `trades.csv`, `returns.csv`,
/// `metrics.yml`, the two windowed CSVs, the summary arithmetic, and the
/// result / metrics / windowed-metrics blocks. Five copies of the output
/// contract meant any change to it had to be made five times, with nothing
/// checking that it was.
fn emit_run(
    iter: &IterationResult,
    opts: &RunOptions,
    started: SystemTime,
    effective_freq: Option<Frequency>,
) -> Result<Summary> {
    write_fills_csv(iter, &opts.out_dir.join("fills.csv"))?;
    if !opts.quiet {
        println!();
        style::print_section("fills");
        stream_fills(iter);
    }
    if !opts.quiet {
        print_ruin_warning(&iter.report);
        print_rejection_warning(&iter.report);
        print_fitted_warning(&iter.report);
        print_liquidation_warning(&iter.report);
    }
    write_trades_csv(iter, &opts.out_dir.join("trades.csv"))?;

    write_returns_csv(iter, &opts.out_dir.join("returns.csv"))?;

    metrics::write_yaml(&iter.metrics, &opts.out_dir.join("metrics.yml"))?;

    if let Some(ws) = iter.windowed.as_deref() {
        let dsr_context = metrics::windows_dsr_context(ws);
        write_windowed_csv(
            ws,
            &iter.bars,
            dsr_context,
            &opts.out_dir.join("metrics.csv"),
        )?;
    }
    if let Some(rs) = iter.rolling.as_deref() {
        write_windowed_csv(rs, &iter.bars, None, &opts.out_dir.join("rolling.csv"))?;
    }

    let summary = Summary {
        final_equity: iter.summary.final_equity,
        return_pct: if opts.cash != 0.0 {
            (iter.summary.final_equity - opts.cash) / opts.cash * 100.0
        } else {
            0.0
        },
        fills: iter.summary.fills,
        bars: iter.summary.bars,
    };

    let finished = SystemTime::now();
    if !opts.quiet {
        print_result_block(opts, &summary, started, finished);
        print_metrics_block(
            &iter.metrics,
            None,
            iter.gross_metrics.as_ref(),
            effective_freq,
        );
        if let Some(windows) = iter.windowed.as_deref() {
            print_windowed_metrics_block(windows);
        }
    }
    Ok(summary)
}

/// Outer-join every symbol's atom stream on `time`. Returns `(times,
/// snapshots)` where each snapshot carries only the symbols with a bar at
/// that time — sparse per bar is normal. Each per-symbol series is already
/// sorted (BTreeMap invariant), so a single N-way merge over cursors
/// Whether a cost model is active for **any** of `symbols` at `freq`.
///
/// Every runner needs this to decide whether the console blocks say "gross" or
/// "net", and each had grown its own spelling: a bare `resolve(&symbol)` for
/// single-asset, an `||` of the two legs for pairs, an `any()` over the
/// universe for basket and multi, and — for portfolio — an `any()` *plus* a
/// separate probe of the `""` default leg.
///
/// That last term is redundant whenever the universe is non-empty, since a
/// configured `default:` resolves for every symbol too. It is preserved rather
/// than dropped: the portfolio runner passes `""` as an extra probe symbol, so
/// the behaviour is identical and the asymmetry is visible at the call site
/// instead of buried in a fifth copy of the expression.
fn costs_active<'a>(
    cost_config: &fugazi::spec::costs::CostConfig,
    symbols: impl IntoIterator<Item = &'a str>,
    freq: Option<Frequency>,
) -> bool {
    symbols
        .into_iter()
        .any(|s| !cost_config.resolve(s, freq).is_none())
}

/// suffices.
///
/// **Grouping is on the exact time label**, so series stamped at different
/// session opens never share a snapshot — deliberately, since folding them by
/// trading date would manufacture lookahead across time zones. Every caller
/// should therefore also run [`crate::overlap::measure_universe`] over the same
/// `per_symbol` and hand the result to
/// [`warn_if_fragmented`](crate::overlap::warn_if_fragmented): a universe no
/// snapshot ever holds in full is silent in every other output. Kept as a
/// convention at the call sites rather than folded in here, so this stays a
/// pure join.
pub(crate) fn join_universe_by_time(
    per_symbol: &[(Symbol, Vec<(String, Atom)>)],
) -> (Vec<String>, Vec<fugazi::types::Snapshot<Symbol>>) {
    // Cursor per symbol.
    let mut cursors = vec![0usize; per_symbol.len()];
    let mut times: Vec<String> = Vec::new();
    let mut snaps: Vec<fugazi::types::Snapshot<Symbol>> = Vec::new();
    loop {
        // Find the smallest time head across all live cursors.
        let next_time: Option<&str> = per_symbol
            .iter()
            .zip(cursors.iter())
            .filter_map(|((_sym, atoms), &i)| atoms.get(i).map(|(t, _)| t.as_str()))
            .min();
        let Some(next) = next_time else {
            break;
        };
        let next_owned = next.to_string();
        let mut snap = fugazi::types::Snapshot::<Symbol>::new();
        for ((sym, atoms), cursor) in per_symbol.iter().zip(cursors.iter_mut()) {
            if let Some((t, atom)) = atoms.get(*cursor)
                && t == &next_owned
            {
                snap.push(Some(sym.clone()), None, atom.clone());
                *cursor += 1;
            }
        }
        times.push(next_owned);
        snaps.push(snap);
    }
    (times, snaps)
}

/// Per-symbol atom streams, each sorted by its time label — what
/// [`DataFrame::atoms`] produces per symbol and [`join_universe_by_time`]
/// consumes.
pub(crate) type SymbolStreams = Vec<(Symbol, Vec<(String, Atom)>)>;

/// Resolve the series a document **reads but does not trade** — every symbol
/// `opts.reads` collected from an explicit `!pick { symbol: … }`, minus the ones
/// `traded` already covers — into per-symbol atom streams ready to be joined in.
///
/// A named series that is not in the input is a **hard error**, not an empty
/// read. `Pick::matching` resolves `None` on a bar it does not match, which is
/// the right answer for a listing gap and exactly the wrong one for a symbol
/// that was never passed: every downstream comparison stays `None`, no signal
/// ever fires, and the run completes with zero fills and nothing said. That
/// failure reads as "the gate filtered everything out" rather than "the gate
/// never evaluated", which is the most expensive way for a backtest to be
/// wrong.
pub(crate) fn read_only_series(
    frame: &DataFrame,
    traded: &[&str],
    reads: &BTreeSet<String>,
) -> Result<SymbolStreams> {
    let mut out = Vec::new();
    if reads.iter().all(|s| traded.contains(&s.as_str())) {
        return Ok(out);
    }
    let available = frame.symbols();
    for sym in reads {
        if traded.contains(&sym.as_str()) {
            continue;
        }
        if !available.iter().any(|s| s == sym) {
            anyhow::bail!(
                "`!pick {{ symbol: {sym} }}` names a series that is not in the input.\n\
                 \n\
                 The document reads `{sym}` but `--series` carries {}. A `!pick` \
                 naming another asset reads that asset's bars off the same \
                 timeline, so the series has to be passed with `-s/--series` \
                 alongside the traded one — it is read, not traded, and adds no \
                 bars of its own to the run.",
                if available.is_empty() {
                    "no symbols".to_string()
                } else {
                    available.join(", ")
                },
            );
        }
        out.push((fugazi::types::symbol(sym), frame.atoms(sym)?.atoms));
    }
    Ok(out)
}

/// Left-join `read_only` series onto a snapshot stream whose timeline is
/// already fixed by the *traded* symbols.
///
/// **Left, not outer**: a series the document only reads must not create bars.
/// A regime gate on BTC should not manufacture an ETH bar on a day ETH did not
/// trade — the traded symbol would be absent from that snapshot, and every
/// per-bar count the run reports (bars, returns, the annualization divisor)
/// would describe a timeline the traded asset never had. So `bars` stays what
/// it was, and a read series simply contributes an entry to the bars it shares.
///
/// Both sides are sorted by the time label (`DataFrame::atoms` walks a
/// `BTreeMap`), which is the same ordering assumption
/// [`join_universe_by_time`] makes, so one forward cursor per series suffices.
pub(crate) fn attach_read_series(
    bars: &[String],
    snapshots: &mut [fugazi::types::Snapshot<Symbol>],
    read_only: &[(Symbol, Vec<(String, Atom)>)],
) {
    for (sym, atoms) in read_only {
        let mut cursor = 0usize;
        for (bar, snap) in bars.iter().zip(snapshots.iter_mut()) {
            while cursor < atoms.len() && &atoms[cursor].0 < bar {
                cursor += 1;
            }
            match atoms.get(cursor) {
                Some((t, atom)) if t == bar => {
                    snap.push(Some(sym.clone()), None, atom.clone());
                    cursor += 1;
                }
                _ => {}
            }
        }
    }
}

#[allow(clippy::too_many_arguments)]
fn print_basket_inputs_block(
    opts: &RunOptions,
    universe: &[Symbol],
    sliced: &Sliced,
    costs_active: bool,
    overlap: &Overlap<&str>,
) {
    style::print_section("inputs");
    style::field("strategy", opts.strategy_label);
    style::field(
        "universe",
        &format!("{} symbols ({})", universe.len(), universe.join(", ")),
    );
    // Printed whether or not it is a problem: `9 of 9` is the positive
    // confirmation that the universe actually meets, which is what a
    // cross-sectional run lives or dies on. The loud version of the bad case
    // is `overlap::warn_if_fragmented`, on stderr.
    if overlap.total > 1 {
        let value = overlap.summary();
        style::field(
            "overlap",
            &if overlap.is_fragmented() {
                style::yellow(&value)
            } else {
                value
            },
        );
    }
    style::field("params", opts.params);
    style::field("period", &period_field(sliced));
    style::field("capital", &format!("{:.2}", opts.cash));
    if costs_active {
        style::field("costs", "active (commission/spread/slippage applied)");
    } else if opts.costs_supplied {
        style::field("costs", "none (explicit)");
    }
    style::field("output", &opts.out_dir.display().to_string());
}

/// How many bars this strategy needs before its chains read settled — the
/// depth `--from` reads back out of the series to warm them.
///
/// Deliberately the same quantity `--walkforward` skips at the head of a
/// series (`stable_bars`, and `warm_up_bars` is its `--keep-unstable` twin), so
/// the two features agree on what "settled" means rather than each inventing a
/// number.
fn warmup_need(
    spec: &StrategySpec,
    snapshots: &[fugazi::types::Snapshot<Symbol>],
    cash: Real,
) -> Result<usize> {
    let schema = backtest::schema_from_snapshots(snapshots);
    let mut built = spec
        .try_build(cash, &schema, None)
        .map_err(backtest::build_error)?;
    // Basket and multi build their per-symbol chains lazily, on first sight of
    // a symbol, so `stable_bars()` only reads true once a snapshot has gone
    // through. The eager shapes must *not* be fed one — a pairs leaf that named
    // no asset would hit the sole-atom guard on a multi-symbol snapshot. Same
    // split as `optimize`'s `needs_probe_feed`.
    if matches!(spec.kind(), "basket" | "multi")
        && let Some(first) = snapshots.first()
    {
        built.update(first.clone());
    }
    Ok(built.stable_bars())
}

/// Refuse a `--from` that would re-run bars the resume state has already seen.
///
/// Resuming replays nothing: the state *is* the strategy at its last bar. A
/// `--from` pointing at or before that bar therefore asks for two incompatible
/// things at once, and silently replaying is the outcome most likely to be
/// mistaken for a longer run.
fn guard_resume_against(slice: &Slice, bars: &[String], opts: &RunOptions) -> Result<()> {
    let (Some(state), Some(label)) = (opts.resume, opts.from_label) else {
        return Ok(());
    };
    let Some(last) = state.last_bar else {
        return Ok(());
    };
    let Some(first_evaluated) = bars.get(slice.eval_start) else {
        return Ok(());
    };
    if calendar::parse_time_to_millis(first_evaluated).is_some_and(|ms| ms <= last) {
        anyhow::bail!(
            "`--from {label}` starts at or before the last bar in `--resume` \
             ({first_evaluated} is not after the state's last bar) — resuming \
             continues from that bar, so this would re-run history rather than \
             extend it. Move `--from` past it, or drop `--resume`."
        );
    }
    Ok(())
}

/// Resolve `--from`/`--until` against this run's joined bar stream, warning if
/// evaluation had to start late and refusing a range that fights `--resume`.
///
/// Returns the whole stream untouched when neither bound was given, so an
/// unsliced run takes exactly the path it always did.
fn resolve_slice(
    spec: &StrategySpec,
    bars: &[String],
    snapshots: &[fugazi::types::Snapshot<Symbol>],
    opts: &RunOptions,
) -> Result<Slice> {
    let Some(range) = opts.range else {
        return Ok(Slice::everything(bars.len()));
    };
    let need = if range.reads_back() {
        warmup_need(spec, snapshots, opts.cash)?
    } else {
        0
    };
    let slice = range.resolve(bars, need)?;
    guard_resume_against(&slice, bars, opts)?;
    // Ungated by `--quiet`, like the overlap and cadence warnings: a run that
    // measured a different period than it was asked for must say so even when
    // the caller asked for silence.
    if let Some(label) = opts.from_label
        && let Some(warning) = daterange::short_warmup_warning(&slice, bars, label, need)
    {
        eprintln!("  {} {warning}", style::yellow("warn"));
    }
    Ok(slice)
}

/// The `period` line: the range that will be measured, and — when `--from`
/// read bars back to settle the chains — how many bars went into that.
///
/// Naming the warm-up here is what keeps the console honest about the
/// difference between "this run saw 900 bars" and "this run is *scored* on 700
/// of them", which is otherwise invisible.
fn period_field(sliced: &Sliced) -> String {
    let (start, end, bars) = (sliced.start(), sliced.end(), sliced.evaluated_bars());
    match sliced.warmup {
        0 => format!("{start} → {end} ({bars} bars)"),
        w => format!("{start} → {end} ({bars} bars, {w} warm-up)"),
    }
}

/// A run's bar stream after `--from` / `--until`.
///
/// The warm-up prefix stays *attached* to both halves:
/// [`backtest::run_iteration_resumable`] splits it off again using
/// `EvalContext::warmup_bars`, so the labels and the snapshots cannot drift
/// out of alignment on the way there.
struct Sliced {
    bars: Vec<String>,
    snapshots: Vec<fugazi::types::Snapshot<Symbol>>,
    /// Leading bars that warm the chains without being measured.
    warmup: usize,
}

impl Sliced {
    /// The labels that will actually be measured.
    fn evaluated(&self) -> &[String] {
        &self.bars[self.warmup.min(self.bars.len())..]
    }

    fn start(&self) -> &str {
        self.evaluated().first().map_or("", |s| s.as_str())
    }

    fn end(&self) -> &str {
        self.evaluated().last().map_or("", |s| s.as_str())
    }

    fn evaluated_bars(&self) -> usize {
        self.evaluated().len()
    }
}

/// The one place `--from` / `--until` is applied, shared by every shape's
/// runner: resolve the range, record the warm-up depth on `inputs`, and hand
/// back the bars to drive.
///
/// Without either bound this is a move and two comparisons — the unsliced run
/// keeps its single resident copy of the stream.
fn sliced_inputs(
    spec: &StrategySpec,
    bars: Vec<String>,
    snapshots: Vec<fugazi::types::Snapshot<Symbol>>,
    inputs: &mut EvalContext,
    opts: &RunOptions,
) -> Result<Sliced> {
    let slice = resolve_slice(spec, &bars, &snapshots, opts)?;
    let warmup = slice.warmup_bars();
    inputs.warmup_bars = (warmup > 0).then_some(warmup);
    if slice.is_everything(bars.len()) {
        return Ok(Sliced {
            bars,
            snapshots,
            warmup,
        });
    }
    let fed = slice.fed();
    Ok(Sliced {
        bars: bars[fed.clone()].to_vec(),
        snapshots: snapshots[fed].to_vec(),
        warmup,
    })
}

/// Inner-join the two legs' atom streams on their `time` label. Returns
/// `(times, left_atoms, right_atoms)` where index `i` corresponds to a bar
/// present in both legs. Each `atoms(...)` slice is sorted ascending by
/// `time` (by construction — `DataFrame::atoms` walks a `BTreeMap`), so a
/// simple two-cursor merge suffices.
fn join_pair_by_time(
    left: &[(String, Atom)],
    right: &[(String, Atom)],
) -> (Vec<String>, Vec<Atom>, Vec<Atom>) {
    let (mut times, mut ls, mut rs) = (Vec::new(), Vec::new(), Vec::new());
    let (mut i, mut j) = (0, 0);
    while i < left.len() && j < right.len() {
        match left[i].0.cmp(&right[j].0) {
            std::cmp::Ordering::Equal => {
                times.push(left[i].0.clone());
                ls.push(left[i].1.clone());
                rs.push(right[j].1.clone());
                i += 1;
                j += 1;
            }
            std::cmp::Ordering::Less => i += 1,
            std::cmp::Ordering::Greater => j += 1,
        }
    }
    (times, ls, rs)
}

fn print_pairs_inputs_block(
    opts: &RunOptions,
    left: &str,
    right: &str,
    sliced: &Sliced,
    costs_active: bool,
) {
    style::print_section("inputs");
    style::field("strategy", opts.strategy_label);
    style::field("pair", &format!("{left} / {right}"));
    style::field("params", opts.params);
    style::field("period", &period_field(sliced));
    style::field("capital", &format!("{:.2}", opts.cash));
    if costs_active {
        style::field("costs", "active (commission/spread/slippage applied)");
    } else if opts.costs_supplied {
        style::field("costs", "none (explicit)");
    }
    style::field("output", &opts.out_dir.display().to_string());
}

// ---------------------------------------------------------------------------
// CSV writers
// ---------------------------------------------------------------------------

/// Write `fills.csv` from an [`IterationResult`]: one row per wallet-booked
/// fill in the order the wallet booked them. `commission` is only present
/// when the iteration's costs were active. `requested_units` sits beside `units`
/// on every row: equal to it on a fill taken at face value, larger on one the
/// wallet fitted to available cash or to the account's leverage.
fn write_fills_csv(iter: &IterationResult, path: &Path) -> Result<()> {
    let mut w = writer(path)?;
    // `requested_units` sits beside `units` and is always written — never
    // conditioned on "was anything scaled", because under any positive cost
    // model an all-in sheds a sliver to make room for commission and that
    // predicate is true on essentially every costed run. Its *value* is what
    // carries the signal, and how large a gap counts as material is the reader's
    // call (see `MATERIALLY_FITTED` for the one this CLI makes).
    let mut header: Vec<&str> = vec![
        "time",
        "symbol",
        "side",
        "units",
        "requested_units",
        "price",
        "kind",
    ];
    if iter.costs_active {
        header.push("commission");
    }
    w.write_record(&header)?;
    for fill in &iter.report.fills {
        let order = &fill.order;
        let time = &iter.bars[fill.bar];
        let side = match order.side {
            Side::Buy => "buy",
            Side::Sell => "sell",
        };
        let kind = match order.kind {
            OrderKind::Market => "market",
            OrderKind::Stop => "stop",
            OrderKind::TakeProfit => "take_profit",
            OrderKind::Limit => "limit",
            OrderKind::Liquidation => "liquidation",
        };
        let mut row: Vec<String> = vec![
            time.clone(),
            order.symbol.to_string(),
            side.to_string(),
            order.units.to_string(),
            order.requested_units.to_string(),
            order.price.to_string(),
            kind.to_string(),
        ];
        if iter.costs_active {
            row.push(order.commission.to_string());
        }
        w.write_record(&row)?;
    }
    w.flush()?;
    Ok(())
}

/// Write `trades.csv` from an [`IterationResult`]: one row per closed
/// round-trip leg reconstructed from the fill blotter by
/// [`fugazi::metrics::reconstruct_trades`] (same reduction the metrics
/// document uses, so row count matches `trades.total`). A run that never
/// closes a position (e.g. buy-and-hold) produces a header-only file.
fn write_trades_csv(iter: &IterationResult, path: &Path) -> Result<()> {
    let mut w = writer(path)?;
    w.write_record([
        "entry_time",
        "exit_time",
        "side",
        "units",
        "entry_price",
        "exit_price",
        "pnl",
        "return",
        "bars_held",
    ])?;
    for trade in fugazi::metrics::reconstruct_trades(&iter.report.fills) {
        let side = match trade.side {
            Side::Buy => "long",
            Side::Sell => "short",
        };
        w.write_record([
            iter.bars[trade.entry_bar].as_str(),
            iter.bars[trade.exit_bar].as_str(),
            side,
            &trade.units.to_string(),
            &trade.entry_price.to_string(),
            &trade.exit_price.to_string(),
            &trade.pnl.to_string(),
            &trade.return_ratio.to_string(),
            &trade.bars_held().to_string(),
        ])?;
    }
    w.flush()?;
    Ok(())
}

/// Write `returns.csv` from an [`IterationResult`].
fn write_returns_csv(iter: &IterationResult, path: &Path) -> Result<()> {
    let mut w = writer(path)?;
    w.write_record(["time", "equity", "return"])?;
    let per_bar =
        fugazi::metrics::per_bar_returns(&iter.report.equity_curve, iter.report.initial_equity);
    for (i, time) in iter.bars.iter().enumerate() {
        let equity = iter.report.equity_curve[i];
        let ret = per_bar[i];
        w.write_record([time.as_str(), &equity.to_string(), &ret.to_string()])?;
    }
    w.flush()?;
    Ok(())
}

/// Echo each fill of `iter` to the console — one line per row, matching
/// the `fills.csv` order. Prints a dim header row first so the columns are
/// self-labelling.
fn stream_fills(iter: &IterationResult) {
    // No header for an empty stream — a bare "fills" title makes it obvious.
    if iter.report.fills.is_empty() {
        return;
    }
    let symbol_w = iter
        .report
        .fills
        .iter()
        .map(|f| f.order.symbol.len())
        .max()
        .unwrap_or(6)
        .max(6);
    let costed = iter.costs_active;
    let header = if costed {
        format!(
            "  {time_col:<10}  {sym_col:<symbol_w$}  {side_col:<4}  {units_col:>10}  {price_col:>8}  {kind_col:<8}  {fee_col}",
            time_col = "time",
            sym_col = "symbol",
            side_col = "side",
            units_col = "units",
            price_col = "price",
            kind_col = "kind",
            fee_col = "fee",
        )
    } else {
        format!(
            "  {time_col:<10}  {sym_col:<symbol_w$}  {side_col:<4}  {units_col:>10}  {price_col:>8}  {kind_col}",
            time_col = "time",
            sym_col = "symbol",
            side_col = "side",
            units_col = "units",
            price_col = "price",
            kind_col = "kind",
        )
    };
    println!("{}", style::dim(&header));
    for fill in &iter.report.fills {
        let order = &fill.order;
        let time = &iter.bars[fill.bar];
        let side_txt = match order.side {
            Side::Buy => "buy",
            Side::Sell => "sell",
        };
        let kind = match order.kind {
            OrderKind::Market => "market",
            OrderKind::Stop => "stop",
            OrderKind::TakeProfit => "take_profit",
            OrderKind::Limit => "limit",
            OrderKind::Liquidation => "liquidation",
        };
        let side_padded = format!("{side_txt:<4}");
        let side_colored = match order.side {
            Side::Buy => style::green(&side_padded),
            Side::Sell => style::red(&side_padded),
        };
        if costed {
            println!(
                "  {}  {:<symbol_w$}  {side_colored}  {:>10.4}  {:>8.2}  {:<8}  {:.4}",
                style::dim(time),
                order.symbol,
                order.units,
                order.price,
                style::dim(kind),
                order.commission,
            );
        } else {
            println!(
                "  {}  {:<symbol_w$}  {side_colored}  {:>10.4}  {:>8.2}  {}",
                style::dim(time),
                order.symbol,
                order.units,
                order.price,
                style::dim(kind),
            );
        }
    }
}

/// Emit a windowed-metrics CSV to `path`: one row per window —
/// `window_start` / `window_end` (the times of the window's first and last
/// bars) followed by the full metric catalogue, one column per dotted
/// `metrics.yml` name. A metric that is degenerate in a window (no trades,
/// zero variance, …) is an empty cell there. Shared between the
/// non-overlapping (`metrics.csv`) and rolling (`rolling.csv`) writes.
///
/// `dsr_context = Some((n_trials, trial_variance))` appends a trailing
/// `selection.deflated_sharpe` column — the per-window DSR against the windows treated
/// as the trial population (see [`metrics::windows_dsr_context`] for the
/// caveats). Wired for `metrics.csv` only; `rolling.csv` passes `None`
/// because its heavy autocorrelation makes the trial-variance model unsound.
fn write_windowed_csv(
    windows: &[metrics::WindowMetrics],
    bars: &[String],
    dsr_context: Option<(usize, Real)>,
    path: &Path,
) -> Result<()> {
    let mut out = writer(path)?;
    let names = windows
        .first()
        .map(|w| metrics::flatten(&w.metrics))
        .unwrap_or_default();
    let mut header: Vec<String> = ["window_start", "window_end"]
        .into_iter()
        .map(String::from)
        .chain(names.iter().map(|(name, _)| (*name).to_string()))
        .collect();
    if dsr_context.is_some() {
        header.push("selection.deflated_sharpe".to_string());
    }
    out.write_record(&header)?;
    for window in windows {
        let mut record = vec![bars[window.start_bar].clone(), bars[window.end_bar].clone()];
        record.extend(
            metrics::flatten(&window.metrics)
                .into_iter()
                .map(|(_, value)| value.map(|v| v.to_string()).unwrap_or_default()),
        );
        if let Some((n_trials, trial_var)) = dsr_context {
            let m = &window.metrics;
            let dsr = fugazi::metrics::deflated_sharpe_from_stats(
                m.risk_adjusted.sharpe,
                m.returns.skewness,
                m.returns.kurtosis,
                m.run.bars,
                m.run.bars_per_year,
                n_trials,
                trial_var,
            );
            record.push(dsr.map(|v| v.to_string()).unwrap_or_default());
        }
        out.write_record(&record)?;
    }
    out.flush()?;
    Ok(())
}

/// A `,`-delimited CSV writer at `path`.
fn writer(path: &Path) -> Result<csv::Writer<std::fs::File>> {
    csv::WriterBuilder::new()
        .delimiter(b',')
        .from_path(path)
        .with_context(|| format!("creating `{}`", path.display()))
}

// ---------------------------------------------------------------------------
// Console blocks (single-run mode)
// ---------------------------------------------------------------------------

/// The "inputs" block: what this run was given. Timing (start/finish) lives
/// in the result block, since it's not an input.
fn print_inputs_block(opts: &RunOptions, sliced: &Sliced, costs_active: bool) {
    style::print_section("inputs");
    style::field("strategy", opts.strategy_label);
    style::field("params", opts.params);
    style::field("period", &period_field(sliced));
    style::field("capital", &format!("{:.2}", opts.cash));
    if costs_active {
        style::field("costs", "active (commission/spread/slippage applied)");
    } else if opts.costs_supplied {
        style::field("costs", "none (explicit)");
    }
    style::field("output", &opts.out_dir.display().to_string());
}

/// The post-run "this account was wiped out" banner.
///
/// Ruin has to be *stated*, not inferred. Everything downstream of it is
/// technically well-formed — a `-100%` return, a 100% drawdown, a flat tail on
/// the equity curve — and a reader skimming the metrics block has no single
/// number that says the run ended early because there was no money left.
fn print_ruin_warning<Sym>(report: &fugazi::RunReport<Sym>) {
    let Some(bar) = report.ruin_bar else { return };
    let bars = report.equity_curve.len();
    style::print_warns(&[format!(
        "ruined at bar {bar} of {bars} — equity reached zero, the book was \
         liquidated there and nothing traded afterwards. Every metric below \
         describes the run up to that point: total return is -100%, max \
         drawdown is 100%, and the equity curve is flat from bar {bar} on",
    )]);
}

/// The post-run "orders were refused" banner.
///
/// Unlike the top-of-run warnings this can only be known after the fact, so it
/// prints between the run and the trades block. A rejection means the run did
/// not trade the way the strategy asked — most often an entry sized beyond
/// available funds, or a protective stop that could not be booked — so the
/// metrics below describe a different strategy than the one specified. Grouped
/// by reason so a systematic problem reads as one line rather than N.
fn print_rejection_warning<Sym>(report: &fugazi::RunReport<Sym>) {
    if report.rejections.is_empty() {
        return;
    }
    let n = report.rejections.len();
    let mut counts: Vec<(String, usize)> = Vec::new();
    for r in &report.rejections {
        let key = format!("{} ({})", r.rejection.error, kind_label(r.rejection.kind));
        match counts.iter_mut().find(|(k, _)| *k == key) {
            Some((_, c)) => *c += 1,
            None => counts.push((key, 1)),
        }
    }
    let detail = counts
        .into_iter()
        .map(|(reason, count)| format!("{count}x {reason}"))
        .collect::<Vec<_>>()
        .join("; ");
    style::print_warns(&[format!(
        "{n} order{} refused by the wallet — the equity curve and metrics below \
         reflect trades that did not happen as specified: {detail}",
        if n == 1 { " was" } else { "s were" },
    )]);
}

/// How far short of its requested magnitude a fill has to land before the
/// banner below calls it material.
///
/// There has to be a threshold, and it has to be here rather than in the
/// wallet. *Any* reduction means the fill was bound by cash or by leverage, so
/// an all-in under any positive commission model lands a hair under 1.0 on
/// every single trade — a counter that fired on "not exactly what was asked"
/// would report every costed run as one long anomaly. 1% is comfortably above
/// the room a commission needs and far below a request that was scaled to a
/// fraction of itself, which is the case worth a line of output.
const MATERIALLY_FITTED: f64 = 0.99;

/// The post-run "orders were scaled down" banner.
///
/// The other half of [`print_rejection_warning`]: an order the wallet *fitted*
/// to the account rather than refusing. It is not a rejection — the fill
/// happened — but past a point it is not the trade the document asked for
/// either, and it used to be invisible at every layer. A `sizing:` above what
/// the account's leverage allows, or a rotation whose funding leg did not
/// arrive, both surface here.
fn print_fitted_warning<Sym>(report: &fugazi::RunReport<Sym>) {
    let fitted: Vec<f64> = report
        .fills
        .iter()
        .map(|f| f.order.fill_ratio())
        .filter(|ratio| *ratio < MATERIALLY_FITTED)
        .collect();
    if fitted.is_empty() {
        return;
    }
    let n = fitted.len();
    let worst = fitted.iter().copied().fold(f64::INFINITY, f64::min);
    style::print_warns(&[format!(
        "{n} fill{} scaled down to fit the account — the size traded was not the          size the document asked for (smallest: {:.1}% of the request). Compare          `units` against `requested_units` in fills.csv; raising the wallet's          leverage is what lifts the ceiling",
        if n == 1 { " was" } else { "s were" },
        worst * 100.0,
    )]);
}

/// The post-run "the account was closed out" banner.
///
/// A margin call is not a strategy decision, and a run that reports one has
/// answered a different question than the one the document asked: everything
/// after the first liquidation is the strategy trading an account that a real
/// venue had already taken away from it once. That has to be a headline rather
/// than a `kind` column somebody might read.
fn print_liquidation_warning<Sym>(report: &fugazi::RunReport<Sym>) {
    let bars: Vec<usize> = report
        .fills
        .iter()
        .filter(|f| f.order.kind == fugazi::OrderKind::Liquidation)
        .map(|f| f.bar)
        .collect();
    if bars.is_empty() {
        return;
    }
    // Legs of one margin call share a bar; count events, not fills.
    let mut events = 1;
    for pair in bars.windows(2) {
        if pair[1] != pair[0] {
            events += 1;
        }
    }
    let first = bars[0];
    style::print_warns(&[format!(
        "the account hit its maintenance margin and was force-closed {events} time{} \
         (first at bar {first} of {}) — every bar after that is a strategy trading an \
         account a real venue would already have closed out. The legs are the \
         `liquidation` rows in fills.csv",
        if events == 1 { "" } else { "s" },
        report.equity_curve.len(),
    )]);
}

/// Human label for an [`OrderKind`] inside the rejection banner.
fn kind_label(kind: fugazi::OrderKind) -> &'static str {
    match kind {
        fugazi::OrderKind::Market => "market",
        fugazi::OrderKind::Stop => "stop",
        fugazi::OrderKind::TakeProfit => "take-profit",
        fugazi::OrderKind::Limit => "limit",
        fugazi::OrderKind::Liquidation => "liquidation",
    }
}

/// The "result" block: the run's outputs, then its wall-clock timing.
fn print_result_block(opts: &RunOptions, s: &Summary, started: SystemTime, finished: SystemTime) {
    println!();
    style::print_section("result");
    style::field("bars", &s.bars.to_string());
    style::field("fills", &s.fills.to_string());
    let delta = s.final_equity - opts.cash;
    let change = format!("{delta:+.2}, {:+.2}%", s.return_pct);
    let change = if delta >= 0.0 {
        style::green(&change)
    } else {
        style::red(&change)
    };
    style::field(
        "capital",
        &format!("{:.2} → {:.2}  ({change})", opts.cash, s.final_equity),
    );
    let elapsed = finished.duration_since(started).unwrap_or_default();
    style::field("started", &style::format_utc(started));
    style::field(
        "finished",
        &format!(
            "{} ({})",
            style::format_utc(finished),
            style::format_elapsed(elapsed)
        ),
    );
}

/// The "metrics" block: a compact summary of `metrics.yml`'s headline
/// figures. When `gross` is set (a costed run), decision-relevant rows also
/// print their gross twin so the cost drag is one line away. When
/// `bar_freq` is known, the `holding` line prints each bar count with a
/// duration twin in the bar cadence's own unit alphabet (`21d`, `4h`).
fn print_metrics_block(
    m: &metrics::Metrics,
    measured: Option<&str>,
    gross: Option<&metrics::Metrics>,
    bar_freq: Option<Frequency>,
) {
    println!();
    style::print_section("metrics");
    if let Some(measured) = measured {
        style::field("measured", measured);
    }
    if let Some(g) = gross {
        let net = m
            .returns
            .cagr_pct
            .map_or("—".to_string(), |v| format!("{v:+.2}%"));
        let gross = g
            .returns
            .cagr_pct
            .map_or("—".to_string(), |v| format!("{v:+.2}%"));
        style::field("cagr", &format!("net {net} · gross {gross}"));
    }
    style::field(
        "return",
        &format!(
            "{:+.2}% ann · vol {:.2}%",
            m.returns.annualized_mean_pct, m.returns.annualized_volatility_pct
        ),
    );
    if let Some(g) = gross {
        let net = format_ratio(m.risk_adjusted.sharpe);
        let gross = format_ratio(g.risk_adjusted.sharpe);
        style::field("sharpe", &format!("net {net} · gross {gross}"));
    } else {
        style::field("sharpe", &format_ratio(m.risk_adjusted.sharpe));
    }
    style::field("sortino", &format_ratio(m.risk_adjusted.sortino));
    style::field("omega", &format_ratio(m.risk_adjusted.omega));
    style::field(
        "max_dd",
        &format!(
            "{:.2}% ({} bars)",
            m.drawdown.max_pct, m.drawdown.max_duration_bars
        ),
    );
    style::field("exposure", &format!("{:.1}%", m.trades.exposure_pct));
    style::field(
        "trades",
        &format!(
            "{} · win {} · pf {}",
            m.trades.total,
            format_pct(m.trades.win_rate_pct),
            format_ratio(m.trades.profit_factor),
        ),
    );
    if let Some(text) = format_holding_line(m, bar_freq) {
        style::field("holding", &text);
    }
}

/// Compose the `holding` line: `avg N bars (~Xu) · min N (~Xu) · max N (~Xu)`,
/// the duration twin dropped when `bar_freq` is unknown. `None` when the run
/// booked no trades (all three legs are absent).
///
/// When min, max, and avg coincide (one closed trade, or every trade held the
/// exact same number of bars), collapses to a single `N bars (~Xu)` — no point
/// showing three identical numbers.
fn format_holding_line(m: &metrics::Metrics, bar_freq: Option<Frequency>) -> Option<String> {
    let avg = m.trades.average_bars;
    let min = m.trades.min_bars.map(|n| n as Real);
    let max = m.trades.max_bars.map(|n| n as Real);
    if avg.is_none() && min.is_none() && max.is_none() {
        return None;
    }
    let bars_str = |bars: Real, precision: usize| -> String {
        let dur = bar_freq
            .map(|f| format!(" (~{})", format_bars_as_duration(bars, f)))
            .unwrap_or_default();
        format!("{bars:.*} bars{dur}", precision)
    };
    // Collapse to a single value when the three legs coincide (either one
    // trade, or every trade held the exact same number of bars). Uses a
    // 1e-6 tolerance since `avg` is a Real from a running mean.
    if let (Some(avg), Some(min), Some(max)) = (avg, min, max)
        && (avg - min).abs() < 1e-6
        && (avg - max).abs() < 1e-6
    {
        let precision = if avg.fract().abs() < 1e-6 { 0 } else { 1 };
        return Some(bars_str(avg, precision));
    }
    let leg = |label: &str, bars: Option<Real>, precision: usize| -> Option<String> {
        Some(format!("{label} {}", bars_str(bars?, precision)))
    };
    let parts: Vec<String> = [leg("avg", avg, 1), leg("min", min, 0), leg("max", max, 0)]
        .into_iter()
        .flatten()
        .collect();
    Some(parts.join(" · "))
}

/// Render `bars` bars of `freq` cadence as a duration in the cadence's own
/// unit alphabet (`21d`, `4h`, `26h` — `Frequency::from_str`-compatible for
/// integer counts). Fractional averages carry one decimal.
fn format_bars_as_duration(bars: Real, freq: Frequency) -> String {
    let (mult, letter) = match freq {
        Frequency::Minute(n) => (n, "m"),
        Frequency::Hour(n) => (n, "h"),
        Frequency::Day(n) => (n, "d"),
        Frequency::Week(n) => (n, "w"),
        Frequency::Month(n) => (n, "M"),
    };
    let total = bars * mult as Real;
    if (total - total.round()).abs() < 1e-6 {
        format!("{total:.0}{letter}")
    } else {
        format!("{total:.1}{letter}")
    }
}

fn format_ratio(v: Option<Real>) -> String {
    v.map_or_else(|| "—".to_string(), |r| format!("{r:.2}"))
}

fn format_pct(v: Option<Real>) -> String {
    v.map_or_else(|| "—".to_string(), |r| format!("{r:.1}%"))
}

/// Printed right after [`print_metrics_block`] under `-w`: each headline stat
/// becomes the cross-window `mean ± std` over the non-overlapping N-bar rows
/// in `metrics.csv`, so the caller sees both the whole-run single estimate
/// and the windowed dispersion around it side-by-side. Same field set and
/// layout as the whole-run block. Windows where a ratio is degenerate (no
/// losing trade for a profit factor, zero variance for Sharpe, …) are dropped
/// from that stat's aggregation via the `Option` filter — a stat with fewer
/// than one defined window prints as `—`.
///
/// No net-vs-gross split under `-w`: the pipeline currently only windows the
/// priced run, and printing whole-run gross next to windowed-net numbers would
/// mix aggregation scopes.
fn print_windowed_metrics_block(windows: &[metrics::WindowMetrics]) {
    println!();
    style::print_section("windowed metrics");
    style::field(
        "windows",
        &format!(
            "{} × {} bars (non-overlapping)",
            windows.len(),
            windows.first().map_or(0, |w| w.metrics.run.bars),
        ),
    );
    let ann_mean = mean_std_of(windows, |m| Some(m.returns.annualized_mean_pct));
    let ann_vol = mean_std_of(windows, |m| Some(m.returns.annualized_volatility_pct));
    style::field(
        "return",
        &format!(
            "{} ann · vol {}",
            format_ms_signed_pct(ann_mean),
            format_ms_unsigned_pct(ann_vol),
        ),
    );
    style::field(
        "sharpe",
        &format_ms_ratio(mean_std_of(windows, |m| m.risk_adjusted.sharpe)),
    );
    style::field(
        "sortino",
        &format_ms_ratio(mean_std_of(windows, |m| m.risk_adjusted.sortino)),
    );
    style::field(
        "omega",
        &format_ms_ratio(mean_std_of(windows, |m| m.risk_adjusted.omega)),
    );
    let max_dd = mean_std_of(windows, |m| Some(m.drawdown.max_pct));
    let max_dur = mean_std_of(windows, |m| Some(m.drawdown.max_duration_bars as Real));
    style::field(
        "max_dd",
        &format!(
            "{} ({} bars)",
            format_ms_unsigned_pct(max_dd),
            format_ms_count(max_dur, 0),
        ),
    );
    style::field(
        "exposure",
        &format_ms_unsigned_pct(mean_std_of(windows, |m| Some(m.trades.exposure_pct))),
    );
    let trades = mean_std_of(windows, |m| Some(m.trades.total as Real));
    let win_rate = mean_std_of(windows, |m| m.trades.win_rate_pct);
    let pf = mean_std_of(windows, |m| m.trades.profit_factor);
    style::field(
        "trades",
        &format!(
            "{} · win {} · pf {}",
            format_ms_count(trades, 1),
            format_ms_unsigned_pct(win_rate),
            format_ms_ratio(pf),
        ),
    );
}

/// Project `f` across each window's `Metrics`, drop `None`s, and reduce to
/// `(mean, population_std)` via [`metrics::mean_std`]. `None` when no window
/// defines the stat.
fn mean_std_of<F>(windows: &[metrics::WindowMetrics], f: F) -> Option<(Real, Real)>
where
    F: Fn(&metrics::Metrics) -> Option<Real>,
{
    metrics::mean_std(windows.iter().filter_map(|w| f(&w.metrics)))
}

/// `+M.MM ± S.SS%` — signed mean (returns can be negative), unsigned stddev,
/// unit suffix once at the end.
fn format_ms_signed_pct(pair: Option<(Real, Real)>) -> String {
    pair.map_or_else(|| "—".to_string(), |(m, s)| format!("{m:+.2} ± {s:.2}%"))
}

/// `M.MM ± S.SS%` — unsigned mean (magnitudes, ratios in percent form).
fn format_ms_unsigned_pct(pair: Option<(Real, Real)>) -> String {
    pair.map_or_else(|| "—".to_string(), |(m, s)| format!("{m:.2} ± {s:.2}%"))
}

/// `M.MM ± S.SS` — unitless ratio (Sharpe, Sortino, Omega, profit factor).
fn format_ms_ratio(pair: Option<(Real, Real)>) -> String {
    pair.map_or_else(|| "—".to_string(), |(m, s)| format!("{m:.2} ± {s:.2}"))
}

/// `M ± S` at `precision` decimals — for counts (trades, drawdown duration
/// bars) treated as floats so a fractional mean survives the format.
fn format_ms_count(pair: Option<(Real, Real)>, precision: usize) -> String {
    pair.map_or_else(
        || "—".to_string(),
        |(m, s)| format!("{m:.*} ± {s:.*}", precision, precision),
    )
}
