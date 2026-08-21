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

### `Wallet.take_rejections`

Still unbound, with the reason recorded in `python/tests/test_parity.py`: it
needs a bar-less rejection type, and `RunReport.rejections` already exposes the
same entries for the run path. Only worth revisiting for a caller driving the
wallet loop by hand who needs rejections *during* the loop rather than after.

### Position-anchored protective stops, and the Rust recipe catalogue

Noted in the Python README's `Strategy` section as not bound. Drop to the wallet
loop for those.

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

**Not to be confused with cross-*symbol* `!pick`, which does work under `run`.**
A document of any shape may read a symbol it does not trade — the runners carry
`traded ∪ !pick`-named and refuse a name the input lacks (see `spec::reads`).
That case has none of the ambiguity above: two symbols on one cadence share a
bar grid, so "absent" means absent and there is nothing to forward-fill. The
`freq` half is still open for exactly the reason stated.

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
