# CLAUDE.md

Guidance for Claude Code in this repo: the **invariants, conventions, and
quick-reference**, read every session. Depth lives elsewhere — reach for it on demand,
and put new depth there rather than here.

| Doc | What it is |
|---|---|
| [ARCHITECTURE](docs/ARCHITECTURE.md) | Subsystem internals: indicator taxonomy, every strategy shape, wallet, run resuming, Monte Carlo, the spec/optimize kernel, Python parity. "See ARCHITECTURE" below means here. |
| [CONTRIBUTING](docs/CONTRIBUTING.md) | The *procedure*. Adding an indicator / signal / operator / metric / provider / live wallet: every place the change has to touch, in order, and which are compiler- or test-enforced. |
| [TESTING](docs/TESTING.md) | The suite's *map*: the five layers, where a given change's test goes, the `tests/common/` harness, how the drift guards are built, the skip-vs-fail fixture policy. Read before adding a test file or helper. |
| [TRADING](docs/TRADING.md) | The *execution path*, end to end: bar → submission → queue/rest → fill → the three books → closed trade. The ordering rules and why (nothing fills on the bar that caused it; fills precede `update`). |
| [STRATEGIES](docs/STRATEGIES.md) · [CLI](docs/CLI.md) · [COSTS](docs/COSTS.md) · [METRICS](docs/METRICS.md) · [PYTHON](docs/PYTHON.md) | User-facing surface docs. |
| [PERFORMANCE](docs/PERFORMANCE.md) | Measured history, phase by phase — what was tried, what it cost, what was reverted. |
| [TODO](TODO.md) | A *decision log*, not a backlog: a judgment already made and what would change it. Read it before re-litigating; don't burn it down. |

## What this is

`fugazi` is a Rust library (edition 2024) of **incremental** technical-analysis
primitives. Every primitive owns its state and advances one sample at a time via
`update()` in ~O(1) — same code for live streaming and batch backtesting. Three
composable layers: **indicators** (numeric sources), **signals**
(`Indicator<Output = bool>`), **strategies** (decision layer trading into a wallet).

**Dependencies.** Unconditional: `serde` + `serde_json` (with **`float_roundtrip`** —
load-bearing for run resuming; without it a restored f64 seed drifts 1 ULP and the
resumed equity curve diverges), `time`, `statrs` (Φ/Φ⁻¹ for PSR/DSR), and the internal
**`fugazi-derive`** (`#[derive(SaveState)]`). Everything else is a feature:
`default = sources + cli`, and `cli → spec + sources + montecarlo`, `spec → runtime +
parallel`. Off unless asked for: **`live`** (venue wallets) and, on its own,
**`montecarlo`** (the crate's only source of randomness — it gates `rand` +
`rand_chacha`, and `cli` turns it on so `run --montecarlo` always exists,
runtime-gated). New unconditional deps are judgment calls — reach for closed-form first.

## Commands

**Before pushing, run `scripts/ci-local.sh`** — it runs exactly what
`.github/workflows/ci.yml` runs, in the same order, with the same env. `cargo test`
plus `cargo clippy` is *not* the gate and never was: four CI checks fire nowhere
else, and each has already broken a green local tree.

| Only checked by | Command |
|---|---|
| rustdoc lints (`redundant_explicit_links`, doc-comment reattachment) | `RUSTDOCFLAGS="-D warnings" cargo doc --no-deps -p fugazi` |
| `python/src` — ~11k lines every other clippy scopes past with `-p fugazi` | `cargo clippy -p fugazi-python --all-targets -- -D warnings` |
| the live wallets — `live` is off by default, so a plain `cargo test` runs *none* of `tests/live_*.rs` or `src/live/`'s unit tests | `cargo test -p fugazi --features live --lib --test live_okx --test live_coinbase --test live_kraken --test live_portfolio` |
| the feature matrix — the `--no-default-features` configurations compile in no other job | `cargo clippy -p fugazi --no-default-features --features <f> --lib -- -D warnings` |

`scripts/ci-local.sh [fmt|rust|version-sync|features|python]` runs one job; `FAST=1`
skips the feature matrix and the wheel rebuild for an inner loop — not enough before a
push. **`tests/ci_mirror.rs` fails if the script and the workflow drift**, so adding a
CI step means adding it to the script too. It also pins the three-way `ruff` version
constant (workflow / script / hook).

**Formatting is gated** — `cargo fmt --all` + `ruff format`, both **at their defaults**
(the alternatives were measured; see TODO *Repo hygiene*). Install the hook once per
clone: `git config core.hooksPath scripts/hooks`. It rewrites only *fully* staged files;
a partially staged one is checked, never rewritten.

- Build: `cargo build`; Test: `cargo test`; Lint: `cargo clippy --all-targets` (keep
  clean); Docs: `cargo doc --open`
- `FUGAZI_REQUIRE_FIXTURES=1 cargo test` — the four cross-validation suites (TA-Lib /
  empyrical / vectorbt / backtesting.py, one per layer) **fail** instead of skipping on a
  missing or stale fixture. CI's Rust job sets it. **A skip is indistinguishable from a
  pass** — that, the `metrics_coverage` guard that catches what the switch can't, and the
  divergences asserted in the generators are all in [docs/TESTING.md](docs/TESTING.md).
- **Regenerating: `pixi run gen`** (`gen-talib` / `gen-metrics` / `gen-wallet` /
  `gen-trades` individually). `pixi.toml` + the committed `pixi.lock` are the *tooling*
  env only, and the lock is load-bearing: it pins `numpy < 2` (empyrical calls the
  removed `np.NINF`) and makes a regenerated fixture's `git diff` attributable to your
  change rather than a BLAS kernel. `pixi run -e bench bench` for the benchmark.

### Bumping the version — sync **seven** places (`cargo check` only catches Rust drift)

1. `Cargo.toml` (workspace root, `X.Y.Z`) — **and** the `fugazi-derive = { …, version =
   "X.Y.Z" }` dependency line in the same file
2. `fugazi-derive/Cargo.toml` (the proc-macro crate, `X.Y.Z`)
3. `python/Cargo.toml` (pyo3 cdylib, `X.Y.Z`)
4. `python/pyproject.toml` (wheel metadata, `X.Y.Z` — what `pip install fugazi` sees)
5. `README.md` — `## Install` snippet, `fugazi = "X.Y"` (major.minor)
6. `python/uv.lock` — the `[[package]] name = "fugazi"` entry's `version`. Nothing reads
   it (CI installs via `maturin` + `pip`, not `uv`), so it drifts silently — it sat at
   `0.42.0` through nine releases. One line; keep it honest.

This list is the **single copy** — CONTRIBUTING's *Release a version* points here rather
than repeating it, and CI's `version-sync` job checks it. Then `cargo check --workspace`
(**not** `build --workspace`: it links the pyo3 cdylib and dies in `ld:` output that
reads like a broken release), commit, tag, and follow
[CONTRIBUTING *Release a version*](docs/CONTRIBUTING.md#release-a-version) — publishing
the GitHub Release, not pushing the tag, is what triggers the publish workflow.

## Design invariants

These are the rules that keep the codebase coherent. Violating one usually looks fine
locally and breaks something three layers away.

### Composition is construction

New "X of Y" takes source `S` in `new` (or `of` for source-generic leaves) with the
right output constraint. **No pipe/`then`/`Chain` combinators** — chaining *is*
nesting constructors: `Ema::new(Sma::new(src, 10), 20)`. `IndicatorExt` /
`BoolIndicatorExt` are fluent builders for **operators only** (comparisons, arithmetic,
lookback, crossover, `unstable`); named indicators use `::new` — don't add `.sma()`-style
builders. Use the internal cores (`EmaState`/`WilderState`, `WindowStats`/
`WindowExtreme`), not each other's public types.

### Adding an operator

A `*Op` type impl'ing the relevant trait (`BinaryOp` / `UnaryOp` / `LookbackOp` /
`ExtremeOp` / `CumulativeOp`) plus a type alias — **never a macro**. Arithmetic, boolean
and lookback ops are zero-sized `Default` markers; comparisons carry a
`Tolerance { abs, rel }`, band `max(abs, rel·max|operand|)`, default `(1e-12, 1e-9)` —
**relative, because operand scale is unbounded**. (The execution-side *quantity* epsilons
are separate, in `src/wallet/types.rs`.) `Combine` feeds the *same* input to both sides
(needs `Input: Clone`; name them `lhs`/`rhs`) and holds its op by value;
`Unary`/`Lookback`/`Extreme`/`Cumulative` hold a zero-sized one as
`PhantomData<fn() -> Op>`. `Change` is a **bidirectional** toggle detector — directional
events come from pairing it with a comparison.

**One marker may wear several hats.** `AddOp` is binary `+` *and* the `CumSum` fold;
`MaxOp`/`MinOp` are the pairwise, rolling *and* cumulative extremes. Reach for that
before adding a near-duplicate op type.

### Blessed series — what a `source:`-omitted leaf reads

Two stacked defaults on any atom leaf; don't conflate them. **Per-tag**
(`expr.rs`'s `default_source` / `default_bar_source` / `default_high` / `default_low`)
is which *sub-expression* a wrapper defaults to — `!sma` → `!close`, `!atr` →
`!current`. The **blessed series** is which *asset* the leaf you bottom out at
projects out of the bar's `Snapshot`: an explicit `root: Root<'_>` parameter on
`NodeSpec::try_build`, `None` meaning `Pick::new()` (sole-atom, panics on 2+).

Four rules that bite. The mechanism, the per-context table of who blesses what, and
`root:`'s default splice are in
[ARCHITECTURE *Blessed series*](docs/ARCHITECTURE.md#blessed-series--the-root-a-source-omitted-leaf-reads-srcspecrootrs).

- **The `RootSpec::as_pick` fast path is correctness, not cost.** Only
  `Pick::rooted` has the *match, else sole-atom unpack* fallback, so any tag driving a
  sub-chain over **untagged** synthesized bars (`!resample`'s `inner:`, the
  `Vec<Candle>` / `Vec<Atom>` drivers) reads `None` on every bar without it — and
  reports a plausible zero-fill backtest rather than failing.
- **Blessing scopes the *default*, never the reachable set.** Any shape may `!pick` any
  symbol in the input, traded or not; the runners carry `traded ∪ !pick`-named and
  nothing else. A named symbol the input lacks is a **hard error**, for the same
  zero-fill reason. See `spec::reads`.
- **`!arg SYM` is optional in basket/multi templates** — the bare and fully-spelled
  forms build the same chain; the explicit one is only for reading a *different* symbol.
- **`Pick::rooted` falls back through `sole_atom_or_none`, not `_or_panic`** — a 2+
  snapshot in a rooted context means "the blessed leg is absent this bar". The three
  `sole_atom_or_*` spellings differ **only** in how 2+ is answered (panic / `None` /
  `Err(count)`); there is deliberately no bare `sole_atom` to bind by accident.

### One handle per shape

Five document shapes (single / pairs / basket / multi / portfolio), two types
(`src/spec/runnable.rs`) where there used to be five of everything:
**`RunnableStrategy`**, the object-safe trait every `Dyn*Strategy` implements, and
**`StrategySpec`**, the sum over the five spec types with one `try_build` /
`try_build_priced` / `universe` / `kind`. **`RunnableStrategyExt`** is the
wallet-generic half, split out only because generic methods would cost the trait its
object safety — `drive_resumable_with` / `warm_up_over` run a spec against an account
you supply, and share one body with `drive`.

**There is no per-shape difference left in the driver.** A portfolio is an ordinary
strategy that trades the wallet it is handed, so no shape overrides `drive` /
`drive_resumable`, and anything shape-specific in a driver is a smell.
**Adding a sixth shape** = a `StrategySpec` variant + a `RunnableStrategy` impl + an arm
in `optimize::build_any_spec` and Python's `spec_from_value`. Not ten new functions.

One asymmetry to know about: basket and multi build per-symbol chains **lazily**, so
`stable_bars()` only reads true after a snapshot has gone through (hence
`needs_probe_feed` in the walk-forward probes), and the eager shapes must *not* be fed a
probe snapshot. **Restoring is the exception** — `restore_state` builds every symbol in
the blob up front, because fills route through `on_fill` *before* `update`. See
ARCHITECTURE *Run resuming*.

### Build errors are values

A spec that parses but can't be *built* — an unknown `!get` column, a malformed `!pick
{ freq }` (or one naming both `freq:` and `stream:`), a slot handed the wrong type,
`!portfolio_book` outside a portfolio, a `!value <list>` outside a weight template, a
**non-constant portfolio `weights:` with no `rebalance_on:`** (nothing would ever read
it — `!never` is the named opt-out) — is bad **input**, not a broken invariant. Report
it, never abort.

- **`NodeSpec::try_build`** (and each shape's `*Spec::try_build`) return `Result<_,
  String>`. `build` remains as an unwrapping shim; prefer `try_build` in new code.
  `runtime::try_chain` / `As::<Out>::try_new` are the fallible twins of `chain` /
  `As::new`; **`Adapter::update`'s type-mismatch panic stays**, unreachable once
  construction is checked.
- **The error carries a `!tag > ` breadcrumb** — each level prepends its own tag, so a
  failure four levels down arrives as `!and > !gt > !sma > !get > <message>`.
  **Messages must not repeat their own tag.**
- **A type mismatch is attributed to the child that produced the wrong type**, not the
  slot that rejected it.
- **Driver-level validation**: `spec::backtest::validated(|| spec.try_build(..))` builds
  once up front, so the run machinery never sees a bad spec.

**Where a panic legitimately remains:** the per-symbol factories in `BasketStrategy` /
`MultiAssetStrategy` build chains lazily inside `update`, with no error path to return
through. Each template is therefore **probed once at build time** against `PROBE_SYMBOL`
— one that builds for the probe builds for every symbol. **Add a per-symbol slot, add it
to the probe.**

**A template defers its value, not its shape.** One step earlier, `SpecTemplate`
typed-parses a copy of the deferred body at **load** with every `!arg` held as an
`undefined` hole, so a typo inside a basket's `score:` or a portfolio's `weights:` is an
ordinary parse error for every consumer of the loader. Preprocessing a tree first? Use
`SpecTemplate::checked`, **not** `from_tree`, which skips the probe.

### Safe defaults, opt-in overrides

Numbers during warm-up or IIR settling are *unsettled*. Every knob that could paper over an
unsettled bar biases toward **waiting**, with one named opt-out:

- **Strategy readiness.** `Strategy::is_ready()` gates `trade()`; `SingleAssetStrategy`
  overrides. Opt-out: `Unstable<S>` (`.unstable()` / `!unstable { source }` / Python
  `.unstable()`).
- **Position sizing.** `position_sizing(indicator)` (default `Value::new(1.0)`) scales
  `value_frac`; `None` from the sizing indicator *skips the whole `trade()` call*. Recipes
  in `fugazi::indicators::sizing` (see the table below). The three book-anchored tags take
  an optional `source:` resolving to the book they read — `!strategy_book` (default) or
  `!portfolio_book` (aggregate; only meaningful in a portfolio weight template).
- **`fugazi get` overlays.** CLI trims each column's pre-`stable_bars()` cells. Opt-out:
  `--keep-unstable`.
- **`-w/--windowed` duration form.** `-w 1d`/`-w 1w`/… demands explicit `AssetClass`
  (`--stocks`/`--forex`/`--crypto`) and resolvable bar cadence. Opt-out: plain bar-count
  `-w N`.
- **Explicit periods.** Windowed constructors take explicit `period` (`> 0`); risk-adjusted
  metrics take explicit rf-rate and bars-per-year.

Adding a knob that touches unsettled data: safest default, one opt-out.

### Conventions and gotchas

- Constructors `assert!(period > 0, ...)`; document warm-up; implement `warm_up_bars()` to
  match exactly (plus `unstable_bars()` when smoothing recursively). Add new indicators to
  `tests/warm_up.rs`.
- **Writing a new indicator? Read *Writing one that is fast without trying* in
  [CONTRIBUTING](docs/CONTRIBUTING.md) before the first line of `update`** — eight rules,
  each one a mistake that shipped and cost 25–60% of an indicator. The one to carry in
  your head: never allocate in `update`.
- Comparison/edge is **`None` until** every source is warmed; `And`/`Or` are `None` until
  both ready — so an edge coincident with warm-up isn't detected (no spurious first-bar
  trade).
- Marker leaves use `PhantomData<fn(I)>` / `fn() -> F` for constraint rules (avoids E0207);
  `Identity<I>` uses `PhantomData<fn(I) -> I>`.
- **Strategies are not rule engines.** Don't add `(signal, action)` tables — including
  wiring `set_limit` into the signal slots — without being asked.
- **Superseded, do not reintroduce:** the two-trait `CandleSource`/`OverlaySource` provider
  split; the N-per-child-`PaperWallet` portfolio; a portfolio owning its own `substrate`
  behind a `SubstrateFactory` + `PortfolioWallet` view. See ARCHITECTURE for why each died.
- The crate has **one** quantile convention (`stats::quantile_of_sorted`, R type-7). Don't
  add a second.
- **Parity discipline.** A Rust API added / extended / renamed is mirrored in
  `python/src/` **in the same PR**. Two tests catch the common cases; the `Wallet` trait
  and the per-tag ledger are hand-maintained (`python/tests/test_parity.py`). See
  ARCHITECTURE *Parity discipline*.
- **Touched the Python surface? Regenerate the stubs** — `python
  tools/gen_python_stubs.py`, then commit `python/fugazi/*.pyi`
  (`python/tests/test_stubs.py` regenerates and diffs). Two rules that bite silently:
  a new pyclass needs `module = "fugazi"` (without it it reports `builtins` and cannot
  be pickled), and a defaulted parameter that is *configuration* rather than a domain
  value goes after a `*`.

## Existing helpers — grep before writing new code

About to write a private helper whose name looks like something here? Grep first.
The rationale behind each lives in the item's own doc comment, or in
[ARCHITECTURE](docs/ARCHITECTURE.md); this table is the index.

### Indicators and cores

| Concern | Reuse | Location |
|---|---|---|
| Real recurrence for internal smoothing | `EmaState` / `WilderState` | `src/indicators/smoothing.rs` |
| Windowed sum/variance/stddev; rolling extremum | `WindowStats` / `WindowExtreme<Op>` — fixed rings, deliberately not on `Ring`. Want mean *and* dispersion? `mean_and_stddev` / `mean_and_variance`, not both calls. **Never reintroduce the `E[X²] − E[X]²` shortcut** | `src/indicators/stats.rs` |
| Any fixed-capacity window (push, evict oldest, iterate oldest-first) | `stats::Ring<T>` — restores via `stats::LoadWindow` (`#[state(window)]`). **Not a `VecDeque`** — see PERFORMANCE *Phase 13* | `src/indicators/stats.rs` |
| Every second-order reading of a paired window, from one scan | `WindowCovariance::moments()` → `Moments` — **ask the core once**, never two accessors | `src/indicators/stats.rs` |
| Rolling quantile / rank-in-window | `WindowQuantile` backing `Percentile` / `PercentileRank` | `src/indicators/{stats,percentile}.rs` |
| Pointwise transform of one source (`abs`/`sign`/`sqrt`/`tanh`/`sigmoid`) | `Unary<S, Op>` + `UnaryOp` — **no bespoke struct for a new one** | `src/indicators/ops.rs` |
| Unbounded running fold (`cum_sum`/`cum_max`/`cum_min`) | `Cumulative<S, Op>` + `CumulativeOp`, folding the **existing** `AddOp`/`MaxOp`/`MinOp` | `src/indicators/ops.rs` |
| Pairwise (two sources, one bar) extremum or power | `Max`/`Min`/`Pow` = `Combine<L, R, Op>`; `.clamp` is `Min` of `Max`. **Not** `RollingMax` | `src/indicators/ops.rs` |
| Rolling two-source statistic (correlation / covariance / beta) | `PairStat<L, R, Op>` + `PairStatOp` over `WindowCovariance`; aliases `Correlation` / `Covariance` / `Beta` | `src/indicators/pairwise.rs` |
| Rolling regression against time (slope / intercept / value / r²) | `LinReg<S>` + `component_accessors!` (`period >= 2`) | `src/indicators/linreg.rs` |
| Bars since an event | `BarsSince`; `BarsSinceHigh`/`BarsSinceLow` (O(1) via `WindowExtreme::since()`) | `src/indicators/bars_since.rs` |
| Three-source ternary | `IfElse::new(cond, t, f)` / `.if_else(t, f)` | `src/indicators/if_else.rs` |
| Multi-output accessor bodies | `component_accessors!` macro | `src/indicators/component.rs` |
| Sizing recipes | `indicators::sizing::{equal_weight, vol_target, atr_risk, drawdown_throttle, equity_vol_target, fractional_kelly}` (`*_of` variants take a caller-supplied atom source, for the basket per-leg case) | `src/indicators/sizing.rs` |
| Serialize one indicator's state | `#[derive(SaveState)]` + `#[state(source\|skip\|window\|config\|core)]` + two forwarding lines. **Config is checked, not replayed**; snapshot shared handles via `Position::snapshot` / `Book::snapshot_state` / `PaperWallet::snapshot_state` | `fugazi-derive/src/lib.rs`, `src/indicators/{position,book}.rs`, `src/wallet/paper.rs` |

### Strategies, wallet, portfolio

| Concern | Reuse | Location |
|---|---|---|
| Position / book tracking inside a strategy | `SingleAssetStrategy::{position, book}`; `BasketStrategy::{position(&sym), book}`; `PairsStrategy::book` + `BookField` accessors | `src/indicators/{position,book}.rs`, `src/strategies/` |
| Cross-sectional rank → `Side` | `strategies::selection::{Selection, TopBottom, Threshold, Quantile, Everything, DynSelection}` (`::new` roots on `Everything`, `::of` re-roots); `BasketStrategy::selection(impl)` takes any impl or closure | `src/strategies/selection.rs`, re-exported from `basket.rs` |
| Declared basket universe (strict vs. lax) | `strategies::universe::{Universe, Floating, AllOf, AnyOf}`; `BasketStrategy::{all_of, any_of, universe}`; YAML `universe: !all_of \| !any_of` | `src/strategies/universe.rs`, `src/spec/basket.rs` |
| Composite strategy over N heterogeneous children on one account | `Portfolio::builder().add(name, s).weights(policy).rebalance_on(sig).build()`, then `backtest::run` (any `Wallet`) or `portfolio.run(snaps)`. Reads: `sub_equity(i)` / `sub_position(i, sym)` / `assert_books_balance(&wallet)`. The account must be the portfolio's **alone** | `src/portfolio/{mod,netting}.rs` |
| Per-child notional book + the handle a child trades | `portfolio::ledger::{Ledger, LedgerWallet}`; `PortfolioInner::{net_and_submit, attribute_fill, book_crosses, book}` | `src/portfolio/{ledger,netting}.rs` |
| Portfolio weight policies / adaptive weighting | `portfolio::policy::{WeightPolicy, Fixed, EqualWeight, ChildSample}`; `PortfolioBuilder::weight_shares(...)` — YAML `weights:` is one `SpecTemplate<NodeSpec>` instantiated per child with `!arg SYM`/`CHILD_NAME`/`CHILD_INDEX` | `src/portfolio/{mod,policy}.rs`, `src/spec/portfolio.rs` |
| Portfolio two-phase rebalance, and the position-phase policy | `PortfolioBuilder::{rebalance_on, position_rebalancer}`; `rebalance::PositionRebalancer` with `Proportional` (default) / `LargestFirst` | `src/portfolio/{mod,netting,rebalance}.rs` |
| Aggregate portfolio `Book`; mark a `Book` from outside | `Portfolio::book()`; `Book::mark_equity(v)` (equity + peak + per-bar return only) | `src/portfolio/mod.rs`, `src/indicators/book.rs` |
| Close every open position **now**, through the cost pipeline | `Wallet::flatten` (`PaperWallet` overrides it synchronously — its queued moves would never settle) | `src/wallet/{mod,paper}.rs` |
| Ask an account what it is | `Wallet::{can_short, quote_ccy, data_sources, carry_coverage}` — all **inform, never enforce**, and a default answer means *"does not say"*. `RunReport::carry_coverage` carries the last out of a run. `SleeveWallet` delegates; `LedgerWallet` delegates only what can cross the portfolio mutex | `src/wallet/{mod,paper,sleeve}.rs`, `src/live/*.rs`, `src/portfolio/{mod,ledger,netting}.rs` |
| Clone a `TradingCosts` bundle | `TradingCosts::clone()` (every model impls `clone_box`) | `src/costs/mod.rs` |

### Running a spec

| Concern | Reuse | Location |
|---|---|---|
| Built-strategy readiness + whole-run report, any shape | `DynSingleStrategy::{stable_bars, warm_up_bars}`; `backtest::measured_report_any(&StrategySpec, &[Snapshot], &EvalContext)` | `src/spec/{strategy,backtest}.rs` |
| Full-run backtest → `Metrics`; slice a report | `backtest::{evaluate_any, evaluate_windowed_any, run_iteration_any}` + `metrics::report_slice`. **No per-shape twins** — one `_any` family covers all five | `src/spec/{backtest,metrics}.rs` |
| Resolved-once run inputs; per-symbol costs; report → metrics | `EvalContext` + `.costs_for_one(sym)` / `.costs_for(syms)` / `.reduce(&report)` / `.reduce_windowed(&report, n)` | `src/spec/backtest.rs` |
| Persist / resume a run's full state | `RunnableStrategy::{save_state, restore_state, drive_resumable}` + `RunState`; `backtest::{run_iteration_resumable, flatten_open_positions}`. See ARCHITECTURE *Run resuming* | `src/spec/{runnable,backtest}.rs`, `src/backtest.rs` |
| Run a spec against a **caller-supplied** wallet (primed paper, or a live venue) | `RunnableStrategyExt::drive_resumable_with`; shared body `runnable::drive_over`. Python: `StrategySpec.run` / `.run_resumable` via `over_any_wallet!` | `src/spec/runnable.rs`, `python/src/{strategy,spec}.rs` |
| Warm indicators over a pause gap without trading | `backtest::warm_up`; `RunnableStrategyExt::warm_up_over`; Python `StrategySpec.warm_up` → state JSON, no report | `src/backtest.rs`, `src/spec/runnable.rs` |
| Returns / trades / drawdown segments from a report | `metrics::{per_bar_returns, reconstruct_trades, drawdown_segments}` | `src/metrics.rs` |
| Resolve a metric name once, reuse | `MetricKey::from_name(name, sample)` + `.resolve(&metrics)` | `src/spec/metrics.rs` |
| Seeded resampling; MC CIs + empirical-null p-values | `montecarlo::{ResampleScheme, resample_indices, resample_slice, rng_from_seed, percentile, std_dev}`; `spec::montecarlo::{McConfig, run_montecarlo, McOutcome}` via `EvalContext::mc` | `src/montecarlo.rs`, `src/spec/montecarlo.rs` |

### Spec loading and the grammar

| Concern | Reuse | Location |
|---|---|---|
| Load whole strategy doc | `spec::load_document(text, &params, base, root, label, kind)` — `load_value` is the same pipeline without the `root::apply_default` splice; `*StrategySpec::from_text_with_params_in` | `src/spec/mod.rs` |
| Load `@file` or inline; YAML → JSON value | `input::Source::{File, Inline}` + `.read()`; `input::parse_value(text)` | `src/spec/input.rs` |
| Load-time `!param` / `!import` substitution | `params::substitute`; `imports::resolve(value, base, root)` — `base` is `input::Source::base_dir()`, the importing file's directory; `root` is the confinement boundary (`--import-root`) it was decoupled from. Partial pass: `params::substitute_partial` | `src/spec/{params,imports,input}.rs` |
| A `!param` / `!arg` body — its `key`, `default` and optional declared `type` | `params::placeholder_of(tag, body)` → `Placeholder`, `.apply(v)` to coerce; `param_type::{ParamType, parse_declaration}`. Both tags share the one parse, so the key set (**closed** — an unknown key is an error, or `typ:` would silently mean "untyped") and the four names (`string`/`numeric`/`integer`/`bool`) can't drift. No `type:`, or `type: null`, = the pre-existing heuristics, coercion skipped entirely. A declaration also checks the `default:`, and under `check` it is what an unset placeholder reports (`undefined::declare` → `HoleTypes.declared`) | `src/spec/{params,param_type,args}.rs` |
| Build-time `!arg` substitution; defer a subtree until args are ready | `args::substitute(value, &args)`; `SpecTemplate<T>` + `.build(&args)`. Preprocessed a tree first? `SpecTemplate::checked`, **not** `from_tree` (which skips the probe) | `src/spec/{args,template}.rs` |
| Build a spec, reporting a bad document instead of aborting | `NodeSpec::try_build` / each `*Spec::try_build` → `Err(String)` with the `!tag > ` breadcrumb; `spec::backtest::{build_error, validated}` | `src/spec/{expr,backtest}.rs` |
| Validate a document **nobody has bound `!param` values for** — shape only, holes typed from their slots | `spec::check::check_value` → `CheckedSpec { spec, holes, reads, built }`. The one copy behind `fugazi check strategy` *and* Python `ta.check_spec`; `spec` is **not runnable** (holes parse as typed zeros). The `check_mode` guard spans the **build**, not just the parse — a deferred template body re-parses there | `src/spec/check.rs` |
| Static type check of an expression tree (`check` only) | `typecheck::{output_type, check_immediate}` — a `None` output type means *skip*, never *invalid* | `src/spec/typecheck.rs` |
| What a tag requires a given slot to produce, without a tree | `typecheck::{slot_demand, slot_demands}`, surfaced as `GrammarField::node_output`. Backed by per-tag prototypes — **don't hand-write a second demand table** | `src/spec/{typecheck,grammar}.rs` |
| Every YAML spelling a tag accepts, not just the canonical one | `GrammarTag::forms` (canonical first) + `GrammarForm::{shape, fields, payload, scope}`. An alternate the variant can't express is **declared** via `#[grammar(alt = …)]`, never a second table | `src/spec/grammar.rs`, `fugazi-derive/src/grammar.rs` |
| A document's evaluation root, and what it names without building | `spec::root::RootSpec` — `node()` (build) · `tree()` (analyse) · `named_symbols()` / `sole_symbol(shape)` / `declared_freq()` / `as_pick()` · `for_symbol` / `for_series` | `src/spec/root.rs` |
| Blessed series of an overlay group / basket leg | `cli::overlay::group_root(symbol, interval)`; `spec::{basket,multi_asset}::leg_root(sym)` — all three return a `RootSpec` | `src/cli/overlay.rs`, `src/spec/{basket,multi_asset}.rs` |
| Which series a document **reads but does not trade** | `spec::reads::{picked_symbols, picked_symbols_of}` — a structural walk of the loaded document; joined in by `cli::run::{read_only_series, attach_read_series}` (**left** join), threaded as `RunOptions::reads` | `src/spec/reads.rs`, `src/cli/{run,optimize}.rs` |
| A document's free-form `meta:` | `spec::meta::Meta` on every document type; `StrategySpec::meta()` / `StrategyRef::meta()` / `CostConfig::meta()`. **Don't relax `deny_unknown_fields` instead** — the typo guard is the point | `src/spec/meta.rs` |
| Constant leaf: number or string | `!value 70` / `!value bull` | `src/spec/expr.rs` |
| Book field leaves, and which book they read | `!equity` / `!equity_peak` / `!drawdown` / `!return_per_bar` / `!trade_pnl` / `!trade_return`, each with optional `source:` — `!strategy_book` (default) or `!portfolio_book`; resolved by `resolve_book_source` | `src/spec/expr.rs` |
| Sugar tag rewrites (all lower to `!value` at load) | `rewrite_weights_sugar` (`!fixed` / `!equal_weight`); `rewrite_sugar_tags` (`!equal_weight <N>` → `!value <1/N>`); `rewrite_value_list_by_index` per child | `src/spec/{portfolio,expr}.rs` |
| Portfolio YAML surface | `PortfolioSpec` (`children`, `weights`, `rebalance_on`) + `PortfolioChildSpec` / `PortfolioChildStrategy`; runner `run::run_portfolio` | `src/spec/portfolio.rs`, `src/cli/{run,optimize,main}.rs` |
| Compute overlay columns from `name: NodeSpec` + attach | `spec::overlay::{OverlayColumn, columns_from_value, columns_from_yaml, prepare, prepare_for, prepare_built, compute_series, compute_snapshots}`; the fallible build is `build_overlay(spec, schema, root)`. Python `ta.compute_overlays`, CLI `-x` | `src/spec/overlay.rs`, `src/cli/overlay.rs`, `python/src/constructors.rs` |

### CLI, data and providers

| Concern | Reuse | Location |
|---|---|---|
| Interval token / `Frequency` / time-column ms; auto-detect cadence | `calendar::{parse_interval, parse_time_to_millis, detect_frequency_from_atoms}`; `Frequency::from_str`; `class.trading_seconds_per_bar(freq)` | `src/spec/calendar.rs` |
| Bracket-split `SYMBOL[FREQ]:` / full scope, and its `\:` escape | `calendar::{parse_scope_parts, parse_scope, unescape_symbol, escape_symbol, is_escaped, looks_like_body}` — only **scope** grammars need the escape; `get` spec heads take the symbol verbatim | `src/spec/calendar.rs`, `src/cli/overlay.rs`, `src/spec/costs/spec.rs` |
| Parse `-w` / `--walkforward` | `WindowSpec::from_str` + `.resolve(bar_freq, class)`; `WalkForwardSpec::from_str` + `.resolve(...)` | `src/spec/calendar.rs` |
| Which cadence a run targets, and whether the input agrees | `cli::cadence::{Census, Series, Finding, apply, warn}` → `Resolution`, called from `main::load_frame` right after `DataFrame::from_series`. **Ambiguity is refused, disagreement is warned.** Precedence: `-f` → the document's `root:` → the `freq` column → detection | `src/cli/cadence.rs` |
| Two cadences of one symbol in one `--series` frame | `DataFrame` keys rows by **`(symbol, freq, IndexKey)`** (`index` column ordering numerically, else `time` as an opaque label); `frequencies_of` / `cadence_groups` / `retain_cadence`. The `freq` cell is verbatim, never case-folded (`1M` vs `1m`) | `src/cli/data.rs` |
| Join an overlay CSV onto a price CSV | Two `get` → two `-s`; `DataFrame::insert` full-joins | `src/cli/data.rs` |
| How much of a multi-symbol universe ever shares a snapshot | `cli::overlap::{measure, measure_universe, warn_if_fragmented}` → `Overlap<K>` (`is_fragmented()` / `summary()`). Measures **observed co-occurrence**, never per-symbol stamp signatures. Joining on the trading *date* is rejected permanently — it manufactures cross-timezone lookahead (see TODO) | `src/cli/overlap.rs` |
| CSV delimiter probe | `csv_source::detect_delimiter(path)` | `src/cli/csv_source.rs` |
| Shell glob (case-insensitive, whole-string) | `glob::Pattern::from_str(pat)` + `.matches(text)` | `src/cli/glob.rs` |
| Fetch any series (candles *or* price-less) | `SeriesSource::atoms(...)` — `Binance`, `BinanceVision` (spot *and* USDⓈ-M futures), `Okx`, `Kraken`, `Coinbase`, `Yahoo`, `CoinGecko`; the CLI adds `file:`. `schema()` = the fixed overlay schema when known before the fetch | `src/sources/mod.rs`, `src/cli/csv_source.rs` |
| Provider schemas, and the CLI's one provider registry | `*::*_schema()` (`OnceLock`); `cli::get::KNOWN_PROVIDERS` | `src/sources/*.rs`, `src/cli/get.rs` |
| Shared overlay schema of an atom stream; bucket an irregular stream | `sources::schema_of(&atoms)`; `sources::floor_to_bucket(ms, interval)` — Monday weeks, 1st-of-month months, epoch modulo otherwise | `src/sources/mod.rs` |

### Type erasure and Python

| Concern | Reuse | Location |
|---|---|---|
| Erase an indicator, **keeping its domain in the type** | `runtime::{erase, Chain<In, Out>, DynIndicator, any, AnyChain}` + the `RealChain`/`BoolChain`/… aliases. A `Chain` *is* an `Indicator`. **Prefer this** — +2.5 ns/sample per level against the payload vocabulary's +13.7 (PERFORMANCE *Phase 6*) | `src/runtime/chain.rs` |
| Erase an indicator so it **describes its own** types at run time | `runtime::{wrap, wrap_sync, unstable_wrap, PayloadIndicator, PayloadValue, AsReal/AsBool/AsCandle/AsAtom/AsStr, chain, try_chain}` — being retired; the spec layer still uses it. Only for a heterogeneous collection differing in *input* domain | `src/runtime/mod.rs` |
| Python: read an overlay column, optionally from another series | `get(schema, key, source=None)` / `get_real` / `get_bool` / `get_str` — `source=pick(sym)` re-roots | `python/src/constructors.rs` |
| Python: domain-preserving wrap / combine / bool build | `map_source!`, `combine_sources!` / `sources_to_signal!` / `combine_signals!` / `combine_multi!`, `source_to_signal!` (the per-shape `src_period!` / `bar_period!` builders are in `constructors.rs`) | `python/src/macros.rs` |
| Python: register a metric on `fugazi.metrics` | Add to `reg!(...)` in `register_metrics_module` | `python/src/metrics.rs` |

### Tests

| Concern | Reuse | Location |
|---|---|---|
| Integration-test harness (bars, snapshot streams, temp paths, running the binary, a `wiremock` server, the live-venue conformance suite) | `mod common;` + `common::{bars,cli,net,fixtures,live}` — each `tests/*.rs` is its own crate, so this is *included*, not imported. See [docs/TESTING.md](docs/TESTING.md) | `tests/common/` |
