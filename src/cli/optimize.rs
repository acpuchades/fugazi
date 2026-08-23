//! CLI wrapper for the `optimize` subcommand.
//!
//! Argument marshaling, DataFrame joining, CSV output, and console styling.
//! The pure sweep kernel — `optimize()`, walkforward layout, ranking, `Sweep` /
//! `Row` / `Evaluation` / `Subgrid` types — lives in `fugazi::spec::optimize`.

use fugazi::types::Symbol;
use std::collections::{BTreeSet, HashMap};
use std::num::NonZeroUsize;
use std::path::Path;
use std::str::FromStr;
use std::time::SystemTime;

use anyhow::{Context, Result, bail};
use fugazi::prelude::*;
use serde_json::Value;

use crate::backtest;
use crate::calendar::{
    self, AssetClass, BarsPerYearSpec, Frequency, ScopedFrequency, WalkForwardSpec, WindowSpec,
};
use crate::costs::CostConfig;
use crate::data::DataFrame;
use crate::daterange::{self, Slice};
use crate::imports;
use crate::input;
use crate::input::StrategyKind;
use crate::metrics;
use crate::overlap;
use crate::run::{attach_read_series, join_universe_by_time, read_only_series};
use crate::style;

use fugazi::spec::pairs::PairsStrategySpec;

// Kernel imports from the library — types, ranking, walk-forward layout.
// Re-exported publicly so `crate::optimize::reject_axes_in_params` (called
// from `main.rs`) and other library-side items keep resolving through this
// module.
pub use fugazi::spec::optimize::{
    AxisScale, ColumnPos, Direction, Evaluation, PLATEAU_TOLERANCE, Row, SmoothKernel,
    SmoothScales, Smoothing, Subgrid, Sweep, build_any_spec, build_spec, build_typed, cartesian,
    combine_params, format_number, format_value, lookup, lookup_windowed, mean_std_of, optimize,
    probe_params, rank_positions, ranking_value, reject_axes_in_params, row_dsr_inputs, split_axes,
};

/// Threaded-in inputs, same shape as [`crate::run::RunOptions`].
pub struct OptimizeOptions<'a> {
    pub cash: Real,
    /// Most gross notional each grid point's account may hold, as a multiple of
    /// equity (`--max-gross`; `1.0` is unlevered).
    pub max_gross: Real,
    /// Annualized interest on a negative cash balance (`--margin-rate`).
    pub margin_rate: Real,
    /// Maintenance-margin ratio, or `None` for no margin call
    /// (`--maintenance-margin`).
    pub maintenance_margin: Option<Real>,
    /// The shape of the strategy YAML being swept. Single-asset is the
    /// legacy path (one symbol probed from the spec + one atom slice from
    /// the frame). Pairs probes `[left, right]` from the first subgrid
    /// (every other subgrid must resolve to the same pair) and joins those
    /// two atom streams into snapshots. Basket / multi-asset take the union
    /// of every symbol in the frame as universe and time-align the
    /// per-symbol streams — same shape as `run_basket` / `run_multi`.
    pub strategy_kind: StrategyKind,
    pub strategy_text: &'a str,
    /// The directory the strategy's `!import` paths resolve against — its own
    /// directory when loaded from `@file`, the working directory for inline
    /// text (see [`crate::input::Source::base_dir`]). Imports are spliced once,
    /// into the base value every grid point is then `!param`-substituted from.
    pub strategy_dir: &'a Path,
    pub strategy_label: &'a str,
    /// `--params` baseline: shared scalars applied under every subgrid. Axes
    /// are rejected upstream via [`reject_axes_in_params`] — this table is
    /// scalar-only by the time it reaches [`run`].
    pub params_table: HashMap<String, Value>,
    /// One folded table per `--grid` flag, in flag order. Each may hold both
    /// scalars (fixed within the subgrid) and axis-shaped values (JSON arrays
    /// or `a..b[:c]` range strings). Layered over `params_table` per subgrid
    /// — a subgrid entry with the same name as a `--params` scalar overrides
    /// it for that subgrid's points.
    pub grid_tables: Vec<HashMap<String, Value>>,
    /// The `-m/--metrics` names to emit as CSV columns.
    pub metrics: Vec<String>,
    /// The `--best-by` metric name to sort by (empty = no sort).
    pub best_by: Option<String>,
    pub output: &'a Path,
    /// `--bars-per-year` entries: each is a plain `N` or a `SYMBOL[FREQ]:N`
    /// override. Same resolution rules as `run` — see
    /// [`crate::calendar::pick_bars_per_year`].
    pub bars_per_year: &'a [BarsPerYearSpec],
    /// Trading-calendar shortcut (`--stocks`/`--forex`/`--crypto`).
    pub asset_class: Option<AssetClass>,
    pub risk_free_rate: Real,
    /// Evaluate each grid point in non-overlapping windows of this size (same
    /// windowing as `run -w`): every `-m` metric becomes two CSV columns
    /// (`<name>_mean` / `<name>_std`, cross-window over the windows where the
    /// metric is defined) and `--best-by` ranks by the windowed mean. The raw
    /// CLI spec — a bar count or a duration; resolved to a bar count against
    /// the trading calendar inside [`run`] (duration form requires
    /// `asset_class` and a resolvable bar cadence).
    pub windowed: Option<WindowSpec>,
    /// `--walkforward IS,OS[,Embargo]`: rolling walk-forward optimization. When
    /// set, [`run`] takes the walk-forward branch (dispatched into
    /// [`walkforward`]) instead of the plain grid sweep — mutually exclusive
    /// with `windowed` (enforced at clap parse time).
    pub walkforward: Option<WalkForwardSpec>,
    /// `--keep-unstable`: under `--walkforward`, skip only the grid-wide
    /// `max(warm_up_bars)` at the head of the atom slice, not
    /// `max(stable_bars)`. Lets IIR settling bleed into the first IS window.
    /// No-op without `walkforward`.
    pub keep_unstable: bool,
    /// `-k/--risk-aversion`: shift each grid point's `--best-by` cross-window
    /// mean *against* it by this many standard deviations before ranking
    /// (direction-aware: `mean − k·std` descending, `mean + k·std` ascending).
    /// `0.0` = rank by the plain mean. Only meaningful with `windowed`.
    pub risk_aversion: Real,
    /// `--smooth` / `--smooth-min-support`: rank `--best-by` by a kernel-
    /// weighted average of each grid point's ranking key over its *parameter
    /// neighbourhood*, so a broad plateau outranks a lone spike. `None` when
    /// `--smooth` wasn't passed — the sweep then ranks on the point estimate
    /// exactly as before. Composes with `risk_aversion`, which is folded into
    /// the key before it is smoothed. See [`fugazi::spec::optimize::smooth_keys`].
    pub smoothing: Option<Smoothing>,
    /// Cost model configured via `--costs`. Every grid point resolves against
    /// the same config for its (strategy symbol, frequency) pair.
    pub cost_config: &'a CostConfig,
    /// `-f/--frequency` entries: plain `CODE` or `SYMBOL:CODE`. The
    /// symbol-matching entry wins, else auto-detection from the strategy's
    /// dominant series in the frame. The resulting effective freq is
    /// forwarded to [`CostConfig::resolve`] per grid point, so freq-scoped
    /// cost entries also see the detected value.
    pub frequency: &'a [ScopedFrequency],
    /// Whether the user passed at least one `--costs` flag — governs the
    /// warning banner.
    pub costs_supplied: bool,
    pub jobs: Option<usize>,
    pub quiet: bool,
    /// `--from` / `--until` / `--strict-from`: which bars the sweep evaluates.
    /// `None` when neither bound was given. See [`crate::daterange`].
    pub range: Option<daterange::DateRange>,
    /// The `--from` value as spelled, for messages that quote it back.
    pub from_label: Option<&'a str>,
}

/// A synthetic one-bar snapshot carrying every universe symbol.
///
/// Basket / multi strategies build their per-symbol chains on first sight of a
/// symbol, so a freshly-built one reports only the rebalance signal's period
/// from `stable_bars()`. Feeding this fires every factory first. The atom is a
/// zero candle with no overlays — safe because a probe never trades, only
/// exercises chain construction.
fn universe_probe_snapshot(universe: &[Symbol]) -> fugazi::types::Snapshot<Symbol> {
    let mut snap = fugazi::types::Snapshot::<Symbol>::new();
    let dummy = Atom::new(Candle::new(0.0, 0.0, 0.0, 0.0, 0.0));
    for sym in universe {
        snap.push(Some(sym.clone()), None, dummy.clone());
    }
    snap
}

/// The grid-wide `max(stable_bars)` — how far back `--from` reads to settle
/// *every* row of the sweep.
///
/// Taking the max rather than each row's own depth is what keeps a sweep's
/// answers comparable: every row then evaluates exactly the same bars, so a
/// difference between two rows is the parameters and not the window. It is the
/// same argument (and the same quantity) as the walk-forward pre-scan in
/// [`fugazi::spec::optimize::walkforward`], which fixes one grid-wide prefix
/// skip so every fold's IS/OOS ranges line up.
///
/// Serial, unlike that pre-scan: a probe builds a strategy without driving it,
/// and this runs once per sweep rather than once per fold.
fn grid_warmup_need(
    subgrids: &[Subgrid],
    probe: impl Fn(&HashMap<String, Value>) -> Result<usize>,
) -> Result<usize> {
    let mut need = 0;
    for subgrid in subgrids {
        for combo in &subgrid.combos {
            let params = combine_params(&subgrid.fixed, &subgrid.axes, combo);
            need = need.max(probe(&params)?);
        }
    }
    Ok(need)
}

/// Resolve `--from` / `--until` for a sweep over `bars`, warning if evaluation
/// had to start late.
///
/// `need` is only asked for when a read-back actually applies, so an unsliced
/// sweep never pays for the grid-wide probe.
fn optimize_slice(
    opts: &OptimizeOptions,
    bars: &[String],
    need: impl FnOnce() -> Result<usize>,
) -> Result<Slice> {
    let Some(range) = opts.range else {
        return Ok(Slice::everything(bars.len()));
    };
    let need = if range.reads_back() { need()? } else { 0 };
    let slice = range.resolve(bars, need)?;
    if let Some(label) = opts.from_label
        && let Some(warning) = daterange::short_warmup_warning(&slice, bars, label, need)
    {
        eprintln!("  {} {warning}", style::yellow("warn"));
    }
    Ok(slice)
}

/// CLI entry for the `optimize` command: marshal `opts` into inputs
/// [`optimize`] can consume (parse the strategy text, fold subgrids, resolve
/// the candle slice for the strategy's symbol), invoke the sweep, then write
pub fn run(frame: &DataFrame, opts: OptimizeOptions) -> Result<()> {
    if opts.grid_tables.is_empty() {
        bail!(
            "no --grid flag passed: at least one is required (use `run` for a single combination)"
        );
    }
    // Build one Subgrid per --grid flag by layering baseline scalars under each
    // flag's own scalars/axes. Keep grid entries taking precedence — if a
    // subgrid names the same key as --params, the subgrid wins for that
    // subgrid's rows.
    let subgrids: Vec<Subgrid> = opts
        .grid_tables
        .iter()
        .enumerate()
        .map(|(idx, grid)| {
            let mut merged = opts.params_table.clone();
            for (k, v) in grid {
                merged.insert(k.clone(), v.clone());
            }
            let (fixed, axes) =
                split_axes(&merged).with_context(|| format!("--grid #{}", idx + 1))?;
            let combos = cartesian(&axes);
            Ok::<_, anyhow::Error>(Subgrid {
                fixed,
                axes,
                combos,
            })
        })
        .collect::<Result<_>>()?;

    let total_points: usize = subgrids.iter().map(Subgrid::points).sum();
    if total_points < 2 {
        bail!(
            "the stacked grid has only {total_points} point(s): pass a `[...]` list, a \
             `start..end[:step]` range, or multiple `--grid` flags with distinct values \
             (use `run` for a single combination)"
        );
    }

    // Check `--smooth-scale`'s pins against the grid they will be applied to,
    // before any strategy is parsed or backtested: a pin that names no axis is
    // a typo the sweep would otherwise honour silently. The kernel repeats the
    // error for library callers; only here can the inert-pin warnings be
    // printed. stderr and ungated by `--quiet`, like `overlap` / `cadence`.
    if let Some(cfg) = &opts.smoothing {
        for warning in cfg.scales.validate_against(&subgrids)? {
            eprintln!("{} {warning}", style::yellow("warning:"));
        }
    }

    // Imports splice once, up front: the resulting base value is what every
    // grid point's `!param` substitution runs over, so a shared fragment costs
    // one read no matter how large the sweep.
    let base_value = input::parse_value_at(opts.strategy_text, opts.strategy_label)?;
    let base_value =
        imports::resolve(base_value, opts.strategy_dir).context("resolving strategy imports")?;

    match opts.strategy_kind {
        StrategyKind::Single => run_single(&opts, subgrids, frame, &base_value),
        StrategyKind::Pairs
        | StrategyKind::Basket
        | StrategyKind::Multi
        | StrategyKind::Portfolio => run_multi_symbol(&opts, subgrids, frame, &base_value),
    }
}

/// The symbols the swept document **reads** through an explicit
/// `!pick { symbol: … }`, probed the same way its traded symbol is.
///
/// A sweep varies `!param`s, and a `!pick` head can be one — so the base value
/// alone would miss a parameterised symbol. Each subgrid's probe point is
/// resolved and walked, mirroring how [`run_single`] / [`run_multi_symbol`]
/// probe `symbol:` / `left:` / `right:` and require every subgrid to agree.
/// A grid axis that varies the *picked* symbol across combos within one subgrid
/// is outside what the probe sees — the same limit the traded-symbol probe has,
/// and the not-in-input error below is what catches the fallout.
fn probe_reads(base_value: &Value, subgrids: &[Subgrid]) -> Result<BTreeSet<String>> {
    let mut out = BTreeSet::new();
    for subgrid in subgrids {
        let resolved =
            fugazi::spec::params::substitute(base_value.clone(), &probe_params(subgrid))?;
        out.extend(fugazi::spec::reads::picked_symbols(&resolved));
    }
    Ok(out)
}

/// A traded series a grid row can resolve to: the instrument, and the cadence
/// its `root:` declared (if it declared one).
///
/// Two rows sharing a key share a prepared snapshot stream. Keyed by both parts
/// because a cadence is a different series of the same instrument, not a
/// different reading of one stream.
#[derive(Clone, PartialEq, Eq, Hash, PartialOrd, Ord, Debug)]
struct RootKey {
    symbol: String,
    freq: Option<String>,
}

/// The root every combo of every subgrid resolves to, deduplicated.
///
/// **Every combo**, not the per-subgrid probe point: a `!param` in the root is
/// exactly the thing a sweep is meant to vary, so sampling one point per subgrid
/// would miss an axis that varies the instrument *within* a subgrid — which is
/// how that case used to end up silently backtesting the probe's bars on every
/// row. Resolving costs one param substitution and one partial parse per combo,
/// against a document already in memory.
fn distinct_roots(base_value: &Value, subgrids: &[Subgrid]) -> Result<Vec<RootKey>> {
    let mut out: BTreeSet<RootKey> = BTreeSet::new();
    for subgrid in subgrids {
        for combo in &subgrid.combos {
            let params = combine_params(&subgrid.fixed, &subgrid.axes, combo);
            out.insert(root_key_of(base_value, &params)?);
        }
    }
    Ok(out.into_iter().collect())
}

/// The [`RootKey`] one grid point resolves to.
fn root_key_of(base_value: &Value, params: &HashMap<String, Value>) -> Result<RootKey> {
    let spec = build_spec(base_value, params)?;
    Ok(RootKey {
        symbol: spec
            .root
            .sole_symbol("single-asset")
            .map_err(backtest::build_error)?,
        freq: spec.root.declared_freq().map(str::to_string),
    })
}

/// The single-asset grid path — resolves every row's root, prepares one
/// snapshot stream per distinct traded series, and drives the sweep through a
/// [`SingleStrategySpec`]-typed closure. Handles walk-forward too (which is
/// only wired for single-asset strategies).
fn run_single(
    opts: &OptimizeOptions,
    subgrids: Vec<Subgrid>,
    frame: &DataFrame,
    base_value: &Value,
) -> Result<()> {
    let started = SystemTime::now();
    // Which traded series this grid touches. More than one is a *root axis* —
    // a sweep over instruments or cadences rather than over parameters.
    let roots = distinct_roots(base_value, &subgrids)?;
    if roots.len() > 1 {
        if opts.walkforward.is_some() {
            bail!(
                "--walkforward lays folds out over one bar timeline, but this grid sweeps {} \
                 traded series ({}) — each has its own bar count, so a fold index means a \
                 different span per row. Sweep the root or walk forward, not both",
                roots.len(),
                roots
                    .iter()
                    .map(|r| format!("`{}`", r.symbol))
                    .collect::<Vec<_>>()
                    .join(", "),
            );
        }
        // Not an error, but the comparability the rest of `optimize` is built
        // around no longer holds: rows are warmed to the grid-wide maximum and
        // otherwise evaluate *their own* series' bars, so a difference between
        // two rows is no longer "the parameters and not the window".
        eprintln!(
            "  {} this grid sweeps {} traded series — rows evaluate different bars, so their \
             metrics are a batch of separate backtests rather than a like-for-like comparison",
            style::yellow("warn"),
            roots.len(),
        );
    }
    let probe_symbol = roots[0].symbol.clone();
    let series = frame.atoms(&probe_symbol)?;
    let atoms = series.atoms;
    let skipped_overlay_columns = series.skipped_columns;
    // Series the document reads but does not trade, resolved once for the whole
    // sweep — and refused here, before a single grid point runs, when one of
    // them is not in the input. `run` makes the same check; a sweep that
    // silently read `None` for a regime gate would produce a whole grid of
    // plausible zero-trade rows.
    let reads = probe_reads(base_value, &subgrids)?;
    let read_only = read_only_series(frame, &[probe_symbol.as_str()], &reads)?;

    // The effective bar cadence, now that the strategy's symbol is known, best
    // evidence first: a symbol-matching `-f/--frequency` entry, then the
    // input's own `freq` column, then detection from the atoms' `time` field
    // (populated by the loader). Threaded into both the annualization
    // (`bars_per_year`) and the per-grid-point cost resolution, so freq-scoped
    // `--costs` entries see the same cadence the calendar does.
    let effective_freq = calendar::pick_frequency(opts.frequency, &probe_symbol)
        .or_else(|| frame.declared_frequency(&probe_symbol))
        .or_else(|| calendar::detect_frequency_from_atoms(atoms.iter().map(|(_, a)| a)));
    let bars_per_year =
        match calendar::pick_bars_per_year(opts.bars_per_year, &probe_symbol, effective_freq) {
            Some(v) => v,
            None => calendar::resolve(
                None,
                opts.asset_class,
                effective_freq,
                calendar::measure_bars_per_year(atoms.iter().map(|(_, a)| a)),
            )
            .map_err(anyhow::Error::msg)?,
        };

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

    // `--from` / `--until`, resolved before the walk-forward branch splits off
    // so both drivers slice the same bars the same way.
    //
    // Walk-forward takes only the evaluated span: its own prefix skip settles
    // the chains inside that span, so handing it a warm-up prefix as well
    // would skip warm-up twice and lay the first fold out later than asked.
    // The sweep takes the prefix and warms across it.
    let sliced_schema = backtest::schema_from_atoms(&atoms);
    let bar_labels: Vec<String> = atoms.iter().map(|(t, _)| t.clone()).collect();
    let slice = optimize_slice(opts, &bar_labels, || {
        let keep_unstable = opts.keep_unstable;
        let cash = opts.cash;
        grid_warmup_need(&subgrids, |params| {
            let spec = build_any_spec(StrategyKind::Single, base_value, params)?;
            let built = spec
                .try_build(cash, &sliced_schema, None)
                .map_err(backtest::build_error)?;
            Ok(if keep_unstable {
                built.warm_up_bars()
            } else {
                built.stable_bars()
            })
        })
    })?;
    let atoms = if slice.is_everything(atoms.len()) {
        atoms
    } else if opts.walkforward.is_some() {
        atoms[slice.eval_start..slice.end].to_vec()
    } else {
        atoms[slice.fed()].to_vec()
    };
    let sweep_warmup =
        (opts.walkforward.is_none() && slice.warmup_bars() > 0).then(|| slice.warmup_bars());

    // Walk-forward branch: an independent driver — different outputs and a
    // fold-scoped per-row measurement rather than one whole-run reduction. The
    // grid loop shape is similar, but the emitted artifacts have their own
    // schema (per-fold winners + composite OOS), so we don't try to squeeze it
    // through the [`Sweep`] shape.
    if let Some(walkforward_spec) = opts.walkforward {
        let schema = backtest::schema_from_atoms(&atoms);
        let keep_unstable = opts.keep_unstable;
        let cash = opts.cash;
        let max_gross = opts.max_gross;
        let margin_rate = opts.margin_rate;
        let maintenance_margin = opts.maintenance_margin;
        let cost_config = opts.cost_config;
        let schema_ref = &schema;
        // Same lift as the sweep path: the unified measurement is
        // snapshot-shaped, so tag each bar with the strategy's symbol once.
        // Interned once: every bar's `Snapshot::single` then clones a refcount
        // rather than allocating a fresh copy of the same symbol.
        let wf_symbol = fugazi::types::symbol(&probe_symbol);
        let wf_bars: Vec<String> = atoms.iter().map(|(t, _)| t.clone()).collect();
        let mut wf_snapshots: Vec<fugazi::types::Snapshot<Symbol>> = atoms
            .iter()
            .map(|(_, a)| fugazi::types::Snapshot::single(wf_symbol.clone(), a.clone()))
            .collect();
        // Left-joined onto the traded symbol's bars — see `run::attach_read_series`.
        // The folds slice this stream by index, so a read series has to be on it
        // before the split, not per fold.
        attach_read_series(&wf_bars, &mut wf_snapshots, &read_only);
        let wf_snapshots_ref = &wf_snapshots;
        let ctx = backtest::EvalContext {
            cash,
            max_gross,
            margin_rate,
            maintenance_margin,
            bars_per_year,
            risk_free_rate: opts.risk_free_rate,
            cost_config,
            effective_freq,
            windowed: None,
            seconds_per_bar,
            mc: None,
            warmup_bars: None,
        };
        let ctx_ref = &ctx;
        let probe = |params: &HashMap<String, Value>| -> Result<usize> {
            let spec = build_any_spec(StrategyKind::Single, base_value, params)?;
            let built = spec
                .try_build(cash, schema_ref, None)
                .map_err(backtest::build_error)?;
            Ok(if keep_unstable {
                built.warm_up_bars()
            } else {
                built.stable_bars()
            })
        };
        let run_backtest = |params: &HashMap<String, Value>| -> Result<fugazi::RunReport<Symbol>> {
            let spec = build_any_spec(StrategyKind::Single, base_value, params)?;
            backtest::measured_report_any(&spec, wf_snapshots_ref, ctx_ref)
                .map_err(backtest::build_error)
        };
        return walkforward_run(
            subgrids,
            atoms.len(),
            probe,
            run_backtest,
            bars_per_year,
            opts.risk_free_rate,
            effective_freq,
            walkforward_spec,
            opts.keep_unstable,
            opts.asset_class,
            seconds_per_bar,
            &opts.metrics,
            opts.best_by.as_deref(),
            opts.smoothing.as_ref(),
            opts.output,
            opts.jobs,
            opts.quiet,
            &skipped_overlay_columns,
            opts.cash,
        );
    }

    let cost_config = opts.cost_config;
    let windowed_n = windowed_bars.map(NonZeroUsize::get);
    // The unified evaluator is snapshot-shaped, so lift the single-asset atom
    // stream once here rather than per grid row (`run_iteration` used to do
    // this inside every call).
    // Interned once — see the walk-forward branch above.
    let symbol = fugazi::types::symbol(&probe_symbol);
    let sweep_bars: Vec<String> = atoms.iter().map(|(t, _)| t.clone()).collect();
    let mut snapshots: Vec<fugazi::types::Snapshot<Symbol>> = atoms
        .iter()
        .map(|(_, a)| fugazi::types::Snapshot::single(symbol.clone(), a.clone()))
        .collect();
    attach_read_series(&sweep_bars, &mut snapshots, &read_only);
    let ctx = backtest::EvalContext {
        cash: opts.cash,
        max_gross: opts.max_gross,
        margin_rate: opts.margin_rate,
        maintenance_margin: opts.maintenance_margin,
        bars_per_year,
        risk_free_rate: opts.risk_free_rate,
        cost_config,
        effective_freq,
        windowed: windowed_bars,
        seconds_per_bar,
        mc: None,
        warmup_bars: sweep_warmup,
    };

    // One prepared stream per distinct traded series. `roots[0]` is the one
    // already built above (it drives the console's period line and the `-w`
    // resolution); the rest are prepared here, each with its **own** cadence,
    // annualization and `--from`/`--until` slice, because those are properties
    // of the series and not of the grid.
    //
    // Memoized rather than per row: a 200-point grid over two instruments
    // prepares two streams, not two hundred.
    let mut streams: HashMap<
        RootKey,
        (Vec<fugazi::types::Snapshot<Symbol>>, backtest::EvalContext),
    > = HashMap::new();
    streams.insert(roots[0].clone(), (snapshots, ctx));
    for key in roots.iter().skip(1) {
        let series = frame.atoms(&key.symbol)?;
        let other_atoms = series.atoms;
        let freq = calendar::pick_frequency(opts.frequency, &key.symbol)
            .or_else(|| {
                key.freq
                    .as_deref()
                    .and_then(|f| fugazi::Frequency::from_str(f).ok())
            })
            .or_else(|| frame.declared_frequency(&key.symbol))
            .or_else(|| calendar::detect_frequency_from_atoms(other_atoms.iter().map(|(_, a)| a)));
        let bpy = match calendar::pick_bars_per_year(opts.bars_per_year, &key.symbol, freq) {
            Some(v) => v,
            None => calendar::resolve(
                None,
                opts.asset_class,
                freq,
                calendar::measure_bars_per_year(other_atoms.iter().map(|(_, a)| a)),
            )
            .map_err(anyhow::Error::msg)?,
        };
        let labels: Vec<String> = other_atoms.iter().map(|(t, _)| t.clone()).collect();
        let other_slice = optimize_slice(opts, &labels, || Ok(slice.warmup_bars()))?;
        let warm = (other_slice.warmup_bars() > 0).then(|| other_slice.warmup_bars());
        let other_atoms = if other_slice.is_everything(other_atoms.len()) {
            other_atoms
        } else {
            other_atoms[other_slice.fed()].to_vec()
        };
        let sym = fugazi::types::symbol(&key.symbol);
        let bars: Vec<String> = other_atoms.iter().map(|(t, _)| t.clone()).collect();
        let mut snaps: Vec<fugazi::types::Snapshot<Symbol>> = other_atoms
            .iter()
            .map(|(_, a)| fugazi::types::Snapshot::single(sym.clone(), a.clone()))
            .collect();
        let reads_here = read_only_series(frame, &[key.symbol.as_str()], &reads)?;
        attach_read_series(&bars, &mut snaps, &reads_here);
        streams.insert(
            key.clone(),
            (
                snaps,
                backtest::EvalContext {
                    cash: opts.cash,
                    max_gross: opts.max_gross,
                    margin_rate: opts.margin_rate,
                    maintenance_margin: opts.maintenance_margin,
                    bars_per_year: bpy,
                    risk_free_rate: opts.risk_free_rate,
                    cost_config,
                    effective_freq: freq,
                    windowed: windowed_bars,
                    seconds_per_bar,
                    mc: None,
                    warmup_bars: warm,
                },
            ),
        );
    }
    let streams_ref = &streams;

    let evaluate_row = move |params: &HashMap<String, Value>| -> Result<Evaluation> {
        let spec = build_any_spec(StrategyKind::Single, base_value, params)?;
        // Which series *this row* trades. Every key was prepared above, so the
        // lookup cannot miss — `distinct_roots` walked the same combos.
        let key = root_key_of(base_value, params)?;
        let (snapshots_ref, ctx_ref) = streams_ref
            .get(&key)
            .expect("every grid point's root was prepared by `distinct_roots`");
        Ok(match windowed_n {
            Some(w) => Evaluation::Windowed(
                backtest::evaluate_windowed_any(&spec, snapshots_ref, ctx_ref, w)
                    .map_err(backtest::build_error)?,
            ),
            None => Evaluation::Whole(Box::new(
                backtest::evaluate_any(&spec, snapshots_ref, ctx_ref)
                    .map_err(backtest::build_error)?,
            )),
        })
    };

    let sweep = optimize(
        subgrids,
        windowed_n,
        &opts.metrics,
        opts.best_by.as_deref(),
        opts.risk_aversion,
        opts.smoothing.as_ref(),
        opts.jobs,
        evaluate_row,
    )?;

    write_grid_csv(opts.output, &sweep)?;

    if !opts.quiet {
        let finished = SystemTime::now();
        let fed: Vec<String> = atoms.iter().map(|(t, _)| t.clone()).collect();
        let period = evaluated_period_line(&fed, sweep_warmup.unwrap_or(0));
        style::print_header("optimize", "sweep a strategy over a parameter grid");
        style::print_warns(&style::collect_warnings(
            &skipped_overlay_columns,
            !opts.costs_supplied,
            "grid results",
        ));
        print_inputs_block(
            opts,
            windowed_bars,
            &sweep.subgrid_summaries,
            &sweep.rows,
            period.as_deref(),
            sweep.smooth_scales.as_deref(),
        );
        // A "best" row only means something when the user gave us a metric to
        // rank by. Without one, the sweep has produced a CSV but no verdict.
        if sweep.best_by.is_some() {
            print_best_block(&sweep, opts.risk_aversion);
        }
        warn_if_nothing_traded(&sweep.rows);
        warn_if_ruined(&sweep.rows, sweep.best_by.is_some());
        print_result_block(sweep.rows.len(), started, finished);
    }
    Ok(())
}

/// The pairs / basket / multi-asset grid path — the tradeable universe is
/// determined by the strategy kind, and per-bar snapshots are the outer-join
/// of every relevant symbol's atom stream on `time` (same shape as
/// `run_basket` / `run_multi` / `run_pairs`).
///
/// **Universe extraction differs by kind:**
/// - `basket:` / `multi:` — every symbol in `frame` (floating universe).
/// - `pairs:` — exactly `[spec.left, spec.right]` probed from the first
///   subgrid. Every other subgrid must resolve to the same left/right
///   (checked upfront), same convention as `run_single`.
///
/// `--windowed` is supported via the per-kind windowed evaluator twins;
/// `--walkforward` routes through [`run_multi_symbol_walkforward`].
fn run_multi_symbol(
    opts: &OptimizeOptions,
    subgrids: Vec<Subgrid>,
    frame: &DataFrame,
    base_value: &Value,
) -> Result<()> {
    let started = SystemTime::now();
    let kind_label = match opts.strategy_kind {
        StrategyKind::Pairs => "pairs",
        StrategyKind::Basket => "basket",
        StrategyKind::Multi => "multi",
        StrategyKind::Portfolio => "portfolio",
        _ => unreachable!("run_multi_symbol only dispatched for pairs/basket/multi/portfolio"),
    };

    // Extract the tradeable universe. Pairs probe the first subgrid to
    // resolve `left`/`right` and validate every other subgrid picks the same
    // pair (loading multiple pair slices from one frame isn't supported).
    // Basket / multi / portfolio take the frame's whole symbol set.
    let universe: Vec<Symbol> = match opts.strategy_kind {
        StrategyKind::Pairs => {
            // A pair's legs are resolved from each combo's roots, and every
            // combo must land on the same pair.
            //
            // Deliberately still a refusal, where the single-asset path now
            // sweeps: a pairs run trades the **inner join** of its two legs, so
            // widening the stream to the union of every swept pair would change
            // which bars each row sees, and a row's result would stop matching
            // the same document run on its own through `run`. The single-asset
            // path has no such coupling — each root gets its own stream.
            let legs = |params: &HashMap<String, Value>| -> Result<(String, String)> {
                let spec = build_typed::<PairsStrategySpec>(base_value, params)?;
                Ok((
                    spec.left
                        .sole_symbol("pairs")
                        .map_err(backtest::build_error)?,
                    spec.right
                        .sole_symbol("pairs")
                        .map_err(backtest::build_error)?,
                ))
            };
            let (left, right) = legs(&probe_params(&subgrids[0]))?;
            for subgrid in subgrids.iter() {
                for combo in &subgrid.combos {
                    let params = combine_params(&subgrid.fixed, &subgrid.axes, combo);
                    let (l, r) = legs(&params)?;
                    if l != left || r != right {
                        bail!(
                            "this grid resolves to pair `{l}`/`{r}` as well as `{left}`/`{right}` \
                             — every grid point must trade the same pair, because a pairs run \
                             evaluates the inner join of its two legs and a different pair is a \
                             different timeline"
                        );
                    }
                }
            }
            vec![fugazi::types::symbol(&left), fugazi::types::symbol(&right)]
        }
        _ => frame.symbols().iter().map(fugazi::types::symbol).collect(),
    };
    if universe.is_empty() {
        bail!(
            "no symbols found in the input series — `{kind_label}:` optimization needs at least \
             one traded asset"
        );
    }
    // Per-symbol atom streams, sorted by time. `DataFrame::atoms` walks a
    // BTreeMap so each per-symbol stream is already ascending; the joiner
    // then N-way merges them into shared bar-tagged snapshots.
    let per_symbol: Vec<(Symbol, Vec<(String, Atom)>)> = universe
        .iter()
        .map(|sym| Ok::<_, anyhow::Error>((sym.clone(), frame.atoms(sym)?.atoms)))
        .collect::<Result<_>>()?;
    let (bars, mut snapshots) = join_universe_by_time(&per_symbol);
    // Empty unless the universe is narrower than the frame (pairs), since every
    // `!pick` target of a basket / multi / portfolio sweep is already traded —
    // so for those three this is the "named a symbol that isn't in the input"
    // check and nothing more.
    let traded_refs: Vec<&str> = universe.iter().map(|s| s.as_ref()).collect();
    let reads = probe_reads(base_value, &subgrids)?;
    let read_only = read_only_series(frame, &traded_refs, &reads)?;
    attach_read_series(&bars, &mut snapshots, &read_only);
    if snapshots.is_empty() {
        bail!(
            "no bars found in the input series across the {} discovered symbol(s)",
            universe.len()
        );
    }
    // Warned here rather than alongside the summary blocks below, which print
    // only once the sweep is done: a fragmented universe means every row of
    // that sweep measures something other than the universe it names, and that
    // is worth knowing before the grid runs, not after. See `crate::overlap`.
    let overlap = overlap::measure_universe(&per_symbol);
    overlap::warn_if_fragmented(&overlap, overlap.at, overlap::RUN_CONSEQUENCE);

    // Cadence: the representative (first) symbol's `--frequency` scope, then
    // its declared `freq` column, then detection from its timestamps. Matches
    // `run_basket` / `run_multi`. A universe whose symbols disagree was warned
    // about at load — see `crate::cadence`.
    let representative = &universe[0];
    let effective_freq = calendar::pick_frequency(opts.frequency, representative)
        .or_else(|| frame.declared_frequency(representative))
        .or_else(|| {
            per_symbol
                .iter()
                .find(|(s, _)| s.as_ref() == representative.as_ref())
                .and_then(|(_, atoms)| {
                    calendar::detect_frequency_from_atoms(atoms.iter().map(|(_, a)| a))
                })
        });
    let bars_per_year =
        match calendar::pick_bars_per_year(opts.bars_per_year, representative, effective_freq) {
            Some(v) => v,
            None => calendar::resolve(
                None,
                opts.asset_class,
                effective_freq,
                per_symbol
                    .iter()
                    .find(|(s, _)| s.as_ref() == representative.as_ref())
                    .and_then(|(_, a)| calendar::measure_bars_per_year(a.iter().map(|(_, at)| at))),
            )
            .map_err(anyhow::Error::msg)?,
        };

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

    // `--from` / `--until` on the *joined* timeline, so a universe whose
    // symbols list at different times slices the way the dates say rather than
    // the way any one file happens to start. Same split as `run_single`:
    // walk-forward gets the evaluated span alone and settles inside it, the
    // sweep gets the warm-up prefix attached.
    let slice = {
        let probe_schema = backtest::schema_from_snapshots(&snapshots);
        let probe_snapshot = universe_probe_snapshot(&universe);
        let keep_unstable = opts.keep_unstable;
        let cash = opts.cash;
        let kind = opts.strategy_kind;
        optimize_slice(opts, &bars, || {
            grid_warmup_need(&subgrids, |params| {
                let spec = build_any_spec(kind, base_value, params)?;
                let mut built = spec
                    .try_build(cash, &probe_schema, None)
                    .map_err(backtest::build_error)?;
                // Basket and multi build per-symbol chains lazily; the eager
                // shapes must not be fed a probe. See `needs_probe_feed`.
                if matches!(kind, StrategyKind::Basket | StrategyKind::Multi) {
                    built.update(probe_snapshot.clone());
                }
                Ok(if keep_unstable {
                    built.warm_up_bars()
                } else {
                    built.stable_bars()
                })
            })
        })?
    };
    let whole = slice.is_everything(bars.len());
    let sweep_warmup =
        (opts.walkforward.is_none() && slice.warmup_bars() > 0).then(|| slice.warmup_bars());
    let keep = if opts.walkforward.is_some() {
        slice.eval_start..slice.end
    } else {
        slice.fed()
    };
    let (bars, snapshots) = if whole {
        (bars, snapshots)
    } else {
        (bars[keep.clone()].to_vec(), snapshots[keep].to_vec())
    };

    // Walk-forward branch: same shape as `run_single`'s — closures inject
    // the basket/multi build + backtest, the driver stays strategy-agnostic.
    if let Some(walkforward_spec) = opts.walkforward {
        return run_multi_symbol_walkforward(
            opts,
            subgrids,
            base_value,
            &snapshots,
            &universe,
            walkforward_spec,
            bars_per_year,
            effective_freq,
            seconds_per_bar,
        );
    }

    let cost_config = opts.cost_config;
    let snapshots_ref = &snapshots;
    let windowed_n = windowed_bars.map(NonZeroUsize::get);
    let kind = opts.strategy_kind;
    let ctx = backtest::EvalContext {
        cash: opts.cash,
        max_gross: opts.max_gross,
        margin_rate: opts.margin_rate,
        maintenance_margin: opts.maintenance_margin,
        bars_per_year,
        risk_free_rate: opts.risk_free_rate,
        cost_config,
        effective_freq,
        windowed: windowed_bars,
        seconds_per_bar,
        mc: None,
        warmup_bars: sweep_warmup,
    };
    let ctx_ref = &ctx;

    // One path for every shape: `StrategySpec` carries which one, and the
    // per-shape differences (wallet, how costs are applied, what the universe
    // is) live behind `RunnableStrategy` / `StrategySpec` rather than here.
    let evaluate_row = move |params: &HashMap<String, Value>| -> Result<Evaluation> {
        let spec = build_any_spec(kind, base_value, params)?;
        Ok(match windowed_n {
            Some(w) => Evaluation::Windowed(
                backtest::evaluate_windowed_any(&spec, snapshots_ref, ctx_ref, w)
                    .map_err(backtest::build_error)?,
            ),
            None => Evaluation::Whole(Box::new(
                backtest::evaluate_any(&spec, snapshots_ref, ctx_ref)
                    .map_err(backtest::build_error)?,
            )),
        })
    };

    let sweep = optimize(
        subgrids,
        windowed_n,
        &opts.metrics,
        opts.best_by.as_deref(),
        opts.risk_aversion,
        opts.smoothing.as_ref(),
        opts.jobs,
        evaluate_row,
    )?;

    write_grid_csv(opts.output, &sweep)?;

    if !opts.quiet {
        let finished = SystemTime::now();
        let period = evaluated_period_line(&bars, sweep_warmup.unwrap_or(0));
        style::print_header("optimize", "sweep a strategy over a parameter grid");
        style::print_warns(&style::collect_warnings(
            &[],
            !opts.costs_supplied,
            "grid results",
        ));
        print_inputs_block(
            opts,
            windowed_bars,
            &sweep.subgrid_summaries,
            &sweep.rows,
            period.as_deref(),
            sweep.smooth_scales.as_deref(),
        );
        if sweep.best_by.is_some() {
            print_best_block(&sweep, opts.risk_aversion);
        }
        warn_if_nothing_traded(&sweep.rows);
        warn_if_ruined(&sweep.rows, sweep.best_by.is_some());
        print_result_block(sweep.rows.len(), started, finished);
    }
    Ok(())
}

/// The basket / multi-asset walk-forward driver — the `--walkforward`
/// peer of [`run_multi_symbol`]'s grid sweep, sharing the strategy-agnostic
/// [`walkforward_run`] via closures.
///
/// **Lazy readiness probing.** Basket / multi strategies build per-symbol
/// chains on first sight of a snapshot — a freshly-constructed strategy
/// has no chains yet and `stable_bars()` reports only the rebalance
/// signal's period. To reveal the grid-wide max the walk-forward layout
/// needs, we feed each throwaway probe strategy one *synthetic* snapshot
/// containing every universe symbol with a dummy [`Atom`], triggering the
/// factories on every symbol before reading `stable_bars()` /
/// `warm_up_bars()`. The dummy atom carries no overlays and a zero
/// candle — safe because the probe never trades, only exercises chain
/// construction.
#[allow(clippy::too_many_arguments)]
fn run_multi_symbol_walkforward(
    opts: &OptimizeOptions,
    subgrids: Vec<Subgrid>,
    base_value: &Value,
    snapshots: &[fugazi::types::Snapshot<Symbol>],
    universe: &[Symbol],
    walkforward_spec: WalkForwardSpec,
    bars_per_year: Real,
    effective_freq: Option<Frequency>,
    seconds_per_bar: Option<Real>,
) -> Result<()> {
    let schema = backtest::schema_from_snapshots(snapshots);
    let keep_unstable = opts.keep_unstable;
    let cash = opts.cash;
    let max_gross = opts.max_gross;
    let margin_rate = opts.margin_rate;
    let maintenance_margin = opts.maintenance_margin;
    let cost_config = opts.cost_config;
    let kind = opts.strategy_kind;

    // Synthetic single-snapshot probe: one dummy atom per universe symbol
    // so the strategy's per-symbol factories fire on the first update() call.
    // The probe strategy never trades — just exposes stable/warm-up state.
    let probe_snapshot = universe_probe_snapshot(universe);

    // `TradingCosts` isn't `Clone` (boxed trait objects inside), so the
    // per-symbol cost bundle is rebuilt inside the run closure for every
    // grid row rather than cloned. `cost_config.resolve` is cheap — a
    // HashMap lookup + trivial model construction — so the cost is
    // negligible next to the backtest itself.
    let schema_ref = &schema;
    let probe_snap_ref = &probe_snapshot;
    let snapshots_ref = snapshots;

    // Basket and multi build their per-symbol chains lazily, on first sight of
    // a symbol, so `stable_bars()` only reads true once a snapshot has gone
    // through — hence the probe feed. The other shapes build eagerly and must
    // *not* be fed it: a pairs leaf that didn't name its asset would hit the
    // sole-atom guard on a multi-symbol snapshot.
    let needs_probe_feed = matches!(kind, StrategyKind::Basket | StrategyKind::Multi);

    // Walk-forward measures whole runs and slices them per fold, so the
    // windowed field is irrelevant here.
    let ctx = backtest::EvalContext {
        cash,
        max_gross,
        margin_rate,
        maintenance_margin,
        bars_per_year,
        risk_free_rate: opts.risk_free_rate,
        cost_config,
        effective_freq,
        windowed: None,
        seconds_per_bar,
        mc: None,
        warmup_bars: None,
    };
    let ctx_ref = &ctx;

    let probe = |params: &HashMap<String, Value>| -> Result<usize> {
        let spec = build_any_spec(kind, base_value, params)?;
        let mut built = spec
            .try_build(cash, schema_ref, None)
            .map_err(backtest::build_error)?;
        if needs_probe_feed {
            built.update(probe_snap_ref.clone());
        }
        Ok(if keep_unstable {
            built.warm_up_bars()
        } else {
            built.stable_bars()
        })
    };

    let run_backtest = |params: &HashMap<String, Value>| -> Result<fugazi::RunReport<Symbol>> {
        let spec = build_any_spec(kind, base_value, params)?;
        backtest::measured_report_any(&spec, snapshots_ref, ctx_ref).map_err(backtest::build_error)
    };

    // Basket / multi drivers currently don't surface `skipped_overlay_columns`
    // — the frame's per-symbol atoms are the source of truth, but the CLI's
    // multi-symbol path never propagated the skip list to this driver. Pass an
    // empty slice so the "warn:" banner doesn't misfire.
    let no_skipped: [String; 0] = [];
    walkforward_run(
        subgrids,
        snapshots.len(),
        probe,
        run_backtest,
        bars_per_year,
        opts.risk_free_rate,
        effective_freq,
        walkforward_spec,
        opts.keep_unstable,
        opts.asset_class,
        seconds_per_bar,
        &opts.metrics,
        opts.best_by.as_deref(),
        opts.smoothing.as_ref(),
        opts.output,
        opts.jobs,
        opts.quiet,
        &no_skipped,
        opts.cash,
    )
}

// ---------------------------------------------------------------------------
// CSV output
// ---------------------------------------------------------------------------

/// Write the sweep CSV: the union axis columns (name-sorted) first, then one
/// column per requested metric — or, under `-w/--windowed`, two columns per
/// metric (`<name>_mean` / `<name>_std`, the cross-window aggregate). Whole-run
/// sweeps also get a trailing `selection.deflated_sharpe` column when the grid has
/// enough spread in Sharpes for the multiple-testing correction to be defined.
/// Under `--smooth`, two further columns are appended — `<best_by>_smoothed`
/// and `<best_by>_support`, the neighbourhood average the rows are ranked by
/// and the fraction of a fully-interior neighbourhood that average rests on.
///
/// **Metric-name suffix, not a `smooth.` scope of their own** — matching the
/// only convention this file has: every flag-gated column here is a suffix on
/// the metric it derives from (`_mean`/`_std` under `-w`, `_is`/`_oos`/`_wfe`
/// under `--walkforward`). `_wfe` is the precedent that decides it: a
/// walk-forward efficiency ratio is not an aggregation of its metric either,
/// and it still takes the suffix. The suffix also keeps the column
/// self-describing — `risk_adjusted.sharpe_smoothed` names what was smoothed,
/// where a scoped `smooth.value` would send the reader back to the invocation
/// to find out. (Caveat inherited from `-k`, not created here: under
/// `-k/--risk-aversion` the smoothed column averages `mean − k·std`, and that
/// shifted key has no raw column of its own in either naming.)
///
/// Existing columns keep their position and their values; smoothing changes
/// row *order*, never a cell.
///
/// `,`-delimited to match `fills.csv` / `trades.csv` / `returns.csv`. Axis cells that the
/// row's subgrid doesn't touch, and missing (omitted) metric values, are both
/// written as an empty cell.
fn write_grid_csv(path: &Path, sweep: &Sweep) -> Result<()> {
    let Sweep {
        union_columns,
        metric_columns,
        windowed,
        deflated_sharpe_context,
        rows,
        ..
    } = sweep;
    let (windowed, deflated_sharpe_context) = (*windowed, *deflated_sharpe_context);
    // The smoothed columns are named after the metric they average, so the CSV
    // reads `risk_adjusted.sharpe` next to `risk_adjusted.sharpe_smoothed`.
    let smoothed_path = sweep
        .smoothing
        .as_ref()
        .and(sweep.best_by.as_ref())
        .map(|(_, path, _)| path.as_str());
    if let Some(parent) = path.parent()
        && !parent.as_os_str().is_empty()
    {
        std::fs::create_dir_all(parent)
            .with_context(|| format!("creating output dir `{}`", parent.display()))?;
    }
    let mut writer = csv::WriterBuilder::new()
        .delimiter(b',')
        .from_path(path)
        .with_context(|| format!("creating `{}`", path.display()))?;

    let mut header: Vec<String> = union_columns.to_vec();
    for (name, _) in metric_columns {
        if windowed {
            header.push(format!("{name}_mean"));
            header.push(format!("{name}_std"));
        } else {
            header.push(name.clone());
        }
    }
    if deflated_sharpe_context.is_some() {
        header.push("selection.deflated_sharpe".to_string());
    }
    if let Some(path) = smoothed_path {
        header.push(format!("{path}_smoothed"));
        header.push(format!("{path}_support"));
    }
    writer.write_record(&header)?;

    // Precompute the flatten position of each metric column against a sample
    // document (whichever eval is available). Then per row, flatten each
    // Metrics **once** and read the columns by indexed access — turning
    // `rows * cols` full-metrics scans into `rows * 1` flattens + `rows * cols`
    // vec[i] lookups. Empirically ~325× faster on a 50k×5 grid.
    // Any row will do — `metrics::flatten`'s ordering is document-shape
    // invariant. Scanning rather than taking `rows[0]` keeps this independent
    // of which row sorted first, and skips a windowed row with no windows.
    let sample_metrics = rows.iter().find_map(|r| match &r.eval {
        Evaluation::Whole(m) => Some(m.as_ref()),
        Evaluation::Windowed(ws) => ws.first().map(|w| &w.metrics),
    });
    let positions: Vec<Option<ColumnPos>> = if let Some(sample) = sample_metrics {
        let flat = metrics::flatten(sample);
        metric_columns
            .iter()
            .map(|(_, path)| flat.iter().position(|(k, _)| *k == path.as_str()))
            .collect()
    } else {
        // Empty sweep — no rows means no lookups needed. Fill with `None`.
        vec![None; metric_columns.len()]
    };

    let cell = |v: Option<Real>| v.map(format_number).unwrap_or_default();
    for row in rows {
        let mut record: Vec<String> = row
            .values
            .iter()
            .map(|v| v.as_ref().map(format_value).unwrap_or_default())
            .collect();
        match &row.eval {
            Evaluation::Whole(m) => {
                // Flatten once, then index each requested column.
                let flat = metrics::flatten(m);
                for pos in &positions {
                    let v = pos.and_then(|p| flat[p].1);
                    record.push(cell(v));
                }
            }
            Evaluation::Windowed(ws) => {
                // Flatten each window once, keep them for the whole row.
                let per_window: Vec<Vec<Option<Real>>> = ws
                    .iter()
                    .map(|w| {
                        metrics::flatten(&w.metrics)
                            .into_iter()
                            .map(|(_, v)| v)
                            .collect()
                    })
                    .collect();
                for pos in &positions {
                    let spread =
                        pos.and_then(|p| mean_std_of(per_window.iter().map(|window| window[p])));
                    record.push(cell(spread.map(|(mean, _)| mean)));
                    record.push(cell(spread.map(|(_, std)| std)));
                }
            }
        }
        // Trailing `selection.deflated_sharpe` cell — uses per-row summary stats extracted
        // via `row_dsr_inputs` (whole-run passthrough or windowed cross-window
        // means; see [`row_dsr_inputs`] and the [`Sweep`] field's rustdoc).
        //
        // Written for *every* row, ruined ones included: only the grid-wide
        // context excludes them (`trial_sharpe`), because they were never
        // candidates. The cell itself is a description, like the `sharpe` one
        // beside it.
        if let Some((n_trials, trial_var)) = deflated_sharpe_context {
            let (sharpe, skew, kurt, n_returns, bpy) = row_dsr_inputs(row);
            let dsr = fugazi::metrics::deflated_sharpe_from_stats(
                sharpe, skew, kurt, n_returns, bpy, n_trials, trial_var,
            );
            record.push(cell(dsr));
        }
        if smoothed_path.is_some() {
            // `support` is written even when the value was dropped by
            // `--smooth-min-support` — a blank smoothed cell next to a low
            // support number is the whole diagnostic.
            let smoothed = row.smoothed.as_ref();
            record.push(cell(smoothed.and_then(|s| s.value)));
            record.push(cell(smoothed.map(|s| s.support)));
        }
        writer.write_record(&record)?;
    }
    writer.flush()?;
    Ok(())
}

// ---------------------------------------------------------------------------
// Console output
// ---------------------------------------------------------------------------

#[allow(clippy::too_many_arguments)]
fn print_inputs_block(
    opts: &OptimizeOptions,
    windowed_bars: Option<NonZeroUsize>,
    subgrid_summaries: &[(String, usize)],
    rows: &[Row],
    period: Option<&str>,
    smooth_scales: Option<&[(String, AxisScale)]>,
) {
    style::print_section("inputs");
    style::field("strategy", opts.strategy_label);
    if subgrid_summaries.len() == 1 {
        // Compact form when there's only one subgrid — matches the pre-stack
        // shape, so a single-`--grid` invocation reads the same as before.
        style::field(
            "grid",
            &format!("{} points · {}", rows.len(), subgrid_summaries[0].0),
        );
    } else {
        style::field(
            "grid",
            &format!(
                "{} points across {} subgrids",
                rows.len(),
                subgrid_summaries.len()
            ),
        );
        for (i, (label, n)) in subgrid_summaries.iter().enumerate() {
            style::field_continuation(&format!("[{}] {n} pts · {label}", i + 1));
        }
    }
    if let Some(p) = period {
        style::field("period", p);
    }
    style::field("capital", &format!("{:.2}", opts.cash));
    // Costs summary — same treatment as `run`: name it explicitly if a model is
    // set, note `none (explicit)` if the user opted in silently. The
    // no-cost warning has been hoisted above the block by `collect_warnings`.
    if !opts.cost_config.is_none() {
        style::field("costs", "active (commission/spread/slippage applied)");
    } else if opts.costs_supplied {
        style::field("costs", "none (explicit)");
    }
    style::field("output", &opts.output.display().to_string());
    if let (Some(spec), Some(bars)) = (opts.windowed, windowed_bars) {
        let msg = match spec {
            WindowSpec::Bars(_) => format!("{bars}-bar windows (mean ± std per metric)"),
            WindowSpec::Duration(_) => {
                format!("{spec} → {bars}-bar windows (mean ± std per metric)")
            }
        };
        style::field("windowed", &msg);
    }
    if let Some(name) = &opts.best_by {
        if opts.risk_aversion > 0.0 {
            style::field(
                "best-by",
                &format!(
                    "{name} (risk-aversion k={}: mean shifted k·std against)",
                    opts.risk_aversion
                ),
            );
        } else {
            style::field("best-by", name);
        }
        if let Some(sm) = &opts.smoothing {
            let mut msg = format!("{} over the parameter neighbourhood", sm.kernel);
            if sm.min_support > 0.0 {
                msg.push_str(&format!(" (min support {})", sm.min_support));
            }
            // Which scale each axis' distances were measured on. Never left
            // implicit: it is the thing that decides what "one step" means, and
            // on an irregular axis the automatic choice is a judgment call.
            if let Some(scales) = smooth_scales.filter(|s| !s.is_empty()) {
                let per_axis: Vec<String> = scales
                    .iter()
                    .map(|(name, scale)| format!("{name} {scale}"))
                    .collect();
                msg.push_str(&format!(" · scale {}", per_axis.join(", ")));
            }
            style::field("smooth", &msg);
        }
    }
}

/// The "result" block for `optimize`: number of grid points evaluated, then
/// wall-clock timing. Mirrors `run`'s result block so both commands look the
/// same at the tail.
/// Warn when not one grid point opened a trade.
///
/// Every cell in the metric columns is empty in that case, which reads exactly
/// like a metric name that didn't resolve — and for a long time it *was*
/// reported as one, because the metric catalogue was derived from a serialized
/// sample and a degenerate run serializes the ratio away entirely. That is
/// fixed at the source (`spec::metrics::resolve_metric_path`), but a sweep
/// where nothing traded is still worth saying out loud: the grid is almost
/// certainly wrong (a period longer than the data, a signal that can't fire),
/// and an all-empty CSV does not say so.
///
/// stderr and ungated by `--quiet`, matching `overlap` / `cadence`: it is a
/// finding about the result, not part of the summary the user asked to silence.
fn warn_if_nothing_traded(rows: &[Row]) {
    if rows.is_empty() {
        return;
    }
    let trades = |m: &metrics::Metrics| m.trades.total;
    let any_traded = rows.iter().any(|row| match &row.eval {
        Evaluation::Whole(m) => trades(m) > 0,
        Evaluation::Windowed(ws) => ws.iter().any(|w| trades(&w.metrics) > 0),
    });
    if any_traded {
        return;
    }
    eprintln!(
        "{} no grid point produced any trades — every metric cell is empty. \
         Check that the parameter ranges can actually fire the strategy's \
         signals over this data (a window longer than the series is the usual \
         cause).",
        style::yellow("warning:")
    );
}

/// The grid-wide "some of these cells are dead accounts" banner.
///
/// `run` gained a ruin banner in `1a253e8`; `optimize` did not, and it needs
/// one *more*, because a sweep reports N cells and shows the user one. A ruined
/// cell is no longer a candidate to win (see
/// [`ranking_lookup`](fugazi::spec::optimize::ranking_lookup)), but it is still
/// a row in the CSV with a headline `sharpe` on it, and unless `run.ruin_bar`
/// was among the `-m` columns nothing on that line says the account was zeroed.
///
/// stderr and ungated by `--quiet`, matching `warn_if_nothing_traded` /
/// `overlap` / `cadence`: a finding about the result, not part of the summary.
fn warn_if_ruined(rows: &[Row], ranked: bool) {
    let ruined = rows.iter().filter(|r| r.eval.ruin_bar().is_some()).count();
    if ruined == 0 {
        return;
    }
    let n = rows.len();
    let tail = if ruined == n {
        " Every cell in this grid ended in ruin — there is no solvent point to \
         pick, and the row shown as `best` is only the first one enumerated."
            .to_string()
    } else if ranked {
        String::new()
    } else {
        " Pass --best-by to rank the sweep; without it the rows are in \
         enumeration order and a ruined cell can be the first one."
            .to_string()
    };
    eprintln!(
        "{} {ruined} of {n} grid {} ended in ruin — the account reached zero \
         and stopped trading. Their bar-return metrics (sharpe, mean_bar, \
         win_rate_pct, …) describe only the part of the run that happened \
         before that, so they are excluded from --best-by ranking; read \
         run.ruin_bar to tell them apart.{tail}",
        style::yellow("warning:"),
        if ruined == 1 { "point" } else { "points" },
    );
}

fn print_result_block(points: usize, started: SystemTime, finished: SystemTime) {
    println!();
    style::print_section("result");
    style::field("points", &points.to_string());
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

/// `start → end (N bars)` when the atom stream has at least one entry, else
/// `None`. Shared by the single-asset and multi-symbol drivers so both echo
/// the same period line as `run` does.
/// The `period` line, over the bars a sweep will actually *measure*.
///
/// `labels` is the fed stream, warm-up prefix included; `warmup` is how much of
/// its head only settles the chains. Reporting the fed range here would
/// overstate both the period and the bar count by that prefix.
fn evaluated_period_line(labels: &[String], warmup: usize) -> Option<String> {
    let evaluated = labels.get(warmup.min(labels.len())..)?;
    let (s, e) = (evaluated.first()?, evaluated.last()?);
    let bars = evaluated.len();
    Some(match warmup {
        0 => format!("{s} → {e} ({bars} bars)"),
        w => format!("{s} → {e} ({bars} bars, {w} warm-up)"),
    })
}

/// Short friendly label for the console — strip the section prefix from a
/// canonical dotted metric path (`risk_adjusted.sharpe` → `sharpe`,
/// `returns.cagr_pct` → `cagr_pct`). CSV columns stay as the canonical
/// dotted path — this is just for display.
fn friendly_metric_label(dotted_or_short: &str) -> String {
    dotted_or_short
        .rsplit_once('.')
        .map(|(_, tail)| tail.to_string())
        .unwrap_or_else(|| dotted_or_short.to_string())
}

/// The `--smooth` half of the best block: the winner's smoothed value, and the
/// gap between the raw argmax and the smoothed ordering.
///
/// That gap is the diagnostic the flag exists to surface. A raw argmax that
/// falls to rank 40 of 50 once its neighbourhood is taken into account was a
/// noise spike, and the console is where a user finds that out — the CSV shows
/// the numbers but not the disagreement.
fn print_smoothing_lines(
    sweep: &Sweep,
    path: &str,
    direction: Direction,
    k: Real,
    smoothing: &Smoothing,
) {
    let rows = &sweep.rows;
    let winner = match rows.first().and_then(|r| r.smoothed) {
        Some(s) => s,
        None => return,
    };
    let smoothed_label = match winner.value {
        Some(v) => format!(
            "{v:.4} · support {:.2} · {}",
            winner.support, smoothing.kernel
        ),
        None => format!("— · support {:.2} · {}", winner.support, smoothing.kernel),
    };
    style::field("smoothed", &smoothed_label);

    // Rows arrive sorted by the smoothed key, so a row's smoothed rank is its
    // position; its raw rank needs the raw keys recomputed over the same rows.
    let raw_keys: Vec<Option<Real>> = rows
        .iter()
        .map(|r| ranking_value(&r.eval, path, direction, k))
        .collect();
    let raw_ranks = rank_positions(&raw_keys, direction);
    let n = rows.len();
    if let Some(raw_argmax) = raw_ranks.iter().position(|&r| r == 1) {
        style::field(
            "ranks",
            &format!(
                "raw argmax #{}/{n} smoothed · smoothed winner #{}/{n} raw",
                raw_argmax + 1,
                raw_ranks[0]
            ),
        );
    }

    // The grid's shape is the result; its maximum is not. Measured in the
    // kernel, before the sort — the lattice is gone by the time we get here.
    if let Some(cells) = sweep.plateau {
        let pct = PLATEAU_TOLERANCE * 100.0;
        style::field(
            "plateau",
            &format!("{cells} of {n} cells connected within {pct:.0}% of the best smoothed value"),
        );
    }
}

fn print_best_block(sweep: &Sweep, k: Real) {
    let (union_columns, metric_columns, rows) =
        (&sweep.union_columns, &sweep.metric_columns, &sweep.rows);
    let best_by = sweep.best_by.as_ref();
    println!();
    style::print_section("best");
    let Some(best) = rows.first() else {
        style::field("params", "(no grid points)");
        return;
    };

    // Skip axis columns the winning row's subgrid doesn't touch — the params
    // line names only what actually took a value, so a stacked sweep's
    // conditional axes don't show as `Z=<empty>`.
    let params_label: String = union_columns
        .iter()
        .zip(best.values.iter())
        .filter_map(|(name, v)| v.as_ref().map(|v| format!("{name}={}", format_value(v))))
        .collect::<Vec<_>>()
        .join(", ");
    style::field("params", &params_label);

    if let Some((_name, path, direction)) = best_by {
        let mut value = format_metric(&best.eval, path);
        // With a risk-aversion penalty the ranking key differs from the mean;
        // show it so the ordering is explainable from the console alone.
        if k > 0.0
            && matches!(best.eval, Evaluation::Windowed(_))
            && let Some(score) = ranking_value(&best.eval, path, *direction, k)
        {
            value = format!("{value} · score {score:.4}");
        }
        // Friendly label for the console; the CSV column keeps the dotted path.
        style::field(&friendly_metric_label(path), &value);
        if let Some(smoothing) = &sweep.smoothing {
            print_smoothing_lines(sweep, path, *direction, k, smoothing);
        }
    }
    for (_name, path) in metric_columns {
        // Skip a metric already printed as the best-by row.
        if best_by.map(|(_, p, _)| p.as_str()) == Some(path.as_str()) {
            continue;
        }
        style::field(
            &friendly_metric_label(path),
            &format_metric(&best.eval, path),
        );
    }
    // Best-row headline metrics from the run block for context — cross-window
    // mean ± std under `-w`, matching the metric rows above.
    let headline = match &best.eval {
        Evaluation::Whole(m) => format!(
            "{:+.2}% ann · vol {:.2}%",
            m.returns.annualized_mean_pct, m.returns.annualized_volatility_pct
        ),
        Evaluation::Windowed(ws) => {
            let fmt = |spread: Option<(Real, Real)>, signed: bool| {
                spread.map_or_else(
                    || "—".to_string(),
                    |(mean, std)| {
                        if signed {
                            format!("{mean:+.2}% ± {std:.2}%")
                        } else {
                            format!("{mean:.2}% ± {std:.2}%")
                        }
                    },
                )
            };
            format!(
                "{} ann · vol {}",
                fmt(
                    metrics::mean_std(ws.iter().map(|w| w.metrics.returns.annualized_mean_pct)),
                    true
                ),
                fmt(
                    metrics::mean_std(
                        ws.iter()
                            .map(|w| w.metrics.returns.annualized_volatility_pct)
                    ),
                    false
                ),
            )
        }
    };
    style::field("return", &headline);

    // Last, directly under the headline it contradicts. `+356.06% ann` on a
    // zeroed account is the exact line this exists to qualify: every number
    // above describes the run up to `bar`, and there was no money after it.
    if let Some(bar) = best.eval.ruin_bar() {
        let bars = match &best.eval {
            Evaluation::Whole(m) => m.run.bars,
            Evaluation::Windowed(ws) => ws.last().map_or(0, |w| w.end_bar + 1),
        };
        style::field(
            "ruin",
            &format!(
                "ruined at bar {bar} of {bars} — every figure above describes \
                 the run before that point",
            ),
        );
        style::field_continuation(
            "this cell is not rankable, so it is shown here only because no \
             solvent cell out-ranked it",
        );
    }
}

/// One metric value for the best block: `1.2345` for a whole-run evaluation,
/// `1.2345 ± 0.6789` for a windowed one, `—` when degenerate (everywhere).
fn format_metric(eval: &Evaluation, path: &str) -> String {
    match eval {
        Evaluation::Whole(m) => {
            lookup(m, path).map_or_else(|| "—".to_string(), |v| format!("{v:.4}"))
        }
        Evaluation::Windowed(ws) => lookup_windowed(ws, path).map_or_else(
            || "—".to_string(),
            |(mean, std)| format!("{mean:.4} ± {std:.4}"),
        ),
    }
}

// ---------------------------------------------------------------------------
// Walk-forward (rolling)
// ---------------------------------------------------------------------------

/// Rolling walk-forward driver — the `--walkforward` peer of the [`optimize`]
/// grid sweep. Runs every grid row's full backtest once, then per fold: slices
/// each row's report into IS + OOS, ranks the rows by `--best-by`'s IS metric,
/// records the winner and the winner's OOS realization, and (across folds)
/// assembles a composite out-of-sample equity curve.
///
/// Emits three artifacts alongside `output` (all sibling files, derived stems):
/// the per-fold table, the composite OOS `bar,equity` curve, and the composite
/// OOS `Metrics` document. Console output mirrors [`run`]'s shape: header,
/// inputs block, per-fold summary.
///
/// Strategy-agnostic: two closures inject the strategy-specific work.
///
/// * `probe_readiness(params) -> usize` — build the strategy for one grid
///   row's params and return its `stable_bars()` (or `warm_up_bars()`
///   under `--keep-unstable`). The grid-wide max is the fold-layout's
///   prefix skip. For basket / multi strategies the caller is responsible
///   for feeding one representative snapshot to trigger lazy per-symbol
///   chain discovery before reading the period — see the
///   [`DynBasketStrategy::stable_bars`](crate::spec::DynBasketStrategy::stable_bars)
///   / [`DynMultiAssetStrategy::stable_bars`](crate::spec::DynMultiAssetStrategy::stable_bars)
///   rustdoc for the contract.
/// * `run_backtest(params) -> RunReport` — build the strategy and drive it
///   through a fresh paper wallet over the whole run, returning the report.
///   The main pass calls this once per grid row; the resulting report is
///   sliced per fold rather than re-running.
///
/// `n_bars` is the length of the bar sequence the reports are indexed
/// against — the atom count for single-asset, the aligned-snapshot count
/// for basket / multi.
#[allow(clippy::too_many_arguments)]
fn walkforward_run<P, R>(
    subgrids: Vec<Subgrid>,
    n_bars: usize,
    probe_readiness: P,
    run_backtest: R,
    bars_per_year: Real,
    risk_free_rate: Real,
    effective_freq: Option<Frequency>,
    spec: WalkForwardSpec,
    keep_unstable: bool,
    asset_class: Option<AssetClass>,
    seconds_per_bar: Option<Real>,
    metric_names: &[String],
    best_by: Option<&str>,
    smoothing: Option<&Smoothing>,
    output: &Path,
    jobs: Option<usize>,
    quiet: bool,
    skipped_overlay_columns: &[String],
    cash: Real,
) -> Result<()>
where
    P: Fn(&HashMap<String, Value>) -> Result<usize> + Sync,
    R: Fn(&HashMap<String, Value>) -> Result<fugazi::RunReport<Symbol>> + Sync,
{
    let (is_bars, oos_bars, embargo_bars) = spec
        .resolve(effective_freq, asset_class)
        .map_err(anyhow::Error::msg)
        .context("resolving `--walkforward`")?;

    // Delegate the strategy-agnostic sweep + per-fold selection + composite
    // stitching to the library kernel. This CLI wrapper owns only the
    // WalkForwardSpec resolution, CSV / YAML output, and console printing.
    let result = crate::spec::optimize::walkforward(
        subgrids,
        n_bars,
        probe_readiness,
        run_backtest,
        bars_per_year,
        risk_free_rate,
        seconds_per_bar,
        is_bars,
        oos_bars,
        embargo_bars,
        metric_names,
        best_by,
        smoothing,
        jobs,
        cash,
    )?;

    // Output — three sibling files.
    write_walkforward_csv(
        output,
        &result.union_columns,
        &result.metric_columns,
        smoothing
            .and(result.best_by.as_ref())
            .map(|(_, path, _)| path.as_str()),
        &result.fold_rows,
    )?;
    write_composite_equity_csv(
        &derive_sibling(output, "composite_oos_equity", "csv"),
        &result.composite_equity,
    )?;
    write_composite_metrics_yaml(
        &derive_sibling(output, "composite_oos_metrics", "yml"),
        &result.composite_metrics,
    )?;

    if !quiet {
        style::print_header("optimize", "walk-forward optimization");
        style::print_warns(&style::collect_warnings(
            skipped_overlay_columns,
            false,
            "grid results",
        ));
        print_walkforward_inputs(
            &spec,
            (result.is_bars, result.oos_bars, result.embargo_bars),
            result.prefix_skip,
            keep_unstable,
            result.folds.len(),
            n_bars,
            output,
        );
        print_walkforward_summary(
            &result.fold_rows,
            &result.metric_columns,
            result.best_by.as_ref(),
        );
    }
    Ok(())
}

/// Given `-o out/wf.csv` and `("composite_oos_equity", "csv")` returns
/// `out/wf.composite_oos_equity.csv`. Preserves the parent directory; folds
/// the stem when the output already has an extension.
fn derive_sibling(output: &Path, suffix_stem: &str, extension: &str) -> std::path::PathBuf {
    let parent = output.parent().unwrap_or_else(|| Path::new(""));
    let stem = output
        .file_stem()
        .map(|s| s.to_string_lossy().into_owned())
        .unwrap_or_else(|| "walkforward".to_string());
    parent.join(format!("{stem}.{suffix_stem}.{extension}"))
}

fn write_walkforward_csv(
    path: &Path,
    union_columns: &[String],
    metric_columns: &[(String, String)],
    smoothed_path: Option<&str>,
    rows: &[crate::spec::optimize::WalkForwardRow],
) -> Result<()> {
    if let Some(parent) = path.parent()
        && !parent.as_os_str().is_empty()
    {
        std::fs::create_dir_all(parent)
            .with_context(|| format!("creating output dir `{}`", parent.display()))?;
    }
    let mut writer = csv::WriterBuilder::new()
        .delimiter(b',')
        .from_path(path)
        .with_context(|| format!("creating `{}`", path.display()))?;

    let mut header: Vec<String> = vec![
        "fold".into(),
        "is_start".into(),
        "is_end".into(),
        "oos_start".into(),
        "oos_end".into(),
    ];
    header.extend(union_columns.iter().cloned());
    for (name, _) in metric_columns {
        header.push(format!("{name}_is"));
        header.push(format!("{name}_oos"));
        header.push(format!("{name}_wfe"));
    }
    // Under `--smooth` the IS argmax is no longer what selected the fold — the
    // neighbourhood average is. Emit it, and the support behind it, so the
    // per-fold choice is auditable against the raw `_is` column beside it.
    if let Some(path) = smoothed_path {
        header.push(format!("{path}_is_smoothed"));
        header.push(format!("{path}_is_support"));
    }
    writer.write_record(&header)?;

    // Same trick as the plain-grid writer: flatten once per Metrics document
    // and index by column position.
    let sample = rows.first().map(|r| &r.oos_metrics);
    let positions: Vec<Option<ColumnPos>> = if let Some(sample) = sample {
        let flat = metrics::flatten(sample);
        metric_columns
            .iter()
            .map(|(_, path)| flat.iter().position(|(k, _)| *k == path.as_str()))
            .collect()
    } else {
        vec![None; metric_columns.len()]
    };

    let cell = |v: Option<Real>| v.map(format_number).unwrap_or_default();
    for row in rows {
        let mut record: Vec<String> = vec![
            row.fold.to_string(),
            row.is_start.to_string(),
            row.is_end.to_string(),
            row.oos_start.to_string(),
            row.oos_end.to_string(),
        ];
        record.extend(
            row.values
                .iter()
                .map(|v| v.as_ref().map(format_value).unwrap_or_default()),
        );
        let is_flat = metrics::flatten(&row.is_metrics);
        let oos_flat = metrics::flatten(&row.oos_metrics);
        for pos in &positions {
            let is_v = pos.and_then(|p| is_flat[p].1);
            let oos_v = pos.and_then(|p| oos_flat[p].1);
            let wfe = match (is_v, oos_v) {
                (Some(i), Some(o)) if i.abs() > f64::EPSILON => Some(o / i),
                _ => None,
            };
            record.push(cell(is_v));
            record.push(cell(oos_v));
            record.push(cell(wfe));
        }
        if smoothed_path.is_some() {
            record.push(cell(row.is_smoothed.and_then(|s| s.value)));
            record.push(cell(row.is_smoothed.map(|s| s.support)));
        }
        writer.write_record(&record)?;
    }
    writer.flush()?;
    Ok(())
}

fn write_composite_equity_csv(path: &Path, equity: &[Real]) -> Result<()> {
    if let Some(parent) = path.parent()
        && !parent.as_os_str().is_empty()
    {
        std::fs::create_dir_all(parent)
            .with_context(|| format!("creating output dir `{}`", parent.display()))?;
    }
    let mut writer = csv::WriterBuilder::new()
        .delimiter(b',')
        .from_path(path)
        .with_context(|| format!("creating `{}`", path.display()))?;
    writer.write_record(["bar", "equity"])?;
    for (i, eq) in equity.iter().enumerate() {
        writer.write_record([i.to_string(), format_number(*eq)])?;
    }
    writer.flush()?;
    Ok(())
}

fn write_composite_metrics_yaml(path: &Path, m: &metrics::Metrics) -> Result<()> {
    if let Some(parent) = path.parent()
        && !parent.as_os_str().is_empty()
    {
        std::fs::create_dir_all(parent)
            .with_context(|| format!("creating output dir `{}`", parent.display()))?;
    }
    let yaml = serde_norway::to_string(m)
        .with_context(|| format!("serializing composite OOS metrics for `{}`", path.display()))?;
    std::fs::write(path, yaml).with_context(|| format!("writing `{}`", path.display()))?;
    Ok(())
}

fn print_walkforward_inputs(
    spec: &WalkForwardSpec,
    resolved: (usize, usize, usize),
    prefix_skip: usize,
    keep_unstable: bool,
    n_folds: usize,
    n_bars: usize,
    output: &Path,
) {
    let (is_b, oos_b, emb_b) = resolved;
    style::print_section("inputs");
    style::field(
        "windows",
        &format!("{spec}  →  IS={is_b}, OS={oos_b}, embargo={emb_b} (bars)"),
    );
    style::field(
        "prefix",
        &format!(
            "{prefix_skip} bars ({})",
            if keep_unstable {
                "keep_unstable → max(warm_up)"
            } else {
                "safe → max(stable)"
            }
        ),
    );
    style::field("folds", &format!("{n_folds}  (over {n_bars} bars)"));
    style::field("output", &format!("{}", output.display()));
    style::field_continuation(&format!(
        "+ {}",
        derive_sibling(output, "composite_oos_equity", "csv").display()
    ));
    style::field_continuation(&format!(
        "+ {}",
        derive_sibling(output, "composite_oos_metrics", "yml").display()
    ));
}

/// `  ruined is@N` / `  ruined oos@N` for a fold whose winner was wiped out on
/// one side of the split, else the empty string.
///
/// The fold table's `_is` / `_oos` / `_wfe` columns are bar-return metrics as
/// often as not, and those cannot see ruin — an efficiency of 0.9 between two
/// dead accounts reads as a well-behaved fold. Ruin in the *in-sample* slice
/// can no longer make a cell win (`ranking_lookup`), so what shows up here is
/// the case that survives selection: the fold's winner was solvent when it was
/// picked and blew up out of sample. That is the single most important thing a
/// walk-forward can tell you, and it had no line.
///
/// Bars are absolute, matching the `[is_start..is_end)` labels on the same row;
/// `report_slice` reports each slice's ruin bar relative to the slice.
fn fold_ruin_marker(row: &crate::spec::optimize::WalkForwardRow) -> String {
    let mut parts: Vec<String> = Vec::new();
    if let Some(b) = row.is_metrics.run.ruin_bar {
        parts.push(format!("is@{}", row.is_start + b));
    }
    if let Some(b) = row.oos_metrics.run.ruin_bar {
        parts.push(format!("oos@{}", row.oos_start + b));
    }
    if parts.is_empty() {
        String::new()
    } else {
        format!("  {} {}", style::yellow("ruined"), parts.join(" "))
    }
}

fn print_walkforward_summary(
    rows: &[crate::spec::optimize::WalkForwardRow],
    metric_columns: &[(String, String)],
    best_by: Option<&(String, String, Direction)>,
) {
    println!();
    style::print_section("folds");
    if let Some((label, path, _dir)) = best_by {
        for row in rows {
            let is_v = lookup(&row.is_metrics, path);
            let oos_v = lookup(&row.oos_metrics, path);
            let wfe = match (is_v, oos_v) {
                (Some(i), Some(o)) if i.abs() > f64::EPSILON => Some(o / i),
                _ => None,
            };
            let params_label: String = row
                .values
                .iter()
                .filter_map(|v| v.as_ref().map(format_value))
                .collect::<Vec<_>>()
                .join(", ");
            style::field(
                &format!("#{}", row.fold),
                &format!(
                    "[{}..{})/[{}..{})  {label}_is={} _oos={} _wfe={}  params: {params_label}{}",
                    row.is_start,
                    row.is_end,
                    row.oos_start,
                    row.oos_end,
                    is_v.map(format_number).unwrap_or_else(|| "—".into()),
                    oos_v.map(format_number).unwrap_or_else(|| "—".into()),
                    wfe.map(format_number).unwrap_or_else(|| "—".into()),
                    fold_ruin_marker(row),
                ),
            );
        }
    } else {
        // No --best-by: dump the first `-m` column's IS/OOS for orientation.
        let path = metric_columns.first().map(|(_, p)| p.as_str());
        for row in rows {
            let (is_str, oos_str) = match path {
                Some(p) => (
                    lookup(&row.is_metrics, p)
                        .map(format_number)
                        .unwrap_or_else(|| "—".into()),
                    lookup(&row.oos_metrics, p)
                        .map(format_number)
                        .unwrap_or_else(|| "—".into()),
                ),
                None => ("—".into(), "—".into()),
            };
            style::field(
                &format!("#{}", row.fold),
                &format!(
                    "[{}..{})/[{}..{})  is={is_str} oos={oos_str}{}",
                    row.is_start,
                    row.is_end,
                    row.oos_start,
                    row.oos_end,
                    fold_ruin_marker(row),
                ),
            );
        }
    }
}
