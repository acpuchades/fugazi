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

//! # spec::optimize
//!
//! Pure sweep-kernel — the strategy-agnostic Cartesian enumeration, ranking,
//! and walk-forward layout that drive the CLI's `optimize` subcommand. Reused
//! by the CLI wrapper in `src/cli/optimize.rs`, which owns the I/O (frame
//! marshaling, CSV output, console styling, progress banners).
//!
//! Reachable from downstream crates (the Python bindings, hosting servers,
//! batch runners) via `fugazi::spec::optimize::*` without pulling in the
//! CLI's clap / csv / progress stack.

use std::collections::HashMap;

use anyhow::{Result, anyhow, bail};
use crate::prelude::*;
use rayon::prelude::*;
use serde_json::Value;

use crate::spec::metrics;
use crate::spec::params;
use crate::spec::{
    BasketStrategySpec, MultiAssetStrategySpec, PairsStrategySpec, PortfolioSpec,
    SingleStrategySpec,
};

/// Sort direction of a `--best-by` optimization: descending = higher is better
/// (Sharpe, CAGR, …); ascending = lower is better (drawdown, volatility, VaR, …).
#[derive(Clone, Copy, Debug, PartialEq)]
pub enum Direction {
    Descending,
    Ascending,
}

/// One sweep axis: `NAME → values`, preserving the enumeration order.
pub type Axis = (String, Vec<Value>);
/// A fixed (scalar) params table + the sweep axes carved out of it.
type Partition = (HashMap<String, Value>, Vec<Axis>);


/// Params for the probe spec: subgrid's fixed scalars + the first value of each
/// of its axes. When the subgrid has no axes this is just the fixed map.
pub fn probe_params(subgrid: &Subgrid) -> HashMap<String, Value> {
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
pub fn optimize<F>(
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
    let pool = crate::spec::pool::build_pool(jobs)?;

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
pub fn sample_metrics(eval: &Evaluation) -> Option<&metrics::Metrics> {
    match eval {
        Evaluation::Whole(m) => Some(m.as_ref()),
        Evaluation::Windowed(ws) => ws.first().map(|w| &w.metrics),
    }
}

/// Substitute a params table into the base strategy value, then typed-parse
/// as a [`PairsStrategySpec`]. Pairs twin of [`build_spec`].
pub fn build_pairs_spec(base: &Value, params: &HashMap<String, Value>) -> Result<PairsStrategySpec> {
    let value = params::substitute(base.clone(), params)?;
    Ok(serde_json::from_value(value)?)
}

/// Substitute a params table into the base strategy value, then typed-parse
/// as a [`BasketStrategySpec`]. Basket twin of [`build_spec`].
pub fn build_basket_spec(base: &Value, params: &HashMap<String, Value>) -> Result<BasketStrategySpec> {
    let value = params::substitute(base.clone(), params)?;
    Ok(serde_json::from_value(value)?)
}

/// Substitute a params table into the base strategy value, then typed-parse
/// as a [`MultiAssetStrategySpec`]. Multi-asset twin of [`build_spec`].
pub fn build_multi_spec(
    base: &Value,
    params: &HashMap<String, Value>,
) -> Result<MultiAssetStrategySpec> {
    let value = params::substitute(base.clone(), params)?;
    Ok(serde_json::from_value(value)?)
}

/// Substitute a params table into the base strategy value, then typed-parse
/// as a [`PortfolioSpec`]. Portfolio twin of [`build_spec`].
pub fn build_portfolio_spec(
    base: &Value,
    params: &HashMap<String, Value>,
) -> Result<PortfolioSpec> {
    let value = params::substitute(base.clone(), params)?;
    Ok(serde_json::from_value(value)?)
}

/// The union of axis-column names across every subgrid: every axis name, plus
/// every scalar name whose effective value differs across subgrids (or is
/// absent in at least one). Name-sorted so the header is stable regardless of
/// flag order.
pub fn compute_union_columns(subgrids: &[Subgrid]) -> Vec<String> {
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
pub fn project_row(subgrid: &Subgrid, combo: &[Value], union_columns: &[String]) -> Vec<Option<Value>> {
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
pub fn subgrid_label(subgrid: &Subgrid, union_columns: &[String]) -> String {
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
pub fn compute_dsr_context(rows: &[Row]) -> Option<(usize, Real)> {
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
pub fn row_summary_sharpe(row: &Row) -> Option<Real> {
    match &row.eval {
        Evaluation::Whole(m) => m.risk_adjusted.sharpe,
        Evaluation::Windowed(ws) => mean_of(ws.iter().map(|w| w.metrics.risk_adjusted.sharpe)),
    }
}

/// Arithmetic mean of the defined entries, or `None` when none are defined.
pub fn mean_of(iter: impl IntoIterator<Item = Option<Real>>) -> Option<Real> {
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
pub fn row_dsr_inputs(row: &Row) -> (Option<Real>, Option<Real>, Option<Real>, usize, Real) {
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
pub enum Evaluation {
    /// Boxed: the document is ~50 fields, dwarfing the windowed variant's Vec.
    Whole(Box<metrics::Metrics>),
    Windowed(Vec<metrics::WindowMetrics>),
}

/// One folded subgrid: its scalar map (baseline layered under this subgrid's
/// `--grid` scalars, minus any name carved out as an axis) plus its axes
/// (name-sorted) and cartesian combos over those axes. A `--grid` flag with
/// only scalars yields one combo (the empty tuple) — a single grid point.
pub struct Subgrid {
    pub fixed: HashMap<String, Value>,
    pub axes: Vec<Axis>,
    pub combos: Vec<Vec<Value>>,
}

impl Subgrid {
    pub fn points(&self) -> usize {
        self.combos.len()
    }
}

/// One row of the grid, sparse across the union of every subgrid's axis
/// columns. `values[i]` is the value for `Sweep::union_columns[i]` — `None`
/// when this row's subgrid doesn't reference that name (the CSV writes the
/// empty cell; the best block skips it).
pub struct Row {
    pub values: Vec<Option<Value>>,
    pub eval: Evaluation,
}

/// Rows and metadata produced by [`optimize`], ready for the CLI to write out.
/// `rows` is sorted by `best_by`'s ranking value when `best_by` is `Some`,
/// otherwise it follows the subgrid-then-cartesian enumeration order.
pub struct Sweep {
    /// The union of every subgrid's axis names, plus every scalar name whose
    /// effective value differs across subgrids — name-sorted. This is exactly
    /// the CSV axis-column header, and it indexes each [`Row::values`].
    pub union_columns: Vec<String>,
    /// One entry per `--grid` flag, in flag order — for the inputs block
    /// breakdown. Each entry is `(axes label, point count)` where the label is
    /// e.g. `"X=\"A\", Y(10)"` (scalars inline, axes as `NAME(N)`); when the
    /// subgrid has neither a scalar override nor an axis it reads `"(baseline)"`.
    pub subgrid_summaries: Vec<(String, usize)>,
    /// Metric column paths resolved against the probe document (`name` → dotted
    /// path). Errors out of [`optimize`] if any name doesn't resolve.
    pub metric_columns: Vec<(String, String)>,
    /// The `--best-by` name, its resolved dotted path, and its direction.
    /// `None` when no `--best-by` was passed.
    pub best_by: Option<(String, String, Direction)>,
    pub rows: Vec<Row>,
    /// True iff `windowed` was set — the CSV writer uses this to emit
    /// `<name>_mean` / `<name>_std` columns per metric.
    pub windowed: bool,
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
    pub deflated_sharpe_context: Option<(usize, Real)>,
}

/// True iff `v` is axis-shaped — a JSON array or a `start..end[:step]`
/// range-shaped string. Used both to carve axes out of a subgrid table
/// (`split_axes`) and to reject axes in the `--params` baseline
/// (`reject_axes_in_params`) — one detector, one meaning.
pub fn is_axis_value(v: &Value) -> bool {
    match v {
        Value::Array(items) => !items.is_empty(),
        Value::String(s) => try_parse_range(s).is_some(),
        _ => false,
    }
}

/// Error if any `--params` value looks like a sweep axis — those must go
/// through `--grid`. The error names every offender so a user with several
/// mistakes fixes them all in one edit rather than one at a time.
pub fn reject_axes_in_params(params: &HashMap<String, Value>) -> Result<()> {
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
pub fn split_axes(params: &HashMap<String, Value>) -> Result<Partition> {
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
pub fn try_parse_range(s: &str) -> Option<Vec<Value>> {
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
pub fn cartesian(axes: &[(String, Vec<Value>)]) -> Vec<Vec<Value>> {
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
pub fn combine_params(
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
pub fn build_spec(base: &Value, params: &HashMap<String, Value>) -> Result<SingleStrategySpec> {
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
pub fn direction_for(path: &str) -> Option<Direction> {
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
pub fn sort_by_metric(rows: &mut Vec<Row>, path: &str, direction: Direction, k: Real) {
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
pub type ColumnPos = usize;

/// Look up a metric by its canonical dotted path against a Metrics document.
/// Uses [`metrics::flatten`] — one Vec allocation of ~60 tuples per call. Fine
/// for one-shot printing / the winning-combo lookup; hot loops (the sort
/// comparator and the CSV writer) precompute positions and flatten once per
/// row instead.
pub fn lookup(m: &metrics::Metrics, path: &str) -> Option<Real> {
    metrics::flatten(m)
        .into_iter()
        .find(|(k, _)| *k == path)
        .and_then(|(_, v)| v)
}

/// A windowed evaluation's cross-window `(mean, stddev)` for one metric path,
/// over the windows where the metric is defined; `None` when it is degenerate
/// in every window.
pub fn lookup_windowed(windows: &[metrics::WindowMetrics], path: &str) -> Option<(Real, Real)> {
    metrics::mean_std(windows.iter().filter_map(|w| lookup(&w.metrics, path)))
}

/// Cross-window `(mean, stddev)` where each window's value is already known —
/// the twin of [`lookup_windowed`] that avoids repeated flattening when the
/// caller has already indexed by column position.
pub fn mean_std_of<I: Iterator<Item = Option<Real>>>(values: I) -> Option<(Real, Real)> {
    metrics::mean_std(values.flatten())
}

/// The single value a row is *ranked* by for a metric path: the whole-run
/// value, or the cross-window mean shifted **against** the row by `k`
/// standard deviations — `mean − k·std` for a higher-is-better (descending)
/// metric, `mean + k·std` for a lower-is-better (ascending) one, so a large
/// spread is always penalized, never rewarded.
pub fn ranking_value(eval: &Evaluation, path: &str, direction: Direction, k: Real) -> Option<Real> {
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


/// Format a grid axis value as it appears in the CSV. Integers stay integer-
/// looking (`5` not `5.0`); strings drop their JSON quotes.
pub fn format_value(v: &Value) -> String {
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
pub fn format_number(v: Real) -> String {
    // A whole f64 still round-trips through `{v}` (Display) as e.g. `5`, but
    // we want the CSV column to read the same shape a spreadsheet expects.
    format!("{v}")
}


/// One fold's bar ranges — same layout across every grid row (fold boundaries
/// are grid-wide, not per-row, so per-fold metrics are directly comparable).
pub struct FoldLayout {
    pub is: std::ops::Range<usize>,
    /// First bar included in OOS metric evaluation (post-embargo). State still
    /// rolls through the embargo bars — they're just dropped from the OOS
    /// reduction.
    pub oos: std::ops::Range<usize>,
}


/// Compute the per-fold ranges. Fold `k` occupies IS
/// `[prefix + k*oos, prefix + k*oos + is)` and OOS
/// `[prefix + k*oos + is + embargo, prefix + k*oos + is + oos)`. The final
/// fold's OOS extends to `n_bars` so trailing bars aren't dropped.
pub fn walkforward_layout(
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

// ---------------------------------------------------------------------------
// Walk-forward driver kernel
// ---------------------------------------------------------------------------

/// One row of the per-fold walk-forward table: the winner's params projected
/// onto the sweep's [`Sweep::union_columns`], its IS + OOS bar ranges, and
/// both metric documents so the caller can emit `_is`/`_oos`/`_wfe` triples
/// per metric column.
pub struct WalkForwardRow {
    pub fold: usize,
    pub is_start: usize,
    pub is_end: usize,
    pub oos_start: usize,
    pub oos_end: usize,
    pub values: Vec<Option<Value>>,
    pub is_metrics: metrics::Metrics,
    pub oos_metrics: metrics::Metrics,
}

/// The full result of a walk-forward run: per-fold rows plus the stitched
/// composite OOS artefacts. The CLI wrapper reduces this into three sibling
/// files (per-fold CSV, composite equity CSV, composite metrics YAML); Python
/// / other embedders reduce it into whatever shape they want.
pub struct WalkForwardResult {
    /// Union of every subgrid's axis columns — mirrors [`Sweep::union_columns`].
    pub union_columns: Vec<String>,
    /// Resolved `(user_name, canonical_dotted_path)` for every requested metric
    /// — mirrors [`Sweep::metric_columns`].
    pub metric_columns: Vec<(String, String)>,
    /// The `--best-by` name, resolved path and direction (`None` = first-in-
    /// enumeration wins each fold).
    pub best_by: Option<(String, String, Direction)>,
    /// Grid-wide max readiness (bars) from the pre-scan — the head skip fed
    /// into [`walkforward_layout`].
    pub prefix_skip: usize,
    /// The resolved fold layout — the same [`FoldLayout`] vector
    /// [`walkforward_layout`] returned.
    pub folds: Vec<FoldLayout>,
    /// One per fold, winner-selected by IS metric ranking. Same order as
    /// [`Self::folds`].
    pub fold_rows: Vec<WalkForwardRow>,
    /// The stitched OOS equity curve — each fold's winner's OOS slice scaled
    /// into the running composite so mode-switches don't create jumps.
    pub composite_equity: Vec<Real>,
    /// Fills from the stitched composite, with per-fold bar offsets applied.
    pub composite_fills: Vec<crate::Fill<String>>,
    /// The composite equity curve reduced through the full metrics catalogue.
    pub composite_metrics: metrics::Metrics,
    /// The resolved IS / OOS / embargo bar counts (post-`WalkForwardSpec::resolve`).
    pub is_bars: usize,
    pub oos_bars: usize,
    pub embargo_bars: usize,
    /// The starting cash used to seed the composite report.
    pub cash: Real,
}

/// Pure walk-forward kernel — strategy-agnostic. Runs one full backtest per
/// grid row (via `run_backtest`), pre-scans grid-wide readiness (via
/// `probe_readiness`), computes the fold layout, and per fold ranks by
/// IS-metric to pick a winner whose OOS slice contributes to the composite.
///
/// `probe_readiness` should return the row's `stable_period()` (or
/// `warm_up_period()` under a keep-unstable opt-out).
///
/// `run_backtest` should return the full-run [`RunReport`](crate::RunReport) —
/// per-fold slicing happens inside via [`metrics::report_slice`].
///
/// The CLI's `walkforward_run` wraps this with `WalkForwardSpec` resolution,
/// output-path derivation, CSV emission, and console printing.
#[allow(clippy::too_many_arguments)]
pub fn walkforward<P, R>(
    subgrids: Vec<Subgrid>,
    n_bars: usize,
    probe_readiness: P,
    run_backtest: R,
    bars_per_year: Real,
    risk_free_rate: Real,
    seconds_per_bar: Option<Real>,
    is_bars: usize,
    oos_bars: usize,
    embargo_bars: usize,
    metric_names: &[String],
    best_by: Option<&str>,
    jobs: Option<usize>,
    cash: Real,
) -> Result<WalkForwardResult>
where
    P: Fn(&HashMap<String, Value>) -> Result<usize> + Sync,
    R: Fn(&HashMap<String, Value>) -> Result<crate::RunReport<String>> + Sync,
{
    assert!(!subgrids.is_empty(), "walkforward: called with zero subgrids");

    // Grid enumeration — same shape as [`optimize`] so subgrids stack the same
    // way and the union-column projection is compatible with the per-fold row.
    let union_columns = compute_union_columns(&subgrids);
    let plan: Vec<(usize, usize)> = subgrids
        .iter()
        .enumerate()
        .flat_map(|(si, s)| (0..s.combos.len()).map(move |ci| (si, ci)))
        .collect();

    // Pre-scan: probe every row's readiness and take the grid-wide max, so
    // every row's IS/OOS ranges are identical and per-fold metrics are
    // directly comparable regardless of which combo winds up warming up faster.
    let pool = crate::spec::pool::build_pool(jobs)?;
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
    // slicing is a bounded-cost operation.
    let run_ref = &run_backtest;
    let reports: Vec<crate::RunReport<String>> = pool.install(|| {
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
    // Metrics document, not a fold slice — a narrow slice can leave many
    // metrics `None`, and short-name matching requires a numeric leaf.
    let sample_metrics = if let Some(first_report) = reports.first() {
        metrics::from_report(first_report, bars_per_year, risk_free_rate, seconds_per_bar)
    } else {
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
    let mut composite_fills: Vec<crate::Fill<String>> = Vec::new();
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
            composite_fills.push(crate::Fill {
                bar: fill.bar + bar_offset,
                order: fill.order,
            });
        }
        running_equity = composite_equity.last().copied().unwrap_or(running_equity);

        let (si, ci) = plan[winner_idx];
        let values = project_row(&subgrids[si], &subgrids[si].combos[ci], &union_columns);

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

    let composite_report = crate::RunReport {
        equity_curve: composite_equity.clone(),
        fills: composite_fills.clone(),
        initial_equity: cash,
    };
    let composite_metrics = metrics::from_report(
        &composite_report,
        bars_per_year,
        risk_free_rate,
        seconds_per_bar,
    );

    Ok(WalkForwardResult {
        union_columns,
        metric_columns,
        best_by,
        prefix_skip,
        folds,
        fold_rows,
        composite_equity,
        composite_fills,
        composite_metrics,
        is_bars,
        oos_bars,
        embargo_bars,
        cash,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::backtest::RunReport;

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
