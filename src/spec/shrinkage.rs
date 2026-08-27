//! Partial pooling — how much a panel's members agree about the optimum, and
//! how far each is allowed to differ from the pooled answer.
//!
//! # What this measures
//!
//! [`crate::spec::panel`] reduces a metric across a panel and ranks on
//! `mean ∓ k·std`. That is an aggregated objective, and it is the right one
//! only when the swept parameter *means the same thing on every member*. When
//! it does not — a dollar-denominated stop pooled across instruments three
//! orders of magnitude apart, or two markets whose best lookback genuinely
//! differs — the pooled winner is a compromise that can be worse on **every**
//! member than that member's own choice. Nothing in the reduction says which
//! case you are in.
//!
//! This module says. Lay the sweep out as a row × member table of scores and
//! read it as a two-way layout:
//!
//! ```text
//! x_rm = μ + α_r + β_m + γ_rm + ε
//!        ^   ^     ^     ^
//!        |   |     |     parameter × member — the members disagree
//!        |   |     member effect — same for every row, so no ranking information
//!        |   shared parameter effect — what pooling is trying to estimate
//!        grand mean
//! ```
//!
//! * **`β_m` is a nuisance.** It is why today's cross-member `std` conflates
//!   "this parameter set is unstable" with "these instruments have different
//!   achievable Sharpe". Identical for every row, so it carries nothing a
//!   ranking can use — yet it inflates every row's `−k·std` penalty, and
//!   inflates it *unequally*, because rows differ in which members they are
//!   defined on. [`Decomposition::demeaned`] removes it.
//! * **`γ_rm` decides whether pooling was valid at all.** Writing
//!   `λ = τ²_γ / (τ²_γ + σ²_ε/n̄)`: at `λ → 0` the members share an optimum and
//!   pooling buys the full variance reduction; at `λ → 1` they are separate
//!   problems and the pooled winner is a compromise.
//!
//! This is a random-effects model in the mixed-model sense. Complete pooling —
//! today's `--pooled` — is `τ²_γ → 0`. No pooling — an ordinary `SYM=[...]`
//! grid axis, one parameter set fitted per instrument — is `τ²_γ → ∞`. Partial
//! pooling estimates it, and the satisfying part is that the **method-of-moments
//! estimator for `τ²_γ` is the interaction variance component**: one quantity
//! both diagnoses whether to pool and supplies the weight that acts on it.
//!
//! # Replication is not optional
//!
//! With one observation per cell, `γ_rm` and `ε_rm` are confounded — the
//! classic unreplicated two-way layout — and `λ` is *precisely their ratio*. So
//! [`Decomposition::lambda`] is an `Option`, and it is `None` for an
//! unreplicated table rather than defaulting to a number. Reporting `λ = 1` for
//! a table that cannot distinguish disagreement from noise would be the exact
//! failure this module exists to prevent.
//!
//! Replicates come from splitting each cell's measurement span:
//!
//! * the plain sweep takes them from `-w/--windowed`, which is why that flag
//!   composes with `--pooled` rather than excluding it;
//! * pooled walk-forward takes them from sub-spans of each fold's **in-sample**
//!   window, so a fold's `λ` rests only on data that fold could see.
//!
//! # Shrinking happens in score space
//!
//! [`Decomposition::shrunk`] adjusts the *scores*, and each member then picks
//! its own argmax off the adjusted column. The alternative — shrinking the
//! chosen parameter itself, `θ_m = θ̄ + λ(θ̂_m − θ̄)` — is easier to describe and
//! wrong three ways: it needs every axis numeric with a meaningful metric (a
//! categorical axis has no shrinkage target), it produces off-lattice values
//! that have to be rounded back, and shrinking each axis independently walks
//! off a diagonal ridge (`FAST`/`SLOW` are correlated, and their joint optimum
//! is not the pair of marginal ones).
//!
//! Score space needs none of that. It stays on the grid, handles categorical
//! axes, respects the surface's geometry, and reads a table this module already
//! builds.

use crate::market::Real;

/// Below this many populated cells the decomposition is refused outright.
///
/// Not a precision floor — an existence one. The interaction mean square has
/// `cells − rows − members + 1` degrees of freedom, so a table barely wider
/// than its own margins has none left to estimate an interaction with, and the
/// ratio those margins feed would be noise reported as a finding.
pub const MIN_CELLS: usize = 6;

/// Fewest observations in a cell for that cell to inform the residual.
///
/// One observation says nothing about within-cell spread. The same "a
/// coefficient over a handful of points is not a weak reading, it is noise"
/// rule [`crate::spec::panel::MIN_SHARED_BARS`] draws for a correlation.
pub const MIN_REPLICATES: usize = 2;

/// Sub-spans each in-sample window is cut into to replicate a cell, when a
/// fold has to estimate `λ` from **its own** data.
///
/// Four, because the two errors are opposite and both real. Fewer leaves the
/// within-cell variance on one or two degrees of freedom per cell, which is
/// the noise-reported-as-a-finding case [`MIN_CELLS`] guards at the table
/// level. More makes each sub-span shorter, and a metric measured over a short
/// span is *itself* noisier — that noise lands in `σ²_ε`, which biases `λ`
/// **down** and would quietly argue for pooling exactly when the evidence got
/// thinner.
pub const FOLD_REPLICATES: usize = 4;

/// Fewest bars a replicate sub-span may have before the split is abandoned.
///
/// Same floor and same reasoning as [`crate::spec::panel::MIN_SHARED_BARS`]: a
/// statistic over a handful of points is not a weak reading, it is noise. A
/// fold whose in-sample window cannot be cut this finely reports no `λ` rather
/// than a `λ` resting on four four-bar Sharpes.
pub const MIN_REPLICATE_BARS: usize = 30;

/// How many equal sub-spans a span of `bars` should be cut into for
/// replication, or `None` when it is too short to cut.
///
/// Returns at most [`FOLD_REPLICATES`], and never a count that would put a
/// sub-span under [`MIN_REPLICATE_BARS`] — a shorter split is preferred to a
/// finer one, and no split at all is preferred to a bad one.
pub fn replicate_split(bars: usize) -> Option<usize> {
    let affordable = bars / MIN_REPLICATE_BARS;
    let k = affordable.min(FOLD_REPLICATES);
    (k >= MIN_REPLICATES).then_some(k)
}

// ---------------------------------------------------------------------------
// The table
// ---------------------------------------------------------------------------

/// A row × member table of scores, each cell holding that pair's replicate
/// observations.
///
/// **Ragged by construction.** A member that had not listed, or that reported
/// no value for the metric, contributes an empty cell — never a zero. That is
/// the contract [`crate::spec::panel::pool_metric`] already keeps, and for the
/// same reason: a substituted zero is indistinguishable from a measurement, and
/// every statistic downstream would silently rest on it.
#[derive(Clone, Debug)]
pub struct ScoreTable {
    rows: usize,
    members: usize,
    /// Row-major, `rows × members`. Each cell holds that pair's replicates.
    cells: Vec<Vec<Real>>,
}

impl ScoreTable {
    /// An empty table of the given shape — every cell unpopulated.
    pub fn new(rows: usize, members: usize) -> Self {
        Self {
            rows,
            members,
            cells: vec![Vec::new(); rows * members],
        }
    }

    pub fn rows(&self) -> usize {
        self.rows
    }

    pub fn members(&self) -> usize {
        self.members
    }

    /// Record one observation for `(row, member)`. Non-finite values are
    /// dropped rather than propagated — a `NaN` in one cell would otherwise
    /// take every sum of squares with it.
    pub fn push(&mut self, row: usize, member: usize, value: Real) {
        if row < self.rows && member < self.members && value.is_finite() {
            self.cells[row * self.members + member].push(value);
        }
    }

    /// Record a cell's replicates in one call.
    pub fn extend(&mut self, row: usize, member: usize, values: impl IntoIterator<Item = Real>) {
        for v in values {
            self.push(row, member, v);
        }
    }

    /// One cell's replicates. Empty when the pair was never measured.
    pub fn cell(&self, row: usize, member: usize) -> &[Real] {
        if row < self.rows && member < self.members {
            &self.cells[row * self.members + member]
        } else {
            &[]
        }
    }

    /// A cell's mean, or `None` when it holds nothing.
    pub fn cell_mean(&self, row: usize, member: usize) -> Option<Real> {
        let c = self.cell(row, member);
        (!c.is_empty()).then(|| c.iter().sum::<Real>() / c.len() as Real)
    }

    /// Populated cells.
    pub fn populated(&self) -> usize {
        self.cells.iter().filter(|c| !c.is_empty()).count()
    }

    /// Total observations across every cell.
    pub fn observations(&self) -> usize {
        self.cells.iter().map(Vec::len).sum()
    }

    /// Cells carrying at least [`MIN_REPLICATES`] observations — the ones that
    /// can speak to within-cell spread.
    pub fn replicated_cells(&self) -> usize {
        self.cells
            .iter()
            .filter(|c| c.len() >= MIN_REPLICATES)
            .count()
    }

    /// Rows with at least one populated cell.
    fn live_rows(&self) -> Vec<usize> {
        (0..self.rows)
            .filter(|&r| (0..self.members).any(|m| !self.cell(r, m).is_empty()))
            .collect()
    }

    /// Members with at least one populated cell.
    fn live_members(&self) -> Vec<usize> {
        (0..self.members)
            .filter(|&m| (0..self.rows).any(|r| !self.cell(r, m).is_empty()))
            .collect()
    }

    /// Fit the two-way layout and estimate its variance components.
    ///
    /// `None` when the table is too sparse to carry the fit — see
    /// [`MIN_CELLS`]. Never a zero-filled answer.
    pub fn decompose(&self) -> Option<Decomposition> {
        decompose(self)
    }
}

// ---------------------------------------------------------------------------
// The decomposition
// ---------------------------------------------------------------------------

/// The fitted two-way layout: the additive part, the variance components, and
/// the shrinkage weight they imply.
///
/// Every effect vector is indexed by the table's own row / member indices and
/// is `None` where that row or member had no populated cell. Nothing here is
/// applied to anything automatically — the same stance
/// [`crate::spec::panel::Breadth`] takes. What a caller does with `λ` is a
/// decision this crate has no basis to make for them.
#[derive(Clone, Debug)]
pub struct Decomposition {
    /// `μ` — the unweighted mean over populated **cell means**, not over raw
    /// observations. A cell measured over more replicates is not a *stronger
    /// claim about that pair*, only a better-measured one, so it must not drag
    /// the grand mean toward itself.
    pub grand_mean: Real,
    /// `α_r`, the shared parameter effect, as a deviation from `grand_mean`.
    pub row_effects: Vec<Option<Real>>,
    /// `β_m`, the member effect, as a deviation from `grand_mean`.
    pub member_effects: Vec<Option<Real>>,
    /// `γ_rm`, row-major, `rows × members` — what the additive part misses.
    pub interactions: Vec<Option<Real>>,
    /// Variance of the fitted `α_r` across live rows: how much the parameter
    /// matters at all. `λ` says little beside a row effect of zero — nothing
    /// is being selected in the first place.
    pub row_variance: Real,
    /// Variance of the fitted `β_m` — the member effect `_z` removes.
    pub member_variance: Real,
    /// `τ²_γ` — the interaction component, bias-corrected for the sampling
    /// noise its cell means carry, and floored at zero.
    pub interaction_variance: Real,
    /// `σ²_ε` — pooled within-cell variance, or `None` for an unreplicated
    /// table, where it is inseparable from the interaction.
    pub residual_variance: Option<Real>,
    /// `λ = τ²_γ / (τ²_γ + σ²_ε/n̄)` in `0..=1`, or `None` when
    /// [`Self::residual_variance`] is.
    ///
    /// `0` — the members share an optimum; pool completely.
    /// `1` — they are separate problems; the pooled winner is a compromise.
    pub lambda: Option<Real>,
    /// Harmonic mean replicate count over replicated cells — the `n̄` in `λ`.
    /// Harmonic, not arithmetic, because `λ` divides by it: the cells that
    /// constrain the estimate least should dominate the denominator.
    pub mean_replicates: Real,
    /// Populated cells the fit rests on.
    pub cells: usize,
    /// Rows with at least one populated cell.
    pub live_rows: usize,
    /// Members with at least one populated cell.
    pub live_members: usize,
    /// Whether every live `(row, member)` pair was populated. An unbalanced
    /// table is fitted all the same — the alternating fit below handles it —
    /// but its components are method-of-moments rather than exact, and a reader
    /// deserves to know which they are looking at.
    pub balanced: bool,
}

impl Decomposition {
    /// The **support** behind `λ`: replicated cells over populated cells.
    ///
    /// A `λ` resting on three of ninety cells and one resting on all ninety are
    /// not the same evidence, and without this they render identically — the
    /// gap [`crate::spec::panel::Pooled::defined`] exists to close, drawn for a
    /// variance component.
    pub fn lambda_support(&self, table: &ScoreTable) -> Real {
        if self.cells == 0 {
            0.0
        } else {
            table.replicated_cells() as Real / self.cells as Real
        }
    }

    /// `x̄_rm` with the member effect removed — the row's score on a scale
    /// where every member contributes on equal terms.
    ///
    /// This is what makes a cross-member spread mean "this parameter set ranks
    /// consistently well" instead of "these instruments are alike". `None`
    /// where the cell is unpopulated, so the caller's support counts stay
    /// honest.
    pub fn demeaned(&self, table: &ScoreTable) -> Vec<Option<Real>> {
        let mut out = vec![None; table.rows() * table.members()];
        for r in 0..table.rows() {
            for m in 0..table.members() {
                let (Some(x), Some(b)) = (table.cell_mean(r, m), self.member_effects[m]) else {
                    continue;
                };
                out[r * table.members() + m] = Some(x - b);
            }
        }
        out
    }

    /// The score surface each member selects its own parameters off under
    /// partial pooling: `μ + α_r + λ·γ_rm`.
    ///
    /// At `λ = 0` every member sees the same surface `μ + α_r` and therefore
    /// picks the pooled winner — complete pooling. At `λ = 1` every member sees
    /// its own cell means back and picks its own winner — no pooling. In
    /// between it borrows strength: a member whose column is noisy is pulled
    /// toward the consensus, one that genuinely disagrees is allowed to keep
    /// disagreeing.
    ///
    /// The member effect `β_m` is deliberately **left out**. It is a constant
    /// within a member's column, so it cannot change which row that column's
    /// argmax lands on; including it would only make the numbers harder to
    /// compare across members.
    ///
    /// `None` for an unreplicated table: without `λ` there is no defensible
    /// surface to select on, and falling back to either extreme would be
    /// picking a pooling policy by accident.
    pub fn shrunk(&self, table: &ScoreTable) -> Option<Vec<Option<Real>>> {
        let lambda = self.lambda?;
        let mut out = vec![None; table.rows() * table.members()];
        for r in 0..table.rows() {
            let Some(alpha) = self.row_effects[r] else {
                continue;
            };
            for m in 0..table.members() {
                let i = r * table.members() + m;
                let Some(gamma) = self.interactions[i] else {
                    continue;
                };
                out[i] = Some(self.grand_mean + alpha + lambda * gamma);
            }
        }
        Some(out)
    }

    /// The headline numbers without the per-cell vectors.
    ///
    /// A fold keeps one of these; keeping the whole [`Decomposition`] would
    /// carry a `rows × members` vector per fold for the sake of a handful of
    /// scalars that are all anything downstream reads.
    pub fn summary(&self, table: &ScoreTable) -> Summary {
        Summary {
            lambda: self.lambda,
            support: self.lambda_support(table),
            cells: self.cells,
            live_rows: self.live_rows,
            live_members: self.live_members,
            row_variance: self.row_variance,
            member_variance: self.member_variance,
            interaction_variance: self.interaction_variance,
            residual_variance: self.residual_variance,
            mean_replicates: self.mean_replicates,
            balanced: self.balanced,
        }
    }
}

/// How a `λ` reads in prose, from the bare number.
///
/// A free function rather than a method on either type: every caller holds a
/// [`Summary`] scalar rather than the [`Decomposition`] it came from, and a
/// second spelling would only be somewhere for these strings to drift apart.
pub fn verdict(lambda: Option<Real>) -> &'static str {
    match lambda {
        None => "not estimable without replication",
        Some(l) if l < 0.15 => "the members agree — pooling is buying variance reduction",
        Some(l) if l < 0.50 => "mostly shared, with member-specific structure on top",
        Some(l) if l < 0.85 => "the members substantially disagree about the optimum",
        Some(_) => "the members are separate problems — the pooled winner is a compromise",
    }
}

/// A [`Decomposition`]'s scalars, detached from the table that produced them —
/// what a fold row, a CSV column and a console line all read.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Summary {
    /// `None` when the table carried no replication; see [`Decomposition::lambda`].
    pub lambda: Option<Real>,
    /// Replicated cells over populated cells, in `0..=1`.
    pub support: Real,
    pub cells: usize,
    pub live_rows: usize,
    pub live_members: usize,
    pub row_variance: Real,
    pub member_variance: Real,
    pub interaction_variance: Real,
    pub residual_variance: Option<Real>,
    pub mean_replicates: Real,
    pub balanced: bool,
}

impl Summary {
    /// Whether the parameter is doing anything at all on this table.
    ///
    /// `λ` compares *disagreement* against *noise* and says nothing about
    /// whether there was a signal to disagree over. On a table whose rows are
    /// indistinguishable — a grid that does not move the metric — a large `λ`
    /// means the members disagree about which of several equivalent parameter
    /// sets is marginally best, which is not the finding it looks like. Callers
    /// that report `λ` should report this beside it.
    pub fn parameter_matters(&self) -> bool {
        self.row_variance > self.interaction_variance * 0.05
    }
}

// ---------------------------------------------------------------------------
// The fit
// ---------------------------------------------------------------------------

/// Iterations of the alternating fit. The additive model over a ragged table
/// has no closed form, but the alternation is a contraction and settles to
/// machine noise well inside this; the cap guards a pathological table, it is
/// not a tuning knob.
const FIT_ITERATIONS: usize = 64;

/// Below this largest per-sweep move, the alternating fit is converged.
const FIT_TOLERANCE: Real = 1e-12;

fn decompose(table: &ScoreTable) -> Option<Decomposition> {
    let live_rows = table.live_rows();
    let live_members = table.live_members();
    let cells = table.populated();
    if cells < MIN_CELLS || live_rows.len() < 2 || live_members.len() < 2 {
        return None;
    }
    // Degrees of freedom left for the interaction once both margins are spent.
    // At zero there is still a fit, but nothing to say about disagreement.
    let df_interaction =
        cells as isize - live_rows.len() as isize - live_members.len() as isize + 1;
    if df_interaction <= 0 {
        return None;
    }

    let (rows, members) = (table.rows(), table.members());
    let mean_of = |r: usize, m: usize| table.cell_mean(r, m);

    // --- the additive part, by alternating projection ---------------------
    //
    // Unweighted in the cell means, not in the raw observations — see
    // `grand_mean`. A ragged table has no closed-form ANOVA fit, so sweep the
    // row margin, then the member margin, until neither moves.
    let mut alpha = vec![0.0; rows];
    let mut beta = vec![0.0; members];
    let mut grand = {
        let mut sum = 0.0;
        let mut n = 0usize;
        for &r in &live_rows {
            for &m in &live_members {
                if let Some(x) = mean_of(r, m) {
                    sum += x;
                    n += 1;
                }
            }
        }
        sum / n as Real
    };

    for _ in 0..FIT_ITERATIONS {
        let mut moved: Real = 0.0;
        for &r in &live_rows {
            let mut sum = 0.0;
            let mut n = 0usize;
            for &m in &live_members {
                if let Some(x) = mean_of(r, m) {
                    sum += x - grand - beta[m];
                    n += 1;
                }
            }
            if n > 0 {
                let next = sum / n as Real;
                moved = moved.max((next - alpha[r]).abs());
                alpha[r] = next;
            }
        }
        for &m in &live_members {
            let mut sum = 0.0;
            let mut n = 0usize;
            for &r in &live_rows {
                if let Some(x) = mean_of(r, m) {
                    sum += x - grand - alpha[r];
                    n += 1;
                }
            }
            if n > 0 {
                let next = sum / n as Real;
                moved = moved.max((next - beta[m]).abs());
                beta[m] = next;
            }
        }
        // Re-centre both margins into the grand mean so the parameterization
        // stays identified — `α` and `β` are each only defined up to a
        // constant, and without this the pair drifts while the fit does not.
        let a_bar = live_rows.iter().map(|&r| alpha[r]).sum::<Real>() / live_rows.len() as Real;
        let b_bar =
            live_members.iter().map(|&m| beta[m]).sum::<Real>() / live_members.len() as Real;
        for &r in &live_rows {
            alpha[r] -= a_bar;
        }
        for &m in &live_members {
            beta[m] -= b_bar;
        }
        grand += a_bar + b_bar;
        if moved < FIT_TOLERANCE {
            break;
        }
    }

    // --- interaction residuals --------------------------------------------
    let mut interactions: Vec<Option<Real>> = vec![None; rows * members];
    let mut weighted_interaction_ss = 0.0;
    for &r in &live_rows {
        for &m in &live_members {
            let Some(x) = mean_of(r, m) else { continue };
            let g = x - grand - alpha[r] - beta[m];
            interactions[r * members + m] = Some(g);
            weighted_interaction_ss += table.cell(r, m).len() as Real * g * g;
        }
    }

    // --- within-cell (residual) variance ----------------------------------
    //
    // Pooled over the cells carrying at least `MIN_REPLICATES`, on their own
    // degrees of freedom. `None` when no cell does: the interaction and the
    // error are then the same quantity measured once, and `λ` is their ratio.
    let mut within_ss = 0.0;
    let mut within_df = 0usize;
    let mut replicate_reciprocals = 0.0;
    let mut replicated = 0usize;
    for &r in &live_rows {
        for &m in &live_members {
            let c = table.cell(r, m);
            if c.len() < MIN_REPLICATES {
                continue;
            }
            let mean = c.iter().sum::<Real>() / c.len() as Real;
            within_ss += c.iter().map(|v| (v - mean) * (v - mean)).sum::<Real>();
            within_df += c.len() - 1;
            replicate_reciprocals += 1.0 / c.len() as Real;
            replicated += 1;
        }
    }
    let residual_variance = (within_df > 0).then(|| within_ss / within_df as Real);
    let mean_replicates = if replicated > 0 {
        replicated as Real / replicate_reciprocals
    } else {
        1.0
    };

    // --- variance components ----------------------------------------------
    //
    // `MS_interaction` estimates `σ²_ε + n̄·τ²_γ`, so the sampling noise the
    // cell means carry is subtracted back out before `τ²_γ` is believed. The
    // floor at zero is not cosmetic: a genuinely additive table produces a
    // small negative estimate about half the time, and a negative variance
    // reported as a finding is worse than the information it carries.
    let ms_interaction = weighted_interaction_ss / df_interaction as Real;
    let interaction_variance = match residual_variance {
        Some(s2) => ((ms_interaction - s2) / mean_replicates).max(0.0),
        None => ms_interaction.max(0.0),
    };
    let lambda = residual_variance.map(|s2| {
        let noise = s2 / mean_replicates;
        let total = interaction_variance + noise;
        if total > 0.0 {
            (interaction_variance / total).clamp(0.0, 1.0)
        } else {
            // Both components vanished: every cell mean sits exactly on the
            // additive fit. That is perfect agreement, not an absent answer.
            0.0
        }
    });

    let variance_of = |values: &[Real], live: &[usize]| -> Real {
        if live.len() < 2 {
            return 0.0;
        }
        let mean = live.iter().map(|&i| values[i]).sum::<Real>() / live.len() as Real;
        live.iter()
            .map(|&i| (values[i] - mean) * (values[i] - mean))
            .sum::<Real>()
            / live.len() as Real
    };

    let balanced = live_rows.len() * live_members.len() == cells;
    Some(Decomposition {
        grand_mean: grand,
        row_effects: (0..rows)
            .map(|r| live_rows.contains(&r).then_some(alpha[r]))
            .collect(),
        member_effects: (0..members)
            .map(|m| live_members.contains(&m).then_some(beta[m]))
            .collect(),
        interactions,
        row_variance: variance_of(&alpha, &live_rows),
        member_variance: variance_of(&beta, &live_members),
        interaction_variance,
        residual_variance,
        lambda,
        mean_replicates,
        cells,
        live_rows: live_rows.len(),
        live_members: live_members.len(),
        balanced,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Build a table from a `rows × members` grid of replicate slices.
    fn table_of(grid: &[&[&[Real]]]) -> ScoreTable {
        let members = grid.first().map_or(0, |r| r.len());
        let mut t = ScoreTable::new(grid.len(), members);
        for (r, row) in grid.iter().enumerate() {
            for (m, cell) in row.iter().enumerate() {
                t.extend(r, m, cell.iter().copied());
            }
        }
        t
    }

    /// A purely additive table — every member ranks the rows identically, only
    /// the level differs — has no interaction, so `λ` is zero and the members
    /// share an optimum.
    #[test]
    fn an_additive_table_has_no_interaction() {
        // row effect (0, 1, 2) + member effect (0, 10), replicated exactly.
        let grid: Vec<Vec<Vec<Real>>> = (0..3)
            .map(|r| {
                (0..2)
                    .map(|m| vec![r as Real + 10.0 * m as Real; 3])
                    .collect()
            })
            .collect();
        let refs: Vec<Vec<&[Real]>> = grid
            .iter()
            .map(|r| r.iter().map(Vec::as_slice).collect())
            .collect();
        let refs: Vec<&[&[Real]]> = refs.iter().map(Vec::as_slice).collect();
        let t = table_of(&refs);
        let d = t.decompose().expect("3x2 balanced, replicated");

        assert!(d.interaction_variance < 1e-9, "{d:?}");
        assert_eq!(d.lambda, Some(0.0));
        assert!(d.balanced);
        assert_eq!(d.cells, 6);
        // The member effect is real and recovered: two members 10 apart, so a
        // population variance of 25 about their mean.
        assert!((d.member_variance - 25.0).abs() < 1e-9, "{d:?}");
        // And the rows carry a genuine shared effect, which is what makes the
        // zero interaction meaningful rather than vacuous.
        assert!(d.row_variance > 0.5, "{d:?}");
    }

    /// Members that rank the rows in *opposite* orders share no optimum. The
    /// interaction dominates and `λ` goes to one — the case where a pooled
    /// winner is a compromise that suits neither member.
    #[test]
    fn opposed_members_drive_lambda_to_one() {
        let t = table_of(&[
            &[&[0.0, 0.0], &[6.0, 6.0]],
            &[&[2.0, 2.0], &[4.0, 4.0]],
            &[&[4.0, 4.0], &[2.0, 2.0]],
            &[&[6.0, 6.0], &[0.0, 0.0]],
        ]);
        let d = t.decompose().expect("4x2 balanced, replicated");

        assert_eq!(d.residual_variance, Some(0.0));
        assert!(d.interaction_variance > 1.0, "{d:?}");
        assert_eq!(d.lambda, Some(1.0));
        // Opposed members cancel in both margins: nothing shared to find.
        assert!(d.row_variance < 1e-9, "{d:?}");
        assert!(d.member_variance < 1e-9, "{d:?}");
    }

    /// The identifiability rule, which is the whole reason `lambda` is an
    /// `Option`: one observation per cell cannot separate disagreement from
    /// noise, so no number is reported.
    #[test]
    fn an_unreplicated_table_reports_no_lambda() {
        let t = table_of(&[
            &[&[0.0], &[6.0]],
            &[&[2.0], &[4.0]],
            &[&[4.0], &[2.0]],
            &[&[6.0], &[0.0]],
        ]);
        let d = t.decompose().expect("4x2, one observation per cell");

        assert_eq!(d.residual_variance, None);
        assert_eq!(d.lambda, None);
        assert!(d.shrunk(&t).is_none(), "no lambda means no shrunk surface");
        // The additive part is still fitted and still useful — `_z` needs no
        // replication, only the table.
        assert!(d.demeaned(&t).iter().all(Option::is_some));
    }

    /// Within-cell noise is *subtracted* from the interaction rather than
    /// counted as disagreement. Two members that agree exactly, measured
    /// noisily, must not read as members that disagree.
    #[test]
    fn within_cell_noise_is_not_counted_as_disagreement() {
        // Identical member columns; the only spread is inside each cell.
        let t = table_of(&[
            &[&[1.0, 3.0], &[1.0, 3.0]],
            &[&[3.0, 5.0], &[3.0, 5.0]],
            &[&[5.0, 7.0], &[5.0, 7.0]],
            &[&[7.0, 9.0], &[7.0, 9.0]],
        ]);
        let d = t.decompose().expect("4x2 balanced, replicated");

        assert!(d.residual_variance.expect("replicated") > 0.5, "{d:?}");
        assert!(d.interaction_variance < 1e-9, "{d:?}");
        assert_eq!(d.lambda, Some(0.0));
    }

    /// A ragged table — members that had not listed for some rows — is fitted
    /// rather than refused, and says so.
    #[test]
    fn a_ragged_table_is_fitted_and_flagged_unbalanced() {
        let mut t = ScoreTable::new(4, 3);
        for r in 0..4 {
            for m in 0..3 {
                // Drop one interior cell: member 2 never reported row 1.
                if r == 1 && m == 2 {
                    continue;
                }
                t.extend(r, m, [r as Real + m as Real, r as Real + m as Real + 1.0]);
            }
        }
        let d = t.decompose().expect("11 of 12 cells");

        assert!(!d.balanced);
        assert_eq!(d.cells, 11);
        assert_eq!(d.live_rows, 4);
        assert_eq!(d.live_members, 3);
        // The absent cell is a hole, never a zero — in the interactions and in
        // both derived surfaces. Row 1, member 2, in a 3-wide row-major layout.
        let hole = t.members() + 2;
        assert!(d.interactions[hole].is_none());
        assert!(d.demeaned(&t)[hole].is_none());
        assert!(d.shrunk(&t).expect("replicated")[hole].is_none());
    }

    /// Too sparse to fit is `None`, not a zero-filled answer. Both the cell
    /// floor and the "no degrees of freedom left for an interaction" case.
    #[test]
    fn a_table_too_sparse_to_fit_is_refused() {
        // Under MIN_CELLS.
        let t = table_of(&[&[&[1.0], &[2.0]], &[&[3.0], &[4.0]]]);
        assert!(t.decompose().is_none());

        // Enough cells, but a single live member — no two-way layout at all.
        let mut one_member = ScoreTable::new(8, 3);
        for r in 0..8 {
            one_member.push(r, 1, r as Real);
        }
        assert!(one_member.decompose().is_none());

        // Enough cells and both margins live, but `cells - rows - members + 1`
        // is zero: a staircase has no interaction degrees of freedom.
        let mut staircase = ScoreTable::new(6, 6);
        for r in 0..6 {
            staircase.push(r, r, r as Real);
            if r + 1 < 6 {
                staircase.push(r, r + 1, r as Real);
            }
        }
        assert!(staircase.decompose().is_none());
    }

    /// `shrunk` interpolates between the two poolings, and the endpoints are
    /// the ones the module claims: at `λ=0` every member's argmax is the same
    /// row, at `λ=1` each member gets its own.
    #[test]
    fn the_shrunk_surface_interpolates_between_both_poolings() {
        // Member 0 peaks at row 0, member 1 at row 3, and member 1's spread is
        // the larger — so the pooled argmax follows it.
        let t = table_of(&[
            &[&[10.0, 10.0], &[0.0, 0.0]],
            &[&[8.0, 8.0], &[4.0, 4.0]],
            &[&[6.0, 6.0], &[8.0, 8.0]],
            &[&[4.0, 4.0], &[20.0, 20.0]],
        ]);
        let d = t.decompose().expect("4x2 balanced, replicated");
        let argmax_for = |surface: &[Option<Real>], m: usize| -> usize {
            (0..t.rows())
                .filter(|&r| surface[r * t.members() + m].is_some())
                .max_by(|&a, &b| {
                    surface[a * t.members() + m]
                        .partial_cmp(&surface[b * t.members() + m])
                        .expect("finite")
                })
                .expect("some row")
        };

        // Complete pooling: one surface, so both members land on one row.
        let complete = Decomposition {
            lambda: Some(0.0),
            ..d.clone()
        }
        .shrunk(&t)
        .expect("lambda set");
        assert_eq!(argmax_for(&complete, 0), argmax_for(&complete, 1));

        // No pooling: each member back on its own column, and they differ.
        let none = Decomposition {
            lambda: Some(1.0),
            ..d.clone()
        }
        .shrunk(&t)
        .expect("lambda set");
        assert_eq!(argmax_for(&none, 0), 0);
        assert_eq!(argmax_for(&none, 1), 3);
    }

    /// A `NaN` in one cell must not take every sum of squares with it — it is
    /// dropped at the door, and the cell reads as short by one observation.
    #[test]
    fn a_non_finite_observation_is_dropped_not_propagated() {
        let mut t = ScoreTable::new(3, 2);
        for r in 0..3 {
            for m in 0..2 {
                t.extend(r, m, [r as Real + m as Real, r as Real + m as Real + 1.0]);
            }
        }
        t.push(0, 0, Real::NAN);
        t.push(0, 1, Real::INFINITY);
        assert_eq!(t.observations(), 12);

        let d = t.decompose().expect("3x2");
        assert!(d.grand_mean.is_finite());
        assert!(d.interaction_variance.is_finite());
        assert!(d.lambda.expect("replicated").is_finite());
    }
}
