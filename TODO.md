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
