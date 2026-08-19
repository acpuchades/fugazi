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
//! For each grid point we drive `crate::backtest::evaluate` and record its
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
use crate::spec::SingleStrategySpec;
use crate::types::Symbol;

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
/// The CLI's `run` wraps it with argument marshaling + CSV write +
/// console output.
///
/// `evaluate_row` owns everything strategy-specific — the base YAML
/// value, the atom / snapshot stream(s), cost config, and the choice of
/// whole-run vs windowed reduction. That closure is the seam Single /
/// Basket / Multi share — the sweep loop itself is strategy-type-agnostic.
/// `windowed` mirrors the closure's mode (used only to shape column
/// headers and DSR aggregation).
#[allow(clippy::too_many_arguments)]
pub fn optimize<F>(
    subgrids: Vec<Subgrid>,
    windowed: Option<usize>,
    metric_names: &[String],
    best_by: Option<&str>,
    risk_aversion: Real,
    smooth: Option<&Smoothing>,
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
    // Smoothing averages the `--best-by` ranking key over a neighbourhood;
    // without a key to rank by there is nothing to average. (Enforced by clap
    // for the CLI; this catches the library and Python callers.)
    if smooth.is_some() && best_by.is_none() {
        bail!("--smooth needs --best-by: there is no ranking key to average over the neighbourhood");
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
                    smoothed: None,
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
        smoothed: None,
    });
    rows.extend(remaining);

    // Sort by --best-by, direction-aware; None cells sort last regardless.
    //
    // `rows[i]` still corresponds to `plan[i]` at this point — subgrid-major,
    // then combo order — which is exactly the layout `smooth_keys` needs to
    // read a subgrid's lattice out of a contiguous slice. So smoothing has to
    // happen *here*, between the rejoin and the sort that destroys it.
    let smoothing = smooth.cloned();
    let mut plateau: Option<usize> = None;
    let mut smooth_scales: Option<Vec<(String, AxisScale)>> = None;
    if let Some((_, ref path, direction)) = best_by {
        let keys: Vec<Option<Real>> = rows
            .iter()
            .map(|r| ranking_value(&r.eval, path, direction, risk_aversion))
            .collect();
        match smooth {
            // Rank by the neighbourhood average instead of the point estimate.
            // `-k` composes for free: it is already folded into `keys`, so what
            // gets smoothed is the risk-adjusted key, not the raw mean.
            Some(cfg) => {
                let smoothed = smooth_keys(&subgrids, &keys, cfg)?;
                smooth_scales = Some(resolved_axis_scales(&subgrids, cfg)?);
                // Measure the plateau here, while the vector is still in
                // lattice order — the sort below is what destroys that.
                plateau = Some(plateau_size(
                    &subgrids,
                    &smoothed,
                    direction,
                    PLATEAU_TOLERANCE,
                    &cfg.scales,
                ));
                let smooth_keys_vec: Vec<Option<Real>> = smoothed.iter().map(|s| s.value).collect();
                for (row, key) in rows.iter_mut().zip(smoothed) {
                    row.smoothed = Some(key);
                }
                sort_by_keys(&mut rows, &smooth_keys_vec, direction);
            }
            None => sort_by_keys(&mut rows, &keys, direction),
        }
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
        smoothing,
        smooth_scales,
        plateau,
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

/// Substitute a params table into the base strategy value, then typed-parse as
/// whichever shape `kind` names — the one builder every driver needs, replacing
/// a five-arm match at each call site.
pub fn build_any_spec(
    kind: crate::spec::input::StrategyKind,
    base: &Value,
    params: &HashMap<String, Value>,
) -> Result<crate::spec::StrategySpec> {
    use crate::spec::StrategySpec as S;
    use crate::spec::input::StrategyKind as K;
    Ok(match kind {
        K::Single => S::Single(Box::new(build_typed(base, params)?)),
        K::Pairs => S::Pairs(Box::new(build_typed(base, params)?)),
        K::Basket => S::Basket(Box::new(build_typed(base, params)?)),
        K::Multi => S::Multi(Box::new(build_typed(base, params)?)),
        K::Portfolio => S::Portfolio(Box::new(build_typed(base, params)?)),
    })
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
///
/// **`combos` is a mixed-radix enumeration over `axes` with the last axis
/// varying fastest** — that is [`cartesian`]'s contract, and every construction
/// site pairs name-sorted [`split_axes`] output with `cartesian` of those same
/// axes. So a combo index is a lattice coordinate: digit `j` of `ci` is
/// `(ci / strides()[j]) % axis_lens()[j]`, and stepping one place along axis `j`
/// is `ci ± strides()[j]`. [`smooth_keys`] and [`plateau_size`] rely on this;
/// `subgrid_index_space_is_mixed_radix` pins it.
pub struct Subgrid {
    pub fixed: HashMap<String, Value>,
    pub axes: Vec<Axis>,
    pub combos: Vec<Vec<Value>>,
}

impl Subgrid {
    pub fn points(&self) -> usize {
        self.combos.len()
    }

    /// Point count along each axis, in axis order — the lattice's radices.
    pub fn axis_lens(&self) -> Vec<usize> {
        self.axes.iter().map(|(_, values)| values.len()).collect()
    }

    /// Combo-index stride per axis: `strides[j] = Π axis_lens()[j+1..]`, so the
    /// last axis has stride 1 (it varies fastest).
    pub fn strides(&self) -> Vec<usize> {
        let lens = self.axis_lens();
        let mut strides = vec![1usize; lens.len()];
        for j in (0..lens.len().saturating_sub(1)).rev() {
            strides[j] = strides[j + 1] * lens[j + 1];
        }
        strides
    }

    /// Lattice coordinate of combo `ci` — its position along each axis.
    pub fn digits(&self, ci: usize) -> Vec<usize> {
        let lens = self.axis_lens();
        self.strides()
            .iter()
            .zip(&lens)
            .map(|(stride, len)| (ci / stride) % len)
            .collect()
    }
}

/// One row of the grid, sparse across the union of every subgrid's axis
/// columns. `values[i]` is the value for `Sweep::union_columns[i]` — `None`
/// when this row's subgrid doesn't reference that name (the CSV writes the
/// empty cell; the best block skips it).
pub struct Row {
    pub values: Vec<Option<Value>>,
    pub eval: Evaluation,
    /// The `--smooth` neighbourhood average of this row's ranking key, or
    /// `None` when smoothing didn't run at all. Populated *before* the sort, so
    /// it rides the permutation with its row. See [`smooth_keys`].
    pub smoothed: Option<SmoothedKey>,
}

/// Rows and metadata produced by [`optimize`], ready for the CLI to write out.
/// `rows` is sorted by `best_by`'s ranking value when `best_by` is `Some` — or
/// by its `--smooth` neighbourhood average when `smoothing` is also set —
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
    /// The `--smooth` configuration that produced each [`Row::smoothed`], or
    /// `None` when smoothing didn't run. The CSV writer keys the two extra
    /// columns off this; the console block echoes the kernel.
    pub smoothing: Option<Smoothing>,
    /// Under `--smooth`, the [`AxisScale`] each smoothed axis resolved to —
    /// name-sorted and deduped across subgrids, so the console can say which
    /// scale it measured on rather than leaving it implicit. `None` when
    /// smoothing didn't run. See [`resolved_axis_scales`].
    pub smooth_scales: Option<Vec<(String, AxisScale)>>,
    /// Under `--smooth`, the size of the largest connected region of grid
    /// points within [`PLATEAU_TOLERANCE`] of the best smoothed value —
    /// measured in lattice space, before `rows` was sorted. The grid's shape is
    /// the result; its maximum is not, and a one-cell plateau under a wide
    /// kernel says the peak is an artifact of this sample.
    pub plateau: Option<usize>,
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
    build_typed(base, params)
}

/// Substitute a params table into the base strategy value, then typed-parse as
/// `T`.
///
/// This replaces six byte-identical two-line functions that differed only in
/// return type (`build_pairs_spec`, `build_basket_spec`, `build_multi_spec`,
/// `build_portfolio_spec`, `build_strategy_ref` and `build_spec`'s body), four
/// of which were `pub` and called exactly once — from `build_any_spec`, one
/// line below.
pub fn build_typed<T: serde::de::DeserializeOwned>(
    base: &Value,
    params: &HashMap<String, Value>,
) -> Result<T> {
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
    sort_by_keys(rows, &keys, direction);
}

/// Order two ranking keys under `direction` — better first, `None` last
/// regardless. The single definition of "better", shared by [`sort_by_keys`]
/// and [`rank_positions`] so a smoothed ordering and a raw ordering can never
/// disagree about direction.
fn compare_keys(a: Option<Real>, b: Option<Real>, direction: Direction) -> std::cmp::Ordering {
    match (a, b) {
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
}

/// Sort `rows` by a caller-supplied key vector (parallel to `rows`), best
/// first, `None` last. Split out of [`sort_by_metric`] so a `--smooth`
/// neighbourhood average sorts through the identical permutation machinery
/// rather than a second comparator that could drift from it.
pub fn sort_by_keys(rows: &mut Vec<Row>, keys: &[Option<Real>], direction: Direction) {
    assert_eq!(keys.len(), rows.len(), "sort_by_keys: key vector must be parallel to rows");
    let mut order: Vec<usize> = (0..rows.len()).collect();
    order.sort_by(|&i, &j| compare_keys(keys[i], keys[j], direction));
    // Apply the permutation in-place with an `Option` scratch buffer — cheap
    // and avoids cloning the (~50-field) Metrics documents held in each Row.
    let mut slots: Vec<Option<Row>> = std::mem::take(rows).into_iter().map(Some).collect();
    for i in order {
        rows.push(slots[i].take().expect("permutation visits each row exactly once"));
    }
}

/// Index of the best key under `direction`, or `None` when every key is
/// `None`.
///
/// **On an exact tie the later grid point wins.** That is `Iterator::max_by`'s
/// convention, which is what the walk-forward fold winner was selected with
/// before this was factored out, and every existing fold table was built
/// against it — so it is pinned here rather than left to whichever iterator
/// adapter happens to be in the call site.
pub fn argbest(keys: &[Option<Real>], direction: Direction) -> Option<usize> {
    keys.iter()
        .enumerate()
        .filter_map(|(i, k)| k.map(|k| (i, k)))
        .fold(None::<(usize, Real)>, |acc, (i, k)| match acc {
            Some((bi, bk)) if compare_keys(Some(k), Some(bk), direction) == std::cmp::Ordering::Greater => {
                Some((bi, bk))
            }
            _ => Some((i, k)),
        })
        .map(|(i, _)| i)
}

/// The 1-based rank of every key under `direction`, parallel to `keys`. Rank 1
/// is the best; `None` keys rank last. Lets the console report the gap between
/// the raw argmax and the smoothed ordering — the diagnostic `--smooth` exists
/// to surface — without a second copy of the direction rules.
pub fn rank_positions(keys: &[Option<Real>], direction: Direction) -> Vec<usize> {
    let mut order: Vec<usize> = (0..keys.len()).collect();
    order.sort_by(|&i, &j| compare_keys(keys[i], keys[j], direction));
    let mut ranks = vec![0usize; keys.len()];
    for (pos, &i) in order.iter().enumerate() {
        ranks[i] = pos + 1;
    }
    ranks
}

/// Position of a metric column inside the `metrics::flatten` output — the
/// output ordering is fixed and shared across every [`Metrics`](crate::spec::metrics::Metrics) document, so a
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


// ---------------------------------------------------------------------------
// Neighbourhood smoothing
// ---------------------------------------------------------------------------

/// Which scale an axis' distances are measured on, once resolved.
///
/// [`smooth_keys`] measures neighbour distance in **parameter units, divided by
/// the axis' own characteristic spacing** — so a radius of `1` means "one
/// typical grid step on this axis" whatever the axis means. The transform
/// applied before that division is this enum.
///
/// Chosen per axis by [`SmoothScales`]: automatically by default, or pinned by
/// `--smooth-scale`. The choice only ever *matters* on an irregularly spaced
/// axis — on a regular one every scale collapses to the same integer stencil.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum AxisScale {
    /// Distance is `|vᵢ − vⱼ| / step`, `step` being the median gap between
    /// successive values. The default for an additive grid.
    Linear,
    /// Distance is `|ln vᵢ − ln vⱼ| / step` in log space. Picked automatically
    /// for a grid the user laid out multiplicatively (`[10,20,50,100,200]`),
    /// where `100→200` really is about as near as `10→20`. Only admissible when
    /// every value on the axis is strictly positive.
    Log,
    /// Distance is `|i − j|` between **declared positions** — the pre-0.65
    /// behaviour, and what `--smooth-scale=index` restores. Depends on how the
    /// list was typed, which is why it is no longer the default.
    Index,
}

impl std::fmt::Display for AxisScale {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(match self {
            AxisScale::Linear => "linear",
            AxisScale::Log => "log",
            AxisScale::Index => "index",
        })
    }
}

impl std::str::FromStr for AxisScale {
    type Err = String;

    fn from_str(s: &str) -> std::result::Result<Self, Self::Err> {
        match s.trim() {
            "linear" => Ok(AxisScale::Linear),
            "log" => Ok(AxisScale::Log),
            "index" => Ok(AxisScale::Index),
            other => Err(format!(
                "unknown smoothing scale `{other}` — expected `linear`, `log` or `index`"
            )),
        }
    }
}

/// `--smooth-scale`: which [`AxisScale`] each axis is measured on.
///
/// Grammar is a `,`-separated list of terms, each either a bare scale name (the
/// grid-wide default, at most one) or `NAME:SCALE` (that one axis). So
/// `--smooth-scale=index` restores the pre-0.65 index-space behaviour
/// wholesale, `--smooth-scale=PERIOD:log` overrides one axis and leaves the
/// rest automatic, and the two compose: `--smooth-scale=linear,PERIOD:log`.
///
/// The default — no term for an axis and no bare default — is **automatic**:
/// `choose_axis_scale` picks whichever transform makes that axis' spacings
/// most nearly uniform. A regular axis is a fixed point of that test, so the
/// heuristic only ever fires on an irregular one.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct SmoothScales {
    default: Option<AxisScale>,
    per_axis: HashMap<String, AxisScale>,
}

impl SmoothScales {
    /// No pins at all — every axis picks its own scale.
    pub fn auto() -> Self {
        Self::default()
    }

    /// Pin every axis to one scale.
    pub fn all(scale: AxisScale) -> Self {
        SmoothScales { default: Some(scale), per_axis: HashMap::new() }
    }

    /// Pin one axis by name, leaving the rest as they were.
    pub fn with_axis(mut self, name: impl Into<String>, scale: AxisScale) -> Self {
        self.per_axis.insert(name.into(), scale);
        self
    }

    /// The scale pinned for `name`, or `None` when it is left automatic.
    /// A per-axis term wins over the bare default.
    pub fn pinned(&self, name: &str) -> Option<AxisScale> {
        self.per_axis.get(name).copied().or(self.default)
    }

    /// True when nothing at all was pinned — the console echo stays quiet
    /// about a flag the user never passed.
    pub fn is_auto(&self) -> bool {
        self.default.is_none() && self.per_axis.is_empty()
    }
}

impl std::str::FromStr for SmoothScales {
    type Err = String;

    fn from_str(s: &str) -> std::result::Result<Self, Self::Err> {
        let mut out = SmoothScales::default();
        for term in s.split(',') {
            let term = term.trim();
            if term.is_empty() {
                continue;
            }
            match term.rsplit_once(':') {
                Some((name, scale)) => {
                    let name = name.trim();
                    if name.is_empty() {
                        return Err(format!("`{term}` names no axis — write `NAME:SCALE`"));
                    }
                    out.per_axis.insert(name.to_string(), scale.parse()?);
                }
                None => {
                    let scale: AxisScale = term.parse()?;
                    if out.default.is_some_and(|d| d != scale) {
                        return Err(format!(
                            "conflicting grid-wide scales in `{s}` — pass at most one bare \
                             scale name, and use `NAME:SCALE` for per-axis overrides"
                        ));
                    }
                    out.default = Some(scale);
                }
            }
        }
        if out.default.is_none() && out.per_axis.is_empty() {
            return Err(format!(
                "`{s}` sets no scale — expected `linear`, `log`, `index`, or `NAME:SCALE` terms"
            ));
        }
        Ok(out)
    }
}

impl std::fmt::Display for SmoothScales {
    /// Round-trips through [`FromStr`](std::str::FromStr): the bare default
    /// first, then per-axis terms name-sorted.
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let mut terms: Vec<String> = self.default.iter().map(|d| d.to_string()).collect();
        let mut pins: Vec<(&String, &AxisScale)> = self.per_axis.iter().collect();
        pins.sort_by(|a, b| a.0.cmp(b.0));
        terms.extend(pins.into_iter().map(|(n, s)| format!("{n}:{s}")));
        f.write_str(&terms.join(","))
    }
}

/// How neighbourhood weight falls off with distance along an axis.
///
/// Distance is in **units of the axis' own characteristic spacing** — see
/// [`smooth_keys`]. On a regularly spaced axis that is exactly `|i − j|`
/// between declared positions, which is what `R` and `S` have always meant;
/// on an irregular one it is the parameter gap divided by the median gap. All
/// three kernels are *separable*: the weight of an offset vector is the product
/// of its per-axis weights, which makes [`SmoothKernel::Box`]'s tensor product
/// exactly the Chebyshev ball of radius `R`. Separability is also why the axes
/// never need a *common* scale — only a scale internal to each.
///
/// Parsed from the `--smooth` grammar via [`FromStr`](std::str::FromStr): `box:R`, `triangle:R`,
/// `gaussian:S`. A bare kernel name takes the default parameter (`box` →
/// `box:1`, the Moore neighbourhood).
#[derive(Clone, Copy, Debug, PartialEq)]
pub enum SmoothKernel {
    /// Uniform over the Chebyshev ball of radius `R`. `box:0` is the identity.
    Box { radius: usize },
    /// `Π (1 − |dⱼ|/(R+1))` — linear falloff, zero outside radius `R`.
    Triangle { radius: usize },
    /// `Π exp(−dⱼ²/2S²)`, truncated at `⌈3S⌉` grid steps per axis. `S` is a
    /// bandwidth in **grid steps**, not in the axis' parameter units.
    Gaussian { bandwidth: Real },
}

/// Slack allowed when testing a real-valued distance against a kernel's reach.
/// A distance derived from float parameter values (`1.0..4.0:0.5` accumulates)
/// can land a few ULP outside an exact radius; without this a neighbour that
/// *is* one step away would silently drop out. Far below any real grid spacing,
/// so it never admits a genuine non-neighbour.
const DISTANCE_TOLERANCE: Real = 1e-9;

impl SmoothKernel {
    /// Per-axis stencil radius: the largest distance that can carry weight.
    pub fn radius(&self) -> usize {
        match self {
            SmoothKernel::Box { radius } | SmoothKernel::Triangle { radius } => *radius,
            // Truncate at 3σ — beyond it the weight is < 1.2% of the peak and
            // the stencil cost grows linearly for nothing.
            SmoothKernel::Gaussian { bandwidth } => (3.0 * bandwidth).ceil() as usize,
        }
    }

    /// The one-dimensional weight at real-valued distance `d`, in units of the
    /// axis' characteristic spacing. Zero beyond [`radius`](Self::radius);
    /// exactly `1.0` at `d = 0` for every kernel.
    ///
    /// Bit-identical to the old integer form when `d` is a whole number, which
    /// is what keeps a regularly spaced grid byte-identical to pre-0.65 output.
    pub fn weight_at(&self, d: Real) -> Real {
        let a = d.abs();
        let reach = self.radius() as Real + DISTANCE_TOLERANCE;
        match self {
            SmoothKernel::Box { .. } => {
                if a <= reach { 1.0 } else { 0.0 }
            }
            SmoothKernel::Triangle { radius } => {
                if a <= reach { 1.0 - a / (*radius as Real + 1.0) } else { 0.0 }
            }
            SmoothKernel::Gaussian { bandwidth } => {
                if a <= reach { (-(a * a) / (2.0 * bandwidth * bandwidth)).exp() } else { 0.0 }
            }
        }
    }

    /// [`weight_at`](Self::weight_at) at an integer lattice offset — the form
    /// the boundary-ignoring reference weight is summed over.
    pub fn weight_1d(&self, d: i64) -> Real {
        self.weight_at(d.unsigned_abs() as Real)
    }

    /// `Σ_{d=−R..R} w(d)` — the weight one axis contributes to a point sitting
    /// in the interior of a *regular* axis of that axis' own spacing.
    /// Deliberately boundary- and density-ignoring: it is the denominator
    /// [`SmoothedKey::support`] is expressed against, so an interior point on a
    /// regular grid scores exactly `1.0`.
    fn ideal_axis_weight(&self) -> Real {
        let r = self.radius() as i64;
        (-r..=r).map(|d| self.weight_1d(d)).sum()
    }
}

impl std::str::FromStr for SmoothKernel {
    type Err = String;

    fn from_str(s: &str) -> std::result::Result<Self, Self::Err> {
        let (name, arg) = match s.split_once(':') {
            Some((n, a)) => (n.trim(), Some(a.trim())),
            None => (s.trim(), None),
        };
        let radius = |default: usize| -> std::result::Result<usize, String> {
            match arg {
                None => Ok(default),
                Some(a) => a
                    .parse::<usize>()
                    .map_err(|_| format!("`{name}` takes a whole-number radius in grid steps, got `{a}`")),
            }
        };
        match name {
            "box" => Ok(SmoothKernel::Box { radius: radius(1)? }),
            "triangle" => Ok(SmoothKernel::Triangle { radius: radius(1)? }),
            "gaussian" => {
                let bandwidth = match arg {
                    None => 1.0,
                    Some(a) => a
                        .parse::<Real>()
                        .map_err(|_| format!("`gaussian` takes a bandwidth in grid steps, got `{a}`"))?,
                };
                if !(bandwidth > 0.0 && bandwidth.is_finite()) {
                    return Err(format!("`gaussian` bandwidth must be > 0 (got {bandwidth})"));
                }
                Ok(SmoothKernel::Gaussian { bandwidth })
            }
            other => Err(format!(
                "unknown smoothing kernel `{other}` — expected `box:R`, `triangle:R` or `gaussian:S` \
                 (R a radius in grid steps, S a bandwidth in grid steps)"
            )),
        }
    }
}

impl std::fmt::Display for SmoothKernel {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            SmoothKernel::Box { radius } => write!(f, "box:{radius}"),
            SmoothKernel::Triangle { radius } => write!(f, "triangle:{radius}"),
            SmoothKernel::Gaussian { bandwidth } => write!(f, "gaussian:{bandwidth}"),
        }
    }
}

/// A configured `--smooth` pass: the kernel, the `--smooth-min-support` floor
/// below which a row's smoothed value is discarded, and the `--smooth-scale`
/// pins.
#[derive(Clone, Debug, PartialEq)]
pub struct Smoothing {
    pub kernel: SmoothKernel,
    /// Minimum realized [`support`](SmoothedKey::support), in `0..=1`. `0.0`
    /// (the default) keeps every row.
    pub min_support: Real,
    /// Per-axis distance scales. [`SmoothScales::auto`] by default.
    pub scales: SmoothScales,
}

impl Smoothing {
    /// Validated constructor. `min_support` outside `0..=1` is refused — above
    /// `1` would discard every row including the fully interior ones, which is
    /// never what a caller means. Scales default to automatic; add pins with
    /// [`with_scales`](Self::with_scales).
    pub fn new(kernel: SmoothKernel, min_support: Real) -> Result<Self> {
        if !(0.0..=1.0).contains(&min_support) {
            bail!("--smooth-min-support must be in 0..=1 (got {min_support})");
        }
        Ok(Smoothing { kernel, min_support, scales: SmoothScales::auto() })
    }

    /// Pin the per-axis distance scales (`--smooth-scale`).
    pub fn with_scales(mut self, scales: SmoothScales) -> Self {
        self.scales = scales;
        self
    }
}

/// One grid point's smoothed ranking key and the neighbourhood evidence
/// behind it.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct SmoothedKey {
    /// The kernel-weighted mean of the ranking keys in this point's
    /// neighbourhood, in the metric's **native orientation** (so it is directly
    /// comparable to the raw metric column). `None` when the point's own raw
    /// key is `None`, or when [`support`](Self::support) fell below the
    /// configured floor — either way it sorts last, exactly like a `None`
    /// metric.
    pub value: Option<Real>,
    /// The weight actually found, as a fraction of the weight a point in the
    /// interior of a *regular* axis of this axis' own median spacing would
    /// find — `Π_j Σ_{d=−R..R} w(d)` over the smoothed axes. `1.0` = as much
    /// evidence as a regular grid of that spacing would give. Always reported,
    /// even when `value` was discarded — that is the diagnostic.
    ///
    /// **Not clamped.** A stretch of an irregular axis denser than its own
    /// median packs more neighbours inside the kernel's reach and reads above
    /// `1.0`; that is the measured quantity, and squeezing it into `0..=1`
    /// would report "exactly fully supported" for two different situations.
    /// [`Smoothing::min_support`] is a floor, so nothing downstream cares.
    ///
    /// See [`smooth_keys`] for why the denominator stays kernel-only rather
    /// than following the local grid density.
    pub support: Real,
}

/// True iff this axis takes part in smoothing: every value is a JSON number
/// *and* there are at least two of them.
///
/// Both exclusions partition the lattice rather than smoothing across it, for
/// the same reason. `SL_MODE=[none,atr,chandelier]` has no ordering, so
/// distance along it is meaningless. A **degenerate** axis — `SLOW=[20]`, or a
/// name one stacked subgrid pins while the others sweep it — is not a swept
/// dimension at all: it carries no neighbourhood information in either
/// direction, so it neither widens a neighbourhood nor belongs in the support
/// denominator. (An axis of length *two* is different in kind: it is genuinely
/// swept, it just has no interior point — see [`smooth_keys`]'s error.)
fn axis_is_numeric(axis: &Axis) -> bool {
    axis.1.len() > 1 && axis.1.iter().all(Value::is_number)
}

/// Relative slack for "these gaps are all the same". Wide enough to absorb the
/// drift `start..end:step` accumulates over a long float range, far tighter
/// than any spacing a user would call irregular.
const REGULAR_SPACING_TOLERANCE: Real = 1e-9;

/// How much flatter log-spacing has to look before [`choose_axis_scale`] picks
/// it. A clear margin, not a hair: ties and near-ties stay linear, which is the
/// scale a reader assumes when they don't think about it.
const LOG_SCALE_MARGIN: Real = 0.5;

/// Coefficient of variation of a sample — `σ/|μ|`, the scale-free measure of
/// "how unequal are these gaps". `None` when the mean is zero (no scale to be
/// free of) or there are fewer than two samples.
fn coefficient_of_variation(xs: &[Real]) -> Option<Real> {
    if xs.len() < 2 {
        return None;
    }
    let n = xs.len() as Real;
    let mean = xs.iter().sum::<Real>() / n;
    if mean == 0.0 || !mean.is_finite() {
        return None;
    }
    let var = xs.iter().map(|x| (x - mean) * (x - mean)).sum::<Real>() / n;
    Some(var.sqrt() / mean.abs())
}

/// Successive gaps of an already-sorted value list.
fn successive_gaps(sorted: &[Real]) -> Vec<Real> {
    sorted.windows(2).map(|w| w[1] - w[0]).collect()
}

/// Pick [`AxisScale::Linear`] or [`AxisScale::Log`] for an axis, by testing
/// which transform makes its spacings most nearly uniform.
///
/// A user who writes `PERIOD=[10,20,50,100,200]` chose a roughly geometric grid
/// deliberately, and in strategy terms `100→200` is about as near as `10→20`;
/// plain linear distance would call the first pair 10× farther apart. So
/// compare the coefficient of variation of the successive gaps of `v` against
/// those of `ln v`, and take log when it wins by [`LOG_SCALE_MARGIN`].
///
/// **A regular axis is a fixed point of the test** (its linear CV is already
/// zero, which nothing can beat), so this only ever fires on an irregular one.
/// **Log is only admissible when every value is strictly positive** — an axis
/// containing `0` or a negative falls back to linear rather than being
/// silently log-transformed.
fn choose_axis_scale(sorted: &[Real]) -> AxisScale {
    let linear = successive_gaps(sorted);
    let Some(cv_linear) = coefficient_of_variation(&linear) else {
        return AxisScale::Linear;
    };
    if !sorted.iter().all(|v| *v > 0.0) {
        return AxisScale::Linear;
    }
    let logs: Vec<Real> = sorted.iter().map(|v| v.ln()).collect();
    let Some(cv_log) = coefficient_of_variation(&successive_gaps(&logs)) else {
        return AxisScale::Linear;
    };
    if cv_log <= LOG_SCALE_MARGIN * cv_linear { AxisScale::Log } else { AxisScale::Linear }
}

/// One smoothed axis, reduced to what the walk needs: the resolved scale and,
/// per declared position, its in-reach neighbours with their 1-D weights.
///
/// The neighbour lists are the whole cost guarantee. They are built once per
/// axis by a sliding window over the *sorted* values — never a search — so the
/// per-point stencil is just the Cartesian product of `neighbours[digit_j]`
/// and the walk stays `O(N · |neighbourhood|)`.
struct AxisGeometry {
    scale: AxisScale,
    /// `neighbours[p]` = `(declared position, weight)` for every position
    /// within the kernel's reach of declared position `p`, **in accumulation
    /// order**. See [`axis_geometry`] for why that order is what it is.
    neighbours: Vec<Vec<(usize, Real)>>,
}

/// Resolve one axis' scale and build its neighbour lists.
///
/// 1. **Scale.** A `--smooth-scale` pin wins; otherwise [`choose_axis_scale`].
///    An explicit `log` on an axis with a non-positive value is an error, not a
///    silent fallback — the user asked for something that has no meaning.
/// 2. **Coordinates.** The transformed values divided by the axis' **median**
///    successive gap. Median, not minimum: `1.0..4.0:0.5` accumulates float
///    error, and a min-gap denominator would turn one `0.4999999` into a
///    phantom scale for the whole axis.
/// 3. **Regular fast path.** When every successive gap is equal within
///    [`REGULAR_SPACING_TOLERANCE`] the coordinates are replaced by exact
///    integer ranks, so `|vᵢ − vⱼ| / step` is computed as `|i − j|` with no
///    float division at all. This is what makes a regular grid *byte*-identical
///    to pre-0.65 output rather than identical to 1e-12 — and it covers every
///    `start..end:step` range and every evenly spaced list.
///
/// **Accumulation order.** Neighbours are listed in ascending coordinate order,
/// always — never in declared order. f64 addition is not associative, so the
/// summation sequence is part of the result, and making it a function of the
/// axis' *values* is what lets two declarations of the same value set agree bit
/// for bit rather than merely to the last ULP. There is no exception for a
/// descending declaration: "how you typed the list cannot matter" is the rule
/// the whole scale change exists to establish, and a rule with a carve-out is
/// not one.
fn axis_geometry(
    axis: &Axis,
    kernel: &SmoothKernel,
    pinned: Option<AxisScale>,
) -> Result<AxisGeometry> {
    let raw: Vec<Real> = axis
        .1
        .iter()
        .map(|v| v.as_f64().expect("axis_is_numeric guarantees every value is a JSON number"))
        .collect();
    let len = raw.len();

    // The axis' values in ascending order — the only thing the scale heuristic
    // may look at, since declaration order is exactly what must not matter.
    let mut sorted: Vec<Real> = raw.clone();
    sorted.sort_by(|a, b| a.partial_cmp(b).unwrap_or(std::cmp::Ordering::Equal));

    let scale = match pinned {
        Some(AxisScale::Log) if !raw.iter().all(|v| *v > 0.0) => bail!(
            "--smooth-scale pins axis `{}` to `log`, but it contains a non-positive value — \
             log distance is undefined there; use `linear` or `index`",
            axis.0
        ),
        Some(s) => s,
        None => choose_axis_scale(&sorted),
    };

    // Transformed coordinates in *declared* position order. `Index` is the
    // pre-0.65 spelling: the declared position itself, already regular.
    let transformed: Vec<Real> = match scale {
        AxisScale::Index => (0..len).map(|p| p as Real).collect(),
        AxisScale::Linear => raw.clone(),
        AxisScale::Log => raw.iter().map(|v| v.ln()).collect(),
    };
    let mut sorted_t: Vec<usize> = (0..len).collect();
    sorted_t.sort_by(|&a, &b| {
        transformed[a].partial_cmp(&transformed[b]).unwrap_or(std::cmp::Ordering::Equal)
    });
    let sorted_vals: Vec<Real> = sorted_t.iter().map(|&p| transformed[p]).collect();

    let gaps = successive_gaps(&sorted_vals);
    let mut sorted_gaps = gaps.clone();
    sorted_gaps.sort_by(|a, b| a.partial_cmp(b).unwrap_or(std::cmp::Ordering::Equal));
    let median = crate::indicators::stats::quantile_of_sorted(&sorted_gaps, 0.5);
    // An axis of one gap (or none) is trivially regular; so is one whose gaps
    // are all equal, and so is the degenerate all-same-value axis (median 0),
    // which has no spacing to normalize by and falls back to rank distance.
    let regular = median <= 0.0
        || gaps.iter().all(|g| (g - median).abs() <= REGULAR_SPACING_TOLERANCE * median.abs());

    // Coordinates the kernel actually sees. Regular → exact integer ranks.
    let mut coord = vec![0.0; len];
    for (rank, &p) in sorted_t.iter().enumerate() {
        coord[p] = if regular { rank as Real } else { transformed[p] / median };
    }

    // Sliding window over the sorted order: both bounds advance monotonically,
    // so building every neighbour list costs one pass plus the neighbours
    // themselves — never a per-point search.
    let reach = kernel.radius() as Real + DISTANCE_TOLERANCE;
    let mut neighbours = vec![Vec::new(); len];
    let (mut lo, mut hi) = (0usize, 0usize);
    for (k, &p) in sorted_t.iter().enumerate() {
        let c = coord[p];
        while lo < k && (coord[sorted_t[lo]] - c).abs() > reach {
            lo += 1;
        }
        hi = hi.max(k);
        while hi + 1 < len && (coord[sorted_t[hi + 1]] - c).abs() <= reach {
            hi += 1;
        }
        let list = &mut neighbours[p];
        list.reserve(hi + 1 - lo);
        for &q in &sorted_t[lo..=hi] {
            let w = kernel.weight_at(coord[q] - c);
            if w != 0.0 {
                list.push((q, w));
            }
        }
    }

    Ok(AxisGeometry { scale, neighbours })
}

/// Kernel-weighted neighbourhood average of a grid's ranking keys.
///
/// `keys` must be in `plan` order — subgrid-major, then each subgrid's combo
/// order — which is exactly the order [`optimize`] builds its rows in *before*
/// sorting, and the order [`walkforward`]'s per-fold key vector is in. The
/// return value is parallel to `keys`.
///
/// **Value space, per-axis normalized.** Distance along an axis is
/// `|t(vᵢ) − t(vⱼ)| / step`, where `t` is the axis' [`AxisScale`] transform and
/// `step` is the median gap between its successive values. A radius of `1`
/// therefore means "one typical grid step on this axis" — the thing index space
/// was reaching for, and got right only when the grid was regular. Because the
/// kernels are **separable** (weight is `Π_j w(dⱼ)`, a product of per-axis
/// weights) the axes never need a metric in common: each needs only a scale
/// internal to itself, and its own spacing is one. On a regularly spaced axis
/// `|vᵢ − vⱼ| / step` *is* `|i − j|`, so every `start..end:step` range and every
/// evenly spaced list behaves exactly as it did before 0.65; the two only
/// diverge on an irregular hand-written list, which is the case index space got
/// wrong. `--smooth-scale=index` restores the old measure wholesale.
///
/// **Order-independence falls out.** Value distance cannot depend on
/// declaration order, so `FAST=[3,9,4,8,5,7,6]` and `FAST=[3,4,5,6,7,8,9]`
/// smooth identically — there is no monotonicity rule for the user to remember
/// and no error to hit.
///
/// **Non-numeric and degenerate axes partition, they do not smooth.** See
/// `axis_is_numeric`: an axis whose values aren't all numbers, or that has
/// only one value, has its offset pinned to zero, so each combination of its
/// levels forms an independent lattice for free.
///
/// **Each subgrid is its own lattice.** `--grid` is repeatable and the point
/// sets are a disjoint union; neighbours are computed within a subgrid's own
/// `axes`/`combos`, never across the sparse `union_columns` projection. Two
/// rows from different subgrids are never neighbours even if their union-column
/// cells happen to be adjacent — which also means a point named by two
/// overlapping `--grid` flags is evaluated twice and gets two independent
/// smoothed values, one per lattice.
///
/// **Edges renormalize (Nadaraya–Watson), they do not pad or reflect.** A
/// boundary point divides by the weight it actually found, and reports that
/// weight as [`SmoothedKey::support`] — because grid maxima like to sit on
/// edges, and an edge estimate resting on a quarter of the samples should be
/// visible rather than silently equal-footed. A neighbour whose raw key is
/// `None` contributes no weight and reduces support without dragging the mean
/// toward zero.
///
/// **What `support` is measured against.** The denominator stays
/// `Π_j Σ_{d=−R..R} w(d)` — the weight a point in the interior of a *regular*
/// axis of that axis' own median spacing would find. It is a property of the
/// kernel alone, so `1.0` keeps meaning one fixed, reachable thing and
/// `--smooth-min-support 1.0` keeps meaning "fully supported". Two alternatives
/// were weighed and rejected: normalizing by the *best weight any position on
/// the axis achieves* makes `1.0` reachable on any grid, but it also erases the
/// "an axis shorter than the kernel's diameter has no interior point" error by
/// redefining its 2-point axis as fully supported; and comparing against the
/// continuous kernel mass under the local density turns `support` into a
/// density estimate whose `1.0` is not attainable in general, which makes
/// `--smooth-min-support 1.0` unusable as an input. The cost of the choice we
/// kept is that on an irregular axis `support` conflates "near an edge" with
/// "in a thin part of the grid" — both are genuinely less-supported estimates,
/// but they warrant different reactions, so the docs say so. A *denser*-than-
/// median pocket finds more weight than the reference and reads above `1.0`,
/// unclamped: it is the ratio that was measured, and `min_support` is a floor.
///
/// **Direction-agnostic.** Smoothing is the same averaging operation for
/// [`Direction::Descending`] and [`Direction::Ascending`]: [`ranking_value`]
/// has already folded `-k/--risk-aversion` in the correct direction, and
/// [`sort_by_keys`] owns the comparison. Do not special-case direction here.
///
/// Cost is `O(N · |neighbourhood|)` — the per-axis neighbour lists are built
/// once per subgrid by a sliding window over the sorted values, and the walk is
/// index arithmetic over their Cartesian product, never a search.
pub fn smooth_keys(
    subgrids: &[Subgrid],
    keys: &[Option<Real>],
    smoothing: &Smoothing,
) -> Result<Vec<SmoothedKey>> {
    let total: usize = subgrids.iter().map(Subgrid::points).sum();
    if keys.len() != total {
        bail!(
            "smooth_keys: got {} ranking keys for {total} grid points — the key vector must be \
             in `plan` order (subgrid-major, then combo order)",
            keys.len()
        );
    }

    let kernel = &smoothing.kernel;
    let mut out: Vec<SmoothedKey> = Vec::with_capacity(total);
    let mut base = 0usize;

    for sg in subgrids {
        let n = sg.points();
        let lens = sg.axis_lens();
        let strides = sg.strides();
        // Numeric, non-degenerate axes are the ones that smooth; every other
        // axis keeps its offset at zero and therefore partitions the lattice.
        let numeric: Vec<usize> =
            (0..sg.axes.len()).filter(|&j| axis_is_numeric(&sg.axes[j])).collect();
        let geoms: Vec<AxisGeometry> = numeric
            .iter()
            .map(|&j| axis_geometry(&sg.axes[j], kernel, smoothing.scales.pinned(&sg.axes[j].0)))
            .collect::<Result<_>>()?;

        // The reference weight an interior point on a regular grid finds —
        // deliberately independent of this grid's density and edges, since it
        // is what `support` is a fraction of.
        let ideal: Real = kernel.ideal_axis_weight().powi(numeric.len() as i32);

        let slice = &keys[base..base + n];
        let mut digits = vec![0usize; sg.axes.len()];
        // Scratch for the Cartesian walk over the per-axis neighbour lists:
        // this point's list per smoothed axis, plus one cursor into each.
        let mut lists: Vec<&[(usize, Real)]> = Vec::with_capacity(numeric.len());
        let mut cursor = vec![0usize; numeric.len()];
        for (ci, own) in slice.iter().enumerate() {
            for (j, digit) in digits.iter_mut().enumerate() {
                *digit = (ci / strides[j]) % lens[j];
            }
            let mut numerator = 0.0;
            let mut weight = 0.0;
            if numeric.is_empty() {
                // No smoothed axis at all: the point is its own neighbourhood.
                if let Some(v) = *own {
                    numerator = v;
                    weight = 1.0;
                }
            } else {
                lists.clear();
                lists.extend(
                    geoms.iter().enumerate().map(|(slot, g)| g.neighbours[digits[numeric[slot]]].as_slice()),
                );
                cursor.iter_mut().for_each(|c| *c = 0);
                'walk: loop {
                    // Fold the per-axis weights in axis order starting from
                    // 1.0 — the same association pre-0.65 built its stencil
                    // weights with, so a regular grid reproduces them bit for
                    // bit.
                    let mut w = 1.0;
                    let mut nci = ci;
                    for (slot, &j) in numeric.iter().enumerate() {
                        let (q, wq) = lists[slot][cursor[slot]];
                        w *= wq;
                        nci = nci + q * strides[j] - digits[j] * strides[j];
                    }
                    if let Some(v) = slice[nci] {
                        numerator += w * v;
                        weight += w;
                    }
                    // Odometer over the neighbour lists, last axis fastest —
                    // the lexicographic order the old stencil enumerated in.
                    let mut slot = numeric.len();
                    loop {
                        if slot == 0 {
                            break 'walk;
                        }
                        slot -= 1;
                        cursor[slot] += 1;
                        if cursor[slot] < lists[slot].len() {
                            break;
                        }
                        cursor[slot] = 0;
                    }
                }
            }
            // A row whose own raw key is `None` is `None` regardless of how
            // healthy its neighbourhood looks — smoothing reweights evidence,
            // it does not manufacture it.
            let support = if ideal > 0.0 { weight / ideal } else { 0.0 };
            let value = match own {
                None => None,
                // The epsilon keeps `--smooth-min-support 1.0` from rejecting a
                // genuinely interior point over a last-ULP division.
                Some(_) if support + 1e-12 < smoothing.min_support => None,
                Some(_) if weight > 0.0 => Some(numerator / weight),
                Some(_) => None,
            };
            out.push(SmoothedKey { value, support });
        }
        base += n;
    }

    // Silently returning all-`None` would hand the caller a grid whose "winner"
    // is just the first point in enumeration order, presented as a verdict.
    if smoothing.min_support > 0.0
        && keys.iter().any(Option::is_some)
        && !out.iter().any(|s| s.value.is_some())
    {
        let best = out.iter().map(|s| s.support).fold(0.0, Real::max);
        bail!(
            "--smooth-min-support {} discarded every grid point (best realized support was {best:.3}). \
             Lower it, shrink the kernel radius, or widen the grid — an axis shorter than the \
             kernel's diameter leaves no fully interior point, and a sparse stretch of an \
             irregular axis reaches less than a regular one of the same median spacing.",
            smoothing.min_support
        );
    }

    Ok(out)
}

/// The [`AxisScale`] each smoothed axis resolved to, name-sorted and deduped
/// across subgrids — what the console echoes so the chosen scale is never
/// implicit. A name that resolves differently in two subgrids (different value
/// sets under the same name) appears once per distinct scale.
///
/// Errors exactly where [`smooth_keys`] would, so the CLI can call it first and
/// report a bad `--smooth-scale` pin before the sweep runs.
pub fn resolved_axis_scales(
    subgrids: &[Subgrid],
    smoothing: &Smoothing,
) -> Result<Vec<(String, AxisScale)>> {
    let mut out: Vec<(String, AxisScale)> = Vec::new();
    for sg in subgrids {
        for axis in &sg.axes {
            if !axis_is_numeric(axis) {
                continue;
            }
            let geom = axis_geometry(axis, &smoothing.kernel, smoothing.scales.pinned(&axis.0))?;
            let entry = (axis.0.clone(), geom.scale);
            if !out.contains(&entry) {
                out.push(entry);
            }
        }
    }
    out.sort();
    Ok(out)
}

/// Band [`plateau_size`] is conventionally measured at: cells within 5% of the
/// best smoothed value. A readout, not a knob — one more `--smooth-*` spelling
/// would buy nothing.
pub const PLATEAU_TOLERANCE: Real = 0.05;

/// Size of the largest connected region of grid points whose smoothed value is
/// within `tol` (a fraction, e.g. `0.05`) of the best smoothed value.
///
/// Connectivity is the **next value up or down** a smoothed axis — `±1` in
/// sorted position, not within the kernel's bandwidth. The console prints this
/// as "N of M cells", so it has to stay a count of *adjacent cells*; measuring
/// it in bandwidth instead would make a sparse stretch of an irregular axis
/// report fewer cells for the same parameter width, which is not what the
/// sentence claims. On a regular axis the two coincide anyway, and sorted
/// position equals declared position. Non-numeric and degenerate axes, and
/// separate subgrids, bound a region exactly as they bound the smoothing
/// itself.
///
/// `scales` only matters for `--smooth-scale=index`, where "the next value up"
/// means the next *declared* position; every other scale is monotone in the
/// value, so they all order an axis the same way.
///
/// The band is measured against the *directed* key, so it means "no worse than"
/// under either [`Direction`]. Scaled by `|best|`, which is the caveat: a best
/// value near zero gives a near-zero band.
///
/// The grid's shape is the result; its maximum is not. A one-cell plateau under
/// a wide kernel says the peak is an artifact.
pub fn plateau_size(
    subgrids: &[Subgrid],
    smoothed: &[SmoothedKey],
    direction: Direction,
    tol: Real,
    scales: &SmoothScales,
) -> usize {
    let best = smoothed
        .iter()
        .filter_map(|s| s.value)
        .fold(None::<Real>, |acc, v| {
            Some(match (acc, direction) {
                (None, _) => v,
                (Some(b), Direction::Descending) => b.max(v),
                (Some(b), Direction::Ascending) => b.min(v),
            })
        });
    let Some(best) = best else { return 0 };
    let band = tol * best.abs();
    let in_band = |v: Option<Real>| match (v, direction) {
        (Some(v), Direction::Descending) => v >= best - band,
        (Some(v), Direction::Ascending) => v <= best + band,
        (None, _) => false,
    };

    let mut largest = 0usize;
    let mut base = 0usize;
    for sg in subgrids {
        let n = sg.points();
        let lens = sg.axis_lens();
        let strides = sg.strides();
        let numeric: Vec<usize> =
            (0..sg.axes.len()).filter(|&j| axis_is_numeric(&sg.axes[j])).collect();
        // Per smoothed axis: declared positions in ascending value order, and
        // each declared position's rank in that order.
        let steps: Vec<(Vec<usize>, Vec<usize>)> = numeric
            .iter()
            .map(|&j| {
                let axis = &sg.axes[j];
                let mut order: Vec<usize> = (0..axis.1.len()).collect();
                if scales.pinned(&axis.0) != Some(AxisScale::Index) {
                    let vals: Vec<Real> =
                        axis.1.iter().map(|v| v.as_f64().unwrap_or(Real::NAN)).collect();
                    order.sort_by(|&a, &b| {
                        vals[a].partial_cmp(&vals[b]).unwrap_or(std::cmp::Ordering::Equal)
                    });
                }
                let mut rank = vec![0usize; order.len()];
                for (r, &p) in order.iter().enumerate() {
                    rank[p] = r;
                }
                (order, rank)
            })
            .collect();

        let mut seen = vec![false; n];
        let mut digits = vec![0usize; sg.axes.len()];
        for start in 0..n {
            if seen[start] || !in_band(smoothed[base + start].value) {
                continue;
            }
            // Flood fill from `start` over the ±1 sorted-position neighbourhood.
            let mut region = 0usize;
            let mut stack = vec![start];
            seen[start] = true;
            while let Some(ci) = stack.pop() {
                region += 1;
                for (j, digit) in digits.iter_mut().enumerate() {
                    *digit = (ci / strides[j]) % lens[j];
                }
                for (slot, &j) in numeric.iter().enumerate() {
                    let (order, rank) = &steps[slot];
                    for step in [-1i64, 1] {
                        let target = rank[digits[j]] as i64 + step;
                        if target < 0 || target >= lens[j] as i64 {
                            continue;
                        }
                        let q = order[target as usize];
                        let nci = ci + q * strides[j] - digits[j] * strides[j];
                        if !seen[nci] && in_band(smoothed[base + nci].value) {
                            seen[nci] = true;
                            stack.push(nci);
                        }
                    }
                }
            }
            largest = largest.max(region);
        }
        base += n;
    }
    largest
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
    /// Under `--smooth`, the winner's smoothed IS ranking key and the support
    /// behind it — the value this fold was actually selected on. `None` when
    /// smoothing didn't run (or when there was no `--best-by` to rank by).
    pub is_smoothed: Option<SmoothedKey>,
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
    pub composite_fills: Vec<crate::Fill<Symbol>>,
    /// Orders refused during the stitched OOS segments, on the same composite
    /// bar axis as `composite_fills`.
    pub composite_rejections: Vec<crate::Rejected<Symbol>>,
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
/// `probe_readiness` should return the row's `stable_bars()` (or
/// `warm_up_bars()` under a keep-unstable opt-out).
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
    smooth: Option<&Smoothing>,
    jobs: Option<usize>,
    cash: Real,
) -> Result<WalkForwardResult>
where
    P: Fn(&HashMap<String, Value>) -> Result<usize> + Sync,
    R: Fn(&HashMap<String, Value>) -> Result<crate::RunReport<Symbol>> + Sync,
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
    let reports: Vec<crate::RunReport<Symbol>> = pool.install(|| {
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
    let mut composite_fills: Vec<crate::Fill<Symbol>> = Vec::new();
    let mut composite_rejections: Vec<crate::Rejected<Symbol>> = Vec::new();
    let mut running_equity: Real = cash;

    for (fold_idx, fold) in folds.iter().enumerate() {
        let mut fold_smoothed: Option<Vec<SmoothedKey>> = None;
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
        //
        // `per_row` is in `plan` order — subgrid-major, then combo order — the
        // same layout `smooth_keys` reads its lattices out of. Keys stay in the
        // metric's *native* orientation (`compare_keys` owns direction), so a
        // smoothed `drawdown.max_pct` reads as a drawdown, not as its negation.
        let mut winner_smoothed: Option<SmoothedKey> = None;
        let winner_idx: usize = match &best_by {
            Some((_, path, direction)) => {
                let keys: Vec<Option<Real>> =
                    per_row.iter().map(|(is_m, _)| lookup(is_m, path)).collect();
                let ranked: Vec<Option<Real>> = match smooth {
                    // This is the selection rule whose out-of-sample behaviour
                    // the composite measures — so smoothing it changes what the
                    // composite is an estimate *of*. That is the point: the
                    // per-fold argmax is exactly the biased rule.
                    Some(cfg) => {
                        let smoothed = smooth_keys(&subgrids, &keys, cfg)?;
                        let values = smoothed.iter().map(|s| s.value).collect();
                        fold_smoothed = Some(smoothed);
                        values
                    }
                    None => keys,
                };
                let idx = argbest(&ranked, *direction).unwrap_or(0);
                winner_smoothed = fold_smoothed.as_ref().map(|s| s[idx]);
                idx
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
        for rejected in oos_slice.rejections {
            composite_rejections.push(crate::Rejected {
                bar: rejected.bar + bar_offset,
                rejection: rejected.rejection,
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
            is_smoothed: winner_smoothed,
        });
    }

    let composite_report = crate::RunReport {
        equity_curve: composite_equity.clone(),
        fills: composite_fills.clone(),
        rejections: composite_rejections.clone(),
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
        composite_rejections,
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
        let report: RunReport<Symbol> = RunReport {
            equity_curve: vec![110.0, 110.0, 132.0, 132.0],
            fills: vec![],
            rejections: Vec::new(),
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

    // -----------------------------------------------------------------------
    // Neighbourhood smoothing
    // -----------------------------------------------------------------------

    fn nums(v: &[i64]) -> Vec<Value> {
        v.iter().map(|n| Value::from(*n)).collect()
    }

    fn strs(v: &[&str]) -> Vec<Value> {
        v.iter().map(|s| Value::from(*s)).collect()
    }

    fn box1(min_support: Real) -> Smoothing {
        Smoothing::new(SmoothKernel::Box { radius: 1 }, min_support).unwrap()
    }

    /// `smooth_keys` and `plateau_size` navigate `combos` by index arithmetic
    /// rather than by searching, so the mixed-radix contract they assume has to
    /// hold exactly. A stride swap here mis-smooths every 2-D grid silently.
    #[test]
    fn subgrid_index_space_is_mixed_radix() {
        let sg = subgrid(&[], &[("A", nums(&[1, 2, 3])), ("B", nums(&[10, 20, 30, 40]))]);
        assert_eq!(sg.axis_lens(), vec![3, 4]);
        // Last axis varies fastest, so B's stride is 1 and A's is 4.
        assert_eq!(sg.strides(), vec![4, 1]);
        for (ci, combo) in sg.combos.iter().enumerate() {
            let digits = sg.digits(ci);
            assert_eq!(combo[0], sg.axes[0].1[digits[0]], "axis A at combo {ci}");
            assert_eq!(combo[1], sg.axes[1].1[digits[1]], "axis B at combo {ci}");
        }
    }

    /// `argbest` is how each walk-forward fold picks its winner. The tie-break
    /// is load-bearing: `max_by` kept the *last* maximum, so a table built
    /// before it was factored out would silently re-select if it flipped.
    #[test]
    fn argbest_keeps_the_later_grid_point_on_a_tie() {
        let tied = vec![Some(1.0), Some(5.0), Some(3.0), Some(5.0), Some(2.0)];
        assert_eq!(argbest(&tied, Direction::Descending), Some(3));
        let tied_low = vec![Some(9.0), Some(1.0), Some(4.0), Some(1.0)];
        assert_eq!(argbest(&tied_low, Direction::Ascending), Some(3));
        // `None` keys are skipped, not ranked last-but-selectable.
        assert_eq!(argbest(&[None, Some(2.0), None], Direction::Descending), Some(1));
        assert_eq!(argbest(&[None, None], Direction::Descending), None);
    }

    #[test]
    fn smooth_kernel_parses_every_documented_form() {
        use std::str::FromStr;
        assert_eq!(SmoothKernel::from_str("box:2").unwrap(), SmoothKernel::Box { radius: 2 });
        assert_eq!(SmoothKernel::from_str("box").unwrap(), SmoothKernel::Box { radius: 1 });
        assert_eq!(
            SmoothKernel::from_str("triangle:3").unwrap(),
            SmoothKernel::Triangle { radius: 3 }
        );
        assert_eq!(
            SmoothKernel::from_str("gaussian:1.5").unwrap(),
            SmoothKernel::Gaussian { bandwidth: 1.5 }
        );
        // Round-trips through Display, so the console echo is re-parseable.
        for spelling in ["box:1", "triangle:2", "gaussian:1.5"] {
            assert_eq!(SmoothKernel::from_str(spelling).unwrap().to_string(), spelling);
        }
        // A bandwidth of zero has no neighbourhood — refuse rather than divide by it.
        assert!(SmoothKernel::from_str("gaussian:0").is_err());
        assert!(SmoothKernel::from_str("boxx:1").is_err());
        assert!(SmoothKernel::from_str("box:wide").is_err());
    }

    /// The whole point of the flag: an isolated maximum is a noise draw, a
    /// broad region is signal. Raw argmax picks the spike; smoothed picks the
    /// plateau.
    #[test]
    fn a_lone_spike_loses_to_a_broad_plateau() {
        let sg = subgrid(&[], &[("P", nums(&[1, 2, 3, 4, 5, 6, 7, 8, 9, 10]))]);
        //             spike at index 2 ─┐        plateau over 6..=8 ─────┐
        let keys: Vec<Option<Real>> =
            [1.0, 1.0, 9.0, 1.0, 1.0, 1.0, 5.0, 5.0, 5.0, 1.0].iter().map(|v| Some(*v)).collect();
        let smoothed = smooth_keys(&[sg], &keys, &box1(0.0)).unwrap();

        let raw_argmax = rank_positions(&keys, Direction::Descending)
            .iter()
            .position(|&r| r == 1)
            .unwrap();
        assert_eq!(raw_argmax, 2, "the raw argmax is the spike");

        let values: Vec<Option<Real>> = smoothed.iter().map(|s| s.value).collect();
        let smooth_argmax = rank_positions(&values, Direction::Descending)
            .iter()
            .position(|&r| r == 1)
            .unwrap();
        assert_eq!(smooth_argmax, 7, "smoothing picks the plateau centre");
        // The spike's own smoothed value is (1+9+1)/3, well under the plateau's 5.
        assert!((smoothed[2].value.unwrap() - 11.0 / 3.0).abs() < 1e-12);
        assert!((smoothed[7].value.unwrap() - 5.0).abs() < 1e-12);
    }

    /// Smoothing is a plain average on an already-directed key, so the very
    /// same call has to pick the low-drawdown *plateau* over the low-drawdown
    /// *spike* when the metric is minimize-oriented.
    #[test]
    fn an_ascending_metric_smooths_identically() {
        let sg = subgrid(&[], &[("P", nums(&[1, 2, 3, 4, 5, 6, 7, 8, 9, 10]))]);
        // Mirror of the descending case: lower is better.
        let keys: Vec<Option<Real>> =
            [9.0, 9.0, 1.0, 9.0, 9.0, 9.0, 5.0, 5.0, 5.0, 9.0].iter().map(|v| Some(*v)).collect();
        let smoothed = smooth_keys(&[sg], &keys, &box1(0.0)).unwrap();
        let values: Vec<Option<Real>> = smoothed.iter().map(|s| s.value).collect();
        assert_eq!(
            rank_positions(&keys, Direction::Ascending).iter().position(|&r| r == 1),
            Some(2),
            "the raw argmin is the spike"
        );
        assert_eq!(
            rank_positions(&values, Direction::Ascending).iter().position(|&r| r == 1),
            Some(7),
            "smoothing picks the plateau centre under Ascending too"
        );
    }

    /// `--grid` is repeatable and the point sets are a disjoint union. Two rows
    /// from different subgrids are never neighbours, even when their
    /// union-column values are adjacent — smoothing happens before projection.
    #[test]
    fn subgrids_never_leak_weight_into_each_other() {
        // P=[1,2,3] and P=[4,5,6]: 3 and 4 are adjacent in *value* space and in
        // the stacked CSV, but they live in different lattices.
        let a = subgrid(&[], &[("P", nums(&[1, 2, 3]))]);
        let b = subgrid(&[], &[("P", nums(&[4, 5, 6]))]);
        let keys: Vec<Option<Real>> = [1.0, 1.0, 1.0, 100.0, 100.0, 100.0]
            .iter()
            .map(|v| Some(*v))
            .collect();
        let both = smooth_keys(&[a, b], &keys, &box1(0.0)).unwrap();

        // Each subgrid smoothed alone must give bit-identical results.
        let a1 = subgrid(&[], &[("P", nums(&[1, 2, 3]))]);
        let b1 = subgrid(&[], &[("P", nums(&[4, 5, 6]))]);
        let alone_a = smooth_keys(&[a1], &keys[..3], &box1(0.0)).unwrap();
        let alone_b = smooth_keys(&[b1], &keys[3..], &box1(0.0)).unwrap();
        for i in 0..3 {
            assert_eq!(both[i], alone_a[i], "subgrid A row {i} saw subgrid B");
            assert_eq!(both[3 + i], alone_b[i], "subgrid B row {i} saw subgrid A");
        }
        // The boundary rows would be visibly dragged if weight had leaked.
        assert!((both[2].value.unwrap() - 1.0).abs() < 1e-12);
        assert!((both[3].value.unwrap() - 100.0).abs() < 1e-12);
    }

    /// A non-numeric axis has no ordering, so lattice distance along it is
    /// meaningless. It partitions instead: each level is its own lattice.
    #[test]
    fn a_categorical_axis_partitions_rather_than_smooths() {
        // Axes sort by name: MODE then P. MODE is the slow axis (stride 3).
        let sg = subgrid(
            &[],
            &[("MODE", strs(&["none", "atr"])), ("P", nums(&[1, 2, 3]))],
        );
        // MODE=none rows are 0..3, MODE=atr rows are 3..6 (axes are name-sorted,
        // and "atr" < "none" is irrelevant — declaration order is preserved).
        let keys: Vec<Option<Real>> = [1.0, 1.0, 1.0, 100.0, 100.0, 100.0]
            .iter()
            .map(|v| Some(*v))
            .collect();
        let smoothed = smooth_keys(&[sg], &keys, &box1(0.0)).unwrap();
        for i in 0..3 {
            assert!(
                (smoothed[i].value.unwrap() - 1.0).abs() < 1e-12,
                "level 0 row {i} was contaminated by level 1: {:?}",
                smoothed[i]
            );
            assert!(
                (smoothed[3 + i].value.unwrap() - 100.0).abs() < 1e-12,
                "level 1 row {i} was contaminated by level 0: {:?}",
                smoothed[3 + i]
            );
        }
        // Support is full despite the partition: a categorical axis contributes
        // no weight to the ideal, so it neither helps nor penalizes.
        assert!((smoothed[1].support - 1.0).abs() < 1e-12);
    }

    /// Boundary points renormalize over the neighbours that exist, and report
    /// how much of a full neighbourhood that was. This is why grid maxima like
    /// to sit on edges, so the reduced support has to be visible.
    #[test]
    fn edges_renormalize_and_report_reduced_support() {
        let sg = subgrid(&[], &[("A", nums(&[1, 2, 3])), ("B", nums(&[1, 2, 3]))]);
        let keys: Vec<Option<Real>> = (0..9).map(|_| Some(1.0)).collect();
        let smoothed = smooth_keys(&[sg], &keys, &box1(0.0)).unwrap();
        // 3x3 lattice, box:1 → ideal is 3*3 = 9 weight units.
        assert!((smoothed[4].support - 1.0).abs() < 1e-12, "the centre is fully interior");
        for corner in [0usize, 2, 6, 8] {
            assert!(
                (smoothed[corner].support - 4.0 / 9.0).abs() < 1e-12,
                "corner {corner} should see 4 of 9 weight units, got {}",
                smoothed[corner].support
            );
        }
        for edge in [1usize, 3, 5, 7] {
            assert!((smoothed[edge].support - 6.0 / 9.0).abs() < 1e-12);
        }
        // Every value is still exactly 1.0 — renormalization, not zero-padding.
        assert!(smoothed.iter().all(|s| (s.value.unwrap() - 1.0).abs() < 1e-12));
    }

    #[test]
    fn min_support_drops_every_non_interior_row() {
        let sg = subgrid(&[], &[("A", nums(&[1, 2, 3])), ("B", nums(&[1, 2, 3]))]);
        let keys: Vec<Option<Real>> = (0..9).map(|_| Some(1.0)).collect();
        let smoothed = smooth_keys(&[sg], &keys, &box1(1.0)).unwrap();
        let kept: Vec<usize> = smoothed
            .iter()
            .enumerate()
            .filter(|(_, s)| s.value.is_some())
            .map(|(i, _)| i)
            .collect();
        assert_eq!(kept, vec![4], "only the fully interior centre clears min-support 1.0");
        // Support is still reported for the dropped rows — that is the diagnostic.
        assert!(smoothed.iter().all(|s| s.support > 0.0));
    }

    /// An axis shorter than the kernel's diameter leaves no interior point at
    /// all, so a min-support of 1.0 would silently null the entire grid and
    /// hand back "the first point wins" dressed as a verdict. Refuse instead.
    #[test]
    fn a_min_support_that_discards_everything_is_an_error() {
        let sg = subgrid(&[], &[("A", nums(&[1, 2]))]);
        let keys = vec![Some(1.0), Some(2.0)];
        let err = smooth_keys(&[sg], &keys, &box1(1.0)).unwrap_err().to_string();
        assert!(err.contains("discarded every grid point"), "unhelpful error: {err}");
        assert!(err.contains("0.667"), "the error should name the best realized support: {err}");
    }

    /// A `None` neighbour is *absent evidence*, not evidence of zero. It leaves
    /// the numerator and the denominator alone and shows up as reduced support.
    #[test]
    fn a_none_neighbour_reduces_support_without_biasing_the_mean() {
        let sg = subgrid(&[], &[("P", nums(&[1, 2, 3, 4, 5]))]);
        let keys = vec![Some(4.0), Some(4.0), Some(4.0), None, Some(4.0)];
        let smoothed = smooth_keys(&[sg], &keys, &box1(0.0)).unwrap();
        // Row 2's neighbourhood is {4.0, 4.0, None}: the mean stays 4.0 (not
        // 8/3, which is what treating None as zero would give) and support drops.
        assert!((smoothed[2].value.unwrap() - 4.0).abs() < 1e-12);
        assert!((smoothed[2].support - 2.0 / 3.0).abs() < 1e-12);
        // A row whose *own* key is None stays None however healthy its neighbours.
        assert_eq!(smoothed[3].value, None);
        assert!(smoothed[3].support > 0.0, "support is still measured for it");
    }

    #[test]
    fn a_subgrid_with_no_axes_smooths_to_itself() {
        let sg = subgrid(&[("X", Value::from(1))], &[]);
        assert_eq!(sg.points(), 1);
        let smoothed = smooth_keys(&[sg], &[Some(7.0)], &box1(1.0)).unwrap();
        assert_eq!(smoothed[0].value, Some(7.0));
        assert!((smoothed[0].support - 1.0).abs() < 1e-12);
    }

    #[test]
    fn triangle_and_gaussian_weight_by_lattice_distance() {
        let sg = subgrid(&[], &[("P", nums(&[1, 2, 3]))]);
        let keys = vec![Some(0.0), Some(0.0), Some(3.0)];
        // triangle:1 → weights (1/2, 1, 1/2). Row 1 sees 0*.5 + 0*1 + 3*.5 = 1.5
        // over a found weight of 2.0.
        let tri = Smoothing::new(SmoothKernel::Triangle { radius: 1 }, 0.0).unwrap();
        let out = smooth_keys(&[sg], &keys, &tri).unwrap();
        assert!((out[1].value.unwrap() - 0.75).abs() < 1e-12, "{:?}", out[1]);

        // gaussian truncates at 3S, so a wide bandwidth reaches the whole axis.
        let sg = subgrid(&[], &[("P", nums(&[1, 2, 3]))]);
        let g = Smoothing::new(SmoothKernel::Gaussian { bandwidth: 1.0 }, 0.0).unwrap();
        let out = smooth_keys(&[sg], &keys, &g).unwrap();
        let w1 = (-0.5f64).exp();
        let w2 = (-2.0f64).exp();
        let expected = (0.0 * w2 + 0.0 * w1 + 3.0 * 1.0) / (w2 + w1 + 1.0);
        assert!((out[2].value.unwrap() - expected).abs() < 1e-12, "{:?}", out[2]);
    }

    #[test]
    fn smooth_keys_refuses_a_key_vector_that_is_not_the_grid() {
        let sg = subgrid(&[], &[("P", nums(&[1, 2, 3]))]);
        let err = smooth_keys(&[sg], &[Some(1.0)], &box1(0.0)).unwrap_err().to_string();
        assert!(err.contains("1 ranking keys for 3 grid points"), "{err}");
    }

    fn floats(v: &[Real]) -> Vec<Value> {
        v.iter().map(|x| Value::from(*x)).collect()
    }

    /// Match two smoothed grids by the axis value each point carries, not by
    /// enumeration position — the whole point being that the two declarations
    /// enumerate the same points in different orders.
    fn by_value(axis: &[Value], smoothed: &[SmoothedKey]) -> Vec<(String, SmoothedKey)> {
        let mut pairs: Vec<(String, SmoothedKey)> = axis
            .iter()
            .zip(smoothed)
            .map(|(v, s)| (format_value(v), *s))
            .collect();
        pairs.sort_by(|a, b| a.0.cmp(&b.0));
        pairs
    }

    /// The reproduction that motivated measuring distance in value space:
    /// seven values, seven identical keys, only the typing order differs. In
    /// index space the winner flipped from `FAST=3` to `FAST=8` and the raw
    /// argmax fell from smoothed rank 1 to rank 6 — silently.
    ///
    /// Asserted as *exact* equality, not a tolerance: neighbours are summed in
    /// ascending value order regardless of declaration, so the two grids
    /// accumulate the same terms in the same sequence.
    #[test]
    fn declaration_order_does_not_affect_smoothing() {
        let sorted = nums(&[3, 4, 5, 6, 7, 8, 9]);
        let scrambled = nums(&[3, 9, 4, 8, 5, 7, 6]);
        let reversed = nums(&[9, 8, 7, 6, 5, 4, 3]);
        // One key per *value*, so both declarations describe the same surface.
        let key_of = |v: &Value| Some(v.as_f64().unwrap() * v.as_f64().unwrap());
        let keys_sorted: Vec<Option<Real>> = sorted.iter().map(key_of).collect();
        let keys_scrambled: Vec<Option<Real>> = scrambled.iter().map(key_of).collect();
        let keys_reversed: Vec<Option<Real>> = reversed.iter().map(key_of).collect();

        for kernel in [
            SmoothKernel::Box { radius: 1 },
            SmoothKernel::Box { radius: 2 },
            SmoothKernel::Triangle { radius: 2 },
            SmoothKernel::Gaussian { bandwidth: 1.5 },
        ] {
            let cfg = Smoothing::new(kernel, 0.0).unwrap();
            let a = smooth_keys(&[subgrid(&[], &[("FAST", sorted.clone())])], &keys_sorted, &cfg)
                .unwrap();
            let b = smooth_keys(
                &[subgrid(&[], &[("FAST", scrambled.clone())])],
                &keys_scrambled,
                &cfg,
            )
            .unwrap();
            let c =
                smooth_keys(&[subgrid(&[], &[("FAST", reversed.clone())])], &keys_reversed, &cfg)
                    .unwrap();
            assert_eq!(
                by_value(&sorted, &a),
                by_value(&scrambled, &b),
                "{kernel} smoothed differently once the list was reordered"
            );
            assert_eq!(
                by_value(&sorted, &a),
                by_value(&reversed, &c),
                "{kernel} smoothed differently once the list was reversed"
            );
        }

        // `--smooth-scale=index` is the documented way back to the old,
        // order-dependent measure — so it had better still be order-dependent.
        let cfg = Smoothing::new(SmoothKernel::Box { radius: 1 }, 0.0)
            .unwrap()
            .with_scales(SmoothScales::all(AxisScale::Index));
        let a =
            smooth_keys(&[subgrid(&[], &[("FAST", sorted.clone())])], &keys_sorted, &cfg).unwrap();
        let b = smooth_keys(&[subgrid(&[], &[("FAST", scrambled.clone())])], &keys_scrambled, &cfg)
            .unwrap();
        assert_ne!(by_value(&sorted, &a), by_value(&scrambled, &b));
    }

    /// The load-bearing compatibility guarantee: on a regularly spaced axis
    /// `|vᵢ − vⱼ| / step` *is* `|i − j|`, so value space and the old index
    /// space must agree **exactly** — not to 1e-12. `axis_geometry`'s regular
    /// fast path is what makes that true: it substitutes integer ranks for the
    /// division, so no float error is introduced to begin with.
    ///
    /// Covers a range axis, an evenly spaced list, and a float axis where the
    /// accumulation in `try_parse_range` would otherwise bite.
    ///
    /// Only *ascending* declarations, deliberately. `index` measures between
    /// declared positions and so orders a descending list back to front; value
    /// space always walks ascending. The two therefore sum the same terms in
    /// opposite sequences, and f64 addition is not associative. That is not a
    /// defect to paper over — declaration order not mattering is the property
    /// this change exists to establish, and `declaration_order_does_not_affect_smoothing`
    /// is where a descending list is pinned.
    #[test]
    fn a_regular_axis_is_byte_identical_to_index_space() {
        let axes: Vec<Vec<Value>> = vec![
            nums(&[3, 4, 5, 6, 7, 8, 9]),                       // 3..9:1
            nums(&[10, 20, 30, 40, 50]),                        // evenly spaced list
            floats(&[1.0, 1.5, 2.0, 2.5, 3.0, 3.5, 4.0]),       // 1.0..4.0:0.5
            floats(&[0.1, 0.2, 0.30000000000000004, 0.4, 0.5]), // 0.1..0.5:0.1, drift and all
        ];
        let kernels = [
            SmoothKernel::Box { radius: 1 },
            SmoothKernel::Box { radius: 2 },
            SmoothKernel::Triangle { radius: 1 },
            SmoothKernel::Triangle { radius: 3 },
            SmoothKernel::Gaussian { bandwidth: 1.0 },
            SmoothKernel::Gaussian { bandwidth: 1.5 },
        ];
        for values in &axes {
            let keys: Vec<Option<Real>> =
                (0..values.len()).map(|i| Some(1.0 / (i as Real + 3.0))).collect();
            for kernel in kernels {
                let auto = Smoothing::new(kernel, 0.0).unwrap();
                let index = auto.clone().with_scales(SmoothScales::all(AxisScale::Index));
                let sg = || vec![subgrid(&[], &[("P", values.clone())])];
                assert_eq!(
                    smooth_keys(&sg(), &keys, &auto).unwrap(),
                    smooth_keys(&sg(), &keys, &index).unwrap(),
                    "{kernel} over {values:?} drifted off index space"
                );
            }
        }

        // Two regular axes at once — the separable product is where a
        // per-axis normalization would show up if it were inexact.
        let sg = || {
            vec![subgrid(
                &[],
                &[("A", floats(&[1.0, 1.5, 2.0, 2.5])), ("B", nums(&[10, 20, 30, 40, 50]))],
            )]
        };
        let keys: Vec<Option<Real>> = (0..20).map(|i| Some((i as Real).sin())).collect();
        let auto = Smoothing::new(SmoothKernel::Triangle { radius: 2 }, 0.0).unwrap();
        let index = auto.clone().with_scales(SmoothScales::all(AxisScale::Index));
        assert_eq!(
            smooth_keys(&sg(), &keys, &auto).unwrap(),
            smooth_keys(&sg(), &keys, &index).unwrap()
        );
    }

    /// The whole point of the change: on `[10,20,50,200]` the `50→200` jump is
    /// eight times the `10→20` one in parameter terms, and index space called
    /// them equally close.
    #[test]
    fn an_irregular_axis_weights_by_parameter_distance() {
        let values = nums(&[10, 20, 50, 200]);
        let axis = || vec![subgrid(&[], &[("PERIOD", values.clone())])];
        // Two one-point spikes, read from the *near* side each time: how much
        // of 10 reaches 20, and how much of 200 reaches 50.
        let from_10 = vec![Some(1.0), Some(0.0), Some(0.0), Some(0.0)];
        let from_200 = vec![Some(0.0), Some(0.0), Some(0.0), Some(1.0)];
        let cfg = box1(0.0);

        let near = smooth_keys(&axis(), &from_10, &cfg).unwrap()[1].value.unwrap();
        let far = smooth_keys(&axis(), &from_200, &cfg).unwrap()[2].value.unwrap();
        assert!(near > far, "10 should reach 20 ({near}) harder than 200 reaches 50 ({far})");
        assert!((far - 0.0).abs() < 1e-12, "200 is more than one typical step from 50");

        // Index space is exactly the claim being refuted: there the two pairs
        // are one declared step apart each, and the bleed is identical.
        let idx = cfg.clone().with_scales(SmoothScales::all(AxisScale::Index));
        let near = smooth_keys(&axis(), &from_10, &idx).unwrap()[1].value.unwrap();
        let far = smooth_keys(&axis(), &from_200, &idx).unwrap()[2].value.unwrap();
        assert!((near - far).abs() < 1e-12, "index space: {near} vs {far}");
        assert!((near - 1.0 / 3.0).abs() < 1e-12);
    }

    /// A user who writes `[10,20,50,100,200]` chose a geometric grid, and
    /// `100→200` is about as near as `10→20` in strategy terms. The heuristic
    /// has to notice that, leave additive grids alone, and never log-transform
    /// an axis that reaches zero.
    #[test]
    fn a_geometric_axis_is_detected_as_log() {
        let scale_of = |values: Vec<Value>| {
            let sg = subgrid(&[], &[("P", values)]);
            let cfg = Smoothing::new(SmoothKernel::Box { radius: 1 }, 0.0).unwrap();
            resolved_axis_scales(&[sg], &cfg).unwrap()[0].1
        };
        assert_eq!(scale_of(nums(&[10, 20, 50, 100, 200])), AxisScale::Log);
        assert_eq!(scale_of(nums(&[1, 2, 4, 8, 16, 32])), AxisScale::Log);
        // Regular grids are a fixed point of the test — nothing beats a zero CV.
        assert_eq!(scale_of(nums(&[10, 20, 30, 40])), AxisScale::Linear);
        assert_eq!(scale_of(floats(&[1.0, 1.5, 2.0, 2.5])), AxisScale::Linear);
        // Irregular but not geometric — log makes the gaps *less* uniform.
        assert_eq!(scale_of(nums(&[10, 20, 30, 45])), AxisScale::Linear);
        // Log is only admissible where every value is strictly positive.
        assert_eq!(scale_of(floats(&[0.0, 1.0, 2.0, 8.0])), AxisScale::Linear);
        assert_eq!(scale_of(floats(&[-4.0, -2.0, -1.0, -0.5])), AxisScale::Linear);

        // On an exactly geometric axis log recovers *uniform* spacing, so the
        // surface is the one an evenly spaced axis of the same length gives —
        // bit for bit, via the regular fast path.
        let keys = vec![Some(1.0), Some(2.0), Some(4.0), Some(8.0), Some(16.0)];
        let cfg = box1(0.0);
        let geometric = smooth_keys(&[subgrid(&[], &[("P", nums(&[10, 20, 40, 80, 160]))])], &keys, &cfg).unwrap();
        let regular = smooth_keys(&[subgrid(&[], &[("P", nums(&[1, 2, 3, 4, 5]))])], &keys, &cfg).unwrap();
        assert_eq!(geometric, regular);
        assert!((geometric[2].value.unwrap() - (2.0 + 4.0 + 8.0) / 3.0).abs() < 1e-12);

        // A merely *roughly* geometric axis still gets far closer to uniform
        // than linear would: every point keeps at least an edge's worth of
        // neighbourhood, where linear distance would strand 200 alone.
        let rough = nums(&[10, 20, 50, 100, 200]);
        let out = smooth_keys(&[subgrid(&[], &[("P", rough.clone())])], &keys, &cfg).unwrap();
        assert!(
            out.iter().all(|s| s.support >= 2.0 / 3.0 - 1e-12),
            "log spacing should leave every point a neighbour: {out:?}"
        );
        let linear = cfg.clone().with_scales(SmoothScales::all(AxisScale::Linear));
        let out = smooth_keys(&[subgrid(&[], &[("P", rough)])], &keys, &linear).unwrap();
        assert!((out[4].support - 1.0 / 3.0).abs() < 1e-12, "linear strands 200: {:?}", out[4]);

        // And an explicit `log` pin on an axis that reaches zero is an error,
        // not a silent fallback — the user asked for something undefined.
        let pinned = Smoothing::new(SmoothKernel::Box { radius: 1 }, 0.0)
            .unwrap()
            .with_scales(SmoothScales::default().with_axis("P", AxisScale::Log));
        let sg = subgrid(&[], &[("P", floats(&[0.0, 1.0, 2.0]))]);
        let err = smooth_keys(&[sg], &[Some(1.0); 3], &pinned).unwrap_err().to_string();
        assert!(err.contains("non-positive"), "{err}");
    }

    /// A one-value numeric axis is not a swept dimension: it carries no
    /// neighbourhood information in either direction, exactly like a
    /// categorical one. Multiplying its `Σ w(d)` into the support denominator
    /// divided every point's support by 3 under `box:1`, so the same sweep
    /// scored 1.000 written `SLOW=20` and 0.333 written `SLOW=[20]`.
    #[test]
    fn a_pinned_axis_does_not_dilute_support() {
        let keys: Vec<Option<Real>> = (0..7).map(|i| Some(i as Real)).collect();
        let cfg = box1(0.0);
        let scalar = subgrid(&[("SLOW", Value::from(20))], &[("FAST", nums(&[3, 4, 5, 6, 7, 8, 9]))]);
        let listed = subgrid(&[], &[("FAST", nums(&[3, 4, 5, 6, 7, 8, 9])), ("SLOW", nums(&[20]))]);
        let a = smooth_keys(&[scalar], &keys, &cfg).unwrap();
        let b = smooth_keys(&[listed], &keys, &cfg).unwrap();
        assert_eq!(a, b, "the two spellings of a pinned axis must smooth identically");
        // Smoothed *values* never moved — only the denominator did.
        assert!((a[3].value.unwrap() - 3.0).abs() < 1e-12);
        assert!((a[3].support - 1.0).abs() < 1e-12, "an interior point reaches 1.0");
        assert!((a[0].support - 2.0 / 3.0).abs() < 1e-12, "the edge is still an edge");
        // Same for the degenerate axis a categorical one shadows.
        let mixed = subgrid(
            &[],
            &[("FAST", nums(&[3, 4, 5, 6, 7, 8, 9])), ("MODE", strs(&["atr"]))],
        );
        assert_eq!(smooth_keys(&[mixed], &keys, &cfg).unwrap(), a);
    }

    /// The user-visible half of the same bug: `--smooth-min-support 1.0` over
    /// `FAST=3..9:1 × SLOW=[20]` hard-errored with "best realized support was
    /// 0.333" on a grid where every interior `FAST` point had a complete
    /// neighbourhood.
    #[test]
    fn min_support_ignores_pinned_axes() {
        let keys: Vec<Option<Real>> = (0..7).map(|i| Some(i as Real)).collect();
        let sg = subgrid(&[], &[("FAST", nums(&[3, 4, 5, 6, 7, 8, 9])), ("SLOW", nums(&[20]))]);
        let out = smooth_keys(&[sg], &keys, &box1(1.0)).unwrap();
        let kept: Vec<usize> =
            out.iter().enumerate().filter(|(_, s)| s.value.is_some()).map(|(i, _)| i).collect();
        assert_eq!(kept, vec![1, 2, 3, 4, 5], "only the two FAST edges fall short");
    }

    /// `support` stays a fraction of `Π_j Σ_{d=−R..R} w(d)` — the weight a point
    /// in the interior of a *regular* axis of this axis' own median spacing
    /// would find. That reference is a property of the kernel alone, which is
    /// what keeps `1.0` reachable and `--smooth-min-support 1.0` meaningful.
    ///
    /// The two rejected alternatives are why this test pins both ends:
    /// normalizing by the best weight *any* position achieves would report
    /// `1.0` in the sparse stretch below (there is no better position to lose
    /// to), and comparing against the continuous kernel mass would put `1.0`
    /// out of reach everywhere. So: a sparse stretch scores below `1.0`, and a
    /// denser-than-median pocket is clamped at `1.0` rather than overshooting.
    #[test]
    fn support_is_measured_against_the_kernel_not_the_local_density() {
        // Median gap 1.0. Positions 0..3 are regular; 4 sits 4 units out.
        let values = floats(&[0.0, 1.0, 2.0, 3.0, 7.0]);
        let sg = subgrid(&[], &[("P", values)]);
        let keys = vec![Some(1.0); 5];
        let out = smooth_keys(&[sg], &keys, &box1(0.0)).unwrap();
        assert!((out[2].support - 1.0).abs() < 1e-12, "a regular interior point is fully supported");
        // The far point has no neighbour within one median gap: itself only.
        assert!((out[4].support - 1.0 / 3.0).abs() < 1e-12, "{:?}", out[4]);
        // Its inward neighbour is an interior *position* but sits on the edge
        // of the sparse stretch, so it too falls short — the honest reading.
        assert!(out[3].support < 1.0, "{:?}", out[3]);

        // A pocket denser than the median finds *more* weight than the reference,
        // and that is reported rather than squeezed into `0..=1`: position 3
        // sees four points where a regular axis of the same median spacing
        // would hand it three. Clamping would report "exactly fully supported"
        // for two different situations.
        let dense = subgrid(&[], &[("P", floats(&[0.0, 1.0, 2.0, 3.0, 3.1, 3.2]))]);
        let out = smooth_keys(&[dense], &[Some(1.0); 6], &box1(0.0)).unwrap();
        assert!((out[3].support - 4.0 / 3.0).abs() < 1e-12, "{:?}", out[3]);
        // `min_support` is a floor, so an over-supported point clears it either
        // way — nothing downstream depended on the clamp.
        let floored = smooth_keys(
            &[subgrid(&[], &[("P", floats(&[0.0, 1.0, 2.0, 3.0, 3.1, 3.2]))])],
            &[Some(1.0); 6],
            &box1(1.0),
        )
        .unwrap();
        assert!(floored[3].value.is_some(), "{:?}", floored[3]);
    }

    #[test]
    fn smooth_scales_parses_every_documented_form() {
        use std::str::FromStr;
        assert_eq!(SmoothScales::from_str("index").unwrap(), SmoothScales::all(AxisScale::Index));
        assert_eq!(
            SmoothScales::from_str("PERIOD:log,ATR_MULT:linear").unwrap(),
            SmoothScales::default()
                .with_axis("PERIOD", AxisScale::Log)
                .with_axis("ATR_MULT", AxisScale::Linear)
        );
        // A bare default and per-axis pins compose; the pin wins.
        let mixed = SmoothScales::from_str("linear,PERIOD:log").unwrap();
        assert_eq!(mixed.pinned("PERIOD"), Some(AxisScale::Log));
        assert_eq!(mixed.pinned("FAST"), Some(AxisScale::Linear));
        assert!(SmoothScales::auto().is_auto());
        assert_eq!(SmoothScales::auto().pinned("FAST"), None);
        // Round-trips through Display, so the echo is re-parseable.
        for spelling in ["index", "linear,PERIOD:log", "ATR_MULT:linear,PERIOD:log"] {
            assert_eq!(SmoothScales::from_str(spelling).unwrap().to_string(), spelling);
        }
        assert!(SmoothScales::from_str("quadratic").is_err());
        assert!(SmoothScales::from_str("P:quadratic").is_err());
        assert!(SmoothScales::from_str("linear,index").is_err());
        assert!(SmoothScales::from_str(":log").is_err());
        assert!(SmoothScales::from_str("").is_err());
    }

    /// The grid's shape is the result; its maximum is not.
    #[test]
    fn plateau_size_measures_the_largest_connected_region() {
        let sg = subgrid(&[], &[("P", nums(&[1, 2, 3, 4, 5, 6, 7, 8, 9, 10]))]);
        // Best is 5.0 over indices 6..=8 (connected, 3 cells); the 4.99 at index
        // 0 is inside the 5% band but isolated.
        let smoothed: Vec<SmoothedKey> = [4.99, 1.0, 1.0, 1.0, 1.0, 1.0, 5.0, 5.0, 5.0, 1.0]
            .iter()
            .map(|v| SmoothedKey { value: Some(*v), support: 1.0 })
            .collect();
        assert_eq!(
            plateau_size(&[sg], &smoothed, Direction::Descending, 0.05, &SmoothScales::auto()),
            3
        );
    }

    /// Adjacency is `±1` in *sorted* position, so the console's "N of M cells"
    /// stays a count of adjacent cells however the list was typed — and a
    /// sparse stretch of an irregular axis is still one step, not zero.
    #[test]
    fn plateau_adjacency_follows_value_order_not_declaration_order() {
        let smoothed = |vs: &[Real]| -> Vec<SmoothedKey> {
            vs.iter().map(|v| SmoothedKey { value: Some(*v), support: 1.0 }).collect()
        };
        // Declared scrambled: values 1,5,2,4,3. The plateau is at values 3,4,5.
        let sg = subgrid(&[], &[("P", nums(&[1, 5, 2, 4, 3]))]);
        let keys = smoothed(&[1.0, 5.0, 1.0, 5.0, 5.0]);
        assert_eq!(
            plateau_size(&[sg], &keys, Direction::Descending, 0.05, &SmoothScales::auto()),
            3,
            "values 3, 4 and 5 are consecutive however they were typed"
        );
        // `--smooth-scale=index` reads adjacency off declared positions again,
        // where the same three cells are not connected.
        let sg = subgrid(&[], &[("P", nums(&[1, 5, 2, 4, 3]))]);
        let index = SmoothScales::all(AxisScale::Index);
        assert_eq!(plateau_size(&[sg], &keys, Direction::Descending, 0.05, &index), 2);
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
        let report: RunReport<Symbol> = RunReport {
            equity_curve: equity,
            fills: vec![],
            rejections: Vec::new(),
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
                            smoothed: None,
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
        let report: RunReport<Symbol> = RunReport {
            equity_curve: equity,
            fills: vec![],
            rejections: Vec::new(),
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
