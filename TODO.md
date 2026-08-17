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

## Repo hygiene

### `python/uv.lock` does not lock the `test` extra

The checked-in lock covers only the base dependencies, so `uv sync --extra test`
— the documented way to run the Python suite locally — rewrites it every time
and leaves a dirty tree. Locking the extra would fix that, but it is churn
unrelated to any release, so it wants its own commit.

Note that CI does not use `uv` at all (`maturin build` + `pip install`), so
nothing depends on this file being right. That is also why its version drifted
nine releases before anyone noticed — see the bump checklist in `CLAUDE.md`,
which now lists it.
