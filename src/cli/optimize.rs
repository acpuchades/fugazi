//! `optimize` — parameter-grid sweep over a strategy.
//!
//! Same shape as `run`: one strategy YAML + `--series` bars + calendar/rf +
//! `--params` for **baseline** placeholder scalars — shared across every grid
//! point and rejected here if they look like axes (a JSON list or a range).
//! The sweep dimensions live in `--grid` (repeatable): each `-g/--grid` flag
//! declares one **subgrid**, with the same term grammar as `--params` plus two
//! extra value forms — `NAME=[v1,v2,v3]` (a discrete list) and
//! `NAME=start..end[:step]` (an inclusive numeric range). Each subgrid layers
//! over `--params` and takes the Cartesian product of its axes; the full grid
//! is the disjoint **union** of the subgrids' point sets, useful when a
//! parameter only makes sense conditionally on another (e.g. one subgrid
//! sweeps `slow` around a slow entry, another sweeps `atr_mult` around a stop).
//! For each grid point we drive [`crate::backtest::evaluate`] and record its
//! [`crate::metrics`] document.
//!
//! Output is one `,`-delimited CSV file (`-o/--output`) with one row per grid
//! point: axis columns first, then one column per `-m/--metrics` name — or,
//! when `-m` is omitted, one column per metric in the whole catalogue. The
//! axis column set is the **union** of every subgrid's axis names plus any
//! scalar that takes different values across subgrids (name-sorted), and cells
//! are left empty for rows whose subgrid doesn't touch that name — so a
//! stacked sweep produces a sparse but rectangular CSV. Column headers are the
//! canonical dotted path (`sharpe` on the command line still lands under
//! `risk_adjusted.sharpe`). Rows are sorted by `--best-by` when it's set
//! (descending for max-oriented metrics like `sharpe`, ascending for
//! min-oriented ones like `max_pct`); otherwise the row order follows the
//! subgrid-then-Cartesian enumeration.
//!
//! The grid runs on a rayon thread pool (`-j/--jobs` picks the size; default is
//! rayon's own default — one worker per logical CPU). Each combination
//! independently clones the parsed strategy tree, applies substitution, and
//! evaluates — no shared mutable state, no locking on the hot path. The outer
//! par_iter carries a `with_min_len` sized to roughly 16 chunks per worker, so
//! a huge grid of cheap combos amortizes task overhead while a small grid still
//! spreads one combo per worker.

use std::collections::HashMap;
use std::num::NonZeroUsize;
use std::path::Path;

use anyhow::{Context, Result, anyhow, bail};
use fugazi::prelude::*;
use rayon::prelude::*;
use serde_json::Value;

use crate::backtest;
use crate::calendar::{
    self, AssetClass, BarsPerYearSpec, Frequency, ScopedFrequency, WalkForwardSpec, WindowSpec,
};
use crate::costs::CostConfig;
use crate::data::DataFrame;
use crate::imports;
use crate::input::StrategyKind;
use crate::input;
use crate::metrics;
use crate::params;
use crate::run::join_universe_by_time;
use crate::spec::{BasketStrategySpec, MultiAssetStrategySpec, SingleStrategySpec};
use crate::style;

/// Sort direction of a `--best-by` optimization: descending = higher is better
/// (Sharpe, CAGR, …); ascending = lower is better (drawdown, volatility, VaR, …).
#[derive(Clone, Copy, Debug, PartialEq)]
pub(crate) enum Direction {
    Descending,
    Ascending,
}

/// One sweep axis: `NAME → values`, preserving the enumeration order.
pub(crate) type Axis = (String, Vec<Value>);
/// A fixed (scalar) params table + the sweep axes carved out of it.
type Partition = (HashMap<String, Value>, Vec<Axis>);

/// Threaded-in inputs, same shape as [`crate::run::RunOptions`].
pub struct OptimizeOptions<'a> {
    pub cash: Real,
    /// The shape of the strategy YAML being swept — single-asset, basket,
    /// multi-asset. `Pairs` is rejected upstream; single-asset is the
    /// legacy path (one symbol probed from the spec + one atom slice from
    /// the frame). Basket / multi-asset take the union of every symbol in
    /// the frame as universe and time-align the per-symbol streams into
    /// one shared snapshot sequence (same shape as `run_basket` /
    /// `run_multi`).
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
    /// `max(warm_up_period)` at the head of the atom slice, not
    /// `max(stable_period)`. Lets IIR settling bleed into the first IS window.
    /// No-op without `walkforward`.
    pub keep_unstable: bool,
    /// `-k/--risk-aversion`: shift each grid point's `--best-by` cross-window
    /// mean *against* it by this many standard deviations before ranking
    /// (direction-aware: `mean − k·std` descending, `mean + k·std` ascending).
    /// `0.0` = rank by the plain mean. Only meaningful with `windowed`.
    pub risk_aversion: Real,
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
}

/// CLI entry for the `optimize` command: marshal `opts` into inputs
/// [`optimize`] can consume (parse the strategy text, fold subgrids, resolve
/// the candle slice for the strategy's symbol), invoke the sweep, then write
/// `metrics.csv` / print the inputs + best blocks.
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
            let (fixed, axes) = split_axes(&merged)
                .with_context(|| format!("--grid #{}", idx + 1))?;
            let combos = cartesian(&axes);
            Ok::<_, anyhow::Error>(Subgrid { fixed, axes, combos })
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

    // Imports splice once, up front: the resulting base value is what every
    // grid point's `!param` substitution runs over, so a shared fragment costs
    // one read no matter how large the sweep.
    let base_value = input::parse_value_at(opts.strategy_text, opts.strategy_label)?;
    let base_value =
        imports::resolve(base_value, opts.strategy_dir).context("resolving strategy imports")?;

    match opts.strategy_kind {
        StrategyKind::Single => run_single(&opts, subgrids, frame, &base_value),
        StrategyKind::Basket | StrategyKind::Multi => {
            run_multi_symbol(&opts, subgrids, frame, &base_value)
        }
        StrategyKind::Pairs => bail!(
            "`fugazi optimize` doesn't support `pairs:` strategies (rejected at dispatch)"
        ),
    }
}

/// The single-asset grid path — probes the strategy's symbol once, fetches
/// its atom slice, and drives the sweep through a
/// [`SingleStrategySpec`]-typed closure. Handles walk-forward too (which is
/// only wired for single-asset strategies).
fn run_single(
    opts: &OptimizeOptions,
    subgrids: Vec<Subgrid>,
    frame: &DataFrame,
    base_value: &Value,
) -> Result<()> {
    // Resolve the strategy's symbol from a probe built with the first subgrid's
    // first combo. Every other subgrid must resolve to the same symbol —
    // otherwise the atoms slice we're about to fetch is only valid for one of
    // them and the others silently backtest against the wrong data. Cheaper to
    // validate here than debug later.
    let probe_spec = build_spec(base_value, &probe_params(&subgrids[0]))?;
    let probe_symbol = probe_spec.symbol.clone();
    for (idx, subgrid) in subgrids.iter().enumerate().skip(1) {
        let other = build_spec(base_value, &probe_params(subgrid))?;
        if other.symbol != probe_symbol {
            bail!(
                "--grid #{} resolves to symbol `{}`, but --grid #1 resolves to `{}` — every \
                 subgrid must trade the same symbol (loading multiple symbol slices from one \
                 frame is not supported)",
                idx + 1,
                other.symbol,
                probe_symbol,
            );
        }
    }
    let series = frame.atoms(&probe_symbol)?;
    let atoms = series.atoms;
    let skipped_overlay_columns = series.skipped_columns;

    // The effective bar cadence, now that the strategy's symbol is known:
    // a symbol-matching `-f/--frequency` entry wins, else auto-detect from
    // the atoms' `time` field (populated by the loader). Threaded into both
    // the annualization (`bars_per_year`) and the per-grid-point cost
    // resolution so freq-scoped `--costs` entries also see the detected cadence.
    let effective_freq = calendar::pick_frequency(opts.frequency, &probe_symbol)
        .or_else(|| calendar::detect_frequency_from_atoms(atoms.iter().map(|(_, a)| a)));
    let bars_per_year =
        calendar::pick_bars_per_year(opts.bars_per_year, &probe_symbol, effective_freq)
            .unwrap_or_else(|| calendar::resolve(None, opts.asset_class, effective_freq));

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

    // Walk-forward branch: an independent driver — different outputs and a
    // fold-scoped per-row measurement rather than one whole-run reduction. The
    // grid loop shape is similar, but the emitted artifacts have their own
    // schema (per-fold winners + composite OOS), so we don't try to squeeze it
    // through the [`Sweep`] shape.
    if let Some(walkforward_spec) = opts.walkforward {
        let schema = backtest::schema_from_atoms(&atoms);
        let keep_unstable = opts.keep_unstable;
        let cash = opts.cash;
        let cost_config = opts.cost_config;
        let atoms_ref = &atoms;
        let schema_ref = &schema;
        let probe = |params: &HashMap<String, Value>| -> Result<usize> {
            let s = build_spec(base_value, params)?;
            let built = s.build(cash, schema_ref);
            Ok(if keep_unstable {
                built.warm_up_period()
            } else {
                built.stable_period()
            })
        };
        let run_backtest =
            |params: &HashMap<String, Value>| -> Result<fugazi::RunReport<String>> {
                let s = build_spec(base_value, params)?;
                let costs = cost_config.resolve(&s.symbol, effective_freq);
                Ok(backtest::measured_report(&s, atoms_ref, cash, costs))
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
            opts.output,
            opts.jobs,
            opts.quiet,
            &skipped_overlay_columns,
            opts.cash,
        );
    }

    let cost_config = opts.cost_config;
    let atoms_ref = &atoms;
    let windowed_n = windowed_bars.map(NonZeroUsize::get);
    let evaluate_row = move |params: &HashMap<String, Value>| -> Result<Evaluation> {
        let spec = build_spec(base_value, params)?;
        Ok(match windowed_n {
            Some(w) => Evaluation::Windowed(backtest::evaluate_windowed(
                &spec,
                atoms_ref,
                opts.cash,
                bars_per_year,
                opts.risk_free_rate,
                cost_config,
                effective_freq,
                w,
                seconds_per_bar,
            )),
            None => Evaluation::Whole(Box::new(backtest::evaluate(
                &spec,
                atoms_ref,
                opts.cash,
                bars_per_year,
                opts.risk_free_rate,
                cost_config,
                effective_freq,
                seconds_per_bar,
            ))),
        })
    };

    let sweep = optimize(
        subgrids,
        windowed_n,
        &opts.metrics,
        opts.best_by.as_deref(),
        opts.risk_aversion,
        opts.jobs,
        evaluate_row,
    )?;

    write_grid_csv(
        opts.output,
        &sweep.union_columns,
        &sweep.metric_columns,
        sweep.windowed,
        sweep.deflated_sharpe_context,
        &sweep.rows,
    )?;

    if !opts.quiet {
        style::print_header("optimize", "sweep a strategy over a parameter grid");
        print_skipped_overlay_warning(&skipped_overlay_columns);
        print_inputs_block(opts, windowed_bars, &sweep.subgrid_summaries, &sweep.rows);
        // A "best" row only means something when the user gave us a metric to
        // rank by. Without one, the sweep has produced a CSV but no verdict.
        if sweep.best_by.is_some() {
            print_best_block(
                &sweep.union_columns,
                &sweep.metric_columns,
                sweep.best_by.as_ref(),
                opts.risk_aversion,
                &sweep.rows,
            );
        }
    }
    Ok(())
}

/// The basket / multi-asset grid path — the tradeable universe is every
/// symbol in `frame`, and per-bar snapshots are the outer-join of every
/// symbol's atom stream on `time` (same shape as `run_basket` /
/// `run_multi`). Walk-forward is not wired for this path; bails with a
/// specific message. `--windowed` is supported via the basket / multi
/// twins of the single-asset windowed evaluator.
fn run_multi_symbol(
    opts: &OptimizeOptions,
    subgrids: Vec<Subgrid>,
    frame: &DataFrame,
    base_value: &Value,
) -> Result<()> {
    let kind_label = match opts.strategy_kind {
        StrategyKind::Basket => "basket",
        StrategyKind::Multi => "multi",
        _ => unreachable!("run_multi_symbol only dispatched for basket/multi"),
    };

    let universe = frame.symbols();
    if universe.is_empty() {
        bail!(
            "no symbols found in the input series — `{kind_label}:` optimization needs at least \
             one traded asset"
        );
    }
    // Per-symbol atom streams, sorted by time. `DataFrame::atoms` walks a
    // BTreeMap so each per-symbol stream is already ascending; the joiner
    // then N-way merges them into shared bar-tagged snapshots.
    let per_symbol: Vec<(String, Vec<(String, Atom)>)> = universe
        .iter()
        .map(|sym| Ok::<_, anyhow::Error>((sym.clone(), frame.atoms(sym)?.atoms)))
        .collect::<Result<_>>()?;
    let (_bars, snapshots) = join_universe_by_time(&per_symbol);
    if snapshots.is_empty() {
        bail!(
            "no bars found in the input series across the {} discovered symbol(s)",
            universe.len()
        );
    }

    // Cadence: try the representative (first) symbol's --frequency scope, then
    // fall back to detection from that symbol's timestamps. Matches
    // `run_basket` / `run_multi`.
    let representative = &universe[0];
    let effective_freq = calendar::pick_frequency(opts.frequency, representative).or_else(|| {
        per_symbol
            .iter()
            .find(|(s, _)| s == representative)
            .and_then(|(_, atoms)| {
                calendar::detect_frequency_from_atoms(atoms.iter().map(|(_, a)| a))
            })
    });
    let bars_per_year =
        calendar::pick_bars_per_year(opts.bars_per_year, representative, effective_freq)
            .unwrap_or_else(|| calendar::resolve(None, opts.asset_class, effective_freq));

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
    let universe_ref = &universe;
    let windowed_n = windowed_bars.map(NonZeroUsize::get);
    let kind = opts.strategy_kind;

    let evaluate_row = move |params: &HashMap<String, Value>| -> Result<Evaluation> {
        Ok(match kind {
            StrategyKind::Basket => {
                let spec = build_basket_spec(base_value, params)?;
                match windowed_n {
                    Some(w) => Evaluation::Windowed(backtest::evaluate_windowed_basket(
                        &spec,
                        snapshots_ref,
                        universe_ref,
                        opts.cash,
                        bars_per_year,
                        opts.risk_free_rate,
                        cost_config,
                        effective_freq,
                        w,
                        seconds_per_bar,
                    )),
                    None => Evaluation::Whole(Box::new(backtest::evaluate_basket(
                        &spec,
                        snapshots_ref,
                        universe_ref,
                        opts.cash,
                        bars_per_year,
                        opts.risk_free_rate,
                        cost_config,
                        effective_freq,
                        seconds_per_bar,
                    ))),
                }
            }
            StrategyKind::Multi => {
                let spec = build_multi_spec(base_value, params)?;
                match windowed_n {
                    Some(w) => Evaluation::Windowed(backtest::evaluate_windowed_multi(
                        &spec,
                        snapshots_ref,
                        universe_ref,
                        opts.cash,
                        bars_per_year,
                        opts.risk_free_rate,
                        cost_config,
                        effective_freq,
                        w,
                        seconds_per_bar,
                    )),
                    None => Evaluation::Whole(Box::new(backtest::evaluate_multi(
                        &spec,
                        snapshots_ref,
                        universe_ref,
                        opts.cash,
                        bars_per_year,
                        opts.risk_free_rate,
                        cost_config,
                        effective_freq,
                        seconds_per_bar,
                    ))),
                }
            }
            _ => unreachable!("run_multi_symbol only dispatched for basket/multi"),
        })
    };

    let sweep = optimize(
        subgrids,
        windowed_n,
        &opts.metrics,
        opts.best_by.as_deref(),
        opts.risk_aversion,
        opts.jobs,
        evaluate_row,
    )?;

    write_grid_csv(
        opts.output,
        &sweep.union_columns,
        &sweep.metric_columns,
        sweep.windowed,
        sweep.deflated_sharpe_context,
        &sweep.rows,
    )?;

    if !opts.quiet {
        style::print_header("optimize", "sweep a strategy over a parameter grid");
        print_inputs_block(opts, windowed_bars, &sweep.subgrid_summaries, &sweep.rows);
        if sweep.best_by.is_some() {
            print_best_block(
                &sweep.union_columns,
                &sweep.metric_columns,
                sweep.best_by.as_ref(),
                opts.risk_aversion,
                &sweep.rows,
            );
        }
    }
    Ok(())
}

/// The basket / multi-asset walk-forward driver — the `--walkforward`
/// peer of [`run_multi_symbol`]'s grid sweep, sharing the strategy-agnostic
/// [`walkforward_run`] via closures.
///
/// **Lazy readiness probing.** Basket / multi strategies build per-symbol
/// chains on first sight of a snapshot — a freshly-constructed strategy
/// has no chains yet and `stable_period()` reports only the rebalance
/// signal's period. To reveal the grid-wide max the walk-forward layout
/// needs, we feed each throwaway probe strategy one *synthetic* snapshot
/// containing every universe symbol with a dummy [`Atom`], triggering the
/// factories on every symbol before reading `stable_period()` /
/// `warm_up_period()`. The dummy atom carries no overlays and a zero
/// candle — safe because the probe never trades, only exercises chain
/// construction.
#[allow(clippy::too_many_arguments)]
fn run_multi_symbol_walkforward(
    opts: &OptimizeOptions,
    subgrids: Vec<Subgrid>,
    base_value: &Value,
    snapshots: &[fugazi::types::Snapshot<String>],
    universe: &[String],
    walkforward_spec: WalkForwardSpec,
    bars_per_year: Real,
    effective_freq: Option<Frequency>,
    seconds_per_bar: Option<Real>,
) -> Result<()> {
    let schema = backtest::schema_from_snapshots(snapshots);
    let keep_unstable = opts.keep_unstable;
    let cash = opts.cash;
    let cost_config = opts.cost_config;
    let kind = opts.strategy_kind;

    // Synthetic single-snapshot probe: one dummy atom per universe symbol
    // so the strategy's per-symbol factories fire on the first update() call.
    // The probe strategy never trades — just exposes stable/warm-up state.
    let probe_snapshot: fugazi::types::Snapshot<String> = {
        let mut s = fugazi::types::Snapshot::<String>::new();
        let dummy_atom = Atom::new(Candle::new(0.0, 0.0, 0.0, 0.0, 0.0));
        for sym in universe {
            s.push(Some(sym.clone()), None, dummy_atom.clone());
        }
        s
    };

    // `TradingCosts` isn't `Clone` (boxed trait objects inside), so the
    // per-symbol cost bundle is rebuilt inside the run closure for every
    // grid row rather than cloned. `cost_config.resolve` is cheap — a
    // HashMap lookup + trivial model construction — so the cost is
    // negligible next to the backtest itself.
    let schema_ref = &schema;
    let probe_snap_ref = &probe_snapshot;
    let snapshots_ref = snapshots;

    let probe = |params: &HashMap<String, Value>| -> Result<usize> {
        match kind {
            StrategyKind::Basket => {
                let spec = build_basket_spec(base_value, params)?;
                let mut built = spec.build(cash, schema_ref);
                // Probe: one synthetic snapshot triggers the lazy per-symbol
                // chain construction so `stable_period()` reflects the
                // fully-populated worst case.
                built.update(probe_snap_ref.clone());
                Ok(if keep_unstable {
                    built.warm_up_period()
                } else {
                    built.stable_period()
                })
            }
            StrategyKind::Multi => {
                let spec = build_multi_spec(base_value, params)?;
                let mut built = spec.build(cash, schema_ref);
                built.update(probe_snap_ref.clone());
                Ok(if keep_unstable {
                    built.warm_up_period()
                } else {
                    built.stable_period()
                })
            }
            _ => unreachable!("run_multi_symbol_walkforward only dispatched for basket/multi"),
        }
    };

    let build_per_symbol_costs = || -> Vec<(String, TradingCosts)> {
        universe
            .iter()
            .map(|s| (s.clone(), cost_config.resolve(s, effective_freq)))
            .collect()
    };
    let run_backtest =
        |params: &HashMap<String, Value>| -> Result<fugazi::RunReport<String>> {
            let report = match kind {
                StrategyKind::Basket => {
                    let spec = build_basket_spec(base_value, params)?;
                    backtest::measured_report_from_strategy(
                        || spec.build(cash, schema_ref),
                        snapshots_ref,
                        cash,
                        build_per_symbol_costs(),
                    )
                }
                StrategyKind::Multi => {
                    let spec = build_multi_spec(base_value, params)?;
                    backtest::measured_report_from_strategy(
                        || spec.build(cash, schema_ref),
                        snapshots_ref,
                        cash,
                        build_per_symbol_costs(),
                    )
                }
                _ => unreachable!("run_multi_symbol_walkforward only dispatched for basket/multi"),
            };
            Ok(report)
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
        opts.output,
        opts.jobs,
        opts.quiet,
        &no_skipped,
        opts.cash,
    )
}

/// Params for the probe spec: subgrid's fixed scalars + the first value of each
/// of its axes. When the subgrid has no axes this is just the fixed map.
fn probe_params(subgrid: &Subgrid) -> HashMap<String, Value> {
    let combo: Vec<Value> = subgrid.axes.iter().map(|(_, v)| v[0].clone()).collect();
    combine_params(&subgrid.fixed, &subgrid.axes, &combo)
}

/// Enumerate every subgrid's Cartesian product, drive `evaluate_row` on
/// each parameter combination (whole-run or windowed at the callsite's
/// discretion), project each result onto the union of axis-column names,
/// and rank the rows by `best_by`'s ranking value — direction-aware, with
/// `risk_aversion` shifting a windowed row's mean *against* it by k·std
/// so dispersion is always penalized. Pure: no filesystem, no printing.
/// The CLI's [`run`] wraps it with argument marshaling + CSV write +
/// console output.
///
/// `evaluate_row` owns everything strategy-specific — the base YAML
/// value, the atom / snapshot stream(s), cost config, and the choice of
/// whole-run vs windowed reduction. That closure is the seam Single /
/// Basket / Multi share — the sweep loop itself is strategy-type-agnostic.
/// `windowed` mirrors the closure's mode (used only to shape column
/// headers and DSR aggregation).
pub(crate) fn optimize<F>(
    subgrids: Vec<Subgrid>,
    windowed: Option<usize>,
    metric_names: &[String],
    best_by: Option<&str>,
    risk_aversion: Real,
    jobs: Option<usize>,
    evaluate_row: F,
) -> Result<Sweep>
where
    F: Fn(&HashMap<String, Value>) -> Result<Evaluation> + Sync,
{
    // A negative k would *reward* dispersion — the opposite of what the flag
    // is for. (Presence alongside `-w`/`--best-by` is enforced by clap.)
    if risk_aversion < 0.0 {
        bail!("--risk-aversion must be >= 0 (got {risk_aversion})");
    }
    // `run` has already validated the subgrid list; `optimize` still asserts
    // the invariants it relies on (non-empty list, non-empty combos in each).
    assert!(!subgrids.is_empty(), "optimize: called with zero subgrids");

    let union_columns = compute_union_columns(&subgrids);
    let subgrid_summaries = subgrids
        .iter()
        .map(|s| (subgrid_label(s, &union_columns), s.points()))
        .collect();

    // Flat enumeration of (subgrid_idx, combo_idx) so we can process the whole
    // stacked grid on one par_iter without nested rayon.
    let plan: Vec<(usize, usize)> = subgrids
        .iter()
        .enumerate()
        .flat_map(|(si, s)| (0..s.combos.len()).map(move |ci| (si, ci)))
        .collect();

    // Probe the first grid point once, up front: it validates the strategy YAML
    // (early error) and gives us a Metrics document to resolve `--metrics` and
    // `--best-by` names against before spinning up the pool.
    let (first_si, first_ci) = plan[0];
    let first_params = combine_params(
        &subgrids[first_si].fixed,
        &subgrids[first_si].axes,
        &subgrids[first_si].combos[first_ci],
    );
    let first_eval = evaluate_row(&first_params)?;
    let first_metrics = sample_metrics(&first_eval).cloned().ok_or_else(|| {
        anyhow!(
            "optimize: first grid point produced no metrics document — the strategy \
             may not run over the provided data (empty snapshot stream?)"
        )
    })?;

    // Resolve column paths once — errors here catch typos before the sweep.
    // An empty `-m/--metrics` defaults to the whole catalogue (one column per
    // `metrics::flatten` leaf). Columns are always the canonical dotted path, so
    // the header carries the section prefix even when the user matched a metric
    // by its short leaf name (`-m sharpe` → column `risk_adjusted.sharpe`).
    let metric_columns: Vec<(String, String)> = if metric_names.is_empty() {
        metrics::flatten(&first_metrics)
            .into_iter()
            .map(|(path, _)| (path.to_string(), path.to_string()))
            .collect()
    } else {
        metric_names
            .iter()
            .map(|name| {
                let (path, _) = metrics::resolve_metric(name, &first_metrics)?;
                Ok::<_, anyhow::Error>((path.clone(), path))
            })
            .collect::<Result<Vec<_>>>()?
    };

    let best_by = best_by
        .map(|name| {
            let (path, _) = metrics::resolve_metric(name, &first_metrics)?;
            let direction = direction_for(&path).ok_or_else(|| {
                anyhow!(
                    "--best-by `{name}` has no built-in direction; pass one whose \
                     direction is known (e.g. sharpe, sortino, cagr_pct, max_pct, \
                     ulcer_index, annualized_volatility_pct)"
                )
            })?;
            Ok::<_, anyhow::Error>((path.clone(), path, direction))
        })
        .transpose()?;

    // Run the grid. The first plan entry is already computed; the rest run
    // on the pool in parallel.
    let pool = crate::pool::build_pool(jobs)?;

    // Chunk the outer par_iter so a huge grid doesn't drown rayon in one task
    // per combo (task overhead dominates when combos are cheap), while a small
    // grid still gets one combo per worker. Target ~16 chunks per worker so
    // work-stealing still balances tail imbalance from combo-to-combo cost
    // variance. `plan[1..]` skips the already-computed first entry.
    let workers = pool.current_num_threads().max(1);
    let remaining_len = plan.len().saturating_sub(1);
    let min_len = remaining_len.div_ceil(workers * 16).max(1);

    let subgrids_ref = &subgrids;
    let union_ref = &union_columns;
    let evaluate_ref = &evaluate_row;
    let remaining: Vec<Row> = pool.install(|| {
        plan[1..]
            .par_iter()
            .with_min_len(min_len)
            .map(|&(si, ci)| {
                let subgrid = &subgrids_ref[si];
                let combo = &subgrid.combos[ci];
                let params = combine_params(&subgrid.fixed, &subgrid.axes, combo);
                let eval = evaluate_ref(&params)?;
                Ok::<_, anyhow::Error>(Row {
                    values: project_row(subgrid, combo, union_ref),
                    eval,
                })
            })
            .collect::<Result<Vec<_>>>()
    })?;

    let mut rows: Vec<Row> = Vec::with_capacity(plan.len());
    rows.push(Row {
        values: project_row(
            &subgrids[first_si],
            &subgrids[first_si].combos[first_ci],
            &union_columns,
        ),
        eval: first_eval,
    });
    rows.extend(remaining);

    // Sort by --best-by, direction-aware; None cells sort last regardless.
    if let Some((_, ref path, direction)) = best_by {
        sort_by_metric(&mut rows, path, direction, risk_aversion);
    }

    // Grid-wide DSR context — computed the same way for whole-run and windowed
    // sweeps; see the field's rustdoc for the windowed-mode aggregation.
    let deflated_sharpe_context = compute_dsr_context(&rows);

    Ok(Sweep {
        union_columns,
        subgrid_summaries,
        metric_columns,
        best_by,
        rows,
        windowed: windowed.is_some(),
        deflated_sharpe_context,
    })
}

/// Extract a [`metrics::Metrics`] document from an evaluation — the whole-run
/// document, or the first window's when the row was reduced windowed. Used
/// by [`optimize`] to resolve `--metrics` / `--best-by` names against the
/// probe row before the sweep spins up. `None` when a windowed row is
/// empty (an unlikely edge case guarded against upstream).
fn sample_metrics(eval: &Evaluation) -> Option<&metrics::Metrics> {
    match eval {
        Evaluation::Whole(m) => Some(m.as_ref()),
        Evaluation::Windowed(ws) => ws.first().map(|w| &w.metrics),
    }
}

/// Substitute a params table into the base strategy value, then typed-parse
/// as a [`BasketStrategySpec`]. Basket twin of [`build_spec`].
fn build_basket_spec(base: &Value, params: &HashMap<String, Value>) -> Result<BasketStrategySpec> {
    let value = params::substitute(base.clone(), params)?;
    Ok(serde_json::from_value(value)?)
}

/// Substitute a params table into the base strategy value, then typed-parse
/// as a [`MultiAssetStrategySpec`]. Multi-asset twin of [`build_spec`].
fn build_multi_spec(
    base: &Value,
    params: &HashMap<String, Value>,
) -> Result<MultiAssetStrategySpec> {
    let value = params::substitute(base.clone(), params)?;
    Ok(serde_json::from_value(value)?)
}

/// The union of axis-column names across every subgrid: every axis name, plus
/// every scalar name whose effective value differs across subgrids (or is
/// absent in at least one). Name-sorted so the header is stable regardless of
/// flag order.
fn compute_union_columns(subgrids: &[Subgrid]) -> Vec<String> {
    use std::collections::BTreeSet;
    let mut columns: BTreeSet<String> = BTreeSet::new();
    // Every axis name — an axis is by definition varying.
    for s in subgrids {
        for (name, _) in &s.axes {
            columns.insert(name.clone());
        }
    }
    // Scalar names that either take different values or aren't present
    // everywhere. `effective_scalar` returns `None` when the subgrid doesn't
    // touch the name at all — that counts as a distinct "value" for the union
    // check (so a name present in one subgrid and missing from another still
    // becomes a column with sparse cells).
    let scalar_names: BTreeSet<String> = subgrids
        .iter()
        .flat_map(|s| s.fixed.keys().cloned())
        .collect();
    for name in scalar_names {
        if columns.contains(&name) {
            continue;
        }
        let first = subgrids[0].fixed.get(&name);
        if subgrids.iter().skip(1).any(|s| s.fixed.get(&name) != first) {
            columns.insert(name);
        }
    }
    columns.into_iter().collect()
}

/// Project a subgrid's (fixed scalars, axis combo) onto the union columns
/// index. Populated from the axis first (per-combo value) then the fixed map;
/// `None` when the subgrid doesn't touch the name.
fn project_row(subgrid: &Subgrid, combo: &[Value], union_columns: &[String]) -> Vec<Option<Value>> {
    let axis_lookup: HashMap<&str, &Value> = subgrid
        .axes
        .iter()
        .zip(combo)
        .map(|((name, _), v)| (name.as_str(), v))
        .collect();
    union_columns
        .iter()
        .map(|name| {
            if let Some(v) = axis_lookup.get(name.as_str()) {
                Some((*v).clone())
            } else {
                subgrid.fixed.get(name).cloned()
            }
        })
        .collect()
}

/// A one-line summary of a subgrid for the inputs block, e.g.
/// `X="A", Y(10)`. Only names that appear in `union_columns` are surfaced —
/// so baseline scalars shared across every subgrid stay silent, and the label
/// carries only what makes this subgrid different. Axes appear as `NAME(N)`
/// (with `N` the point count on that axis); scalars as `NAME=value`. A
/// subgrid that neither overrides nor sweeps any union column reads
/// `"(baseline)"`.
fn subgrid_label(subgrid: &Subgrid, union_columns: &[String]) -> String {
    use std::collections::BTreeSet;
    let union: BTreeSet<&str> = union_columns.iter().map(String::as_str).collect();
    let axis_names: BTreeSet<&str> = subgrid.axes.iter().map(|(n, _)| n.as_str()).collect();
    let mut parts: Vec<String> = Vec::new();
    // Scalar entries that vary across subgrids. Name-sorted (BTreeSet).
    let mut scalars: Vec<(&str, &Value)> = subgrid
        .fixed
        .iter()
        .filter(|(k, _)| union.contains(k.as_str()) && !axis_names.contains(k.as_str()))
        .map(|(k, v)| (k.as_str(), v))
        .collect();
    scalars.sort_by_key(|(k, _)| *k);
    for (name, value) in scalars {
        parts.push(format!("{name}={}", format_value(value)));
    }
    // Axes in this subgrid's declaration order (already name-sorted by `split_axes`).
    for (name, values) in &subgrid.axes {
        parts.push(format!("{name}({})", values.len()));
    }
    if parts.is_empty() {
        "(baseline)".to_string()
    } else {
        parts.join(", ")
    }
}

/// Grid-wide inputs to the per-row DSR: `(n_trials, sample_variance_of_sharpe)`.
/// `None` when fewer than two rows have a defined Sharpe or the variance is
/// zero — DSR is meaningless in either case (no null distribution, no
/// dispersion to correct against). In windowed mode a row's Sharpe is the
/// cross-window mean of window Sharpes (see the [`Sweep`] field's rustdoc).
fn compute_dsr_context(rows: &[Row]) -> Option<(usize, Real)> {
    let sharpes: Vec<Real> = rows.iter().filter_map(row_summary_sharpe).collect();
    if sharpes.len() < 2 {
        return None;
    }
    let n = sharpes.len() as Real;
    let mean = sharpes.iter().sum::<Real>() / n;
    // Sample variance (ddof=1) — matches the reference variance-of-estimators
    // used across the Bailey/LdP DSR literature.
    let var = sharpes.iter().map(|s| (s - mean).powi(2)).sum::<Real>() / (n - 1.0);
    if !(var > 0.0 && var.is_finite()) {
        return None;
    }
    Some((sharpes.len(), var))
}

/// A row's summary Sharpe: the whole-run value in [`Evaluation::Whole`], the
/// cross-window arithmetic mean in [`Evaluation::Windowed`]. `None` when no
/// window has a defined Sharpe (all had zero variance, for instance).
fn row_summary_sharpe(row: &Row) -> Option<Real> {
    match &row.eval {
        Evaluation::Whole(m) => m.risk_adjusted.sharpe,
        Evaluation::Windowed(ws) => mean_of(ws.iter().map(|w| w.metrics.risk_adjusted.sharpe)),
    }
}

/// Arithmetic mean of the defined entries, or `None` when none are defined.
fn mean_of(iter: impl IntoIterator<Item = Option<Real>>) -> Option<Real> {
    let (sum, n) = iter
        .into_iter()
        .flatten()
        .fold((0.0_f64, 0_usize), |(s, k), v| (s + v, k + 1));
    if n == 0 { None } else { Some(sum / n as Real) }
}

/// The `(sharpe, skew, kurt, n_returns, bars_per_year)` tuple the per-row DSR
/// consumes. For a windowed row, skew / kurt are cross-window means and
/// `n_returns` is the summed window bar counts — the same aggregation the
/// windowed `_mean` columns already use, so this cell is comparable to them.
fn row_dsr_inputs(row: &Row) -> (Option<Real>, Option<Real>, Option<Real>, usize, Real) {
    match &row.eval {
        Evaluation::Whole(m) => (
            m.risk_adjusted.sharpe,
            m.returns.skewness,
            m.returns.kurtosis,
            m.run.bars,
            m.run.bars_per_year,
        ),
        Evaluation::Windowed(ws) => {
            let sharpe = mean_of(ws.iter().map(|w| w.metrics.risk_adjusted.sharpe));
            let skew = mean_of(ws.iter().map(|w| w.metrics.returns.skewness));
            let kurt = mean_of(ws.iter().map(|w| w.metrics.returns.kurtosis));
            let n_returns: usize = ws.iter().map(|w| w.metrics.run.bars).sum();
            // Every window under one row shares the same bars_per_year, so any
            // one is representative; `0.0` guards against an empty windowed
            // row (which will fail the `> 0.0` check downstream anyway).
            let bpy = ws
                .first()
                .map(|w| w.metrics.run.bars_per_year)
                .unwrap_or(0.0);
            (sharpe, skew, kurt, n_returns, bpy)
        }
    }
}

// ---------------------------------------------------------------------------
// Grid construction
// ---------------------------------------------------------------------------

/// One grid point's metric evaluation: the whole measured run reduced to a
/// single document, or (`-w/--windowed`) one document per non-overlapping
/// window, aggregated per metric as cross-window mean ± stddev.
pub(crate) enum Evaluation {
    /// Boxed: the document is ~50 fields, dwarfing the windowed variant's Vec.
    Whole(Box<metrics::Metrics>),
    Windowed(Vec<metrics::WindowMetrics>),
}

/// One folded subgrid: its scalar map (baseline layered under this subgrid's
/// `--grid` scalars, minus any name carved out as an axis) plus its axes
/// (name-sorted) and cartesian combos over those axes. A `--grid` flag with
/// only scalars yields one combo (the empty tuple) — a single grid point.
pub(crate) struct Subgrid {
    pub(crate) fixed: HashMap<String, Value>,
    pub(crate) axes: Vec<Axis>,
    pub(crate) combos: Vec<Vec<Value>>,
}

impl Subgrid {
    fn points(&self) -> usize {
        self.combos.len()
    }
}

/// One row of the grid, sparse across the union of every subgrid's axis
/// columns. `values[i]` is the value for `Sweep::union_columns[i]` — `None`
/// when this row's subgrid doesn't reference that name (the CSV writes the
/// empty cell; the best block skips it).
pub(crate) struct Row {
    pub(crate) values: Vec<Option<Value>>,
    pub(crate) eval: Evaluation,
}

/// Rows and metadata produced by [`optimize`], ready for the CLI to write out.
/// `rows` is sorted by `best_by`'s ranking value when `best_by` is `Some`,
/// otherwise it follows the subgrid-then-cartesian enumeration order.
pub(crate) struct Sweep {
    /// The union of every subgrid's axis names, plus every scalar name whose
    /// effective value differs across subgrids — name-sorted. This is exactly
    /// the CSV axis-column header, and it indexes each [`Row::values`].
    pub(crate) union_columns: Vec<String>,
    /// One entry per `--grid` flag, in flag order — for the inputs block
    /// breakdown. Each entry is `(axes label, point count)` where the label is
    /// e.g. `"X=\"A\", Y(10)"` (scalars inline, axes as `NAME(N)`); when the
    /// subgrid has neither a scalar override nor an axis it reads `"(baseline)"`.
    pub(crate) subgrid_summaries: Vec<(String, usize)>,
    /// Metric column paths resolved against the probe document (`name` → dotted
    /// path). Errors out of [`optimize`] if any name doesn't resolve.
    pub(crate) metric_columns: Vec<(String, String)>,
    /// The `--best-by` name, its resolved dotted path, and its direction.
    /// `None` when no `--best-by` was passed.
    pub(crate) best_by: Option<(String, String, Direction)>,
    pub(crate) rows: Vec<Row>,
    /// True iff `windowed` was set — the CSV writer uses this to emit
    /// `<name>_mean` / `<name>_std` columns per metric.
    pub(crate) windowed: bool,
    /// `(n_trials, Var[SR])` collected across the sweep, or `None` when the
    /// grid has fewer than two rows with a defined Sharpe or when the trial
    /// variance is zero — DSR is meaningless in either case. Consumed by the
    /// CSV writer to emit the `selection.deflated_sharpe` column: the per-row DSR
    /// against the grid-wide null (Bailey & López de Prado, 2014).
    ///
    /// Windowing regularizes but does not eliminate multiple-testing bias: the
    /// user still picked *this* cell out of `N`, and its cross-window mean
    /// Sharpe is still a max-of-many statistic. So DSR is also emitted in
    /// windowed mode, using each cell's cross-window mean Sharpe / skewness /
    /// kurtosis as its summary, and the sum of the cell's window bar counts as
    /// `n_returns`. Aggregating higher moments by cross-window mean is
    /// imperfect (it isn't the pooled-returns skewness), but it matches how the
    /// windowed CSV columns already summarize their metrics, so the number
    /// stays comparable to the other `_mean` cells.
    pub(crate) deflated_sharpe_context: Option<(usize, Real)>,
}

/// True iff `v` is axis-shaped — a JSON array or a `start..end[:step]`
/// range-shaped string. Used both to carve axes out of a subgrid table
/// (`split_axes`) and to reject axes in the `--params` baseline
/// (`reject_axes_in_params`) — one detector, one meaning.
fn is_axis_value(v: &Value) -> bool {
    match v {
        Value::Array(items) => !items.is_empty(),
        Value::String(s) => try_parse_range(s).is_some(),
        _ => false,
    }
}

/// Error if any `--params` value looks like a sweep axis — those must go
/// through `--grid`. The error names every offender so a user with several
/// mistakes fixes them all in one edit rather than one at a time.
pub(crate) fn reject_axes_in_params(params: &HashMap<String, Value>) -> Result<()> {
    let mut offenders: Vec<&str> = params
        .iter()
        .filter_map(|(k, v)| is_axis_value(v).then_some(k.as_str()))
        .collect();
    if offenders.is_empty() {
        return Ok(());
    }
    offenders.sort();
    bail!(
        "--params only accepts scalar values; move axis-shaped values to `--grid`: {}",
        offenders.join(", "),
    );
}

/// Partition the effective params table into fixed (scalar) entries and sweep
/// axes. An axis is either a `Value::Array` (JSON list) or a `Value::String`
/// matching the `start..end[:step]` range syntax. Insertion order isn't stable
/// on `HashMap`, so axes come out **sorted by name** — the sort key is the CSV
/// column order too, so a user gets the same output regardless of flag order.
fn split_axes(params: &HashMap<String, Value>) -> Result<Partition> {
    let mut fixed = HashMap::new();
    let mut axes: Vec<Axis> = Vec::new();
    for (k, v) in params {
        match v {
            Value::Array(items) => {
                if items.is_empty() {
                    bail!("--params axis `{k}` has an empty list");
                }
                axes.push((k.clone(), items.clone()));
            }
            Value::String(s) => match try_parse_range(s) {
                Some(values) => axes.push((k.clone(), values)),
                None => {
                    fixed.insert(k.clone(), v.clone());
                }
            },
            _ => {
                fixed.insert(k.clone(), v.clone());
            }
        }
    }
    axes.sort_by(|a, b| a.0.cmp(&b.0));
    Ok((fixed, axes))
}

/// `start..end[:step]` → the inclusive integer or float sequence. `None` for a
/// string that doesn't look like a range (so the caller falls back to
/// treating it as a fixed scalar string).
fn try_parse_range(s: &str) -> Option<Vec<Value>> {
    let (range, step) = match s.split_once(':') {
        Some((r, st)) => (r, Some(st)),
        None => (s, None),
    };
    let (start, end) = range.split_once("..")?;
    let start = start.trim();
    let end = end.trim();
    if start.is_empty() || end.is_empty() {
        return None;
    }
    // Prefer an integer range when start/end/step are all integers — it keeps
    // JSON integer typing (which is how `--params FAST=5` reads), which the
    // strategy spec's `usize` fields need.
    if let (Ok(s0), Ok(s1)) = (start.parse::<i64>(), end.parse::<i64>()) {
        let step_i = match step {
            Some(st) => st.trim().parse::<i64>().ok()?,
            None => 1,
        };
        if step_i <= 0 || s1 < s0 {
            return None;
        }
        let mut out = Vec::new();
        let mut i = s0;
        while i <= s1 {
            out.push(Value::from(i));
            i += step_i;
        }
        return Some(out);
    }
    // Float fallback for real-valued sweeps (thresholds, %s).
    let s0 = start.parse::<f64>().ok()?;
    let s1 = end.parse::<f64>().ok()?;
    let step_f = match step {
        Some(st) => st.trim().parse::<f64>().ok()?,
        None => 1.0,
    };
    if step_f <= 0.0 || s1 < s0 {
        return None;
    }
    let mut out = Vec::new();
    let mut x = s0;
    while x <= s1 + step_f * 1e-9 {
        out.push(Value::from(x));
        x += step_f;
    }
    Some(out)
}

/// Cartesian product of the axes, preserving axis order in each combination.
fn cartesian(axes: &[(String, Vec<Value>)]) -> Vec<Vec<Value>> {
    let mut out: Vec<Vec<Value>> = vec![Vec::new()];
    for (_, values) in axes {
        let mut next = Vec::with_capacity(out.len() * values.len());
        for prefix in &out {
            for v in values {
                let mut row = prefix.clone();
                row.push(v.clone());
                next.push(row);
            }
        }
        out = next;
    }
    out
}

/// Combine the fixed params with one grid combination into the effective
/// substitution table for that point.
fn combine_params(
    fixed: &HashMap<String, Value>,
    axes: &[(String, Vec<Value>)],
    combo: &[Value],
) -> HashMap<String, Value> {
    let mut out = fixed.clone();
    for (i, v) in combo.iter().enumerate() {
        out.insert(axes[i].0.clone(), v.clone());
    }
    out
}

/// Substitute a params table into the base strategy value, then typed-parse.
fn build_spec(base: &Value, params: &HashMap<String, Value>) -> Result<SingleStrategySpec> {
    let value = params::substitute(base.clone(), params)?;
    Ok(serde_json::from_value(value)?)
}

// ---------------------------------------------------------------------------
// Direction table for --best-by
// ---------------------------------------------------------------------------

/// Direction lookup keyed by the metric's canonical dotted path. Full paths
/// avoid the leaf-name collisions the flat catalog would hit (e.g. `total` is
/// both `returns.total` — descending — and `trades.total` — no clear direction).
///
/// Every entry names a metric where higher-is-better vs lower-is-better is
/// unambiguous; ambiguous or context-dependent metrics (`skewness`, `kurtosis`,
/// trade counts, distribution moments, …) are deliberately absent so that a
/// `--best-by` on one errors out with a hint rather than silently guessing.
fn direction_for(path: &str) -> Option<Direction> {
    match path {
        // Higher is better — return, PnL, risk-adjusted ratios, trade quality.
        "run.final_equity"
        | "returns.total"
        | "returns.total_pct"
        | "returns.cagr_pct"
        | "returns.annualized_mean_pct"
        | "returns.mean_bar"
        | "returns.median_bar"
        | "returns.best_bar"
        | "returns.worst_bar"
        | "returns.positive_bars_pct"
        | "returns.tail_ratio"
        | "risk_adjusted.sharpe"
        | "risk_adjusted.sortino"
        | "risk_adjusted.calmar"
        | "risk_adjusted.omega"
        | "risk_adjusted.ulcer_performance_index"
        | "drawdown.recovery_factor"
        | "trades.win_rate_pct"
        | "trades.profit_factor"
        | "trades.payoff_ratio"
        | "trades.expectancy"
        | "trades.kelly_fraction"
        | "trades.average_win"
        | "trades.largest_win"
        | "trades.average_loss"
        | "trades.largest_loss"
        | "trades.average_return_pct" => Some(Direction::Descending),
        // Lower is better — drawdown, volatility, tail loss.
        "returns.stddev_bar"
        | "returns.annualized_volatility_pct"
        | "returns.var_95"
        | "returns.cvar_95"
        | "risk_adjusted.ulcer_index"
        | "drawdown.max"
        | "drawdown.max_pct"
        | "drawdown.max_duration_bars"
        | "drawdown.avg"
        | "drawdown.avg_pct"
        | "drawdown.avg_duration_bars"
        | "drawdown.time_in_drawdown_pct" => Some(Direction::Ascending),
        _ => None,
    }
}

/// Sort `rows` by `path`'s ranking value (the whole-run value, or the
/// cross-window mean shifted against the row by `k` stddevs under `-w` — see
/// [`ranking_value`]); direction is descending → largest first, ascending →
/// smallest first. Rows whose metric is `None` (an omitted degenerate ratio)
/// always sort to the end.
///
/// The comparator is called `O(N log N)` times; a naive `ranking_value` in the
/// closure re-flattens each `Metrics` on every compare (windowed: once per
/// window per compare). So we precompute the ranking value per row once, then
/// sort a permutation vector by those cached keys and apply it — turning
/// `O(N log N)` flattens into `O(N)`.
fn sort_by_metric(rows: &mut Vec<Row>, path: &str, direction: Direction, k: Real) {
    let keys: Vec<Option<Real>> = rows
        .iter()
        .map(|r| ranking_value(&r.eval, path, direction, k))
        .collect();
    let mut order: Vec<usize> = (0..rows.len()).collect();
    order.sort_by(|&i, &j| {
        let (av, bv) = (keys[i], keys[j]);
        match (av, bv) {
            (Some(x), Some(y)) => {
                let cmp = x.partial_cmp(&y).unwrap_or(std::cmp::Ordering::Equal);
                if direction == Direction::Descending {
                    cmp.reverse()
                } else {
                    cmp
                }
            }
            (Some(_), None) => std::cmp::Ordering::Less,
            (None, Some(_)) => std::cmp::Ordering::Greater,
            (None, None) => std::cmp::Ordering::Equal,
        }
    });
    // Apply the permutation in-place with an `Option` scratch buffer — cheap
    // and avoids cloning the (~50-field) Metrics documents held in each Row.
    let mut slots: Vec<Option<Row>> = std::mem::take(rows).into_iter().map(Some).collect();
    for i in order {
        rows.push(slots[i].take().expect("permutation visits each row exactly once"));
    }
}

/// Position of a metric column inside the `metrics::flatten` output — the
/// output ordering is fixed and shared across every [`Metrics`] document, so a
/// name resolves to a stable index which can be looked up in `O(1)` per row.
type ColumnPos = usize;

/// Look up a metric by its canonical dotted path against a Metrics document.
/// Uses [`metrics::flatten`] — one Vec allocation of ~60 tuples per call. Fine
/// for one-shot printing / the winning-combo lookup; hot loops (the sort
/// comparator and the CSV writer) precompute positions and flatten once per
/// row instead.
fn lookup(m: &metrics::Metrics, path: &str) -> Option<Real> {
    metrics::flatten(m)
        .into_iter()
        .find(|(k, _)| *k == path)
        .and_then(|(_, v)| v)
}

/// A windowed evaluation's cross-window `(mean, stddev)` for one metric path,
/// over the windows where the metric is defined; `None` when it is degenerate
/// in every window.
fn lookup_windowed(windows: &[metrics::WindowMetrics], path: &str) -> Option<(Real, Real)> {
    metrics::mean_std(windows.iter().filter_map(|w| lookup(&w.metrics, path)))
}

/// Cross-window `(mean, stddev)` where each window's value is already known —
/// the twin of [`lookup_windowed`] that avoids repeated flattening when the
/// caller has already indexed by column position.
fn mean_std_of<I: Iterator<Item = Option<Real>>>(values: I) -> Option<(Real, Real)> {
    metrics::mean_std(values.flatten())
}

/// The single value a row is *ranked* by for a metric path: the whole-run
/// value, or the cross-window mean shifted **against** the row by `k`
/// standard deviations — `mean − k·std` for a higher-is-better (descending)
/// metric, `mean + k·std` for a lower-is-better (ascending) one, so a large
/// spread is always penalized, never rewarded.
fn ranking_value(eval: &Evaluation, path: &str, direction: Direction, k: Real) -> Option<Real> {
    match eval {
        Evaluation::Whole(m) => lookup(m, path),
        Evaluation::Windowed(ws) => {
            lookup_windowed(ws, path).map(|(mean, std)| match direction {
                Direction::Descending => mean - k * std,
                Direction::Ascending => mean + k * std,
            })
        }
    }
}

// ---------------------------------------------------------------------------
// CSV output
// ---------------------------------------------------------------------------

/// Write the sweep CSV: the union axis columns (name-sorted) first, then one
/// column per requested metric — or, under `-w/--windowed`, two columns per
/// metric (`<name>_mean` / `<name>_std`, the cross-window aggregate). Whole-run
/// sweeps also get a trailing `selection.deflated_sharpe` column when the grid has
/// enough spread in Sharpes for the multiple-testing correction to be defined.
/// `,`-delimited to match `fills.csv` / `trades.csv` / `returns.csv`. Axis cells that the
/// row's subgrid doesn't touch, and missing (omitted) metric values, are both
/// written as an empty cell.
fn write_grid_csv(
    path: &Path,
    union_columns: &[String],
    metric_columns: &[(String, String)],
    windowed: bool,
    deflated_sharpe_context: Option<(usize, Real)>,
    rows: &[Row],
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
    writer.write_record(&header)?;

    // Precompute the flatten position of each metric column against a sample
    // document (whichever eval is available). Then per row, flatten each
    // Metrics **once** and read the columns by indexed access — turning
    // `rows * cols` full-metrics scans into `rows * 1` flattens + `rows * cols`
    // vec[i] lookups. Empirically ~325× faster on a 50k×5 grid.
    let sample_metrics = rows.first().and_then(|r| match &r.eval {
        Evaluation::Whole(m) => Some(m.as_ref()),
        Evaluation::Windowed(ws) => ws.first().map(|w| &w.metrics),
    });
    let positions: Vec<Option<ColumnPos>> = if let Some(sample) = sample_metrics {
        let flat = metrics::flatten(sample);
        metric_columns
            .iter()
            .map(|(_, path)| {
                flat.iter()
                    .position(|(k, _)| *k == path.as_str())
            })
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
                    let spread = pos.and_then(|p| {
                        mean_std_of(per_window.iter().map(|window| window[p]))
                    });
                    record.push(cell(spread.map(|(mean, _)| mean)));
                    record.push(cell(spread.map(|(_, std)| std)));
                }
            }
        }
        // Trailing `selection.deflated_sharpe` cell — uses per-row summary stats extracted
        // via `row_dsr_inputs` (whole-run passthrough or windowed cross-window
        // means; see [`row_dsr_inputs`] and the [`Sweep`] field's rustdoc).
        if let Some((n_trials, trial_var)) = deflated_sharpe_context {
            let (sharpe, skew, kurt, n_returns, bpy) = row_dsr_inputs(row);
            let dsr = fugazi::metrics::deflated_sharpe_from_stats(
                sharpe,
                skew,
                kurt,
                n_returns,
                bpy,
                n_trials,
                trial_var,
            );
            record.push(cell(dsr));
        }
        writer.write_record(&record)?;
    }
    writer.flush()?;
    Ok(())
}

/// Format a grid axis value as it appears in the CSV. Integers stay integer-
/// looking (`5` not `5.0`); strings drop their JSON quotes.
fn format_value(v: &Value) -> String {
    match v {
        Value::String(s) => s.clone(),
        Value::Number(n) => n.to_string(),
        Value::Bool(b) => b.to_string(),
        Value::Null => String::new(),
        other => other.to_string(),
    }
}

/// Format a metric number for the CSV. Uses full precision (not `{:.2}`) so a
/// downstream tool can re-round.
fn format_number(v: Real) -> String {
    // A whole f64 still round-trips through `{v}` (Display) as e.g. `5`, but
    // we want the CSV column to read the same shape a spreadsheet expects.
    format!("{v}")
}

// ---------------------------------------------------------------------------
// Console output
// ---------------------------------------------------------------------------

fn print_inputs_block(
    opts: &OptimizeOptions,
    windowed_bars: Option<NonZeroUsize>,
    subgrid_summaries: &[(String, usize)],
    rows: &[Row],
) {
    println!("{}", style::bold("inputs"));
    print_field("strategy", opts.strategy_label);
    if subgrid_summaries.len() == 1 {
        // Compact form when there's only one subgrid — matches the pre-stack
        // shape, so a single-`--grid` invocation reads the same as before.
        print_field(
            "grid",
            &format!("{} points · {}", rows.len(), subgrid_summaries[0].0),
        );
    } else {
        print_field(
            "grid",
            &format!(
                "{} points across {} subgrids",
                rows.len(),
                subgrid_summaries.len()
            ),
        );
        for (i, (label, n)) in subgrid_summaries.iter().enumerate() {
            print_indented(&format!("[{}] {n} pts · {label}", i + 1));
        }
    }
    print_field("capital", &format!("{:.2}", opts.cash));
    // Costs summary — same treatment as `run`: name it explicitly if a model is
    // set, note `none (explicit)` if the user opted in silently, and warn if no
    // flag at all.
    if !opts.cost_config.is_none() {
        print_field("costs", "active (commission/spread/slippage applied)");
    } else if opts.costs_supplied {
        print_field("costs", "none (explicit)");
    }
    if !opts.costs_supplied {
        println!(
            "  {} no cost model set — commission, spread, and slippage are zero; grid results are frictionless",
            style::yellow("warn")
        );
    }
    print_field("output", &opts.output.display().to_string());
    if let (Some(spec), Some(bars)) = (opts.windowed, windowed_bars) {
        let msg = match spec {
            WindowSpec::Bars(_) => format!("{bars}-bar windows (mean ± std per metric)"),
            WindowSpec::Duration(_) => {
                format!("{spec} → {bars}-bar windows (mean ± std per metric)")
            }
        };
        print_field("windowed", &msg);
    }
    if let Some(name) = &opts.best_by {
        if opts.risk_aversion > 0.0 {
            print_field(
                "best-by",
                &format!(
                    "{name} (risk-aversion k={}: mean shifted k·std against)",
                    opts.risk_aversion
                ),
            );
        } else {
            print_field("best-by", name);
        }
    }
}

/// The "skipped overlay columns" banner: non-numeric CSV columns that were
/// dropped from the overlay [`Schema`] because at least one value failed to
/// parse as [`Real`]. Silent when nothing was skipped.
fn print_skipped_overlay_warning(skipped: &[String]) {
    if skipped.is_empty() {
        return;
    }
    let msg = format!(
        "skipped non-numeric overlay column{}: {} \
         — not accessible via `!get`",
        if skipped.len() == 1 { "" } else { "s" },
        skipped.join(", "),
    );
    println!("  {} {msg}", style::yellow("warn"));
}

fn print_best_block(
    union_columns: &[String],
    metric_columns: &[(String, String)],
    best_by: Option<&(String, String, Direction)>,
    k: Real,
    rows: &[Row],
) {
    println!("\n{}", style::bold("best"));
    let Some(best) = rows.first() else {
        print_field("params", "(no grid points)");
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
    print_field("params", &params_label);

    if let Some((name, path, direction)) = best_by {
        let mut value = format_metric(&best.eval, path);
        // With a risk-aversion penalty the ranking key differs from the mean;
        // show it so the ordering is explainable from the console alone.
        if k > 0.0
            && matches!(best.eval, Evaluation::Windowed(_))
            && let Some(score) = ranking_value(&best.eval, path, *direction, k)
        {
            value = format!("{value} · score {score:.4}");
        }
        print_field(name, &value);
    }
    for (name, path) in metric_columns {
        // Skip a metric already printed as the best-by row.
        if best_by.map(|(_, p, _)| p.as_str()) == Some(path.as_str()) {
            continue;
        }
        print_field(name, &format_metric(&best.eval, path));
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
                        ws.iter().map(|w| w.metrics.returns.annualized_volatility_pct)
                    ),
                    false
                ),
            )
        }
    };
    print_field("return", &headline);
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

fn print_field(label: &str, value: &str) {
    // Metric names can outgrow the 9-char label column (`total_pct`, dotted
    // paths); keep at least one space between label and value.
    let padded = if label.len() < 9 {
        format!("{label:<9}")
    } else {
        format!("{label} ")
    };
    println!("  {}{value}", style::dim(&padded));
}

/// A trailing hangover line under a `print_field` — indented to sit under the
/// value column (2 leading spaces + 9-char label column = 11 spaces).
fn print_indented(text: &str) {
    println!("           {text}");
}

// ---------------------------------------------------------------------------
// Walk-forward (rolling)
// ---------------------------------------------------------------------------

/// One fold's bar ranges — same layout across every grid row (fold boundaries
/// are grid-wide, not per-row, so per-fold metrics are directly comparable).
struct FoldLayout {
    is: std::ops::Range<usize>,
    /// First bar included in OOS metric evaluation (post-embargo). State still
    /// rolls through the embargo bars — they're just dropped from the OOS
    /// reduction.
    oos: std::ops::Range<usize>,
}

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
///   row's params and return its `stable_period()` (or `warm_up_period()`
///   under `--keep-unstable`). The grid-wide max is the fold-layout's
///   prefix skip. For basket / multi strategies the caller is responsible
///   for feeding one representative snapshot to trigger lazy per-symbol
///   chain discovery before reading the period — see the
///   [`DynBasketStrategy::stable_period`](crate::spec::DynBasketStrategy::stable_period)
///   / [`DynMultiAssetStrategy::stable_period`](crate::spec::DynMultiAssetStrategy::stable_period)
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
    output: &Path,
    jobs: Option<usize>,
    quiet: bool,
    skipped_overlay_columns: &[String],
    cash: Real,
) -> Result<()>
where
    P: Fn(&HashMap<String, Value>) -> Result<usize> + Sync,
    R: Fn(&HashMap<String, Value>) -> Result<fugazi::RunReport<String>> + Sync,
{
    let (is_bars, oos_bars, embargo_bars) = spec
        .resolve(effective_freq, asset_class)
        .map_err(anyhow::Error::msg)
        .context("resolving `--walkforward`")?;

    // Grid enumeration — same shape as [`optimize`] so subgrids stack the same
    // way and the union-column projection is compatible with the per-fold row.
    let union_columns = compute_union_columns(&subgrids);
    let plan: Vec<(usize, usize)> = subgrids
        .iter()
        .enumerate()
        .flat_map(|(si, s)| (0..s.combos.len()).map(move |ci| (si, ci)))
        .collect();

    // Pre-scan: build every row's strategy once (throwaway) and take grid-wide
    // max readiness. Doing this before the fold layout — so every row's IS/OOS
    // ranges are identical, and per-fold metrics are directly comparable
    // regardless of which combo winds up warming up faster.
    let pool = crate::pool::build_pool(jobs)?;
    let plan_ref = &plan;
    let subgrids_ref = &subgrids;
    let probe_ref = &probe_readiness;
    let prefix_skip: usize = pool.install(|| {
        plan_ref
            .par_iter()
            .map(|&(si, ci)| {
                let subgrid = &subgrids_ref[si];
                let combo = &subgrid.combos[ci];
                let params = combine_params(&subgrid.fixed, &subgrid.axes, combo);
                probe_ref(&params)
            })
            .try_reduce(|| 0usize, |a, b| Ok(a.max(b)))
    })?;

    let folds = walkforward_layout(n_bars, prefix_skip, is_bars, oos_bars, embargo_bars)?;

    // Main pass: one full backtest per row. Store the reports so per-fold
    // slicing is a bounded-cost operation (equity is `Vec<f64>` of length
    // `n_bars`; fills are typically short).
    let run_ref = &run_backtest;
    let reports: Vec<fugazi::RunReport<String>> = pool.install(|| {
        plan_ref
            .par_iter()
            .map(|&(si, ci)| {
                let subgrid = &subgrids_ref[si];
                let combo = &subgrid.combos[ci];
                let params = combine_params(&subgrid.fixed, &subgrid.axes, combo);
                run_ref(&params)
            })
            .collect::<Result<Vec<_>>>()
    })?;

    // Resolve --metrics / --best-by against the first row's *whole-run*
    // Metrics document, not a fold slice — a narrow slice (e.g. embargo
    // eating most of a 2-bar OOS) can leave many metrics `None`, and
    // `resolve_metric` requires a *numeric* leaf when matching by short name.
    // The whole-run reduction covers the full catalogue reliably.
    let sample_metrics = if let Some(first_report) = reports.first() {
        metrics::from_report(first_report, bars_per_year, risk_free_rate, seconds_per_bar)
    } else {
        // Never reached: walkforward_layout errors out on zero folds, and the
        // grid always has ≥1 row (enforced upstream in `run`).
        bail!("walkforward: empty fold or grid")
    };

    let metric_columns: Vec<(String, String)> = if metric_names.is_empty() {
        metrics::flatten(&sample_metrics)
            .into_iter()
            .map(|(path, _)| (path.to_string(), path.to_string()))
            .collect()
    } else {
        metric_names
            .iter()
            .map(|name| {
                let (path, _) = metrics::resolve_metric(name, &sample_metrics)?;
                Ok::<_, anyhow::Error>((path.clone(), path))
            })
            .collect::<Result<Vec<_>>>()?
    };

    let best_by = best_by
        .map(|name| {
            let (path, _) = metrics::resolve_metric(name, &sample_metrics)?;
            let direction = direction_for(&path).ok_or_else(|| {
                anyhow!(
                    "--best-by `{name}` has no built-in direction; pass one whose \
                     direction is known (e.g. sharpe, sortino, cagr_pct, max_pct, \
                     ulcer_index, annualized_volatility_pct)"
                )
            })?;
            Ok::<_, anyhow::Error>((path.clone(), path, direction))
        })
        .transpose()?;

    // Per-fold pass: for each fold, compute every row's IS + OOS metrics,
    // pick the winner by IS-metric ranking, and collect the winner's OOS
    // slice for the composite.
    let mut fold_rows: Vec<WalkForwardRow> = Vec::with_capacity(folds.len());
    let mut composite_equity: Vec<Real> = Vec::new();
    let mut composite_fills: Vec<fugazi::Fill<String>> = Vec::new();
    let mut running_equity: Real = cash;

    for (fold_idx, fold) in folds.iter().enumerate() {
        let per_row: Vec<(metrics::Metrics, metrics::Metrics)> = pool.install(|| {
            reports
                .par_iter()
                .map(|r| {
                    let is_slice = metrics::report_slice(r, fold.is.clone());
                    let oos_slice = metrics::report_slice(r, fold.oos.clone());
                    (
                        metrics::from_report(
                            &is_slice,
                            bars_per_year,
                            risk_free_rate,
                            seconds_per_bar,
                        ),
                        metrics::from_report(
                            &oos_slice,
                            bars_per_year,
                            risk_free_rate,
                            seconds_per_bar,
                        ),
                    )
                })
                .collect()
        });

        // Winner selection. Without --best-by we still emit a row per fold, but
        // the "winner" is just the first grid point in enumeration order (same
        // convention the plain grid sweep uses when --best-by is absent).
        let winner_idx: usize = match &best_by {
            Some((_, path, direction)) => {
                let keys: Vec<Option<Real>> = per_row
                    .iter()
                    .map(|(is_m, _)| {
                        // Direction-aware key: flip sign for ascending metrics so
                        // `max_by` still finds the winner.
                        lookup(is_m, path).map(|v| match direction {
                            Direction::Descending => v,
                            Direction::Ascending => -v,
                        })
                    })
                    .collect();
                keys.iter()
                    .enumerate()
                    .filter_map(|(i, k)| k.map(|k| (i, k)))
                    .max_by(|a, b| a.1.partial_cmp(&b.1).unwrap_or(std::cmp::Ordering::Equal))
                    .map(|(i, _)| i)
                    .unwrap_or(0)
            }
            None => 0,
        };
        let (winner_is, winner_oos) = &per_row[winner_idx];

        // Composite OOS: stitch the winner's OOS slice onto the running curve.
        // Scale each fold's equity into the running total so mode-switching
        // between winners doesn't create discontinuities.
        let oos_slice = metrics::report_slice(&reports[winner_idx], fold.oos.clone());
        let scale = if oos_slice.initial_equity > 0.0 {
            running_equity / oos_slice.initial_equity
        } else {
            1.0
        };
        let bar_offset = composite_equity.len();
        for eq in &oos_slice.equity_curve {
            composite_equity.push(*eq * scale);
        }
        for fill in oos_slice.fills {
            composite_fills.push(fugazi::Fill {
                bar: fill.bar + bar_offset,
                order: fill.order,
            });
        }
        running_equity = composite_equity.last().copied().unwrap_or(running_equity);

        // Project the winner's params onto the union columns for the CSV.
        let (si, ci) = plan[winner_idx];
        let values =
            project_row(&subgrids[si], &subgrids[si].combos[ci], &union_columns);

        fold_rows.push(WalkForwardRow {
            fold: fold_idx,
            is_start: fold.is.start,
            is_end: fold.is.end,
            oos_start: fold.oos.start,
            oos_end: fold.oos.end,
            values,
            is_metrics: winner_is.clone(),
            oos_metrics: winner_oos.clone(),
        });
    }

    // Composite OOS artifact: build a synthetic RunReport from the stitched
    // equity + fills, then run every metric against it.
    let composite_report = fugazi::RunReport {
        equity_curve: composite_equity.clone(),
        fills: composite_fills,
        initial_equity: cash,
    };
    let composite_metrics = metrics::from_report(
        &composite_report,
        bars_per_year,
        risk_free_rate,
        seconds_per_bar,
    );

    // Output — three sibling files.
    write_walkforward_csv(output, &union_columns, &metric_columns, &fold_rows)?;
    write_composite_equity_csv(&derive_sibling(output, "composite_oos_equity", "csv"), &composite_equity)?;
    write_composite_metrics_yaml(
        &derive_sibling(output, "composite_oos_metrics", "yml"),
        &composite_metrics,
    )?;

    if !quiet {
        style::print_header("optimize", "walk-forward optimization");
        print_skipped_overlay_warning(skipped_overlay_columns);
        print_walkforward_inputs(
            &spec,
            (is_bars, oos_bars, embargo_bars),
            prefix_skip,
            keep_unstable,
            folds.len(),
            n_bars,
            output,
        );
        print_walkforward_summary(&fold_rows, &metric_columns, best_by.as_ref());
    }
    Ok(())
}

/// Compute the per-fold ranges. Fold `k` occupies IS
/// `[prefix + k*oos, prefix + k*oos + is)` and OOS
/// `[prefix + k*oos + is + embargo, prefix + k*oos + is + oos)`. The final
/// fold's OOS extends to `n_bars` so trailing bars aren't dropped.
fn walkforward_layout(
    n_bars: usize,
    prefix_skip: usize,
    is: usize,
    oos: usize,
    embargo: usize,
) -> Result<Vec<FoldLayout>> {
    if prefix_skip >= n_bars {
        bail!(
            "walkforward: prefix skip ({prefix_skip} bars) is >= total bars ({n_bars}); \
             the strategy grid's readiness period doesn't fit in the input"
        );
    }
    let usable = n_bars - prefix_skip;
    if is + oos > usable {
        bail!(
            "walkforward: one IS+OOS fold ({is}+{oos} = {} bars) doesn't fit into the \
             usable range ({usable} bars, after skipping {prefix_skip} for readiness) — \
             shrink the windows or extend the input",
            is + oos,
        );
    }
    if embargo >= oos {
        bail!(
            "walkforward: embargo ({embargo} bars) >= OS ({oos} bars) — the entire \
             out-of-sample window would be embargoed"
        );
    }
    let n_folds = (usable - is) / oos;
    if n_folds == 0 {
        bail!(
            "walkforward: no full fold fits (usable={usable}, IS={is}, OS={oos})"
        );
    }
    let mut out = Vec::with_capacity(n_folds);
    for k in 0..n_folds {
        let is_start = prefix_skip + k * oos;
        let is_end = is_start + is;
        let mut oos_end = is_end + oos;
        // Last fold absorbs trailing bars — windows are minimums, not exact
        // widths (matches "sizes-are-minimums" from the design chat).
        if k + 1 == n_folds {
            oos_end = n_bars;
        }
        let oos_start = (is_end + embargo).min(oos_end);
        out.push(FoldLayout {
            is: is_start..is_end,
            oos: oos_start..oos_end,
        });
    }
    Ok(out)
}

/// One row of the per-fold walk-forward CSV. Carries the winner's params
/// (projected onto union columns), IS + OOS bar ranges, and both metric
/// documents so the writer can emit `_is`/`_oos`/`_wfe` triples per
/// `--metrics` column.
struct WalkForwardRow {
    fold: usize,
    is_start: usize,
    is_end: usize,
    oos_start: usize,
    oos_end: usize,
    values: Vec<Option<Value>>,
    is_metrics: metrics::Metrics,
    oos_metrics: metrics::Metrics,
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
    rows: &[WalkForwardRow],
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
    std::fs::write(path, yaml)
        .with_context(|| format!("writing `{}`", path.display()))?;
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
    println!("{}", style::bold("inputs"));
    print_field("windows", &format!("{spec}  →  IS={is_b}, OS={oos_b}, embargo={emb_b} (bars)"));
    print_field(
        "prefix",
        &format!(
            "{prefix_skip} bars ({})",
            if keep_unstable { "keep_unstable → max(warm_up)" } else { "safe → max(stable)" }
        ),
    );
    print_field("folds", &format!("{n_folds}  (over {n_bars} bars)"));
    print_field("output", &format!("{}", output.display()));
    print_indented(&format!(
        "+ {}",
        derive_sibling(output, "composite_oos_equity", "csv").display()
    ));
    print_indented(&format!(
        "+ {}",
        derive_sibling(output, "composite_oos_metrics", "yml").display()
    ));
}

fn print_walkforward_summary(
    rows: &[WalkForwardRow],
    metric_columns: &[(String, String)],
    best_by: Option<&(String, String, Direction)>,
) {
    println!("\n{}", style::bold("folds"));
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
            print_field(
                &format!("#{}", row.fold),
                &format!(
                    "[{}..{})/[{}..{})  {label}_is={} _oos={} _wfe={}  params: {params_label}",
                    row.is_start,
                    row.is_end,
                    row.oos_start,
                    row.oos_end,
                    is_v.map(format_number).unwrap_or_else(|| "—".into()),
                    oos_v.map(format_number).unwrap_or_else(|| "—".into()),
                    wfe.map(format_number).unwrap_or_else(|| "—".into()),
                ),
            );
        }
    } else {
        // No --best-by: dump the first `-m` column's IS/OOS for orientation.
        let path = metric_columns.first().map(|(_, p)| p.as_str());
        for row in rows {
            let (is_str, oos_str) = match path {
                Some(p) => (
                    lookup(&row.is_metrics, p).map(format_number).unwrap_or_else(|| "—".into()),
                    lookup(&row.oos_metrics, p).map(format_number).unwrap_or_else(|| "—".into()),
                ),
                None => ("—".into(), "—".into()),
            };
            print_field(
                &format!("#{}", row.fold),
                &format!(
                    "[{}..{})/[{}..{})  is={is_str} oos={oos_str}",
                    row.is_start, row.is_end, row.oos_start, row.oos_end
                ),
            );
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use fugazi::backtest::RunReport;

    /// The windowed lookup aggregates a metric across windows as
    /// (mean, population std), and `ranking_value` sorts by the mean.
    #[test]
    fn windowed_lookup_aggregates_mean_and_std() {
        // Two 2-bar windows: +10% (100 → 110) then +20% (110 → 132).
        let report: RunReport<String> = RunReport {
            equity_curve: vec![110.0, 110.0, 132.0, 132.0],
            fills: vec![],
            initial_equity: 100.0,
        };
        let windows = metrics::windowed_from_report(&report, 2, 252.0, 0.0, None);
        assert_eq!(windows.len(), 2);

        let (mean, std) = lookup_windowed(&windows, "returns.total_pct").unwrap();
        assert!((mean - 15.0).abs() < 1e-9);
        assert!((std - 5.0).abs() < 1e-9);

        let eval = Evaluation::Windowed(windows);
        let rank = |direction, k| ranking_value(&eval, "returns.total_pct", direction, k);
        assert!((rank(Direction::Descending, 0.0).unwrap() - 15.0).abs() < 1e-9);
        // Risk aversion shifts the mean *against* the row: minus k·std for a
        // higher-is-better metric, plus k·std for a lower-is-better one.
        assert!((rank(Direction::Descending, 1.0).unwrap() - 10.0).abs() < 1e-9);
        assert!((rank(Direction::Ascending, 1.0).unwrap() - 20.0).abs() < 1e-9);
        // A metric degenerate in every window (no trades → no win rate) reads
        // None, so its row sorts last and its CSV cells stay empty.
        assert_eq!(
            ranking_value(&eval, "trades.win_rate_pct", Direction::Descending, 1.0),
            None
        );
    }

    #[test]
    fn range_int_inclusive_with_default_step() {
        let out = try_parse_range("3..7").unwrap();
        let ints: Vec<i64> = out.iter().map(|v| v.as_i64().unwrap()).collect();
        assert_eq!(ints, vec![3, 4, 5, 6, 7]);
    }

    #[test]
    fn range_int_with_step() {
        let out = try_parse_range("3..10:2").unwrap();
        let ints: Vec<i64> = out.iter().map(|v| v.as_i64().unwrap()).collect();
        assert_eq!(ints, vec![3, 5, 7, 9]);
    }

    #[test]
    fn range_float_fallback() {
        let out = try_parse_range("0.5..2.0:0.5").unwrap();
        let floats: Vec<f64> = out.iter().map(|v| v.as_f64().unwrap()).collect();
        assert_eq!(floats, vec![0.5, 1.0, 1.5, 2.0]);
    }

    #[test]
    fn range_rejects_zero_step() {
        assert!(try_parse_range("1..5:0").is_none());
    }

    #[test]
    fn range_rejects_non_range_string() {
        assert!(try_parse_range("BTC").is_none());
        assert!(try_parse_range("hello").is_none());
    }

    #[test]
    fn cartesian_is_ordered_by_axis_declaration() {
        let axes = vec![
            ("a".to_string(), vec![Value::from(1), Value::from(2)]),
            ("b".to_string(), vec![Value::from(10), Value::from(20)]),
        ];
        let combos = cartesian(&axes);
        assert_eq!(combos.len(), 4);
        // Innermost axis (`b`) varies fastest.
        assert_eq!(combos[0], vec![Value::from(1), Value::from(10)]);
        assert_eq!(combos[1], vec![Value::from(1), Value::from(20)]);
        assert_eq!(combos[2], vec![Value::from(2), Value::from(10)]);
    }

    #[test]
    fn split_axes_sorts_by_name_and_partitions() {
        let mut params = HashMap::new();
        params.insert("SLOW".into(), serde_json::json!([10, 20]));
        params.insert("SYM".into(), Value::from("BTC"));
        params.insert("FAST".into(), Value::from("3..5:1"));
        let (fixed, axes) = split_axes(&params).unwrap();
        assert_eq!(fixed.len(), 1);
        assert_eq!(fixed.get("SYM"), Some(&Value::from("BTC")));
        assert_eq!(axes.len(), 2);
        assert_eq!(axes[0].0, "FAST");
        assert_eq!(axes[1].0, "SLOW");
    }

    #[test]
    fn direction_for_known_metrics() {
        assert_eq!(
            direction_for("risk_adjusted.sharpe"),
            Some(Direction::Descending)
        );
        assert_eq!(direction_for("drawdown.max_pct"), Some(Direction::Ascending));
        assert_eq!(
            direction_for("returns.cagr_pct"),
            Some(Direction::Descending)
        );
        assert_eq!(direction_for("trades.total"), None);
    }

    #[test]
    fn reject_axes_in_params_flags_lists_and_ranges() {
        let mut params = HashMap::new();
        params.insert("SYM".into(), Value::from("BTC"));
        params.insert("FAST".into(), serde_json::json!([3, 5, 8]));
        params.insert("SLOW".into(), Value::from("10..20:2"));
        let err = reject_axes_in_params(&params).unwrap_err().to_string();
        // Both offenders named, alphabetized for a stable message.
        assert!(err.contains("FAST"), "err = {err}");
        assert!(err.contains("SLOW"), "err = {err}");
        assert!(!err.contains("SYM"), "err = {err}");
        // Bare-string scalars that don't look like ranges pass through.
        params.remove("FAST");
        params.remove("SLOW");
        assert!(reject_axes_in_params(&params).is_ok());
        // Empty arrays are treated as scalars (they're rejected downstream by
        // `split_axes` with a clearer message).
        params.insert("EMPTY".into(), Value::Array(vec![]));
        assert!(reject_axes_in_params(&params).is_ok());
    }

    /// A subgrid with `fixed` from a merged (baseline + grid) map, `axes`
    /// sorted by name, and cartesian combos over those axes.
    fn subgrid(fixed: &[(&str, Value)], axes: &[(&str, Vec<Value>)]) -> Subgrid {
        let fixed: HashMap<String, Value> =
            fixed.iter().map(|(k, v)| ((*k).to_string(), v.clone())).collect();
        let mut axes: Vec<Axis> = axes
            .iter()
            .map(|(name, values)| ((*name).to_string(), values.clone()))
            .collect();
        axes.sort_by(|a, b| a.0.cmp(&b.0));
        let combos = cartesian(&axes);
        Subgrid { fixed, axes, combos }
    }

    #[test]
    fn union_columns_include_axes_and_varying_scalars() {
        // Subgrid 1: X="A" fixed, Y axis (1..3). Subgrid 2: X="B" fixed, Z axis (10, 20).
        // Baseline SYM=BTC would be merged into both `fixed`s — same value across
        // subgrids, so it must *not* become a column.
        let a = subgrid(
            &[("SYM", Value::from("BTC")), ("X", Value::from("A"))],
            &[("Y", vec![Value::from(1), Value::from(2), Value::from(3)])],
        );
        let b = subgrid(
            &[("SYM", Value::from("BTC")), ("X", Value::from("B"))],
            &[("Z", vec![Value::from(10), Value::from(20)])],
        );
        let cols = compute_union_columns(&[a, b]);
        // Name-sorted: X (differing scalar), Y (axis in 1), Z (axis in 2).
        // SYM shared across both → not a column.
        assert_eq!(cols, vec!["X".to_string(), "Y".to_string(), "Z".to_string()]);
    }

    #[test]
    fn union_columns_pick_up_absent_scalars() {
        // Subgrid 1 has M=1 fixed, subgrid 2 doesn't touch M at all — that
        // asymmetry alone makes M a column so its rows expose which subgrid
        // set it (the "conditional-presence" case).
        let a = subgrid(&[("M", Value::from(1))], &[("Y", vec![Value::from(1)])]);
        let b = subgrid(&[], &[("Z", vec![Value::from(10)])]);
        let cols = compute_union_columns(&[a, b]);
        assert_eq!(cols, vec!["M".to_string(), "Y".to_string(), "Z".to_string()]);
    }

    #[test]
    fn project_row_populates_axis_and_fixed_and_leaves_absent_empty() {
        let a = subgrid(
            &[("SYM", Value::from("BTC")), ("X", Value::from("A"))],
            &[("Y", vec![Value::from(1), Value::from(2)])],
        );
        let cols = vec!["X".to_string(), "Y".to_string(), "Z".to_string()];
        // Combo picks Y=2 (second axis value); Z is absent → empty cell.
        let combo = vec![Value::from(2)];
        let row = project_row(&a, &combo, &cols);
        assert_eq!(row, vec![Some(Value::from("A")), Some(Value::from(2)), None]);
    }

    #[test]
    fn subgrid_label_omits_baseline_scalars() {
        // With union_columns = [X, Y, Z] (baseline SYM shared), the label
        // names X (varying scalar in this subgrid) and Y (its axis) — SYM
        // stays silent even though it's in `fixed`.
        let a = subgrid(
            &[("SYM", Value::from("BTC")), ("X", Value::from("A"))],
            &[("Y", vec![Value::from(1), Value::from(2), Value::from(3)])],
        );
        let cols = vec!["X".to_string(), "Y".to_string(), "Z".to_string()];
        assert_eq!(subgrid_label(&a, &cols), "X=A, Y(3)");
    }

    #[test]
    fn subgrid_label_falls_back_to_baseline_when_nothing_varies() {
        // A subgrid with no axes and no union-column scalars — reads as
        // `(baseline)` in the summary line.
        let a = subgrid(&[("SYM", Value::from("BTC"))], &[]);
        let cols: Vec<String> = vec![];
        assert_eq!(subgrid_label(&a, &cols), "(baseline)");
    }

    // Perf probe — run with:
    //   cargo test --release optimize::tests::bench_sort -- --ignored --nocapture
    #[test]
    #[ignore]
    fn bench_sort_by_metric_vs_precomputed() {
        use std::time::Instant;

        // One synthetic run reduced to Metrics, then cloned N times with a
        // perturbed sharpe so the sort actually reorders. This is the exact
        // Metrics shape the CLI sorts.
        let mut equity = Vec::with_capacity(1_000);
        let mut e = 100.0_f64;
        let mut s: u64 = 0xdead_beef;
        for _ in 0..1_000 {
            s = s.wrapping_mul(6364136223846793005).wrapping_add(1442695040888963407);
            let n = ((s >> 33) as f64 / u32::MAX as f64) - 0.5;
            e *= 1.0 + 0.0002 + 0.01 * n;
            equity.push(e);
        }
        let report: RunReport<String> = RunReport {
            equity_curve: equity,
            fills: vec![],
            initial_equity: 100.0,
        };
        let base = metrics::from_report(&report, 252.0, 0.0, None);

        for &n in &[1_000_usize, 10_000, 50_000] {
            let make_rows = || -> Vec<Row> {
                (0..n)
                    .map(|i| {
                        let mut m = base.clone();
                        // Perturb one field so sort is non-trivial.
                        if let Some(sh) = m.risk_adjusted.sharpe.as_mut() {
                            *sh += (i as f64) * 1e-6;
                        }
                        Row {
                            values: vec![],
                            eval: Evaluation::Whole(Box::new(m)),
                        }
                    })
                    .collect()
            };

            // Baseline: what optimize::sort_by_metric actually does.
            let mut rows = make_rows();
            let t = Instant::now();
            sort_by_metric(&mut rows, "risk_adjusted.sharpe", Direction::Descending, 0.0);
            let baseline = t.elapsed().as_secs_f64();
            let _ = std::hint::black_box(rows.len());

            // Fix: precompute the ranking value per row once, sort by it.
            let mut rows = make_rows();
            let t = Instant::now();
            let mut keyed: Vec<(usize, Option<Real>)> = rows
                .iter()
                .enumerate()
                .map(|(i, r)| (i, ranking_value(&r.eval, "risk_adjusted.sharpe", Direction::Descending, 0.0)))
                .collect();
            keyed.sort_by(|a, b| match (a.1, b.1) {
                (Some(x), Some(y)) => y.partial_cmp(&x).unwrap_or(std::cmp::Ordering::Equal),
                (Some(_), None) => std::cmp::Ordering::Less,
                (None, Some(_)) => std::cmp::Ordering::Greater,
                (None, None) => std::cmp::Ordering::Equal,
            });
            // Reorder rows to match keyed order.
            let mut reordered: Vec<Row> = Vec::with_capacity(n);
            let mut src: Vec<Option<Row>> = rows.drain(..).map(Some).collect();
            for (i, _) in &keyed {
                reordered.push(src[*i].take().unwrap());
            }
            let fixed = t.elapsed().as_secs_f64();
            let _ = std::hint::black_box(reordered.len());

            eprintln!(
                "n={:>6}  sort_by_metric = {:.3}s   precomputed = {:.3}s   speedup = {:.1}x",
                n,
                baseline,
                fixed,
                baseline / fixed,
            );
        }
    }

    // The other resolve_metric hot loop: write_grid_csv calls `lookup` once per
    // (row, metric_column). Bench with 5 metric columns.
    #[test]
    #[ignore]
    fn bench_csv_lookup_vs_flatten() {
        use std::time::Instant;

        let mut equity = Vec::with_capacity(1_000);
        let mut e = 100.0_f64;
        let mut s: u64 = 0xf00d_f00d;
        for _ in 0..1_000 {
            s = s.wrapping_mul(6364136223846793005).wrapping_add(1442695040888963407);
            let n = ((s >> 33) as f64 / u32::MAX as f64) - 0.5;
            e *= 1.0 + 0.0002 + 0.01 * n;
            equity.push(e);
        }
        let report: RunReport<String> = RunReport {
            equity_curve: equity,
            fills: vec![],
            initial_equity: 100.0,
        };
        let base = metrics::from_report(&report, 252.0, 0.0, None);

        let cols = [
            "risk_adjusted.sharpe",
            "returns.total_pct",
            "drawdown.max_pct",
            "returns.cagr_pct",
            "trades.win_rate_pct",
        ];

        for &n in &[1_000_usize, 10_000, 50_000] {
            let docs: Vec<metrics::Metrics> = (0..n).map(|_| base.clone()).collect();

            // Baseline: what write_grid_csv does — lookup per (row, column).
            let t = Instant::now();
            let mut sink = 0.0_f64;
            for d in &docs {
                for c in &cols {
                    if let Some(v) = lookup(d, c) {
                        sink += v;
                    }
                }
            }
            let baseline = t.elapsed().as_secs_f64();
            let _ = std::hint::black_box(sink);

            // Fix: flatten each doc once via `metrics::flatten`, then indexed
            // lookups. `flatten` returns column names in fixed order, so we
            // resolve the column *positions* once and index into the vec.
            let flat_sample = metrics::flatten(&base);
            let positions: Vec<usize> = cols
                .iter()
                .map(|c| flat_sample.iter().position(|(k, _)| *k == *c).unwrap())
                .collect();
            let t = Instant::now();
            let mut sink = 0.0_f64;
            for d in &docs {
                let flat = metrics::flatten(d);
                for &pos in &positions {
                    if let Some(v) = flat[pos].1 {
                        sink += v;
                    }
                }
            }
            let fixed = t.elapsed().as_secs_f64();
            let _ = std::hint::black_box(sink);

            eprintln!(
                "n={:>6} cols=5   baseline = {:.3}s   flatten = {:.3}s   speedup = {:.1}x",
                n,
                baseline,
                fixed,
                baseline / fixed,
            );
        }
    }

    #[test]
    fn walkforward_layout_absorbs_trailing_bars_into_last_fold() {
        // 100 bars, no prefix skip, IS=20, OS=10, no embargo.
        // n_folds = (100 - 20) / 10 = 8. Last fold's OOS extends to bar 100.
        let folds = walkforward_layout(100, 0, 20, 10, 0).unwrap();
        assert_eq!(folds.len(), 8);
        // First fold: IS [0..20), OOS [20..30).
        assert_eq!(folds[0].is, 0..20);
        assert_eq!(folds[0].oos, 20..30);
        // Second fold: IS [10..30), OOS [30..40) — slides by OS=10.
        assert_eq!(folds[1].is, 10..30);
        assert_eq!(folds[1].oos, 30..40);
        // Last fold: OOS extends to bar 100 (absorbing 10 trailing bars past
        // where the nominal 10-bar OOS would end at 90).
        assert_eq!(folds[7].oos.end, 100);
    }

    #[test]
    fn walkforward_layout_honors_prefix_skip() {
        // 50 bars, skip 5 for readiness, IS=10, OS=5.
        // usable=45; n_folds = (45 - 10) / 5 = 7.
        let folds = walkforward_layout(50, 5, 10, 5, 0).unwrap();
        assert_eq!(folds.len(), 7);
        assert_eq!(folds[0].is, 5..15);
        assert_eq!(folds[0].oos.start, 15);
    }

    #[test]
    fn walkforward_layout_embargo_shifts_oos_start_only() {
        // Embargo drops the first bars from OOS metrics; the fold's OOS end
        // (and next fold's IS start) is unchanged.
        let folds = walkforward_layout(60, 0, 20, 10, 3).unwrap();
        assert_eq!(folds[0].is, 0..20);
        assert_eq!(folds[0].oos, 23..30);
        assert_eq!(folds[1].is, 10..30);
        assert_eq!(folds[1].oos.start, 33);
    }

    #[test]
    fn walkforward_layout_rejects_when_no_fold_fits() {
        assert!(walkforward_layout(10, 0, 20, 5, 0).is_err()); // IS > usable
        assert!(walkforward_layout(30, 25, 10, 5, 0).is_err()); // prefix > usable-fold
        assert!(walkforward_layout(50, 50, 10, 5, 0).is_err()); // prefix == n_bars
        assert!(walkforward_layout(100, 0, 20, 10, 10).is_err()); // embargo == OS
    }
}
