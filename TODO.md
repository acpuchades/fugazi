# TODO

Deferred work, with the reasoning that deferred it. Each entry says what was
decided and what would change the decision — an item here is a *judgment made*,
not a task nobody got to.

## Python bindings

### A whole-wallet default cost bundle

`PaperWallet.set_costs_for(symbol, costs, freq=None)` (0.52.0) covers per-symbol
costs, which is what the driver layer itself installs and what `by_symbol`
scoping expects. Rust also has `PaperWallet::with_costs(funds, costs)` for a
single bundle applied to every symbol, and that has no Python twin.

Deferred because resolving a `CostConfig` into one bundle needs a symbol to
resolve *against*, so a whole-wallet form would have to pick a placeholder and
silently take the config's `default:` leg — quietly wrong for any config using
`by_symbol`. Worth doing if a caller hits the N-symbol boilerplate; the honest
shape is probably `set_costs_for_all(symbols, costs, freq=None)` looping the
existing call, not a `with_costs` mirror.

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

### `python/uv.lock` does not lock the `test` extra

The checked-in lock covers only the base dependencies, so `uv sync --extra test`
— the documented way to run the Python suite locally — rewrites it every time
and leaves a dirty tree. Locking the extra would fix that, but it is churn
unrelated to any release, so it wants its own commit.

Note that CI does not use `uv` at all (`maturin build` + `pip install`), so
nothing depends on this file being right. Nothing enforces its version either,
which is why the release checklist lists it explicitly.
