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
use std::path::Path;

use anyhow::{Context, Result, anyhow, bail};
use fugazi::prelude::*;
use rayon::prelude::*;
use serde_json::Value;

use crate::backtest;
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

    // Run the grid. The `first_combo` result is already computed; the rest run
    // on the pool in parallel.
    let pool = build_pool(opts.jobs)?;
    let cash = opts.cash;
    let bpy = opts.bars_per_year;
    let rf = opts.risk_free_rate;
    let base = &base_value;
    let candles_ref = &candles;
    let axes_ref = &axes;
    let fixed_ref = &fixed;

    let remaining: Vec<Row> = pool.install(|| {
        combos[1..]
            .par_iter()
            .map(|combo| {
                let params = combine_params(fixed_ref, axes_ref, combo);
                let spec = build_spec(base, &params)?;
                let m = backtest::evaluate(&spec, candles_ref, cash, bpy, rf);
                Ok::<_, anyhow::Error>(Row {
                    combo: combo.clone(),
                    metrics: m,
                })
            })
            .collect::<Result<Vec<_>>>()
    })?;

    let mut rows: Vec<Row> = Vec::with_capacity(combos.len());
    rows.push(Row {
        combo: first_combo.clone(),
        metrics: first_metrics,
    });
    rows.extend(remaining);

    // Sort by --best-by, direction-aware; None cells sort last regardless.
    if let Some((_, ref path, direction)) = best_by {
        sort_by_metric(&mut rows, path, direction);
    }

    write_csv(opts.output, &axes, &metric_columns, &rows)?;

    if !opts.quiet {
        print_header();
        print_inputs_block(&opts, &axes, &rows);
        // A "best" row only means something when the user gave us a metric to
        // rank by. Without one, the sweep has produced a CSV but no verdict.
        if best_by.is_some() {
            print_best_block(&axes, &metric_columns, best_by.as_ref(), &rows);
        }
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// Grid construction
// ---------------------------------------------------------------------------

/// One row of the grid, keyed to its axes' order.
struct Row {
    combo: Vec<Value>,
    metrics: metrics::Metrics,
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

/// Sort `rows` by `path`'s value; direction is descending → largest first,
/// ascending → smallest first. Rows whose metric is `None` (an omitted
/// degenerate ratio) always sort to the end.
fn sort_by_metric(rows: &mut [Row], path: &str, direction: Direction) {
    rows.sort_by(|a, b| {
        let av = lookup(&a.metrics, path);
        let bv = lookup(&b.metrics, path);
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

// ---------------------------------------------------------------------------
// CSV output
// ---------------------------------------------------------------------------

/// Write the sweep CSV: axis columns first (declaration order), then one
/// column per requested metric. `;`-delimited to match `trades.csv` /
/// `returns.csv`. Missing (omitted) metric values are written as an empty
/// cell.
fn write_csv(
    path: &Path,
    axes: &[(String, Vec<Value>)],
    metric_columns: &[(String, String)],
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
        header.push(name.clone());
    }
    writer.write_record(&header)?;

    for row in rows {
        let mut record: Vec<String> = row.combo.iter().map(format_value).collect();
        for (_, path) in metric_columns {
            record.push(match lookup(&row.metrics, path) {
                Some(v) => format_number(v),
                None => String::new(),
            });
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

fn print_header() {
    println!(
        "{} · {}",
        style::bold(&format!(
            "{} {}",
            env!("CARGO_PKG_NAME"),
            env!("CARGO_PKG_VERSION")
        )),
        style::dim(env!("CARGO_PKG_REPOSITORY"))
    );
    println!(
        "{}",
        style::dim("optimize · sweep a strategy over a parameter grid")
    );
    println!();
}

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
    print_field("output", &opts.output.display().to_string());
    if let Some(name) = &opts.best_by {
        print_field("best-by", name);
    }
}

fn print_best_block(
    axes: &[(String, Vec<Value>)],
    metric_columns: &[(String, String)],
    best_by: Option<&(String, String, Direction)>,
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

    if let Some((name, path, _)) = best_by {
        let value = lookup(&best.metrics, path);
        print_field(
            name,
            &value.map_or_else(|| "—".to_string(), |v| format!("{v:.4}")),
        );
    }
    for (name, path) in metric_columns {
        // Skip a metric already printed as the best-by row.
        if best_by.map(|(_, p, _)| p.as_str()) == Some(path.as_str()) {
            continue;
        }
        let value = lookup(&best.metrics, path);
        print_field(
            name,
            &value.map_or_else(|| "—".to_string(), |v| format!("{v:.4}")),
        );
    }
    // Best-row headline metrics from the run block for context.
    print_field(
        "return",
        &format!(
            "{:+.2}% ann · vol {:.2}%",
            best.metrics.returns.annualized_mean_pct,
            best.metrics.returns.annualized_volatility_pct
        ),
    );
}

fn print_field(label: &str, value: &str) {
    println!("  {}{value}", style::dim(&format!("{label:<9}")));
}

#[cfg(test)]
mod tests {
    use super::*;

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
