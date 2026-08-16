# CLAUDE.md

Guidance for Claude Code in this repo. This file is the **invariants, conventions,
and quick-reference** — the load-bearing summary read every session. The depth
lives elsewhere; reach for it on demand:

- **[doc/ARCHITECTURE.md](doc/ARCHITECTURE.md)** — the full subsystem internals
  (indicator taxonomy, every strategy shape, wallet, run resuming, Monte Carlo,
  the spec/optimize kernel, Python parity). When a section below says "see
  ARCHITECTURE", that's where the detail moved.
- **[doc/CONTRIBUTING.md](doc/CONTRIBUTING.md)** — the *procedure*. Adding an
  indicator / signal / operator / metric / provider? It lists every place each
  change has to touch, in order, and which are compiler- or test-enforced.
- **[doc/TESTING.md](doc/TESTING.md)** — the test suite's *map*: the four layers
  and what each is for, where a given change's test goes, the shared
  `tests/common/` harness, how the drift guards are built, and the
  skip-vs-fail fixture policy (`FUGAZI_REQUIRE_FIXTURES=1`). Read it before
  adding a test file or a test helper.
- **[doc/STRATEGIES.md](doc/STRATEGIES.md)** (YAML spec) · **[doc/CLI.md](doc/CLI.md)**
  · **[doc/COSTS.md](doc/COSTS.md)** · **[doc/METRICS.md](doc/METRICS.md)** ·
  **[doc/PYTHON.md](doc/PYTHON.md)** — user-facing surface docs.
- **[TODO.md](TODO.md)** — a *decision log*, not a backlog. An entry records a
  judgment already made and what would change it. Don't burn it down; do read it
  before re-litigating something it already settled.

## What this is

`fugazi` is a Rust library (edition 2024) of **incremental** technical-analysis
primitives. Every primitive owns its state and advances one sample at a time via
`update()` in ~O(1) — same code for live streaming and batch backtesting.

Three composable layers: **indicators** (numeric sources), **signals**
(`Indicator<Output = bool>`), **strategies** (decision layer trading into a wallet).
See [doc/ARCHITECTURE.md](doc/ARCHITECTURE.md) for each.

**Dependencies.** Unconditional: `serde`+`serde_json` (with the **`float_roundtrip`**
feature — load-bearing for run resuming; without it a restored f64 seed drifts 1 ULP
and the resumed equity curve diverges), `time`, `statrs` (Φ/Φ⁻¹ for PSR/DSR), and the
internal **`fugazi-derive`** proc-macro crate (`#[derive(SaveState)]`). Default-on
features: **`sources`** (remote providers), **`runtime`** (type-erasure vocabulary in
`fugazi::runtime`), **`cli`** (binary; implies both, plus `montecarlo`). Off by default:
**`montecarlo`** (the resampling significance layer — the crate's only source of
randomness, so it gates `rand` + `rand_chacha`; `cli` turns it on so the `run
--montecarlo` flag always exists, runtime-gated). New unconditional deps are judgment
calls — reach for closed-form first.

## Commands

- Build: `cargo build`; Test: `cargo test`; Lint: `cargo clippy --all-targets` (keep
  clean); Docs: `cargo doc --open`
- `FUGAZI_REQUIRE_FIXTURES=1 cargo test` — makes the two cross-validation suites
  (`talib_validation`, `metrics_validation`) **fail** instead of skipping when their
  generated fixture is missing or stale. Both fixtures are committed under
  `tests/data/` (`.gitignore` carries an explicit note not to re-ignore
  `talib_expected.csv`), and CI's Rust job sets this — so a stale fixture fails
  rather than silently comparing nothing. See [doc/TESTING.md](doc/TESTING.md).

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

Then **`cargo check --workspace`** (updates `Cargo.lock`), commit the manifests + README
+ Lock, tag `vX.Y.Z`, push. Then **publish a GitHub Release** for that tag (`gh release
create vX.Y.Z --generate-notes`) — the release-publish event is what triggers the
publish workflow; pushing the tag alone does not. `python/README.md` has no version
string. The `fugazi-derive` version and the root's dependency pin on it must match, or
`cargo` errors.

**Not `cargo build --workspace`** — it links the pyo3 cdylib, which needs a Python
interpreter and fails locally with a wall of `ld:` output that looks like a broken
release but isn't (`maturin develop` links it properly). `check` refreshes the lock just
the same and type-checks *both* crates, so it verifies strictly more of what a bump can
break.

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

A `*Op` type impl'ing the relevant trait (`BinaryOp`/`LookbackOp`/`ExtremeOp`) plus a
type alias — **never a macro**. Arithmetic/boolean/lookback are zero-sized `Default`
markers; comparisons carry a `Tolerance { abs, rel }` (band = `max(abs, rel·max|operand|)`, default `(1e-12, 1e-9)` — **relative, because operand scale is unbounded**; the execution-side quantity epsilons live in `src/wallet.rs`). `Combine` feeds the *same* input to both sides
(requires `Input: Clone`; use `lhs`/`rhs` naming), holds op by value; `Lookback`/`Extreme`
hold a zero-sized op as `PhantomData<fn() -> Op>`. `Change` is a **bidirectional** toggle
detector; directional events come from pairing it with a comparison.

### Blessed series — what a `source:`-omitted leaf reads

Two stacked defaults on any atom leaf; don't conflate them.

1. **Per-tag** (`src/spec/expr.rs`, `default_source` / `default_bar_source` /
   `default_high` / `default_low`): which *sub-expression* a wrapper defaults to —
   `!ema`/`!sma`/`!rsi` → `!close`, `!atr`/`!obv`/`!adx` → `!current`, `!donchian` →
   `!high`/`!low`.
2. **Blessed series**: once you bottom out at a leaf (`!close`, `!high`, `!current`,
   `!get`, …), which *asset* it projects out of the bar's `Snapshot`.

The blessed series is an explicit `root: Option<&Selector<String>>` **parameter** on
`NodeSpec::try_build`, consumed by `pick_root` / `build_pick`. `Some(sel)` →
`Pick::rooted(sel)`, `None` → `Pick::new()` (sole-atom, panics on 2+). The ~142 match
arms never mention it — they fan out through the `atom_src` / `atom_src_any` closures.

**Who blesses what:**

| Context | root |
|---|---|
| `SingleStrategySpec::build` | `Some(by_symbol(self.symbol))` |
| `BasketStrategySpec` / `MultiAssetStrategySpec` per-leg factories | `Some(by_symbol(sym))` via `leg_root` |
| overlay column, per `(symbol, freq)` series | `Some(group key)` via `cli::overlay::group_root` |
| `PortfolioSpec` `weights:`, single-asset child | `Some(by_symbol(child.symbol))` |
| `PairsStrategySpec` | `None` — two legs, neither privileged |
| portfolio/basket/multi `rebalance_on:`, portfolio `weights:` on non-single children | `None` — gate spans everything |
| `!sharpe` & co.'s `strategy:` subtree | `None` — the embedded strategy blesses itself |

Consequences: **`!arg SYM` is optional, not required** in basket/multi templates (`score:
!rsi { period: 14 }` and the fully-spelled `!pick { symbol: !arg SYM }` build the same
chain; the explicit form is the only way to read a *different* symbol per leg).
**`pick_any_root` ignores the root** (calendar leaves read only `atom.time`, shared by
every entry). **`Pick::rooted` falls back through `lone_atom`, not `sole_atom`** — in a
rooted context a 2+ snapshot is ordinary (the blessed leg is absent this bar), so it reads
`None`; `sole_atom` there would panic on every basket with a listing gap.

### One handle per shape

Five document shapes (single / pairs / basket / multi / portfolio). Two types collapse
what used to be five-of-everything (`src/spec/runnable.rs`):

- **`RunnableStrategy`** — object-safe trait over every built strategy: `Strategy<Input =
  Snapshot<String>, Symbol = String>` plus `stable_bars()` / `warm_up_bars()` /
  `drive()`. Every `Dyn*Strategy` implements it.
- **`StrategySpec`** — the sum over the five spec types, with one `try_build` /
  `try_build_priced` / `universe` / `kind`.

**The one genuine per-shape difference lives behind those, not at call sites**: `drive`'s
default body is the `PaperWallet` path and `DynPortfolio` overrides it. Anything
else that looks shape-specific in a driver is a smell. (`try_build_priced` is *not* a
second one — costs ride on the wallet now, so three of its five params are `_`-prefixed
and its body is `self.try_build(cash, schema, None)` for all five shapes.) **Adding a sixth shape** = a
`StrategySpec` variant + a `RunnableStrategy` impl + an arm in `optimize::build_any_spec`
and Python's `spec_from_value`. Not ten new functions.

One asymmetry: basket and multi build per-symbol chains **lazily**, so `stable_bars()`
only reads true after one snapshot has gone through — hence the `needs_probe_feed` flag in
the walk-forward probes. The eager shapes must *not* be fed a probe snapshot (a pairs leaf
that didn't name its asset would trip the sole-atom guard).

### Build errors are values

A spec that parses but can't be *built* — an unknown `!get` column, a malformed `!pick {
freq }`, a slot handed the wrong type, `!portfolio_book` outside a portfolio, a `!value
<list>` outside a weight template — is bad **input**, not a broken invariant. Report it,
never abort.

- **`NodeSpec::try_build`** (and each shape's `*Spec::try_build`) return `Result<_,
  String>`. `build` remains as an unwrapping shim; prefer `try_build` in new code.
- **The error carries a `!tag > ` breadcrumb** — each recursion level prepends its own
  tag, so a failure four levels down arrives as `!and > !gt > !sma > !get > <message>`
  and `diagnostics::split_trail` renders the path on its own `at:` line. **Messages must
  not repeat their own tag.**
- **A type mismatch is attributed to the child that produced the wrong type**, not the
  slot that rejected it (`AsReal::try_new` errors are wrapped with the *child's* tag).
- `runtime::try_chain` / `As::<Out>::try_new` are the fallible twins of `chain` /
  `As::new`. **`Adapter::update`'s type-mismatch panic stays** — unreachable once
  construction is checked.

**Where a panic legitimately remains:** the per-symbol factories in `BasketStrategy` /
`MultiAssetStrategy` build chains lazily inside `update` — no error path to return through.
Each template is therefore **probed once at build time** against `PROBE_SYMBOL`
(`spec::basket::probe_template`, `spec::multi_asset::probe_signal`/`probe_expr`). A
template that builds for the probe builds for every symbol. **If you add a per-symbol slot,
add it to the probe.**

**Driver-level validation.** `spec::backtest::validated(|| spec.try_build(..))` builds once
up front so the run machinery (which still goes through the infallible shim) never sees a
bad spec. The CLI runners and every optimize row call it; `build_error(e)` is the `anyhow`
adapter.

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
- **Parity discipline.** When a Rust API is added/extended/renamed, mirror it in
  `python/src/` **in the same PR** (`lib.rs` is just module wiring — the code is in
  `constructors.rs` / `classes.rs` / `strategy.rs` / `metrics.rs` / `spec.rs` /
  `sources.rs` / `macros.rs`). Two tests catch the common cases; the `Wallet`
  trait and the per-tag ledger are hand-maintained (`python/tests/test_parity.py`). See the
  Python section of ARCHITECTURE.

## Existing helpers — grep before writing new code

If you're about to write a private helper whose name looks like something here, grep first.

| Concern | Reuse | Location |
|---|---|---|
| Integration-test harness (bars, snapshot streams, temp paths, running the binary, a `wiremock` server) | `mod common;` + `common::{bars,cli,net,fixtures}` — each `tests/*.rs` is its own crate, so this is included, not imported. See [doc/TESTING.md](doc/TESTING.md) | `tests/common/` |
| Bracket-split `SYMBOL[FREQ]:` / full scope | `calendar::parse_scope_parts(text)` / `parse_scope(text)` | `src/spec/calendar.rs` |
| Interval token / Frequency / time-column ms | `calendar::parse_interval` / `Frequency::from_str` / `parse_time_to_millis` | `src/spec/calendar.rs` |
| Auto-detect bar cadence | `calendar::detect_frequency_from_atoms(...)` | `src/spec/calendar.rs` |
| Parse `-w` / `--walkforward` | `WindowSpec::from_str` + `.resolve(bar_freq, class)`; `WalkForwardSpec::from_str` + `.resolve(...)` | `src/spec/calendar.rs` |
| Built-strategy readiness + full `RunReport` | `DynSingleStrategy::{stable_bars, warm_up_bars}`; `backtest::measured_report_any(&StrategySpec, &[Snapshot], &EvalContext)` | `src/spec/strategy.rs`, `src/spec/backtest.rs` |
| Persist / resume a run's full state | `RunnableStrategy::{save_state, restore_state, drive_resumable}` + `RunState`; `backtest::run_iteration_resumable`; `backtest::flatten_open_positions` (`--flatten`). See ARCHITECTURE *Run resuming* | `src/spec/runnable.rs`, `src/spec/backtest.rs`, `src/backtest.rs` |
| Serialize one indicator's state | `#[derive(SaveState)]` + `#[state(source)]`/`#[state(skip)]` + two `impl Indicator` forwarding lines; snapshot shared handles via `Position::snapshot`/`Book::snapshot_state`/`PaperWallet::snapshot_state` | `fugazi-derive/src/lib.rs`, `src/indicators/{position,book}.rs`, `src/wallet.rs` |
| Trading seconds a bar of `freq` spans | `class.trading_seconds_per_bar(freq)` | `src/spec/calendar.rs` |
| Shared overlay schema of atom stream | `fugazi::sources::schema_of(&atoms)` | `src/sources/mod.rs` |
| Fetch any series (candles *or* price-less) | `SeriesSource::atoms(...)` — `Binance`, `Okx`, `Coinbase`, `Yahoo`, `CoinGecko`, `BinanceVision`. `schema()` = fixed overlay schema when known before the fetch (`Coinbase` has none — OHLCV only) | `src/sources/mod.rs` |
| Provider schemas | `*::*_schema()` (`OnceLock`) | `src/sources/{binance,binance_vision,yahoo,coingecko,okx}.rs` |
| Bucket an irregular sample stream onto a cadence | `sources::floor_to_bucket(ms, interval)` — Monday weeks, 1st-of-month months, epoch modulo otherwise | `src/sources/mod.rs` |
| Join overlay CSV onto price CSV | Two `get` → two `-s`; `DataFrame::insert` full-joins | `src/cli/data.rs` |
| Compute overlay columns from `name: NodeSpec` + attach | `spec::overlay::{OverlayColumn, columns_from_value, columns_from_yaml, prepare, prepare_for, prepare_built, compute_series, compute_snapshots}` (Python: `ta.compute_overlays`; CLI `-x` via `build_overlay`). `compute_snapshots` is the multi-symbol path | `src/spec/overlay.rs`, `python/src/constructors.rs`, `src/cli/overlay.rs` |
| Blessed series of an overlay group / basket leg | `cli::overlay::group_root(symbol, interval)`; `spec::basket::leg_root(sym)` / `spec::multi_asset::leg_root(sym)` | `src/cli/overlay.rs`, `src/spec/{basket,multi_asset}.rs` |
| Build a spec, reporting a bad document instead of aborting | `NodeSpec::try_build` and each shape's `*Spec::try_build` — `Err(String)` with the `!tag > ` breadcrumb. `spec::backtest::build_error(e)` renders as `anyhow`; `spec::backtest::validated(...)` is the discard-value form | `src/spec/expr.rs`, `src/spec/backtest.rs` |
| Overlay build that errors instead of aborting | `spec::overlay::build_overlay(spec, schema, root) -> Result<..>` | `src/spec/overlay.rs` |
| CSV delimiter probe | `csv_source::detect_delimiter(path)` | `src/cli/csv_source.rs` |
| Shell glob (case-insensitive, whole-string) | `glob::Pattern::from_str(pat)` + `.matches(text)` | `src/cli/glob.rs` |
| Scope symbol `\:` escape (`BTC/USDT:USDT` vs. the `SYMBOL[FREQ]:` prefix) | `calendar::{unescape_symbol, escape_symbol, is_escaped, looks_like_body}` — only **scope** grammars need it; `get` spec heads take the symbol verbatim | `src/spec/calendar.rs`, `src/cli/overlay.rs`, `src/spec/costs/spec.rs` |
| Load `@file` or inline; YAML → JSON value | `input::Source::{File, Inline}` + `.read()`; `input::parse_value(text)` | `src/spec/input.rs` |
| Load whole strategy doc | `spec::load_value(text, &params, base)`; `*StrategySpec::from_text_with_params_in` | `src/spec/mod.rs` |
| Load-time `!param` / `!import` substitution | `params::substitute` / `imports::resolve(value, base)` | `src/cli/{params,imports}.rs` |
| Dir relative `!import` resolves against | `input::Source::base_dir()` | `src/spec/input.rs` |
| Build-time `!arg` substitution | `args::substitute(value, &args)` | `src/spec/args.rs` |
| Defer spec subtree until args ready | `SpecTemplate<T>` + `.build(&args)` (typed-parses a copy with args held undefined in check mode) | `src/spec/template.rs`, `src/spec/args.rs` |
| Static type check of an expression tree (`check` only) | `typecheck::{output_type, check_immediate}` — `None` output type means *skip*, never *invalid* | `src/spec/typecheck.rs` |
| Constant leaf: number or string | `!value 70` / `!value bull` | `src/spec/expr.rs` |
| Three-source ternary | `IfElse::new(cond, t, f)` / `.if_else(t, f)` | `src/indicators/if_else.rs` |
| Multi-output accessor bodies | `component_accessors!` macro | `src/indicators/component.rs` |
| Real recurrence for internal smoothing | `EmaState` / `WilderState` | `src/indicators/smoothing.rs` |
| Windowed sum/variance/stddev; rolling extremum | `WindowStats` / `WindowExtreme<Op>`. **Dispersion reads scan the window** (O(period)); the `E[X²] − E[X]²` shortcut cancels away `(mean/σ)²` digits and was wrong at crypto price scale — don't reintroduce it | `src/indicators/stats.rs` |
| Rolling quantile / rank-in-window | `WindowQuantile` backing `Percentile` / `PercentileRank` | `src/indicators/stats.rs`, `src/indicators/percentile.rs` |
| The crate's **one** quantile convention (R type-7) | `stats::quantile_of_sorted(sorted, p)` — don't add a second | `src/indicators/stats.rs` |
| Bars since an event | `BarsSince` (bool source), `BarsSinceHigh`/`BarsSinceLow` (O(1) over `WindowExtreme::since()`) | `src/indicators/bars_since.rs` |
| Position tracking inside strategy | `SingleAssetStrategy::position()`; `BasketStrategy::position(&sym)` | `src/indicators/position.rs`, `src/strategies/*.rs` |
| Sizing recipes | `indicators::sizing::{equal_weight, vol_target, vol_target_of, atr_risk, atr_risk_of, drawdown_throttle, equity_vol_target, fractional_kelly}` (`*_of` variants take a caller-supplied atom source for the basket per-leg case) | `src/indicators/sizing.rs` |
| Cross-sectional rank → `Side` | Trait `strategies::basket::Selection<Sym>`; composable built-ins `TopBottom<S>`/`Threshold<S>`/`Quantile<S>` (`::new` roots on `Everything`, `::of(inner, ...)` re-roots), `DynSelection` erases an inner; free functions `top_bottom`/`threshold`/`quantile`; `BasketStrategy::selection(impl)` installs any impl or closure | `src/strategies/basket.rs` |
| Declared basket universe (strict vs. lax) | `BasketStrategy::{all_of, any_of, universe}`; trait `strategies::basket::Universe` with impls `Floating`/`AllOf<Sym>`/`AnyOf<Sym>`; YAML `universe: !all_of [...] \| !any_of [...]` | `src/strategies/basket.rs`, `src/spec/basket.rs` |
| Strategy-lifetime equity/trade tracking | `SingleAssetStrategy::book()`/`PairsStrategy::book()`/`BasketStrategy::book()` + `BookField` accessors | `src/indicators/book.rs`, `src/strategies/*.rs` |
| Composite Strategy over N heterogeneous children netted onto one account | `Portfolio::builder().add(name, strategy).weights(policy).rebalance_on(signal).build()`, then `backtest::run(&mut portfolio, &mut wallet, snapshots)` (any `Wallet`), or `portfolio.run(snapshots)`. Per-child reads: `sub_equity(i)` / `sub_position(i, sym)` / `assert_books_balance(&wallet)` | `src/portfolio/mod.rs`, `src/portfolio/netting.rs` |
| Portfolio YAML surface | `PortfolioSpec` (`children`, `weights: Option<SpecTemplate<NodeSpec>>`, `rebalance_on`) + `PortfolioChildSpec` + `PortfolioChildStrategy`; `portfolio:` prefix; driven through `backtest::{measured_report_any, evaluate_any, evaluate_windowed_any, run_iteration_any}`; runner `run::run_portfolio` | `src/spec/portfolio.rs`, `src/cli/{run,optimize,main}.rs` |
| The account a portfolio trades (paper or live) | the wallet passed to `backtest::run(&mut portfolio, &mut wallet, snaps)` — any `Wallet<Sym>`. Must be the portfolio's **alone** | `src/portfolio/mod.rs` |
| Per-child notional book + the handle a child trades | `portfolio::ledger::{Ledger, LedgerWallet}`; netting/attribution in `portfolio::netting::PortfolioInner::{net_and_submit, attribute_fill, book_crosses, book}`; `Portfolio::assert_books_balance(&wallet)` | `src/portfolio/ledger.rs`, `src/portfolio/netting.rs` |
| Portfolio weight policies | `portfolio::policy::{WeightPolicy, Fixed, EqualWeight, ChildSample}` | `src/portfolio/policy.rs` |
| Portfolio adaptive weighting (per-child indicator) | `PortfolioBuilder::weight_shares(Vec<Box<dyn Indicator<Input=Snapshot<Sym>, Output=Real>>>)`; YAML `weights:` is a bare `SpecTemplate<NodeSpec>` instantiated per-child with `!arg SYM`/`!arg CHILD_NAME`/`!arg CHILD_INDEX`. Book source explicit on each node | `src/portfolio/mod.rs`, `src/spec/portfolio.rs` |
| Sugar tag rewrites (all lower to `!value` at load) | portfolio `weights:` — `rewrite_weights_sugar` (`!fixed [...]` → `!value [...]`, `!equal_weight` → `!value 1.0`); sizing `!equal_weight <N>` — `rewrite_sugar_tags` → `!value <1/N>` | `src/spec/portfolio.rs`, `src/spec/expr.rs` |
| Per-child indexing of a list literal in weight expressions | `NodeSpec::Value(ValueLit::List(Vec<Real>))`; `PortfolioSpec::build` runs `rewrite_value_list_by_index` per child | `src/spec/expr.rs`, `src/spec/portfolio.rs` |
| Aggregate portfolio Book | `Portfolio::book()` (marked via `Book::mark_equity` from `Σ sub.equity()` each `update`; passed as the `portfolio_book` build arg) | `src/portfolio/mod.rs`, `src/indicators/book.rs`, `src/spec/portfolio.rs` |
| Select which book a book-reading node reads | `source:` field — `!strategy_book` (default) or `!portfolio_book` (aggregate; build error elsewhere). Both build-time source-selectors; resolution via `resolve_book_source` inside `NodeSpec::build` | `src/spec/expr.rs` |
| Book field leaves (composable) | `!equity`, `!equity_peak`, `!drawdown`, `!return_per_bar`, `!trade_pnl`, `!trade_return` — each takes optional `source:` (default `!strategy_book`) | `src/spec/expr.rs` |
| Externally-mark a `Book`'s equity | `Book::mark_equity(value)` — updates equity + peak + per-bar return; leaves cash/legs/trade tracking untouched | `src/indicators/book.rs` |
| Portfolio two-phase rebalance | `Portfolio::builder().rebalance_on(signal)` — cash phase (`PortfolioInner::rebalance_ledgers_to`) then position phase (`LedgerWallet::set_position`) | `src/portfolio/mod.rs::rebalance_now`, `src/portfolio/netting.rs` |
| Pluggable position-phase policy | `portfolio::rebalance::PositionRebalancer<Sym>` trait; built-ins `Proportional` (default) / `LargestFirst`; install via `PortfolioBuilder::position_rebalancer(...)` | `src/portfolio/rebalance.rs`, `src/portfolio/mod.rs` |
| Ask whether an account can hold a short | `Wallet::can_short()` — default `true`; `false` on spot (`CoinbaseWallet`); wrappers delegate (`SleeveWallet` → inner, `LedgerWallet` → the account, cached in `PortfolioInner::account_can_short`). Informs, never enforces | `src/wallet.rs`, `src/live/coinbase.rs`, `src/portfolio/{mod,ledger,netting}.rs` |
| Clone a `TradingCosts` bundle | `TradingCosts::clone()` (every model impls `clone_box`) | `src/costs/mod.rs` |
| Partial `!param` pass | `params::substitute_partial(value, &table)` — used by `imports::resolve` for `!import`'s inline `params:` | `src/spec/params.rs` |
| Resolve metric name once, reuse | `MetricKey::from_name(name, sample)` + `.resolve(&metrics)` | `src/spec/metrics.rs` |
| Wrap indicator as `DynIndicator` / zero unstable / typed view / chain | `runtime::{wrap, unstable_wrap, AsReal/AsBool/AsCandle/AsAtom/AsStr, chain}` | `src/runtime.rs` |
| Full-run backtest → `Metrics`; slice a report | `backtest::{evaluate_any, evaluate_windowed_any, run_iteration_any}`, all taking `&EvalContext`; `metrics::report_slice`. **There are no per-shape twins** — one `_any` family covers all five | `src/spec/backtest.rs`, `src/spec/metrics.rs` |
| Resolved-once run inputs; per-symbol cost bundles; report → metrics | **`EvalContext`** + `.costs_for_one(sym)` / `.costs_for(syms)` / `.reduce(&report)` / `.reduce_windowed(&report, n)` | `src/spec/backtest.rs` |
| Whole-run report for any spec shape | `backtest::measured_report_any(&StrategySpec, ..)` — `evaluate_any` / `evaluate_windowed_any` are thin `ctx.reduce(...)` wrappers | `src/spec/backtest.rs` |
| Returns / trades / drawdown segments from a report | `metrics::{per_bar_returns, reconstruct_trades, drawdown_segments}` | `src/metrics.rs` |
| Seeded resampling (IID / moving-block / stationary bootstrap) | `montecarlo::{ResampleScheme, resample_indices, resample_slice, rng_from_seed, percentile, std_dev}` — pure, `rand`-only, behind `montecarlo` | `src/montecarlo.rs` |
| Monte Carlo CIs + empirical-null p-values over a run | `spec::montecarlo::{McConfig, run_montecarlo, McOutcome}`; runs in the backtest layer via `EvalContext::mc` → `attach_montecarlo`; CLI `run::emit_montecarlo` is IO-only | `src/spec/montecarlo.rs`, `src/spec/backtest.rs`, `src/cli/run.rs` |
| Python: read an overlay column, optionally from another series | `get(schema, key, source=None)` / `get_real` / `get_bool` / `get_str` — `source=pick(sym)` re-roots | `python/src/constructors.rs` |
| Python: domain-preserving wrap / combine / bool build | `map_source!`, `combine_sources!`/`sources_to_signal!`/`combine_signals!`/`combine_multi!`, `source_to_signal!` | `python/src/macros.rs` (the per-shape `src_period!`/`bar_period!` builders are in `constructors.rs`) |
| Python: register metric on `fugazi.metrics` | Add to `reg!(...)` in `register_metrics_module` | `python/src/metrics.rs` |
