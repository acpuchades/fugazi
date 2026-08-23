//! Panel pooling — score **one** parameter set across several instruments and
//! reduce across them, instead of ranking each `(params, instrument)` cell.
//!
//! # Why this is not a root axis
//!
//! `optimize` already varies the traded series: a `SYM=[...]` axis resolves one
//! prepared stream per distinct `root:` and evaluates each row against its own
//! (`cli::optimize::distinct_roots`). What it then does is *rank* those rows
//! against each other, which answers "which instrument cooperated best with
//! which parameters" — a question with `N×M` hypotheses behind it and no
//! defence against the winner being whichever pair happened to fit.
//!
//! Pooling answers the other question. The instrument axis is **reduced over**
//! rather than ranked on, so the grid is `N` hypotheses wide again, one score
//! per parameter set, and the number a
//! [deflated Sharpe](crate::metrics::deflated_sharpe_from_stats) should be
//! parameterized by is the one the sweep actually tested.
//!
//! # It is the windowed reduction over a different partition
//!
//! `-w/--windowed` cuts one run into non-overlapping spans and ranks the row by
//! `mean ∓ k·std` across them ([`crate::spec::optimize::ranking_value`]).
//! Pooling computes the same statistic over a different partition — members of
//! a panel rather than windows of a run — so `--risk-aversion` composes for
//! free and means the same thing it always did: a parameter set that only works
//! on one member of the panel is penalized for the spread, exactly as one that
//! only worked in one window is.
//!
//! Two properties are carried over deliberately, and one is new:
//!
//! * **An undefined metric stays undefined.** [`pool_metric`] averages over the
//!   members that *reported* a value, never over zeros substituted for the ones
//!   that could not compute one — the same `filter_map` contract
//!   [`crate::spec::optimize::lookup_windowed`] has.
//! * **A ruined member makes the whole row unrankable.** Not "drops out of the
//!   mean" — see [`PanelMetrics`].
//! * **The support is reported.** A mean over 2 of 30 members and a mean over
//!   30 of 30 are not the same evidence, and without [`Pooled::defined`] they
//!   render identically. This is the one thing the windowed reduction does not
//!   carry, because a window that reports nothing is far rarer than a member
//!   that never listed.
//!
//! # The axis is time, not bar index
//!
//! Instruments list at different dates, so member bar index `k` is a different
//! moment for each of them. Everything here is therefore laid out on a
//! [`PanelAxis`] — the sorted union of every member's bar keys — and mapped
//! back down to each member's own bar range only at the point of measurement.
//! A member with no bars inside a fold's window contributes nothing to that
//! fold and does not shift it. See [`PanelAxis::member_range`].

use std::collections::BTreeSet;
use std::ops::Range;

use anyhow::{Context, Result, bail};

use crate::market::Real;
use crate::spec::metrics::{self, Metrics};
use crate::types::{Snapshot, Symbol};

// ---------------------------------------------------------------------------
// Pooled metric readings
// ---------------------------------------------------------------------------

/// One panel member's metric document, tagged with the member it came from.
///
/// The tag is carried so a pooled row can say *which* members reported and
/// which did not. It is the member's name as the caller spelled it — the
/// symbol under the CLI's `--pooled`, whatever label the Python caller passed
/// alongside its snapshot stream.
#[derive(Clone, Debug)]
pub struct PanelMetrics {
    pub member: String,
    pub metrics: Metrics,
}

impl PanelMetrics {
    /// Whether this member's account was ruined over the measured span.
    pub fn is_ruined(&self) -> bool {
        self.metrics.run.ruin_bar.is_some()
    }
}

/// A pooled reading of one metric across a panel: the cross-member `mean` and
/// population `std`, plus how many members that average actually rests on.
///
/// `defined <= members` always. The gap is the members that ran but could not
/// compute this metric — a member that never traded has no `win_rate_pct`, a
/// member with one bar has no `sharpe`. Those are dropped from the average
/// rather than counted as zero, so `defined` is the only thing separating a
/// well-supported mean from a mean over the two members that happened to
/// produce a number.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Pooled {
    pub mean: Real,
    pub std: Real,
    /// Members that reported a value for this metric.
    pub defined: usize,
    /// Members that were measured at all — i.e. that had bars in this span.
    pub members: usize,
}

impl Pooled {
    /// The fraction of measured members backing [`Self::mean`], in `0..=1`.
    pub fn support(&self) -> Real {
        if self.members == 0 {
            0.0
        } else {
            self.defined as Real / self.members as Real
        }
    }
}

/// Pool one metric path across a panel: `(mean, std)` over the members that
/// reported it, with the support counts beside it.
///
/// `None` when **no** member reported the metric — the same "degenerate in
/// every window" answer [`crate::spec::optimize::lookup_windowed`] gives, so
/// the ranking layer above needs no new case for it.
pub fn pool_metric(members: &[PanelMetrics], path: &str) -> Option<Pooled> {
    let values: Vec<Real> = members
        .iter()
        .filter_map(|m| crate::spec::optimize::lookup(&m.metrics, path))
        .collect();
    let (mean, std) = metrics::mean_std(values.iter().copied())?;
    Some(Pooled {
        mean,
        std,
        defined: values.len(),
        members: members.len(),
    })
}

/// The member that ruined, if any — the panel's answer to
/// [`crate::spec::optimize::Evaluation::ruin_bar`].
///
/// **Any member ruining disqualifies the whole row**, rather than that member
/// quietly dropping out of the pooled mean. The alternative is actively
/// dangerous: [`pool_metric`] averages over members that reported, so a member
/// blowing up would *remove* its own contribution and **raise** the pooled
/// score. A search over that objective is rewarded for finding parameters that
/// destroy an account, which is precisely the perversity
/// [`crate::spec::optimize::ranking_lookup`] exists to prevent — applied there
/// to one account and here to a panel of them.
///
/// The numbers themselves are kept, as they are for a single ruined run: the
/// pooled mean over the survivors is a true statement about the survivors, and
/// `ruined` beside it says not to select on it.
pub fn ruined_member(members: &[PanelMetrics]) -> Option<&PanelMetrics> {
    members.iter().find(|m| m.is_ruined())
}

// ---------------------------------------------------------------------------
// The pooled axis
// ---------------------------------------------------------------------------

/// One panel member's own bar clock: the key of every bar it has, ascending.
///
/// Keys are millisecond epoch stamps read off each snapshot's atoms. They are
/// the join key, not a convenience — a panel is aligned on *when*, never on bar
/// index, because bar `k` of a 2017 listing and bar `k` of a 2021 one are
/// different years.
#[derive(Clone, Debug)]
pub struct MemberAxis {
    pub name: String,
    /// Strictly ascending bar keys.
    pub keys: Vec<i64>,
}

impl MemberAxis {
    /// Read a member's bar clock off its snapshot stream.
    ///
    /// Every snapshot must carry a bar time and the stream must be strictly
    /// ascending — both are refusals rather than repairs, because the whole
    /// point of the pooled axis is that two members' bars are comparable, and a
    /// stream that cannot say when its bars happened cannot be placed on it.
    pub fn from_snapshots(name: impl Into<String>, snapshots: &[Snapshot<Symbol>]) -> Result<Self> {
        let name = name.into();
        let mut keys = Vec::with_capacity(snapshots.len());
        for (i, snap) in snapshots.iter().enumerate() {
            let time = snap.any_atom().and_then(|a| a.time).ok_or_else(|| {
                anyhow::anyhow!(
                    "pooled panel: member `{name}` has no bar time at index {i} — pooling lays \
                     folds out on a shared clock, so every member's bars must carry a `time`"
                )
            })?;
            keys.push(time.0);
        }
        Self::from_keys(name, keys)
    }

    /// The same, from keys the caller already has (the CLI reads them off the
    /// frame's index rather than re-deriving them from built snapshots).
    pub fn from_keys(name: impl Into<String>, keys: Vec<i64>) -> Result<Self> {
        let name = name.into();
        if let Some(w) = keys.windows(2).position(|w| w[1] <= w[0]) {
            bail!(
                "pooled panel: member `{name}` is not strictly ascending in time at index {} \
                 ({} then {}) — a member's bars must be ordered and de-duplicated before pooling",
                w + 1,
                keys[w],
                keys[w + 1],
            );
        }
        Ok(Self { name, keys })
    }

    /// Index of the first bar at or after `key`, i.e. the half-open range's
    /// lower bound. `keys.len()` when every bar predates it.
    fn lower_bound(&self, key: i64) -> usize {
        self.keys.partition_point(|&k| k < key)
    }
}

/// The panel's shared clock: the sorted union of every member's bar keys, plus
/// the readiness prefix the grid needs before any of it is measurable.
///
/// Fold layout happens in *this* index space, which is what lets
/// [`crate::spec::optimize::walkforward_layout`] be reused verbatim: a fold is
/// a range of pooled indices, translated to a per-member bar range only when a
/// member is actually measured. `is`/`oos`/`embargo` therefore count **pooled
/// bars** — moments at which at least one member quoted — which for a panel on
/// a single cadence is just "bars", and for a ragged one is the only reading
/// under which fold *k* means the same span for every member.
#[derive(Clone, Debug)]
pub struct PanelAxis {
    /// The sorted, de-duplicated union of every member's keys.
    pub keys: Vec<i64>,
    pub members: Vec<MemberAxis>,
    /// Grid-wide readiness, in each member's **own** bars — the panel twin of
    /// the walk-forward pre-scan's `max(stable_bars)`. Applied per member on
    /// its own clock, so a late lister warms up over its own first bars rather
    /// than being credited for history it does not have.
    pub ready_bars: usize,
    /// Pooled index at which the **first** member becomes ready — the pooled
    /// analogue of `WalkForwardResult::prefix_skip`.
    ///
    /// The first member, not the last. Waiting for every member would truncate
    /// the panel's history to its most recent listing, which on a few years of
    /// hourly crypto discards most of the sample to buy a comparability the
    /// support counts already report. Early folds legitimately rest on few
    /// members; [`Pooled::defined`] is how that stays visible instead of
    /// silent.
    pub prefix_skip: usize,
}

impl PanelAxis {
    /// Build the shared clock from per-member clocks and a grid-wide readiness.
    pub fn new(members: Vec<MemberAxis>, ready_bars: usize) -> Result<Self> {
        if members.is_empty() {
            bail!("pooled panel: no members — pooling needs at least one instrument");
        }
        let union: BTreeSet<i64> = members
            .iter()
            .flat_map(|m| m.keys.iter().copied())
            .collect();
        let keys: Vec<i64> = union.into_iter().collect();

        // The earliest moment any member is ready to be measured. A member with
        // fewer bars than the readiness prefix is never ready and contributes
        // nothing anywhere — it is not an error (a freshly listed instrument in
        // an otherwise long panel is ordinary), but if *every* member is in that
        // position there is nothing to measure and that is.
        let first_ready = members
            .iter()
            .filter_map(|m| m.keys.get(ready_bars).copied())
            .min();
        let Some(first_ready) = first_ready else {
            bail!(
                "pooled panel: no member has more than {ready_bars} bars, which is the grid's \
                 readiness requirement — every member would be warming up for its whole history"
            );
        };
        let prefix_skip = keys.partition_point(|&k| k < first_ready);
        Ok(Self {
            keys,
            members,
            ready_bars,
            prefix_skip,
        })
    }

    /// Number of pooled bars — moments at which at least one member quoted.
    pub fn len(&self) -> usize {
        self.keys.len()
    }

    pub fn is_empty(&self) -> bool {
        self.keys.is_empty()
    }

    /// Translate a **pooled** index range into member `m`'s own bar range.
    ///
    /// Empty when the member has no bars in that span — it had not listed yet,
    /// it has already delisted, or it is still inside its own readiness prefix.
    /// An empty range is the whole mechanism behind "a member with no bars in a
    /// fold's window contributes nothing to that fold rather than shifting it":
    /// the fold's boundaries come from the pooled clock and never move, and the
    /// member simply is not measured.
    pub fn member_range(&self, m: usize, pooled: Range<usize>) -> Range<usize> {
        let member = &self.members[m];
        if pooled.start >= pooled.end || self.keys.is_empty() {
            return 0..0;
        }
        let start_key = self.keys[pooled.start.min(self.keys.len() - 1)];
        // Half-open at the pooled level stays half-open at the member level:
        // `pooled.end` is the first key *not* in the range, so bound by it when
        // it exists and by the member's full length when the range runs to the
        // end of the panel.
        let start = member.lower_bound(start_key).max(self.ready_bars);
        let end = match self.keys.get(pooled.end) {
            Some(&end_key) => member.lower_bound(end_key),
            None => member.keys.len(),
        };
        if end <= start { 0..0 } else { start..end }
    }

    /// Every member's bar range for one pooled range, in member order.
    pub fn member_ranges(&self, pooled: Range<usize>) -> Vec<Range<usize>> {
        (0..self.members.len())
            .map(|m| self.member_range(m, pooled.clone()))
            .collect()
    }

    /// How many members have at least one measurable bar in a pooled range —
    /// the support behind a fold, before any metric is even computed.
    pub fn support(&self, pooled: Range<usize>) -> usize {
        self.member_ranges(pooled)
            .iter()
            .filter(|r| !r.is_empty())
            .count()
    }
}

// ---------------------------------------------------------------------------
// Pooled walk-forward
// ---------------------------------------------------------------------------

use crate::spec::optimize::{
    Direction, FoldLayout, SmoothedKey, Smoothing, Subgrid, argbest, combine_params,
    compute_union_columns, project_row, smooth_keys, walkforward_layout,
};
use rayon::prelude::*;
use serde_json::Value;
use std::collections::HashMap;

/// The pooled ranking key for one fold: `mean ∓ k·std` across the members that
/// reported `path`, or `None` for a panel with a ruined member.
///
/// The twin of [`crate::spec::optimize::ranking_value`] for a bare slice of
/// member documents — a fold selects on documents, not on an `Evaluation`,
/// exactly as the single-stream walk-forward selects on a bare `Metrics`.
pub fn pooled_ranking_key(
    members: &[PanelMetrics],
    path: &str,
    direction: Direction,
    k: Real,
) -> Option<Real> {
    if ruined_member(members).is_some() {
        return None;
    }
    pool_metric(members, path).map(|p| match direction {
        Direction::Descending => p.mean - k * p.std,
        Direction::Ascending => p.mean + k * p.std,
    })
}

/// One member's stitched out-of-sample composite: each fold's **pooled** winner
/// applied to that member's own bars, scaled onto a running curve.
///
/// There is one of these per member rather than one netted curve for the panel,
/// and that is a deliberate refusal. Netting `M` members into a single equity
/// curve requires choosing weights and a rebalance cadence — an allocation
/// policy. `optimize` has no business inventing one silently, and fugazi
/// already has a shape that expresses it explicitly: `portfolio:`. So pooling
/// answers "how did this parameter set do out-of-sample, per instrument, and
/// how much did that vary", and leaves "what would a book of them have earned"
/// to the layer that can state its own assumptions.
pub struct MemberComposite {
    pub member: String,
    pub equity: Vec<Real>,
    pub fills: Vec<crate::Fill<Symbol>>,
    pub rejections: Vec<crate::Rejected<Symbol>>,
    /// The composite curve reduced through the full metrics catalogue.
    pub metrics: Metrics,
}

/// One fold of a pooled walk-forward: the winner chosen on the **pooled**
/// in-sample score, and both pooled metric sets it produced.
pub struct PanelFoldRow {
    pub fold: usize,
    /// In-sample range, in **pooled** indices (see [`PanelAxis`]).
    pub is: Range<usize>,
    /// Post-embargo out-of-sample range, in pooled indices.
    pub oos: Range<usize>,
    pub values: Vec<Option<Value>>,
    /// The winner's per-member in-sample documents — only members with bars in
    /// this fold's IS window appear.
    pub is_members: Vec<PanelMetrics>,
    /// The winner's per-member out-of-sample documents.
    pub oos_members: Vec<PanelMetrics>,
    /// Under `--smooth`, the winner's smoothed pooled IS key.
    pub is_smoothed: Option<SmoothedKey>,
}

impl PanelFoldRow {
    /// Members with bars in this fold's in-sample window.
    pub fn is_support(&self) -> usize {
        self.is_members.len()
    }
    /// Members with bars in this fold's out-of-sample window.
    pub fn oos_support(&self) -> usize {
        self.oos_members.len()
    }
}

/// The full result of a pooled walk-forward run.
pub struct PanelWalkForward {
    pub union_columns: Vec<String>,
    pub metric_columns: Vec<(String, String)>,
    pub best_by: Option<(String, String, Direction)>,
    /// The shared clock every fold was laid out on.
    pub axis: PanelAxis,
    /// Fold ranges, in **pooled** indices.
    pub folds: Vec<FoldLayout>,
    pub fold_rows: Vec<PanelFoldRow>,
    /// One stitched OOS composite per member, in the panel's member order.
    pub composites: Vec<MemberComposite>,
    pub is_bars: usize,
    pub oos_bars: usize,
    pub embargo_bars: usize,
    pub cash: Real,
}

impl PanelWalkForward {
    /// The composites as poolable documents — what
    /// [`pool_metric`] reduces to get the headline out-of-sample numbers.
    pub fn composite_members(&self) -> Vec<PanelMetrics> {
        self.composites
            .iter()
            .map(|c| PanelMetrics {
                member: c.member.clone(),
                metrics: c.metrics.clone(),
            })
            .collect()
    }

    /// Pool one metric across the per-member composites.
    pub fn composite_metric(&self, path: &str) -> Option<Pooled> {
        pool_metric(&self.composite_members(), path)
    }
}

/// Pure pooled walk-forward kernel — strategy-agnostic, the panel twin of
/// [`crate::spec::optimize::walkforward`].
///
/// The difference that matters is *where selection happens*. Running the
/// single-stream kernel once per instrument fits a different parameter set to
/// each, which is the opposite of pooling. Here each fold ranks the grid by the
/// **pooled** in-sample score, picks **one** winner, and applies it
/// out-of-sample to every member — so the folds are on a shared clock, the
/// hypothesis count is the grid's and not the grid's times the panel's, and the
/// per-member composites all switch parameters on the same dates.
///
/// `probe_readiness` and `run_backtest` are both called with
/// `(params, member_index)`; the member index selects the caller's snapshot
/// stream for that instrument.
///
/// Parallelism follows the single-stream kernel's rule exactly: the readiness
/// pre-scan and the main backtest pass fan out over a **flattened
/// `(row, member)` plan** on one `par_iter` — never a member `par_iter` nested
/// inside a row one, which would fight for the same threads. Folds stay serial
/// (the composite carries running equity across them); rows within a fold are
/// parallel.
#[allow(clippy::too_many_arguments)]
pub fn panel_walkforward<P, R>(
    subgrids: Vec<Subgrid>,
    members: Vec<MemberAxis>,
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
    risk_aversion: Real,
    smooth: Option<&Smoothing>,
    jobs: Option<usize>,
    cash: Real,
) -> Result<PanelWalkForward>
where
    P: Fn(&HashMap<String, Value>, usize) -> Result<usize> + Sync,
    R: Fn(&HashMap<String, Value>, usize) -> Result<crate::RunReport<Symbol>> + Sync,
{
    if subgrids.is_empty() {
        bail!("pooled walkforward: called with zero subgrids");
    }
    if members.is_empty() {
        bail!("pooled walkforward: called with zero panel members");
    }
    if risk_aversion < 0.0 {
        bail!("--risk-aversion must be >= 0 (got {risk_aversion})");
    }
    if smooth.is_some() && best_by.is_none() {
        bail!(
            "--smooth needs --best-by: there is no ranking key to average over the neighbourhood"
        );
    }
    if let Some(cfg) = smooth {
        cfg.scales.validate_against(&subgrids)?;
    }

    let n_members = members.len();
    let union_columns = compute_union_columns(&subgrids);
    let plan: Vec<(usize, usize)> = subgrids
        .iter()
        .enumerate()
        .flat_map(|(si, s)| (0..s.combos.len()).map(move |ci| (si, ci)))
        .collect();
    let params_of = |si: usize, ci: usize| {
        let subgrid = &subgrids[si];
        combine_params(&subgrid.fixed, &subgrid.axes, &subgrid.combos[ci])
    };

    // The flattened `(row, member)` work list — one par_iter, no nesting.
    let work: Vec<(usize, usize)> = (0..plan.len())
        .flat_map(|r| (0..n_members).map(move |m| (r, m)))
        .collect();

    let pool = crate::spec::pool::build_pool(jobs)?;

    // Pre-scan: grid-wide *and* panel-wide max readiness, so every member's
    // warm-up prefix is the same number of its own bars and no fold's metrics
    // depend on which combo happened to settle faster.
    let ready_bars: usize = pool.install(|| {
        work.par_iter()
            .map(|&(r, m)| {
                let (si, ci) = plan[r];
                probe_readiness(&params_of(si, ci), m)
            })
            .try_reduce(|| 0usize, |a, b| Ok(a.max(b)))
    })?;

    let axis = PanelAxis::new(members, ready_bars)?;
    let folds = walkforward_layout(
        axis.len(),
        axis.prefix_skip,
        is_bars,
        oos_bars,
        embargo_bars,
    )
    .with_context(|| {
        format!(
            "pooled walkforward over {n_members} members on a shared clock of {} bars \
                 (the union of every member's, of which {} are the readiness prefix)",
            axis.len(),
            axis.prefix_skip,
        )
    })?;

    // Main pass: one full backtest per (row, member).
    let flat_reports: Vec<crate::RunReport<Symbol>> = pool.install(|| {
        work.par_iter()
            .map(|&(r, m)| {
                let (si, ci) = plan[r];
                run_backtest(&params_of(si, ci), m)
            })
            .collect::<Result<Vec<_>>>()
    })?;
    let report_at = |row: usize, member: usize| &flat_reports[row * n_members + member];

    // Resolve `--metrics` / `--best-by` against a *whole-run* document, as the
    // single-stream kernel does — a fold slice can leave many metrics `None`.
    let sample = metrics::from_report(
        report_at(0, 0),
        bars_per_year,
        risk_free_rate,
        seconds_per_bar,
    );
    let metric_columns: Vec<(String, String)> = if metric_names.is_empty() {
        metrics::flatten(&sample)
            .into_iter()
            .map(|(path, _)| (path.to_string(), path.to_string()))
            .collect()
    } else {
        metric_names
            .iter()
            .map(|name| {
                let (path, _) = metrics::resolve_metric(name, &sample)?;
                Ok::<_, anyhow::Error>((path.clone(), path))
            })
            .collect::<Result<Vec<_>>>()?
    };
    let best_by = best_by
        .map(|name| {
            let (path, _) = metrics::resolve_metric(name, &sample)?;
            let direction = crate::spec::optimize::direction_for(&path).ok_or_else(|| {
                anyhow::anyhow!(
                    "--best-by `{name}` has no built-in direction; pass one whose \
                     direction is known (e.g. sharpe, sortino, cagr_pct, max_pct, \
                     ulcer_index, annualized_volatility_pct)"
                )
            })?;
            Ok::<_, anyhow::Error>((path.clone(), path, direction))
        })
        .transpose()?;

    // Measure one (row, member) pair over one pooled range. A member with no
    // bars in the range yields `None` and is simply absent from the row's
    // result — never a zero-filled placeholder, which is what would make
    // `Pooled::defined` a lie.
    let measure_one = |row: usize, m: usize, pooled: Range<usize>| -> Option<PanelMetrics> {
        let range = axis.member_range(m, pooled);
        if range.is_empty() {
            return None;
        }
        Some(PanelMetrics {
            member: axis.members[m].name.clone(),
            metrics: metrics::from_report(
                &metrics::report_slice(report_at(row, m), range),
                bars_per_year,
                risk_free_rate,
                seconds_per_bar,
            ),
        })
    };
    let measure = |row: usize, pooled: Range<usize>| -> Vec<PanelMetrics> {
        (0..n_members)
            .filter_map(|m| measure_one(row, m, pooled.clone()))
            .collect()
    };

    let mut fold_rows: Vec<PanelFoldRow> = Vec::with_capacity(folds.len());
    let mut equity: Vec<Vec<Real>> = vec![Vec::new(); n_members];
    let mut fills: Vec<Vec<crate::Fill<Symbol>>> = vec![Vec::new(); n_members];
    let mut rejections: Vec<Vec<crate::Rejected<Symbol>>> = vec![Vec::new(); n_members];
    let mut running: Vec<Real> = vec![cash; n_members];

    for (fold_idx, fold) in folds.iter().enumerate() {
        // The pooled IS documents — what selection reads.
        //
        // Fanned out over the flattened `(row, member)` work list, not over
        // rows with a serial member loop inside. Both keep every worker busy on
        // a wide grid, but only this one does when the grid is *narrower than
        // the machine* — a 4-point grid over a 30-member panel is 120 units of
        // independent work, and measuring it 4-wide would idle most of the box.
        // A flat list is also better balanced than a nested `par_iter` would
        // be, and costs no nested-scheduling overhead.
        //
        // Regrouped by row afterwards: `per_row_is` must stay in `plan` order —
        // subgrid-major, then combo order — because that is the layout
        // `smooth_keys` reads its lattices out of.
        let measured: Vec<Option<PanelMetrics>> = pool.install(|| {
            work.par_iter()
                .map(|&(r, m)| measure_one(r, m, fold.is.clone()))
                .collect()
        });
        let per_row_is: Vec<Vec<PanelMetrics>> = measured
            .chunks(n_members)
            .map(|row| row.iter().flatten().cloned().collect())
            .collect();

        let mut fold_smoothed: Option<Vec<SmoothedKey>> = None;
        let mut winner_smoothed: Option<SmoothedKey> = None;
        let winner: usize = match &best_by {
            Some((_, path, direction)) => {
                let keys: Vec<Option<Real>> = per_row_is
                    .iter()
                    .map(|ms| pooled_ranking_key(ms, path, *direction, risk_aversion))
                    .collect();
                let ranked: Vec<Option<Real>> = match smooth {
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

        let oos_members = measure(winner, fold.oos.clone());

        // Stitch the winner's OOS slice onto each member's own composite. Each
        // member carries its own running equity, so a fold in which a member had
        // no bars leaves that member's curve untouched rather than flat-filling
        // it — a gap in coverage, not a run of zero returns.
        for (m, range) in axis.member_ranges(fold.oos.clone()).into_iter().enumerate() {
            if range.is_empty() {
                continue;
            }
            let slice = metrics::report_slice(report_at(winner, m), range);
            let scale = if slice.initial_equity > 0.0 {
                running[m] / slice.initial_equity
            } else {
                1.0
            };
            let offset = equity[m].len();
            equity[m].extend(slice.equity_curve.iter().map(|e| e * scale));
            fills[m].extend(slice.fills.into_iter().map(|f| crate::Fill {
                bar: f.bar + offset,
                order: f.order,
            }));
            rejections[m].extend(slice.rejections.into_iter().map(|r| crate::Rejected {
                bar: r.bar + offset,
                rejection: r.rejection,
            }));
            running[m] = equity[m].last().copied().unwrap_or(running[m]);
        }

        let (si, ci) = plan[winner];
        fold_rows.push(PanelFoldRow {
            fold: fold_idx,
            is: fold.is.clone(),
            oos: fold.oos.clone(),
            values: project_row(&subgrids[si], &subgrids[si].combos[ci], &union_columns),
            is_members: per_row_is[winner].clone(),
            oos_members,
            is_smoothed: winner_smoothed,
        });
    }

    let composites: Vec<MemberComposite> = (0..n_members)
        .map(|m| {
            let report = crate::RunReport {
                ruin_bar: equity[m].iter().position(|&e| e <= 0.0),
                equity_curve: equity[m].clone(),
                fills: fills[m].clone(),
                rejections: rejections[m].clone(),
                initial_equity: cash,
                carry_coverage: None,
            };
            MemberComposite {
                member: axis.members[m].name.clone(),
                metrics: metrics::from_report(
                    &report,
                    bars_per_year,
                    risk_free_rate,
                    seconds_per_bar,
                ),
                equity: report.equity_curve,
                fills: report.fills,
                rejections: report.rejections,
            }
        })
        .collect();

    Ok(PanelWalkForward {
        union_columns,
        metric_columns,
        best_by,
        axis,
        folds,
        fold_rows,
        composites,
        is_bars,
        oos_bars,
        embargo_bars,
        cash,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn axis(name: &str, keys: &[i64]) -> MemberAxis {
        MemberAxis::from_keys(name, keys.to_vec()).expect("ascending")
    }

    #[test]
    fn union_axis_merges_ragged_members() {
        let a = axis("A", &[1, 2, 3, 4]);
        let b = axis("B", &[3, 4, 5]);
        let panel = PanelAxis::new(vec![a, b], 0).unwrap();
        assert_eq!(panel.keys, vec![1, 2, 3, 4, 5]);
        // Earliest ready member is A at key 1, so nothing is skipped.
        assert_eq!(panel.prefix_skip, 0);
    }

    #[test]
    fn prefix_skip_follows_the_first_ready_member_not_the_last() {
        // A lists early and is ready at its 2nd bar (key 3); B lists late and is
        // ready at key 30. Waiting for B would discard A's whole history.
        let a = axis("A", &[1, 2, 3, 4, 10, 20, 30]);
        let b = axis("B", &[20, 30, 40]);
        let panel = PanelAxis::new(vec![a, b], 2).unwrap();
        assert_eq!(panel.keys, vec![1, 2, 3, 4, 10, 20, 30, 40]);
        // A is ready at keys[2] == 3, which is pooled index 2.
        assert_eq!(panel.prefix_skip, 2);
    }

    #[test]
    fn a_member_absent_from_a_window_contributes_an_empty_range() {
        let a = axis("A", &[1, 2, 3, 4]);
        let b = axis("B", &[10, 11, 12]);
        let panel = PanelAxis::new(vec![a, b], 0).unwrap();
        // keys == [1,2,3,4,10,11,12]; pooled 0..4 is entirely A's.
        assert_eq!(panel.member_range(0, 0..4), 0..4);
        assert_eq!(panel.member_range(1, 0..4), 0..0);
        assert_eq!(panel.support(0..4), 1);
        // ...and pooled 4..7 is entirely B's.
        assert_eq!(panel.member_range(0, 4..7), 0..0);
        assert_eq!(panel.member_range(1, 4..7), 0..3);
        assert_eq!(panel.support(4..7), 1);
    }

    #[test]
    fn member_range_never_precedes_that_members_readiness() {
        let a = axis("A", &[1, 2, 3, 4, 5]);
        let panel = PanelAxis::new(vec![a], 3).unwrap();
        // The pooled range covers bars 0..5, but the first three are warm-up.
        assert_eq!(panel.member_range(0, 0..5), 3..5);
    }

    #[test]
    fn fold_boundaries_are_shared_even_when_cadences_differ() {
        // A quotes twice as often as B. A pooled range still means one span.
        let a = axis("A", &[0, 10, 20, 30, 40, 50]);
        let b = axis("B", &[0, 20, 40]);
        let panel = PanelAxis::new(vec![a, b], 0).unwrap();
        assert_eq!(panel.keys, vec![0, 10, 20, 30, 40, 50]);
        // Pooled 0..4 is keys [0,10,20,30] — i.e. everything before key 40.
        assert_eq!(panel.member_range(0, 0..4), 0..4);
        assert_eq!(panel.member_range(1, 0..4), 0..2); // B's bars at 0 and 20
        assert_eq!(panel.support(0..4), 2);
    }

    /// A member whose curve is `equity`, reduced through the real catalogue.
    fn member(name: &str, equity: Vec<Real>) -> PanelMetrics {
        let report: crate::RunReport<Symbol> = crate::RunReport {
            equity_curve: equity,
            fills: vec![],
            rejections: Vec::new(),
            initial_equity: 100.0,
            ruin_bar: None,
            carry_coverage: None,
        };
        PanelMetrics {
            member: name.to_string(),
            metrics: metrics::from_report(&report, 252.0, 0.0, None),
        }
    }

    #[test]
    fn undefined_stays_undefined_and_support_is_reported() {
        // A and B both have a return series, so both report a Sharpe. C is a
        // single bar — no dispersion to divide by, so it reports none at all.
        let panel = vec![
            member("A", vec![110.0, 110.0]),
            member("B", vec![130.0, 130.0]),
            member("C", vec![100.0]),
        ];
        let sharpe =
            |m: &PanelMetrics| crate::spec::optimize::lookup(&m.metrics, "risk_adjusted.sharpe");
        let (a, b) = (sharpe(&panel[0]).unwrap(), sharpe(&panel[1]).unwrap());
        assert_eq!(sharpe(&panel[2]), None, "C must not report a Sharpe");

        let pooled = pool_metric(&panel, "risk_adjusted.sharpe").expect("two members reported");
        // The mean is over the two that reported...
        assert!((pooled.mean - (a + b) / 2.0).abs() < 1e-12, "{pooled:?}");
        // ...and emphatically *not* over three with a zero substituted for C.
        assert!((pooled.mean - (a + b) / 3.0).abs() > 1e-9, "{pooled:?}");
        // A mean over 2 of 3 and a mean over 3 of 3 must not read identically.
        assert_eq!(pooled.defined, 2);
        assert_eq!(pooled.members, 3);
        assert!((pooled.support() - 2.0 / 3.0).abs() < 1e-9);
    }

    #[test]
    fn a_metric_no_member_reports_pools_to_none() {
        // A single flat bar reports no Sharpe, so the panel reports none.
        let panel = vec![member("A", vec![100.0])];
        assert!(pool_metric(&panel, "risk_adjusted.sharpe").is_none());
    }

    #[test]
    fn a_ruined_member_is_found_rather_than_dropped_from_the_mean() {
        let mut dead = member("B", vec![100.0, 0.0]);
        dead.metrics.run.ruin_bar = Some(1);
        let panel = vec![member("A", vec![110.0, 110.0]), dead];
        // The pooled mean still averages what it can — the number is kept.
        assert!(pool_metric(&panel, "returns.total_pct").is_some());
        // ...but the row is disqualified, so nothing selects on it.
        assert_eq!(ruined_member(&panel).map(|m| m.member.as_str()), Some("B"));
    }

    #[test]
    fn non_ascending_member_is_refused() {
        let err = MemberAxis::from_keys("A", vec![1, 3, 2]).unwrap_err();
        assert!(err.to_string().contains("strictly ascending"), "{err}");
    }

    #[test]
    fn a_panel_no_member_is_ready_for_is_refused() {
        let a = axis("A", &[1, 2]);
        let err = PanelAxis::new(vec![a], 10).unwrap_err();
        assert!(err.to_string().contains("readiness"), "{err}");
    }
}
