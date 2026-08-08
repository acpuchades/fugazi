# Contributing

Recipes for the changes this repo sees most often, written so either a person or
an agent can follow them without reverse-engineering the pattern first.

`fugazi` has one set of primitives behind **four surfaces**: the Rust library, the
YAML spec layer, the CLI, and the Python bindings. Almost every "add a thing"
task is really "add it to the library, then thread it through the other three".
Most of the cost of a change is remembering the third and fourth.

- [Before you start](#before-you-start)
- [Add an indicator](#add-an-indicator)
- [Add a signal](#add-a-signal)
- [Add an operator](#add-an-operator)
- [Add a strategy shape](#add-a-strategy-shape)
- [Add a metric](#add-a-metric)
- [Add a remote provider](#add-a-remote-provider)
- [Add a sizing recipe](#add-a-sizing-recipe)
- [Work in progress](#work-in-progress)
- [Release a version](#release-a-version)
- [The drift guards](#the-drift-guards)
- [Conventions that bite](#conventions-that-bite)

---

## Before you start

```sh
cargo build                                        # library + CLI
cargo test -p fugazi                               # 960+ tests, incl. doctests
cargo clippy -p fugazi --all-targets -- -D warnings # CI gate; keep it clean

cd python && maturin develop && pytest             # bindings
```

Two repo-specific rules:

- **Do not run `cargo fmt` across the repo.** The tree is not rustfmt-clean
  (~95 files differ) and CI does not check formatting. A blanket reformat buries
  your change in noise. Match the surrounding style by hand.
- **Grep before writing a helper.** `CLAUDE.md` carries a table of existing
  helpers ("Existing helpers — grep before writing new code"). If the private
  function you're about to write has a name resembling a row in that table, it
  already exists. This repo has been bitten by it: six `evaluate*` bodies
  independently inlined a block byte-identical to `schema_from_snapshots`, forty
  lines above them.

---

## Add an indicator

The worked example is a rolling `Real → Real` indicator, the most common shape.
Seven places, in dependency order. Three are enforced (you cannot get them wrong
silently); four are not, so do them deliberately.

### 1. The indicator itself — `src/indicators/<name>.rs`

Own your source, generic over it. Composition in this crate *is* construction:

```rust
pub struct Foo<S> { source: S, /* state */, pub value: Option<Real> }

impl<S> Foo<S> {
    pub fn new(source: S, period: usize) -> Self {
        assert!(period > 0, "Foo period must be > 0");
        // ...
    }
}

impl<S: Indicator<Output = Real>> Indicator for Foo<S> {
    type Input = S::Input;
    type Output = Real;
    fn update(&mut self, input: Self::Input) -> Option<Real> { /* ... */ }
    fn value(&self) -> Option<Real> { self.value }
    fn warm_up_period(&self) -> usize { /* EXACT — see below */ }
    fn unstable_period(&self) -> usize { /* 0 unless you carry a seed forward */ }
    fn reset(&mut self) { /* ... */ }
}
```

Reuse the shared cores rather than another indicator's public type:
`WindowStats` (sum / variance / stddev), `WindowExtreme` (monotonic deque),
`WindowQuantile` (rolling order statistics), `EmaState` / `WilderState`
(recursive smoothing). They live in `src/indicators/{stats,smoothing}.rs` and
have no `Indicator` impl on purpose.

**`warm_up_period()` must be exact**: `update` returns `None` for the first
`warm_up_period() - 1` samples and `Some` from sample `warm_up_period()` onward.
Not an estimate — `tests/warm_up.rs` asserts it sample by sample.

### 2. Re-export — `src/indicators/mod.rs`

`mod foo;` plus `pub use foo::Foo;`.

### 3. The YAML tag — `src/spec/expr.rs`

Add a variant to `ExprSpec` **and** to the private `ExprSpecRaw` mirror, then a
`try_build` arm. The recursive-build shorthands are already in scope:

```rust
Foo { source, period } => dyn_indicator::wrap(
    crate::indicators::Foo::new(real(source)?, *period),
),
```

- `real(s)?` / `candle(s)?` build a child and view it as `Real` / `Candle`.
- `atom_src(source.as_ref())?` is the `Pick`-shaped `source:` slot every
  atom-input leaf takes.
- Give the `source:` field a `#[serde(default = "default_source")]` so a bare
  `!foo { period: 20 }` means "of the close".
- **Report bad input with `Err`, never `panic!`.** See
  [Build errors are values](../CLAUDE.md#build-errors-are-values).

### 4. The type table — `src/spec/typecheck.rs`  *(compiler-enforced)*

Two exhaustive, wildcard-free matches: `output_type` (what your tag produces)
and `children` (what each slot demands). **The crate will not compile until you
classify the new variant**, which is the point — this is the one drift guard
that cannot be skipped.

### 5. The catalogue — `src/cli/list.rs`  *(test-enforced)*

Add an `Entry` to the matching `Group` in `GROUPS`. Groups are kept in
alphabetical order of title.

`list::tests::the_catalogue_documents_every_spec_tag` fails until you do. Its
expected set comes from serde's own variant list, so it needs no upkeep.

### 6. The Python binding — `python/src/lib.rs`  *(test-enforced)*

For the common shapes there is a macro; use it rather than naming a concrete
nested type like `Ema<Sma<Current, …>>`, which Python cannot carry across FFI:

| Shape | Macro |
| --- | --- |
| `Real` source + period | `src_period!(foo, Foo, "doc")` |
| whole-candle + period | `bar_period!(foo, Foo, "doc")` |
| whole-candle, no args | `bar_noarg!(foo, Foo, "doc")` |
| multi-output + period | `bar_period_multi!(foo, Foo, "doc")` |

Then register the name in the `reg!(...)` list inside `#[pymodule] fn fugazi`,
and add a smoke test to `python/tests/test_fugazi.py`.

`python/tests/test_parity.py` fails until the tag is either bound or listed in
`NOT_BOUND` **with a reason**. "Not worth binding" is a fine reason; leaving it
undecided is not.

### 7. The warm-up battery — `tests/warm_up.rs`

Add a case to `warm_up_is_exact_for_the_catalogue`:

```rust
real_case(Foo::new(Identity::new(), 20), "foo");     // Real-source
candle_case(Foo::new(Current::close(), 20), "foo");  // candle-rooted
```

If your indicator's first `Some` is **data-dependent** (it can't be predicted
from the sample count alone), it does not belong in this battery — `IfElse` and
`BarsSince` are the existing exclusions. Add it to the documented exclusion list
in `warm_up_is_exact_for_composition` with a one-line reason, and test its
readiness some other way. Do not weaken `assert_exact_warm_up` to accommodate it.

### 8. Docs

`doc/STRATEGIES.md` for the tag reference, `README.md` if it's headline-worthy,
`python/README.md` if the Python call shape is non-obvious.

### Checklist

```sh
cargo test -p fugazi                                # warm-up + catalogue + typecheck
cargo clippy -p fugazi --all-targets -- -D warnings
cd python && maturin develop && pytest              # smoke + parity
```

---

## Add a signal

A signal is just `Indicator<Output = bool>` — there is no second trait
hierarchy. Same seven steps, with:

- The variant goes on `SignalSpec` (`src/spec/signal.rs`), not `ExprSpec`.
- `boolean(s)?` is the child shorthand; `real(s)?` for a `Real` operand.
- The catalogue entry goes in a signal-flavoured group (comparisons, boolean
  logic, edge detectors, crossovers).
- Python-side, a signal is usually a **method** on `Indicator` / `Signal`
  rather than a module function — record which in `test_parity.py`'s
  `METHOD_BOUND`.

---

## Add an operator

Arithmetic, comparison, boolean logic and lookback all run through generic
carriers, so a new operator is **a trait impl plus a type alias**, never a new
carrier type and never a macro:

```rust
#[derive(Debug, Clone, Copy, Default)]
pub struct PowOp;
impl BinaryOp for PowOp {
    type Lhs = Real; type Rhs = Real; type Output = Real;
    fn apply(&self, lhs: Real, rhs: Real) -> Option<Real> { Some(lhs.powf(rhs)) }
}
pub type Pow<L, R> = Combine<L, R, PowOp>;
```

- Binary → `BinaryOp` + `Combine` (`src/indicators/ops.rs`).
- Unary lookback → `LookbackOp` + `Lookback`.
- Rolling extremum → `ExtremeOp` + `Extreme`.

Arithmetic and boolean ops are zero-sized `Default` markers; comparison ops
carry their `epsilon` by value. Then follow steps 3–8 above.

---

## Add a strategy shape

Rare, but the machinery is built for it. A shape is a document type plus a live
strategy; everything downstream is generic over
[`RunnableStrategy`](../src/spec/runnable.rs).

1. The strategy in `src/strategies/` (or `src/portfolio/` for a composite).
2. Its `*StrategySpec` + `Dyn*Strategy` wrapper in `src/spec/`, with `try_build`.
3. `impl RunnableStrategy for Dyn*Strategy` — `stable_period` / `warm_up_period`,
   and override `drive` **only** if it can't use a plain `PaperWallet` (the
   portfolio does, because its fills route through a composite wallet).
4. A `StrategySpec` variant, and arms in `kind` / `try_build` /
   `try_build_priced` / `universe`.
5. A `StrategyKind` variant + prefix routing in `src/spec/input.rs`, and an arm
   in `optimize::build_any_spec` and Python's `spec_from_value`.
6. A section in `doc/STRATEGIES.md`.

You should **not** need to add anything to `spec/backtest.rs`, the optimize
kernel, or the CLI's evaluate paths — those are shape-agnostic. If you find
yourself wanting to, the difference probably belongs on `RunnableStrategy` or
`StrategySpec` instead.

## Add a metric

`src/metrics.rs` has **no aggregate `compute`** — one `pub fn` per metric.

1. Write the function. Build on `per_bar_returns` / `reconstruct_trades` /
   `drawdown_segments` rather than re-walking the report.
2. Return `Option<Real>` if the denominator can vanish, plain `Real` if it's
   always defined (`0.0` on empty input).
3. Values in natural units — `0.15` means +15%, not 15.
4. Add the field to the CLI `Metrics` document (`src/spec/metrics.rs`) and
   populate it in `metrics::from_report`. The serde name is the YAML/CSV column
   name, so it is user-visible.
5. Bind it: `#[pyfunction]` plus the name in `register_metrics_module`'s
   `reg!(...)`. `Option<Real>` maps to `Optional[float]`; `Real` to `float`.
6. Document it in `doc/METRICS.md`.

New numeric leaves on the `Metrics` document become `optimize --metrics` /
`--best-by` selectors automatically. If the metric has a natural direction
(higher is better / lower is better), add it to `direction_for` in
`src/spec/optimize.rs` or `--best-by` will refuse it.

---

## Add a remote provider

All providers implement the one `SeriesSource` trait (`src/sources/`):

1. `src/sources/<venue>.rs` implementing `atoms()`, plus `schema()` when the
   overlay columns are known before the fetch and `tickers()` when the venue
   exposes an enumeration endpoint.
2. Publish the schema through a `OnceLock` (`<venue>_schema()`), matching the
   existing providers.
3. If samples arrive off-cadence, bucket them with the shared
   `sources::floor_to_bucket` so two overlay CSVs join on the same timestamps.
   Note whether each column is a **level** (keep one representative sample) or
   an **accrual** (sum over the bucket) — funding rate is the accrual case.
4. Register in `KNOWN_PROVIDERS` (`src/cli/get.rs`) so `fugazi get` and
   `fugazi list sources` pick it up.
5. Python: a `Py*` client class, registered, plus a branch in
   `fetch(provider=…)`. Every provider exposes one `.fetch(...)` routed through
   `fetch_frame`.
6. Tests go in `tests/sources_<venue>.rs` against `wiremock`, not the live API.

A price-less provider is fine — `Atom::candle` is `Option`, so an overlay-only
series just yields atoms with no bar.

---

## Add a sizing recipe

Free functions in `src/indicators/sizing.rs` returning an
`Indicator<Output = Real>`.

- **Price-based** recipes that read the strategy's own asset need a
  source-generic twin (`foo` and `foo_of(source, …)`), because the bare form
  uses `Pick::new()` and panics on a multi-symbol snapshot — a basket needs to
  pass a per-leg `Pick::matching(...)`.
- **Book-anchored** recipes take `&Book` and get a `source:` field in YAML
  resolving to `!strategy_book` (default) or `!portfolio_book`.

Then the usual spec tag + catalogue entry + parity decision.

---

## Work in progress

Things that are deliberately half-built, so you don't mistake them for bugs or
finish them the wrong way.

### Strategy-layer limit entries 🚧

The wallet layer has resting limit orders — `Wallet::set_limit` / `cancel_limit`,
implemented by both `PaperWallet` and `BinanceFuturesWallet`, mirrored in Python.
**No strategy shape uses them.** `SingleAssetStrategy` and the other four still
enter at market, so a limit entry today means writing your own `Strategy` (or
driving the wallet directly).

Wiring them into the four signal slots is a design question rather than missing
plumbing. An entry signal that fires is currently an instruction that *will*
execute next bar; a limit entry may never fill, so the strategy layer would have
to decide what the signal means:

- does the intent expire when the signal stops firing, or rest until cancelled?
- does an unfilled limit convert to a market order after N bars?
- what happens when the exit signal fires while the entry is still resting?

Each answer is a different product, and the crate deliberately has no
`(signal, action)` rule table to hang them on (see *Not a rule engine* in
`CLAUDE.md`). **Don't pick one without being asked.**

## Release a version

Four manifests must agree; `cargo build` only catches the Rust drift.

1. `Cargo.toml` — `X.Y.Z`
2. `python/Cargo.toml` — `X.Y.Z`
3. `python/pyproject.toml` — `X.Y.Z`
4. `README.md` — the `## Install` snippet, `fugazi = "X.Y"` (major.minor only)

Then `cargo build --workspace` to refresh `Cargo.lock`, commit the five files,
tag `vX.Y.Z`, and push the tag. **A GitHub workflow publishes to crates.io and
PyPI** — never run `cargo publish` or `maturin publish` by hand.
`python/README.md` carries no version string.

---

## The drift guards

When one of these fails, it is telling you something specific:

| Failure | Meaning |
| --- | --- |
| `src/spec/typecheck.rs` won't compile | A new `ExprSpec` variant is unclassified. Add it to both matches. |
| `declared_output_type_matches_what_build_produces` | The type table claims a tag produces something `build` doesn't. |
| `declared_child_expectations_match_what_build_demands` | The table claims a slot is typed, but `build` accepts the wrong type there. |
| `the_catalogue_documents_every_spec_tag` | A tag is invisible to `fugazi list indicators`. |
| `the_catalogue_documents_nothing_the_parser_rejects` | The catalogue advertises a tag that errors — typo, or a removed variant. |
| `test_parity.py::test_every_*_tag_is_bound_or_declared_unbound` | A tag has no Python counterpart and no recorded reason. |
| `test_parity.py::test_the_declared_tables_do_not_go_stale` | A tag left the spec layer but its parity entry didn't. |
| `warm_up_is_exact_for_*` | `warm_up_period()` disagrees with when the first `Some` actually lands. |
| `tests/talib_validation.rs` | An indicator's numbers drifted from the TA-Lib reference. |

The expected side of the catalogue and parity tests is read off **serde's own
variant list** (`spec::typecheck::known_expr_tags` and friends), so it stays
correct for free — do not replace it with a hand-written list.

---

## Conventions that bite

**Composition is construction.** "X of Y" takes its source in `new` (or `of` for
source-generic leaves). Do not add pipe / `then` / `Chain` combinators.

**Safe defaults, one named opt-out.** Any knob that could paper over an
unsettled bar waits by default, with exactly one explicit escape hatch
(`Unstable` / `--keep-unstable`). If you add such a knob, follow the pattern.

**Build errors are values.** `ExprSpec::try_build` returns `Result`; messages
carry a `!tag > ` breadcrumb built inside-out, and must not repeat their own tag
(the trail already has it). A type mismatch is attributed to the *child* that
produced the wrong type. The only place a panic legitimately remains is the lazy
per-symbol factories in `basket` / `multi_asset` — if you add a per-symbol slot
there, **add it to the build-time probe**.

**Blessed series.** A `source:`-omitted price leaf reads "this series", carried
as the `root: Option<&Selector<String>>` parameter on `try_build`. Calendar
leaves ignore the root and use `PickAny`, because every entry in a bar shares
`atom.time`. Adding a leaf that depends on *which* asset means routing it
through `atom_src`, not `atom_src_any`.

**Wallet is the only execution seam.** Strategies never learn prices; `trade`
takes `&self` and a `&mut dyn Wallet`. Don't add a price argument to reach past
it.

**Not a rule engine.** `SingleAssetStrategy` is four signal slots plus
protective levels. Don't add `(signal, action)` tables without being asked.
