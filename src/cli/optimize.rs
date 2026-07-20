//! CLI wrapper for the `optimize` subcommand.
//!
//! Argument marshaling, DataFrame joining, CSV output, and console styling.
//! The pure sweep kernel — `optimize()`, walkforward layout, ranking, `Sweep` /
//! `Row` / `Evaluation` / `Subgrid` types — lives in `fugazi::spec::optimize`.

use std::collections::HashMap;
use std::num::NonZeroUsize;
use std::path::Path;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

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
use crate::run::join_universe_by_time;
use crate::style;

// Kernel imports from the library — types, ranking, walk-forward layout.
// Re-exported publicly so `crate::optimize::reject_axes_in_params` (called
// from `main.rs`) and other library-side items keep resolving through this
// module.
pub use fugazi::spec::optimize::{
    ColumnPos, Direction, Evaluation, Row, Subgrid,
    build_basket_spec, build_multi_spec, build_pairs_spec, build_portfolio_spec,
    build_spec, cartesian, combine_params, compute_union_columns, direction_for,
    format_number, format_value, lookup, lookup_windowed, mean_std_of, optimize,
    probe_params, project_row, ranking_value, reject_axes_in_params, row_dsr_inputs,
    split_axes, walkforward_layout,
};


/// Threaded-in inputs, same shape as [`crate::run::RunOptions`].
pub struct OptimizeOptions<'a> {
    pub cash: Real,
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
        StrategyKind::Pairs
        | StrategyKind::Basket
        | StrategyKind::Multi
        | StrategyKind::Portfolio => run_multi_symbol(&opts, subgrids, frame, &base_value),
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
    let started = SystemTime::now();
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
        let finished = SystemTime::now();
        let period = bar_period_line(
            atoms.first().map(|(t, _)| t.as_str()),
            atoms.last().map(|(t, _)| t.as_str()),
            atoms.len(),
        );
        style::print_header("optimize", "sweep a strategy over a parameter grid");
        style::print_warns(&collect_warnings(&skipped_overlay_columns, !opts.costs_supplied));
        print_inputs_block(
            opts,
            windowed_bars,
            &sweep.subgrid_summaries,
            &sweep.rows,
            period.as_deref(),
        );
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
    let universe: Vec<String> = match opts.strategy_kind {
        StrategyKind::Pairs => {
            let probe = build_pairs_spec(base_value, &probe_params(&subgrids[0]))?;
            let left = probe.left.clone();
            let right = probe.right.clone();
            for (idx, subgrid) in subgrids.iter().enumerate().skip(1) {
                let other = build_pairs_spec(base_value, &probe_params(subgrid))?;
                if other.left != left || other.right != right {
                    bail!(
                        "--grid #{} resolves to pair `{}`/`{}`, but --grid #1 resolves to \
                         `{}`/`{}` — every subgrid must trade the same pair",
                        idx + 1,
                        other.left,
                        other.right,
                        left,
                        right,
                    );
                }
            }
            vec![left, right]
        }
        _ => frame.symbols(),
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
    let per_symbol: Vec<(String, Vec<(String, Atom)>)> = universe
        .iter()
        .map(|sym| Ok::<_, anyhow::Error>((sym.clone(), frame.atoms(sym)?.atoms)))
        .collect::<Result<_>>()?;
    let (bars, snapshots) = join_universe_by_time(&per_symbol);
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
            StrategyKind::Pairs => {
                let spec = build_pairs_spec(base_value, params)?;
                match windowed_n {
                    Some(w) => Evaluation::Windowed(backtest::evaluate_windowed_pairs(
                        &spec,
                        snapshots_ref,
                        opts.cash,
                        bars_per_year,
                        opts.risk_free_rate,
                        cost_config,
                        effective_freq,
                        w,
                        seconds_per_bar,
                    )),
                    None => Evaluation::Whole(Box::new(backtest::evaluate_pairs(
                        &spec,
                        snapshots_ref,
                        opts.cash,
                        bars_per_year,
                        opts.risk_free_rate,
                        cost_config,
                        effective_freq,
                        seconds_per_bar,
                    ))),
                }
            }
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
            StrategyKind::Portfolio => {
                let spec = build_portfolio_spec(base_value, params)?;
                match windowed_n {
                    Some(w) => Evaluation::Windowed(backtest::evaluate_windowed_portfolio(
                        &spec,
                        snapshots_ref,
                        opts.cash,
                        bars_per_year,
                        opts.risk_free_rate,
                        cost_config,
                        effective_freq,
                        w,
                        seconds_per_bar,
                    )),
                    None => Evaluation::Whole(Box::new(backtest::evaluate_portfolio(
                        &spec,
                        snapshots_ref,
                        opts.cash,
                        bars_per_year,
                        opts.risk_free_rate,
                        cost_config,
                        effective_freq,
                        seconds_per_bar,
                    ))),
                }
            }
            _ => unreachable!(
                "run_multi_symbol only dispatched for pairs/basket/multi/portfolio"
            ),
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
        let finished = SystemTime::now();
        let period = bar_period_line(bars.first().map(String::as_str), bars.last().map(String::as_str), bars.len());
        style::print_header("optimize", "sweep a strategy over a parameter grid");
        style::print_warns(&collect_warnings(&[], !opts.costs_supplied));
        print_inputs_block(
            opts,
            windowed_bars,
            &sweep.subgrid_summaries,
            &sweep.rows,
            period.as_deref(),
        );
        if sweep.best_by.is_some() {
            print_best_block(
                &sweep.union_columns,
                &sweep.metric_columns,
                sweep.best_by.as_ref(),
                opts.risk_aversion,
                &sweep.rows,
            );
        }
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
            StrategyKind::Pairs => {
                // Pairs' chains are held eagerly (both legs known at
                // construction from `left`/`right`), so `stable_period()`
                // reads meaningful numbers on a freshly-built strategy —
                // no probe-snapshot feed needed.
                let spec = build_pairs_spec(base_value, params)?;
                let built = spec.build(cash, schema_ref);
                Ok(if keep_unstable {
                    built.warm_up_period()
                } else {
                    built.stable_period()
                })
            }
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
            StrategyKind::Portfolio => {
                // Portfolio captures per-child readiness at build (see
                // `PortfolioSpec::build`); we don't need a probe-snapshot
                // feed here — the aggregate is already the max child
                // stable/warm-up, computed on typed children before they
                // were boxed. Costs don't affect readiness, so we build
                // without.
                let spec = build_portfolio_spec(base_value, params)?;
                let built = spec.build(cash, schema_ref, None);
                Ok(if keep_unstable {
                    built.warm_up_period()
                } else {
                    built.stable_period()
                })
            }
            _ => unreachable!(
                "run_multi_symbol_walkforward only dispatched for pairs/basket/multi/portfolio"
            ),
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
                StrategyKind::Pairs => {
                    let spec = build_pairs_spec(base_value, params)?;
                    backtest::measured_report_from_strategy(
                        || spec.build(cash, schema_ref),
                        snapshots_ref,
                        cash,
                        build_per_symbol_costs(),
                    )
                }
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
                StrategyKind::Portfolio => {
                    // Portfolio uses its own composite wallet driver.
                    // The unscoped default costs are installed as every
                    // sub-wallet's fallback; per-symbol scoped bundles
                    // are then installed on every sub via
                    // `install_costs_for` so whichever child ends up
                    // filling a given symbol books at the right rate.
                    let spec = build_portfolio_spec(base_value, params)?;
                    let default_costs = cost_config.resolve("", effective_freq);
                    let costs_opt = (!default_costs.is_none()).then_some(default_costs);
                    backtest::measured_report_portfolio(
                        || {
                            let mut p = spec.build(cash, schema_ref, costs_opt);
                            for sym in universe.iter() {
                                let c = cost_config.resolve(sym, effective_freq);
                                p.install_costs_for(sym, c);
                            }
                            p
                        },
                        snapshots_ref,
                    )
                }
                _ => unreachable!(
                    "run_multi_symbol_walkforward only dispatched for pairs/basket/multi/portfolio"
                ),
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

// ---------------------------------------------------------------------------
// Console output
// ---------------------------------------------------------------------------

fn print_inputs_block(
    opts: &OptimizeOptions,
    windowed_bars: Option<NonZeroUsize>,
    subgrid_summaries: &[(String, usize)],
    rows: &[Row],
    period: Option<&str>,
) {
    style::print_section("inputs");
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
    if let Some(p) = period {
        print_field("period", p);
    }
    print_field("capital", &format!("{:.2}", opts.cash));
    // Costs summary — same treatment as `run`: name it explicitly if a model is
    // set, note `none (explicit)` if the user opted in silently. The
    // no-cost warning has been hoisted above the block by `collect_warnings`.
    if !opts.cost_config.is_none() {
        print_field("costs", "active (commission/spread/slippage applied)");
    } else if opts.costs_supplied {
        print_field("costs", "none (explicit)");
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

/// The "result" block for `optimize`: number of grid points evaluated, then
/// wall-clock timing. Mirrors `run`'s result block so both commands look the
/// same at the tail.
fn print_result_block(points: usize, started: SystemTime, finished: SystemTime) {
    println!();
    style::print_section("result");
    print_field("points", &points.to_string());
    let elapsed = finished.duration_since(started).unwrap_or_default();
    print_field("started", &format_utc(started));
    print_field(
        "finished",
        &format!("{} ({})", format_utc(finished), format_elapsed(elapsed)),
    );
}

/// Collect the top-of-run warnings for `optimize` — same shape as `run`'s.
fn collect_warnings(skipped: &[String], no_cost: bool) -> Vec<String> {
    let mut w = Vec::new();
    if !skipped.is_empty() {
        w.push(format!(
            "skipped non-numeric overlay column{}: {} — not accessible via `!get`",
            if skipped.len() == 1 { "" } else { "s" },
            skipped.join(", "),
        ));
    }
    if no_cost {
        w.push(
            "no cost model set — commission, spread, and slippage are zero; \
             grid results are frictionless"
                .to_string(),
        );
    }
    w
}

/// `start → end (N bars)` when the atom stream has at least one entry, else
/// `None`. Shared by the single-asset and multi-symbol drivers so both echo
/// the same period line as `run` does.
fn bar_period_line(start: Option<&str>, end: Option<&str>, bars: usize) -> Option<String> {
    let (s, e) = (start?, end?);
    Some(format!("{s} → {e} ({bars} bars)"))
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

fn print_best_block(
    union_columns: &[String],
    metric_columns: &[(String, String)],
    best_by: Option<&(String, String, Direction)>,
    k: Real,
    rows: &[Row],
) {
    println!();
    style::print_section("best");
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
        print_field(&friendly_metric_label(path), &value);
    }
    for (_name, path) in metric_columns {
        // Skip a metric already printed as the best-by row.
        if best_by.map(|(_, p, _)| p.as_str()) == Some(path.as_str()) {
            continue;
        }
        print_field(&friendly_metric_label(path), &format_metric(&best.eval, path));
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
    style::print_field(label, value, 9);
}

/// A trailing hangover line under a `print_field` — indented to sit under the
/// value column (2 leading spaces + 9-char label column = 11 spaces).
fn print_indented(text: &str) {
    style::print_field_continuation(text, 9);
}

fn format_elapsed(d: Duration) -> String {
    let secs = d.as_secs_f64();
    if secs < 1.0 {
        format!("{} ms", d.as_millis())
    } else if secs < 60.0 {
        format!("{secs:.2} s")
    } else {
        format!("{}m {:02}s", d.as_secs() / 60, d.as_secs() % 60)
    }
}

/// Format a [`SystemTime`] as `YYYY-MM-DD HH:MM:SS UTC` — same as `run.rs`.
/// Kept here (not lifted to `style.rs`) since the caller is inside this
/// module's result-block flow.
fn format_utc(t: SystemTime) -> String {
    let secs = t.duration_since(UNIX_EPOCH).map_or(0, |d| d.as_secs());
    let (days, rem) = (secs / 86_400, secs % 86_400);
    let (hour, min, sec) = (rem / 3_600, (rem % 3_600) / 60, rem % 60);

    let z = days as i64 + 719_468;
    let era = if z >= 0 { z } else { z - 146_096 } / 146_097;
    let doe = z - era * 146_097;
    let yoe = (doe - doe / 1_460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let day = doy - (153 * mp + 2) / 5 + 1;
    let month = if mp < 10 { mp + 3 } else { mp - 9 };
    let year = yoe + era * 400 + i64::from(month <= 2);

    format!("{year:04}-{month:02}-{day:02} {hour:02}:{min:02}:{sec:02} UTC")
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
        style::print_warns(&collect_warnings(skipped_overlay_columns, false));
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
    style::print_section("inputs");
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
