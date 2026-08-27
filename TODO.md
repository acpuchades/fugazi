# TODO

Deferred work, with the reasoning that deferred it. Each entry says what was
decided and what would change the decision — an item here is a *judgment made*,
not a task nobody got to.

## Python bindings

### No async surface — `fetch` and the live wallets are blocking

Every network call on the Python side is synchronous: `Binance.fetch(...)` and
friends block, and `OkxWallet` / `CoinbaseWallet` do their REST work inside
`update()` / `refresh_account()`. Internally there *is* a tokio runtime — a
process-wide one in `sources.rs`, driven with `block_on` under a `py.detach`, so
the GIL is at least free for the duration.

Deferred, not overlooked. Three reasons:

1. **The engine is a per-bar state machine, not an IO pipeline.** `update(candle)`
   is nanoseconds of arithmetic; there is nothing inside a strategy to await. An
   `async def update` would be a coroutine wrapper around synchronous work — the
   shape people mean by "async-washing" — and would buy latency nowhere.
2. **The IO is at the edges, and the edges are already the caller's.** A live loop
   is *their* `while True`. Someone on asyncio can put the blocking call in
   `asyncio.to_thread(...)` today and lose nothing: the GIL is released across the
   request, and — since the `Send` supertrait on `RunnableStrategy` — across a
   whole `run()` too. A thread parked in fugazi blocks nothing else.
3. **`async fn` in a pyclass means committing to an executor.** pyo3-async-runtimes
   binds the extension to a specific one, and getting it wrong is worse than not
   offering it — an `OkxWallet` that only works under `asyncio` and not `trio`, or
   that deadlocks against the caller's own runtime, is a support burden the
   blocking version does not have.

What would change it: a caller running enough concurrent venue connections that
one thread per stream is the actual bottleneck. The honest first step then is an
async **`SeriesSource`** — the fetch path, which is genuinely IO-bound and has no
per-bar state — and leaving the wallets blocking behind `to_thread`. Binding the
whole surface async is not the increment.

### Monte Carlo is not interruptible

Every other long call on the Python surface now takes Ctrl-C: `run` polls through
`interruptible`, and the grid sweep and walk-forward run under `run_watched` — the
work on a scoped thread, the main thread as watchdog, because CPython runs signal
handlers on the main thread only and `rayon::ThreadPool::install` blocks the
caller rather than letting it steal.

`run_montecarlo` is the one that stays uninterruptible. It releases the GIL, so it
blocks no other thread; it just cannot be cancelled, and `permutations=1000`
re-drives the whole strategy a thousand times.

Not done because the seam is not there and the cheap way to add one is bad. It
takes no closure, and its `indices.par_iter().map(...)` produces the rows that
become the p-values — so cancellation means either a breaking signature change or
an additive `run_montecarlo_polled` *plus* restructuring that `par_iter` to
short-circuit through `Result`. That is statistically load-bearing code with
committed fixtures behind it, and the payoff is convenience on a path that
already releases the GIL. Wrong trade today.

What would change it: someone actually waiting on a sweep they cannot cancel, or
that `par_iter` needing to grow a `Result` for its own reasons — at which point
the hook is nearly free and should go in with it.

### A `panel=` member key is typed; there is no second pooling mechanism

`panel_axis=` substitutes a member's mapping key into the named `!param`, and
the key carries its own JSON type (`{5: snaps}` → the number `5`). It used to
substitute the name as a string unconditionally, which is right for a ticker and
made every typed slot unreachable — reported from downstream, and a real
divergence: the CLI's `--pooled AXIS` reads its members from a `--params` /
`--grid` entry as typed `serde_json::Value`s, so pooling over a numeric
parameter was expressible there and nowhere in Python.

Two alternatives were weighed. **Parsing the label** as a JSON scalar is one
line and silently ambiguous for a member genuinely named `"5"`; the key's Python
type answers the same question with nothing to guess. **A Python `pooled="AXIS"`
that carves a grid axis**, mirroring the CLI exactly, was the other — rejected
because the Python surface has one flat `snapshots=` list and no frame to slice
per root, so a symbol axis would have to read out of a merged stream, which is
precisely what `panel=` being data-keyed exists to avoid. It would be a second
pooling mechanism in one surface, coherent for only one of the two axis kinds.

The cost kept: a panel member always carries its own stream, so pooling over a
parameter hands the same series over once per member and copies it that many
times. A shared-stream spelling (`panel=[5, 10, 15]` alongside `snapshots=`)
would fix that and is a second shape for one parameter — not worth it until
someone pools a parameter over a stream large enough to notice.

### `Wallet.take_rejections`

Still unbound, with the reason recorded in `python/tests/test_parity.py`: it
needs a bar-less rejection type, and `RunReport.rejections` already exposes the
same entries for the run path. Only worth revisiting for a caller driving the
wallet loop by hand who needs rejections *during* the loop rather than after.

### Position-anchored protective stops, and the Rust recipe catalogue

Noted in the Python README's `Strategy` section as not bound. Drop to the wallet
loop for those.

## Wallet and execution

### Cash contention between two buys breaks by submission order, not pro-rata

`PaperWallet::advance` settles a bar in phases so that cash-crediting fills land
before cash-consuming ones — a rotation is funded by its own sale regardless of
the order the snapshot's rows happen to be in. That fixes the case with a
*funding-derived* answer. It leaves one that has none: two buys that both want
cash, neither of which funds the other, against a balance too small for both.

The tie breaks on `OrderId` — submission order. Two alternatives were weighed:

- **Pro-rata**, scaling every contending buy by the same factor. Arguably the
  better semantic for a basket: asked for three legs of ⅓ and can afford 90%,
  you want all three at 0.9, not two whole ones and a starved third. Rejected
  for now because `shrink_buy_to_fit` is a fixed-point iteration against an
  opaque cost pipeline, so a pro-rata split means giving each buy a *budget*
  rather than the live balance, and the budgets have to be re-solved as each
  fill lands. Real work, for a case that only arises when a spec asks for more
  than 100% of equity.
- **Symbol order.** Deterministic and needs no plumbing, but it is an arbitrary
  preference dressed up as a rule — "AAVE before ZEC" is not a market
  microstructure.

Submission order is what the strategy actually expressed, and it is what a venue
would honour first-come-first-served, so it is the honest default. **What would
change this:** a shape where contention is routine rather than a
mis-specification — leveraged baskets, or a `weights:` policy that deliberately
oversubscribes — at which point pro-rata stops being a nicety.

Note the residual: a strategy that iterates a `HashMap` to submit (basket, multi)
picks its own submission order from hash order, so under contention its result is
stable for a given build but not meaningful. Sorting *there* was considered and
rejected — it would make an arbitrary answer deterministic rather than making the
answer not matter, which is what the phase split does for every non-contended
case.

### A fitted fill is scaled down without saying so — *settled*

**Settled in 0.75.0, in the shape this entry called for.** `requested_units` is a
field on `Order`, beside `units`; `fill_ratio()` is `units / requested_units`;
`fills.csv` carries the column on every row and the CLI warns past
`MATERIALLY_FITTED` (1%). The reasoning that picked that shape is kept below,
because it is what rules out the alternatives if this is ever revisited.

*Any* reduction means the fill was bound, so a naive "was it shrunk" counter
fires on every all-in trade under any positive cost model and reads as noise — an
all-in `value_frac(1.0)` **must** shed a sliver to make room for commission, or it
would fail the affordability check and drop the fill entirely. Distinguishing
"shed room for costs" from "starved" needs either a materiality threshold (a magic
number) or the requested magnitude carried alongside the filled one so the
consumer can judge. Carrying the magnitude costs one field, needs no new drain or
retention policy, rides the blotter that already reaches `fills.csv` and the
Python report, and leaves the threshold where it belongs — with whoever is
reading.

**Amended in 0.84.0: the threshold moved into the library.** "The CLI picks 1%
and says so; nothing else has to" was half right. Carrying the magnitude *is* the
right primitive and it stays. But leaving the threshold to each consumer meant
the CLI owned the only implementation of the question, so a library or Python
caller had no way to ask what the banner was answering, and would have had to
pick a second number that could disagree with it. The threshold is now
`wallet::MATERIALLY_FITTED` with `Order::is_materially_fitted` over it and
`RunReport::materially_fitted` reducing a run to `(count, worst)`; the CLI banner
reads that rather than its own copy. The magic number is still a magic number —
it is just a magic number stated once, and it governs *reporting* only: nothing
on the fill path reads it, so moving it changes no backtest.

What made it urgent was the second half of the same report: the bound itself was
asymmetric. A buy was limited by the cash it spent and a sale credited cash, so
`sizing: 3.0` executed at 1x long and 3x short under one spec value, and the
silence covered *that* too. Both are now bounded by gross notional
(`PaperWallet::with_max_gross`, `1.0` by default — for a long-only book the same
inequality as `funds >= 0`, so an unlevered long backtest is unchanged), and a
request above the cap is fitted and recorded rather than reinterpreted.

### Cost of carry — *settled*

**Settled in 0.76.0: a fourth cost leg.** `CarryModel` is charged once per bar by
`advance`, on the position carried *into* the bar and marked at its `open`, with
three models — `!funding` (per-bar rate read from an overlay column, signed both
ways), `!annual` (a constant annualized rate per side), `!both`. Cash interest on
a negative balance is `PaperWallet::with_margin_rate` / `--margin-rate`, on the
account rather than in the per-symbol bundle, because that is what the balance
is.

The entry that stood here argued a fourth leg was too large because funding needs
"a rate *schedule*, not a constant". That turned out to be the answer rather than
the obstacle: the rate is **data**, so it arrives on the atom for the bar it is
charged for, through `CarryModel::column` and `Wallet::observe`. No new
configuration surface, no accrual cadence to invent —
`binance-vision-futures` already publishes `funding_rate` *summed per bar* for
exactly this reading, so a `1d` bar carries all three of its 8-hourly settlements
and the model needs no notion of settlement timing at all.

Both silent-failure modes are checked rather than left to be noticed: a
`!funding` whose column is absent, and an `!annual` with no resolvable cadence,
each charge nothing on every bar and leave a curve indistinguishable from "carry
was free". `run` warns before the sweep (`CostConfig::carry_requirements`), and
`PaperWallet::carry_coverage()` reports `(wanted, got)` after it.

### Liquidation is modelled; a full margin model still is not

`PaperWallet::with_maintenance_margin(ratio)` force-closes the book when equity
falls below `ratio × gross`, as `OrderKind::Liquidation`. **Opt-in**, and the
default stays off deliberately: the ratio is a venue assumption that varies by
exchange, instrument and tier, so stating it is the caller's job. This narrows —
but does not close — what `ARCHITECTURE.md` says about a margin model.

What is here: a threshold test at the end of each bar, triggered on the bar's
**adverse extreme** (a long at the `low`, a short at the `high`), because a wick
is what liquidates a levered account.

What is **not** here, and what a real venue does:

- **A fill price.** The forced legs book at the bar's `close`. The price at which
  equity actually crossed the threshold is a point on a surface once more than
  one symbol is involved, and is not recoverable from one bar. For a
  single-symbol book it *is* computable in closed form — that is the obvious
  refinement, and the reason this is written down.
- **Partial liquidation.** Real venues close enough to restore the ratio, tier by
  tier; this closes everything.
- **Tiered ratios.** One number for the whole book, where a venue scales the
  requirement with position size.
- **Continuous marking.** The check runs once per bar, so a breach that opens and
  closes inside one bar is caught only if the bar's extreme reaches it.

**What would change this:** a user reconciling a live liquidation against a
backtest and finding the *size* of the loss wrong rather than its existence.
Getting the event right is most of the value; getting the fill exact needs the
path, which a bar does not carry.

### Sizing stays denominated in equity, not in buying power — *settled*

**Rejected in 0.84.0, by measurement.** The proposal: re-base
`Size::ValueFraction` so `value_frac(f)` targets `f × max_gross × equity` rather
than `f × equity`. Then `sizing` would span flat → fully deployed, `max_gross`
would define what "fully deployed" means, and a document would become
leverage-agnostic. It is inert at `max_gross = 1` (verified byte-identical on
fills, returns, trades and metrics), 1 is the default, and the window closes the
first time anyone runs a genuinely levered book — so it looked both free and
urgent.

It is neither. Three measurements, all on a 1,200-bar three-regime fixture:

1. **The premise is false.** "Only documents written incorrectly exceed
   `sizing: 1.0`" assumes `sizing` is a fraction. It is an arbitrary
   real-valued expression, and every recipe in `indicators::sizing` is unbounded
   above: `!vol_target` at a 20% target exceeded `1.0` on **54%** of bars
   (median 1.07, max 3.81); `!atr_risk` at 2%/2×ATR on **33%** (max 1.96).
   Correctly-written documents are already `max_gross`-sensitive — at the
   default cap that run had 38 of 139 fills fitted, the worst to 33.5%.
2. **It breaks the thing it would apply to.** A vol target is denominated in
   equity by convention: "a 20% vol target" is 20% of equity's vol, not of
   buying power's. Re-based on a 3x wallet the same document realized **35.5%**
   vol against its 20% target (from 15.8%), max drawdown **55.0%** (from 25.0%),
   Sharpe 0.38 (from 0.74). Multiplying a risk target by leverage does not scale
   it, it removes it.
3. **It does not achieve its own goal.** The point was for `sizing ∈ [0,1]` to
   never be clipped. Re-based at 3x, 38 of 139 fills were *still* fitted —
   because `3.81 × 3` overshoots a 3x cap. The current base at 3x fits **zero**.
   The re-base makes a levered account strictly worse at honouring the document.

**What the rejection did leave open, and what closed it.** A ceiling can only
ever *stop* a request, so `max_gross` alone left the commonest document shape — a
constant `sizing:` at or below 1 — completely insensitive to leverage, which is
the opposite of what a leverage knob is for. The gap was that one number was
being asked to do two jobs: **bound** the book and **drive** deployment. Splitting
them is what the re-base was groping at without naming.

`PaperWallet::with_leverage` / `--leverage` (0.84.0) is the driver: it multiplies
what a *fractional* `Size` resolves to, and defaults `max_gross` to itself so the
raised request is not fitted straight back down. `value_frac` stays denominated
in equity — the resolution is unchanged and only the account's own number moves,
which is why `resolve_at_leverage` multiplies *outside* the `ValueFraction` arm
rather than inside it. `Size::Units` and `PositionFraction` are never scaled: a
named unit count is a specific intent, the same reason it is never fitted.

The vol-target interaction does not disappear, it becomes *chosen*: at
`--leverage 3` a 20%-target document holds 35.5% realized vol. That is the
honest reading of asking for 3x leverage, and it is a different act from the
re-base, which would have done the same thing to a document whose author only
raised a ceiling. Measured on the same fixture — `--max-gross 3` alone: vol
15.8%, Sharpe 0.74. `--leverage 3 --max-gross 10`: vol 44.3%, Sharpe **0.74**.
Identical Sharpe at three times the vol is the signature of a correct leverage
multiplier, and it is the check to re-run if this is ever touched.

So the split stands, and it is the split the proposal itself argued for: the
document states the rule in equity, the account states what it will carry, and
the rule stays portable — exactly `TradingCostsConfig`'s standing. The docstring
was the thing that was wrong ("`value_frac(1.0)` is all-in" stopped being true in
0.75), and it is now corrected everywhere rather than the semantics being bent to
fit it. The additive `gross_frac` sibling is rejected for the same reason: it
would be a second spelling of `value_frac(f × max_gross)` that re-introduces
exactly the ambiguity above for whoever reaches for it.

`tests/leverage.rs::a_sizing_that_fits_is_identical_at_every_ceiling` pins the
invariant by **exact equality** across five ceilings, and
`a_vol_target_document_is_bounded_by_the_ceiling_not_rescaled_by_it` pins the
counter-case. Both fail against a re-based build, as do three tests that predate
this entry.

**What would change this:** a sizing vocabulary in which every expression is
provably a fraction of buying power — which would mean `vol_target`,
`atr_risk`, `equity_vol_target` and `fractional_kelly` all dividing by
`max_gross` to stay equity-denominated, i.e. four special cases to preserve one
general rule. That trade is worse than the one it replaces.

### A portfolio child's cash rule refuses where every other shape fits

`PortfolioInner::record_intent` bounds a child's buy by its **own ledger's
cash**, and refuses outright when it does not fit. Every other shape *fits*: a
single-asset `sizing: 3.0` on an unlevered account fills at 1x, records the ask
in `requested_units`, and warns. The portfolio child books nothing — and because
a child hard-cap refusal is drained to the child rather than to the run report,
nothing lands in `rejections` either. The run reports zero fills, zero
rejections and a flat curve, indistinguishable from a strategy that never fired.

The cash rule itself is right and load-bearing: it is what stops child A
spending child B's money, which is the whole of notional attribution. What is
wrong is the *refuse* rather than *fit*.

**Fixed in 0.84.0 for the levered case only**, because that case was blocking
`--leverage` outright: above 1x a ledger's cash is *meant* to be negative, so
the bound there is now gross against `account_cap × ledger_equity` — the
ledger-scale twin of the account's own `max_gross` rule, preserving sibling
isolation at every leverage. The unlevered path is untouched and still refuses;
`tests/portfolio.rs::a_levered_portfolio_child_is_not_refused_by_its_own_ledger_cash`
pins that divergence deliberately, so it reads as a recorded decision rather
than a surprise.

**What would change this:** making the unlevered path fit rather than refuse.
It is the right shape and it removes a real silent drop, but it changes existing
unlevered portfolio results, and a ledger-level clamp is invisible in a way the
account-level one is not — `requested_units` is stamped by the account, which
would only ever see the already-clamped number. Doing it properly means the
ledger recording its own ask, which is a wider change than the leverage work it
was found under.

### `max_gross` limits what is asked for, not what is held

`max_gross` is checked when a fill books and never re-checked afterwards, so a
book at exactly the cap drifts over it the moment marks move against it. It
bounds what a strategy may *ask for*; what happens to a book that has already
drifted is `with_maintenance_margin`'s job, and the two are deliberately separate
numbers — a venue's position limit and its liquidation threshold are not one
setting either.

Exits stay exempt from the cap precisely so a drifted account can trade its way
back under without needing the margin call to do it.

### Setting a venue's leverage is not exposed

`Wallet::leverage` reads; nothing writes. OKX has
`POST /api/v5/account/set-leverage` and it would be a small hook, so the omission
is a choice rather than an oversight: a write here changes a real account's
configuration out from under whatever else is trading it, and the read half is
what the honesty problem actually needed — a deployment can now record what its
fills executed at and reconcile against the `max_gross` its backtest ran under.

**What would change this:** a caller who needs to *set* leverage from fugazi
rather than in the venue's UI, and can say why the reconcile-and-refuse path
(read it, compare it to the document's assumption, refuse to start) is not
enough. Note that the paper counterpart already exists, which was the harder
half — a control whose backtest silently disagreed would be worse than no
control.

## Metrics

### A flow-neutralizing curve helper

0.52.0 documents that every `metrics` function assumes a **closed system** and
gives the chain-linked correction a caller must apply
(`r_i = (E_i - F_i) / E_{i-1} - 1`), across the Rust module header,
`Wallet::adjust_funds`, `docs/METRICS.md`, `fugazi.metrics.__doc__` and the
Python README. Tracking flows stays out of scope — it is portfolio accounting,
and the flow series would have to thread through `per_bar_returns`, the one
intermediate every other metric consumes.

What was *not* settled is whether a **pure** curve transform belongs — something
like `metrics::flow_adjusted_curve(curve, flows, initial) -> Vec<Real>`. It is
the same family as `per_bar_returns` (no state, no accounting, curve in / curve
out) and would serve the ordinary notebook case of a DCA backtest with monthly
contributions. It was left out because shipping it blesses one attribution
convention (flow-at-period-end vs at-period-start) in code rather than leaving
the caller to choose; the docs currently give both formulas and say to pick one.

Decide this on evidence: if more than one caller writes the same correction by
hand, ship the helper with the end-of-period convention and document the other.

## Optimize

### Ruin is excluded from *ranking*, not from the metrics

A wiped-out grid cell keeps every number it had — `sharpe`, `mean_bar`,
`win_rate_pct`, all of them — and loses only its candidacy: `ranking_lookup`
returns `None` for a ruined row, so `--best-by` can never return one, `--smooth`
gives it no weight, and a walk-forward fold will not select a cell that died
inside its own in-sample slice. Two alternatives were considered when the
bar-return blind spot was found (0.65.0) and rejected.

**`None` from the metric itself** — "a run that ceased to exist has no Sharpe" —
loses because the rule it needs does not exist. The set of statistics a wipeout
invalidates is not "the ratios": an adversarial pair of curves beats a solvent
profitable run on 17 of the 39 rankable paths outright, and of the remaining 22
only nine are safe by *construction* (the terminal-wealth ones, `drawdown.max*`
and `worst_bar`). `best_bar`, `largest_win` and `payoff_ratio` are one lucky
pre-ruin trade away; `stddev_bar` and `ulcer_index` are bounded only by
`1/sqrt(bars)`. Satisfying the invariant that way means nulling ~30 of 39,
including ten fields that are non-`Option` today — a schema change to
`metrics.yml` that leaves a ruined run's document nearly empty, throws away the
only evidence of what the parameter set was doing before it died, *and* still
covers only the metrics someone remembered to list. One predicate at the
ranking boundary covers all 39 and every metric added later.

**A parallel `pre_ruin.` namespace**, with the plain name `None`, loses for the
same reason plus a second catalogue to keep in step — `direction_for` entries,
`metrics::flatten`, CSV columns, Python — to say what `run.ruin_bar` beside the
plain value already says.

What would change it: a metric whose *pre-ruin* value is not merely unrankable
but actively misleading as a description — one where reading it at all invites
the wrong conclusion regardless of the ruin flag next to it. None in the current
catalogue is; `tests/ruin.rs` pins the choice.

### …and it is not a *trial* either — the DSR population

`ranking_lookup` covers every place a `Metrics` becomes a per-row key. It left
one number behind, because that one is derived grid-wide and so had nothing to
inherit the rule from: `compute_dsr_context`'s `(N, Var[SR])` counted a ruined
row as a trial and put its pre-ruin Sharpe into the variance, while the same row
was barred from ever being returned. `trial_sharpe` (0.65.0) carries the rule
across — the trial population is the candidates.

The argument is not "a dead account deserves no say". It is that DSR corrects
for a maximum having been taken over a set, so the set has to be the one the
maximum could have come from; after `ranking_lookup` that set excludes the
ruined rows by construction. Two things follow, and both were checked rather
than assumed:

- **Counting them is not conservative.** The intuition says a bigger `N` means a
  bigger correction. But a ruined cell whose pre-ruin Sharpe sits near the
  grid's mean *shrinks* `Var[SR]`, and `E[max SR₀] = √V·[…]` shrinks with it, so
  the whole grid's DSR goes **up**. `a_ruined_row_is_not_a_dsr_trial` pins the
  harmful direction with a fixture that reproduces it.
- **It is not even the same estimator.** `run` pins the curve at zero from the
  ruin bar on, so a ruined cell's Sharpe is a statistic over a truncated sample
  of a different effective length. `Var[SR]` across a mix of those and
  full-length ones is not the dispersion of one estimator across trials, which
  is the quantity the closed form asks for.

The ruined row keeps its own `selection.deflated_sharpe` cell, against the
candidates' null — the same call the decision above makes for `sharpe`, and for
the same reason. Nulling it would have made DSR the single metric that blanks on
ruin, which is alternative (1) reintroduced for one column.

Visible consequence, accepted: a grid in which fewer than two cells survived now
emits **no DSR column at all** rather than a column built from dead accounts.
That reads correctly — there was no selection to correct for. `optimize`'s ruin
banner already says every cell died.

What would change it: a use of DSR where the question really is "how many
configurations did I try", independent of which could win — a search-effort
audit rather than post-selection inference. That is a different statistic and
should be a different column, not this one re-pointed.

`run -w`'s `windows_dsr_context` is deliberately untouched: its population is
the windows of one run, no ranking predicate acts on them, and the post-ruin
windows are flat zeros whose Sharpe is already `None`.

### The rest of the plateau summary: "fraction of the grid above baseline"

`--smooth`'s console block reports the largest connected plateau within 5% of
the best smoothed value. The other half of that readout — what fraction of the
grid clears a *do-nothing baseline* — is deliberately not shipped.

Deferred because "do-nothing" has no metric-independent definition. For
`cagr_pct` it is plausibly buy-and-hold, or zero; for `drawdown.max_pct` a
do-nothing run has a drawdown of zero, which every real strategy loses to, so
the fraction would read 0% and mean nothing; for `trades.win_rate_pct` there is
no baseline at all. Picking one per metric would mean a second direction-table-
shaped thing to maintain, and picking one globally would be quietly wrong for
most of the catalogue. Worth doing if a concrete baseline lands — the honest
shape is probably an explicit `--baseline <STRATEGY>` evaluated through the same
`EvalContext`, compared on the same `--best-by` path, rather than a synthesized
null.

The plateau *tolerance* is likewise fixed at 5% rather than exposed as a flag:
it is a readout, not a knob, and one more `--smooth-*` spelling buys nothing
that reading the `_smoothed` column doesn't.

### Smoothing reads lattices, not point clouds

`--smooth` / `smooth=` refuses a grid where no subgrid sweeps a numeric axis of
two or more values, rather than smoothing an arbitrary set of evaluated points.
Reported from downstream (fugazi-web) as `smooth=` silently no-opping on a
concrete-point grid; the no-op was real, the silence was the defect, and the
refusal is what shipped.

Smoothing a **point cloud** — nearest-neighbour weights over whatever points the
caller happened to evaluate, no lattice required — was considered and not done.
It sounds like a generalization and is a different estimator: without a lattice
there is no "one typical step" per axis to normalize distance by, so the kernel
radius stops meaning anything a user can reason about, and `support`'s
denominator (the weight a fully interior point of a *regular* axis would find)
has no referent at all. The honest version needs a bandwidth per axis in
parameter units and a density-aware support, which is the design
`smooth_keys`' doc already weighs and rejects for the lattice case. A filtered
product (`FAST < SLOW`) is the motivating shape and is better served by passing
the block and letting the illegal corner score `None` — a `None` neighbour
already contributes no weight and lowers support, which is the right answer.

Consequence kept: `SmoothedKey::support` is `Option<Real>`, `None` for a point
whose subgrid has no smoothed axis. That case survives the refusal in a *mixed*
grid (a pinned point stacked beside a swept block), where reporting the empty
product's `1.0` would read exactly like a fully interior point. A
`min_support > 0` floor drops it.

## Run resuming

### No migration path between `RunState` format versions

`RUN_STATE_FORMAT_VERSION` went 1 → 2 when the basket / multi / portfolio blobs
gained required keys (the rebalance gate everywhere; children, `bars_seen` and
weight-share chains on a portfolio; in-flight netting state under `inner`). A v1
file is refused with a clear message.

No migration was written, and none should be: a v1 portfolio blob does not
*contain* its children's state — that was the bug — so a migration could only
fabricate it, which is exactly the silently-wrong outcome the version field
exists to prevent. The remedy is to re-run the history (resuming optimizes a
re-run; it does not replace one) or to finish on the build that wrote the state.

This changes if state files ever become long-lived artefacts rather than
process-to-process handoffs — a deployment that cannot afford to replay months of
bars would justify a `v1 → v2` shim for the three shapes whose v1 blob *is*
complete (single, pairs, and any portfolio that never traded).

### The resume file drops the blotter and the rejection log

A `RunState` carries state, not history. `PaperWallet`'s fill blotter and
rejection log used to ride along in it and **dominated** the file — on a
1500-bar 8-symbol basket, 253 KB of 258 KB (98%), growing linearly in bars while
everything else stays bounded by the universe and the indicators' periods.
Dropping them took that file to 10.6 KB and made it flat in run length.

They were removed rather than trimmed because nothing reads them across the seam:
no fill, pricing or restore path consults the blotter at all, `RunReport::fills`
comes from `Wallet::update`'s return value, and `take_rejections` needs only
"everything so far has been drained", which an empty log with a zero cursor
states exactly. A resume file's job is resumption; a blotter in it does the
caller's job badly, and anyone needing full history across restarts needs their
own durable store. The visible consequence is that a resumed wallet's `orders()`
covers the resumed chunk — already true of the per-chunk `RunReport`.

No version bump: the keys were dropped from `WalletSnapshot` and serde ignores
unknown fields, so a state written by an older build still resumes identically
(pinned by `a_state_carrying_legacy_history_keys_still_resumes`).

The same reasoning bounds both logs *in memory* at `wallet::DEFAULT_RETENTION`
(10k entries), since a strategy driven live for years would otherwise never free
a fill; `PaperWallet::with_retention(None)` is the named opt-out.

What would change this: a caller with a real need for `orders()` to span a
restart. The answer then is still their own store, not the resume file — unless
the retention bound itself proves wrong for a legitimate workload, in which case
the default is one constant.

### A live wallet's `RunState.wallet` is `Null`, not a snapshot

`Wallet::snapshot_state` defaults to `Null` and `OkxWallet` / `CoinbaseWallet`
take that default, so a live resume restores the strategy's indicator state and
re-reads positions and cash from the venue.

The alternative — serialize whatever the live wallet can report and replay it —
was rejected because a stale local view would silently overwrite the broker's
truth, and the window in which it goes stale is exactly the window a resume
exists to cover. What would change it: a venue whose account state is *not*
cheaply readable at resume time, where a local snapshot is the only source. Then
the honest shape is an explicit opt-in on the wallet, not a changed default.

### `warm_up` returns state, not a report

`StrategySpec.warm_up` hands back the `RunState` alone. Returning a `RunReport`
was considered and dropped: every field of one describes trades that deliberately
did not happen, so a report would be a page of zeros inviting the reader to
reduce it to metrics. The state is the only output that composes — prime from
history, hand it to `run_resumable`, go live.

## Spec documents

### The math primitives that were surveyed and left out

0.69 added eighteen tags to close the gaps where a document either could not
express something at all or had to pay for it several times over: `!abs`,
`!sign`, `!sqrt`, `!tanh`, `!sigmoid`, `!pow`, `!min`, `!max`, `!clamp`,
`!cum_sum`, `!cum_max`, `!cum_min`, `!covariance`, `!beta` and the four
`!linreg_*` readings.

The cost argument is the one worth recording, because it is not obvious from the
YAML: `IfElse::update` advances **all three** branches unconditionally and
`Combine` holds two independent source instances. So `|x|` spelled as
`!if_else { cond: !gt {x, 0}, then: x, else: !mul {x, -1} }` builds and ticks
*three* copies of `x` every bar, and a three-way `!sign` builds five. That is
what made these primitives rather than recipes — not the verbosity.

The same survey named four groups that were **rejected**, and each would need a
new argument to reopen:

- **`!floor` / `!ceil` / `!round` / `!mod`.** Quantisation is a venue property —
  a lot size, a tick size — and belongs at the wallet layer where the venue's
  own increments are known, not in an expression that has no idea what it is
  being rounded *for*. `!mod` buys only calendar cycles, which the calendar
  leaves already cover. Reopen if a strategy-layer need appears that is not
  execution quantisation.
- **Trigonometry.** The one real use is cyclic seasonality encoding
  (`sin(2π·doy/365)` as a feature). Worth adding the day the crate grows a
  seasonality story; until then it is six tags nothing in the repo would call.
- **Cross-sectional `!rank` / `!softmax`.** These are a different layer:
  ranking *across* a universe on one bar is `strategies::basket::Selection`,
  which already composes (`TopBottom`/`Threshold`/`Quantile`). A per-expression
  rank tag would be a second, weaker spelling of it.
- **`!coalesce` / `!nz` — "read 0 when the source is `None`".** Refused on
  principle, not on cost. `None` is how this crate says *not settled yet*, and
  the readiness gate is built on it; a tag that turns an unsettled bar into a
  tradeable zero contradicts *Safe defaults, opt-in overrides* directly.
  `!unstable` is the named opt-out and it is the honest one — it says "trade
  through the settling tail", not "pretend the value is zero".

Two more were considered and folded in rather than added: `!rolling_sum` is
`!sma` times its period, one extra node, and `!linreg_angle` (TA-Lib has it) is
`atan` of a slope whose units are arbitrary — the slope itself is the number a
strategy can reason about.

### `meta:` is a named subtree, not a relaxed `deny_unknown_fields`

0.60.0 gave every document (the five strategy shapes, presets, portfolio
children, costs files, dataset files) a free-form `meta:` key so an external
service can keep its own record next to a strategy it generated. The obvious
cheaper alternative — drop `deny_unknown_fields` at the document root and let
any extra key through — was considered and rejected twice over:

- **It trades the typo guard for the feature.** `symbl: BTC` or
  `rebalance_of: !every 5` would parse and silently do nothing, and a strategy
  that quietly ignores a field you wrote is a much worse failure than one that
  refuses to load. That guard is why the attribute is there.
- **It has no collision story.** The day fugazi adds a real `tags:` field, every
  service already storing its own `tags:` at the root breaks. A namespaced
  subtree makes that impossible by construction, in both directions: fugazi
  never reads under `meta:`, and never adds a field inside it.

What would change it: nothing about typo detection — but if a *second*
uninterpreted namespace is ever wanted (say a separate one owned by the CLI),
the answer is another named key, not opening the root.

**Overlay column files are deliberately excluded.** They are the one document
with no envelope — every key *is* a column name — so a `meta:` field could only
be carved out of the column namespace, silently turning an existing column into
metadata. Widening what parses is cheap and backwards-compatible; narrowing it
is neither, and metadata about a set of columns has somewhere better to live
(the dataset file that declares them). `tests/spec_meta.rs` pins `meta` as an
ordinary column name there, so "completing" the feature can't happen by
accident.

**`meta:` is substituted like any other subtree**, so `!import` / `!param`
resolve inside it — deliberate (`meta: !import shared-meta.yml` is useful), at
the price of a literal `{param: …}` map inside `meta:` being read as a
placeholder. Excluding `meta` would mean teaching the untyped tree-walkers
(`params::substitute`, `imports::resolve`) about document structure, which they
are specifically built not to know.

### A grammar field's `default` is tagged, and stops at the root floor

`GrammarField::default` is `{"literal": 12} | {"expr": "!close"} | null`, not a
bare JSON value. Two terser shapes were considered and rejected:

- **Put `"!close"` in `default` bare.** Indistinguishable from a field whose
  default really is the *string* `"close"`. No field carries a string default
  today, but the vocabulary has `str` and `str_operand` field types, so that is
  luck rather than a guarantee — and a consumer would be left inferring which it
  had from the field's `type`, exactly the guessing v7 removes.
- **Tag only the expression: a bare value means a literal.** Terser, and it
  would have kept the 34 literal readers working. But `JsonValue | {expr:
  string}` is not a discriminable union — `JsonValue` subsumes objects, so no
  type system can check the branch and every consumer falls back to a runtime
  `"expr" in d` probe. That probe is sound only while no field defaults to an
  object, which is true today and guaranteed by nothing. It is the same
  can't-tell-the-two-apart bug this entry's field exists to fix, one size down.

What would change it: nothing foreseen. Ten characters a record buys a union
that is checkable in Rust, TypeScript, and Python alike.

**The fragment stops at a root floor, deliberately.** Every `expr` default is a
bare leaf — `!close`, never `!close { source: … }` — because a bare leaf already
reads the series its document blesses. The 33 slots left reporting `null` are
the leaves' *own* `source:` keys, where omission means "the strategy's own
series": there is no tag for that, and reporting an `expr` there would mean
inventing one. `null` is the honest answer, and the floor is what
makes it a cheap one — the unspellable thing sits one rung below every fragment
we actually emit, so nothing a consumer needs is lost. What would change it: a
tag that names the blessed series (`!self`, say), which nothing has needed.

### The default rebalance cadences are per-shape, and each was checked once

Reviewed across all five shapes. The verdict is *keep every default* — recorded
here so it isn't re-derived from "basket fires and the others don't looks
inconsistent". It isn't: the gate denotes a different act per shape.

- **`single:` / `multi:`** — `!never`. The gate reaches only the resize branch of
  `strategies::trade_leg`; entries, exits, reversals and protective levels fire
  regardless. Firing by default would make every strategy a constant-fraction
  rebalancer, paying turnover against a live cost model nobody wrote down. The
  two must agree with each other besides — a `multi:` is N `single:`s sharing a
  book, so a differing default would change behavior on conversion for no reason.
- **`pairs:`** — `!never`, the closest call. The gate re-hedges both legs to equal
  notional, and there is a real argument that a spread trade should maintain its
  hedge. Two things kill it as a *default*: as the spread widens, re-hedging adds
  to the losing leg (a martingale — an opinion a library shouldn't hold
  uninvited), and equal notional isn't the correct hedge ratio anyway (beta /
  cointegration weights are). Continuously maintaining the wrong ratio is worse
  than drift, which is at least visible.
- **`basket:`** — `!every 1`, and effectively forced. The gate wraps *selection*,
  not just resize, so `!never` is a basket that never trades (pinned by
  `rebalance_on_never_freezes_the_basket`). Every periodic alternative is
  arbitrary: a bar count means a different horizon per cadence, which isn't known
  at build. "Rank and hold the top N" with no schedule stated means every bar.
- **`portfolio:`** — `!never`. Drift-with-P&L is the right default; a rebalance is
  a real trading decision, and flipping it would silently convert every existing
  portfolio into a daily rebalancer.

What the review *did* find was not a wrong default but a combination the default
voided: **a non-constant `weights:` with no `rebalance_on:`**. Weight shares are
read only inside `Portfolio::rebalance_now`, so an ungated dynamic expression was
built, updated every bar, and consulted on none — the portfolio ran its
equal-split seed and reported a plausible backtest whose weighting rule had never
executed. Constants were never affected (`!value <list>` / `!value <scalar>` are
pre-resolved into the build-time seed), so this bit exactly the adaptive case,
the one where the user most clearly wrote an instruction.

Fixed as a **build error**, not a changed default — the same reasoning that keeps
`deny_unknown_fields` on every document: a strategy that quietly ignores a field
you wrote is a worse failure than one that refuses to load. `rebalance_on: !never`
is the named opt-out, so the escape hatch is a statement of intent rather than
deleting the weights.

What would change any of this: a shape whose gate semantics change. If `pairs:`
ever grows a real hedge-ratio slot (beta, cointegration weights) rather than
splitting notional evenly, its default is worth re-opening — maintaining a
*correct* ratio is a different proposition from maintaining an arbitrary one.

## Datasets

### A fragmented universe is diagnosed, never repaired

`get`, `run` and `optimize` all warn when no snapshot holds every symbol the
universe carries (`src/cli/overlap.rs`; `get` measures the rows it writes, the
other two the per-symbol streams `join_universe_by_time` consumes). They report
and stop there.

**Joining on the trading date was rejected, and should stay rejected.** Tokyo
closes before New York opens, so folding `^N225 00:00Z` and `SPY 13:30Z` into
one snapshot because they share a date would hand a strategy trading `^N225` an
S&P close from thirteen hours in its future. The exact-stamp grouping is what
stops fugazi manufacturing lookahead across time zones; a `--join-on-date` flag
would be a lookahead switch with a friendly name. The remedy for a fragmented
universe is a different universe (one session's worth of symbols), which is a
dataset choice the consumer makes.

What the warning deliberately does *not* do is compare per-symbol session
signatures: daylight saving alone gives `^FTSE` `{07:00, 08:00}` against
`^GDAXI` `{06:00, 07:00, 08:00}`, so signature equality would flag series that
share nearly every bar. It measures observed co-occurrence, and fires only on
`widest < total`.

Not extended to the two shapes that don't time-join: `single:` has one symbol,
and `pairs:` **inner**-joins its two legs (`run::join_pair_by_time`), so a
disjoint pair yields zero bars and fails loudly already. If a third caller of
`join_universe_by_time` appears, it needs the two-line measure/warn pair — the
joiner stays pure rather than printing, so this is a convention, not a
guarantee.

### `!pick { freq }` still reads nothing under `run`

`get` tags every snapshot entry with its `(symbol, freq)`, so a `get -x` overlay
can say `!pick { freq: 1d }`. `run` does not: `join_universe_by_time` and the
single/pairs paths all push `None` for the freq tag, and `Selector::matches`
requires equality when the query's freq is `Some`, so the same expression
resolves to nothing in a backtest. The bar-cadence census made the
loader *keep* the cadence — `DataFrame` keys on `(symbol, freq, time)` — so the
tag is now available to push.

**Deliberately not pushed.** It is two lines of code and a whole design
decision: snapshots group by exact timestamp, so a `1d` bar occupies one
snapshot in twenty-four and a `!pick { freq: 1d }` leaf would read `None` on the
other twenty-three. Whether that is an absent sample or a forward-fill is the
entire multi-cadence-strategy question — a strategy trading hourly against a
daily filter wants the fill, an indicator averaging daily closes wants the
absence, and the crate has no vocabulary for the difference. Doing the cheap
half first would ship the ambiguity as a feature.

Revisit when there is a concrete strategy shape asking for it; the answer starts
with what a cross-cadence leaf reads between bars, not with the tag.

**A document *may* now declare its own cadence, and that is a different lever.**
`root: !pick { symbol: BTC, freq: 4h }` feeds the **frame loader** — it joins the
cadence precedence chain one rung below `-f/--frequency`, so `cadence::apply`
prunes the frame to that cadence before any snapshot is built
(`RootSpec::declared_freq`). The ambiguity above never arises, because each run
still sees exactly one cadence and a cadence *sweep* is N separate runs, one per
grid row. Nothing about `Selector`, `Pick`, or the `None` freq tag the drivers
push changed. The entry below stays open on its own terms: reading *two*
cadences inside one run is still the unanswered question.

**Not to be confused with cross-*symbol* `!pick`, which does work under `run`.**
A document of any shape may read a symbol it does not trade — the runners carry
`traded ∪ !pick`-named and refuse a name the input lacks (see `spec::reads`).
That case has none of the ambiguity above: two symbols on one cadence share a
bar grid, so "absent" means absent and there is nothing to forward-fill. The
`freq` half is still open for exactly the reason stated.

### `optimize` sweeps the traded series; pooling reduces over it, pairs still refuse

Before `root:`, `optimize` bound one atom slice and one snapshot stream to a
probe symbol for the whole grid. Cross-subgrid disagreement was refused with a
message; disagreement *within* one subgrid was not checked at all, so a
`SYM=[...]` axis silently backtested the probe symbol's bars on every row and
emitted a grid of plausible, wrong numbers. That refusal was never recorded
here, and the silent half was a bug.

Now `distinct_roots` resolves **every combo** — not the per-subgrid probe point,
which is what missed the within-subgrid case — and one stream is prepared per
distinct `(symbol, freq)`, memoized. A row's metrics equal what the same
document produces standalone through `run`; `tests/cross_asset_reads.rs` pins
exactly that, because a test asserting only "the rows differ" would have passed
the old behaviour too.

One refusal kept, one lifted for a case it never covered:

- **`--walkforward` + a root axis that is *ranked on*.** Still refused, and the
  reason is unchanged: folds are laid out over one bar timeline; two instruments
  have different bar counts, so fold *k* would span a different period per row.
  There is no reading of that number worth emitting.
- **`--walkforward` + a root axis that is *reduced over* — now allowed
  (`--pooled`).** A different question, not a relaxation of the above. Pooling
  does not rank the instrument axis at all: each fold picks **one** winner on
  the pooled in-sample score and applies it out-of-sample to every member. The
  objection was that fold *k* would mean a different span per row, and the
  answer is that a pooled fold is not laid out on bar indices — it is laid out
  on the panel's **shared clock**, the sorted union of every member's bar times,
  and mapped down to a member's own bars only at the point of measurement. So
  fold *k* is one span for the whole panel by construction, and a member with no
  bars in it contributes nothing rather than shifting it. See `src/spec/panel.rs`
  and `tests/pooled.rs`.

  What that costs: pooled fold ranges index the shared clock, not any member's
  bars, so they are not comparable to the single-stream driver's. That is
  inherent — a ragged panel has no single bar axis — and the fold table carries
  `is_members` / `oos_members` so the support behind each fold is read off
  rather than assumed.

  Two sub-decisions, both with a defensible opposite:

  - **The head skip follows the first ready member, not the last.** Waiting for
    every member makes each fold fully supported, but truncates the panel to its
    most recent listing — on a few years of hourly crypto that discards most of
    the sample to buy a comparability the support counts already report. Early
    folds resting on few members is the accepted cost.
  - **A ruined member disqualifies the whole row**, rather than dropping out of
    the pooled mean. Dropping it would *raise* the row's score, so a search over
    that objective would be rewarded for finding parameters that destroy an
    account — the same perversity `ranking_lookup` exists to prevent, one layer
    up.

**`fugazi run --pooled` — settled.** `optimize --pooled` over a one-point grid
already *was* a pooled run, so the gap was purely ergonomic: `run`'s job is
reporting what one already-chosen parameter set does, and that's exactly what
`--pooled` on `run` now reports, pooled. Resolved the three open questions:

- **The value-list grammar.** ~~`--params AXIS=[...]` carries the member
  list~~ — **revised: `--pooled` carries the panel itself.** See *`--pooled`
  declares its own panel* below, at the end of this section.
- **Equity-curve output.** No netted curve — same refusal as the pooled
  walk-forward composite, for the same reason: netting `M` members needs a
  weighting and a rebalance cadence, which `portfolio:` already states
  explicitly. Each member writes its own full `run` output
  (`fills.csv`/`trades.csv`/`returns.csv`/`metrics.yml`, plus the windowed CSVs
  under `-w`) under `<out_dir>/<MEMBER>/`, so a pooled run is diagnosable one
  member at a time; the top-level `metrics.yml` is the pooled reduction
  (`fugazi::spec::panel::pooled_document`, shared with the pooled walk-forward
  composite writer rather than a second serializer).
- **`--resume`/`--montecarlo` per member.** Split rather than answered
  uniformly: `--resume`/`--save-state`/`--flatten` are refused outright — a
  pooled run has one member's worth of state per member, not one `RunState`
  for the panel, and resuming a panel one member short of last time is a
  footgun worth naming rather than silently allowing. `--montecarlo` needed no
  decision at all: it already runs and writes `montecarlo.csv` per member,
  since each member drives its own `iterate()` call.

One more bug this surfaced and fixed in passing: `optimize --pooled` on a
`pairs:`/`basket:`/`multi:`/`portfolio:` document was **silently ignored**
rather than refused — `opts.pooled` was only ever read on the single-asset
path. Now refused with a message naming the shape, matching how every other
  single-asset-only knob here is refused rather than swallowed.

- **A `pairs:` grid varying its legs.** A pairs run evaluates the **inner join**
  of its two legs. Widening the stream to the union of every swept pair would
  change which bars each row sees, so a row would stop matching the same
  document run through `run` — the one property that makes the single-asset
  sweep trustworthy. Sweeping pairs needs a per-pair join, which is a bigger
  change than the stream map.

And one warning, not an error: a root axis means rows evaluate different bars,
so the grid is a batch of separate backtests rather than the like-for-like
parameter comparison the rest of `optimize` is built around (`--smooth`, the
grid-wide `max(stable_bars)`). Refusing it would be wrong — comparing an
instrument's best parameters *is* the use case — but leaving it unsaid would
let a reader trust a ranking the numbers don't support. `--pooled` suppresses
that warning, because it is the fix for exactly what the warning describes: the
rows are one per parameter set again, and the axis is reduced over rather than
compared across.

### `--pooled` declares its own panel, over N axes

`--params AXIS=[...] --pooled AXIS` on `run`, and `--grid AXIS=[...] --pooled
AXIS` on `optimize`, both made `--pooled` a *reference* to a member list
declared elsewhere. Two things were wrong with that, one cosmetic and one not.

The cosmetic one: `--params` means *this name equals this value* everywhere
else, and `optimize` **rejects** the exact string `run --pooled` required
(`reject_axes_in_params` on the baseline table). One flag, opposite meanings, in
sibling subcommands.

The structural one: on `optimize` the referenced axis was declared inside
`--grid` only to be immediately carved back out of every subgrid, which bought
two error paths that existed for no other reason — the axis missing from a
subgrid, and two subgrids disagreeing on its members (which would have meant two
rows pooling over different populations, the one property the `_mean` columns
rest on). Declared on `--pooled` itself, both are unrepresentable: the panel is
one population by construction.

So `--pooled` now takes the axes with their values, in the `--params`/`--grid`
term grammar it already shared: `--pooled 'SYM=["BTCUSDT","ETHUSDT"]'`. Ranges
and `@file` come along for free. A name it declares may not also appear in
`--params`/`--grid` — ranked on and reduced over are opposite treatments, and
resolving that by precedence would produce a table whose columns don't say which
happened. `split_pooled_axis` is gone; `fugazi::spec::panel::Panel` owns the
parse, the product and the labels for both subcommands.

**N axes pool over the cartesian product.** `SYM=[...],SLOW=[...]` is a panel of
`|SYM|·|SLOW|` members. It is one question — does this survive across
instruments *and* across the other thing — not a grid of them, so the cells are
averaged rather than ranked, and `-k` penalizes a parameter set that only works
in one cell exactly as it does for one instrument. The reduction kernel never
had to change: it takes a `Vec<PanelMetrics>` and does not care what varied.

Three consequences worth naming:

- **A member's label is the params spec that reproduces it** (`SYM=BTC,SLOW=100`),
  not a bare value. It is what `metrics.yml`'s `members:` keys and the console
  use, chosen so the first thing anyone does with a member that dragged the mean
  down — re-run it alone — is a copy-paste. It is a label, not a parse target: a
  value containing `,` or `=` renders literally and nothing reads it back.
- **Member directories and per-member composite files are index-prefixed**
  (`out/1_SYM_BTC_USDT/`). Sanitizing alone collides: every non-alphanumeric
  character folds to `_`, so `BTC/USDT` and `BTC-USDT` — the same asset as two
  venues spell it, an ordinary thing to find in one panel — both became
  `BTC_USDT`, and the second member's artefacts silently overwrote the first's
  while the pooled `metrics.yml` still reported a mean over both. That was a live
  bug before the multi-axis labels made it likelier; `tests/pooled.rs` pins it.
  The two copies of `sanitize_member` are now one `run::member_file_stem`.
- **A one-point grid is no longer refused under `--pooled`.** The `use \`run\`
  for a single combination` guard exists to catch a sweep that isn't one; a
  pooled row over a one-point grid is still a reduction across a panel, with the
  `_mean`/`_std`/`_n` columns and the deflated-Sharpe machinery intact. `run
  --pooled` reports the same panel as directories plus a YAML reduction; neither
  subsumes the other.

**What this does *not* unlock: a cross-cadence panel on a shared symbol.**
`--pooled 'SYMBOL=[...],FREQ=["1h","4h"]'` is expressible and would be the
motivating case for the product form, but a frame carrying both cadences of one
symbol is refused upstream by the cadence census (`Finding::Ambiguous`, "a
strategy trades one of them") before pooling is ever consulted, and `-f` narrows
per *symbol*, so it cannot vary per member. Making it work means teaching
`cadence::apply` that a panel declares several traded cadences and having
`retain_cadence` keep more than one — plus a cadence-aware `atoms()` on the
per-member path. That is a data-layer change, not a flag-grammar one, and it is
deliberately not in this one. Pooling over `FREQ` works today only where the
cadences live on different symbols.

### Pooling's validity is measured, not classified — *settled, shipped as `--shrink`*

`--pooled` is already an aggregated objective — `ranking_value`'s Panel arm
optimizes `mean_m ∓ k·std_m` over the panel, once. What was missing is any
report of whether aggregating was *valid* for the strategy in hand, and the two
candidate ways to supply it are not equally good.

**Rejected: a static dimension lattice.** `#[grammar(dim = "price" | …)]` on each
`NodeSpec` variant, propagated through `typecheck`, flagging a comparison between
a bare `!value` literal and a dimensioned side — the idea being that pooling a
`$100` stop across BTC and DOGE averages apples. It dies three ways. `!get` is
schema-dependent and must resolve to `Unknown`, so the check goes **silent for
exactly the documents that read prices through overlay columns** and stays loud
on toy ones. Dimensioned comparisons are frequently correct (`!volume > 1e6`, a
support level in a single-instrument document), so the false-positive rate is
unknown and plausibly high. And it is subsumed: a dimensioned axis does not fail
subtly under pooling — the members disagree about the optimum, which is what the
parameter×member interaction term measures, along with regime-dependent
thresholds and genuinely heterogeneous members, none of which the lattice
reaches. Cost avoided: a declaration on ~142 variants, a propagation lattice, an
exemption table with staleness tests, and a staged rollout to bound the
false-positive rate.

What would change it: the interaction readout shipping and users routinely
unable to explain a high fraction. That is evidence a *localizing* check earns
its keep — and by then real cases would name which node shapes cause it, so it
would be a small targeted check, not a lattice. That is the better order anyway.

**Kept, because they are closed sets rather than inference over a user's tree:**
warning when a *dimensioned metric* is the pooled ranking metric (~40 fixed keys
in `metrics::flatten`, already enumerated by `tests/metrics_coverage.rs`; its
cross-member `std` carries each member's volatility, so `-k` penalizes the
panel's composition), and warning on an *unscoped absolute cost term* under
`--pooled` (cost terms scope on stream ids already; a flat `fee_per_trade` across
three price magnitudes ruins the cheap members and reads as "the parameters do
not generalize").

**Chosen instead: measure it.** Pooling buys variance reduction on the selection
surface — `√N_eff`, not `√N` — and only when the swept parameter means the same
thing on every member. That is a **per-axis** property, so a strategy never
written for pooling still benefits on its bar-count axes while being corrupted on
its dimensioned ones, in one sweep, with nothing today separating the two. The
row×member score matrix a pooled sweep already produces supports a two-way
decomposition whose interaction component is the method-of-moments estimator for
the random-effect variance — so one quantity both diagnoses whether to pool and
supplies the weight that acts on it. Complete pooling is that variance at zero;
today's `SYM=[...]` grid axis is it at infinity; partial pooling estimates it.

Three consequences already settled, before any of it is built:

- **Shrinkage goes in score space, not parameter space.** `θ_m = θ̄ + λ(θ̂_m − θ̄)`
  is easier to explain and needs every axis numeric with a meaningful metric (a
  categorical axis has no shrinkage target), produces off-lattice values, and
  shrinks each axis independently — which walks off a `FAST`/`SLOW` ridge.
- **Demeaning is an added column, never a replacement.** Removing the member
  fixed effect makes `-k` mean "ranks consistently well" instead of "these
  instruments are alike", but it destroys the level: the best row of a uniformly
  losing grid ranks first. Pre-1.0 the default *could* be switched; it should not
  be, because it answers a different question rather than the same one better.
- **`--pooled -w` is what makes the variance components estimable.** With one
  observation per cell the interaction and the residual are confounded, and their
  ratio is the whole estimator. Folds supply the replication; the analytic Sharpe
  SE the DSR path already computes is a Sharpe-only cross-check.

Open and not papered over: the DSR **trial count** under partial pooling. Complete
pooling is `N` hypotheses — the argument `src/spec/panel.rs`'s module doc rests
on — and no pooling is `N×M`; between them it presumably scales with the
shrinkage weight, but there is no defensible formula, and making the deflation
honest was pooling's original motivation. That blocks the plain-sweep form, not
the walk-forward one.

Also noted while surveying: `panel::effective_breadth` is computed, exposed to
Python, and printed by **no CLI path** — the number that stops a reader treating
thirty correlated perps as thirty pieces of evidence exists and is invisible.

Three things the build changed, each because writing it exposed a design error:

- **`-w` and `--pooled` were mutually exclusive**, in one clap `ArgGroup` — so
  the sweep had no replication available and the design doc's claim that
  `--pooled -w` is where the components become estimable was simply false. They
  compose now, and the windowed reduction rides *beside* the whole-run document
  (`PanelMetrics::windows`) rather than replacing it, so turning replication on
  changes no pooled number. That surfaced a latent bug in turn: `Sweep::windowed`
  came from the *flag* while `Sweep::panel` came from the *rows*, and once both
  could hold at once the CSV wrote a two-column header over three-column
  records. `windowed` is derived from the rows now, as its own rustdoc had
  argued for.
- **The `λ = 0` reference had to move onto the shrunk scale.** Choosing the
  pooled winner from each member's whole in-sample window while the members
  chose from cell means over sub-spans put two honest numbers on different
  scales, and a fold could report `λ 0.000` beside `2 member(s) chose
  differently` — impossible by construction, since at `λ = 0` every member sees
  one surface. Under `--shrink` the reference is `argmax_r (μ + α_r)`.
- **`-k` is refused alongside `--shrink`.** They are rival answers to the same
  question — one charges for the spread between members, the other models it —
  and applying both pays for the same disagreement twice. Refused rather than
  quietly ignored, per the precedent set by refusing an inert `--smooth`.

And one thing worth knowing before reading a `λ`: there are **two**, and they
disagree on purpose. The per-fold one is estimated from sub-spans of that fold's
own in-sample window, so it is lookahead-free and is what selection acts on —
and because a metric measured over a short span is itself noisy, and that noise
lands in the denominator, it is systematically conservative. The run-level one
uses folds as replicates, is better powered, and is deliberately not used for
selection, because a component estimated over every fold and applied inside fold
1 would let fold 10's data pick fold 1's winner. A low per-fold `λ` beside a high
run-level one is the fold saying it cannot yet separate disagreement from noise
on its own evidence.

The DSR **trial count** under partial pooling, which was the stated blocker on
the plain-sweep form, turned out to need no new theory. Under `--shrink` a member
ranks on `mu + alpha_r + lambda*gamma_rm` — a shared term every member has and a
private term only it has — so two members' *ranking surfaces* are correlated
exactly to the extent the shared term dominates, and `K / (1 + (K-1)*rho)` is the
same correlated-estimators reading `effective_breadth` already applies to member
returns. Trials become `grid points x searches`. Both limits land on counts that
were already right: `x1` at complete pooling (so an agreeing panel's deflated
Sharpe is bit-identical with and without the flag — the property that made it
safe to ship) and `xK` when the members share nothing. `rho` is measured off the
surface rather than derived, so nothing is assumed of the fit's orthogonality and
a ragged table needs no special case.

The output-shape half was never a real obstacle either, once Stage 3's precedent
was taken seriously: don't reshape the grid CSV, add a sibling. Per-member picks
go to `<stem>.member_winners.csv`, written only when the members diverged.

Full design, staging, the measured end-to-end result and the rest of the
rejected alternatives: [docs/design/pooling.md](docs/design/pooling.md).

### Downstream keeps its own panel fork — *settled by `ScoreTable`, on a condition*

fugazi-web builds a pooled sweep as a fork: one `optimize()` per member over
that member's own snapshots, reduced across members by its own `core/pooling`,
rather than calling `optimize(panel=)`. Both sides now agree that is the right
shape, and the entry is here so nobody re-opens it from first principles.

**What settled it is `ScoreTable`, not a preference.** Until 0.87 the fork cost
downstream the shrinkage readouts — `shrink=` is a parameter of
`optimize(panel=)`, so a caller that pools its own way has nothing to plumb it
into, and the only route to `λ` was collapsing the fork. Exposing the estimator
over a caller-built matrix decoupled the two: the fork is now free of the
readout, so neither side trades anything to keep its own panel construction.
That is the load-bearing fact — the question stopped being live because the
coupling went away, not because anyone won it.

**Why the fork exists.** `rooted_documents` substitutes each symbol into the
strategy's *root* and re-renders, so a single-asset strategy is poolable exactly
as its author wrote it, with a **literal** root and no `!param` in the root
slot. `panel_axis=` cannot supply that: it substitutes a named parameter, so it
requires the document to have parameterized its root in the first place.
Collapsing onto `optimize(panel=)` would therefore make a literal-root strategy
un-poolable over instruments — a capability regression, not a refactor — and it
would also change what a stored pooled walk-forward claims, since upstream nets
no composite curve across members (see `MemberComposite`'s refusal).

**The condition, without which this entry goes stale.** What is settled is
keeping the fork *for as long as a literal-root strategy has to be poolable over
instruments*. There is a live downstream discussion about making the param panel
symbol-aware — partitioning the stream when the pooled param is the root's
symbol slot — which would route almost every pooled sweep through a param axis,
which is exactly the shape `panel_axis=` wants. If that lands and literal roots
become rare enough to stop supporting, the collapse gets cheap again and this is
worth reopening on its own merits.

Settled from both sides, on that condition. Not permanently.

## Repo hygiene

### `python/uv.lock` is not enforced by anything, and `uv sync` prunes `maturin`

The lock now covers the `test` extra — `mypy` included — so it matches
`python/pyproject.toml` rather than trailing it.

Two things a reader should know before reaching for `uv`:

**Nothing depends on this file being right.** CI installs via `maturin build` +
`pip install` and never invokes `uv`; no check compares the lock against
`pyproject.toml`. A dependency added to the extra and not re-locked drifts
silently, exactly as `mypy` did the moment it was added — the lock still
resolved, it just did not contain it.

**`uv sync --extra test` breaks the build step.** It prunes the venv to the lock,
and `maturin` is a *build* tool that has no business in a `test` extra — so the
sync removes it and the next `scripts/ci-local.sh` fails at "Build + install"
with `Failed to spawn: maturin`. The supported path is `scripts/ci-local.sh`,
which creates the venv with an explicit list and is what CI mirrors; an earlier
version of this entry called `uv sync` "the documented way to run the suite",
which nothing in the repo has ever said.

If `uv` becomes a first-class workflow rather than a lockfile that rides along,
the shape is a `dev` extra (`fugazi[test]` plus `maturin`) and a CI
`uv lock --check`. Neither earns its keep while the failure mode is a local venv
losing a tool the script would reinstall.

### Both formatters run at their defaults, and there is no `rustfmt.toml`

Decided by measurement, not taste. The candidates were run over the whole tree
and scored by files touched (`cargo fmt --all -- --check`, at the 0.69.0 tree):

| config | files reformatted |
|---|---|
| **rustfmt defaults** | **1708** |
| `max_width = 90` | 2077 |
| `max_width = 88` | 2265 |
| `max_width = 100`, `use_small_heuristics = "Max"` | 2520 |
| `max_width = 80` | 3308 |
| `max_width = 80`, `use_small_heuristics = "Max"` | 3593 |

Defaults win at both ends, for opposite reasons. Widening loses because the code
is *written* at 80 columns (p95 = 80 characters across `src/` + `python/src/`),
so a wider budget re-joins hand-split lines instead of leaving them alone —
`use_small_heuristics = "Max"` is the extreme of this and is the worst option
tested. Narrowing loses because rustfmt's small-item heuristics are *derived*
from `max_width`: cutting it to 80 shrinks `struct_lit_width` and
`fn_call_width` with it, and explodes short literals that fit fine today.

So there is no `rustfmt.toml`, and adding one should come with the same table.
The two knobs that would genuinely suit this codebase — `wrap_comments` and
`group_imports` — are **nightly-only**, and the gate runs on stable. Their
absence is also load-bearing in the good direction: the hand-set 80-column prose
in every module header is untouched by the formatter and stays that way.

`ruff.toml` does exist, but only because `tools/` sits outside
`python/pyproject.toml` and would otherwise resolve against ruff's built-in
defaults — two configurations for one repo. Its values *are* ruff's defaults,
written down so a release cannot move them silently.

What would change it: rustfmt stabilizing `wrap_comments` (then the module
headers are worth a look, and the table is worth re-running), or the tree
drifting far enough from 80 columns that the p95 argument no longer holds.

### The ruff *linter* is not wired — only the formatter

`ruff check` runs nowhere. `ruff.toml` has no `[lint]` section, on purpose.

Formatting is a decision with one answer and no backlog: run it, commit the
baseline, gate it. Linting is neither. It is a rule-set choice (which of ruff's
~800 rules, in a repo whose Python is 21 files of test harness and fixture
generators), and every rule enabled arrives with findings someone has to triage
— against code whose job is to be an *independent* reference implementation, where
"more idiomatic" is not obviously better.

What would change it: the Python surface growing past test-harness scale, or a
concrete bug that a specific rule set would have caught. Then it lands as its own
commit with its own baseline, the way the formatter did — not as a line added to
this file.

### Parallelising the local gate was measured and reverted; `pytest` is the target

`scripts/ci-local.sh` runs serially, and stays that way. It was rewritten to run
`fmt` and `version-sync` first, then `rust`, `features` and the Python build
concurrently, then `pytest`; the whole thing was measured on 16 cores and thrown
away. Numbers, so nobody has to re-derive them:

| | serial | parallel |
|---|---|---|
| warm tree | 1:53.7 | 1:30.2 |
| dirty tree (two feature commits) | 5:56.9 of work | 4:11.5 |

**Why it went back.** Twenty-three seconds on a warm tree cost 232 lines, 2.4 GB
of duplicate target directories, and the live output — three concurrent cargo
runs interleaved are unreadable, so each stream had to be buffered to a file and
printed whole when it finished. It also saturates all 16 cores, which is bad
manners on a machine shared with the work you are actually trying to do. The
dirty-tree win is the real one and dirty runs are the rare ones.

**Concurrency has to be bought with `CARGO_TARGET_DIR`, if it is ever bought
again.** Cargo holds an exclusive lock on a build directory for the whole
invocation, so simply backgrounding the jobs against one `target/` yields
`Blocking waiting for file lock on build directory` and a serial run with extra
steps. Each stream needs its own directory; the duplication is ~1.2 GB apiece and
is only the third-party dependency graph, because every feature-matrix row and
the bindings resolve to a different feature set than the `rust` job and never
shared `fugazi` artifacts anyway.

**`pytest` cannot run beside a compiler.** The two interrupt tests in
`python/tests/test_specs.py` calibrate an uninterrupted duration and then fire
`SIGINT` at a quarter of it, so they need the load *steady*, not merely low.
Beside three cargo streams that start and stop, the calibration runs slow and the
measured run runs fast, and the interrupt lands after the work has finished: both
failed that way on the first parallel run, and both pass standalone at a load
average of 16. Any future attempt needs a barrier before `pytest`, which costs
~9% of the wall clock the parallelism could otherwise have saved.

**What is actually worth attacking.** `pytest` is **1:12 of the 1:53** — 64% of a
warm run, and everything else put together is the other third. Nothing structural
about the gate matters next to that one number, which is why the timings are now
printed slowest-first at the end of every run. `pytest-xdist` (`-n auto`) is the
obvious answer and is deliberately not taken: it is not installed, the suite has
never been run under it, and the two interrupt tests above are exactly the shape
that breaks when tests share a worker pool. What would change it: someone running
the suite under `-n auto` enough times to show it is stable, landing it as its own
commit with the dependency added to `ci.yml`'s install list, the venv list in the
script, and `python/pyproject.toml`'s `test` extra together.

The one subtraction that was kept is the feature matrix's `cargo check` rows. The
first is contained in the second — clippy is the front-end plus lints, `-D
warnings` makes it strictly stricter, and the two share their dependency
artifacts — so seven of the fourteen invocations were a second analysis of the
library carrying no signal. Dropped from `ci.yml` as well, since each matrix job
ran both sequentially on one runner.
