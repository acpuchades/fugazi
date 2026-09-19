//! The pooled walk-forward (panel) bindings — `PanelBreadth` /
//! `DemeanedScore` / `ScoreTable` / `PanelDecomposition` / `PanelShrinkage` /
//! `PanelFold` / `MemberComposite` / `PanelWalkForwardResult` and the
//! `run_panel_walkforward` driver, split out of `spec.rs` along the same seam
//! the Rust side draws (`src/spec/{panel,shrinkage}.rs`), so the structural
//! correspondence a parity review leans on holds module-for-module.

use crate::prelude::*;
// The binding modules were one flat namespace before the split and still read
// as one: each pulls in its siblings, so a cross-module reference needs no path.
#[allow(unused_imports)]
use crate::carriers::*;
#[allow(unused_imports)]
use crate::classes::*;
#[allow(unused_imports)]
use crate::constructors::*;
#[allow(unused_imports)]
use crate::errors::SpecError;
#[allow(unused_imports)]
use crate::metrics::*;
#[allow(unused_imports)]
use crate::sources::*;
#[allow(unused_imports)]
use crate::spec::*;
#[allow(unused_imports)]
use crate::strategy::*;
#[allow(unused_imports)]
use crate::wallets::*;
use serde_json::Value as JsonValue;

// ---------------------------------------------------------------------------
// Pooled walk-forward
// ---------------------------------------------------------------------------

/// What a panel is worth as evidence: `(effective, mean_correlation, members,
/// pairs)`, with names.
///
/// Returned by both `PanelWalkForwardResult.effective_breadth` (correlation
/// between member *returns*) and `PanelDecomposition.selection_breadth`
/// (correlation between member *ranking surfaces*). Same quantity, same field
/// order, two things to correlate.
///
/// **Still a tuple where it counts.** It iterates, indexes and compares as
/// `(effective, mean_correlation, members, pairs)`, so
/// `eff, rho, n, pairs = result.effective_breadth` keeps working; `.members` is
/// simply the spelling that cannot be transposed.
// `skip_from_py_object`: these are results, never arguments — nothing
// accepts a PanelBreadth. The `Clone` is for returning by value.
#[pyclass(name = "PanelBreadth", module = "fugazi", skip_from_py_object)]
#[derive(Clone, Copy)]
pub(crate) struct PyPanelBreadth {
    pub(crate) effective: Real,
    pub(crate) mean_correlation: Real,
    pub(crate) members: usize,
    pub(crate) pairs: usize,
}

#[pymethods]
impl PyPanelBreadth {
    /// How many *independent* members the panel is worth —
    /// `members / (1 + (members - 1) * mean_correlation)`.
    #[getter]
    pub(crate) fn effective(&self) -> Real {
        self.effective
    }

    /// Mean pairwise correlation over the pairs that could be measured.
    /// Negative values are reported but floored at zero inside `effective`.
    #[getter]
    pub(crate) fn mean_correlation(&self) -> Real {
        self.mean_correlation
    }

    /// Members with enough data to be correlated against anything — the `M` in
    /// the formula, not the panel's declared size.
    #[getter]
    pub(crate) fn members(&self) -> usize {
        self.members
    }

    /// Pairs actually measured.
    #[getter]
    pub(crate) fn pairs(&self) -> usize {
        self.pairs
    }

    pub(crate) fn __iter__(&self, py: Python<'_>) -> PyResult<Py<PyAny>> {
        Ok(self.as_tuple(py)?.try_iter()?.into_any().unbind())
    }

    pub(crate) fn __len__(&self) -> usize {
        4
    }

    pub(crate) fn __getitem__(
        &self,
        py: Python<'_>,
        index: &Bound<'_, PyAny>,
    ) -> PyResult<Py<PyAny>> {
        // Through the object protocol, not `PyTuple::get_item`: that takes a
        // `usize`, which would silently drop negative indices and slices — two
        // things a caller reasonably expects of something that destructures.
        Ok(self.as_tuple(py)?.as_any().get_item(index)?.unbind())
    }

    pub(crate) fn __eq__(&self, py: Python<'_>, other: &Bound<'_, PyAny>) -> PyResult<bool> {
        self.as_tuple(py)?.eq(other)
    }

    pub(crate) fn __hash__(&self, py: Python<'_>) -> PyResult<isize> {
        self.as_tuple(py)?.hash()
    }

    pub(crate) fn __repr__(&self) -> String {
        format!(
            "PanelBreadth(effective={:.4}, mean_correlation={:.4}, members={}, pairs={})",
            self.effective, self.mean_correlation, self.members, self.pairs,
        )
    }
}

impl PyPanelBreadth {
    fn as_tuple<'py>(&self, py: Python<'py>) -> PyResult<Bound<'py, pyo3::types::PyTuple>> {
        use pyo3::IntoPyObject;
        (
            self.effective,
            self.mean_correlation,
            self.members,
            self.pairs,
        )
            .into_pyobject(py)
    }

    pub(crate) fn from_breadth(b: fugazi_core::spec::panel::Breadth) -> Self {
        Self {
            effective: b.effective,
            mean_correlation: b.mean_correlation,
            members: b.members,
            pairs: b.pairs,
        }
    }
}

/// A row's cross-member score with the member level removed:
/// `(mean, std, defined, members)`, with names.
///
/// `defined` and `members` are the coverage behind `mean` — a mean over 2 of 30
/// members and one over 30 of 30 are not the same evidence, and positionally
/// they render as "2 of 30" and "30 of 2" with equal plausibility. That is what
/// the names are for.
///
/// **Still a tuple where it counts** — it iterates, indexes and compares as
/// `(mean, std, defined, members)`.
// `skip_from_py_object`: these are results, never arguments — nothing
// accepts a DemeanedScore. The `Clone` is for returning by value.
#[pyclass(name = "DemeanedScore", module = "fugazi", skip_from_py_object)]
#[derive(Clone, Copy)]
pub(crate) struct PyDemeanedScore {
    pub(crate) mean: Real,
    pub(crate) std: Real,
    pub(crate) defined: usize,
    pub(crate) members: usize,
}

#[pymethods]
impl PyDemeanedScore {
    /// Cross-member mean of the demeaned cells — the key `shrink=` ranks on.
    #[getter]
    pub(crate) fn mean(&self) -> Real {
        self.mean
    }

    /// Population standard deviation across those same members.
    #[getter]
    pub(crate) fn std(&self) -> Real {
        self.std
    }

    /// Members that **reported** a value for this row.
    #[getter]
    pub(crate) fn defined(&self) -> usize {
        self.defined
    }

    /// Members in the panel. `defined <= members` always; the gap is members
    /// that ran but could not compute the metric.
    #[getter]
    pub(crate) fn members(&self) -> usize {
        self.members
    }

    pub(crate) fn __iter__(&self, py: Python<'_>) -> PyResult<Py<PyAny>> {
        Ok(self.as_tuple(py)?.try_iter()?.into_any().unbind())
    }

    pub(crate) fn __len__(&self) -> usize {
        4
    }

    pub(crate) fn __getitem__(
        &self,
        py: Python<'_>,
        index: &Bound<'_, PyAny>,
    ) -> PyResult<Py<PyAny>> {
        // Through the object protocol, not `PyTuple::get_item`: that takes a
        // `usize`, which would silently drop negative indices and slices — two
        // things a caller reasonably expects of something that destructures.
        Ok(self.as_tuple(py)?.as_any().get_item(index)?.unbind())
    }

    pub(crate) fn __eq__(&self, py: Python<'_>, other: &Bound<'_, PyAny>) -> PyResult<bool> {
        self.as_tuple(py)?.eq(other)
    }

    pub(crate) fn __hash__(&self, py: Python<'_>) -> PyResult<isize> {
        self.as_tuple(py)?.hash()
    }

    pub(crate) fn __repr__(&self) -> String {
        format!(
            "DemeanedScore(mean={:.4}, std={:.4}, defined={}, members={})",
            self.mean, self.std, self.defined, self.members,
        )
    }
}

impl PyDemeanedScore {
    fn as_tuple<'py>(&self, py: Python<'py>) -> PyResult<Bound<'py, pyo3::types::PyTuple>> {
        use pyo3::IntoPyObject;
        (self.mean, self.std, self.defined, self.members).into_pyobject(py)
    }
}

/// A row × member table of scores — the estimator's input, for callers that
/// pool a panel themselves.
///
/// `optimize(panel=…, shrink=True)` builds one of these internally and never
/// shows it. That is no use to a caller who reduces across members with its own
/// machinery: there is nothing for `shrink=` to be plumbed into, and reaching it
/// would mean giving up whatever made the caller's own pooling worth having.
/// This is the same estimator with the sweep taken off the front.
///
/// Rows are parameter points, members are whatever you pooled over. Each cell
/// holds that pair's **replicate** observations — measure the same
/// `(row, member)` over several sub-spans and push each one, because with a
/// single observation per cell "the members disagree" and "the backtests are
/// noisy" are the same sum of squares and no split exists.
///
/// **Ragged is expected.** A pair you never measured is simply an empty cell.
/// Never push a zero to stand in for one: a substituted zero is
/// indistinguishable from a measurement, and every statistic downstream would
/// rest on it.
///
/// ```py
/// t = ta.ScoreTable(rows=len(grid), members=len(panel))
/// for r, params in enumerate(grid):
///     for m, member in enumerate(panel):
///         t.extend(r, m, sharpe_per_window(params, member))   # the replicates
/// d = t.decompose()                    # None if too sparse to fit
/// d.summary.disagreement               # lambda
/// d.shrunk                             # per-member surface to select off
/// ```
#[pyclass(name = "ScoreTable", module = "fugazi")]
pub(crate) struct PyScoreTable {
    pub(crate) inner: fugazi_core::spec::shrinkage::ScoreTable,
}

#[pymethods]
impl PyScoreTable {
    /// An empty `rows × members` table.
    #[new]
    #[pyo3(signature = (rows, members))]
    pub(crate) fn new(rows: usize, members: usize) -> Self {
        Self {
            inner: fugazi_core::spec::shrinkage::ScoreTable::new(rows, members),
        }
    }

    /// Build from a nested `cells[row][member] -> sequence of replicates`.
    ///
    /// The shape is taken from the outer lengths, so a short row is an error
    /// rather than a silently narrower table — a ragged *input* is a bug, while
    /// a ragged *table* (empty cells) is ordinary and is spelled by passing an
    /// empty sequence.
    #[staticmethod]
    pub(crate) fn from_cells(cells: Vec<Vec<Vec<Real>>>) -> PyResult<Self> {
        let rows = cells.len();
        let members = cells.first().map_or(0, Vec::len);
        for (r, row) in cells.iter().enumerate() {
            if row.len() != members {
                return Err(PyValueError::new_err(format!(
                    "ScoreTable.from_cells: row {r} has {} members but row 0 has {members} — \
                     every row must span the same members; an unmeasured pair is an empty \
                     sequence, not a missing column",
                    row.len(),
                )));
            }
        }
        let mut inner = fugazi_core::spec::shrinkage::ScoreTable::new(rows, members);
        for (r, row) in cells.into_iter().enumerate() {
            for (m, replicates) in row.into_iter().enumerate() {
                inner.extend(r, m, replicates);
            }
        }
        Ok(Self { inner })
    }

    /// Record one observation. Out-of-range indices and non-finite values are
    /// dropped rather than raising — a `NaN` in one cell would otherwise take
    /// every sum of squares with it.
    pub(crate) fn push(&mut self, row: usize, member: usize, value: Real) {
        self.inner.push(row, member, value);
    }

    /// Record a cell's replicates in one call.
    pub(crate) fn extend(&mut self, row: usize, member: usize, values: Vec<Real>) {
        self.inner.extend(row, member, values);
    }

    #[getter]
    pub(crate) fn rows(&self) -> usize {
        self.inner.rows()
    }

    #[getter]
    pub(crate) fn members(&self) -> usize {
        self.inner.members()
    }

    /// One cell's replicates; empty when the pair was never measured.
    pub(crate) fn cell(&self, row: usize, member: usize) -> Vec<Real> {
        self.inner.cell(row, member).to_vec()
    }

    /// A cell's mean, or `None` when it holds nothing.
    pub(crate) fn cell_mean(&self, row: usize, member: usize) -> Option<Real> {
        self.inner.cell_mean(row, member)
    }

    /// Cells holding at least one observation.
    #[getter]
    pub(crate) fn populated(&self) -> usize {
        self.inner.populated()
    }

    /// Total observations across every cell.
    #[getter]
    pub(crate) fn observations(&self) -> usize {
        self.inner.observations()
    }

    /// Cells carrying enough replicates to speak to within-cell spread. Zero
    /// here is why `decompose().summary.disagreement` comes back `None`.
    #[getter]
    pub(crate) fn replicated_cells(&self) -> usize {
        self.inner.replicated_cells()
    }

    /// Fit the two-way layout, or `None` when the table cannot carry it.
    ///
    /// `None` on three conditions, all of them "there is not enough table",
    /// never "the answer is zero": fewer than 6 populated cells, fewer than two
    /// live rows or members, or no degrees of freedom left for an interaction
    /// once both margins are spent (`cells - rows - members + 1 <= 0`).
    /// [`populated`](Self::populated) and
    /// [`replicated_cells`](Self::replicated_cells) are how you tell which.
    pub(crate) fn decompose(&self, py: Python<'_>) -> PyResult<Option<Py<PyPanelDecomposition>>> {
        self.inner
            .decompose()
            .map(|fit| {
                Py::new(
                    py,
                    PyPanelDecomposition {
                        table: self.inner.clone(),
                        fit,
                    },
                )
            })
            .transpose()
    }

    pub(crate) fn __repr__(&self) -> String {
        format!(
            "ScoreTable(rows={}, members={}, populated={}, observations={})",
            self.inner.rows(),
            self.inner.members(),
            self.inner.populated(),
            self.inner.observations(),
        )
    }
}

/// A fitted two-way layout: the summary, and the two surfaces you act on.
///
/// Holds a copy of the table it was fitted from, so every reading below is a
/// plain attribute rather than a call you have to pass the table back into.
#[pyclass(name = "PanelDecomposition", module = "fugazi")]
pub(crate) struct PyPanelDecomposition {
    pub(crate) table: fugazi_core::spec::shrinkage::ScoreTable,
    pub(crate) fit: fugazi_core::spec::shrinkage::Decomposition,
}

#[pymethods]
impl PyPanelDecomposition {
    /// The headline reading — `disagreement` (λ), `parameter_matters`,
    /// `verdict`, and the variance components. Same class
    /// `Sweep.shrinkage` returns.
    #[getter]
    pub(crate) fn summary(&self, py: Python<'_>) -> PyResult<Py<PyPanelShrinkage>> {
        Py::new(
            py,
            PyPanelShrinkage {
                inner: self.fit.summary(&self.table),
            },
        )
    }

    /// Cell means with the **member level removed**, as `rows × members`.
    ///
    /// This is what to rank on when you want a cross-member spread to mean
    /// "this parameter set ranks consistently well" rather than "these members
    /// are alike": the member effect is identical for every row, so it carries
    /// no ranking information, yet it still inflates the spread — and
    /// unequally, since rows differ in which members they are defined on.
    ///
    /// `None` where the cell is unpopulated, so your own support counts stay
    /// honest. Needs no replication — only the table.
    #[getter]
    pub(crate) fn demeaned(&self) -> Vec<Vec<Option<Real>>> {
        Self::nest(self.fit.demeaned(&self.table), self.table.members())
    }

    /// The surface each member selects its own parameters off under partial
    /// pooling: `mu + alpha_r + lambda * gamma_rm`, as `rows × members`.
    ///
    /// At `lambda = 0` every column is identical and every member picks the
    /// pooled winner; at `lambda = 1` each gets its own cell means back. In
    /// between, a member whose column is noisy is pulled toward the consensus
    /// while one that genuinely disagrees keeps disagreeing. Take an argmax
    /// down each column.
    ///
    /// **`None` when `disagreement` is** — without a lambda there is no
    /// defensible surface, and falling back to either pooling extreme would be
    /// choosing a pooling policy by accident.
    #[getter]
    pub(crate) fn shrunk(&self) -> Option<Vec<Vec<Option<Real>>>> {
        self.fit
            .shrunk(&self.table)
            .map(|s| Self::nest(s, self.table.members()))
    }

    /// The shared parameter effect per row, as a deviation from the grand mean.
    /// `None` for a row with no populated cell.
    #[getter]
    pub(crate) fn row_effects(&self) -> Vec<Option<Real>> {
        self.fit.row_effects.clone()
    }

    /// The member level per member — the nuisance term `demeaned` removes.
    #[getter]
    pub(crate) fn member_effects(&self) -> Vec<Option<Real>> {
        self.fit.member_effects.clone()
    }

    /// What the additive part misses, as `rows × members` — the disagreement
    /// itself, before it is shrunk.
    #[getter]
    pub(crate) fn interactions(&self) -> Vec<Vec<Option<Real>>> {
        Self::nest(self.fit.interactions.clone(), self.table.members())
    }

    /// The grand mean the effects are deviations from.
    #[getter]
    pub(crate) fn grand_mean(&self) -> Real {
        self.fit.grand_mean
    }

    /// How many **independent searches over the grid** per-member selection
    /// amounts to, as `(effective, mean_correlation, members, pairs)`.
    ///
    /// Multiply your candidate count by `effective` before deflating: letting
    /// every member select for itself takes the maximum over more draws than
    /// the candidate count alone admits. `1.0` when the members agree (one
    /// shared surface, so one search), up to the member count when they share
    /// nothing.
    ///
    /// `None` alongside a `None` `shrunk` — with no surface there is nothing to
    /// correlate.
    #[getter]
    pub(crate) fn selection_breadth(&self) -> Option<PyPanelBreadth> {
        fugazi_core::spec::panel::selection_breadth(&self.fit, &self.table)
            .map(PyPanelBreadth::from_breadth)
    }

    pub(crate) fn __repr__(&self) -> String {
        let s = self.fit.summary(&self.table);
        let lambda = s
            .lambda
            .map_or_else(|| "None".to_string(), |l| format!("{l:.3}"));
        format!(
            "PanelDecomposition(disagreement={lambda}, cells={}, rows={}, members={})",
            s.cells, s.live_rows, s.live_members,
        )
    }
}

impl PyPanelDecomposition {
    /// Row-major flat vector to `rows × members` nesting.
    ///
    /// The Rust side is flat because it indexes hot loops; a Python caller
    /// wants `surface[row][member]` and should not be doing the arithmetic —
    /// getting that stride wrong is silent, not loud.
    fn nest(flat: Vec<Option<Real>>, members: usize) -> Vec<Vec<Option<Real>>> {
        if members == 0 {
            return Vec::new();
        }
        flat.chunks(members).map(<[_]>::to_vec).collect()
    }
}

/// How much of the spread between panel members is real disagreement rather
/// than backtest noise — the reading `shrink=` acts on.
///
/// A pooled sweep ranks one parameter set across every member. That is the
/// right thing to do only when the members *share* an optimum; when they do
/// not, the pooled winner is a compromise that can be worse on every member
/// than that member's own answer. This is the number that says which case you
/// are in.
///
/// The headline is [`disagreement`](Self::disagreement) — written `λ` in the
/// docs and the CSVs, and spelled out here because `lambda` is a Python
/// keyword and `sweep.shrinkage.lambda` would be a `SyntaxError`.
#[pyclass(name = "PanelShrinkage", module = "fugazi")]
pub(crate) struct PyPanelShrinkage {
    pub(crate) inner: fugazi_core::spec::shrinkage::Summary,
}

#[pymethods]
impl PyPanelShrinkage {
    /// `λ` in `0..=1`: the share of the spread between members that is genuine
    /// disagreement about the optimum rather than estimation noise.
    ///
    /// `0.0` — the members agree; pooling is buying variance reduction.
    /// `1.0` — they are separate problems; the pooled winner suits nobody.
    ///
    /// **`None` is not zero.** It means the table carried no within-cell
    /// replication, so disagreement and noise are literally the same sum of
    /// squares and no split exists to report — a different statement from "the
    /// members agree perfectly". Pass `windowed=` in a sweep to supply the
    /// replication; under `walkforward=` each fold splits its own in-sample
    /// window and needs no extra argument. Every other component below is
    /// still defined and still reported.
    #[getter]
    pub(crate) fn disagreement(&self) -> Option<Real> {
        self.inner.lambda
    }

    /// Whether the swept parameter moves this metric at all.
    ///
    /// Read it *with* [`disagreement`](Self::disagreement), never instead of
    /// it. `λ` compares disagreement against noise and says nothing about
    /// whether there was a signal to disagree over: on a grid that barely moves
    /// the metric, a high `λ` means the members disagree about which of several
    /// equivalent parameter sets is marginally best, which is not the finding
    /// it looks like. [`verdict`](Self::verdict) folds this in so the prose
    /// cannot be read without it.
    #[getter]
    pub(crate) fn parameter_matters(&self) -> bool {
        self.inner.parameter_matters()
    }

    /// The one-line reading, caveat included.
    ///
    /// Carries the same words the CLI prints, and appends the
    /// grid-barely-moves-this-metric warning when
    /// [`parameter_matters`](Self::parameter_matters) is false — so a caller
    /// who reports only this cannot report a misleading `λ`.
    #[getter]
    pub(crate) fn verdict(&self) -> String {
        let base = fugazi_core::spec::shrinkage::verdict(self.inner.lambda);
        if self.inner.lambda.is_some() && !self.inner.parameter_matters() {
            format!("{base} — but the grid barely moves this metric")
        } else {
            base.to_string()
        }
    }

    /// Replicated cells over populated cells, in `0..=1` — how much of the
    /// table actually backs `disagreement`. A `λ` resting on three cells of
    /// ninety and one resting on all ninety are not the same evidence.
    #[getter]
    pub(crate) fn support(&self) -> Real {
        self.inner.support
    }

    /// Populated `(row, member)` cells the fit rests on.
    #[getter]
    pub(crate) fn cells(&self) -> usize {
        self.inner.cells
    }

    /// Grid rows with at least one populated cell.
    #[getter]
    pub(crate) fn live_rows(&self) -> usize {
        self.inner.live_rows
    }

    /// Members with at least one populated cell.
    #[getter]
    pub(crate) fn live_members(&self) -> usize {
        self.inner.live_members
    }

    /// Variance of the shared parameter effect — how much the parameter moves
    /// the metric at all, before any member-specific structure.
    #[getter]
    pub(crate) fn row_variance(&self) -> Real {
        self.inner.row_variance
    }

    /// Variance of the per-member level. This is the nuisance term: identical
    /// for every row, so it carries no ranking information, which is why
    /// `shrink=` ranks on the member-demeaned score instead.
    #[getter]
    pub(crate) fn member_variance(&self) -> Real {
        self.inner.member_variance
    }

    /// `τ²_γ` — the parameter × member interaction, bias-corrected for the
    /// sampling noise its cell means carry and floored at zero.
    #[getter]
    pub(crate) fn interaction_variance(&self) -> Real {
        self.inner.interaction_variance
    }

    /// `σ²_ε` — pooled within-cell variance, or `None` on an unreplicated
    /// table where it cannot be told apart from the interaction.
    #[getter]
    pub(crate) fn residual_variance(&self) -> Option<Real> {
        self.inner.residual_variance
    }

    /// Harmonic mean replicate count over the replicated cells.
    #[getter]
    pub(crate) fn mean_replicates(&self) -> Real {
        self.inner.mean_replicates
    }

    /// Whether every live `(row, member)` pair was populated. An unbalanced
    /// table is fitted all the same, but its components are method-of-moments
    /// rather than exact.
    #[getter]
    pub(crate) fn balanced(&self) -> bool {
        self.inner.balanced
    }

    /// Both halves of the reading in one line, so a bare `print()` cannot show
    /// `λ` without its caveat.
    pub(crate) fn __repr__(&self) -> String {
        let lambda = self
            .inner
            .lambda
            .map_or_else(|| "None".to_string(), |l| format!("{l:.3}"));
        format!(
            "PanelShrinkage(disagreement={lambda}, support={:.2}, cells={}, \
             parameter_matters={}, verdict={:?})",
            self.inner.support,
            self.inner.cells,
            if self.inner.parameter_matters() {
                "True"
            } else {
                "False"
            },
            self.verdict(),
        )
    }
}

impl PyPanelShrinkage {
    pub(crate) fn wrap(
        py: Python<'_>,
        summary: Option<fugazi_core::spec::shrinkage::Summary>,
    ) -> PyResult<Option<Py<Self>>> {
        summary.map(|inner| Py::new(py, Self { inner })).transpose()
    }
}

/// One fold of a pooled walk-forward: the parameter set that won this fold on
/// the **pooled** in-sample score, and the per-member documents behind it.
///
/// `is_range` / `oos_range` are indices into the panel's shared clock — the
/// sorted union of every member's bar times — not into any one member's bars.
/// That is what makes fold *k* the same span for every member of a ragged
/// panel; see `PanelWalkForwardResult.axis_len`.
#[pyclass(name = "PanelFold", module = "fugazi")]
pub(crate) struct PyPanelFold {
    pub(crate) fold: usize,
    pub(crate) is_range: (usize, usize),
    pub(crate) oos_range: (usize, usize),
    pub(crate) axis_columns: Vec<String>,
    pub(crate) axis_values: Vec<Option<JsonValue>>,
    pub(crate) is_members: Vec<(String, SpecMetrics)>,
    pub(crate) oos_members: Vec<(String, SpecMetrics)>,
    pub(crate) is_smoothed: Option<Real>,
    pub(crate) is_support: Option<Real>,
    /// Under `shrink=`, this fold's own decomposition — estimated from
    /// sub-spans of *this fold's* in-sample window, so it rests only on data
    /// the fold could see.
    pub(crate) shrinkage: Option<fugazi_core::spec::shrinkage::Summary>,
    /// Under `shrink=`, `(member, axis values)` for each member's own pick.
    pub(crate) member_winners: Vec<(String, Vec<Option<JsonValue>>)>,
    /// Members whose pick differed from the pooled winner in this fold.
    pub(crate) departed: Vec<String>,
}

#[pymethods]
impl PyPanelFold {
    #[getter]
    pub(crate) fn fold(&self) -> usize {
        self.fold
    }
    /// In-sample range on the panel's shared clock, `(start, end)`.
    #[getter]
    pub(crate) fn is_range(&self) -> (usize, usize) {
        self.is_range
    }
    /// Post-embargo out-of-sample range on the panel's shared clock.
    #[getter]
    pub(crate) fn oos_range(&self) -> (usize, usize) {
        self.oos_range
    }
    /// The winning parameter set, as `{axis: value}`.
    #[getter]
    pub(crate) fn values(&self, py: Python<'_>) -> PyResult<Py<pyo3::types::PyDict>> {
        let d = pyo3::types::PyDict::new(py);
        for (name, v) in self.axis_columns.iter().zip(&self.axis_values) {
            match v {
                Some(val) => d.set_item(name, json_to_py(py, val)?)?,
                None => d.set_item(name, py.None())?,
            }
        }
        Ok(d.into())
    }
    /// Per-member in-sample documents for the winning row, keyed by member.
    ///
    /// Only members with bars in this fold's window appear. A member that had
    /// not listed yet is **absent**, never present-and-zero — which is what
    /// makes `len(fold.metrics_is)` a usable support count.
    #[getter]
    pub(crate) fn metrics_is(&self, py: Python<'_>) -> PyResult<Py<PyAny>> {
        members_to_py(py, &self.is_members)
    }
    /// Per-member out-of-sample documents for the winning row.
    #[getter]
    pub(crate) fn metrics_oos(&self, py: Python<'_>) -> PyResult<Py<PyAny>> {
        members_to_py(py, &self.oos_members)
    }
    /// Members with bars in this fold's in-sample window.
    #[getter]
    pub(crate) fn is_support_members(&self) -> usize {
        self.is_members.len()
    }
    /// Members with bars in this fold's out-of-sample window.
    #[getter]
    pub(crate) fn oos_support_members(&self) -> usize {
        self.oos_members.len()
    }
    /// Under `smooth=`, the neighbourhood average of the winning row's pooled
    /// IS ranking key — the value this fold was actually selected on.
    #[getter]
    pub(crate) fn is_smoothed(&self) -> Option<Real> {
        self.is_smoothed
    }
    /// The neighbourhood support behind `is_smoothed`.
    #[getter]
    pub(crate) fn is_support(&self) -> Option<Real> {
        self.is_support
    }

    /// Under `shrink=`, this fold's own [`PanelShrinkage`] — or `None` when the
    /// sweep was not shrunk, or the fold's in-sample window was too short to
    /// split into replicates.
    ///
    /// Per fold rather than once for the run, because a panel that agreed early
    /// and split later is a different story from one that never agreed, and a
    /// single number tells neither.
    ///
    /// **Deliberately conservative, and lower than the run-wide reading.** A
    /// fold estimates from sub-spans of its own in-sample window — which is
    /// what keeps it lookahead-free — but a metric measured over a short span
    /// is itself noisy, and that noise lands in the denominator. Per-fold
    /// `disagreement` of 0.275 / 0.0 / 0.0 against 0.815 for
    /// `PanelWalkForwardResult.shrinkage` is an ordinary spread, not a
    /// contradiction: it is the fold saying it cannot yet separate disagreement
    /// from noise on its own evidence. Label which is which if you render both;
    /// `docs/CLI.md` carries the longer version.
    #[getter]
    pub(crate) fn shrinkage(&self, py: Python<'_>) -> PyResult<Option<Py<PyPanelShrinkage>>> {
        PyPanelShrinkage::wrap(py, self.shrinkage)
    }

    /// Under `shrink=`, each member's own parameters for this fold as
    /// `{member: {axis: value}}` — the same shape as
    /// [`values`](Self::values), which is the pooled winner they are being
    /// compared against.
    ///
    /// Empty when the sweep was not shrunk. At `disagreement == 0` every entry
    /// equals `values`, which is complete pooling spelled out.
    #[getter]
    pub(crate) fn member_winners(&self, py: Python<'_>) -> PyResult<Py<pyo3::types::PyDict>> {
        winners_to_py(py, &self.axis_columns, &self.member_winners)
    }

    /// Members whose pick differed from the pooled winner in this fold.
    ///
    /// The useful half of [`member_winners`](Self::member_winners) when you
    /// only want to know *whether* the panel split and who split: "one member
    /// went its own way" and "every member did" are different findings that a
    /// mean `λ` renders identically.
    #[getter]
    pub(crate) fn departed(&self) -> Vec<String> {
        self.departed.clone()
    }

    pub(crate) fn __repr__(&self) -> String {
        format!(
            "PanelFold(fold={}, is={:?}, oos={:?}, members={}/{})",
            self.fold,
            self.is_range,
            self.oos_range,
            self.oos_members.len(),
            self.is_members.len(),
        )
    }
}

/// `{member: {axis: value}}` from per-member axis rows sparse across `columns`.
///
/// Shared by `Sweep.member_winners` and `PanelFold.member_winners` so the two
/// cannot drift into different shapes for the same idea — and shaped as a dict
/// of dicts rather than a list of records to match `PanelFold.values`, which is
/// the pooled winner a caller compares these against.
pub(crate) fn winners_to_py(
    py: Python<'_>,
    columns: &[String],
    winners: &[(String, Vec<Option<JsonValue>>)],
) -> PyResult<Py<pyo3::types::PyDict>> {
    let out = pyo3::types::PyDict::new(py);
    for (member, values) in winners {
        let d = pyo3::types::PyDict::new(py);
        for (name, v) in columns.iter().zip(values) {
            match v {
                Some(val) => d.set_item(name, json_to_py(py, val)?)?,
                None => d.set_item(name, py.None())?,
            }
        }
        out.set_item(member, d)?;
    }
    Ok(out.into())
}

fn members_to_py(py: Python<'_>, members: &[(String, SpecMetrics)]) -> PyResult<Py<PyAny>> {
    let d = pyo3::types::PyDict::new(py);
    for (name, m) in members {
        d.set_item(name, metrics_to_py(py, m)?)?;
    }
    Ok(d.into_any().unbind())
}

/// One panel member's stitched out-of-sample composite.
#[pyclass(name = "MemberComposite", module = "fugazi")]
pub(crate) struct PyMemberComposite {
    pub(crate) member: String,
    pub(crate) equity: Vec<Real>,
    pub(crate) fills: Vec<fugazi_core::Fill<Symbol>>,
    pub(crate) metrics: SpecMetrics,
}

#[pymethods]
impl PyMemberComposite {
    #[getter]
    pub(crate) fn member(&self) -> String {
        self.member.clone()
    }
    #[getter]
    pub(crate) fn equity(&self) -> Vec<Real> {
        self.equity.clone()
    }
    #[getter]
    pub(crate) fn fills(&self) -> Vec<PyFill> {
        self.fills
            .iter()
            .map(|f| PyFill { inner: f.clone() })
            .collect()
    }
    #[getter]
    pub(crate) fn metrics(&self, py: Python<'_>) -> PyResult<Py<PyAny>> {
        metrics_to_py(py, &self.metrics)
    }
    pub(crate) fn __repr__(&self) -> String {
        format!(
            "MemberComposite(member={:?}, bars={})",
            self.member,
            self.equity.len()
        )
    }
}

/// The result of a pooled walk-forward (`ta.optimize(..., panel=...,
/// walkforward=(is, oos))`).
///
/// One parameter set is chosen per fold on the **pooled** in-sample score and
/// applied out-of-sample to every member, so all the composites switch
/// parameters on the same dates.
///
/// There is deliberately no single netted composite curve: netting `M` members
/// into one account needs a weighting and a rebalance cadence, which is an
/// allocation policy fugazi expresses explicitly with `portfolio:` rather than
/// inventing inside `optimize`. Use `pooled(metric)` for the cross-member
/// headline, and `composites` for the per-instrument curves.
#[pyclass(name = "PanelWalkForwardResult", module = "fugazi")]
pub(crate) struct PyPanelWalkForwardResult {
    pub(crate) is_bars: usize,
    pub(crate) oos_bars: usize,
    pub(crate) embargo_bars: usize,
    pub(crate) prefix_skip: usize,
    pub(crate) axis_len: usize,
    pub(crate) members: Vec<String>,
    pub(crate) folds: Vec<Py<PyPanelFold>>,
    pub(crate) composites: Vec<Py<PyMemberComposite>>,
    pub(crate) composite_members: Vec<fugazi_core::spec::panel::PanelMetrics>,
    pub(crate) columns: Vec<String>,
    pub(crate) metric_columns: Vec<(String, String)>,
    pub(crate) cash: Real,
    /// `(effective, mean_correlation, members, pairs)`, computed once at
    /// construction — it is a scalar property of the finished panel rather than
    /// of any row, so recomputing it per access would re-correlate every pair
    /// to arrive at the same number.
    pub(crate) breadth: Option<(Real, Real, usize, usize)>,
    /// `(member, fold count)` for every member that departed at least once,
    /// most-frequent first — the order `PanelWalkForward::departures` sorts in.
    pub(crate) departures: Vec<(String, usize)>,
    /// The run-wide decomposition, folds as replicates.
    pub(crate) shrinkage: Option<fugazi_core::spec::shrinkage::Summary>,
}

#[pymethods]
impl PyPanelWalkForwardResult {
    #[getter]
    pub(crate) fn is_bars(&self) -> usize {
        self.is_bars
    }
    #[getter]
    pub(crate) fn oos_bars(&self) -> usize {
        self.oos_bars
    }
    #[getter]
    pub(crate) fn embargo_bars(&self) -> usize {
        self.embargo_bars
    }
    /// Bars trimmed off the head of the shared clock for grid-wide readiness —
    /// the pooled analogue of `WalkForwardResult.prefix_skip`.
    ///
    /// Measured from the point the **first** member becomes ready, not the
    /// last: waiting for every member would truncate the panel's history to its
    /// most recent listing. Early folds therefore rest on fewer members, which
    /// `PanelFold.is_support_members` reports rather than hides.
    #[getter]
    pub(crate) fn prefix_skip(&self) -> usize {
        self.prefix_skip
    }
    /// Length of the panel's shared clock — the union of every member's bar
    /// times. Fold ranges index into this, not into any one member's bars.
    #[getter]
    pub(crate) fn axis_len(&self) -> usize {
        self.axis_len
    }
    /// The panel's member names, in order.
    #[getter]
    pub(crate) fn members(&self) -> Vec<String> {
        self.members.clone()
    }
    #[getter]
    pub(crate) fn columns(&self) -> Vec<String> {
        self.columns.clone()
    }
    #[getter]
    pub(crate) fn metric_columns(&self) -> Vec<(String, String)> {
        self.metric_columns.clone()
    }
    #[getter]
    pub(crate) fn cash(&self) -> Real {
        self.cash
    }
    /// How many *independent* members this panel's results are worth:
    /// `(effective, mean_correlation, members, pairs)`, or `None` when fewer
    /// than two members shared enough history to be correlated at all.
    ///
    /// A pooled row reports `N` hypotheses rather than `N x M`, which is the
    /// honest count — and it invites the reading that `M` members are `M`
    /// pieces of evidence. For a panel drawn from one market's worth of
    /// instruments they are not: at an average pairwise correlation of 0.8,
    /// thirty members are worth about 1.2, and a pooled Sharpe over them
    /// deserves roughly the confidence of a single backtest. The reading is
    /// `M / (1 + (M - 1) * rho_bar)`.
    ///
    /// Measured on the **composites' own returns**, not on the members' price
    /// series: what a pooled figure rests on is how much the results co-moved,
    /// and a strategy trading two correlated markets at different times earns
    /// more independence than their prices would suggest.
    ///
    /// Reported, never applied. What to do with it — deflate against it, widen
    /// an interval, or go and find less correlated members — is a decision the
    /// caller has the context to make and this crate does not.
    #[getter]
    pub(crate) fn effective_breadth(&self) -> Option<PyPanelBreadth> {
        self.breadth.map(
            |(effective, mean_correlation, members, pairs)| PyPanelBreadth {
                effective,
                mean_correlation,
                members,
                pairs,
            },
        )
    }

    /// Members that departed from the pooled winner at least once, and in how
    /// many folds — `{member: folds}`, most-frequent first.
    ///
    /// Empty when the run was not shrunk, and **also** when the panel agreed
    /// throughout. That second case is a real result, not an absence: complete
    /// pooling was already each member's own answer.
    ///
    /// This is the reading a run-level `λ` flattens. "One member went its own
    /// way in every fold" and "everyone drifted once" can produce the same mean
    /// disagreement and mean very different things.
    #[getter]
    pub(crate) fn departures(&self, py: Python<'_>) -> PyResult<Py<pyo3::types::PyDict>> {
        let d = pyo3::types::PyDict::new(py);
        for (member, folds) in &self.departures {
            d.set_item(member, folds)?;
        }
        Ok(d.into())
    }

    /// The panel's `λ` over the **whole run**, with folds as the replicate axis.
    ///
    /// Free, so reported without `shrink=`: every fold already measures every
    /// `(row, member)` in-sample to rank the grid.
    ///
    /// Deliberately *not* what any fold selected on — a component estimated
    /// over every fold and applied inside fold 1 would let fold 10's data pick
    /// fold 1's winner. Use `PanelFold.shrinkage` for the lookahead-free
    /// per-fold estimate each fold acted on; this one describes the run after
    /// the fact and is better powered.
    ///
    /// Better powered means it will read **higher** than the per-fold numbers,
    /// which rest on a handful of short sub-spans and are conservative as a
    /// result. Expect the two to differ; it is not a bug. See
    /// `PanelFold.shrinkage`.
    #[getter]
    pub(crate) fn shrinkage(&self, py: Python<'_>) -> PyResult<Option<Py<PyPanelShrinkage>>> {
        PyPanelShrinkage::wrap(py, self.shrinkage)
    }

    #[getter]
    pub(crate) fn folds(&self, py: Python<'_>) -> Vec<Py<PyPanelFold>> {
        self.folds.iter().map(|f| f.clone_ref(py)).collect()
    }
    /// One stitched out-of-sample composite per member, in panel order.
    #[getter]
    pub(crate) fn composites(&self, py: Python<'_>) -> Vec<Py<PyMemberComposite>> {
        self.composites.iter().map(|c| c.clone_ref(py)).collect()
    }

    /// Pool one metric across the per-member composites:
    /// `(mean, std, defined, members)`, or `None` when no member reported it.
    ///
    /// The mean is over the members that reported — a member with no trades has
    /// no win rate and is dropped rather than counted as zero — so `defined` is
    /// what separates a well-supported number from a mean over two survivors.
    pub(crate) fn pooled(&self, metric: &str) -> PyResult<Option<(Real, Real, usize, usize)>> {
        let sample = self
            .composite_members
            .first()
            .ok_or_else(|| PyValueError::new_err("pooled(): the panel has no members"))?;
        let (path, _) = fugazi_core::spec::metrics::resolve_metric(metric, &sample.metrics)
            .map_err(|e| PyValueError::new_err(format!("pooled(): {e:#}")))?;
        Ok(
            fugazi_core::spec::panel::pool_metric(&self.composite_members, &path)
                .map(|p| (p.mean, p.std, p.defined, p.members)),
        )
    }

    /// The number of folds — `len(result)` == `len(result.folds)`.
    pub(crate) fn __len__(&self) -> usize {
        self.folds.len()
    }
    /// Iterate the folds, so `for fold in result` needs no `.folds` detour.
    pub(crate) fn __iter__(&self, py: Python<'_>) -> PyResult<Py<PyAny>> {
        crate::classes::iter_over(py, self.folds(py))
    }
    /// Index or slice the folds — `result[0]`, `result[-1]`, `result[:2]`.
    pub(crate) fn __getitem__(
        &self,
        py: Python<'_>,
        index: &Bound<'_, PyAny>,
    ) -> PyResult<Py<PyAny>> {
        let list = pyo3::types::PyList::new(py, self.folds(py))?;
        Ok(list.as_any().get_item(index)?.unbind())
    }
    pub(crate) fn __repr__(&self) -> String {
        format!(
            "PanelWalkForwardResult(folds={}, members={}, is={}, oos={}, embargo={})",
            self.folds.len(),
            self.members.len(),
            self.is_bars,
            self.oos_bars,
            self.embargo_bars,
        )
    }
}

/// The pooled walk-forward driver — the `panel=` peer of [`run_walkforward`].
#[allow(clippy::too_many_arguments)]
pub(crate) fn run_panel_walkforward(
    py: Python<'_>,
    detected: &str,
    base_value: &JsonValue,
    members: &[PanelMember],
    // When set, each member's key is substituted for this `!param` before its
    // spec is built — as a JSON string for a `str` key, as a number for an
    // `int`/`float` one. Rooting every member on its own series is the usual
    // use; pooling over a numeric parameter is the other. The Python twin of
    // the CLI's `--pooled`.
    panel_axis: Option<&str>,
    cost_config: &fugazi_core::spec::costs::CostConfig,
    subgrids: Vec<spec_optimize::Subgrid>,
    is_bars: usize,
    oos_bars: usize,
    embargo_bars: usize,
    metric_names: &[String],
    best_by: Option<&str>,
    risk_aversion: Real,
    smoothing: Option<&spec_optimize::Smoothing>,
    // Partial pooling — see `fugazi_core::spec::shrinkage`. Each fold estimates
    // its own `λ` from sub-spans of its in-sample window and lets each member
    // depart from the pooled winner by that much.
    shrink: bool,
    jobs: Option<usize>,
    cash: Real,
    max_gross: Option<Real>,
    leverage: Real,
    margin_rate: Real,
    maintenance_margin: Option<Real>,
    bars_per_year: Real,
    risk_free_rate: Real,
    seconds_per_bar: Option<Real>,
) -> PyResult<Py<PyAny>> {
    use fugazi_core::spec::panel;

    // Each member's own bar clock, read off its snapshots. Refused here rather
    // than deep in the kernel so a stream with no `time` names the member.
    let axes: Vec<panel::MemberAxis> = members
        .iter()
        .map(|m| panel::MemberAxis::from_snapshots(&m.name, &m.snaps))
        .collect::<anyhow::Result<Vec<_>>>()
        .map_err(|e| SpecError::new_err(format!("pooled walkforward: {e:#}")))?;

    let interrupt = crate::classes::SweepInterrupt::new();
    let result = crate::classes::run_watched(
        py,
        &interrupt,
        || -> anyhow::Result<panel::PanelWalkForward> {
            let needs_probe_feed = matches!(detected, "basket" | "multi");
            let ctx = spec_backtest::EvalContext {
                cash,
                max_gross,
                leverage,
                margin_rate,
                maintenance_margin,
                bars_per_year,
                risk_free_rate,
                cost_config,
                effective_freq: None,
                stream: None,
                windowed: None,
                seconds_per_bar,
                mc: None,
                warmup_bars: None,
            };
            let ctx_ref = &ctx;
            // One schema per member: members are different instruments, so
            // their overlay columns need not agree.
            let schemas: Vec<_> = members
                .iter()
                .map(|m| spec_backtest::schema_from_snapshots(&m.snaps))
                .collect();

            let member_params = |params: &std::collections::HashMap<String, JsonValue>,
                                 m: usize|
             -> std::collections::HashMap<String, JsonValue> {
                let mut p = params.clone();
                if let Some(axis) = panel_axis {
                    p.insert(axis.to_string(), members[m].axis.clone());
                }
                p
            };
            let probe_readiness = |params: &std::collections::HashMap<String, JsonValue>,
                                   m: usize|
             -> anyhow::Result<usize> {
                let params = member_params(params, m);
                let value = fugazi_core::spec::params::substitute(base_value.clone(), &params)?;
                let spec = spec_from_value(value, detected)?;
                let mut built = spec
                    .try_build(cash, &schemas[m], None)
                    .map_err(spec_backtest::build_error)?;
                if needs_probe_feed && let Some(first) = members[m].snaps.first() {
                    built.update(first.clone());
                }
                Ok(built.stable_bars())
            };

            let run_backtest = |params: &std::collections::HashMap<String, JsonValue>,
                                m: usize|
             -> anyhow::Result<fugazi_core::RunReport<Symbol>> {
                if interrupt.should_stop() {
                    anyhow::bail!("interrupted");
                }
                let params = member_params(params, m);
                let value = fugazi_core::spec::params::substitute(base_value.clone(), &params)?;
                let spec = spec_from_value(value, detected)?;
                spec_backtest::check_member_universe_pub(
                    &spec,
                    &members[m].name,
                    &members[m].snaps,
                )
                .map_err(spec_backtest::build_error)?;
                spec_backtest::measured_report_any(&spec, &members[m].snaps, ctx_ref)
                    .map_err(spec_backtest::build_error)
            };

            panel::panel_walkforward(
                subgrids,
                axes,
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
                risk_aversion,
                smoothing,
                shrink,
                jobs,
                cash,
            )
        },
    )
    .map_err(|e| SpecError::new_err(format!("pooled walkforward: {e:#}")));
    let result = interrupt.raise_over(result)?;

    let columns = result.union_columns.clone();
    let metric_columns = result.metric_columns.clone();
    let mut fold_objs: Vec<Py<PyPanelFold>> = Vec::with_capacity(result.fold_rows.len());
    for row in &result.fold_rows {
        let to_pairs = |ms: &[panel::PanelMetrics]| -> Vec<(String, SpecMetrics)> {
            ms.iter()
                .map(|m| (m.member.clone(), m.metrics.clone()))
                .collect()
        };
        fold_objs.push(Py::new(
            py,
            PyPanelFold {
                fold: row.fold,
                is_range: (row.is.start, row.is.end),
                oos_range: (row.oos.start, row.oos.end),
                axis_columns: columns.clone(),
                axis_values: row.values.clone(),
                is_members: to_pairs(&row.is_members),
                oos_members: to_pairs(&row.oos_members),
                is_smoothed: row.is_smoothed.and_then(|s| s.value),
                is_support: row.is_smoothed.and_then(|s| s.support),
                shrinkage: row.shrinkage,
                member_winners: row
                    .member_winners
                    .iter()
                    .map(|w| (w.member.clone(), w.values.clone()))
                    .collect(),
                departed: row
                    .member_winners
                    .iter()
                    .filter(|w| w.departed)
                    .map(|w| w.member.clone())
                    .collect(),
            },
        )?);
    }
    let composite_members = result.composite_members();
    let mut composite_objs: Vec<Py<PyMemberComposite>> =
        Vec::with_capacity(result.composites.len());
    for c in &result.composites {
        composite_objs.push(Py::new(
            py,
            PyMemberComposite {
                member: c.member.clone(),
                equity: c.equity.clone(),
                fills: c.fills.clone(),
                metrics: c.metrics.clone(),
            },
        )?);
    }
    let py_result = Py::new(
        py,
        PyPanelWalkForwardResult {
            is_bars: result.is_bars,
            oos_bars: result.oos_bars,
            embargo_bars: result.embargo_bars,
            prefix_skip: result.axis.prefix_skip,
            axis_len: result.axis.len(),
            members: result.axis.members.iter().map(|m| m.name.clone()).collect(),
            folds: fold_objs,
            composites: composite_objs,
            composite_members,
            columns,
            metric_columns,
            cash: result.cash,
            breadth: result
                .effective_breadth()
                .map(|b| (b.effective, b.mean_correlation, b.members, b.pairs)),
            departures: result.departures(),
            shrinkage: result.run_shrinkage,
        },
    )?;
    Ok(py_result.into_any())
}
