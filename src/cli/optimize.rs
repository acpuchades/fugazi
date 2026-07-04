//! `optimize` — parameter-grid sweep over a strategy.
//!
//! Same shape as `run`: one strategy YAML + `--series` bars + calendar/rf +
//! `--params` for baseline placeholders. What's new is that a `--params` value
//! may be a **sweep axis** rather than a fixed scalar — either a JSON array
//! (`[3,5,8]`) or an inclusive numeric range (`3..20:1`, with an optional
//! `:step`). Every combination of the axes is a **grid point**: for each one
//! we drive [`crate::backtest::evaluate`] and record its [`crate::metrics`]
//! document.
//!
//! Output is one `;`-delimited CSV file (`-o/--output`) with one row per grid
//! point: axis columns first (in declaration order), then one column per
//! `-m/--metrics` name. Rows are sorted by `--best-by` when it's set (descending
//! for max-oriented metrics like `sharpe`, ascending for min-oriented ones like
//! `max_pct`); otherwise the row order follows the cartesian enumeration.
//!
//! The grid runs on a rayon thread pool (`-j/--jobs` picks the size; default is
//! rayon's own default — one worker per logical CPU). Each combination
//! independently clones the parsed strategy tree, applies substitution, and
//! evaluates — no shared mutable state, no locking on the hot path.

use std::collections::HashMap;
use std::num::NonZeroUsize;
use std::path::Path;

use anyhow::{Context, Result, anyhow, bail};
use fugazi::prelude::*;
use rayon::prelude::*;
use serde_json::Value;

use crate::backtest;
use crate::calendar::Frequency;
use crate::costs::CostConfig;
use crate::data::DataFrame;
use crate::input;
use crate::metrics;
use crate::params;
use crate::spec::StrategySpec;
use crate::style;

/// Sort direction of a `--best-by` optimization: descending = higher is better
/// (Sharpe, CAGR, …); ascending = lower is better (drawdown, volatility, VaR, …).
#[derive(Clone, Copy, Debug, PartialEq)]
enum Direction {
    Descending,
    Ascending,
}

/// One sweep axis: `NAME → values`, preserving the enumeration order.
type Axis = (String, Vec<Value>);
/// A fixed (scalar) params table + the sweep axes carved out of it.
type Partition = (HashMap<String, Value>, Vec<Axis>);

/// Threaded-in inputs, same shape as [`crate::backtest::RunOptions`].
pub struct OptimizeOptions<'a> {
    pub cash: Real,
    pub strategy_text: &'a str,
    pub strategy_label: &'a str,
    /// Effective `--params` table: sweep axes stay as `Value::Array` /
    /// range-shaped strings, fixed scalars stay as scalars.
    pub params_table: HashMap<String, Value>,
    /// The `-m/--metrics` names to emit as CSV columns.
    pub metrics: Vec<String>,
    /// The `--best-by` metric name to sort by (empty = no sort).
    pub best_by: Option<String>,
    pub output: &'a Path,
    pub bars_per_year: Real,
    pub risk_free_rate: Real,
    /// Disable the default stability gating (and its metric anchor) for every
    /// grid point — see `run --keep-unstable`.
    pub keep_unstable: bool,
    /// Evaluate each grid point in non-overlapping windows of this many bars
    /// (same windowing as `run -w`): every `-m` metric becomes two CSV columns
    /// (`<name>_mean` / `<name>_std`, cross-window over the windows where the
    /// metric is defined) and `--best-by` ranks by the windowed mean.
    pub windowed: Option<NonZeroUsize>,
    /// `-k/--risk-aversion`: shift each grid point's `--best-by` cross-window
    /// mean *against* it by this many standard deviations before ranking
    /// (direction-aware: `mean − k·std` descending, `mean + k·std` ascending).
    /// `0.0` = rank by the plain mean. Only meaningful with `windowed`.
    pub risk_aversion: Real,
    /// Cost model configured via `--costs`. Every grid point resolves against
    /// the same config for its (strategy symbol, frequency) pair.
    pub cost_config: &'a CostConfig,
    /// Bar frequency (from `-f/--frequency`), forwarded to
    /// [`CostConfig::resolve`] for each grid point.
    pub frequency: Option<Frequency>,
    /// Whether the user passed at least one `--costs` flag — governs the
    /// warning banner.
    pub costs_supplied: bool,
    pub jobs: Option<usize>,
    pub quiet: bool,
}

/// Run the sweep per `opts`, writing the CSV and printing the best row.
pub fn run(frame: &DataFrame, opts: OptimizeOptions) -> Result<()> {
    let (fixed, axes) = split_axes(&opts.params_table)?;
    if axes.is_empty() {
        bail!(
            "--params has no sweep axes: at least one value must be a `[...]` list \
             or a `start..end[:step]` range (use `run` for a single combination)"
        );
    }
    // A negative k would *reward* dispersion — the opposite of what the flag
    // is for. (Presence alongside `-w`/`--best-by` is enforced by clap.)
    if opts.risk_aversion < 0.0 {
        bail!("--risk-aversion must be >= 0 (got {})", opts.risk_aversion);
    }

    let base_value = input::parse_value(opts.strategy_text).context("parsing strategy")?;
    let combos = cartesian(&axes);

    // Probe the first combination once, up front: it validates the strategy YAML
    // (early error), gives us the symbol so we can slice the candle stream once,
    // and gives us a Metrics document to resolve `--metrics` and `--best-by`
    // names against before spinning up the pool.
    let first_combo = &combos[0];
    let first_params = combine_params(&fixed, &axes, first_combo);
    let first_spec = build_spec(&base_value, &first_params)?;
    let symbol = first_spec.symbol.clone();
    let candles = frame.candles(&symbol)?;
    let first_metrics = backtest::evaluate(
        &first_spec,
        &candles,
        opts.cash,
        opts.bars_per_year,
        opts.risk_free_rate,
        opts.keep_unstable,
        opts.cost_config,
        opts.frequency,
    );

    // Resolve column paths once — errors here catch typos before the sweep.
    let metric_columns: Vec<(String, String)> = opts
        .metrics
        .iter()
        .map(|name| {
            let (path, _) = metrics::resolve_metric(name, &first_metrics)?;
            Ok::<_, anyhow::Error>((name.clone(), path))
        })
        .collect::<Result<Vec<_>>>()?;

    let best_by = opts
        .best_by
        .as_deref()
        .map(|name| {
            let (path, _) = metrics::resolve_metric(name, &first_metrics)?;
            let direction = direction_for(&path).ok_or_else(|| {
                anyhow!(
                    "--best-by `{name}` has no built-in direction; pass one whose \
                     direction is known (e.g. sharpe, sortino, cagr_pct, max_pct, \
                     ulcer_index, annualized_volatility_pct)"
                )
            })?;
            Ok::<_, anyhow::Error>((name.to_string(), path, direction))
        })
        .transpose()?;

    // Run the grid. The `first_combo` result is already computed (windowed
    // mode re-evaluates it windowed — one extra backtest, off the hot path);
    // the rest run on the pool in parallel.
    let pool = build_pool(opts.jobs)?;
    let cash = opts.cash;
    let bpy = opts.bars_per_year;
    let rf = opts.risk_free_rate;
    let keep_unstable = opts.keep_unstable;
    let windowed = opts.windowed.map(NonZeroUsize::get);
    let base = &base_value;
    let candles_ref = &candles;
    let axes_ref = &axes;
    let fixed_ref = &fixed;

    let cost_config = opts.cost_config;
    let frequency = opts.frequency;
    let evaluate = |spec: &StrategySpec| -> Evaluation {
        match windowed {
            Some(w) => Evaluation::Windowed(backtest::evaluate_windowed(
                spec,
                candles_ref,
                cash,
                bpy,
                rf,
                keep_unstable,
                cost_config,
                frequency,
                w,
            )),
            None => Evaluation::Whole(Box::new(backtest::evaluate(
                spec,
                candles_ref,
                cash,
                bpy,
                rf,
                keep_unstable,
                cost_config,
                frequency,
            ))),
        }
    };

    let remaining: Vec<Row> = pool.install(|| {
        combos[1..]
            .par_iter()
            .map(|combo| {
                let params = combine_params(fixed_ref, axes_ref, combo);
                let spec = build_spec(base, &params)?;
                Ok::<_, anyhow::Error>(Row {
                    combo: combo.clone(),
                    eval: evaluate(&spec),
                })
            })
            .collect::<Result<Vec<_>>>()
    })?;

    let mut rows: Vec<Row> = Vec::with_capacity(combos.len());
    rows.push(Row {
        combo: first_combo.clone(),
        eval: match windowed {
            Some(_) => evaluate(&first_spec),
            None => Evaluation::Whole(Box::new(first_metrics)),
        },
    });
    rows.extend(remaining);

    // Sort by --best-by, direction-aware; None cells sort last regardless.
    if let Some((_, ref path, direction)) = best_by {
        sort_by_metric(&mut rows, path, direction, opts.risk_aversion);
    }

    write_csv(opts.output, &axes, &metric_columns, windowed.is_some(), &rows)?;

    if !opts.quiet {
        style::print_header("optimize", "sweep a strategy over a parameter grid");
        print_inputs_block(&opts, &axes, &rows);
        // A "best" row only means something when the user gave us a metric to
        // rank by. Without one, the sweep has produced a CSV but no verdict.
        if best_by.is_some() {
            print_best_block(
                &axes,
                &metric_columns,
                best_by.as_ref(),
                opts.risk_aversion,
                &rows,
            );
        }
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// Grid construction
// ---------------------------------------------------------------------------

/// One grid point's metric evaluation: the whole measured run reduced to a
/// single document, or (`-w/--windowed`) one document per non-overlapping
/// window, aggregated per metric as cross-window mean ± stddev.
enum Evaluation {
    /// Boxed: the document is ~50 fields, dwarfing the windowed variant's Vec.
    Whole(Box<metrics::Metrics>),
    Windowed(Vec<metrics::WindowMetrics>),
}

/// One row of the grid, keyed to its axes' order.
struct Row {
    combo: Vec<Value>,
    eval: Evaluation,
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
fn build_spec(base: &Value, params: &HashMap<String, Value>) -> Result<StrategySpec> {
    let value = params::substitute(base.clone(), params)?;
    Ok(serde_json::from_value(value)?)
}

/// Build the rayon pool — an explicit `-j/--jobs` count, else rayon's default
/// (one worker per logical CPU).
fn build_pool(jobs: Option<usize>) -> Result<rayon::ThreadPool> {
    let mut builder = rayon::ThreadPoolBuilder::new();
    if let Some(n) = jobs {
        builder = builder.num_threads(n);
    }
    builder
        .build()
        .context("building the rayon thread pool for --jobs")
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
fn sort_by_metric(rows: &mut [Row], path: &str, direction: Direction, k: Real) {
    rows.sort_by(|a, b| {
        let av = ranking_value(&a.eval, path, direction, k);
        let bv = ranking_value(&b.eval, path, direction, k);
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
}

/// Look up a metric by its canonical dotted path against a Metrics document.
/// Uses [`metrics::resolve_metric`]'s value channel (a re-serialize per call,
/// but only invoked when sorting/printing — not on the hot per-combo path).
fn lookup(m: &metrics::Metrics, path: &str) -> Option<Real> {
    metrics::resolve_metric(path, m).ok().and_then(|(_, v)| v)
}

/// A windowed evaluation's cross-window `(mean, stddev)` for one metric path,
/// over the windows where the metric is defined; `None` when it is degenerate
/// in every window.
fn lookup_windowed(windows: &[metrics::WindowMetrics], path: &str) -> Option<(Real, Real)> {
    metrics::mean_std(windows.iter().filter_map(|w| lookup(&w.metrics, path)))
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

/// Write the sweep CSV: axis columns first (declaration order), then one
/// column per requested metric — or, under `-w/--windowed`, two columns per
/// metric (`<name>_mean` / `<name>_std`, the cross-window aggregate).
/// `;`-delimited to match `trades.csv` / `returns.csv`. Missing (omitted)
/// metric values are written as an empty cell.
fn write_csv(
    path: &Path,
    axes: &[(String, Vec<Value>)],
    metric_columns: &[(String, String)],
    windowed: bool,
    rows: &[Row],
) -> Result<()> {
    if let Some(parent) = path.parent()
        && !parent.as_os_str().is_empty()
    {
        std::fs::create_dir_all(parent)
            .with_context(|| format!("creating output dir `{}`", parent.display()))?;
    }
    let mut writer = csv::WriterBuilder::new()
        .delimiter(b';')
        .from_path(path)
        .with_context(|| format!("creating `{}`", path.display()))?;

    let mut header: Vec<String> = axes.iter().map(|(name, _)| name.clone()).collect();
    for (name, _) in metric_columns {
        if windowed {
            header.push(format!("{name}_mean"));
            header.push(format!("{name}_std"));
        } else {
            header.push(name.clone());
        }
    }
    writer.write_record(&header)?;

    let cell = |v: Option<Real>| v.map(format_number).unwrap_or_default();
    for row in rows {
        let mut record: Vec<String> = row.combo.iter().map(format_value).collect();
        for (_, path) in metric_columns {
            match &row.eval {
                Evaluation::Whole(m) => record.push(cell(lookup(m, path))),
                Evaluation::Windowed(ws) => {
                    let spread = lookup_windowed(ws, path);
                    record.push(cell(spread.map(|(mean, _)| mean)));
                    record.push(cell(spread.map(|(_, std)| std)));
                }
            }
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

fn print_inputs_block(opts: &OptimizeOptions, axes: &[(String, Vec<Value>)], rows: &[Row]) {
    println!("{}", style::bold("inputs"));
    print_field("strategy", opts.strategy_label);
    let axes_label: String = axes
        .iter()
        .map(|(name, values)| format!("{name}({})", values.len()))
        .collect::<Vec<_>>()
        .join(", ");
    print_field("grid", &format!("{} points · {axes_label}", rows.len()));
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
    if let Some(w) = opts.windowed {
        print_field("windowed", &format!("{w}-bar windows (mean ± std per metric)"));
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

fn print_best_block(
    axes: &[(String, Vec<Value>)],
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

    let params_label: String = axes
        .iter()
        .zip(best.combo.iter())
        .map(|((name, _), v)| format!("{name}={}", format_value(v)))
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
        let windows = metrics::windowed_from_report(&report, 2, 252.0, 0.0);
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
}
