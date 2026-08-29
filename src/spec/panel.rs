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

use std::collections::{BTreeSet, HashMap};
use std::ops::Range;

use anyhow::{Context, Result, bail};
use serde_json::Value;

use crate::market::Real;
use crate::spec::metrics::{self, Metrics};
use crate::spec::optimize::{cartesian, format_value, split_axes};
use crate::spec::shrinkage;
use crate::types::{Snapshot, Symbol};

// ---------------------------------------------------------------------------
// What a panel is made of
// ---------------------------------------------------------------------------

/// One member of a panel: the substitutions that produce it, and the label
/// everything downstream keys it by.
#[derive(Clone, Debug, PartialEq)]
pub struct Member {
    /// `NAME=VALUE` per axis, `,`-joined. Deliberately the `--pooled`/`--params`
    /// spec that reproduces this member on its own, so a pooled `metrics.yml`
    /// key is copy-pasteable back into a plain `run` — which is the first thing
    /// anyone does with the member that dragged a pooled mean down.
    ///
    /// It is a label, not a parse target: a member value containing `,` or `=`
    /// renders literally and does not round-trip. Nothing reads it back.
    pub label: String,
    /// This member's value for each pooled axis, in axis order (name-sorted).
    pub values: Vec<(String, Value)>,
}

impl Member {
    /// Layer this member's values over a params table, in place.
    pub fn apply(&self, params: &mut HashMap<String, Value>) {
        for (name, value) in &self.values {
            params.insert(name.clone(), value.clone());
        }
    }

    /// `params` with this member layered over it.
    pub fn params_over(&self, params: &HashMap<String, Value>) -> HashMap<String, Value> {
        let mut out = params.clone();
        self.apply(&mut out);
        out
    }
}

/// The `--pooled` panel: N parameter axes, reduced over their **cartesian
/// product**.
///
/// One axis is the common case — a list of instruments. Several is the same
/// reduction over a product rather than a different mechanism:
/// `SYMBOL=[...],FREQ=[...]` asks whether an edge survives across instruments
/// *and* cadences, which is one question with one answer, not a grid of them.
/// Every cell of the product is a member, and members are averaged, never
/// ranked against each other.
///
/// The axes carry their own values (`--pooled 'SYM=["BTC","ETH"]'`) rather than
/// naming an entry declared elsewhere. That is what makes "every member of the
/// panel is the same population for every row" true by construction instead of
/// by a check — and it keeps `--params` meaning *this name equals this value*
/// in every subcommand, which naming a `--params` entry did not.
#[derive(Clone, Debug)]
pub struct Panel {
    axes: Vec<(String, Vec<Value>)>,
    members: Vec<Member>,
}

impl Panel {
    /// Build a panel from a `--pooled` params table.
    ///
    /// Every entry has to be axis-shaped (a `[...]` list or a
    /// `start..end[:step]` range): a scalar term is the one thing this flag
    /// cannot mean, since reducing over a single value is a plain run. Axes come
    /// out name-sorted (from [`split_axes`]) and the product is enumerated with
    /// the last axis varying fastest, so member order never depends on how the
    /// flag was typed.
    pub fn from_params(table: &HashMap<String, Value>) -> Result<Self> {
        let (fixed, axes) = split_axes(table).context("--pooled")?;
        if !fixed.is_empty() {
            let mut names: Vec<&str> = fixed.keys().map(String::as_str).collect();
            names.sort_unstable();
            bail!(
                "--pooled takes axes, not single values: {}. A pooled axis is a `[...]` list \
                 or a `start..end[:step]` range — reducing over one value is what plain \
                 `--params` already does",
                names.join(", "),
            );
        }
        if axes.is_empty() {
            bail!(
                "--pooled declares no axis — pass the panel's members, e.g. \
                 --pooled 'SYMBOL=[\"BTCUSDT\",\"ETHUSDT\"]'"
            );
        }
        let members: Vec<Member> = cartesian(&axes)
            .into_iter()
            .map(|combo| {
                let values: Vec<(String, Value)> = axes
                    .iter()
                    .map(|(name, _)| name.clone())
                    .zip(combo)
                    .collect();
                Member {
                    label: values
                        .iter()
                        .map(|(name, value)| format!("{name}={}", format_value(value)))
                        .collect::<Vec<_>>()
                        .join(","),
                    values,
                }
            })
            .collect();
        if members.len() < 2 {
            bail!(
                "--pooled has only {} member — pooling reduces across a panel, and a panel \
                 of one is just a plain run with an extra layer of indirection",
                members.len(),
            );
        }
        Ok(Self { axes, members })
    }

    /// The pooled axes, name-sorted. Each is a name and its declared values.
    pub fn axes(&self) -> &[(String, Vec<Value>)] {
        &self.axes
    }

    /// The names this panel substitutes — what may not collide with a
    /// `--params` scalar or a `--grid` axis.
    pub fn names(&self) -> impl Iterator<Item = &str> {
        self.axes.iter().map(|(name, _)| name.as_str())
    }

    /// The panel's members, in product order.
    pub fn members(&self) -> &[Member] {
        &self.members
    }

    pub fn len(&self) -> usize {
        self.members.len()
    }

    /// Always `false` — [`Self::from_params`] refuses a panel under two members.
    /// Present because clippy asks for it beside [`Self::len`].
    pub fn is_empty(&self) -> bool {
        self.members.is_empty()
    }

    /// The axes as the console reports them: `` `SYMBOL` (3) \u{00b7} `FREQ` (2) ``.
    /// A single axis renders as just its name — the member count is already
    /// printed beside it, and `` `SYM` (3) `` over 3 members says it twice.
    pub fn describe(&self) -> String {
        if let [(name, _)] = self.axes.as_slice() {
            return format!("`{name}`");
        }
        self.axes
            .iter()
            .map(|(name, values)| format!("`{name}` ({})", values.len()))
            .collect::<Vec<_>>()
            .join(" \u{00b7} ")
    }
}

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
    /// This member's run cut into non-overlapping windows, when the caller
    /// asked for one (`-w/--windowed`). Empty otherwise.
    ///
    /// **Nothing in the pooled reduction reads this.** `pool_metric` and every
    /// `_mean`/`_std`/`_n` column go through [`Self::metrics`] exactly as
    /// before, so adding windows to a panel changes no pooled number. It exists
    /// for [`crate::spec::shrinkage`], which needs *within-cell* replication to
    /// separate "the members disagree" from "the backtests are noisy" — and
    /// with one observation per member those two are the same quantity.
    pub windows: Vec<metrics::WindowMetrics>,
}

impl PanelMetrics {
    /// A member's whole-run document, with no windowed replicates.
    pub fn new(member: impl Into<String>, metrics: Metrics) -> Self {
        Self {
            member: member.into(),
            metrics,
            windows: Vec::new(),
        }
    }

    /// The same, carrying the windowed reduction of the *same* run.
    pub fn with_windows(
        member: impl Into<String>,
        metrics: Metrics,
        windows: Vec<metrics::WindowMetrics>,
    ) -> Self {
        Self {
            member: member.into(),
            metrics,
            windows,
        }
    }

    /// Whether this member's account was ruined over the measured span.
    pub fn is_ruined(&self) -> bool {
        self.metrics.run.ruin_bar.is_some()
    }

    /// This member's readings of one metric across its windows — the replicate
    /// observations for its cell of a [`crate::spec::shrinkage::ScoreTable`].
    ///
    /// Windows that could not compute the metric are dropped rather than
    /// zero-filled, the same `filter_map` contract [`pool_metric`] keeps.
    pub fn window_values<'a>(&'a self, path: &'a str) -> impl Iterator<Item = Real> + 'a {
        self.windows
            .iter()
            .filter_map(move |w| crate::spec::optimize::lookup(&w.metrics, path))
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

// ---------------------------------------------------------------------------
// The score table
// ---------------------------------------------------------------------------

/// Lay a pooled sweep out as the row × member table
/// [`crate::spec::shrinkage`] decomposes.
///
/// `rows` is one entry per grid point, each holding that point's per-member
/// documents. `members` is the panel's member labels **in panel order**, which
/// is what gives every row the same column layout — a row's documents only
/// cover the members that reported, so they cannot index the table by
/// themselves.
///
/// Each cell takes that member's windowed replicates when it has them
/// ([`PanelMetrics::windows`], populated under `-w`) and falls back to its
/// single whole-span reading otherwise. A table built entirely from the
/// fallback is unreplicated, and [`crate::spec::shrinkage::Decomposition::lambda`]
/// will be `None` for it — which is the honest answer, not a shortfall.
/// Every member label appearing in a pooled sweep, in first-appearance order.
///
/// The panel's own order where the caller has it; derived here because
/// [`crate::spec::optimize::optimize`] is handed evaluations, not a [`Panel`],
/// and a row's documents cover only the members that *reported* — so no single
/// row can be trusted to name the whole panel.
pub fn member_labels(rows: &[&[PanelMetrics]]) -> Vec<String> {
    let mut seen: Vec<String> = Vec::new();
    for row in rows {
        for doc in *row {
            if !seen.iter().any(|n| n == &doc.member) {
                seen.push(doc.member.clone());
            }
        }
    }
    seen
}

/// The whole sweep-level reading: each row's member-demeaned pooled score, and
/// the decomposition it came from.
///
/// The demeaned score is what makes a cross-member spread mean *"this parameter
/// set ranks consistently well"* rather than *"these instruments are alike"* —
/// the member effect is identical for every row and therefore carries no
/// ranking information, while still inflating every row's `−k·std` penalty, and
/// inflating it unequally because rows differ in which members they are defined
/// on.
///
/// `None` when the table cannot be fitted (see
/// [`crate::spec::shrinkage::MIN_CELLS`]). Note this is *not* the same
/// condition as `λ` being available: demeaning needs only the table, while `λ`
/// additionally needs within-cell replication.
pub fn demeaned_sweep(rows: &[&[PanelMetrics]], path: &str) -> Option<SweepShrinkage> {
    analyse_sweep(rows, path, None)
}

/// Everything a pooled sweep can learn from one fit of its score table.
///
/// One struct rather than three entry points because all of it comes off the
/// same decomposition, and building the table three times to answer three
/// questions about it would be both slower and a chance for the three answers
/// to disagree.
pub struct SweepShrinkage {
    /// Each row's member-demeaned pooled score, in row order.
    pub demeaned: Vec<Option<Pooled>>,
    /// The decomposition's headline scalars.
    pub summary: shrinkage::Summary,
    /// Member labels, in the column order the fit used.
    pub members: Vec<String>,
    /// Under a direction, each member's own argmax over the shrunk surface —
    /// `(member, row index)`. Empty when no direction was given, or when `λ` is
    /// unavailable and there is therefore no defensible surface to select off.
    pub member_winners: Vec<(String, usize)>,
    /// How many **independent searches over the grid** those selections amount
    /// to — see [`selection_breadth`]. `None` alongside an empty
    /// `member_winners`.
    pub selection: Option<Breadth>,
}

/// [`demeaned_sweep`] plus, when a ranking direction is supplied, the per-member
/// selections and the search count they imply.
///
/// The direction is optional because the demeaned columns are a readout every
/// pooled sweep gets, while selecting per member is what `--shrink` asks for.
pub fn analyse_sweep(
    rows: &[&[PanelMetrics]],
    path: &str,
    direction: Option<Direction>,
) -> Option<SweepShrinkage> {
    let members = member_labels(rows);
    if members.len() < 2 {
        return None;
    }
    let table = score_table_of(rows, &members, path);
    let decomposition = table.decompose()?;
    let cells = decomposition.demeaned(&table);
    let demeaned = (0..rows.len())
        .map(|r| {
            let values: Vec<Real> = (0..members.len())
                .filter_map(|m| cells[r * members.len() + m])
                .collect();
            let (mean, std) = metrics::mean_std(values.iter().copied())?;
            Some(Pooled {
                mean,
                std,
                defined: values.len(),
                members: members.len(),
            })
        })
        .collect();

    // Per-member selection, only when asked and only when `λ` exists. Without a
    // `λ` there is no shrunk surface — falling back to either pooling extreme
    // would be choosing a pooling policy by accident, which is the one thing
    // this module refuses to do.
    let mut member_winners = Vec::new();
    let mut selection = None;
    if let Some(direction) = direction
        && let Some(surface) = decomposition.shrunk(&table)
    {
        for (m, name) in members.iter().enumerate() {
            let column: Vec<Option<Real>> = (0..table.rows())
                .map(|r| surface[r * members.len() + m])
                .collect();
            if let Some(idx) = argbest(&column, direction) {
                member_winners.push((name.clone(), idx));
            }
        }
        selection = selection_breadth(&decomposition, &table);
    }

    Some(SweepShrinkage {
        demeaned,
        summary: decomposition.summary(&table),
        members,
        member_winners,
        selection,
    })
}

/// [`score_table`] over borrowed rows — what a sweep has, since its per-row
/// documents live inside an [`crate::spec::optimize::Evaluation`].
pub fn score_table_of(
    rows: &[&[PanelMetrics]],
    members: &[String],
    path: &str,
) -> crate::spec::shrinkage::ScoreTable {
    let index: HashMap<&str, usize> = members
        .iter()
        .enumerate()
        .map(|(i, name)| (name.as_str(), i))
        .collect();
    let mut table = crate::spec::shrinkage::ScoreTable::new(rows.len(), members.len());
    for (r, row) in rows.iter().enumerate() {
        for doc in *row {
            let Some(&m) = index.get(doc.member.as_str()) else {
                continue;
            };
            let mut any = false;
            for v in doc.window_values(path) {
                table.push(r, m, v);
                any = true;
            }
            if !any && let Some(v) = crate::spec::optimize::lookup(&doc.metrics, path) {
                table.push(r, m, v);
            }
        }
    }
    table
}

pub fn score_table(
    rows: &[Vec<PanelMetrics>],
    members: &[String],
    path: &str,
) -> crate::spec::shrinkage::ScoreTable {
    let borrowed: Vec<&[PanelMetrics]> = rows.iter().map(Vec::as_slice).collect();
    score_table_of(&borrowed, members, path)
}

// ---------------------------------------------------------------------------
// Effective breadth
// ---------------------------------------------------------------------------

/// Below this many shared bars a pair reports no correlation at all.
///
/// A coefficient over a handful of points is not a weak reading, it is noise —
/// and here it would propagate straight into a headline that claims to say how
/// much evidence a panel holds. The same floor `pool_metric`'s "defined vs
/// members" split draws for a metric, drawn for a pair.
pub const MIN_SHARED_BARS: usize = 30;

/// How many *independent* members a panel is actually worth.
///
/// A pooled row reports `N` hypotheses instead of `N×M`, which is the honest
/// count — but it invites the reading that `M` members are `M` pieces of
/// evidence, and for a panel of one market's worth of instruments they are not.
/// Thirty crypto pairs that all track the same beta are worth about one; a
/// pooled Sharpe over them deserves roughly the confidence of a single
/// backtest, not thirty.
///
/// The reading is the standard one for an equal-weighted mean of `M`
/// estimators with average pairwise correlation `ρ̄`:
///
/// ```text
/// effective = M / (1 + (M − 1)·ρ̄)
/// ```
///
/// It is deliberately a *reported* number rather than a correction applied to
/// anything. What a caller does with it — deflate against it, widen an
/// interval, or go and find less correlated members — is a decision this crate
/// has no basis to make for them.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Breadth {
    /// Members with enough history to be correlated against anything.
    pub members: usize,
    /// Pairs that cleared [`MIN_SHARED_BARS`] and were actually measured.
    pub pairs: usize,
    /// Mean pairwise Pearson correlation over those pairs.
    pub mean_correlation: Real,
    /// `members / (1 + (members − 1)·mean_correlation)`.
    pub effective: Real,
}

/// Pearson correlation of two equal-length samples, or `None` when either is
/// constant — a flat series has no variance to share, which is a different
/// answer from "uncorrelated" and must not be averaged in as zero.
fn pearson(xs: &[Real], ys: &[Real]) -> Option<Real> {
    let n = xs.len().min(ys.len());
    if n < 2 {
        return None;
    }
    let mean_x = xs[..n].iter().sum::<Real>() / n as Real;
    let mean_y = ys[..n].iter().sum::<Real>() / n as Real;
    let (mut sxy, mut sxx, mut syy) = (0.0, 0.0, 0.0);
    for i in 0..n {
        let (dx, dy) = (xs[i] - mean_x, ys[i] - mean_y);
        sxy += dx * dy;
        sxx += dx * dx;
        syy += dy * dy;
    }
    let denominator = (sxx * syy).sqrt();
    if !denominator.is_finite() || denominator <= 0.0 {
        return None;
    }
    let r = sxy / denominator;
    r.is_finite().then_some(r.clamp(-1.0, 1.0))
}

/// The effective breadth of a panel, from each member's `(bar keys, per-bar
/// values)`.
///
/// **Each pair is joined on its own shared keys**, never on a global
/// intersection: a member that listed last week overlaps the rest by a
/// fortnight, and intersecting everything first would collapse the axis every
/// other pair is measured on down to that. The same rule
/// [`PanelAxis::member_range`] follows for folds, applied to correlation.
///
/// `None` when fewer than two members clear [`MIN_SHARED_BARS`] against
/// anything, or when no pair could be measured. Reporting the member count
/// there would be answering a question nobody could check.
///
/// A **negative** mean correlation is floored at zero rather than allowed
/// through. Such a panel really does carry more independent information than
/// its member count, but the denominator crosses zero on the way to saying so
/// and reports an infinite breadth, which is a claim no panel supports.
pub fn effective_breadth(members: &[(&[i64], &[Real])]) -> Option<Breadth> {
    let usable: Vec<&(&[i64], &[Real])> = members
        .iter()
        .filter(|(keys, values)| keys.len().min(values.len()) >= MIN_SHARED_BARS)
        .collect();
    if usable.len() < 2 {
        return None;
    }
    let mut coefficients: Vec<Real> = Vec::new();
    for (i, a) in usable.iter().enumerate() {
        for b in usable.iter().skip(i + 1) {
            let (xs, ys) = shared(a, b);
            if xs.len() < MIN_SHARED_BARS {
                continue;
            }
            if let Some(r) = pearson(&xs, &ys) {
                coefficients.push(r);
            }
        }
    }
    if coefficients.is_empty() {
        return None;
    }
    let m = usable.len();
    let mean_correlation = coefficients.iter().sum::<Real>() / coefficients.len() as Real;
    let denominator = 1.0 + (m as Real - 1.0) * mean_correlation.max(0.0);
    Some(Breadth {
        members: m,
        pairs: coefficients.len(),
        mean_correlation,
        effective: m as Real / denominator,
    })
}

/// How many *independent searches over the grid* a shrunk panel actually ran.
///
/// This is the number a [deflated Sharpe](crate::metrics::deflated_sharpe_from_stats)
/// has to be parameterized by once each member selects for itself, and it is
/// the question that kept partial pooling out of the plain sweep: complete
/// pooling searches the grid **once** (`M` trials), no pooling searches it once
/// **per member** (`M·K`), and partial pooling is somewhere between with no
/// obvious place to stand.
///
/// It turns out to need no new theory. Under `--shrink` member `m` ranks on
/// `μ + α_r + λ·γ_rm` — a shared term every member has and a private term only
/// it has — so two members' *ranking vectors* are correlated exactly to the
/// extent that the shared term dominates. That is the same "M correlated
/// estimators are worth `M / (1 + (M−1)·ρ̄)` independent ones" reading
/// [`effective_breadth`] already applies to member returns, applied instead to
/// the surfaces those members select off.
///
/// Measured rather than derived: the correlation is taken over the columns of
/// the surface as it actually came out, so no orthogonality is assumed of the
/// fit and a ragged table needs no special case. The limits come out right by
/// construction —
///
/// * `λ = 0` — every column is `μ + α_r`, identical, `ρ̄ = 1`, and this is `1`.
///   One search, `M` trials, exactly the count complete pooling reports today.
/// * `λ = 1` with nothing shared — the columns are unrelated, `ρ̄ = 0`, and this
///   is `K`. `M·K` trials, exactly the count an ordinary `SYM=[...]` grid axis
///   deserves.
/// * `λ = 1` but a dominant shared effect — still near `1`, which is right:
///   every member picks the same row anyway, so only one search happened.
///
/// A negative mean correlation is floored at zero, for the reason
/// [`effective_breadth`] gives: the denominator crosses zero on the way to
/// claiming more independence than a panel can support. `None` when the surface
/// has fewer than two usable columns or no column varies — with nothing to
/// correlate, this would be a number about nothing.
pub fn selection_breadth(
    decomposition: &shrinkage::Decomposition,
    table: &shrinkage::ScoreTable,
) -> Option<Breadth> {
    let surface = decomposition.shrunk(table)?;
    let members = table.members();
    // Each member's ranking vector, keyed by row index so two columns defined
    // on different subsets of the grid still line up. Rows where a member has
    // no score are simply absent from its vector, exactly as a member that had
    // not listed is absent from a returns series.
    let columns: Vec<(Vec<i64>, Vec<Real>)> = (0..members)
        .map(|m| {
            (0..table.rows())
                .filter_map(|r| surface[r * members + m].map(|v| (r as i64, v)))
                .unzip()
        })
        .collect();
    let borrowed: Vec<(&[i64], &[Real])> = columns
        .iter()
        .map(|(k, v)| (k.as_slice(), v.as_slice()))
        .collect();
    // The grid is not a clock, so the `MIN_SHARED_BARS` floor `effective_breadth`
    // applies to bar keys would be measuring the wrong thing here — a four-point
    // grid is a legitimate sweep. Correlate directly, on the same pairwise
    // "shared keys" rule.
    let usable: Vec<&(&[i64], &[Real])> = borrowed.iter().filter(|(k, _)| k.len() >= 2).collect();
    if usable.len() < 2 {
        return None;
    }
    let mut coefficients: Vec<Real> = Vec::new();
    for (i, a) in usable.iter().enumerate() {
        for b in usable.iter().skip(i + 1) {
            let (xs, ys) = shared(a, b);
            if xs.len() < 2 {
                continue;
            }
            if let Some(r) = pearson(&xs, &ys) {
                coefficients.push(r);
            }
        }
    }
    if coefficients.is_empty() {
        return None;
    }
    let k = usable.len();
    let mean_correlation = coefficients.iter().sum::<Real>() / coefficients.len() as Real;
    let denominator = 1.0 + (k as Real - 1.0) * mean_correlation.max(0.0);
    Some(Breadth {
        members: k,
        pairs: coefficients.len(),
        mean_correlation,
        effective: k as Real / denominator,
    })
}

/// The two members' values at the bar keys they share, in key order. Both
/// members' keys are strictly ascending ([`MemberAxis::from_keys`] refuses
/// otherwise), so this is a merge rather than a lookup table.
fn shared(a: &(&[i64], &[Real]), b: &(&[i64], &[Real])) -> (Vec<Real>, Vec<Real>) {
    let (mut i, mut j) = (0usize, 0usize);
    let (mut xs, mut ys) = (Vec::new(), Vec::new());
    let (an, bn) = (a.0.len().min(a.1.len()), b.0.len().min(b.1.len()));
    while i < an && j < bn {
        match a.0[i].cmp(&b.0[j]) {
            std::cmp::Ordering::Less => i += 1,
            std::cmp::Ordering::Greater => j += 1,
            std::cmp::Ordering::Equal => {
                xs.push(a.1[i]);
                ys.push(b.1[j]);
                i += 1;
                j += 1;
            }
        }
    }
    (xs, ys)
}

/// The `{pooled: {...}, members: {...}}` document a pooled `metrics.yml`
/// writes: every metric path's cross-member `mean`/`std`/`defined`/`members`,
/// plus every member's own whole metrics document keyed by name — the same
/// pair [`PanelMetrics`] carries, just serialized.
///
/// Shared by `optimize`'s pooled-walkforward composite writer and `fugazi run
/// --pooled`'s pooled `metrics.yml` — both reduce the same `PanelMetrics`
/// slice to the same on-disk shape, so a reader who has seen one already
/// knows the other.
pub fn pooled_document(members: &[PanelMetrics]) -> serde_json::Value {
    let mut pooled = serde_json::Map::new();
    if let Some(sample) = members.first() {
        for (metric_path, _) in metrics::flatten(&sample.metrics) {
            if let Some(p) = pool_metric(members, metric_path) {
                pooled.insert(
                    metric_path.to_string(),
                    serde_json::json!({
                        "mean": p.mean,
                        "std": p.std,
                        "defined": p.defined,
                        "members": p.members,
                    }),
                );
            }
        }
    }
    serde_json::json!({
        "pooled": pooled,
        "members": members
            .iter()
            .map(|m| (m.member.clone(), serde_json::to_value(&m.metrics).unwrap_or_default()))
            .collect::<serde_json::Map<_, _>>(),
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
    /// Under `--shrink`, this fold's two-way decomposition of the in-sample
    /// score table — estimated from sub-spans of **this fold's** IS window, so
    /// it rests only on data the fold could see.
    pub shrinkage: Option<shrinkage::Summary>,
    /// Under `--shrink`, the row each member selected off the shrunk surface.
    /// Empty otherwise, which is complete pooling: every member took
    /// [`Self::values`].
    pub member_winners: Vec<MemberWinner>,
}

/// One member's own choice for a fold, under partial pooling.
///
/// At `λ = 0` every entry names the pooled winner and this is a more verbose
/// spelling of complete pooling. At `λ = 1` each names that member's own
/// argmax. The interesting cases are in between, and they are the reason this
/// is recorded per member rather than summarized: "the panel mostly agreed
/// except for one member" is a different finding from "every member went its
/// own way", and a mean `λ` renders them identically.
#[derive(Clone, Debug)]
pub struct MemberWinner {
    pub member: String,
    /// Index into the fold's row plan — the same index space as
    /// [`PanelFoldRow::values`]'s row.
    pub row: usize,
    /// This member's winning parameters, projected onto the union columns.
    pub values: Vec<Option<Value>>,
    /// Whether this member departed from the pooled winner.
    pub departed: bool,
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
    /// The panel's `λ` over the whole run, with **folds** as the replicate axis
    /// — one observation per `(grid row, member, fold)`.
    ///
    /// Free, and therefore reported without `--shrink`: every fold already
    /// measures every `(row, member)` in-sample to rank the grid, so this
    /// accumulates readings that were taken anyway rather than taking more.
    ///
    /// It is deliberately **not** what selection uses. A component estimated
    /// over every fold and then applied inside fold 1 would let fold 10's data
    /// pick fold 1's winner. This describes the run after the fact;
    /// [`PanelFoldRow::shrinkage`] is the lookahead-free estimate each fold
    /// actually acted on.
    ///
    /// `None` without `--best-by` (no metric to build a table over) or when the
    /// table is too sparse to fit.
    pub run_shrinkage: Option<shrinkage::Summary>,
}

impl PanelWalkForward {
    /// The composites as poolable documents — what
    /// [`pool_metric`] reduces to get the headline out-of-sample numbers.
    pub fn composite_members(&self) -> Vec<PanelMetrics> {
        self.composites
            .iter()
            .map(|c| PanelMetrics::new(c.member.clone(), c.metrics.clone()))
            .collect()
    }

    /// Pool one metric across the per-member composites.
    pub fn composite_metric(&self, path: &str) -> Option<Pooled> {
        pool_metric(&self.composite_members(), path)
    }

    /// Each composite's bar keys, in the order its equity was stitched.
    ///
    /// Rebuilt the way the stitch was built — fold by fold, through
    /// [`PanelAxis::member_ranges`] — rather than stored beside the curve,
    /// because the two would then be a pair that could drift. A fold in which a
    /// member had no bars contributed nothing to its curve and contributes
    /// nothing here.
    pub fn composite_keys(&self) -> Vec<Vec<i64>> {
        let mut out = vec![Vec::new(); self.composites.len()];
        for row in &self.fold_rows {
            for (m, range) in self
                .axis
                .member_ranges(row.oos.clone())
                .into_iter()
                .enumerate()
            {
                if let Some(keys) = out.get_mut(m) {
                    keys.extend_from_slice(&self.axis.members[m].keys[range]);
                }
            }
        }
        out
    }

    /// Members that departed from the pooled winner at least once, and in how
    /// many folds — the "one member went its own way" reading a mean `λ`
    /// flattens.
    pub fn departures(&self) -> Vec<(String, usize)> {
        let mut counts: Vec<(String, usize)> = Vec::new();
        for row in &self.fold_rows {
            for w in row.member_winners.iter().filter(|w| w.departed) {
                match counts.iter_mut().find(|(name, _)| *name == w.member) {
                    Some((_, n)) => *n += 1,
                    None => counts.push((w.member.clone(), 1)),
                }
            }
        }
        counts.sort_by(|a, b| b.1.cmp(&a.1).then_with(|| a.0.cmp(&b.0)));
        counts
    }

    /// How many *independent* members this panel's out-of-sample results are
    /// worth — see [`effective_breadth`].
    ///
    /// Measured on the composites' **own** returns rather than on the members'
    /// price series, and the distinction is the whole reading: what a pooled
    /// figure rests on is how much the *results* co-moved, not how much the
    /// instruments did. A strategy that trades two correlated markets at
    /// different times produces two more nearly independent curves than their
    /// prices would suggest, and it should be credited for it.
    pub fn effective_breadth(&self) -> Option<Breadth> {
        let keys = self.composite_keys();
        let returns: Vec<Vec<Real>> = self
            .composites
            .iter()
            .map(|c| crate::metrics::per_bar_returns(&c.equity, self.cash))
            .collect();
        let members: Vec<(&[i64], &[Real])> = keys
            .iter()
            .zip(returns.iter())
            .map(|(k, r)| (k.as_slice(), r.as_slice()))
            .collect();
        effective_breadth(&members)
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
///
/// # `shrink`
///
/// With `shrink` set, each fold additionally estimates its own `λ` (see
/// [`crate::spec::shrinkage`]) from sub-spans of its in-sample window, and each
/// member selects its own winner off the shrunk surface `μ + α_r + λ·γ_rm`
/// rather than taking the pooled one. At `λ = 0` that is complete pooling and
/// every member picks the same row — the flag adds a readout and changes
/// nothing else. At `λ = 1` each member picks its own.
///
/// It needs `best_by`: there is no surface to shrink without a ranking key, the
/// same way `smooth` needs one.
///
/// **Everything the fold selects on comes from that fold's own in-sample
/// window.** Estimating `λ` over all folds at once would be cheaper — the fold
/// measurements are already there — and would be lookahead: fold 1's winner
/// would rest on a variance component that fold 10's data helped estimate.
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
    shrink: bool,
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
    if shrink && best_by.is_none() {
        bail!(
            "--shrink needs --best-by: partial pooling shrinks a *ranking key* toward the \
             panel's consensus, and without one there is no surface for a member to select off"
        );
    }
    if shrink && risk_aversion > 0.0 {
        bail!(
            "--shrink and --risk-aversion are rival answers to the same question. \
             `-k` charges a parameter set for the spread between members; --shrink models \
             that spread and lets each member move by however much of it is real. Applying \
             both pays for the same disagreement twice. Pick one"
        );
    }
    if let Some(cfg) = smooth {
        cfg.validate_against(&subgrids)?;
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
    let reduce = |report: &crate::RunReport<Symbol>, range: Range<usize>| {
        metrics::from_report(
            &metrics::report_slice(report, range),
            bars_per_year,
            risk_free_rate,
            seconds_per_bar,
        )
    };
    let measure_one = |row: usize, m: usize, pooled: Range<usize>| -> Option<PanelMetrics> {
        let range = axis.member_range(m, pooled);
        if range.is_empty() {
            return None;
        }
        Some(PanelMetrics::new(
            axis.members[m].name.clone(),
            reduce(report_at(row, m), range),
        ))
    };
    // The same measurement, plus the within-cell replicates a fold needs to
    // estimate `λ` from **its own** in-sample data.
    //
    // Sub-spans of the in-sample window, never other folds: a fold selects on
    // what it can see, and borrowing another fold's measurements to decide this
    // fold's winner is lookahead however it is dressed up. The extra cost is
    // metric reduction over slices of a report that was already run, not extra
    // backtests — which is why this is affordable per fold at all.
    let measure_one_replicated =
        |row: usize, m: usize, pooled: Range<usize>| -> Option<PanelMetrics> {
            let range = axis.member_range(m, pooled);
            if range.is_empty() {
                return None;
            }
            let report = report_at(row, m);
            let whole = reduce(report, range.clone());
            let Some(k) = shrinkage::replicate_split(range.len()) else {
                // Too short to cut. The cell still reports its whole-span
                // value; it simply cannot speak to within-cell spread, and
                // `Decomposition::lambda_support` is what makes that visible.
                return Some(PanelMetrics::new(axis.members[m].name.clone(), whole));
            };
            let span = range.len() / k;
            let windows: Vec<metrics::WindowMetrics> = (0..k)
                .map(|i| {
                    let start = range.start + i * span;
                    // The last sub-span absorbs the remainder rather than
                    // dropping it — the same rule `windowed_from_report`
                    // follows for a trailing partial window.
                    let end = if i + 1 == k { range.end } else { start + span };
                    metrics::WindowMetrics {
                        start_bar: start,
                        end_bar: end.saturating_sub(1),
                        metrics: reduce(report, start..end),
                    }
                })
                .collect();
            Some(PanelMetrics::with_windows(
                axis.members[m].name.clone(),
                whole,
                windows,
            ))
        };
    let mut fold_rows: Vec<PanelFoldRow> = Vec::with_capacity(folds.len());
    // The run-wide score table, accumulated as the folds go: one cell per
    // `(grid row, member)`, one replicate per fold. Scalars only — retaining
    // each fold's `per_row_is` documents to rebuild this afterwards would cost
    // `folds × rows × members` full metric documents for the sake of one number
    // per cell.
    let mut run_table = best_by
        .as_ref()
        .map(|_| shrinkage::ScoreTable::new(plan.len(), n_members));
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
                .map(|&(r, m)| {
                    if shrink {
                        measure_one_replicated(r, m, fold.is.clone())
                    } else {
                        measure_one(r, m, fold.is.clone())
                    }
                })
                .collect()
        });
        let per_row_is: Vec<Vec<PanelMetrics>> = measured
            .chunks(n_members)
            .map(|row| row.iter().flatten().cloned().collect())
            .collect();

        // Accumulate this fold's readings into the run-wide table before
        // anything is selected — every row, not just the winner, since the
        // table's rows *are* the grid.
        if let (Some(table), Some((_, path, _))) = (run_table.as_mut(), best_by.as_ref()) {
            let names: Vec<&str> = axis.members.iter().map(|m| m.name.as_str()).collect();
            for (r, docs) in per_row_is.iter().enumerate() {
                for doc in docs {
                    let Some(m) = names.iter().position(|n| *n == doc.member) else {
                        continue;
                    };
                    if let Some(v) = crate::spec::optimize::lookup(&doc.metrics, path) {
                        table.push(r, m, v);
                    }
                }
            }
        }

        // Partial pooling: fit this fold's own two-way layout, and select the
        // pooled winner *and* every member's own pick off the **same** surface.
        //
        // Sharing the scale is what makes `departed` mean anything. The
        // decomposition is fitted on cell means over in-sample sub-spans, while
        // `pooled_ranking_key` reads each member's whole-window document — two
        // honest numbers that need not agree on an argmax. Ranking the members
        // on one and the reference on the other produced the contradiction it
        // was built to rule out: a fold reporting `λ = 0`, where every member
        // sees the identical surface `μ + α_r`, and *also* reporting that both
        // members chose differently.
        //
        // So under `--shrink` the reference is `argmax_r (μ + α_r)` — complete
        // pooling expressed on the shrunk scale — and `λ = 0` yields no
        // departures by construction. Without `--shrink`, nothing changes.
        let mut fold_shrinkage: Option<shrinkage::Summary> = None;
        let mut member_winners: Vec<MemberWinner> = Vec::new();
        let mut shrunk_columns: Option<Vec<Vec<Option<Real>>>> = None;
        let mut consensus: Option<Vec<Option<Real>>> = None;
        if shrink && let Some((_, path, _)) = &best_by {
            let names: Vec<String> = axis.members.iter().map(|m| m.name.clone()).collect();
            let table = score_table(&per_row_is, &names, path);
            if let Some(decomposition) = table.decompose() {
                fold_shrinkage = Some(decomposition.summary(&table));
                consensus = Some(
                    (0..plan.len())
                        .map(|r| decomposition.row_effects[r].map(|a| decomposition.grand_mean + a))
                        .collect(),
                );
                shrunk_columns = decomposition.shrunk(&table).map(|surface| {
                    (0..n_members)
                        .map(|m| {
                            (0..plan.len())
                                .map(|r| surface[r * n_members + m])
                                .collect()
                        })
                        .collect()
                });
            }
        }

        let mut fold_smoothed: Option<Vec<SmoothedKey>> = None;
        let mut winner_smoothed: Option<SmoothedKey> = None;
        let winner: usize = match &best_by {
            Some((_, path, direction)) => {
                // Under `--shrink` the key is the consensus surface; otherwise
                // it is the pooled `mean ∓ k·std` it has always been. `-k` is
                // refused alongside `--shrink` precisely because the two are
                // rival answers to "what should spread between members cost" —
                // one charges for it, the other models it.
                let keys: Vec<Option<Real>> = match &consensus {
                    Some(c) => c.clone(),
                    None => per_row_is
                        .iter()
                        .map(|ms| pooled_ranking_key(ms, path, *direction, risk_aversion))
                        .collect(),
                };
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

        let mut chosen: Vec<usize> = vec![winner; n_members];
        if let (Some(columns), Some((_, _, direction))) = (&shrunk_columns, &best_by) {
            let names: Vec<String> = axis.members.iter().map(|m| m.name.clone()).collect();
            for (m, column) in columns.iter().enumerate() {
                // Smoothing runs *after* the shrink: it borrows strength from
                // neighbouring parameter points, shrinkage from other members,
                // and this order leaves both defined. The column is in lattice
                // order, the same layout `smooth_keys` reads the pooled key
                // vector out of, so it applies unchanged.
                let column: Vec<Option<Real>> = match smooth {
                    Some(cfg) => smooth_keys(&subgrids, column, cfg)?
                        .iter()
                        .map(|s| s.value)
                        .collect(),
                    None => column.clone(),
                };
                let Some(idx) = argbest(&column, *direction) else {
                    continue;
                };
                chosen[m] = idx;
                let (si, ci) = plan[idx];
                member_winners.push(MemberWinner {
                    member: names[m].clone(),
                    row: idx,
                    values: project_row(&subgrids[si], &subgrids[si].combos[ci], &union_columns),
                    departed: idx != winner,
                });
            }
        }

        // Each member's out-of-sample documents, under the row *that member*
        // selected. Without `--shrink` every entry of `chosen` is the pooled
        // winner and this is `measure(winner, ..)` spelled out.
        let oos_members: Vec<PanelMetrics> = (0..n_members)
            .filter_map(|m| measure_one(chosen[m], m, fold.oos.clone()))
            .collect();

        // Stitch each member's OOS slice onto its own composite. Each member
        // carries its own running equity, so a fold in which a member had no
        // bars leaves that member's curve untouched rather than flat-filling
        // it — a gap in coverage, not a run of zero returns.
        for (m, range) in axis.member_ranges(fold.oos.clone()).into_iter().enumerate() {
            if range.is_empty() {
                continue;
            }
            let slice = metrics::report_slice(report_at(chosen[m], m), range);
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
            shrinkage: fold_shrinkage,
            member_winners,
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
                attribution: None,
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
        run_shrinkage: run_table
            .as_ref()
            .and_then(|t| t.decompose().map(|d| d.summary(t))),
    })
}

#[cfg(test)]
mod selection_breadth_tests {
    use super::*;

    /// A replicated table whose cells are `f(row, member)`.
    fn table_from(
        rows: usize,
        members: usize,
        f: impl Fn(usize, usize) -> Real,
    ) -> ScoreTableFixture {
        let mut t = shrinkage::ScoreTable::new(rows, members);
        for r in 0..rows {
            for m in 0..members {
                // Two replicates per cell, symmetric about the cell value, so
                // the within-cell variance is a knob independent of the value.
                let v = f(r, m);
                t.extend(r, m, [v - 0.05, v + 0.05]);
            }
        }
        let d = t.decompose().expect("dense replicated table fits");
        ScoreTableFixture { table: t, fit: d }
    }

    struct ScoreTableFixture {
        table: shrinkage::ScoreTable,
        fit: shrinkage::Decomposition,
    }

    /// **The complete-pooling limit, and the reason this is safe to ship.**
    ///
    /// When the members agree, `λ` is zero, every member's ranking surface is
    /// the identical `μ + α_r`, and the panel ran exactly **one** search over
    /// the grid. The deflated Sharpe's trial count is then unchanged from what
    /// complete pooling has always reported — so turning `--shrink` on cannot
    /// silently re-baseline the DSR of a panel that did not need it.
    #[test]
    fn an_agreeing_panel_ran_one_search() {
        // Purely additive: row effect plus member level, no interaction.
        let f = table_from(6, 3, |r, m| r as Real + 10.0 * m as Real);
        assert_eq!(
            f.fit.lambda,
            Some(0.0),
            "an additive table has no interaction"
        );

        let b = selection_breadth(&f.fit, &f.table).expect("three usable columns");
        assert_eq!(b.members, 3);
        assert!(
            (b.mean_correlation - 1.0).abs() < 1e-9,
            "identical surfaces are perfectly correlated, got {}",
            b.mean_correlation,
        );
        assert!(
            (b.effective - 1.0).abs() < 1e-9,
            "one shared surface is one search, got {}",
            b.effective,
        );
    }

    /// **The no-pooling limit.** Members that rank the grid in unrelated orders
    /// each searched it for themselves, so the maximum was taken over `K` times
    /// as many draws and the trial count has to say so.
    #[test]
    fn members_that_share_nothing_ran_a_search_each() {
        // Each member's optimum sits at a different row, and the shared row
        // effect cancels — so the surface is almost pure interaction.
        let peaks = [0usize, 3, 6];
        let f = table_from(9, 3, |r, m| if r == peaks[m] { 10.0 } else { 0.0 });
        assert!(
            f.fit.lambda.expect("replicated") > 0.9,
            "members peaking on different rows disagree, got {:?}",
            f.fit.lambda,
        );

        let b = selection_breadth(&f.fit, &f.table).expect("three usable columns");
        assert!(
            b.effective > 2.5,
            "three unrelated surfaces are close to three searches, got {} (rho {})",
            b.effective,
            b.mean_correlation,
        );
        assert!(b.effective <= 3.0, "never more searches than members");
    }

    /// A dominant shared effect means every member picks the same row *anyway*,
    /// so only one search happened however high `λ` is. Charging the panel for
    /// `K` searches there would deflate a result nobody over-searched for.
    #[test]
    fn a_dominant_shared_effect_is_still_one_search() {
        // A large row effect with a small per-member wobble on top.
        let f = table_from(8, 3, |r, m| {
            r as Real * 10.0 + if (r + m) % 2 == 0 { 0.2 } else { -0.2 }
        });
        let b = selection_breadth(&f.fit, &f.table).expect("three usable columns");
        assert!(
            b.effective < 1.5,
            "a shared optimum is one search whatever lambda says, got {} (rho {})",
            b.effective,
            b.mean_correlation,
        );
    }

    /// Without `λ` there is no shrunk surface, so there is nothing to correlate
    /// and no search count to report — `None`, never a default.
    #[test]
    fn an_unreplicated_table_reports_no_search_count() {
        let mut t = shrinkage::ScoreTable::new(6, 3);
        for r in 0..6 {
            for m in 0..3 {
                t.push(r, m, r as Real + m as Real);
            }
        }
        let d = t.decompose().expect("dense table fits");
        assert_eq!(
            d.lambda, None,
            "one observation per cell identifies nothing"
        );
        assert!(selection_breadth(&d, &t).is_none());
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn axis(name: &str, keys: &[i64]) -> MemberAxis {
        MemberAxis::from_keys(name, keys.to_vec()).expect("ascending")
    }

    fn table(entries: &[(&str, Value)]) -> HashMap<String, Value> {
        entries
            .iter()
            .map(|(k, v)| ((*k).to_string(), v.clone()))
            .collect()
    }

    /// Two axes pool over their product, and the enumeration is name-sorted
    /// with the last axis varying fastest — the same layout `cartesian` gives
    /// a subgrid, so member order is a property of the panel and not of the
    /// order the flag's terms were typed in.
    #[test]
    fn several_axes_enumerate_their_cartesian_product_in_a_fixed_order() {
        let panel = Panel::from_params(&table(&[
            ("SYM", serde_json::json!(["UP", "DOWN"])),
            ("FAST", serde_json::json!([2, 3])),
        ]))
        .expect("two axes");
        assert_eq!(panel.len(), 4);
        assert_eq!(
            panel
                .members()
                .iter()
                .map(|m| m.label.as_str())
                .collect::<Vec<_>>(),
            [
                "FAST=2,SYM=UP",
                "FAST=2,SYM=DOWN",
                "FAST=3,SYM=UP",
                "FAST=3,SYM=DOWN"
            ],
        );
        // A member substitutes every one of its axes, over whatever the row
        // already fixed.
        let params = panel.members()[3].params_over(&table(&[("SLOW", serde_json::json!(9))]));
        assert_eq!(params["SYM"], serde_json::json!("DOWN"));
        assert_eq!(params["FAST"], serde_json::json!(3));
        assert_eq!(params["SLOW"], serde_json::json!(9));
    }

    /// The two ways a `--pooled` term is not a panel: a scalar (which would be
    /// a plain run), and axes whose product is a single cell (whose `_std` of
    /// zero reads like consistency rather than like an absent comparison).
    #[test]
    fn a_scalar_term_and_a_product_of_one_are_both_refused() {
        let scalar = Panel::from_params(&table(&[("SYM", serde_json::json!("UP"))]))
            .expect_err("a scalar is not an axis");
        assert!(
            scalar.to_string().contains("takes axes, not single values"),
            "{scalar}"
        );

        let one = Panel::from_params(&table(&[
            ("SYM", serde_json::json!(["UP"])),
            ("FAST", serde_json::json!([2])),
        ]))
        .expect_err("a product of one is a panel of one");
        assert!(one.to_string().contains("only 1 member"), "{one}");
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
            attribution: None,
        };
        PanelMetrics::new(name, metrics::from_report(&report, 252.0, 0.0, None))
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

    // --- effective breadth -------------------------------------------------

    /// `(keys, values)` for a member on a daily clock starting at `start`.
    fn series(start: i64, values: Vec<Real>) -> (Vec<i64>, Vec<Real>) {
        let keys = (0..values.len() as i64)
            .map(|i| start + i * 86_400_000)
            .collect();
        (keys, values)
    }

    fn breadth_of(members: &[(Vec<i64>, Vec<Real>)]) -> Option<Breadth> {
        let refs: Vec<(&[i64], &[Real])> = members
            .iter()
            .map(|(k, v)| (k.as_slice(), v.as_slice()))
            .collect();
        effective_breadth(&refs)
    }

    #[test]
    fn identical_members_are_worth_one() {
        // The reading the whole thing exists for: a panel of one market wearing
        // three names is one piece of evidence, not three.
        let wave: Vec<Real> = (0..60).map(|i| ((i as Real) * 0.7).sin() * 0.01).collect();
        let members = vec![
            series(0, wave.clone()),
            series(0, wave.clone()),
            series(0, wave),
        ];
        let b = breadth_of(&members).expect("three members share a clock");
        assert_eq!(b.members, 3);
        assert_eq!(b.pairs, 3);
        assert!((b.mean_correlation - 1.0).abs() < 1e-9, "{b:?}");
        assert!((b.effective - 1.0).abs() < 1e-9, "{b:?}");
    }

    #[test]
    fn uncorrelated_members_are_worth_their_count() {
        // Orthogonal by construction: a sine and a cosine over a whole number of
        // periods have zero sample correlation.
        let n = 120;
        let a: Vec<Real> = (0..n)
            .map(|i| ((i as Real) * std::f64::consts::TAU / 60.0).sin())
            .collect();
        let b: Vec<Real> = (0..n)
            .map(|i| ((i as Real) * std::f64::consts::TAU / 60.0).cos())
            .collect();
        let members = vec![series(0, a), series(0, b)];
        let got = breadth_of(&members).expect("two members share a clock");
        assert!(got.mean_correlation.abs() < 0.05, "{got:?}");
        assert!((got.effective - 2.0).abs() < 0.2, "{got:?}");
    }

    #[test]
    fn a_negatively_correlated_panel_does_not_report_infinite_breadth() {
        // The denominator crosses zero on the way to saying "more independent
        // than its count", so the mean is floored at 0 and the answer caps at M.
        let wave: Vec<Real> = (0..60).map(|i| ((i as Real) * 0.7).sin() * 0.01).collect();
        let inverted: Vec<Real> = wave.iter().map(|v| -v).collect();
        let members = vec![series(0, wave), series(0, inverted)];
        let b = breadth_of(&members).expect("two members share a clock");
        assert!(b.mean_correlation < -0.9, "{b:?}");
        assert!(
            b.effective.is_finite() && (b.effective - 2.0).abs() < 1e-9,
            "{b:?}"
        );
    }

    #[test]
    fn each_pair_is_joined_on_its_own_shared_bars() {
        // A late lister overlaps the others by a little; intersecting every
        // member first would collapse the axis the other pair is measured on
        // down to that overlap. It is measured on its own, or not at all.
        let wave: Vec<Real> = (0..90).map(|i| ((i as Real) * 0.7).sin() * 0.01).collect();
        let members = vec![
            series(0, wave.clone()),
            series(0, wave.iter().map(|v| v * 1.1).collect()),
            // Starts 70 bars in: 20 shared bars with each of the others, under
            // MIN_SHARED_BARS, so it contributes no pair at all.
            series(70 * 86_400_000, wave[..40].to_vec()),
        ];
        let b = breadth_of(&members).expect("the first two share a clock");
        assert_eq!(
            b.pairs, 1,
            "only the well-overlapped pair is measurable: {b:?}"
        );
    }

    #[test]
    fn a_panel_nothing_can_be_measured_on_reports_nothing() {
        // Not the member count. Reporting 3 here would be answering a question
        // nobody could check.
        let short: Vec<Real> = vec![0.01; 5];
        let members = vec![series(0, short.clone()), series(0, short)];
        assert!(breadth_of(&members).is_none());
    }

    #[test]
    fn a_flat_member_has_no_correlation_to_share() {
        // A constant series has no variance, which is a different answer from
        // "uncorrelated" and must not be averaged in as a zero.
        let wave: Vec<Real> = (0..60).map(|i| ((i as Real) * 0.7).sin() * 0.01).collect();
        let members = vec![series(0, wave), series(0, vec![0.0; 60])];
        assert!(breadth_of(&members).is_none());
    }
}
