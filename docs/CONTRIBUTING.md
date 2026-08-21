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
- [Where a test goes](TESTING.md#where-a-test-goes) *(in docs/TESTING.md)*
- [Work in progress](#work-in-progress)
- [Release a version](#release-a-version)
- [The drift guards](#the-drift-guards)
- [Conventions that bite](#conventions-that-bite)

---

## Before you start

```sh
cargo build                                        # library + CLI
cargo test -p fugazi                               # 1100+ tests, incl. doctests
cargo clippy -p fugazi --all-targets -- -D warnings # inner loop

cd python && maturin develop && pytest             # bindings
```

**The gate is `scripts/ci-local.sh`** — the four commands above are the inner
loop, not the check. The script runs exactly what `.github/workflows/ci.yml`
runs, in the same order, and three of those checks fire nowhere else: the
rustdoc lints (only under `RUSTDOCFLAGS=-D warnings`), clippy over `python/src`
(~11k lines that `-p fugazi` scopes past), and the feature matrix (`live`
compiles in no other job). Run it before you push.

```sh
scripts/ci-local.sh              # all four jobs, ~23 checks
scripts/ci-local.sh rust         # one job: rust | version-sync | features | python
FAST=1 scripts/ci-local.sh       # skip the matrix + wheel rebuild (inner loop only)
```

`tests/ci_mirror.rs` fails if the script stops matching the workflow, so a new
CI step has to land in both.

[docs/TESTING.md](TESTING.md) is the map of the test suite — the five layers,
where a given change's test belongs, the shared `tests/common/` harness, and the
fixture skip-vs-fail policy. Read it before adding a test file or a helper.

Three repo-specific rules:

- **Do not run `cargo fmt` across the repo.** The tree is not rustfmt-clean
  (~95 files differ) and CI does not check formatting. A blanket reformat buries
  your change in noise. Match the surrounding style by hand.
- **Grep before writing a helper.** `CLAUDE.md` carries a table of existing
  helpers ("Existing helpers — grep before writing new code"). If the private
  function you're about to write has a name resembling a row in that table, it
  already exists. This repo has been bitten by it: six `evaluate*` bodies
  independently inlined a block byte-identical to `schema_from_snapshots`, forty
  lines above them.
- **The same rule applies in `tests/`.** Each file there is its own crate, so
  shared harness code is `mod common;`-included, not imported — check
  `tests/common/` before writing a bar builder, a temp path, a binary
  invocation, or a mock server. This repo has been bitten by that one too:
  `unique_path` and `serve` each existed byte-identical in two files.

---

## Add an indicator

The worked example is a rolling `Real → Real` indicator, the most common shape.
Nine places, in dependency order. Steps 2–6 and 9 are enforced — the compiler or
a test stops you. Steps 7 and 8 (the warm-up battery and the reference battery)
are **opt-in by nature**: a battery only asserts what is in it, so a case you
don't add is a case nobody checks. Do those two deliberately.

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
    fn warm_up_bars(&self) -> usize { /* EXACT — see below */ }
    fn unstable_bars(&self) -> usize { /* 0 unless you carry a seed forward */ }
    fn reset(&mut self) { /* ... */ }
}
```

Reuse the shared cores rather than another indicator's public type:
`WindowStats` (mean / variance / stddev / higher moments), `WindowExtreme` (monotonic deque),
`WindowQuantile` (rolling order statistics), `EmaState` / `WilderState`
(recursive smoothing). They live in `src/indicators/{stats,smoothing}.rs` and
have no `Indicator` impl on purpose.

**`warm_up_bars()` must be exact**: `update` returns `None` for the first
`warm_up_bars() - 1` samples and `Some` from sample `warm_up_bars()` onward.
Not an estimate — `tests/warm_up.rs` asserts it sample by sample.

#### Writing one that is fast without trying

None of this is micro-optimisation, and none of it is optional-but-nice: each
line below is a mistake that shipped, was measured, and cost between 25% and 60%
of an indicator. The full numbers are in `docs/PERFORMANCE.md`; this is the
short form, in the order you will hit them.

* **Do not allocate in `update`.** No `Vec`, no `Box`, no `String`, no
  `VecDeque` — not per bar, not per *anything*. If you need a window, it has a
  fixed capacity known at construction, so it is a `Box<[T]>` ring built in
  `new`. This is what the shared cores are; `WindowExtreme` was itself still a
  growable `VecDeque` until Phase 10, which cost `Aroon` **38% of its runtime**
  for a flexibility nothing used. `tests/perf_guard.rs` asserts allocations do
  not scale with bar count, and it is in CI.
* **Reuse a core; do not write a fifth window.** `WindowStats`, `WindowExtreme`,
  `WindowQuantile`, `EmaState`, `WilderState`. A new one starts out with the
  bugs and the costs these have already had fixed.
* **Ask a core for everything you need in one call.** `WindowStats::mean()`
  followed by `stddev()` computes the mean twice, and a `divsd` is ~15 cycles,
  unpipelined, on the critical path. `mean_and_stddev()` / `mean_and_variance()`
  exist for that; if you add a pair with a shared intermediate, add the combined
  reader too. `Bollinger`, `ZScore` and the Kelly sizer all had this.
* **Take the narrowest input domain the indicator actually consumes.** A bar
  indicator's source is `Indicator<Output = Candle>`, not `Atom`. Lifting a
  40-byte `Candle` into an 88-byte `Atom` per bar was **97 instructions/sample**
  on the Python bar path and 77% of what the ATR benchmark was reporting.
* **`crate::num::max_finite`, never `f64::max`.** Rust specifies `f64::max` to
  *ignore* NaN, and the fixups cost 22 instructions against 10 — 26% of ATR.
  `src/num.rs` exists for this; the divergence (NaN propagates) is deliberate
  and documented.
* **If a query has to scan the window, reduce it on `LANES` accumulators over
  one contiguous run** — see `lanes_sum_sq` in `src/indicators/stats.rs`. A
  single running total makes every add wait on the one before it, so the scan
  costs `period` × FPU add latency regardless of how tight the loop is. Reduce
  the *full ring* when the window is full, not the two halves `slices()`
  returns: at short periods the halves put most of the window in the scalar
  remainder and the lanes buy nothing.
* **Do not reach for `.shared()`.** `Shared`/`SharedComponent` advance a source
  once per bar across several accessors, but the cell is an `Arc<Mutex<_>>` and
  each accessor pays an uncontended lock — ~12 ns. Every leaf indicator in this
  crate is cheaper than that, so two independent `Component`s (what
  `src/spec/expr.rs` builds, and what `macd.line()` / `macd.signal()` build)
  measure **13× faster** than the shared pair. It pays only for a genuinely
  expensive source.
* **Storing the output is free — keep `value()`.** The obvious worry is that
  `pub value: Option<Real>` costs a store per bar. Measured, it is **+1.1% at
  the ceiling**, because these loops are latency-bound and the store ports idle
  in the shadow of the recurrence. Do not invent a no-store variant.
* **If TA-Lib has an equivalent, add it to `benches/three_tier.rs`** — and to
  `tools/bench_talib_native.c` and `tools/bench_three_tier.py`, which carry the
  other tiers. That is how `Aroon` was found to be losing to `TA_AROON`; an
  indicator nothing compares stays slow quietly. Match the *unit of work*: a
  multi-output indicator's `update` produces every line, so it goes against the
  one TA-Lib call that fills every output array.
* **Through the bindings, the budget is 1.25× `talib`** — the `py vs py` column,
  since both sides cross a Python boundary. Over that is a bug in the binding,
  not a property of the incremental design. One standing exemption: anything
  whose cost is dominated by a `WindowStats` *dispersion* read, because the Rust
  engine alone is already 3.35× there and the gap is the centred variance, not
  the boundary. See *The Python binding budget* in
  [docs/PERFORMANCE.md](PERFORMANCE.md) — and add a written exemption there
  rather than letting a row sit over budget unexplained.

#### Multi-output indicators

An `Output` that is a value struct carries three extras: `component_accessors!`
for the per-line `Component` accessors (step 1), a `MultiOutput` impl in
`python/src/carriers.rs`, and a `PyMulti` constructor (step 6).

Their cost through the Python bindings is **dominated by writes, and scales with
the number of output lines** — a batch `feed` materialises one full-length NumPy
column per line. Measured with callgrind on a three-line value
(`cs.macd`/`cs.sma`, cache simulation on):

| | 1 line (`sma`) | 3 lines (`macd`) |
|---|---:|---:|
| instructions/sample | 38.8 | 154.5 |
| data writes/sample | 4.7 | 19.5 in the fold alone |
| D1 write misses/sample | 0.125 | 0.375 |

The misses are already optimal — 0.375 is exactly three columns at one cache
line per eight doubles — so there is nothing to win there. The fold itself has
**essentially no cache misses at all** (23 D1 read misses per million samples):
it is issue-bound on instructions and stores, not memory-stalled. So:

* **`write_row` must be a plain field copy.** No allocation, no `Vec`, no
  formatting. It is called once per bar per indicator.
* **Every line you add is a full output column.** Through the bindings that is
  roughly 0.5-1.6 ns/sample, converged, for the column alone. Cheap per line,
  but it is the one thing that scales, and a *derived* line — `oscillator` is
  `up - down`, `histogram` is `macd - signal` — spends it on a subtraction the
  caller could do. Measured: dropping both bought `aroon` 1.6 ns/sample and
  `macd` 0.7. Small enough not to be worth an API change, large enough not to
  add lines carelessly.
* **`Option<Real>` per line costs two words each** (`Option<f64>` has no niche),
  so the `pub` per-line fields the `value()` contract needs are `2N` words of
  stores per bar. That is a third of the fold's writes for a three-line value.
  It is the accepted price of `value()` — see the write-back measurement in
  `benches/breaking.rs` — but it is a reason to keep `N` small, not a licence to
  grow it.
* **Add the indicator to `tools/icount_python.py` and `benches/icount.rs`.**
  Those two together are what let the boundary be separated from the engine
  (subtract one from the other); an indicator in neither cannot be diagnosed
  when it turns out slow.
* **If the engine has a data-dependent inner loop** — a rolling extreme, the
  quantile core, anything with a `while` over a window — say so next to the
  workload. That subtraction is only valid for input-independent work, and
  `Aroon` costs 1.43x more on one realistic price series than another. See
  *Ruled out, so nobody re-tries them* in [docs/PERFORMANCE.md](PERFORMANCE.md).

**The output allocation is not yours to fix and not a fugazi problem.** `lines`
separate NumPy arrays cost ~8-10 ns/sample against ~0.4 for one, a threshold at
two arrays rather than a slope. TA-Lib pays the same toll — `talib.AROON` costs
7.84 ns/sample more than `talib.AROONOSC` for its second output array, which is
very nearly its whole Python wrapper cost.

### 2. Re-export — `src/indicators/mod.rs`

`mod foo;` plus `pub use foo::Foo;`.

### 3. The YAML tag — `src/spec/expr.rs`

Add a variant to `NodeSpec` **and** to the private `NodeSpecRaw` mirror, then a
`try_build` arm. (`tests/hand_maintained_mirrors.rs` guards the mirror — the
compiler catches a missing variant via `typecheck.rs` and a missing *named*
default via the field's type, but a dropped bare `#[serde(default)]` on an
`Option` field compiles clean and silently makes the key required.)

The recursive-build shorthands are already in scope:

```rust
Foo { source, period } => dyn_indicator::wrap(
    crate::indicators::Foo::new(real(source)?, period.get()),
),
```

- **A period field is a `NonZeroUsize`**, so serde rejects `period: 0` at parse
  and `fugazi check` catches it — no build-time guard needed, and `.get()` hands
  the `usize` to the constructor. Use a plain `usize` only where 0 is genuinely
  meaningful (`longs` / `shorts` on a selection rule). A constraint *between*
  two fields still needs a build-arm check returning `Err` — see
  `!variance_ratio`, whose `period >= lag + 2` no type can express.
- `real(s)?` / `candle(s)?` build a child and view it as `Real` / `Candle`.
- `atom_src(source.as_ref())?` is the `Pick`-shaped `source:` slot every
  atom-input leaf takes.
- Give the `source:` field a `#[serde(default = "default_source")]` so a bare
  `!foo { period: 20 }` means "of the close".
- **Declare the required fields first**, defaulted ones after — `period` before
  `source`, not the other way round. Field order is nothing to YAML (mappings
  are unordered) but it is what `spec_grammar()` reports and what every consumer
  renders: `!sma { period, source=!close }` leads with what you must write.
  Nothing enforces this, so it is on you; the payoff is that no consumer has to
  re-sort, which is why the ordering lives here and not in `cli::list`.
- **Document it** *(test-enforced)*. Every variant **and every field** needs a
  one-line `///` doc — `spec_grammar()`'s prose is the generated end-user
  reference, and `spec_grammar::tests::every_tag_and_field_is_documented` fails
  until both are present. Say what a field defaults to when it's
  `#[serde(default)]` (e.g. `period` → "lookback in bars"; `source` → "…defaults
  to `!close`").
- **Classify the variant with `#[grammar(kind = "…")]`** *(compiler-enforced)*.
  `#[derive(SpecGrammar)]` on `NodeSpec` reflects the variant into
  `spec_grammar()` (names, shape, fields, defaults, and `///` prose all come
  from the definition), but it cannot infer the semantic family — so `kind`
  (`source` / `indicator` / `operator` / `predicate` / `function`) is mandatory
  and the crate will not compile without it. Add `output = "bool"` (etc.) when
  the tag isn't a scalar, and `since = "X.Y"` on the new variant (existing tags
  carry the baseline).
  **If your tag parses in a shape its variant doesn't have**, say so with
  `alt = "…"` — a variant's field form is one spelling, and the descriptor's
  `forms` list is what a consumer validates and completes against. There is one
  pattern today, `unary_source` (the inner written bare *or* under a lone
  `source:` key, as `!changed` / `!unstable` do); adding a second means adding an
  arm to `alt_form` in the derive. Don't skip this on the theory that nobody
  reads it: `no_unary_wrapper_hides_an_undeclared_mirror` probes the mirror
  spelling of every unary-shaped tag and fails if one parses undeclared. The descriptor's `default` for a scalar field is read
  from its `#[serde(default = "…")]`, so a canonical numeric default belongs in
  a const-backed default fn (see `MACD_FAST` & co. in `expr.rs`), not hard-coded
  in the Python signature — the parity test pins the two together. A **node**
  default (a `source:` falling back to `!close` / `!current`) is read the same
  way and reported as the other arm of the tagged `default`, `{ expr: "!close" }`
  — the YAML fragment omitting the key writes. Give it a
  `#[serde(default = "…")]` fn returning the leaf and the descriptor spells it
  for you. Two tests then hold you to it —
  `a_default_expr_is_equivalent_to_omitting_the_field` parses the tag both ways
  and demands the same tree, and `defaulted_expression_slots_name_their_default`
  fails if a defaulted expression slot reports nothing (teach
  `grammar::default_expr_of` the spelling; don't leave the fact in the `///`).
  An `Option<Box<NodeSpec>>` slot is the deliberate exception: its default is
  `None`, "the key is absent", which names no node.
  `spec_json_schema()` is a further projection of the descriptor, so a
  correctly-annotated tag flows into the JSON Schema for free (a brand-new
  *field type* is the one exception — it lands as `"other"` until you add its
  fragment to `type_fragment` in `spec::grammar`, to `tests/spec_grammar.rs`'s
  `FIELD_TYPES`, **and** to `_dummy` in `python/tests/test_spec_json_schema.py`;
  `hand_maintained_mirrors` checks the last one from `cargo test`, since
  forgetting it otherwise only shows up under `pytest`).
- **Report bad input with `Err`, never `panic!`.** See
  [Build errors are values](../CLAUDE.md#build-errors-are-values).

### 4. The type table — `src/spec/typecheck.rs`  *(compiler-enforced)*

Two exhaustive, wildcard-free matches: `output_type` (what your tag produces)
and `children` (what each slot demands). **The crate will not compile until you
classify the new variant**, which is the point — this is the one drift guard
that cannot be skipped.

`children` feeds two consumers now. The parse-time check (`check_immediate`) is
one; the other is `slot_demand(tag, slot)`, the tag-keyed view that stamps
`node_output` onto the grammar descriptor for editor tooling. It reads the same
table through a **prototype** node synthesised from your tag's own grammar
record, so there is nothing extra to write — but `demand_table_covers_every_node_slot`
fails if a tag with an expression slot reports no demand, which happens when
`children` is missing an arm *or* when `prototype` can't build the tag (a new
field type needs a case in `prototype_filler`).

### 5. The catalogue — nothing to write  *(test-enforced)*

`fugazi list indicators` renders itself from the grammar descriptor, so a tag
correctly annotated in step 3 appears automatically. There is no hand-written
entry to add — what you *do* owe it is a `category`, which comes from the
`CATEGORIES` taxonomy in `src/spec/grammar.rs`.

`spec::grammar::tests::categories_cover_every_tag_once` fails until your tag is
in exactly one category, and `categories_are_alphabetical` keeps the list
ordered. `cli::list::tests::the_output_renders_every_category_and_tag` then
checks the rendering. All three read serde's own variant list, so they need no
upkeep.

### 6. The Python binding — `python/src/constructors.rs`  *(test-enforced)*

For the common shapes there is a macro; use it rather than naming a concrete
nested type like `Ema<Sma<Current, …>>`, which Python cannot carry across FFI:

| Shape | Macro |
| --- | --- |
| `Real` source + period | `src_period!(foo, Foo, "doc")` |
| whole-candle + period | `bar_period!(foo, Foo, "doc")` |
| whole-candle, no args | `bar_noarg!(foo, Foo, "doc")` |
| multi-output + period | `bar_period_multi!(foo, Foo, "doc")` |

All four live in `constructors.rs` alongside the constructors they generate.
(`python/src/macros.rs` is a different set — the domain-preserving `map_source!`
/ `combine_sources!` combinators.)

Then, in `python/src/lib.rs`, add the name **both** to the
`use crate::constructors::{…}` list and to the `reg!(...)` list inside
`#[pymodule] fn fugazi` — a glob import doesn't carry a `#[pyfunction]`, which
is why every registered function is named explicitly. Finally add a smoke test
to `python/tests/test_fugazi.py`.

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

### 8. The numeric reference — `tests/indicator_reference.rs`

`warm_up.rs` pins *when* the first value lands; this pins *what it is*. Add a
case with expected values **derived by hand from your formula**, and put the
derivation in the doc comment above it, over an input short enough to check by
eye:

```rust
/// SMA(3) of `1..5` is the mean of each trailing triple: `(1+2+3)/3 = 2`, …
#[test]
fn sma_is_the_mean_of_its_window() {
    let got = run(Sma::new(Identity::new(), 3), RAMP.to_vec());
    assert_series(&got, &warm(2, &[2.0, 3.0, 4.0]), "sma3");
}
```

**Do not paste in what the implementation currently prints.** A golden master
agrees with any bug already present. If the value is only defensible against
another library's convention (TA-Lib's ADX seeding, say), it belongs in
`tests/talib_validation.rs` instead — with a column added to
`tools/gen_talib_fixtures.py` and to that file's `REQUIRED` list, then
`pixi run gen-talib` to regenerate the committed fixture.

### 9. Docs — `docs/STRATEGIES.md`  *(test-enforced)*

`docs/STRATEGIES.md` for the tag reference, `README.md` if it's headline-worthy,
`python/README.md` if the Python call shape is non-obvious.

`spec_grammar::every_tag_appears_in_the_strategies_reference` fails until your
tag is named there — as `!name`, or as a `name` code span if it reads as a bare
word. This step used to be enforced by nothing, and 15 tags had slipped
through.

### Checklist

```sh
cargo test -p fugazi                                # warm-up + catalogue + typecheck
cargo clippy -p fugazi --all-targets -- -D warnings
cd python && maturin develop && pytest              # smoke + parity
```

---

## Add a signal

A signal is just `Indicator<Output = bool>` — there is no second trait
hierarchy, and (since the value/signal spec split was merged) **no second spec
enum**: a boolean tag is an ordinary `NodeSpec` variant whose `output_type()` is
`Bool`. Same seven steps as any node, with:

- The variant goes on `NodeSpec` / `NodeSpecRaw` (`src/spec/expr.rs`) like every
  other tag — its `try_build` arm returns a `Bool`-output `dyn_indicator::wrap`
  (via `boolean(s)?` for a Bool child, `real(s)?` for a `Real` operand), and
  `typecheck.rs`'s `output_type` classifies it `Some(DynType::Bool)` while
  `children` demands `BOOL` on any boolean-inner slot.
- The strategy signal slots take a `NodeSpec`; the bool-ness is enforced by the
  slot's `AsBool` view at build (uniform with every other type clash).
- The catalogue entry goes in a signal-flavoured group (comparisons, boolean
  logic, edge detectors, crossovers).
- Python-side, a signal is usually a **method** on `Indicator` / `Signal`
  rather than a module function — record which in `test_parity.py`'s
  `METHOD_BOUND` (the single `test_every_node_tag_is_bound` test now covers it).

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
- Pointwise unary → `UnaryOp` + `Unary`. `apply` returns `Option`, so an input
  the operator has no answer for (`√x` of a negative) reads `None` rather than
  propagating a NaN.
- Unary lookback → `LookbackOp` + `Lookback`.
- Rolling extremum → `ExtremeOp` + `Extreme`.
- Unbounded running fold → `CumulativeOp` + `Cumulative`. The fold takes
  `acc: Option<Real>` so an op with no identity element seeds from its first
  sample.

**Check whether the marker already exists first.** These traits are on the
operation, not on the carrier, so one marker can serve several: `AddOp` is both
binary `+` and the fold behind `CumSum`, and `MaxOp`/`MinOp` are the pairwise,
rolling and cumulative extremes alike. `CumSum`/`CumMax`/`CumMin` added **zero**
new op types. A near-duplicate marker is the smell this is meant to prevent.

Arithmetic and boolean ops are zero-sized `Default` markers; comparison ops
carry their `epsilon` by value. Then follow steps 3–8 above.

---

## Add a strategy shape

Rare, but the machinery is built for it. A shape is a document type plus a live
strategy; everything downstream is generic over
[`RunnableStrategy`](../src/spec/runnable.rs).

1. The strategy in `src/strategies/` (or `src/portfolio/` for a composite).
2. Its `*StrategySpec` + `Dyn*Strategy` wrapper in `src/spec/`, with `try_build`.
3. `impl RunnableStrategy for Dyn*Strategy` — `stable_bars` / `warm_up_bars`,
   and override `drive` **only** if it can't use a plain `PaperWallet` (the
   portfolio does, because its fills route through a composite wallet).
4. A `StrategySpec` variant, and arms in `kind` / `try_build` /
   `try_build_priced` / `universe`.
5. A `StrategyKind` variant + prefix routing in `src/spec/input.rs`, and an arm
   in `optimize::build_any_spec` and Python's `spec_from_value`.
6. A section in `docs/STRATEGIES.md`.

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
5. Bind it *(test-enforced)*: `#[pyfunction]` plus the name in
   `register_metrics_module`'s `reg!(...)`. `Option<Real>` maps to
   `Optional[float]`; `Real` to `float`.
   `hand_maintained_mirrors::every_rust_metric_is_bound_on_the_python_module`
   fails until the name is there.
6. **Give it an external reference** *(test-enforced)*. Add a row to whichever
   `(metric, expected)` generator fits — `tools/gen_metrics_fixtures.py`
   (empyrical, equity-curve maths) or `tools/gen_trade_metrics_fixtures.py`
   (backtesting.py, anything trade- or drawdown-shaped) — then `pixi run gen`
   and commit the fixture. If no reference library has an opinion, add the field
   to `EXEMPT` in `tests/metrics_coverage.rs` with the reason and the test that
   does cover it. `metrics_coverage::every_metric_is_cross_validated_or_exempt`
   fails until you have done one or the other.
7. Document it in `docs/METRICS.md`.

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

## Add a live wallet

A live venue is one `Wallet<String>` impl behind the `live` feature (`src/live/`,
alongside `OkxWallet`) — the failure-side twin of a provider. Every strategy
shape drives it unchanged: `backtest::run(&mut strategy, &mut venue, snaps)`.

1. `src/live/<venue>.rs` implementing `Wallet<String>`. The trait is a small
   synchronous `&mut self` surface; a REST API is async, so own a private
   `tokio` runtime and block on each call (drive it from a *synchronous* context
   only). Serve the reads (`funds`/`position`/`price`/`equity`) from an
   account-state cache refreshed at the top of `update()`; route `set_position`
   (the one required movement) to the venue and return `Ack::Working` — the fill
   lands later.
2. Override the optional methods the venue supports, and **leave the rest at
   their trait defaults** — the seam degrades rather than breaks. In particular:
   - **`take_rejections()`** — the failure stream. Any wallet that can drop an
     order **must** override it, or a refused entry/exit silently desyncs the
     strategy's view of its position. Buffer every refusal (submit-time,
     at-fill, and refused protective legs) and drain them here.
   - **`positions()`** — enumerate open positions (the trait default is empty).
     Needed so a [`SleeveWallet`] can snapshot a baseline and a `Portfolio` can
     check its books; without it those treat the account as flat.
   - **`poll_fills()`** for a venue that reports fills out of band (a trades
     endpoint) rather than only through `update()`.
   - Resting `set_stop`/`set_take_profit`/`cancel_protective` and
     `set_limit`/`cancel_limit`/`cancel` where the venue has them; the `size` on
     the protective legs is **reduce-only** and may be a *partial* (several
     owners rest their own share on one account).
   Any unit translation the venue needs lives at this boundary — `OkxWallet`
   converts contracts↔base units so nothing above the wallet sees a contract.
3. Errors: the trait-facing `WalletError` is a `Copy` enum with no room for
   detail, so return the `Venue` category and stash the endpoint/status/body on
   an internal `LiveError` log (`src/live/mod.rs`) a caller can inspect
   (`<Venue>Wallet::errors()`).
4. Re-export from `src/live/mod.rs` and `src/lib.rs`, both under
   `#[cfg(feature = "live")]`.
5. Python (mirror `PyOkxWallet` in `python/src/strategy.rs`): a `Py*Wallet`
   pyclass mirroring `PaperWallet`'s order-flow surface plus the venue's
   constructors; register it in `#[pymodule]`; **add its type to the
   `over_any_wallet!` dispatch's `cast::<…>` chain** — one edit, and it reaches
   both the hand-built shapes' `.run(...)` and the spec surface
   (`StrategySpec.run` / `.run_resumable` / `.warm_up`). Enable `"live"` in
   `python/Cargo.toml`. Record its bound / not-bound methods in
   `python/tests/test_parity.py` (test-enforced ledger).
6. Tests go against a mock HTTP server (`wiremock`) — `tests/live_okx.rs` and the
   `src/live/okx.rs` unit tests — never the live API. A venue's free **demo /
   paper** endpoint (OKX selects it with an `x-simulated-trading` header) is the
   manual end-to-end.

[`SleeveWallet`]: run the strategy against its own carve-out of an account that
already holds positions — wrap the live wallet before `backtest::run`.

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
implemented by both `PaperWallet` and `OkxWallet`, mirrored in Python.
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
`(signal, action)` rule table to hang them on (see *Not a rule engine* below).
**This is an open design question — please raise an issue before implementing
one**, so the semantics get settled once rather than per-shape.

## Release a version

**The list of places that must agree lives in
[CLAUDE.md](../CLAUDE.md#bumping-the-version--sync-seven-places-cargo-check-only-catches-rust-drift)**
— deliberately a single copy, so the list cannot drift between documents. CI's
`version-sync` job checks them, so a mismatch fails the build rather than
shipping.

Then `cargo check --workspace` to refresh `Cargo.lock`, commit the manifests,
tag `vX.Y.Z`, and push the tag. Then **publish a GitHub Release** for that tag
(`gh release create vX.Y.Z --generate-notes`, or Releases → Draft a new release).
**Publishing the release triggers a GitHub workflow that publishes to crates.io
and PyPI** — pushing the tag alone does nothing, and you should never run `cargo
publish` or `maturin publish` by hand. `python/README.md` carries no version string.

Use `check`, not `build --workspace`. Building the workspace links the pyo3
cdylib, which needs a Python interpreter to resolve `_PyBaseObject_Type` and
friends; locally it dies in a wall of `ld:` output that reads like a broken
release and isn't. (`maturin develop` is what links it properly — see
[Before you start](#before-you-start).) `check` refreshes the lock identically
and type-checks both crates, so it catches strictly more of what a version bump
can actually break.

---

## The drift guards

When one of these fails, it is telling you something specific:

| Failure | Meaning |
| --- | --- |
| `src/spec/typecheck.rs` won't compile | A new `NodeSpec` variant is unclassified. Add it to both matches. |
| `declared_output_type_matches_what_build_produces` | The type table claims a tag produces something `build` doesn't. |
| `declared_child_expectations_match_what_build_demands` | The table claims a slot is typed, but `build` accepts the wrong type there. |
| `categories_cover_every_tag_once` | A tag has no `CATEGORIES` entry (or two), so `fugazi list indicators` can't place it. |
| `categories_are_alphabetical` | The `CATEGORIES` taxonomy went out of order. |
| `the_output_renders_every_category_and_tag` | A tag is invisible to `fugazi list indicators`. |
| `every_tag_appears_in_the_strategies_reference` | A tag has no entry in the `docs/STRATEGIES.md` prose reference. |
| `the_mirror_has_every_variant` | `NodeSpecRaw` doesn't mirror a `NodeSpec` variant. |
| `the_mirror_repeats_every_serde_default` | A `#[serde(default)]` on an `Option` field wasn't copied to the mirror — the key silently becomes required. |
| `every_rust_metric_is_bound_on_the_python_module` | A `src/metrics.rs` function isn't in `register_metrics_module`'s `reg!(...)`. |
| `every_grammar_field_type_has_a_python_dummy_value` | A new grammar field type has no sample in `test_spec_json_schema.py::_dummy` — that `pytest` file would fail with a `KeyError`, which `cargo test` alone would not show. |
| `test_parity.py::test_every_*_tag_is_bound_or_declared_unbound` | A tag has no Python counterpart and no recorded reason. |
| `test_parity.py::test_the_declared_tables_do_not_go_stale` | A tag left the spec layer but its parity entry didn't. |
| `warm_up_is_exact_for_*` | `warm_up_bars()` disagrees with when the first `Some` actually lands. |
| `tests/indicator_reference.rs` | An indicator's numbers drifted from its own definition. Always runs. |
| `tests/talib_validation.rs` | An indicator's numbers drifted from the TA-Lib reference. |
| `tests/metrics_validation.rs` | An equity-curve metric drifted from the empyrical reference. |
| `tests/wallet_validation.rs` | `PaperWallet`'s cash, position, equity or cost arithmetic drifted from the vectorbt reference — **including the bar a market order fills on**. |
| `tests/trade_metrics_validation.rs` | A `trades.*` or drawdown-duration metric drifted from the backtesting.py reference. |
| `tests/metrics_coverage.rs` | A metric exists with no reference value and no written exemption — or an exemption went stale. Never skips. |
| `tests/driver_contract.rs` | `backtest::run`'s per-bar order or readiness gating changed. |
| `tests/ci_mirror.rs` | `.github/workflows/ci.yml` gained (or changed) a step that `scripts/ci-local.sh` doesn't run — the local gate has fallen behind CI and would report green on a tree CI rejects. |

**Four of these could disable themselves.** The cross-validation suites compare
against an external library and *skip* when their generated fixture is absent —
and a skip is indistinguishable from a pass. `talib_expected.csv` was in
`.gitignore`, so for months the TA-Lib drift guard compared nothing on every
clean checkout.

Every fixture is now committed, and **CI runs the test job with
`FUGAZI_REQUIRE_FIXTURES=1`**, which turns a missing-or-stale fixture into a
failure. If you add an indicator to `tools/gen_talib_fixtures.py`, regenerate
(`pixi run gen-talib`) and commit the fixture or CI will tell you.
`tests/indicator_reference.rs` is the unconditional battery underneath —
hand-derived values, no fixture needed.

That switch fires on a fixture that went *missing*, though, not on a metric that
was never put in one — those are different failures. `tests/metrics_coverage.rs`
is the second: it reads the fixtures for their key sets alone, so it needs no
reference library, never skips, and fails when a field of `metrics::Metrics` has
neither a reference value nor a written exemption.

The generator environment is pinned by a committed `pixi.lock`, so regenerating
without changing anything yields an empty `git diff` and any hunk you *do* see
is attributable to your change. Read that diff before committing — see
[TESTING.md](TESTING.md#fixtures-and-the-skip-vs-fail-policy).

The expected side of the catalogue and parity tests is read off **serde's own
variant list** (`spec::typecheck::known_node_tags` and friends), so it stays
correct for free — do not replace it with a hand-written list.

---

## Conventions that bite

**Composition is construction.** "X of Y" takes its source in `new` (or `of` for
source-generic leaves). Do not add pipe / `then` / `Chain` combinators.

**Safe defaults, one named opt-out.** Any knob that could paper over an
unsettled bar waits by default, with exactly one explicit escape hatch
(`Unstable` / `--keep-unstable`). If you add such a knob, follow the pattern.

**Build errors are values.** `NodeSpec::try_build` returns `Result`; messages
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

`Portfolio` bends this the least of anything you might expect. It **trades the
wallet `backtest::run` hands it** like every other strategy; what it adds is a
`Ledger` per child, and a `LedgerWallet` seam so a child's `value_frac(1.0)`
still means "all of *my* equity". Children's intents are netted into one order
per symbol on that one account. It does **not** own a wallet, and the
`SubstrateFactory` / `PortfolioWallet` view / mis-pairing guard that an earlier
design used are gone — see *Superseded, do not reintroduce* in
[ARCHITECTURE](ARCHITECTURE.md#portfoliosym-srcportfolio) before recreating any
of them.

The one guard that remains is structural: `PortfolioBuilder::add` **panics on a
`Portfolio` child**. Nesting compiles (a `Portfolio` satisfies `add`'s bounds)
but cannot work, and a silently flat equity curve is the failure mode to design
against. If you add a shape that composes strategies, refuse the impossible
wiring at build time the same way — loudly, not by producing zeros.

**Not a rule engine.** `SingleAssetStrategy` is four signal slots plus
protective levels. Adding a `(signal, action)` table is a deliberate
non-goal — raise an issue first.
