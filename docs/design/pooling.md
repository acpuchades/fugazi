# Pooling — from one shared parameter to partial pooling

**Status:** built, Stages 0–3. `--shrink` ships; Stage 4 (partial pooling in the
plain sweep, returning N parameter sets) remains blocked on the DSR trial count
— see *Open questions*. What the implementation changed relative to the plan is
recorded in *What shipped* at the end.
**Scope:** what pooling actually buys, the four ways it can buy nothing, and a
partial-pooling design that replaces the all-or-nothing choice between "one
parameter set for the whole panel" and "one per member".

## Why

`--pooled` already *is* an aggregated objective: `ranking_value`'s Panel arm
computes `mean_m(metric) − k·std_m(metric)` over the panel and optimizes it once
(`src/spec/optimize.rs:1293`). There is no second mechanism hiding behind the
flag, and none is proposed here.

The gap is that nothing reports whether aggregating was *valid* for the strategy
and panel in hand. Pooling's benefit is mechanical and conditional, and both
halves are currently invisible.

**The mechanism is variance reduction on the selection surface.** Optimizing
over a grid of `M` points selects the argmax of `M` noisy estimates; the winner
is inflated by roughly the max of `M` noise draws. `deflated_sharpe_from_stats`
(`src/metrics.rs:833`) corrects how that inflation is *reported* and does nothing
to reduce it — the parameter was still chosen by noise. Averaging `N` members'
estimates before selecting shrinks the estimation noise by `√N`, so the argmax
lands closer to the true one. That is the whole win.

**The condition is that the parameter means the same thing on every member.**
Not that the strategy was written for pooling — it needn't be scale-free, needn't
know it is in a panel. Only that the swept quantity has shared meaning:

| Axis kind | Shared meaning | Pooling |
|---|---|---|
| Bar counts — `SMA 20`, `RSI 14`, ATR window | **Yes, by construction.** A bar is a bar | The clean case |
| Dimensionless thresholds — `RSI > 70`, `z > 2` | Partial. `70` is a different quantile per instrument | Degrades gracefully |
| Dimensioned — a `$100` stop, `min_volume 1e6` | **No.** No shared quantity to average | Averages apples |

This is a **per-axis** property, not a per-document one, and a typical strategy
that was never written with pooling in mind has a mix — most of its grid is bar
counts, which are shared whether or not the author thought about it. So the
question is never "is this document poolable" but "which of these axes is".

**And `√N` is `√N_eff`, not `√N`.** Thirty crypto perps with `ρ̄ ≈ 0.8` give
`effective ≈ 1.2`: thirty backtests bought one backtest's worth of noise
reduction. `effective_breadth` computes exactly this (`src/spec/panel.rs:375`),
is reachable from Python (`python/src/spec.rs`), and is printed by no CLI path.

## What already works, unchanged

This is the load-bearing observation: most of the structure partial pooling needs
is already here, built for other reasons.

- **The reduction kernel is metric-agnostic.** `pool_metric` takes a
  `&[PanelMetrics]` and a path and does not care what varied
  (`src/spec/panel.rs:275`). `Pooled` already carries `defined`/`members`, so
  "a mean over 2 of 30" is already distinguishable from "a mean over 30 of 30".
- **`-k` composes for free** and means the same thing over members as over
  windows — the Panel and Windowed arms of `ranking_value` are the same shift on
  a different partition.
- **The shared clock exists.** `PanelAxis` / `MemberAxis` lay every member on the
  sorted union of bar keys and map back down per member
  (`src/spec/panel.rs:558`, `:632`). Fold `k` is one span for the whole panel.
- **The output shape partial pooling forces already exists in one path.**
  `panel_walkforward` (`src/spec/panel.rs:854`) returns one `MemberComposite` per
  member (`:709`) and deliberately refuses to net them into a single curve. N
  results rather than one is the shape that path already has.
- **The premise that a raw argmax is overfit is settled in this codebase.**
  `--smooth` (`src/spec/optimize.rs:2139`) borrows strength from *neighbouring
  parameter points* for precisely this reason. Shrinkage borrows strength from
  *other members*. They are orthogonal axes of one idea and compose.
- **Two independent routes to a residual variance.** Folds via `-w`, and the
  analytic Sharpe standard error the DSR path already computes.
- **`Metrics` already carries the activity fields** Stage 0 needs:
  `trades.total`, `trades.exposure_pct` (`src/spec/metrics.rs:907`, `:922`) and
  `run.bars`.

## The stance: measure validity, do not classify it

Pooling's validity is an empirical property of a (strategy, panel, axis) triple.
It is measurable from the row×member score matrix a pooled sweep already
produces, and it is **not** reliably inferable from the document text.

A user reaching for `--pooled` is making a judgment about their own strategy.
The tool's job is to make that judgment checkable after the fact and cheap to
get wrong — not to pre-empt it with a static analysis that would be loud on toy
documents and silent on real ones.

## Design decisions

### D1 — The failure modes are four, and each has a number

None of the four is currently reported. Each is cheap from data already in hand.

| Failure | Number that catches it | Exists? |
|---|---|---|
| Members are near-duplicates — illusion of breadth | `effective_breadth` | Computed, unwired |
| It is not the same strategy on each member | Activity dispersion (trades/1000 bars, `exposure_pct` spread) | Fields exist, nothing derives it |
| Members do not share an optimum | Interaction fraction `λ` | Needs the matrix (D3) |
| The mean rests on a fraction of the panel | `Pooled::defined / members` | **Reported already** |

### D2 — Pooling is a per-axis decision; `--pooled` makes it per-sweep

The flag declares its own panel axes and reduces over all of them; every `--grid`
axis stays ranked. Nothing measures whether that split was right. The interaction
fraction is computable per swept axis, which turns "should I pool this?" from a
judgment call into a readout — and for an axis that should *not* be pooled, the
honest answer is per-member fitting, not averaging.

### D3 — The interaction term is both the diagnostic and the estimator

Model the row×member score matrix as a two-way layout:

```text
x_rm = μ + α_r + β_m + γ_rm + ε
       ^     ^     ^     ^
       |     |     |     parameter × member — the members disagree
       |     |     member fixed effect — no ranking information
       |     shared parameter effect — what pooling is trying to estimate
       grand mean
```

`β_m` is the nuisance term that makes today's cross-member `std` conflate "this
parameter set is unstable" with "these instruments have different achievable
Sharpe". It is identical for every row of the grid, so it carries no ranking
information — yet it inflates every row's `−k·std` penalty, and inflates it
*unequally*, because rows differ in which members they are `defined` on.

`γ_rm` is the one that decides whether pooling was valid at all. Writing
`λ = τ²_γ / (τ²_γ + σ²_ε)`:

- `λ → 0` — the members share an optimum. Pool completely; the strategy gets the
  full `√N_eff`.
- `λ → 1` — the members are separate problems. The pooled winner is a compromise
  that may be worse on *every* member than that member's own optimum. That is
  not robustness, it is loss.

This is a random-effects model in the LMM sense: complete pooling is `τ²_γ → 0`,
today's `SYM=[...]` grid axis is `τ²_γ → ∞` (a fixed effect per member, each fit
on `1/N` of the evidence), and partial pooling estimates it. The satisfying part
is that **the method-of-moments estimator for `τ²_γ` is the interaction variance
component** — the same quantity that diagnoses whether to pool is the one that
supplies the weight. `λ` is not a diagnostic bolted beside the estimator; it is
the estimator's parameter.

`λ` is therefore the single number this design is organised around, and it is
reportable (Stage 1) long before anything acts on it (Stage 3).

### D4 — Shrink in score space, not parameter space

**Chosen.** Shrink `γ` by `λ`, then each member selects
`argmax_r [α_r + λ·γ_rm]`. `λ=0` gives every member the pooled winner; `λ=1`
gives every member its own; in between is partial pooling.

**Rejected: `θ_m = θ̄ + λ(θ̂_m − θ̄)`.** Easier to explain and wrong three ways.
It requires every axis to be numeric with a meaningful metric — there is no
shrinking a categorical axis toward anything. It produces off-lattice values that
must be rounded back. And shrinking each axis independently ignores that score
surfaces have ridges: `FAST`/`SLOW` are correlated, and independent per-axis
shrinkage walks off the ridge.

Score-space shrinkage handles categorical axes, stays on the grid, respects the
surface's geometry, and operates on the matrix Stage 1 already builds.

### D5 — Identifiability: folds are the replicates

With one observation per cell, `γ_rm` and `ε_rm` are confounded — the classic
unreplicated two-way layout. `λ` is precisely their ratio, so this is not a
detail; it is the thing that decides whether the estimator exists.

Two routes, both already built:

- **Folds.** `-w/--windowed` cuts each run into non-overlapping spans, giving
  `x_rmf` over row × member × fold. That is within-cell variance, and the
  decomposition becomes identifiable. **`--pooled -w` is therefore not merely a
  supported combination but the configuration in which the variance components
  are estimable at all** — which is worth saying in `docs/CLI.md` regardless of
  whether any of this ships.
- **The analytic Sharpe SE.** `deflated_sharpe_from_stats` already derives a
  standard error from skew, kurtosis and `T`. Model-based, Sharpe-only, no folds.

Build on the first (metric-agnostic); treat the second as a cross-check. If the
two disagree badly on `λ`, that disagreement is itself a finding.

### D6 — No static dimension lattice

**Considered and rejected.** The alternative to measuring validity was inferring
it: a dimension lattice (`Price`/`Qty`/`Notional`/`Dimensionless`/`Bars`)
declared per `NodeSpec` variant via `#[grammar(dim = "...")]`, propagated in
`typecheck`, flagging a comparison between a bare `!value` literal and a
dimensioned side.

It dies on three counts:

- **It is silent where it matters.** `!get` is schema-dependent, so it resolves
  to `Unknown` and must be skipped — the check goes quiet for exactly the users
  who read prices through overlay columns, and stays loud on toy documents.
- **Dimensioned comparisons are frequently correct.** `!volume > 1e6` liquidity
  filters, a hardcoded support level in a single-instrument document, `!close > 0`
  guards. The false-positive rate is unknown and plausibly high.
- **It is subsumed.** A dimensioned axis does not fail subtly under pooling: the
  members disagree about the optimum, which is what `λ` measures. The interaction
  term catches dimensioned axes *and* regime-dependent thresholds *and* genuinely
  heterogeneous members — three failure modes for one estimator, none of which
  need a judgment about what `!get` produces.

Cost avoided: a declaration on ~142 variants, a propagation lattice, an
exemption table with its own staleness tests, and a staged rollout to bound the
false-positive rate.

**What would change it:** the `λ` readout shipping and users routinely being
unable to explain a high fraction. That is evidence a *localizing* check has
value — and by then the real cases would name which node shapes cause it, so it
would be a small targeted check rather than a full lattice. That is the better
order to build it in regardless.

Replaced by documentation: `docs/CLI.md` under `--pooled` states the condition
outright (pooling is valid when the swept parameters mean the same thing on
every member), carries the axis-kind table from *Why*, and names the
consequence — pooling a non-shared axis does not error, it silently returns a
compromise.

### D7 — Demeaning is an additional column, never a replacement

Removing `β_m` (standardizing within each member's grid column, then reducing
across members as today) makes `−k·std` mean "does this parameter set rank
consistently well" instead of "are these instruments alike".

But demeaning destroys the level: the best row of a uniformly losing grid ranks
first. So `{name}_z` sits beside `{name}_mean`/`_std`/`_n` and is *selected*,
not substituted. Pre-1.0 the default could simply be switched
(`no-compat-compromises`), and it should not be — the demeaned score answers a
different question, not the same question better.

### D8 — Two classifications survive D6, because they are closed sets

D6 rejects inference over a user's expression tree. It does not reject facts
about sets the crate owns:

- **Metric scale.** ~40 fixed leaf keys in `metrics::flatten`, already enumerated
  by `tests/metrics_coverage.rs`. Whether `total_pnl` is scale-free is not a
  judgment call. Warn when a dimensioned metric is the pooled ranking metric: its
  cross-member `std` carries each member's volatility, so `-k` penalizes the
  panel's composition rather than the parameter set.
- **Unscoped absolute costs.** Cost terms scope on stream ids
  (`src/spec/costs/spec.rs:30`), so per-member fee structures are already
  expressible. A `fee_per_trade` with no scope, across a panel spanning three
  price magnitudes, is a fact about what was typed on the command line. It ruins
  the low-priced members and reads as "the parameters do not generalize".

## What breaks, and the fix for each

- **`Evaluation::Panel` is per-row** (`src/spec/optimize.rs:563`), and both
  demeaning and the decomposition are over a grid *column*. Neither can live
  inside `ranking_value(&row.eval, …)`, which sees one row. Fix: a two-pass
  ranking — evaluate all rows, build the row×member matrix per metric, then rank.
  This is the real structural cost of Stages 1–2 and the reason they are not a
  one-afternoon patch.
- **The results CSV is one row, one score.** Stage 2 adds columns and is fine.
  Stage 4 returns N parameter sets and is not; see *Open questions*.
- **`defined < members`** makes the matrix ragged. The decomposition must handle
  missing cells rather than assume a full layout, and the support counts stay
  reported for the same reason they exist today.

## Staging

### Stage 0 — reporting only, no semantics change

1. Wire `effective_breadth` into both pooled console blocks. `run --pooled` has
   every member's equity curve in hand; `optimize --pooled` carries only
   `Metrics` in `Evaluation::Panel`, so report breadth **once for the panel**
   rather than per row — it is close to a property of the panel, and per-row
   would imply a precision it does not have.
2. Activity dispersion: per member, trades per 1000 bars and `exposure_pct`;
   min/max/spread across the panel, beside breadth. Arithmetic over `Metrics`.
3. Warn on unscoped absolute cost terms under `--pooled` (D8).
4. `docs/CLI.md`: the validity condition, the axis-kind table, and that `-w`
   is what makes the panel's variance components estimable (D5).

Ships together. Nothing here changes a ranking.

### Stage 1 — the matrix and `λ`

Two-pass ranking; build the row×member(×fold) matrix; report the variance
decomposition per swept axis and the resulting `λ`. Still no behaviour change —
`λ` is printed, nothing consumes it. This is where the design gets tested against
real sweeps, and where D6's revisit trigger would fire.

### Stage 2 — demeaned score columns

`{name}_z` / `{name}_z_std` beside the existing pooled columns, selectable as the
ranking metric. Changes selection, so: its own release and its own doc note.

### Stage 3 — partial pooling, in walk-forward first

`pooled_walkforward_run` already returns per-member composites and already lays
folds on the shared clock, so it has both the output shape and the replication
structure. Per-member winners per fold, shrunk by `λ`. Prototype here, where
there is no friction, rather than in the plain sweep, where there is.

### Stage 4 — partial pooling in the plain sweep

Blocked on the output-shape question below. Not scheduled.

### Explicitly not in scope

- **Rescaling bar data** — dividing each member's prices by first close or
  rolling σ. Fills print at fictional prices so `fills.csv` stops being
  auditable; the cost pipeline's absolute terms (per-trade fee, spread, tick and
  lot rounding, funding) become nonsense; `--cash` no longer buys comparable
  exposure; and a cross-asset `!pick` gate needs the same scaling on both legs or
  the ratio between them changes silently. It converts a backtest into something
  that is not one.
- **A `!normalize` tag** — that is `!zscore` with extra steps, and belongs beside
  the primitives already surveyed and left out in TODO's *Spec documents*.
- **Auto-inserting normalization into a document** — makes every document build
  and every result unattributable.
- **The dimension lattice** — D6.

## Open questions

- **The DSR trial count under partial pooling.** Complete pooling is `N`
  hypotheses, which is the argument `src/spec/panel.rs`'s module doc rests on. No
  pooling is `N×M`. Partial pooling is between, and presumably scales with `λ` —
  but "presumably" is doing real work there and there is no defensible formula
  yet. Since making the deflation honest was pooling's original motivation, this
  is a blocker for Stage 4, not a detail.
- **The output shape of N parameter sets.** Stage 3 escapes it because
  `MemberComposite` already refuses to net. The plain sweep's contract is one row
  per parameter point; per-member winners do not fit it, and inventing a shape
  before Stage 3 has run on real data would be guessing.
- **`λ` per axis or per sweep.** Per axis is more useful and needs a defensible
  decomposition when axes interact (a `FAST`/`SLOW` ridge is not separable).
- **The interaction estimate on a ragged matrix.** `defined < members` is common;
  the estimator must degrade rather than assume balance.

## Parity and CI obligations

- Anything new on the Rust surface mirrors into `python/src/` in the same PR, per
  CLAUDE.md's *Parity discipline*. `panel::effective_breadth` is already exposed
  there and would keep its shape.
- Touching the Python surface means regenerating stubs
  (`python tools/gen_python_stubs.py`) and committing `python/fugazi/*.pyi`.
- New keys in `metrics::flatten` need a reference value or a written exemption in
  `tests/metrics_coverage.rs` — that guard never skips.
- `tests/pooled.rs` is where the panel's behaviour is pinned; each stage adds to
  it. Stage 0's readouts are console output, so they pin as CLI assertions.

## What shipped

Stages 0–3, as `fugazi optimize --shrink` (`src/spec/shrinkage.rs`, wired
through `src/spec/panel.rs` and both `optimize` paths). Four things differ from
the plan above, each because building it exposed something the design had wrong.

### D5 was factually wrong: `-w` and `--pooled` were mutually exclusive

The plan asserted that `--pooled -w` "is the configuration in which the variance
components are estimable". They were in the same clap `ArgGroup`, so the
combination was **refused** — the sweep had no replication available at all.

Fixed by declaring the group `multiple(true)`. `-w` now composes and supplies
each member's run cut into windows, carried on `PanelMetrics::windows` *beside*
the whole-run document rather than instead of it. `pool_metric` and every
`_mean`/`_std`/`_n` column still read `PanelMetrics::metrics`, so **turning
replication on changes no pooled number** — pinned by
`windowed_composes_with_pooled_and_changes_no_pooled_number`.

That exposed a latent bug in turn: `Sweep::windowed` was set from the *flag*
while `Sweep::panel` was derived from the *rows*. Once both could be true at
once, the CSV writer emitted a two-column windowed header over three-column
panel records and failed on the field-count mismatch. `windowed` is now derived
from the rows like `panel`, which is what the field's own rustdoc had argued for
all along.

### The λ = 0 contradiction, and why the reference had to move

The first implementation chose the pooled winner with `pooled_ranking_key` (each
member's *whole* in-sample window) and let members pick off the decomposition
(cell means over in-sample *sub-spans*). Two honest numbers on different scales,
which need not share an argmax — so a fold could report `λ 0.000` beside `2
member(s) chose differently`, which is impossible by construction: at `λ = 0`
every member sees the identical surface `μ + α_r`.

Under `--shrink` the reference is now `argmax_r (μ + α_r)` — complete pooling
expressed on the shrunk scale — so `λ = 0` yields no departures by construction.
Pinned by `a_zero_lambda_fold_has_no_departures`. Without `--shrink` nothing
changed.

### `-k` is refused, not composed

Making the reference share the shrunk scale left `-k` with nothing to act on,
and the honest reading is that it never belonged there: `-k` **charges** a
parameter set for the spread between members, `--shrink` **models** that spread.
Running both pays for the same disagreement twice.

Refused rather than silently ignored, following the precedent
`fix(optimize): … refuse an inert --smooth` already set.

### Two λs, not one, and they disagree on purpose

- **Per fold** (`PanelFoldRow::shrinkage`) — estimated from sub-spans of that
  fold's own in-sample window, so it is lookahead-free and is what selection
  acts on. Because a metric measured over a short sub-span is itself noisy, and
  that noise lands in `σ²_ε`, it is systematically **conservative**.
- **Per run** (`PanelWalkForward::run_shrinkage`) — folds as the replicate axis,
  accumulated during the fold loop as scalars. Free, better powered, and
  deliberately *not* used for selection: a component estimated over every fold
  and applied inside fold 1 would let fold 10's data pick fold 1's winner.

A low per-fold `λ` beside a high run-level one is therefore not a contradiction
to be tuned away — it is the fold saying it cannot yet separate disagreement
from noise on its own evidence. Both are reported.

### Measured end to end

On a two-member panel of a 6-bar cycle and a 90-bar cycle — members with
genuinely different optima — `λ ≈ 0.99` ("separate problems"), the slow member
departed in all four folds, and its out-of-sample total went from `+609%` under
complete pooling to `+647%` under partial pooling, with the agreeing member
untouched. That is the shape the design predicted: borrow strength where members
agree, let them differ where they do not.
