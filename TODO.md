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
`Wallet::adjust_funds`, `doc/METRICS.md`, `fugazi.metrics.__doc__` and the
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
