# Testing (architecture reference)

How this crate's tests are organised, what each layer is responsible for, and
the rules that keep them from drifting back into the shapes this document was
written to prevent. [CONTRIBUTING.md](CONTRIBUTING.md) is the *procedure* for
adding a thing; this is the *map* of where its tests go.

- [The layers](#the-layers)
- [Where a test goes](#where-a-test-goes)
- [Shared support — `tests/common/`](#shared-support--testscommon)
- [The drift guards](#the-drift-guards)
- [Fixtures, and the skip-vs-fail policy](#fixtures-and-the-skip-vs-fail-policy)
- [Rules that bite](#rules-that-bite)

---

## The layers

Five, from narrowest to widest. Each has one job; a test that could live in two
places belongs in the narrower one.

| Layer | Lives in | Sees | Runs under |
|---|---|---|---|
| **Unit** | `#[cfg(test)] mod tests` beside the code | `pub(crate)` internals | `cargo test --lib` |
| **Integration** | `tests/*.rs`, one crate per file | the public API only | `cargo test` |
| **End-to-end** | `tests/{run,costs,optimize,overlap,cadence,examples_validate}.rs` | the `fugazi` binary via `Command` | `cargo test` (needs the `cli` feature) |
| **Cross-validation** | `tests/{talib,metrics,wallet,trade_metrics}_validation.rs` | an external reference library's numbers | `cargo test` (every fixture committed; skips only if one is removed) |
| **Coverage guard** | `tests/metrics_coverage.rs` | which metrics have a reference at all | `cargo test` (reads key sets only — never skips) |
| **Performance guards** | `tests/perf_guard.rs` | allocation counts and type widths | `cargo test` |

Plus **doctests** (37 of them, mostly in `README.md` and the strategy-shape
docs), which are the executable half of the user-facing prose. A `no_run` /
`ignore` doctest is a compile check only — say so in the fence when that is
deliberate.

The performance layer is last for a reason: it asserts **only what is exact**.
Nothing in it is timed, because a wall-clock assertion on a shared CI runner
fails on contention rather than on regressions. What it does check —
allocation counts that must not scale with bar count, and the `size_of` facts
the erasure design rests on — is deterministic across machines, allocators and
rustc versions. Timed comparisons live in `benches/` and
`scripts/perf-compare.sh`, run by a human against a baseline; see
[PERFORMANCE](PERFORMANCE.md).

### Unit tests

The bulk of the suite (~785 in the library, ~93 in the binary). They exist to
reach what the public API can't: `pub(crate)` cores (`WindowStats`,
`WilderState`), private helpers, and the wildcard-free match tables that back
the drift guards.

The four shared O(1) cores in `src/indicators/stats.rs` are tested
**differentially** — drive the core and a deliberately naive recomputation over
the same adversarial stream and require them to agree bar for bar. A
hand-written expected series would pin one window; this pins the *eviction
bookkeeping*, which is where the bugs are. Reach for that shape whenever a piece
of code is an optimisation of an obvious slower one.

### Integration tests

Each file under `tests/` compiles as its own crate against the public API, so
these are also the crate's most honest API-shape review: if a test needs
something not exported, so does a user.

**They must drive through the production entry point.** For anything strategy-
shaped that is [`fugazi::backtest::run`], never a hand-rolled `for` loop —
because `run` calls `trade()` **only when `is_ready()`**, and routes fills and
rejections in a defined order. This is not hypothetical: `tests/strategies.rs`
and `tests/pairs.rs` used to roll their own loop and call `trade()`
unconditionally, and two catalogue tests were asserting trades the real driver
suppresses. `tests/driver_contract.rs` now pins that contract directly.

### End-to-end tests

Shell out to the binary through `common::cli::Cmd`, assert on the artefacts it
wrote. Keep them about **wiring** — that a flag reaches the resolver, that a
column appears, that two spellings agree. Numeric truth belongs a layer down,
where a failure names the function rather than the subcommand.

### Cross-validation

fugazi's numbers against an independent implementation's. These pin
*conventions* (which seed, which divisor, which quantile rule, which bar a fill
lands on), which is the one thing a self-consistent test can never catch. One
suite per layer, because no reference library spans two:

| Suite | Layer | Reference |
|---|---|---|
| `talib_validation.rs` | indicators | TA-Lib |
| `metrics_validation.rs` | equity-curve metrics | empyrical |
| `wallet_validation.rs` | `PaperWallet` execution | vectorbt |
| `trade_metrics_validation.rs` | trade-level metrics | backtesting.py |

`wallet_validation.rs` is the one that reaches below the equity curve: it
replays a fixed order schedule and compares cash, position and equity bar by
bar, which is what pins the **fill-timing rule** (queue at bar N, fill at bar
N+1's open — the difference between a backtest and a lookahead). The other three
all start from numbers a fill already produced.

Two things a cross-check cannot do, and the guards for each:

- **Disable itself.** All four skip when their fixture is missing — see
  [the fixture policy](#fixtures-and-the-skip-vs-fail-policy) and
  `FUGAZI_REQUIRE_FIXTURES=1`.
- **Notice what it never covered.** A new metric with no reference value is not
  a stale fixture; nothing above goes red for it. `tests/metrics_coverage.rs`
  walks `metrics::flatten` and demands every field carry either a reference
  value or a written exemption naming what does cover it. It reads fixtures for
  their key sets only, so it needs no reference library and cannot skip.

**Where the two disagree, say so in the generator.** backtesting.py's
`Profit Factor`, `Avg. Trade [%]`, `Exposure Time [%]` and two duration fields
each answer a different question from the fugazi field sharing their name. Those
divergences are documented and *asserted* in
`tools/gen_trade_metrics_fixtures.py`, so a future version quietly changing
convention fails the generator rather than silently re-baselining the fixture.
A cross-check whose disagreements are undocumented decays into a golden master.

---

## Where a test goes

| You changed | It needs |
|---|---|
| An indicator's math | a case in `tests/indicator_reference.rs` (hand-derived expected values) **and** in `tests/warm_up.rs` |
| An indicator's warm-up / settling | `tests/warm_up.rs` — and nothing else; that file is the sole authority |
| A `pub(crate)` core | a differential unit test beside it |
| A `NodeSpec` tag | the compiler and the catalogue/parity guards will tell you; add grammar coverage in `tests/spec_grammar.rs` |
| Strategy decision logic | `tests/strategies.rs` (catalogue-wide) or the shape's own file (`pairs.rs`, `portfolio.rs`) |
| `backtest::run` / `backtest::warm_up` | `tests/driver_contract.rs` |
| Run resuming (`save_state`/`restore_state`, `RunState`, `--flatten`) | `tests/resume.rs` — chunked-resume-vs-one-shot for **every** shape, at three or more chunks — **and** `python/tests/test_specs.py`, which drives the same property through the bindings |
| Wallet order flow | unit tests in `src/wallet.rs`; live venues via the `common::live` conformance suite in `tests/live_<venue>.rs`, against `wiremock` |
| `PaperWallet` fill pricing, cash or cost arithmetic | `tests/wallet_validation.rs` — extend the schedule in `tools/gen_wallet_bars.py` or add a cost configuration, then `pixi run gen-wallet` |
| A metric | a unit test in `src/metrics.rs`, **plus** a reference value in one of the two `(metric, expected)` generators — `tests/metrics_coverage.rs` fails until it has one or an exemption |
| A CLI flag | `tests/run.rs`, `tests/costs.rs` or `tests/optimize.rs` via `common::cli::Cmd` |
| Ruin — the zero-equity floor (`RunReport::ruin_bar`, the pinned curve, the bounded drawdown) | `tests/ruin.rs`, which owns the property end to end: the driver, the metrics that derive from it, slicing, `--flatten`, `portfolio:`, and the `optimize --best-by` ranking. It spans four layers, so it is feature-named rather than split across the files for each |
| A diagnostic one subcommand prints | the file named for that command; one spanning several (like the snapshot-overlap warning or the bar-cadence census) gets a feature-named file — `tests/overlap.rs`, `tests/cadence.rs`, as `tests/costs.rs` already does for `--costs` |
| An `examples/` file | nothing — `tests/examples_compile.rs` (Rust) and `tests/examples_validate.rs` (YAML) cover the directory, and each refuses to let a new file in uncovered |
| A hand-maintained mirror (`NodeSpecRaw`, the `fugazi.metrics` registration) | `tests/hand_maintained_mirrors.rs` |
| Anything on the per-bar path (`update`, `trade`, `on_fill`, the driver) | nothing new — `tests/perf_guard.rs` already asserts that path allocates a constant number of times regardless of bar count, and will fail if you add a per-bar allocation |
| A remote provider | `tests/sources_<venue>.rs` against `wiremock` — **never** the live API |
| Anything with a Python mirror | `python/tests/` in the same PR (see the parity discipline in [ARCHITECTURE](ARCHITECTURE.md#parity-discipline)) |

---

## Shared support — `tests/common/`

Each integration file is a separate crate, so shared code is `mod common;`-
included rather than imported. Before it existed the harnesses were
copy-pasted: `unique_path` byte-identical in `run.rs` and `costs.rs`, `serve` in
`live_okx.rs` and `live_coinbase.rs`, six files with their own `flat_bar`. A fix
to one copy never reached the others.

| Module | Holds | Feature gate |
|---|---|---|
| `common::bars` | synthetic candles (`flat`, `banded`), snapshot streams, `overlay_only_atom` | — |
| `common::fixtures` | `tests/data/` CSV loading, `Csv`, and the skip-vs-fail policy | — |
| `common::cli` | `Cmd` (fluent binary invocation), `Outcome`, `unique_path`, `Artefacts` | `cli` |
| `common::net` | `serve` — a `wiremock` server on a kept-alive runtime, for **blocking** clients | `sources` |
| `common::live` | the conformance suite every venue wallet must pass: `LiveVenue`, `VenueFixture`, `mount` | `live` |

Three conventions:

- **A venue wallet's behaviour is shared; its payloads are not.**
  `common::live` holds thirteen parameterized bodies a venue lists as one-line
  `#[test]` delegations, and they assert **counts and outcomes** — one POST
  reached the venue, one rejection was booked, this fill reached the strategy.
  Request-body assertions stay in `tests/live_<venue>.rs`, because the payload
  *is* the venue contract and a shared assertion over it would be an
  `if venue ==` in disguise. Adding a venue means implementing `LiveVenue`;
  nothing in the suite needs editing.
- **Shapes are shared; series constants are not.** `common::bars` gives you bar
  and snapshot *builders*. A file whose assertions depend on which crossovers
  its path fires (`resume.rs`, `montecarlo.rs`, `strategies.rs`) keeps its own
  generator — sharing one would couple those expectations, and a tweak for one
  test would silently retune the others.
- **`mod common;` only where used.** Cargo compiles the module into every
  including crate, so an unused helper is dead code there (hence the blanket
  `#![allow(dead_code)]`, which is the cost of the idiom, not an oversight).

`Cmd` always captures output and puts stderr in the panic message. Three of the
hand-rolled invocations it replaced used `.status()`, so a non-zero exit failed
with `"exited with failure"` and no diagnostic — exactly when you need one.

---

## The drift guards

Tests whose job is to fail when two things that must agree stop agreeing.
[CONTRIBUTING.md](CONTRIBUTING.md#the-drift-guards) has the full table of what
each failure *means*; the architectural point is how they're built:

**Derive the expected side, never hand-write it.** The catalogue and parity
guards read their expected set off serde's own variant list
(`spec::typecheck::known_node_tags` and friends), so they stay correct for free.
A hand-maintained list is the thing they exist to replace.

**Make omission a compile error where you can.** `src/spec/typecheck.rs`'s two
matches are exhaustive with no wildcard, so a new `NodeSpec` variant does not
build until it is classified. `#[derive(SaveState)]`'s default-is-state rule is
the same trick: forgetting `#[state(source)]` on a new box field fails to
compile rather than silently losing state on resume.

**Pin the count when a list can only shrink silently.**
`strategies.rs::the_catalogue_covers_every_built_in_strategy` turns "someone
dropped an entry while refactoring" into a failure rather than a quietly
narrower sweep.

**Guard the tooling too, not just the code.** `ci_mirror.rs` compares
`scripts/ci-local.sh` against `.github/workflows/ci.yml` command by command,
because a local gate that has silently fallen behind CI is worse than no local
gate: it reports green and the push goes red. It checks one direction only — the
script may run *more* than CI, never less. It also guards a three-way constant:
the ruff version pin appears in the workflow, the script *and*
`scripts/hooks/pre-commit`, and a hook pinned to a different series would format
code the gate then rejects.

---

## Fixtures, and the skip-vs-fail policy

`tests/data/` holds committed inputs and generated reference values; the
generators are in `tools/`, and neither reference library is a Cargo dependency.
They come from `pixi.toml` at the repo root — `pixi run gen-talib` /
`pixi run gen-metrics`. See [tests/data/README.md](../tests/data/README.md) for
the file-by-file detail.

The rule that matters:

> **A skip is indistinguishable from a pass.** Any suite that can decline to run
> must be able to be made to fail instead.

This bit here: `talib_expected.csv` was in `.gitignore`, so on every clean
checkout the TA-Lib cross-check compared **nothing** while being listed as a
drift guard. The policy that resolves it:

0. **Commit the generated fixture.** Both are now committed, so both suites run
   everywhere by default. This is the part that actually fixed it; the rest is
   defence in depth.
1. `FUGAZI_REQUIRE_FIXTURES=1` turns every missing-or-stale fixture from a skip
   into a failure. **CI's Rust job sets it**, so a re-ignored or stale fixture
   fails the build rather than quietly narrowing what is checked.
2. A skip prints a **banner**, not a one-line `eprintln!` lost in the noise.
3. Each suite asserts it compared a **non-zero** number of cells, so a
   present-but-empty fixture fails rather than passing vacuously.
4. `tests/indicator_reference.rs` holds the numeric line unconditionally. Its
   expected values are hand-derived from each indicator's own definition and
   shown in the comment above each one, so it needs no external library and
   cannot skip.

Neither replaces the other: `indicator_reference` pins fugazi against its own
documented formulas; the cross-checks pin it against an independent
implementation's conventions.

There is a fifth point, which is about the *other* end of the same guard.
A committed fixture is only a reference if you can tell what changed it, so the
environment that generates it is pinned too — `pixi.lock` is committed alongside
`pixi.toml`. Regenerating without changing anything must produce an empty `git
diff`; the previous unpinned conda env failed that, and had additionally rotted
into an outright crash (empyrical calls `np.NINF`, removed in NumPy 2). See
[tests/data/README.md](../tests/data/README.md#why-the-environment-is-pinned).

The same failure mode is already called out in `.github/workflows/ci.yml`,
where `jsonschema` and `pyyaml` are installed explicitly because the Python
schema tests `importorskip` them — leaving them out makes those files skip
silently rather than fail.

---

## Rules that bite

**A golden master is not a reference value.** A number recorded from the
implementation agrees with any bug the implementation already has. In
`tests/indicator_reference.rs` every expected value is derived from the
definition and the derivation is in the comment. If you cannot write the
derivation, the test belongs in a cross-validation suite where the reference is
named.

**Assert the behaviour, not the absence of a panic.** `let _ = sig.is_true();`
after a loop tests nothing; neither does `assert!(x.is_finite())` on a value
that is finite by construction. If the interesting outcome is hard to assert,
that is usually the test telling you the input fixture is wrong.

**Prefer the production path to a convenient one.** See the driver rule above.
The same applies to fixtures: an *untagged* snapshot (what a bare `Vec<Candle>`
lifts into) is skipped for wallet pricing, so a test written against
`Vec<Candle>` measures a flat curve and passes for the wrong reason —
`driver_contract.rs` pins that too.

**One mega-test per file hides failures.** Where a battery is genuinely one
sweep (`warm_up.rs`, the catalogue), pass the case's *name* into the helper so
the message identifies it. Where the cases differ in **why** they are grouped —
`talib_validation`'s exact-convention vs. converged-seed families — split them,
because the grouping is the information.

**Tolerances carry a reason.** `1e-12` for closed-form arithmetic, `1e-9` for
two implementations of one formula, `2e-2` for a recursive smoother compared
over its converged tail only. A loosened tolerance with no comment is a silenced
failure. `stats.rs`'s
`variance_precision_is_bounded_by_the_mean_to_dispersion_ratio` goes further and
pins a *known limitation* with a note saying which assertion to delete when it
is fixed.

**Test data goes in a unique temp path.** `common::cli::unique_path` — never a
fixed name in the shared `/tmp`, which collides with a parallel run or another
user's leftovers and surfaces as `PermissionDenied`.

**Network tests hit `wiremock`, never the venue.** The handful that touch a real
endpoint are `#[ignore]`d and say so in their ignore reason.
