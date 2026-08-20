# Performance

How to measure this crate, what the current numbers are, and what is known to
be slow and why.

This file is the *record*. It is not a promise: the numbers below are from one
machine (16 cores, Linux 6.18, rustc 1.95, release profile) and are useful as a
**before/after** pair, not as an absolute spec. Re-measure locally before
drawing a conclusion.

## Measuring

The harness lives in `benches/` (criterion) plus one plain probe binary. None of
it is wired into CI — `scripts/ci-local.sh` and `.github/workflows/ci.yml` are
held in sync by `tests/ci_mirror.rs`, and benchmarks are a development
instrument, not a gate.

**What *is* in CI is `tests/perf_guard.rs`**, and it is deliberately narrow: it
asserts only things that are exact — that the per-bar path's allocation count
does not scale with bar count, and the `size_of` facts the erasure design rests
on. Nothing there is timed. A wall-clock assertion on a shared runner fails on
contention, and an absolute instruction count drifts every time `stable` rustc
moves; neither would catch a regression more often than it cried wolf. The
guard also feeds itself a deliberately-allocating workload and requires the
check to fire, so a passing run means the measurement still works rather than
that the assertion has been quietly hollowed out.

| Want | Run |
|---|---|
| A/B a change | `scripts/perf-compare.sh save before` on the base, then `scripts/perf-compare.sh diff before` on your branch |
| One bench group | `cargo bench --bench tree` |
| Allocations/bar, bytes/bar, peak RSS | `cargo bench --bench footprint` |
| Instruction counts (deterministic) | `scripts/perf-compare.sh icount` |
| Instructions/sample **through Python**, vs `talib` | `pixi run -e bench python3 tools/icount_python.py` |
| Wall-clock, all four tiers | `pixi run -e bench bench`, then `python3 tools/plot_performance.py` |

**The two Python instruments answer different questions and will not agree.**
`icount_python.py` is deterministic and contention-immune but cannot see page
faults; `bench_three_tier.py` sees time but needs a quiet machine. Phase 8 below
is the case where they pointed opposite ways and the time-based one was right —
read that before trusting either alone.

criterion prints a significance verdict per benchmark against a saved baseline,
so a move inside the noise reads as "No change in performance" rather than as a
number to eyeball.

`perf` is not assumed to be installed; `icount` uses callgrind, which is
deterministic (same binary + same input ⇒ same count) and therefore a better A/B
instrument, at the cost of a ~50× slowdown.

### What each bench target is for

| Target | Question it answers |
|---|---|
| `indicators` | What does one `update` of each indicator core cost, fed a raw scalar/candle stream? The baseline everything else is built from. |
| `tree` | How does per-bar cost scale with expression-tree *depth*, split into `update` / `is_ready` / `drive`? |
| `driver` | End-to-end `backtest::run`, YAML-built vs the equivalent hand-written Rust strategy. |
| `multi_asset` | Per-*symbol-bar* cost as the universe grows 2 → 64. Flat means linear; climbing means an O(N²) lookup. |
| `wallet` | `PaperWallet::update` / `equity` / a submit→fill round-trip, swept over held-position count. |
| `metrics` | `RunReport` → `Metrics`, which `optimize` pays once per grid row per fold. |
| `metrics_variants` | What that reduction *could* cost — five cumulative candidates (fused equity walk, fused return moments, `select_nth_unstable` quantiles, fused trade pass) against the shipped one, each asserted bit-identical before it is timed. Prototypes, like `breaking`. |
| `footprint` | Allocation count, bytes, and peak RSS. Not criterion — it installs a counting global allocator, which inside a criterion target would also tally criterion's own bookkeeping. |
| `icount` | A fixed workload run exactly once, for callgrind. Answers "does this change do more work?" immune to contention and to code layout. Keep `sma_rust`/`macd_rust` in every run even when a change cannot touch them: a control reading 0.00% is what makes a −31% elsewhere in the same table believable. |
| `breaking` | Prototypes for the proposed breaking changes, so each is a measured number rather than an argument. Currently `update(&Input)` and dropping `Indicator::value()`. |
| `erasure` | What one level of type erasure costs, `PayloadValue` vs `Chain`, at 2/3/5 levels. The bench that justified Phase 6 — and that has to keep justifying it. |
| `stddev_tradeoff` | Accuracy *and* cost of the centred variance against TA-Lib's `E[X²] − E[X]²` shortcut, so the choice rests on numbers. |
| `snapshot_scan` | Does splitting `Snapshot`'s tags from its atoms make `find` cheaper? It does not — see Phase 13. Kept because it prints the working-set size beside the timing, which is the column that settles it, and because the next person to re-derive the 4.7× cache-line ratio should be able to disprove it in one command. |
| `window_ring` | `VecDeque` against the `Ring<T>` that replaced it, **all variants in one binary** — the arrangement trap 10/11 forces. Carries the shipped indicators too, and a single-workload `-- <name>` mode for callgrind (which is what settled `vwap`/`correlation`, where wall-clock had the sign wrong). |
| `latency` | Per-event latency distributions (p50…p99.9), warm against cold, for one `update` after an idle gap. The only target that measures latency rather than throughput; carries its own timer noise floor. |
| `three_tier` | The Rust tier of the TA-Lib comparison, scalar **and** multi-output. Not criterion: it emits machine-readable ns/sample for `tools/bench_three_tier.py` to line up against the other tiers. Also carries the `Component`-vs-`Shared` pair (Phase 10). |

## Measurement conditions

Every number in the results table is measured on an otherwise idle system, with
the baseline and the current tree built from **identical codegen settings**
(`CARGO_PROFILE_BENCH_LTO=false CARGO_PROFILE_BENCH_CODEGEN_UNITS=16`) so the
comparison isolates the code change rather than the profile.

Throughout this document the **baseline** is commit `da252ff` — the version bump
that set the manifests to 0.58.0, *before* any of the work below. Note that the
released `v0.58.0` tag is not that commit: the perf work landed on top of
`da252ff` and shipped inside 0.58.0, so the tag points past the baseline. Compare
against the SHA, never the tag.

Method, if you need to reproduce it:

```
git worktree add ../fugazi-base da252ff
cp -r benches ../fugazi-base/            # add the [[bench]] entries too
# then, with the same CARGO_PROFILE_BENCH_* on both sides:
cargo bench --bench tree --bench driver --bench metrics --bench multi_asset
scripts/perf-compare.sh icount ../fugazi-base
```

## A measurement that lied

`driver/sma_crossover/rust` clocked **+10.9% to +23.8% slower** after the
readiness change — reproducibly, with codegen equalised. It looked like a real
regression worth chasing.

It was not. Callgrind says that workload executes **1.61% fewer instructions**
than before:

| workload | baseline | now | instructions |
|---|---:|---:|---:|
| `sma_rust` | 111 733 102 | 109 935 508 | **−1.61%** |
| `macd_rust` | 175 407 807 | 107 533 113 | −38.69% |
| `sma_yaml` | 208 016 784 | 178 760 483 | −14.06% |
| `macd_yaml` | 274 927 805 | 193 674 165 | −29.55% |
| `tree8` | 456 018 069 | 305 387 829 | −33.03% |

Less work, more wall-clock: that is **code layout** in a separately-linked
binary, not a regression. Its signature was visible in the wall-clock data —
the same two binaries re-run gave +23.8% and then +10.9%, while the other three
benchmarks in the same runs reproduced to within 0.1pp — but signature is not
proof, and wall-clock alone cannot supply the proof.

Hence `benches/icount.rs` and `scripts/perf-compare.sh icount`. **Reach for it
whenever a wall-clock delta is under ~25% or varies between runs**, because at
that scale layout bias and real work are indistinguishable by timing. It is not
a replacement for timing — instruction count ignores cache, branch prediction
and ILP — but it answers "is this change doing more work?" exactly, and that is
usually the question.

## Baseline — `da252ff`

The pre-optimisation numbers (indicator cores, tree depth, driver, universe
scaling, wallet, metrics, footprint) are **not reproduced here** — every one has
been superseded by a later table, and keeping both invites quoting the wrong
one. They are in git at `da252ff`:

    git show da252ff:docs/PERFORMANCE.md

What still matters from that commit is the *method*, above: identical codegen on
both sides (`CARGO_PROFILE_BENCH_LTO=false CARGO_PROFILE_BENCH_CODEGEN_UNITS=16`),
or you are measuring the profile rather than the change.

## Results — baseline → released v0.58.0

Quiet machine, identical codegen on both sides, so **the profile change (F7) is
excluded from this table**; it is measured separately below.

### What changed

- **F1 — the readiness threshold is memoised.** `Strategy::is_ready` was
  `bars_seen >= stable_bars()`, and `stable_bars()` walks the whole indicator
  tree. `Combine::unstable_bars` asks both children for `stable_bars()` and then
  asks itself for `warm_up_bars()`, walking them again — so visits grow
  exponentially with expression depth, and the driver did it once per bar to
  recompute a value fixed at construction.
- **F2 — `SharedComponent::{warm_up_bars, unstable_bars}` no longer lock.** Both
  are structural properties of the source, read once at construction. The
  readiness walk called exactly these two, every bar, through the whole tree.
- **F8 — the metrics reduction sorts once**, and reuses the `max_drawdown` it
  already computed instead of letting `calmar` / `recovery_factor` each rebuild
  `drawdown_segments`.

### Wall-clock

| benchmark | baseline | now | change |
|---|---:|---:|---:|
| `tree/is_ready/8` | 2.344 ms | 3.16 µs | **−99.9%** |
| `tree/is_ready/1` | 76.60 µs | 3.25 µs | −95.8% |
| `metrics/from_report/200000` | 22.566 ms | 9.416 ms | **−58.3%** |
| `metrics/from_report/100000` | 10.977 ms | 4.768 ms | −56.6% |
| `driver/macd_crossover/rust` | 33.377 ms | 18.888 ms | **−43.4%** |
| `tree/drive/8` | 6.133 ms | 3.635 ms | −40.7% |
| `tree/drive/6` | 4.163 ms | 2.918 ms | −29.9% |
| `driver/macd_crossover/yaml` | 77.003 ms | 54.599 ms | −29.1% |
| `tree/drive/4` | 2.792 ms | 2.034 ms | −27.2% |
| `driver/sma_crossover/yaml` | 58.856 ms | 52.025 ms | −11.6% |
| `tree/drive/1` | 926.06 µs | 875.74 µs | −5.4% |
| `multi_asset/drive/*` | — | — | −7.0% … +0.1% |
| `driver/sma_crossover/rust` | 20.282 ms | 25.119 ms | *(+23.8% — layout, see above)* |

**`tree/is_ready` is now flat across depth** — 3.25 / 3.05 / 3.18 / 3.18 / 3.16 µs
at depths 1/2/4/6/8, against 76.6 µs → 2.34 ms before. The flatness is the point:
the tree walk is gone, not merely faster.

`tree/drive` improves monotonically with depth (−5.4 / −9.7 / −27.2 / −29.9 /
−40.7%), which is what the mechanism predicts, since a deeper tree spent a larger
share of each bar in `is_ready`.

**`multi_asset` barely moves.** Its legs are shallow SMA crossovers, so the walk
being removed was cheap; the per-symbol `is_ready` in `trade` was never the
bottleneck there. The earlier contended runs showed this group swinging −1.0% to
+86.2% and I briefly chased it as a regression — it is neither a regression nor a
win, just noise around zero.

### Instruction counts

See the table under *A measurement that lied*. `macd_rust` −38.7% and `tree8`
−33.0% corroborate the wall-clock for F2 and F1 respectively.

### Attribution

`sma_crossover/rust` uses no `.shared()`; `macd_crossover/rust` is the same
strategy shape with four `SharedComponent`s. The instruction-count gap between
them (−1.6% vs −38.7%) is F2, cleanly isolated.

`metrics/from_report` −58.3% is arithmetically self-checking: baseline 22.57 ms,
one sort of the return series ≈ 4.0 ms (`metrics/parts/median_return`), and
removing three sorts plus two `drawdown_segments` predicts ≈ 12.6 ms saved
against 13.2 ms observed. The `metrics/parts/*` benchmarks each moved < 6%,
confirming the gain is deduplication rather than anything compiler-side.

### The profile change (F7), settled

Method: **one source tree built three ways**, both instruments. Comparing
configurations requires that each variant be timed against its own freshly-built
binary, so `scripts/perf-compare.sh` deletes bench binaries before each variant
build and fails loudly if more than one matches — selecting one by glob picks by
hash rather than by build time, and will silently measure a stale build.

| config | rebuild | instructions | wall-clock (median) |
|---|---:|---:|---:|
| (untuned) | 42 s | — | — |
| `lto = "thin"` | 45 s | −1.7 … −5.0% | −6.8% |
| `lto = "thin"`, `codegen-units = 1` | 68 s | **−8.9 … −20.1%** | **−24.1%** |

`codegen-units = 1` carries it, and earns its +23 s. The dyn/YAML paths gain
most — −17.3% instructions on `sma_yaml`, −20.1% on a depth-8 tree — which is
what you would expect when the hot path is generic code crossing module
boundaries plus trait objects.

Thin LTO alone is marginal, and its wall-clock is mixed in sign (+8% on some
`tree/update` sizes) *despite* a small instruction reduction — layout noise
again. It is kept because it composes with `codegen-units = 1` at negligible
cost, not on its own merit.

### Correction to a previously-recorded conclusion

`tests/perf_bench.rs` recorded that the shared-handle memo "moves ~3%" because
"four `SharedComponent`s taking a mutex per bar costs about what the duplicated
arithmetic did", and concluded the residual gap was the type-erasure layer. The
mutex cost was real but **not in `update`** — it was in the readiness walk. That
note is amended in place rather than deleted; the memo experiment it describes
still stands.

## Phase 3 — allocations, hashing, and a determinism bug

Same method: identical codegen on both sides.

### What changed

- **A determinism bug in `PaperWallet::equity`.** It summed positions in
  `HashMap` order, and `RandomState` is seeded per process — so a multi-symbol
  equity curve could differ by a ULP *between two runs of the same binary on the
  same data*, and a ULP either side of a threshold is a different trade.
  `marked_equity` now sums in a canonical (ascending) order, the same fix
  `Book::update` already applied to its legs. `Book`'s own per-bar `Vec`
  allocation is gone too — both use a stack buffer up to 32 legs.
- **A fast in-crate hasher** (`src/hash.rs`, ~30 lines, no new dependency) for
  the wallet's six symbol-keyed maps. SipHash is the right default for untrusted
  keys and the wrong one for a handful of symbols the user chose.
- **Per-bar allocations removed**: `PaperWallet::update` no longer clones the
  symbol on every bar (`get_mut`-then-`insert`), `extract_self_atom` borrows
  instead of cloning an 88-byte `Atom` and no longer builds a `Selector`,
  `Book::update_one` marks a single leg without cloning its symbol, and
  `BasketStrategy` stops re-cloning symbols into its `latest_*` maps.
- **`MultiAssetStrategy`'s per-bar symbol lookup** is built once per bar into a
  keyed map instead of rescanning the snapshot inside the per-leg loop.
- **`cli::run`** builds `bars` first and then *consumes* the atoms into
  snapshots, instead of cloning each `Atom` out of a still-live vector.

### Footprint

| | before | after |
|---|---:|---:|
| allocations per bar, driving a run | 5.00 | **1.00** |
| bytes per bar, driving a run | 44.0 | **9.0** |

The remaining one-per-bar allocation is `wallet.update(sym.clone(), candle)` in
`backtest::drive`, forced by `Wallet::update` taking `Sym` by value. That is a
public trait signature — see the breaking candidates.

Snapshot *construction* is unchanged at 3.00 allocs/bar and 201 bytes/bar for
40 bytes of OHLCV; that needs an interned symbol type, also a breaking change.

### Wall-clock

| benchmark | baseline | now | change |
|---|---:|---:|---:|
| `wallet/update/16` | 1.642 ms | 711 µs | **−56.7%** |
| `wallet/equity/16` | 5.416 ms | 2.591 ms | −52.2% |
| `wallet/update/1` | 1.464 ms | 709 µs | −51.6% |
| `wallet/equity/64` | 22.447 ms | 12.717 ms | −43.3% |
| `driver/macd_crossover/rust` | 33.377 ms | 13.383 ms | **−59.9%** |
| `wallet/fill_roundtrip` | 6.854 ms | 4.681 ms | −31.7% |
| `driver/sma_crossover/yaml` | 58.856 ms | 42.749 ms | −27.4% |
| `tree/drive/8` | 6.133 ms | 3.588 ms | −41.5% |
| `multi_asset/drive/64` | 110.910 ms | 94.089 ms | −15.2% |
| `multi_asset/update/64` | 101.030 ms | 84.725 ms | −16.1% |

`driver/sma_crossover/rust` now reads −6.4%, having shown +23.8% in Phase 2 —
the same binary-layout artefact, moving the other way. Take it as noise in
either direction; the instruction count is the number to trust there.

### Universe scaling is better but still not flat

Per *symbol-bar* (should be constant in N):

| symbols | 2 | 8 | 16 | 32 | 64 |
|---|---:|---:|---:|---:|---:|
| `update`, baseline | 204 ns | 241 | 345 | 501 | 789 |
| `update`, now | 227 ns | 248 | 332 | 449 | **662** |
| `drive`, now | 310 ns | 317 | 398 | 517 | **735** |

Improved at the top end and still climbing, because **the dominant O(N²) is not
in the strategy loop** — it is `Snapshot::find`, a linear scan, called by every
`Pick::matching` leaf, once per leaf per symbol per bar. That is rooted in a
deliberate design decision (`src/snapshot.rs`: "the storage is deliberately a
sequence rather than a hashmap: `Selector` is a predicate, not a key"), so
changing it is a design question, not a tuning one. Noted, not attempted.

Small universes pay slightly more for the keyed lookup than a two-element scan
(`update/2` is +11%); the crossover is around N = 16, and `drive/2` is −7.2%
because the wallet-side hashing win more than covers it.

## Phase 5 — `Symbol` becomes a shared handle

> Phase 13 later made `Symbol` a newtype over that `Arc<str>` so it could carry a
> precomputed hash. Everything below still holds — the handle is still shared and
> a clone is still a refcount bump.

The symbol type the spec / runtime / CLI / Python layers key assets by is now
`Arc<str>` (`fugazi::Symbol`) instead of `String`. A symbol is cloned constantly
and mutated never — once per symbol per bar by the driver alone — so cloning
should be a refcount bump, not an allocation. The indicator and strategy layers
stay **generic** over `Sym`; a pure-Rust caller can still use `&'static str`.

Interning happens at each boundary where a document, a CSV or a Python `str`
becomes a symbol, and once per run rather than once per bar.

### Footprint

| | before | after |
|---|---:|---:|
| driving a run — allocs/bar | 1.00 | **0.00** |
| driving a run — bytes/bar | 9.0 | 8.0 |
| snapshot build, 1 symbol — allocs/bar | 3.00 | **2.00** |
| snapshot build, 8 symbols — allocs/bar | 11.00 | **3.00** |
| snapshot build, 32 symbols — allocs/bar | 37.01 | **5.01** |
| peak RSS (200k bars) | 98.0 MiB | **87.0 MiB** |

A 200 000-bar run now performs **37 allocations in total**, against 200 045
before: the last per-bar allocation was `wallet.update(sym.clone(), candle)`,
which the by-value `Wallet::update` signature forced and which is now free.

### Python — where this was aimed

The Python bindings gain most, because they rebuild symbols across the FFI
boundary. `snapshots_from_sequence` now carries a `SymbolInterner`, so a
20 000-bar × 32-symbol series allocates 32 `Arc`s rather than 640 000.

Measured with `python/bench/bench_run.py`, **release wheels on both sides**:

| case | before | after | change |
|---|---:|---:|---:|
| run, 32 symbols | 2.20 µs/bar | 1.81 | **−18%** |
| run, 8 symbols | 1.41 µs/bar | 1.21 | −14% |
| snapshot conversion, 1 symbol | 0.43 µs/bar | 0.39 | −9% |
| snapshot conversion, 32 symbols | 5.06 µs/bar | 4.78 | −6% |

**Measure the Python side with a release wheel.** `maturin develop` builds in
*debug*, which is 7–10× slower and amplifies unrelated changes out of all
proportion — a first pass at these numbers, taken on debug wheels, showed a 33%
"regression" that did not exist. Use `maturin develop --release`, or build a
wheel and install it.

The interner also has to be scoped correctly: an early version built one per
*snapshot*, trading a per-symbol string allocation for a whole hash table per
bar, and cost +86% on a 32-symbol conversion. It now exists only for the
sequence path, where the cache can actually hit.

### What the Python profile actually looks like

Worth recording, because it is not what the Rust benchmarks suggest: for a
Python caller driving a multi-symbol backtest, **the FFI boundary costs more
than the engine**. At 32 symbols, snapshot conversion is 4.78 µs/bar against
1.81 µs/bar for the whole run. Optimising the engine further has a ceiling for
Python users that boundary work does not.

## Phase 6 — `runtime::Chain`, a domain-typed erasure vocabulary

The largest single win in this whole pass, and the one that finally made the
Python bindings competitive. Not breaking: the new vocabulary was added beside
the old one, the consumers moved over, and nothing in the public surface changed
shape.

### The problem

Both runtime-driven builders erase at *every* level of an expression:

* spec/YAML — `NodeSpec::build` does `wrap(Sma::new(AsReal::try_new(child)?, p))`
* Python — `Source::new(Sma::new(source, p))` over an already-erased source

The old handle, `Box<dyn PayloadIndicator>`, exchanged a `PayloadValue` — an enum
**88 bytes** wide, because it is as wide as its `Atom` variant. Every level moved
that payload in and back out, with a discriminant branch and drop glue on each
move. **This cost the CLI as much as it cost Python**; it was never a bindings
problem.

### The change

`src/runtime/chain.rs`. The domain stays in the type instead of being carried
alongside the value:

```rust
pub trait DynIndicator<In, Out>: Indicator<Input = In, Output = Out> + Send + Sync {
    fn dyn_clone(&self) -> Box<dyn DynIndicator<In, Out>>;
}
pub type Chain<In, Out> = Box<dyn DynIndicator<In, Out>>;
```

`Chain` implements `Indicator` itself, so it drops into any constructor's source
slot with no adapter — which is what made the migration mechanical. `AnyChain` is
the sum over the six domains, for the one thing a payload box could do that a
`Chain` cannot: let a builder *discover* a domain it did not know statically.
`any(inner)` picks the variant from the indicator's `Output` type.

The retiring vocabulary was renamed after its mechanism first
(`DynIndicator` → `PayloadIndicator`, `DynValue` → `PayloadValue`, …), as a
mechanical commit that kept the tree green, so the two could coexist during the
migration.

### Measured — `cargo bench -p fugazi --bench erasure`

| chain | ns/sample | vs concrete |
|---|---:|---:|
| concrete, no erasure, 1 node | 1.36 | 1.0× |
| `PayloadValue`, 2 levels | 24.77 | 18.2× |
| `PayloadValue`, 3 levels | 37.64 | 27.6× |
| `PayloadValue`, 5 levels | 65.83 | 48.3× |
| **`Chain`, 2 levels** | **3.81** | **2.8×** |
| **`Chain`, 3 levels** | **5.40** | **4.0×** |
| **`Chain`, 5 levels** | **11.18** | **8.2×** |
| hand-rolled single-method trait, 2 levels | 3.66 | 2.7× |

**Marginal cost per extra level: `PayloadValue` +13.7 ns, `Chain` +2.5 ns.** The
last row is the floor — the least an erased scalar boundary can cost — and
`Chain` sits on it, so there is nothing further to win here.

### Python — the point of the exercise

Per-level cost measured three ways, all agreeing, via `_bench_feed_stage` /
`_bench_feed_built` — diagnostic hooks that were in `python/src/constructors.rs`
at the time and have since been **removed** (they routed through the output path
this document's Phase 8 replaced, so they had stopped emulating `feed()`; see
"Things that were tried"). Rebuild an equivalent probe if you need to re-derive
this table:

| levels | Rust-built chain | Python-built, driven from Rust | Python-built, via `.feed()` |
|---:|---:|---:|---:|
| 1 | 4.03 | 3.11 | 4.07 |
| 2 | 6.32 | 6.27 | 6.43 |
| 3 | 7.77 | 8.42 | 7.59 |
| 4 | 11.54 | 11.29 | 11.02 |
| 6 | 18.19 | 19.09 | 17.80 |

Three independent paths landing on the same ~2.8 ns/level is the check that
matters: it says the bindings add **nothing** per level beyond the erasure
itself, so `feed()` and the constructors are both off the hook.

### Scope of the migration

`python/src/{carriers,classes,constructors,macros,strategy}.rs`. `TypedSource<In,
Out>` — the newtype that existed purely to reattach `Input`/`Output` to a payload
box — is gone; `Source<I>`, `StrSource<I>` and `AtomBox<I>` are now plain aliases
for `Chain<I, _>`, and `SignalBox<I>` survives only because it flattens a
warming-up `None` to `false`.

One payload hand-off remains, at `carrier_inner_indicator`: the overlay-column
API takes a heterogeneous list whose members differ in *input* domain, which one
`Vec<Chain<_, _>>` cannot express. It costs one box per column at build time and
nothing per bar. It goes when `spec::overlay` moves over.

### The spec/YAML layer

Migrated too, and it is the same win: `NodeSpec::try_build` returns an
[`AnyChain`] instead of a payload box, and the `AsReal`/`AsBool`/… views it used
to coerce through are now `into_real()`/`into_bool()`/… on that enum.

Instruction counts against the commit before the migration
(`scripts/perf-compare.sh icount`, callgrind — deterministic, so these are exact
rather than sampled):

| workload | before | after | change |
|---|---:|---:|---:|
| `sma_rust` | 72 629 569 | 72 629 862 | **0.00%** |
| `macd_rust` | 67 139 942 | 67 140 185 | **0.00%** |
| `sma_yaml` | 120 858 828 | 83 224 424 | **−31.14%** |
| `macd_yaml` | 130 420 152 | 93 110 954 | **−28.61%** |
| `tree8` (depth-8 expression) | 214 228 698 | 129 873 952 | **−39.38%** |

The two hand-written Rust strategies are the **control**: this change cannot
touch them, and they read 0.00%. That is what makes the rest of the column
believable. Depth-8 gains most, which is the prediction the erasure model makes.

Wall-clock agrees where it is large enough to trust
(`cargo bench --bench driver`, 50 000 bars):

| | before | after | YAML ÷ Rust |
|---|---:|---:|---:|
| SMA crossover, YAML | 35.38 ms | **16.79 ms** | 2.76× → **1.38×** |
| MACD crossover, YAML | 37.34 ms | **21.36 ms** | 3.46× → **1.71×** |

In the same run the *unchanged* Rust paths moved +15.8% and −4.8% — pure code
layout, as the 0.00% instruction counts prove. This is trap 6 in the list below,
caught by the instrument that exists for it.

### What still rides the payload vocabulary

Not everything should move, and two things deliberately have not:

* **Overlay columns** (`spec::overlay`). `drive` asks each column what *input* it
  wants — a whole snapshot for a spec-built column, a single bar for a Python
  carrier — and one `Vec<AnyChain>` cannot hold both, since every `AnyChain` is
  snapshot-rooted. `AnyChain::into_payload` bridges at the build boundary; the
  subtree below it is narrow.
* **`spec::typecheck`.** `PayloadType` is also the *static* type vocabulary, and
  it is a plain tag enum with no payload. `AnyChain::output_type()` returns it,
  so a builder compares what a subtree declared against what it built with no
  translation step. Nothing is paid for this at run time.

## The Python bindings plan — executed

This section held a decomposition of the Python boundary and a priority-ordered
plan. **Every item in it has landed** (Phases 8-10 and the binding-budget section
below), so it has been replaced by the outcome rather than kept as a to-do list
that reads as outstanding work. The decomposition method it introduced — measure
the boundary, subtract the engine, attribute the difference — is the one still
used, and *The Python binding budget* below is its current form.

## Breaking candidates — measured, not yet done

Changes that would break the public API, **prototyped and measured** rather
than argued for. `benches/breaking.rs` holds the prototypes.

(`Sym = Arc<str>` was the third; it has since been implemented as
`fugazi::Symbol` — see *Phase 5* below.)

Read the figures as **ceilings**. Each prototype is narrower than the real
change would be, and a couple of them enjoy inlining that the general case
would not.

(The narrow-erasure vocabulary was the fourth and highest-value entry here. It
turned out not to be breaking at all, and it is **done** — see *Phase 6* below.)

### 1. `Indicator::update(&mut self, input: &Self::Input)`

Prototyped as a by-reference chain computing the same SMA crossover
(`RefIndicator` in `benches/breaking.rs`).

| | per 2 000 bars |
|---|---:|
| library, by value | 121.6 µs |
| prototype, by reference | **35.0 µs** |

−71%, but **do not take that at face value**. The prototype is monomorphic,
and it fuses `Pick` + `Close` into one node, so it avoids the `Atom` round-trip
entirely rather than merely avoiding the clone. It bounds the win; it does not
predict it.

**The sharpest reason to want it is `Pick`, and it is the reason this is the only
candidate that helps the YAML path at all.** Every spec-built leaf is rooted on a
`Pick`, whose `update` is:

```rust
self.value = self.source.update(input).and_then(|snap| snap.find(..).cloned()); // clone 1
self.value.clone()                                                             // clone 2
```

**Two 88-byte `Atom` clones per leaf per bar** — a depth-8 tree pays sixteen. And
`Pick` can avoid neither: the first because the snapshot owns the atom, the second
because it must return an owned `Atom` while keeping one for `value()`. That is the
same constraint that makes `Identity<Atom>` unfixable in place, and the reason a
`FromInput`-style dodge does not generalise: an `Indicator` owes callers a stored
value, so anything in a chain position must retain one. By reference, `Pick` hands
back an `&Atom` borrowed from the snapshot and both clones disappear.

Also removed: `Combine` feeds the same input to *both* sides, so every binary node
clones its input.

**Why the alternatives do not reach it.** The `Atom`-vs-`Candle` domain question
that the Python carriers can answer (P1 of the plan above) has no analogue here —
`AnyChain`'s variants are keyed by *output* domain and every one of them has
`Input = Snapshot<Symbol>`, because a YAML strategy is inherently multi-symbol and
therefore always roots on a `Pick`. There is no `Atom` input to narrow.

**Cost.** The largest of the candidates: ~60 indicators, `fugazi-derive`,
`runtime`'s erasure vocabulary, all five strategy shapes, `python/src/`, and every
doc example. Worth doing only if the ceiling is confirmed on a narrower slice
first — converting `Pick` and `Combine` alone would test the thesis, and `Pick`
alone would price the paragraph above.

### 2. Index `Snapshot` for lookup

Per *symbol-bar* cost still climbs with universe size after Phase 3 (227 ns at
N = 2 to 662 ns at N = 64, where it should be flat). The residual O(N²) is
`Snapshot::find` — a linear scan, run by every `Pick::matching` leaf, once per
leaf per symbol per bar.

This is not an oversight. `src/snapshot.rs` states it: *"the storage is
deliberately a sequence rather than a hashmap: `Selector` is a predicate, not a
key"*, which is what lets a snapshot avoid `Sym: Eq + Hash` and permits
duplicate tags with first-match-wins. Any fix has to keep those properties —
e.g. an optional lazily-built `symbol → first index` side table, used only when
the selector names a symbol and the entry count justifies it, falling back to
the scan otherwise.

**Cost.** Contained (one module), but it touches a documented invariant, so it
needs a design decision rather than a patch. Worth it for large universes and
irrelevant below ~16 symbols.

## Three-tier comparison — TA-Lib vs fugazi (Rust) vs fugazi (Python)

`tools/bench_three_tier.py` drives all three from one input, 200 000 samples,
median of 7. Run it with `pixi run -e bench bench` — that environment is the one
place `talib` and a built `fugazi` wheel are importable from the same
interpreter, which is what the comparison needs.

The numbers below are a recorded run; re-running reproduces the *shape*, not the
digits, since they depend on the machine and on which TA-Lib build the lock
resolves to.

## Phase 7 — the ATR gap, and what was actually wrong

Adding the native tier made fugazi's ATR look **2.8× slower** than TA-Lib. It
was worth chasing, and chasing it found two different things — one a real
optimisation, one a benchmark that had been lying for as long as it existed.

The machine was contended at the time, so none of this was settled by
wall-clock. `benches/icount.rs` grew `atr_none` / `atr_atom` / `atr_candle` /
`atr_manual_max` — the same computation with one variable changed each, plus a
control to subtract — and callgrind settled it deterministically.

| workload | instr/bar (net of control) | vs native `TA_ATR` (15.4) |
|---|---:|---:|
| `atr_atom` — as the benchmark drove it | 146.5 | 9.5× |
| `atr_candle` — fed a `Candle` | 34.0 | 2.2× |
| `atr_manual_max` — and without `f64::max` | 25.0 | 1.6× |

**1. 77% of the number was the benchmark.** `benches/three_tier.rs` held a
`Vec<Atom>` and passed `a.clone()` per bar — an 88-byte copy TA-Lib does not pay
(it reads three flat `double` arrays) and that real code does not pay either
(the driver *moves* the atom out of the snapshot). It now feeds `Candle`s. This
is not a speedup; it is a measurement that was wrong.

**2. `f64::max` costs 26% of ATR.** Rust specifies it to *ignore* NaN, and that
contract is not free: the true-range expression compiles to **22 instructions**
with `cmpunordsd`/`andnpd`/`orpd` fixups, against **10** for `if a > b { a }`,
which is what TA-Lib's C gets from its `>`. `src/num.rs` is that comparison,
applied to `TrueRange`, `Dmi`, `Rsi` and `Sar`. ATR: **34.0 → 22.0
instructions/bar.**

The exchange is documented and pinned rather than waved at. Two divergences from
`f64::max`, not one:

* **NaN propagates** instead of being suppressed. Deliberate — a NaN high is
  corrupt input, and an ATR that reports a plausible number from it is worse
  than one that reports NaN.
* **`±0.0` ties resolve to the second operand.** Found by an exhaustive test
  sweeping every pair of finite values, not by reasoning — and then found to be
  a non-issue by CI, which disagreed with this machine about what `f64::max`
  does there. `f64::max` documents that for equal inputs *"either input may be
  returned non-deterministically"*, so there was no guarantee to diverge from.
  Two rounds of being wrong about one line, both caught by tests rather than by
  argument.

Every expected-value fixture — TA-Lib cross-validation included — passes
unchanged, because no test feeds a NaN or a negative zero.

**Result:** ATR is **4.33 ns/sample against native TA-Lib's 4.83 — 0.90×.** The
apparent 2.8× loss was a benchmark artifact stacked on top of a real but much
smaller gap, and the real half is now closed.

### One baseline per tier

There are **four** tiers, not three, and it took getting it wrong to see why.

The comparison originally measured TA-Lib once, through `talib` (the Cython
bindings), and used that single number as the baseline for *both* fugazi rows.
That is right for the Python row — both sides cross a Python boundary — and
wrong for the Rust row, where the wrapper's cost is credited to fugazi.

Measured cleanly the error is small (`sma` 1.47 vs 1.37 ns/sample, `atr` 5.40 vs
4.83 — 5–12%), but it is an error in the flattering direction and it costs
nothing to avoid. `tools/bench_talib_native.c` is the native tier;
`tools/bench_three_tier.py` builds and runs it, and skips it rather than failing
when there is no C toolchain.

**A caution about that "measured cleanly".** An earlier revision of this section
claimed the ATR wrapper cost 2.5× (17.3 ns/sample against 7.0). It does not —
those runs were taken while the machine was loaded, and the figure was wrong by
a factor of ten *in the direction that made the argument look more important*.
Two things came out of that:

* every tier now discards a **warm-up** pass (a cold process reads TA-Lib's SMA
  at 1.99 ns/sample against a warm 1.38 — a 44% error, and it inflates the
  baseline, so it flatters fugazi);
* the driver takes the **best of three full passes** rather than one. Contention
  is strictly one-sided — it can only make a run slower — so the minimum is the
  least-polluted observation, where a median folds the contended passes back in.

### Results

| | TA-Lib C | TA-Lib py | fugazi rs | fugazi py | **rs vs C** | **py vs py** |
|---|---:|---:|---:|---:|---:|---:|
| `sma` | 1.37 | 1.46 | 1.37 | 4.97 | **1.00×** | 3.4× |
| `ema` | 2.06 | 2.16 | 1.36 | 4.86 | **0.66×** | 2.3× |
| `rsi` | 4.79 | 4.98 | 4.69 | 8.47 | **0.98×** | 1.7× |
| `atr` | 4.77 | 5.52 | 4.54 | 36.56 | **0.95×** | 6.6× |
| `stddev` | 3.33 | 3.56 | 10.61 | 12.77 | 3.19× | 3.6× |

ns/sample, 200 000 samples, median of 7, best of 3 passes. `docs/assets/performance.svg`
is this table as a chart (`tools/plot_performance.py`), normalised to the C column.

The **Rust** engine is at parity or better on `sma`/`ema`/`rsi`/`atr` against the
C library itself, while staying incremental. `stddev` is the deliberate loss.

### Phase 8 — the input columns were being copied, and that was most of the cost

**This is the largest single win on the Python side, and the one that had been
sitting in plain sight the longest.** Everything in the two sections below it was
measured before it and remains true as history; the numbers there are superseded.

`feed()` asked Python for each column's contiguous `float64` buffer and then
copied it into a Rust `Vec<f64>` — five copies for an OHLCV frame. It now reads
Python's memory in place ([`Column`] in `python/src/constructors.rs`).

The copy had a written justification, and the justification was false:

> Keep the columns owned (not borrowed from Python) — the buffer fast path in
> `column_to_vec` already copied them, and holding Python buffers alive across
> `update` calls would pin the GIL to the whole loop.

`feed` is a `#[pyfunction]`. It holds the GIL for its entire duration either way.
A constraint that never existed was written down as a reason, and then nobody —
including me, for most of two sessions — went back and checked it.

#### What it cost

| | ns/sample |
|---|---:|
| `close().feed(frame)` | 24.51 |
| four 1.6 MB array copies, alone | 15.95 |
| allocating **and touching** the output array | 0.18 |

Two thirds of the call. And **not the `memcpy`** — the page faults. A 1.6 MB
allocation is well past glibc's `mmap` threshold, so each is fresh anonymous
memory the kernel faults in a page at a time and reclaims on free, to be faulted
again on the next call. The give-away is that *one* copy is nearly free (0.21
ns/sample — glibc recycles a single hot block) while *four* cost 16.

The confirming experiment, before any code changed: re-running with
`MALLOC_MMAP_THRESHOLD_` raised, so big blocks stay on the heap and stay faulted
between calls, made `close().feed(frame)` **7.8× faster on its own**. That is not
something a library may ask of its host process, so the allocation had to go
instead.

#### Result

| ns/sample | before | after | |
|---|---:|---:|---:|
| `close()` on a frame | 27.16 | **4.38** | 6.2× |
| `sma(close())` on a frame | 30.04 | **7.72** | 3.9× |
| `sma(identity())` on a 1-D array | 13.61 | **5.15** | 2.6× |
| `atr(14)` on a frame | 36.90 | **14.62** | 2.5× |

`py vs py`, against `talib`'s own bindings:

| | before | after |
|---|---:|---:|
| `sma` | 7.81× | **2.56×** |
| `ema` | 5.17× | **1.69×** |
| `rsi` | 3.08× | **1.62×** |
| `stddev` | 5.42× | **3.40×** |
| `atr` | 2.07× | **0.57×** — faster than `talib` |

#### Two more instances of the same mistake

Found by asking "where else?" rather than by profiling:

* **`build_bools`** was `np.asarray(vec_of_bool)`, and **`build_multi`** was
  `asarray(&[f64])` per line. Both route through pyo3's sequence conversion, so a
  200 000-bar signal materialised 200 000 Python `bool` objects and a three-line
  indicator 600 000 `float`s — for NumPy to parse straight back out. That exact
  spelling was **already documented as rejected a few lines above**, for floats
  only. Both now fill a NumPy buffer directly.
* **`fugazi.metrics`** took `Vec<Real>` arguments. pyo3 fills a `Vec<f64>` by
  walking the object with the sequence protocol and calling `extract` per
  element, so a NumPy array was taken apart one `float` at a time. The tell was
  that an `ndarray` and a `list` cost *the same* — 44.79 vs 45.68 ns/sample —
  which means there was no fast path for the array at all. A `Series` newtype
  with a `FromPyObject` impl fixed 26 signatures at once:

  | | before | after | |
  |---|---:|---:|---:|
  | `metrics.sharpe(ndarray)` | 44.79 | **2.54** | 17.6× |
  | `metrics.ulcer_index(ndarray)` | 42.81 | **1.62** | 26.4× |
  | `metrics.sharpe(list)` | 45.68 | 39.85 | 1.1× |

  A `list` has no buffer to borrow and still goes element-wise; that path is for
  correctness, not speed. The array/list gap *appearing* is the confirmation.

#### The two corners the first sweep missed

Found by asking "where else does this shape appear?" after the fact, not by
measuring. One paid, one did not.

**Multi-output indicators allocated once per bar.** `MultiOutput::values()`
returned `vec![self.macd, self.signal, self.histogram]` — a fresh three-`f64`
heap block **every bar**, 200 000 of them for a 200 000-bar frame. Then
`build_multi` collected them row-major into `Vec<Option<Vec<Real>>>` and rebuilt
the whole thing column-major, materialising the result twice before NumPy saw
any of it. `write_into(&mut Vec<Real>)` against one reused scratch buffer removes
the per-bar allocation; `feed_into_columns` folds straight into one NumPy array
per line, removing the transpose. Interleaved, minimum of three passes:

| ns/sample | before | after | |
|---|---:|---:|---:|
| `macd` (3 lines) | 55.90 | **20.53** | 2.7× |
| `bollinger` (3 lines) | 85.76 | **52.36** | 1.6× |
| `dmi` (2 lines) | 83.51 | **58.40** | 1.4× |

**The strategy path was the same shape and it bought nothing.**
`PyStrategy.run` went through `candles_from_frame`, which copied every column and
zipped them into a `Vec<Candle>` — 8 MB for 200 000 bars — which was then walked
again to build snapshots. Streaming it removes the copies and the intermediate,
and deletes `frame_to_candles` / `assemble_candles` outright as the last users of
the `Vec<Candle>` path. Measured: **274.63 → 270.10 ns/sample, i.e. nothing.**

The reason is worth recording, because it is the counterexample to the section
above: input conversion is now **3.41 ns/sample of a 285 ns/sample call — 1.2%.**
`run` is dominated by the backtest itself and by building one `Snapshot` per bar,
each of which is an `Arc<Vec<…>>` allocation. Fixing an inefficiency that is real
but is 1% of its call site does not show up, however bad it looks in isolation.
The change was kept for the ~60 lines of code and the 8 MB of peak memory it
removes, not for speed, and it should not be cited as a performance win.

That per-bar `Snapshot` allocation is the next thing on this path, and it is a
core-design question rather than a binding one — see the `Snapshot` entry under
*Known costs*.

#### Where the remaining Python gap is — erasure, not the boundary

After Phase 8 the bindings are 1.6-2.5x `talib` on the scalar rows and faster on
`atr`. This is where the rest sits, measured rather than assumed.

Per-function, `ta.sma(ta.identity(), 14).feed(array)`, 4 M samples:

| | instr/sample |
|---|---:|
| `PyIndicator::feed` — read the column, write NumPy, loop | 17.0 |
| `Erased<Sma>::update` | 41.0 |
| `Erased<Identity>::update` | 4.0 |
| **total** | **62.0** |

That table invites a wrong conclusion — that an erased level costs 4 — so vary
one level and subtract instead (`benches/icount.rs`, `sma_scalar_*`, net of a
control that runs the same loop with no indicator):

| erased levels | | net instr/sample |
|---:|---|---:|
| 0 | `Sma::new(Identity, 14)` monomorphised | **16.0** |
| 1 | the leaf fused into `Sma`, erased once | **37.0** |
| 2 | as the bindings build it today | **45.0** |
| — | `talib.SMA`, whole call | 8.1 |

**Erasure costs ~29 instructions/sample in total, and it is very unevenly
split**: the inner level costs **8**, the outer one **21**. The outer is dearer
because removing the *last* erasure is what lets LLVM inline the chain into the
driving loop and keep `Sma`'s state in registers across samples; with any
erasure at all, that state round-trips through memory every update.

Two corrections to earlier readings, both of which flattered a conclusion:

* A per-function profile of the Python path charges only 4 instructions/sample to
  the inner `Erased<Identity>::update`, which makes a level look nearly free. The
  call setup, argument move and `Option<Real>` return are charged to the
  *caller*. Vary one level and subtract; do not read a level's cost off its own
  callee total.
* The first version of this table put the monomorphised baseline at 20.0 and
  erasure at ~30. That baseline **discarded its output** while the erased
  variants stored theirs, so it was measuring less I/O, not just less
  indirection. With all variants writing an `Option<Real>` per sample the
  baseline is 16.0. The headline number barely moved, which is luck rather than
  vindication — the per-level split it implied was wrong.

Two consequences worth keeping straight:

* **The boundary is no longer the problem.** 17 of 62. Further work on `feed`
  itself has little left to win.
* **fugazi's SMA is 20 instructions where `talib`'s is 8**, because TA-Lib
  vectorises one pass over the array and an incremental `update()` cannot. Yet
  the Rust tier and the C library both measure **1.37 ns/sample** — identical.
  Instruction parity is unreachable by construction; time parity is already
  there. Do not chase the former.

**Fusing** — building `Sma<Identity<Real>>` concretely when the source is a plain
root, so a two-level chain becomes one — is worth **8.0 instructions/sample**,
not the ~15 first estimated. **Done** (`PendingRoot` in `python/src/carriers.rs`),
and it delivered exactly that on both shapes:

| instr/sample | before | after |
|---|---:|---:|
| `sma(identity())`, 1-D | 62.11 | **54.11** |
| `sma(close())`, frame | 94.16 | **86.16** |
| `close()` alone — nothing wraps it | 53.15 | 53.15 |
| `atr(14)` — takes no source | 95.81 | 95.81 |

The two rows that cannot fuse are unchanged to the hundredth, which is the
control: fusing moved what it should and nothing else.

**Wall-clock cannot see it, and that is expected.** 8 instructions is ~0.3
ns/sample; this machine's minimum-of-35 reproduces to about ±0.4. The run after
the change put `sma` at 3.92 ns against 3.57 before, `rsi` at 6.99 against 7.76
and `stddev` at 11.23 against 11.79 — scattered both ways, i.e. trap 6 (wall-clock
conflates work with code layout) at exactly the granularity it was written for.
The README table was therefore **not** re-cut from that run: substituting one
noise sample for another is not an update. Instruction count is the instrument
that resolves a change this size, and it is unambiguous.

Two design notes worth keeping, both of which cost a rebuild to learn:

* **The root is metadata on `PyIndicator`, not two new `AnySource` variants.**
  The variant version works and is worse: `AnySource` is what ~15 unrelated
  matches dispatch on, so widening it meant either an arm in each or a
  `settle()` normaliser plus six `unreachable!()` arms the compiler cannot verify
  away. Not a good trade for 8 instructions. As carrier metadata every existing
  match is untouched and only the fusing constructors know roots exist.
* **The bar field stays a *type* parameter.** Making it a runtime enum would
  collapse seven monomorphisations into one, and costs ~5 instructions/sample on
  every field read: `ta.close().feed(frame)` went 53.2 → 58.2, a 9% regression on
  the commonest root in the API, and it gave back most of the 8 when fused. The
  seven typed instantiations cost **+0.9% of extension size** (15.75 → 15.89 MB),
  which is the cheaper side of that trade.

Its other value is being the *mechanism* a fully monomorphised carrier would
need, since that also requires the root's concrete type to survive to the
wrapping constructor.

**Batching still does not pay, now confirmed against the fused shape.** The
theory was that fusing and batching are complementary — a concrete chain inside
the box, the loop on its side of the boundary, nothing opaque left per sample, so
the whole thing inlines. Measured: 35.76 against fused's 37.02, i.e. 1.3, nowhere
near the 21 that removing the outer level buys. Two independent attempts now say
the same thing, so the batch idea is closed.

Also checked and **ruled out**: a small-input regime. The ratio is flat at
2.4-3.0x from N = 200 to N = 200 000, and `talib` pays per-call overhead too
(7.82 ns/sample at N = 200 against 1.36 at N = 200 000). The gap is per-sample,
not per-call, so `OutputKind::detect`'s per-call `String` and the `numpy` import
lookup are not worth touching.

#### Phase 9 — the cost of erasure is state placement, not the vtable

The last and largest piece, and the one whose *cause* was mis-identified for
three phases. Driving an erased chain one sample at a time costs ~21
instructions/sample more than driving the same concrete indicator. That is not
the indirect call: a vtable call with a predictable target is about two
instructions. It is that the indicator's state lives behind the box, so the
compiler cannot prove it does not alias the caller's output buffer, and must
reload and store every field on every sample. Held in a local, those fields
promote to registers for the whole loop.

`DynIndicator::update_slice` folds a slice; `Erased<I>` overrides the default to
copy the concrete indicator into a local, run, and write back once. That single
line is the entire win:

| | net instr/sample |
|---|---:|
| concrete indicator, no erasure at all | 16.00 |
| erased, slice, **state copied to a local** | **16.04** |
| erased, slice, state left behind the box | 37.02 |
| erased, one `update` per sample | 37.02 |
| erased, `&mut dyn FnMut` in and out | 46.03 |

Both negative rows matter. Batching **without** the local copy changes nothing —
which is why it looked like a dead end when measured twice before, at -1.2 and
+1.3. And the samples must arrive as a **slice**: routing them through a closure
is worse than doing nothing, because a closure's captures are themselves behind
a pointer, so the aliasing problem simply moves.

`feed()` chunks 128 samples through a stack buffer rather than handing over a
whole frame, since a whole-frame candle slice would mean rebuilding the 8 MB
`Vec<Candle>` that Phase 8 removed. That concedes 4.5 of the 21 and keeps the
allocation gone. Chunking also lifted the candle assembly into a tight loop, so
the frame paths gained more than the 16.5 the prototype predicted:

| instr/sample | before | after | |
|---|---:|---:|---:|
| `close()` | 53.15 | **29.50** | −44% |
| `sma(close())` | 86.16 | **50.38** | −42% |
| `ema` | 65.16 | **34.15** | −48% |
| `rsi` | 116.16 | **70.40** | −39% |
| `atr` | 95.81 | **49.37** | −48% |
| `sma(identity())`, 1-D | 54.11 | **39.75** | −27% |

**And unlike fusing, this is big enough for wall-clock to see** — which is the
whole reason it survives and fusing was nearly reverted. `py vs py`:

| | before | after |
|---|---:|---:|
| `sma` | 2.52× | **1.62×** |
| `ema` | 1.71× | **1.00×** — parity |
| `rsi` | 1.61× | **1.13×** |
| `atr` | 0.57× | **0.48×** |
| `stddev` | 3.37× | 3.23× |

Fusing stays because the two are complementary: a fused chain is one concrete
struct, so *all* of it promotes to registers, where an unfused chain leaves its
inner level behind a second pointer that `update_slice` cannot reach into.

The core addition is a **defaulted** method on `runtime::DynIndicator` — the
erasure vocabulary that exists to serve boundaries. No existing signature
changes, every current impl keeps compiling untouched, and a Rust caller who
never calls it sees nothing.

Correctness is the obvious risk, since chunking a stateful fold is exactly where
an off-by-one hides: `feed()` is verified exact against per-sample `update()` at
periods 3/14/127/128/129/200 — either side of the 128 boundary — for recursive
(`rsi`, `ema`) and multi-field (`atr`) state.

#### The methodological lesson, which is the expensive part

The change immediately before this one cut instructions per sample and made
wall-clock **worse**, and I spent real effort trying to reconcile that as a
contradiction. It is not one: **instruction counting is blind to page faults.**
A fault retires almost no instructions and burns thousands of cycles.
`close().feed(frame)` was running at an IPC of roughly 0.4 — the CPU was stalled,
not working — and no amount of `callgrind` would have said so.

That is why both instruments are kept, and why they disagree by design:

* `tools/icount_python.py` — deterministic, contention-immune, sees *work*.
  Use it for "did this change the amount of computing".
* `tools/bench_three_tier.py` — noisy, needs a quiet machine, sees *time*.
  Use it for "does this help a user".

The deeper miss is that none of it needed a profiler. Copying 8 MB per call, to
read data that was already contiguous and already in memory, is visibly wrong on
inspection. I ranked it third on the strength of an instruction count that said
5.1/sample, when reading the code should have ranked it first.

### The candle-frame input path (superseded)

Superseded by Phase 8, which found the copy that dominated it. Kept only as a
pointer so the number is not re-derived: `git log -S'24 ns/sample' docs/PERFORMANCE.md`.

### Where the Python time goes

Measured, not assumed, with `_bench_feed_stage` — a diagnostic hook, since
removed (see "Things that were tried"), that ran each prefix of the `feed()`
pipeline so every layer could be priced separately. One run, so the ratios are
internally consistent:

| | ns/sample | vs TA-Lib |
|---|---:|---:|
| `talib.SMA` | 2.03 | 1.0× |
| `feed` — input only | 0.32 | |
| `feed` — input + indicator (**one** erasure) | 7.31 | |
| `feed` — whole pipeline, one erasure | **8.15** | **4.0×** |
| `fz.sma(identity())` — two erasures | 50.14 | 24.7× |
| `fz.sma(ema(identity()))` — three erasures | 79.86 | 39.4× |

That decomposition is what identified the target, and it held up: the pipeline
was never the problem — end to end with one erased layer it was already 4.0× a
vectorised C library — while **each additional erased layer cost ~30 ns/sample**,
and the Python builders add one per call.

Phase 6 removed it. The same measurement now reads:

| | ns/sample | vs TA-Lib |
|---|---:|---:|
| `talib.SMA` | 2.11 | 1.0× |
| `fz.sma(identity())` — two levels | **6.43** | **3.0×** |
| `fz.sma(ema(identity()))` — three levels | **7.59** | **3.6×** |
| six levels | 17.80 | 8.4× |

Per level: **~30 ns → ~2.8 ns**. Construction was never the cost (0.009
ns/sample), input is not (0.32), output is not (0.75), and depth now costs about
what an indirect call costs.

### What was fixed here

| | before | after |
|---|---:|---:|
| input conversion | ~155 ns/element | 0.32 ns/sample |
| output conversion | 19.29 ns/sample | **0.75** |
| per erased level | ~30 ns/sample | **~2.8** |
| `feed(sma(identity))` overall | ~160 ns/sample | **~5** |

The input fix is the buffer-protocol path (and the reason for `abi3-py311`); the
output fix fills `np.empty`'s buffer directly instead of building a Python list
for NumPy to copy back out; the per-level fix is `runtime::Chain` (Phase 6).

### What `stddev` buys with its 2×

`cargo bench --bench stddev_tradeoff` re-derives both halves of the choice
`src/indicators/stats.rs` documents, so it rests on numbers rather than on a
comment. Relative error against a Kahan-compensated reference, period 20:

| mean | σ | centred (ours) | shortcut (TA-Lib's) |
|---:|---:|---:|---:|
| 1e2 | 1 | 1.3e-16 | 1.3e-11 |
| 1e2 | 0.01 | exact | 1.3e-7 |
| 1e5 | 100 | exact | 1.1e-10 |
| 1e5 | 0.01 | exact | **9.6e-3** |
| 1e9 | 1 | 2.4e-13 | **6e1** |
| 1e9 | 0.01 | 9.7e-10 | **1e0** |

Cost, same window: 12.29 ns/sample centred vs 3.62 shortcut — **3.39×** (it was
15.18 / 3.99× before the lanes; see Phase 10).

So the trade is ~10.7 ns/sample for a result that stays correct to ~1e-13 where
the shortcut is wrong by **6000%**. A five-figure instrument quoted to the cent
(1e5 / 0.01) already costs the shortcut 1% of its answer, and `ZScore` divides
by that. Keep the centred pass.

There is no free lunch in between: Welford's online algorithm is O(1) and far
better conditioned than the naive shortcut, but it has no numerically stable
*removal* step, which a sliding window needs on every sample.

## Phase 10 — multi-output indicators

The comparison had never covered an indicator that emits several lines. TA-Lib
fills every output array in one call and a fugazi multi-output `update` returns
the whole value struct, so that is the unit of work on both sides; `dmi`/`adx`
are deliberately asymmetric in *call count*, because TA-Lib has no combined entry
point and must re-derive the same Wilder-smoothed true range per line.

Current figures are in *The Python binding budget* below. What the phase changed:

**`WindowExtreme` was still a heap `VecDeque`.** Its monotonic deque can never
hold more than `period` entries — every entry is a distinct sample index inside
the window — so the capacity was known at construction all along, which is the
same argument that moved `WindowStats` to a fixed ring years earlier. `Aroon`
runs two of them: **17.31 → 10.72 ns/sample**, the largest single win here.
`Donchian`, `Stochastic`, `RollingMax`/`RollingMin` and `BarsSinceHigh`/`Low`
share the core. The conversion evicts the aged-out front *before* the dominated
tail; the passes are independent, and that order is what keeps the deque at
`period` entries rather than momentarily `period + 1`, which is what lets the
ring be exactly `period` slots.

**The centred dispersion pass runs on four accumulators** (`LANES`), reducing the
*full ring* when the window is full rather than the two halves `slices()`
returns — at period 10 the split halves put six of ten samples in the scalar
remainder and the lanes bought nothing. **15.18 → 12.29 ns/sample at period 20.**
Not bit-identical to a single running total and cannot be (floats do not
reassociate); it is the better-conditioned arrangement, and
`variance_is_exact_at_market_scale` pins it against a two-pass reference.

**`Bollinger` asked for the mean twice** — `stats.mean()` then `stats.stddev()`,
and `stddev` recomputes the mean to centre on it. `mean_and_stddev` /
`mean_and_variance` return the pair from one pass; `ZScore` and the Kelly sizer
had the same duplication.

### Two things measured and left alone

**`Indicator::value()` costs nothing worth a breaking change.** Every indicator
stores what it just returned, and multi-output ones store it per line.
`benches/breaking.rs` prototypes three MACDs over identical arithmetic:

| variant | ns/bar |
|---|---:|
| `no_store` — returns and keeps nothing (the ceiling) | 2.351 |
| `stored_lines` — the library's shape | 2.354 |
| `stored_lines` + a `value()` read every bar | 2.379 |

**+1.1% at the ceiling**, and that already includes a `value()` read per bar that
only the readiness walk performs. The stores are free because the loop is
latency-bound on dependent EMA recurrences. All five variants sit inside one
noise band, so the ordering means nothing beyond that.

**`Shared` is a pessimisation for cheap sources.** Two MACD lines: 1.64
ns/sample as two independent `Component`s (what `src/spec/expr.rs` builds),
21.88 off a `.shared()` handle. The cell is an `Arc<Mutex<_>>`, so each accessor
pays an uncontended lock per bar — ~12 ns — and the mutex is not optional, since
`Clone` on the handle is an `Arc::clone` and a strategy cloned onto another
thread would otherwise race. **It pays only when the source costs more per bar
than the lock**, which no leaf indicator here does. Both workloads stay in
`benches/three_tier.rs`.

## Phase 11 — the run-metrics reduction

`spec::metrics::from_report` reduces one `RunReport` to the `metrics.yml`
document. Once per run that is invisible; `optimize` pays it **once per grid row
per fold**, and `rolling_from_report` pays a slice of it once per bar. Phase F8
(above) had already taken it from 22.6 ms to 9.4 ms at 200 000 bars by sorting
the return series once instead of four times. This is what was left.

### What it was actually spending

Measured per piece, 200 000 bars, quiet machine:

| piece | cost | share |
|---|---:|---:|
| gathering the return-series numbers | 4.10 ms | 43% |
| one sort of the return series | 3.76 ms | 39% |
| walking the equity curve (returns + drawdown segments + ulcer) | 0.93 ms | 10% |
| everything else (trades, exposure, building the document) | ~0.8 ms | 8% |

**The sort was not the biggest cost — recomputation was**, which is the opposite
of what the F8 note above predicted and the reason this phase exists. Every
public metric takes a bare slice, so nothing is shared between them: `sharpe`
derives the mean and then the mean and stddev again inside
`annualized_volatility`, `sortino` derives the mean twice more, `skewness` and
`kurtosis` each re-derive the mean *and* `Σ(x − mean)²` before their own moment,
and `probabilistic_sharpe` then calls all three of `sharpe` / `skewness` /
`kurtosis` a second time from scratch. Fourteen numbers, ~30 walks.

That is the right API for the module — every one of those is a single call from
Python — and the wrong one for a reducer that wants all fourteen at once.

### What changed

Three `pub(crate)` cores in `metrics`, gated on `spec` (their only caller):

- **`ReturnStats::of`** — every accumulator in two passes. Two, not one: the
  `E[X²] − E[X]²` shortcut cancels the leading digits and was wrong at crypto
  price scale (see `WindowStats`).
- **`quantile_reads`** — the four quantile reads need six order statistics and
  one tail mean, not a total order. `select_nth_unstable`, recursing into both
  partitions, is ~2n comparisons against ~n·log₂n. Permutes in place, so the
  1.6 MB copy goes too.
- **`TradeStats::of`** — the trade section in one walk instead of ~20, two of
  which allocated a `Vec` of filtered PnLs.

Plus three redundancies: `probabilistic_sharpe_from_stats` instead of rescanning,
`average_bars_held` / `min` / `max` asked once rather than twice, and
`ulcer_performance_index_with_ulcer` — the document emits both `ulcer_index` and
`ulcer_performance_index`, and the latter recomputed the former, a second full
walk of the equity curve.

Separately, **`report_slice` binary-searches the blotter** instead of filtering
it. A blotter is written as the run advances, so it is bar-ordered and two
`partition_point` calls give the range. That made `rolling_from_report`
O(bars × log fills) rather than O(bars × fills).

### Wall-clock

| benchmark | before | after | change |
|---|---:|---:|---:|
| `metrics/from_report/200000` | 9.570 ms | 2.82 ms | **−70%** |
| `metrics/from_report/100000` | 4.745 ms | 1.44 ms | −70% |
| `metrics/from_report/10000` | 391.4 µs | 137.8 µs | −65% |
| `metrics/report_slice` (one window) | 3.132 µs | 134.4 ns | **−96%** |
| rolling sweep, 50k bars, w=252, serial | 271.2 ms | 243.5 ms | −10.2% |

The last two rows are A/B'd **inside one binary** (`linear_filter` vs
`binary_search` in `benches/metrics.rs`), which is why they are quoted to more
significant figures than the rest: both sides share a build and a machine state,
so the ratio survives contention that the absolutes do not.

**The rolling row is the honest end-to-end number, and it is 10%, not 96%.** At a
252-bar window the reduction of each window dominates the slice that produced it.
The per-slice win only becomes the story when there are many cheap windows —
`rolling_from_report` reads 16.7 / 34.7 / 101.0 ms at windows 63 / 252 / 1000
over 50 000 bars.

### Instruction counts, and one hypothesis killed

The landed reduction measured ~28% above a prototype of the same algorithm, and
the obvious suspect was cross-crate codegen: `from_report` is generic over `Sym`
so it monomorphises into the *calling* crate, while the cores stay in `fugazi`'s
codegen unit. `#[inline]` on the three cores, measured with callgrind
(`icount metrics_reduction` minus `icount metrics_none`, which subtracts the
200 000-bar report construction):

| | net instructions | per bar |
|---|---:|---:|
| without `#[inline]` | 33,081,786 | 165.41 |
| with `#[inline]` | 31,875,481 | 159.38 |

**−3.65% — real, kept, and far too small to be the explanation.** Most of that
28% was the duplicated `ulcer_index` walk, found by reading rather than
measuring; the rest is inside the run-to-run spread. Which is the point of using
callgrind here: this machine's criterion spread on *untouched* code was ±8–17%
across the runs of this phase, so an effect of this size is invisible to
wall-clock and only a deterministic instrument can rule it in or out.

### Output is unchanged, bit-for-bit

Not "within tolerance" — identical, because none of these changes reorders an
accumulation. `metrics::tests::reduction_cores_match_public_metrics` pins all 14
return derivations across nine series (empty, one, two, zero-variance,
one-sided, both parities of a long noisy series) and all 21 trade derivations
across six vectors, comparing `to_bits`. `benches/metrics_variants.rs` carries
the same check end to end against the pre-change call sequence, and
`report_slice_matches_a_linear_filter_on_every_range` covers all 45 ranges over
a deliberately awkward blotter.

Two details that requirement forced, both of which would otherwise have shipped
as silent one-ULP drift:

- **`Iterator::sum::<f64>()` folds from `-0.0`**, the additive identity f64
  actually has, and returns it verbatim on an empty iterator. `profit_factor` on
  a run with no winning trade is `Some(-0.0)`; a hand-rolled accumulator seeded
  `0.0` answers `Some(0.0)`.
- **`value_at_risk` / `conditional_value_at_risk` derive their tail as
  `1.0 - confidence`** = `0.050000000000000044`, while `tail_ratio` writes `0.05`
  as a literal. Reproduced exactly here; *fixed* in Phase 12 below, which
  corrects what this note originally claimed about it.

### What was measured and rejected

**Fusing the three equity-curve walks into one: 925.6 µs → 926.7 µs.** Flat. The
pass is store-bound — `per_bar_returns` writes 1.6 MB and `drawdown_segments`
grows a `Vec` — so removing reads of a buffer that is already resident buys
nothing. The remaining 0.93 ms is attacked by allocating less, not by fusing
loops. `benches/metrics_variants.rs` keeps the variant so this stays measured
rather than re-argued.

**`sort_unstable_by` instead of the selection machinery**: 3.76 ms → 2.80 ms,
against 0.70 ms for `select_nth_unstable`. A one-token change worth a quarter of
the sort, kept in the variants bench as the fallback if the ~80 lines of
introselect ever stop earning their place.

## Phase 12 — a 4e-17 input moving a metric by 2e-5

Not a performance change; found while reading the reduction for one, and fixed
because the reduction's bit-identity guarantee made it impossible to leave
ambiguous.

### The bug

`value_at_risk` and `conditional_value_at_risk` take a **confidence** and derive
the tail as `1.0 - confidence`. `tail_ratio` writes its `0.05` as a **literal**.
`1.0 - 0.95` is `0.050000000000000044`; the literal is
`0.05000000000000000277`. They are 4.4e-17 apart, and that gap is not
recoverable — by Sterbenz's lemma the subtraction is *exact*, so the error is
already in `fl(0.95)` before any arithmetic happens.

That difference should have been invisible. It was not, because `tail_mean`
sized the CVaR tail with `ceil(n·p)`, which is discontinuous exactly where `n·p`
is an integer — and at `p = 0.05` that is every `n` divisible by 20, which is
every round bar count anyone runs. At 10 000 samples the two spellings gave a
501- and a 500-element tail: **a ~2e-5 move in a published metric, off 4e-17 of
input.**

### The fix, and why this formula

The tail is now `floor(p·(n − 1)) + 1` — the `p`-quantile's lower order
statistic, inclusive.

| | at `n = 10 000`, `p = 0.05` | distance to the discontinuity |
|---|---:|---:|
| `ceil(n·p)` | 500.0000000000000277 | **~3e-14 — sitting on it** |
| `floor(p·(n−1)) + 1` | 499.95 | 0.05 of an index unit |

Three reasons, any one sufficient:

- **It is already the crate's convention.** `p·(n − 1)` is the R type-7 index
  base `quantile_of_sorted` uses. `ceil(n·p)` was a *second* quantile convention
  living in a private helper, which CLAUDE.md explicitly forbids.
- **It is well-conditioned where the old one was not**, and structurally so:
  `(n − 1)·p` lands near `.95` precisely when `n·p` lands on an integer.
- **It is what the reference computes.** `empyrical.conditional_value_at_risk`
  is `int((n - 1) * cutoff)` then the mean of the first `cutoff_index + 1`. The
  match is now exact rather than approximate.

Both spellings now produce the same tail, and what remains between `var_95` and
`tail_ratio`'s 5th percentile is one ULP of interpolation weight — the right
size for a disagreement of this kind.

### Why nothing went red

`tests/data/metrics_returns.csv` is **252 returns, and at `n = 252` every
candidate formula and both spellings agree on 13**. The cross-validation suite
could not have caught this at any tolerance. `metrics::tests::
the_tail_cutoff_is_spelling_independent_and_matches_empyrical` covers the sizes
that do diverge (1 000 · 2 000 · 10 000 · 20 000 · 100 000 · 200 000), encoding
the reference formula directly so it needs no reference library — and it asserts
that each of those sizes *still exercises the old bug*, so a future edit cannot
quietly pick sizes that prove nothing.

Widening the fixture instead would mean regenerating `metrics_returns.csv`, which
re-baselines every metric in the file to fix one. The Rust test is the cheaper
guard and the stricter one.

## Phase 13 — the windows that never got the ring

Trick #3 records `WindowStats` moving from a `VecDeque` to a hand-rolled fixed
ring, worth `Sma` 5.25 → 1.38 ns/sample. `WindowExtreme` got the same treatment
later. **Six other fixed-capacity windows never did**, and nothing in the
codebase pointed at them — the trick was written up as a fact about
`WindowStats` rather than as a pattern with unconverted instances.

They were found by asking a different question from the one Phases 1–12 asked.
Those measured *instruction counts* and *allocation counts*, both of which are
close to exhausted here. This one asked about **memory layout**, an axis this
document had not covered at all: a `grep -iE 'cache line|structure of
arrays|smallvec|prefetch'` over it returned nothing before this section.

### What was converted

A generic `Ring<T>` (`src/indicators/stats.rs`) now backs four of the six:

| core | was | backs |
|---|---|---|
| `Lookback` (`ops.rs`) | `VecDeque<Option<Real>>` | `Lag` / `Diff` / `Ratio` — `.lag()`, `.diff()`, `.ratio()`, `!lookback` |
| `WmaState` | `VecDeque<Real>` | `Wma`, and **three at a time** inside `Hma` |
| `WindowCovariance` | `VecDeque<(Real, Real)>` | `Correlation`, rolling beta |
| `Vwap` | `VecDeque<(Real, Real)>` | `Vwap` |

`WindowStats` and `WindowExtreme` are **not** expressed in terms of `Ring` and
should not be. Each carries a hand-rolled ring tuned to an access pattern the
generic one does not have: `WindowStats` needs the whole buffer as *one*
contiguous run when full (splitting a period-10 window into two halves put six
of ten samples in the scalar remainder and the `LANES` accumulators bought
nothing), and `WindowExtreme` pushes and pops at both ends to stay monotonic.

**Two were deliberately left alone**, and this is the note that says why rather
than leaving them to look like oversights. `WindowQuantile`'s update is already
O(period) from the sorted `Vec`'s insert-and-remove memmove, and
`VarianceRatio` recomputes its statistic from scratch on every full window — in
both the deque is a small fraction of an update that is dominated by something
else. The two `trailing.rs` windows (`MaxDrawdown`, `Calmar`) drive a whole
`Strategy` per bar, next to which the deque is a rounding error.

### Results

Callgrind, instructions per sample, net of a control workload the change cannot
touch (`cargo bench --bench window_ring -- <workload>`):

| workload | before | after | |
|---|---:|---:|---:|
| `diff_1` | 49.63 | **7.62** | −84.6% |
| `wma_14` | 65.62 | **26.64** | −59.4% |
| `hma_14` (3 WMAs) | 200.58 | **121.62** | −39.4% |
| `correlation_20` | 407.18 | **303.83** | −25.4% |
| `vwap_20` | 144.63 | **129.64** | −10.4% |
| `control` | 1 719 255 | 1 719 302 | +0.003% |

The control is the reading that makes the rest trustworthy: 47 instructions out
of 1.72 M, on a workload the change cannot reach.

`diff_1` is the largest because its element type was wrong too. `Option<Real>`
is **16 bytes with no niche** (`Option<bool>` is 1), so a period-20 buffer held
336 bytes of storage for 168 bytes of data.

Wall-clock, same change, same machine:

| | before | after | |
|---|---:|---:|---|
| `wma_14` | 4.66 | 3.01 | 1.55× |
| `hma_14` | 16.94 | 7.10 | 2.39× |
| `diff_1` | 3.80 | 0.68 | 5.59× |
| `vwap_20` | 21.40 | 23.45 | **0.91×** |
| `correlation_20` | 23.65 | 26.13 | **0.91×** |

### The instrument failed on two rows, and the doc predicted how

**Wall-clock said `vwap` and `correlation` got ~10% slower. Callgrind says they
do 10.4% and 25.4% less work.** The wall-clock reading was code layout, not
work — and the tell was available without callgrind: the *control* moved 13%
between the two runs (0.82 → 0.71 ns/sample), so the instrument's own drift was
larger than the effect being read off it.

This is trap 6 firing exactly as written — "below ~25%, or when a delta moves
between runs, use callgrind" — on a change where three of the five rows were far
enough above the band to be read straight off the wall-clock. **A run can be
trustworthy for some of its rows and not others**, and the noise floor is a
property of the size of the delta, not of the run.

A second trap fired in the same session, cheaply. The prototype block's absolute
numbers moved between two runs of `window_ring` (lookback period 14 read
B/A = 0.77, then 0.43) because a shipped-indicator section had been *added to
the same file* in between. That is trap 11, and the defence held: the
shipped-indicator rows were identical in both runs and stayed comparable, while
the prototype rows were treated as a fresh baseline rather than as a comparison.

### Format preservation, and the derive role it needed

Every window here is part of the run-state blob, and the wire format is a bare
array in logical (oldest-first) order. A `Ring` **cannot restore itself from
one**: the array does not record the capacity, and a window saved mid-warm-up is
shorter than its period, so a plain `Deserialize` would silently restore it at
the wrong size and the resumed run would diverge for the rest of its life.

For the two serde-derived cores the fix is the pattern trick #4 already
established — a hand-written `Serialize`/`Deserialize` pair over a `*Repr`
shadow struct that reads the enclosing `period`.

For the two `#[derive(SaveState)]` cores it needed something new, because that
derive *assigns* fields wholesale. Rather than hand-write ~35 lines of JSON
plumbing twice, `fugazi-derive` grew a fourth field role:

```rust
#[state(window)]
buffer: Ring<Option<Real>>,
```

which saves like ordinary state and loads through `stats::LoadWindow`, taking
the capacity from the **destination** — sound because the run-state contract is
already that the structure is rebuilt from the spec first and only values are
replayed in.

Pinned by `the_pre_ring_wire_format_still_loads`,
`the_pre_ring_lookback_state_still_resumes`, `the_pre_ring_vwap_state_still_resumes`
and `a_lookback_window_holding_none_round_trips`, each of which carries a
**literal blob in the pre-conversion shape** rather than a round trip of the new
code — a round trip would pass just as happily against a format that had moved.

Every conversion is bit-identical, which is why no fixture moved. `Ring::push`
evicts and returns the oldest in one operation, so the arithmetic order is
unchanged from the `pop_front`/`push_back` pair it replaces. `WindowCovariance`'s
centred pass now reduces the window's two contiguous halves into the *same three
accumulators* instead of driving a chained iterator — order preserved, so still
bit-identical. Summing the halves *separately* and adding would not be; that is
the trap recorded under `WindowStats::variance`.

### What is still on this axis

Measured but not taken, in descending confidence:

* **Splitting `Snapshot` into tags and atoms — tried, measured, reverted.**
  See below; it is the one item on this list that has been settled, and it was
  settled against.
* **`WilderState` divides once per bar, and `Adx` holds four of them.** `update`
  is `(prev * (p - 1.0) + input) / p` with `p = self.period as Real` — an
  int→float conversion and a genuine `divsd` every bar, which LLVM cannot
  strength-reduce without fast-math. Precomputing `(p-1)/p` and `1/p` makes it
  one FMA. `Adx<Identity<Atom>>` is **360 bytes** — 5.6 cache lines — and its
  three `Dmi` smoothers store `period` identically three times while `seen`/`sum`
  (16 bytes each) are dead forever after warm-up. **Not bit-identical**, so it
  moves the TA-Lib fixture: an asserted-divergence judgment, not a free win.
* **`WindowStats` keeps its window in a separate heap block** (`Box<[Real]>`), so
  every dispersion read scans a block scattered away from the struct that owns
  it, and an `optimize` sweep interleaves hundreds of them in allocation order.
  Inline storage would make `Sma` ~300 bytes and could cost more in tree-walk
  locality than it recovers — prototype in `benches/breaking.rs` before
  believing either direction.
* **`Option<Real>` as the universal per-level currency** — 16 bytes with no
  niche, stored and returned by every Real-valued level. The textbook answer is a
  NaN sentinel, which is also branchless. **Recorded as rejected, not as
  available**: the blast radius exceeds breaking-candidate #1 (~60 indicators,
  `fugazi-derive`, `runtime`, all five strategy shapes, `python/src`), it
  collides with the crate's existing NaN conventions (`max_finite`,
  `Atom::is_priceable`, the wallet's `close <= 0.0` guards), and it cannot cover
  bool outputs. A register-passed `Some(x)` is already cheap; the win is a store
  width, not a branch.

### The `Snapshot` tag/atom split — a correct number about a bottleneck that did not exist

Recorded at length because the arithmetic behind it is right, is easy to
re-derive, and is **not a reason to do this**.

`Snapshot::find` runs once per `!pick`-rooted leaf per symbol per bar and reads
only the `(symbol, freq)` tag. Stored interleaved, `Entry<Symbol>` is **112
bytes** and the tag is the first **24**, so the scan pulls in 4.7× more cache
than it looks at — 112 cache lines at a 64-symbol universe where 24 would do.
Splitting the storage into a tags vector and an atoms vector was implemented in
full, kept every invariant that blocked breaking-candidate #2 (predicate not key,
no `Eq + Hash`, duplicates legal, first-match-wins, `Snapshot` still 8 bytes,
clone still a refcount bump), and passed all 50 test binaries.

It is **slower**, and not marginally:

| | interleaved | split | |
|---|---:|---:|---:|
| `find_2` (instructions) | 705 802 | 758 358 | +7.4% |
| `find_16` | 11 324 334 | 12 933 141 | +14.2% |
| `find_64` | 153 056 749 | 174 875 611 | +14.3% |
| `multi_asset/drive/64` (wall-clock) | 79.5 ms | 92.3 ms | +16.1% |
| `control` | 914 337 | 914 351 | +0.002% |

**The cache-line ratio was arithmetic about a bottleneck that does not exist at
any universe size this crate targets.** `cargo bench --bench snapshot_scan`
prints the number that kills it in its own column: at N = 64 the whole
interleaved array is **7 168 bytes**. It fits in L1 several times over and is
touched every bar, so there was never a miss to save — and the split then paid
for a second allocation, a second pointer chase and a second live cache stream
to halve a footprint that was already resident.

The instruction count says the rest. `find_map` over interleaved entries returns
the matching reference directly; `position()` over tags followed by an indexed
fetch adds index bookkeeping to every element scanned and a bounds check to
every call. That is where the 14% went, and no amount of tuning the split
recovers it, because the thing it buys cannot be collected.

**The lesson generalises past this change, and it is the one worth keeping: a
static layout ratio is not a prediction.** Bytes-touched-versus-bytes-needed
says what a scan *could* save if it were memory-bound. Whether it is memory-bound
is a separate question, answered by the working set against the cache — and
that check costs one multiplication. It was not done here, and 4.7× read as a
forecast for most of a day.

Note also which instrument would have misled: wall-clock reported +1.7% at N = 2
rising to +16.1% at N = 64, all `p = 0.00`. Under trap 6 those are inside the
band where layout is not separable from work, so they could not have closed the
question on their own — but the *shape* was the tell. **A layout artifact does
not scale monotonically with a runtime parameter.** A delta that tracks N is
tracking work, and callgrind then said so outright.

### Where `Snapshot::find` actually spends, and why the index table is the wrong fix

The revert above raised the obvious next question — if not the layout, what? —
and the answer changes which of breaking-candidate #2's options is worth taking.

`cargo bench --bench snapshot_scan` decomposes the per-entry cost. Every row
drives the **same loop shape** (iterate a pre-built `Vec` of 64 distinct
selectors) and differs only in scan length and in what the comparison must do:

| workload | instr/lookup | entries scanned | instr/entry |
|---|---:|---:|---:|
| `miss_freq` — freq-only query, no string work | 397.5 | 64 | 6.21 |
| `miss_ragged` — symbols rejected on length | 554.2 | 64 | 8.66 |
| `miss_prebuilt` — equal-length symbols | 2 323.1 | 64 | **36.30** |
| `hit_prebuilt` — equal-length, hits at its own index | 1 183.5 | 32.5 | **36.42** |

`miss_prebuilt` and `hit_prebuilt` reach the same per-entry cost from *different
scan lengths*, which is the cross-check that the model is right. So:

| | instr/entry | |
|---|---:|---|
| the scan itself (iterate, compare `Option<Frequency>`) | 6.21 | |
| symbol comparison rejected by the **length** check | +2.45 | |
| **`memcmp`, when the lengths are equal** | **+27.64** | **76% of `find`** |

**Three quarters of `Snapshot::find` is `memcmp` on equal-length symbols**, and
real universes are overwhelmingly equal-length — `BTCUSDT`/`ETHUSDT`/`SOLUSDT`,
`AAPL`/`MSFT`. `Arc<str>` compares by *content*: the fat pointer carries the
length inline so a length mismatch is free, but equal lengths force a deref and a
byte compare. std's `Arc` specialisation short-circuits on pointer equality, so
the entry that *matches* is cheap; it is the `N − 1` that do not which pay.

End to end, on the same `MultiAssetStrategy` drive `benches/multi_asset.rs` runs,
at N = 64 over two universes differing **only** in whether the symbols share a
length — no library change on either side:

| | instructions | per symbol-bar |
|---|---:|---:|
| equal-length symbols | 137 108 116 | 7 141 |
| ragged-length symbols | 72 583 332 | 3 780 |
| | **1.89×** | |

**`memcmp` is 47% of a 64-symbol backtest.** Two independent paths agree: 47%
measured directly, and 50% derived from the table above (`find` is 66% of the
drive × 76% of `find`). It is if anything an *under*-estimate — the ragged
universe is penalised on the other side, since the wallet's `HashMap<Symbol, _>`
hashes 32-character keys where the equal-length one hashes 4.

**This reranks the two options in breaking-candidate #2.** The lazily-built
`symbol → first index` side table attacks the scan *length*, needs
`Sym: Eq + Hash`, breaks the documented predicate-not-key invariant — and still
has to hash the symbol's bytes, which is a pass over exactly the data `memcmp`
was reading. Making the *comparison* cheap attacks 76% of the cost with no
change to the O(N), no new storage layout, and no invariant: comparing a
fingerprint first is a sound **negative** filter, because a mismatch proves
inequality and a match falls through to the real compare.

### The fix, and the two places it did *not* belong

Taken, in 0.65.0. What matters is where it ended up, because the first two
attempts were worse than the problem.

**Not on `Snapshot`.** The obvious shape is a fingerprint stored inline in the
tag, compared before the bytes. It works, and it needs `Sym: SymbolKey` on
`find` and `push` — a bound that propagates into **`indicator.rs` (34 sites)
and `indicators/ext.rs` (56)**, the crate's most central generic surfaces. That
would permanently couple the indicator layer to "symbols are strings" in
exchange for a perf fix, and contradict `snapshot.rs`'s standing promise that a
pure-Rust caller may use any `Sym`. Built, compiled, measured for blast radius,
discarded.

**Not pointer equality.** Cheaper still, and unsound — see [`Symbol`]'s own docs.
`symbol()` is `Arc::from`, a fresh allocation per call, so `symbol("BTC")` twice
gives two pointers. The selector's symbol is interned when the spec is built and
the snapshot's when data is loaded, so pointer equality would match nothing and
the run would report a plausible **zero-fill backtest**.

**On `Symbol` itself.** It stopped being `pub type Symbol = Arc<str>` and became
a newtype carrying a precomputed FxHash of its own bytes, with `PartialEq`
comparing the hash first. That needs **no bound anywhere** — every existing
`Sym: PartialEq` call site gets it for free — and it reaches further than the
`Snapshot` version could: the wallet's `HashMap` collision checks, `Position`
lookups and `marked_equity`'s sort are all comparisons too.

| | before | after | |
|---|---:|---:|---:|
| `miss_prebuilt` (instr/entry) | 36.30 | **8.35** | −77% |
| `hit_prebuilt` (instr/entry) | 36.42 | **9.43** | −74% |
| `miss_freq` — no string work, the control | 6.21 | 6.24 | +0.5% |
| `drive_equal` (64-symbol backtest) | 137 108 116 | **67 076 273** | **2.04×** |
| `multi_asset/drive/64` (wall-clock) | 79.5 ms | **41.1 ms** | **−48.3%** |

Two tells that it landed cleanly. `miss_freq` — the workload that walks the same
entries doing no string comparison — is unchanged, so nothing but the comparison
moved. And `drive_equal` (67.1 M) is now **cheaper** than `drive_ragged`
(73.7 M): the equal-length universe went from the penalised case to the cheap
one, because ragged symbols are longer and so cost more to hash once at
interning. The wall-clock delta scales monotonically with the universe — −8.4% at
N = 2, −16.8% at 16, −39.4% at 32, −48.3% at 64 — which is the shape a
per-symbol-scan cost has, and the reason to believe it over a flat shift.

2.04× exceeds the 1.89× ceiling measured from the equal-vs-ragged gap, and that
is the point: the gap only priced what `find` was paying, and the newtype makes
every `Symbol` comparison in the crate cheap.

**What it costs.** `Symbol` is three words rather than two. `tests/perf_guard.rs`
asserted two, and that guard was right to fire — but it was asserting a *proxy*.
The invariant behind it is that cloning a symbol allocates nothing, so the guard
now asserts **that**, directly, with the counting allocator it already had. A
width check would have had to be relaxed on every future change; the allocation
check does not.

`Hash` still delegates to the bytes rather than to the cached hash, and must:
`Borrow<str>` requires an owned symbol to hash exactly as the `&str` it is looked
up with, and the crate documents that `HashMap<Symbol, _>` stays `&str`-queryable.
So this buys cheap **equality**, not cheap hashing. The remaining
`symbol → index` side table is still open and now clearly second: after this,
what is left of `find` is the 6.24 instr/entry of pure scan.

### What the 64-symbol profile looks like now

Re-profiled after the above, because removing 47% of a run reorders everything
under it. `cargo bench --bench snapshot_scan -- drive_index0` is `drive_equal`
in every respect — same 64 symbols, same four `Pick` leaves per symbol-bar, same
arithmetic — except every leaf reads the symbol at index 0, so each `find` stops
after one entry instead of scanning 32.5. The gap is the scan, priced exactly:

| | instructions | per symbol-bar |
|---|---:|---:|
| `drive_equal` | 67 322 725 | 3 506 |
| `drive_index0` | 47 097 064 | 2 453 |
| **the scan** | **20 225 661** | **1 053 — 30% of the run** |

So the `symbol → index` side table is worth **1.43×** on top of 0.65.0, and
**2.91× cumulative** against 0.64.0. It is now the largest identified item, and
it still needs the design decision breaking-candidate #2 always needed.

Underneath it (`drive_index0`, callgrind with `--separate-callers=1`):

| | share |
|---|---:|
| `backtest::drive` | 30.2% |
| `Sma::update` — the whole inlined leaf chain, `Pick` + `Close` + window | 28.5% |
| `HashMap::insert'drive` | 5.0% |
| `hash_one'drive` | 3.9% |
| `memcmp'drive` | 3.1% |
| `memcmp'Sma::update` | 3.1% |
| `sip::Hasher::write'hash_one` | **2.6%** |
| `HashMap::get'drive` | 1.7% |

**SipHash should not appear in this profile at all**, and finding out where it
came from is the next item on the list:

### The SipHash in the driver — and a wrong attribution, corrected

SipHash has no business in this profile: the crate carries its own FxHash for
symbol-keyed maps (`src/hash.rs`, trick #6) precisely because a symbol map is
touched several times per bar per symbol. Two maps had been missed.

**The first attribution was wrong, and is left here rather than quietly fixed.**
`--separate-callers=1` put `hash_one` inside `backtest::drive`, and the obvious
candidate was `Book::update`, which does
`s.legs.entry(sym).or_insert_with(LegState::flat)` once per symbol per bar on a
default-`RandomState` map. That is a genuine defect — two of this document's own
tricks un-applied in one function, #6 (SipHash on a symbol map) and #9 (`entry`
takes the key **by value**, so it cloned a symbol per symbol per bar for a slot
present after the first) — and `Book::update_one` twelve lines above already did
both correctly. Fixing it was worth **−2.4% / −3.4%**.

It was not the SipHash. The profile still read 2.61% after the fix, essentially
unmoved. Everything above `drive` is inlined, so deeper `--separate-callers`
buys nothing: the chain just reads `hash_one'drive'main`. The lesson is narrow
and repeatable — **caller attribution stops at the inlining boundary, and a
plausible callee inside that boundary is a hypothesis, not a result.** The check
that settled it cost one run: make the change, and see whether the symbol
actually leaves the profile.

The real source was `MultiAssetStrategy`'s `states: HashMap<Sym,
PerAssetState<Sym>>`, whose per-bar discovery filter does
`!self.states.contains_key(s)` **once per symbol per bar** — and which sits three
lines above `bar_candles: SymMap<Sym, Candle>`, correctly on FxHash, in the same
struct.

| stage | `drive_equal` | `drive_index0` |
|---|---:|---:|
| 0.65.0 | 67 322 725 | 47 097 064 |
| `Book::update` — FxHash + trick #9 | 65 702 574 (−2.4%) | 45 509 816 (−3.4%) |
| `MultiAssetStrategy::states` — FxHash | **63 046 899 (−6.4%)** | **42 862 706 (−9.0%)** |

SipHash no longer appears in the profile at all, which is the check that the
first fix lacked. Per symbol-bar: 3 506 → 3 284. Cumulative against the 0.64.0
baseline: **2.17×**.

**Two things to preserve.** `Book::remark` still sums in a canonical sorted
order, and must: FxHash is unseeded, so iteration order becomes *incidentally*
stable, and `src/hash.rs` is explicit that "the iteration order happens to be
stable" is a much weaker guarantee than "the sum has a canonical order". Do not
delete the sort on the strength of the hasher. And `states` is iterated to
advance each symbol's chains — that order was already arbitrary under
`RandomState`, so nothing could have depended on it; FxHash only makes the
arbitrary choice repeatable.

**Worth a grep, not a guess:** the remaining default-hasher maps are
`Schema::indexes` (`HashMap<String, usize>`, overlay column lookup) and
`BasketStrategy`'s five `HashMap<Sym, Chain<Sym>>`. Neither is exercised by this
workload, so neither is measured here — which is the reason to check rather than
assume they are fine.

### Closing Phase 13 — the non-breaking levers are exhausted

Where it finished, callgrind instructions net of control
(`cargo bench --bench snapshot_scan -- <workload>`):

| workload | 0.64.0 | now | | per symbol-bar |
|---|---:|---:|---:|---:|
| `drive_equal` — 64-symbol multi-asset | 137 108 116 | **63 865 706** | **2.15×** | 3 326 |
| `drive_basket` — 64-symbol basket | 92 874 542 † | **77 777 695** | **1.19×** † | 4 051 |
| `drive_index0` — the same, scan removed | — | 43 680 976 | — | 2 275 |

† the basket baseline was taken *after* the `Symbol` change, so 1.19× is the
basket-specific gain on top of it, not the total since 0.64.0.

What was taken, in order: the `Ring<T>` conversions; `Symbol` carrying its hash;
`Book::update`, `MultiAssetStrategy::states` and eight of `BasketStrategy`'s nine
maps off SipHash; and `ranked_take` partitioning instead of sorting.

**Four things were measured and *not* taken.** Each is recorded so the next
person spends the machine time on something else:

* **Splitting `Snapshot` into tags and atoms** — 14% slower. The cache-line ratio
  was right and irrelevant; see above.
* **Inline storage for `WindowStats`' window** (`[Real; 32]` in the struct rather
  than a `Box<[Real]>` beside it) — **22.8% worse** at period 20
  (`window_ring -- win_heap` vs `win_inline`, both shapes in one binary). Same
  lesson as the split, from the other direction: the block is L1-resident either
  way, so the indirection it removes was free, while the fatter struct is not.
* **Converting the last four `VecDeque` windows.** Phase 13 skipped
  `WindowQuantile`, `VarianceRatio` and the two `trailing.rs` windows on the
  argument that their updates are already O(period). Now measured rather than
  argued: in `percentile_100` the deque is a slice of `WindowQuantile::update`'s
  42.5%, while **`memcpy` alone is 22.7%** — the sorted `Vec`'s two memmoves, not
  the deque. The argument held.
* **Fusing those two memmoves.** `WindowQuantile::update` inserts the new sample
  and then removes the evicted one, each a memmove averaging `period/2`. One
  fused shift of `|insert_pos − remove_pos|` slots would replace both, and for a
  random walk those ranks are usually adjacent. **Declined on risk, not on
  size.** `cmp_asc` makes `NaN` compare `Equal` to everything, so
  `binary_search_by` may legitimately match a `NaN` when searching for an
  ordinary value — and today's insert happens *before* the search, so the search
  runs against a vector that already contains the new sample. Reproducing that
  index exactly without materialising the intermediate vector means emulating
  `binary_search_by` under a non-total comparator, which is implementation-
  defined. It is one indicator, already only ~3× an `Sma`, and this file already
  warns that green tests are not evidence for a change of this class.

**What is left all breaks compatibility**, which is why this phase stops here:

| lever | what it would cost |
|---|---|
| `symbol → index` side table (30% of a 64-symbol run, ~1.43×) | `Sym: Eq + Hash` on `find`, which propagates into `indicator.rs` and `ext.rs` — the same ripple measured for `SymbolKey` |
| `latest_score` / `Sides` off SipHash (the residual 6.2% of the basket profile) | `Selection::select(&self, &HashMap<Sym, Real>)` is a **public trait**; narrowing the hasher breaks every caller-written impl |
| `Indicator::update(&mut self, &Self::Input)` (breaking candidate #1) | ~60 indicators, `fugazi-derive`, `runtime`, five strategy shapes, `python/src` |
| `WilderState` precomputing `1/p` and `(p−1)/p` into one FMA | not bit-identical, so it moves the TA-Lib fixture — an asserted-divergence decision |
| `Option<Real>` → a `NaN` sentinel | rejected above on blast radius and on collision with the crate's own `NaN` conventions |

## The Python binding budget — 1.25×, with one exemption

**A fugazi indicator through the Python bindings must cost no more than 1.25×
the same indicator through `talib`, TA-Lib's own bindings.** That is the `py vs
py` column of the four-tier table: both sides cross a Python boundary, so it is
the comparison a Python user actually faces, and it is the only one a binding
change can move. `rs vs C` is a statement about the engine and is not covered by
this budget.

1.25× rather than parity because TA-Lib is vectorised and fugazi is incremental,
and the incremental design is bought deliberately — it is what lets the same code
drive a live stream. A quarter is what that is worth. Anything past it is a bug
in the binding, not a property of the design.

### Standing against the budget

From the converged run above:

| | `py vs py` | this morning | |
|---|---:|---:|---|
| `macd` | **0.27×** | 1.05× | passes — ~4× faster than `talib` |
| `atr` | 0.47× | 0.46× | passes |
| `ema` | 0.74× | 0.97× | passes |
| `dmi` | **0.78×** | 1.68× | passes — faster than `talib` |
| `rsi` | 1.05× | 1.15× | passes |
| `adx` | **1.12×** | 2.00× | passes |
| `sma` | **1.17×** | 1.59× | passes |
| `aroon` | 1.38× | 2.51× | **over** — the only one, and structural |
| `bbands` | 1.89× | 3.90× | **exempt** |
| `stddev` | 3.39× | 3.72× | **exempt** |

**The `was` column is not comparable and is kept only to show direction.** It
predates the harness fix below, which found that the two Python tiers were
perturbing each other through a shared heap — so every figure in it was measured
against a baseline that moved with fugazi's own allocation behaviour. The left
column is the first set taken with each tier in its own process.

Both columns are converged runs (19 passes and 27). The `was` column predates
the column-major fold and the column-at-a-time candle fill. **Nothing moved** —
see below, because that is the finding, not a null result to skip past.

### The exemption, and why it is one

`stddev` and everything whose cost is dominated by a `WindowStats` **dispersion**
read — `bbands` today, and `ZScore`, the trailing `Sharpe`/`Volatility`
indicators, `skewness`/`kurtosis` and the Kelly sizer if they are ever added to
the comparison — are exempt.

The exemption is about the **algorithm, not the binding**, and that is the whole
of its justification: the Rust engine alone is already 3.35× native TA-Lib on
`stddev`, before a single line of pyo3 runs. No amount of binding work reaches
1.25× from there, because the gap is the centred variance pass being chosen over
TA-Lib's cancelling O(1) shortcut — a correctness decision this document
re-derives from numbers in two places, most bluntly the 896-of-4981 windows where
`talib.STDDEV` returns exactly `0.0` on the benchmark's own price series.

So the exemption is not "this one is allowed to be slow". It is "this one's cost
is not the binding's to answer for". **If the variance algorithm is ever
revisited, the exemption goes with it** — the budget then applies to whatever the
new cost is.

An exemption is written here rather than assumed, for the same reason
`tests/metrics_coverage.rs` demands a written exemption for a metric with no
reference value: a ratio that is over budget and a ratio that is over budget *on
purpose* look identical in the table, and the difference is the entire point.

### How the multi-output path got there, in one place

Four changes and one measurement fix, in the order they landed:

| | effect |
|---|---|
| iterator scatter, then a **column-major fold** so the scatter is a copy | −18 to −22 instructions/sample |
| candle fields filled **a column at a time**, hoisting an enum match out of the per-sample path | frame reader 48.2 → 17.1 instructions/sample |
| **`write_strided`** — each value written once, straight to its slot | first change to move wall-clock: `macd` 1.05× → 0.81×, `dmi` 1.68× → 1.27× |
| **one `(lines, n)` allocation**, columns as views | NumPy's allocator caches one stable-sized buffer and thrashes on several: 10.70 → 1.61 ns/sample for three columns |
| **each Python tier in its own process** | the ratios before this are not comparable — see below |

Two findings from it are worth carrying forward.

**The boundary was memory-bound before it was instruction-bound.** The first
three changes removed 49-52 instructions/sample — a fifth of `dmi`'s total — and
converged wall-clock did not move at all. The fourth removed *stores* from a loop
the cache profile showed was store-bound, and moved wall-clock three to five
times more than it moved instructions. **Removing work only helps if it is the
work the loop is waiting on**, and the two instruments together are what tell you
which. This is the mirror image of trap 8.

**The output allocation is not fugazi's to fix, and `talib` pays it too.**
`talib.AROON` costs 7.84 ns/sample more than `talib.AROONOSC` purely for its
second output array — very nearly its entire Python wrapper cost. Returning a
*frame* rather than a tuple is what lets fugazi allocate once; the cost is that
the columns share ownership, documented on `Multi.feed`.

**The two Python tiers were perturbing each other through a shared heap.** They
ran in the driver's process, one after the other. Same data, same call, quiet
machine: `talib.MACD` costs **29.60 ns/sample in a fresh process and 7.51** in
one where fugazi has already run — a 4× swing in fixed C code behind a Cython
wrapper, caused entirely by what the other tier left in the allocator. That
coupled the `py vs py` column to fugazi's own allocation behaviour: change how
fugazi allocates and its baseline moves with it.

It surfaced as an *impossibility*, not a wrong-looking number — `talib` through
Python reading faster than the TA-Lib C library it calls. **That check is free
and worth making every time this table is read**: `talib_py` must exceed
`talib_C` in every row. Each tier now runs in its own interpreter.

The lesson generalises past this harness: **two implementations timed in one
process are not independent when both allocate.** The faster one can make the
slower one look good, and a change to either moves both. Fork, or accept that the
ratio measures the pair.

**`aroon`'s third column is not the answer, and the reason matters.** `Aroon`
emits `{up, down, oscillator}` where `TA_AROON` emits two, and `oscillator` is
derived (`up - down`). Prototyped and measured, converged: dropping it takes
`aroon` from 1.37× to **1.28×** — still over budget, in exchange for breaking a
documented output column. Not taken. The estimate beforehand was −3.5 ns and the
truth was −1.27, because **the `(lines, n)` allocation had already captured most
of it**: with one shared buffer the whole allocation is ~1.6 ns, so a third of it
is half a nanosecond. The two changes are *substitutes, not complements*, and the
second was measured first — which is the only reason the trade looked good on
paper. What remains in `aroon` is structural.

### Ruled out, so nobody re-tries them

Five hypotheses for the multi-output boundary, each plausible, each measured,
each wrong. Recorded because four of them were mine and every one looked
obvious enough to try again.

| hypothesis | measured | verdict |
|---|---|---|
| the per-chunk `self.clone()` in `update_slice_flat` | removing it: `aroon` 369.74 → 367.75 instructions/sample | **2 instructions.** No. |
| `Aroon`'s boundary is anomalous (208.8 vs `adx`'s 94.2) | its engine is data-dependent (1.43× between two input series; `adx` 1.02×) and the two instruments feed different series | **An instrument artifact.** No anomaly. |
| instruction count is the constraint | −49 to −52 instructions/sample across three changes; converged wall-clock unmoved | **Memory-bound.** No. |
| we read more input columns than TA-Lib (5 vs 2-3) | `atr` on a 5- vs 3-column frame: −0.09 ns/sample; `dmi` −2.13 | **Negligible.** No. |
| materialising a 40-byte `Candle` per bar, which TA-Lib never does | same SMA, candle root vs 1-D root: 3.44 vs 2.48 ns/sample | **0.95 ns.** No. |

**And the premise behind two of them was wrong.** The framing was "why do we pay
an allocator toll TA-Lib avoids?" — TA-Lib does not avoid it. On its own kernels,
same inputs and window, `talib.AROON` (2 output arrays) costs **7.84 ns/sample
more** than `talib.AROONOSC` (1 output array), which matches the +7.20 that raw
NumPy allocation costs going from one array to two. That toll is very nearly
TA-Lib's *entire* Python wrapper cost.

So the honest decomposition, on `macd`, where both emit three columns:

| | TA-Lib | fugazi |
|---|---:|---:|
| engine | 12.47 | 1.51 |
| total through Python | 21.66 | 22.68 |
| **boundary** | **9.19** | **21.17** |
| … output allocation (both pay) | ~8 | ~10 |
| … **its own work** | **~1** | **~11** |

TA-Lib's wrapper does about a nanosecond of work: one Cython call handing three
raw pointers to a C kernel. Ours does eleven. Roughly 6 ns of that is per extra
output column beyond the first and beyond allocation, and it is **not** any of
the five rows above. It remains unattributed.

The one measured, sizeable win left is on the allocation line, and it is an
advantage TA-Lib structurally cannot take: `talib.MACD` returns a tuple of
independent arrays, so it must allocate three; fugazi returns a *frame*, which
can be one `(lines, n)` buffer with the columns as views. Measured, that is
**10.70 → 1.61 ns/sample** for three columns. The cost is that the columns then
share a base, so keeping one retains all of them — 4.6 MB where a caller expects
1.5 MB, and `sys.getsizeof` reports 112 bytes while it does. That trade is a
judgement call, not a measurement, which is why it is written here rather than
taken.

### The iterator scatter — what it bought, and what it did not

`feed_into_columns` scattered its flat chunk into the output columns with an
indexed read, `flat[r * lines + j]`, bounds-checked once per element; the scalar
path has walked its output with `cells.next()` since Phase 8 for exactly that
reason. Both sides are iterators now.

Converged, it moved **`dmi` from 1.87× to 1.64×** and left `adx` (1.99 → 2.01)
and `aroon` (2.47 → 2.48) where they were. `dmi` writes two output columns and
the other two write three, so "fewer bounds checks" does not explain the split —
whatever the three-column path is paying, it is not this, and it is worth
understanding before the next change is attempted.

**The commit that landed this claimed more.** It quoted `adx` 2.01× → 1.66× and
`aroon` 2.33× → 2.07× from the decomposition probe — a single script, run once,
comparing against a `talib` baseline taken in the same script minutes apart on a
warm machine. The converged four-tier run says those two did not move. The probe
was not lying about its own numbers; it was measuring a different thing (one
process, one pass, no convergence) and being read as if it were the table. **A
figure from a diagnostic probe is a hypothesis. Only the converged run is a
result.**

`sma` is a different shape of problem: at 2.17 ns/sample against `talib`'s 1.40
the whole budget is 1.75 ns and the engine alone is 1.35, so the binding has
0.4 ns — about a nanosecond and a half of headroom in total. It is the one row
where the boundary cost is already near the floor and the target may simply be
unreachable without a batch entry point.

## Per-event latency — what the throughput numbers do not say

Every other figure in this document is amortised throughput: total time over
200 000 samples, divided. That is right for a backtest, which is a tight loop
over a warm cache. It is the wrong measure for a live stream, where a bar
arrives, is handled, and nothing runs until the next one — and between events the
i-cache, branch predictors and TLB go cold.

`cargo bench --bench latency` measures the other thing: one `update` per timed
sample, `sleep(gap)` before each cold one, reported as percentiles. 20 000
events, 1 ms gap, times in ns including one `Instant` bracket:

| | warm p50 | warm p99.9 | cold p50 | cold p99.9 |
|---|---:|---:|---:|---:|
| `timer` (the floor) | 20.0 | 31.0 | 70.0 | 211.0 |
| `sma` | 30.0 | 81.0 | 190.0 | 491.0 |
| `atr` | 30.0 | 81.0 | 191.0 | 591.0 |
| `macd` | 30.0 | 90.0 | 190.0 | 411.0 |
| `rsi` | 40.0 | 101.0 | 211.0 | 561.0 |
| `sma_erased` | 31.0 | 81.0 | 201.0 | 602.0 |

**The amortised numbers overstate live per-event cost by roughly an order of
magnitude.** `Sma::update` is 1.4 ns/sample in the four-tier table; the first
update after a 1 ms gap costs ~190 ns at p50 and ~490 at p99.9. Even against the
cold timer floor of 70 ns, that is ~120 ns of real work for an operation the
throughput benchmark bills at 1.4. Nothing is wrong with either number — they
answer different questions, and only one of them is about a live stream.

Two things the harness has to say about itself, both of which shape how it is
read:

* **The clock is bigger than the thing being timed.** `Instant::now()` is ~20 ns
  and an `Sma::update` ~1.4, so the `timer` row is the instrument's own noise
  floor and is printed first. Rows whose warm p50 is not clear of it are marked
  `NO` in a `resolved?` column rather than being allowed to read as results —
  the same discipline `tools/icount_python.py` needed after it reported a
  negative instruction count.
* **The clock goes cold too**: 20 ns warm, **70 cold**, p99.9 of 211. So a cold
  row carries a more expensive bracket than its warm twin, and the cold/warm
  ratio does *not* fully cancel. `timer` is therefore repeated in the ratio
  table — 3.5× at p50 — and every other ratio (5.3–6.5× at p50) should be read
  against it, not alone.

`max` is deliberately not reported as a ratio. It is one observation, and a warm
run that happened to be preempted once yields a warm max above the cold max: the
first version printed 0.0× and 308× on the same run.

**What this does not yet cover**, and would need to before any latency claim is
made: a whole strategy step rather than one indicator, the wallet path, and
percentiles under a realistic event *arrival* distribution rather than a fixed
sleep. It also measures a process that is otherwise idle, which flatters it —
a real system has other work between events keeping some of the cache warm.

## The tricks in the codebase, and why they are there

Each of these looks like an odd way to write the code until you know what it is
avoiding. If you are about to "simplify" one, this is the note that tells you
what it will cost. Every entry was measured, and the measurement is repeatable
with the benches named.

| # | Where | The trick | What it avoids |
|---|---|---|---|
| 1 | `strategies/{single_asset,pairs,multi_asset}.rs` | `is_ready()` reads a cached threshold (`OnceLock` / plain field) instead of calling `stable_bars()` | `stable_bars()` walks the whole tree, and `Combine::unstable_bars` re-walks both children, so visits grow **exponentially with depth**. Recomputed per bar it was 40% of a depth-8 run. |
| 2 | `indicators/component.rs` | `SharedComponent` stores `warm_up`/`unstable` at construction | Both were behind the shared `Mutex`. The readiness walk called them every bar through the whole tree — ~38% of a `.shared()` strategy's runtime. |
| 3 | `indicators/stats.rs` | `WindowStats` is a hand-rolled ring buffer, not a `VecDeque`; the generic `Ring<T>` beside it is the same trick for everything else | Deque growth checks and index wrapping on a capacity that never changes. Took `Sma` 5.25 → 1.38 ns/sample, which is what put it level with TA-Lib. **This is a pattern, not a fact about `WindowStats`** — it went unapplied to six sibling windows for as long as it was written up as one. Phase 13 converted four (`Diff` −84.6% instructions, `Wma` −59.4%, `Hma` −39.4%, `Correlation` −25.4%, `Vwap` −10.4%) and records why the other two are correctly left alone. |
| 4 | `indicators/stats.rs` | Hand-written `Serialize`/`Deserialize` emitting `{period, window, sum}` | Lets #3 change representation **without changing the run-state format**. Delete it and every existing resume file breaks. |
| 5 | `wallet/paper.rs`, `indicators/book.rs` | Equity sums into a **stack buffer**, sorted, folded from cash | Two things at once: `HashMap` order varies per process, so summing in it made the equity curve drift by a ULP *between runs* (a real bug); and the old code allocated a `Vec` per bar to sort one element. |
| 6 | `hash.rs` | An in-crate FxHash `BuildHasher` for symbol maps | SipHash on `String` keys, several times per bar per symbol, for keys the user chose. ~30 lines beats a dependency here (crate policy: closed form first). |
| 7 | `snapshot.rs` | `Symbol` is a newtype over `Arc<str>` **carrying a precomputed hash**, interned at every boundary | Two costs, not one. *Cloning*: a symbol is cloned constantly and mutated never — took a 200k-bar run from 200 045 allocations to **37** (Phase 5). *Comparing*: `Arc<str>` compares by content, and same-length symbols (`BTCUSDT`/`ETHUSDT`) reach `memcmp`, which was **47% of a 64-symbol backtest** — hash-first `PartialEq` made it **2.04×** (Phase 13). Comparing `Arc` pointers instead is **unsound**: `symbol()` allocates fresh, so the spec-interned and data-interned handles differ and every run would report a plausible zero-fill backtest. `Hash` must keep delegating to the bytes or `Borrow<str>` breaks. |
| 8 | `strategies/single_asset.rs` | `extract_self_atom` returns `Option<&Atom>` | Cloning an 88-byte `Atom` per bar to read one `Copy` candle out of it. |
| 9 | `wallet/paper.rs`, `strategies/basket.rs` | `get_mut`-then-`insert` instead of bare `insert` | After the first bar the key is present, so `insert` clones the symbol every bar only to drop it. |
| 10 | `python/src/constructors.rs` | `column_to_vec` takes a buffer-protocol fast path | Otherwise one Python `float` object per element: ~155 ns/element, against ~1 ns of indicator work. **This is why the wheel is `abi3-py311`.** |
| 11 | `python/src/constructors.rs` | `ndarray_from_values` fills `np.empty`'s buffer in one pass | `np.asarray(vec)` builds a Python list first (a float object per element). Even `np.frombuffer(bytearray)` needed an intermediate `Vec` plus a `bytearray`. 19.29 → **0.75 ns/sample**. |
| 12 | `Cargo.toml` | `codegen-units = 1` | Worth −8.9…−20.1% instructions. The thin LTO beside it is nearly free but does little alone — don't split them without re-measuring. |
| 13 | `spec/metrics.rs` | One `sorted_asc` shared by the four quantile metrics | Four independent sorts of the same series: ~16.8 ms of a 22.8 ms reduction, paid once per `optimize` row per fold. |
| 14 | `runtime/chain.rs` | `Chain<In, Out>` keeps the domain **in the type** instead of in the value | `PayloadValue` is 88 bytes (as wide as its `Atom` variant) and crossed the boundary twice per expression level: **+13.7 ns/level**, against +2.5 for a `Chain`. Adding a payload enum back to recover self-description undoes the whole of Phase 6. |
| 15 | `runtime/chain.rs` | `Erased<I>` is an explicit adapter, not a blanket `impl<T: Indicator> DynIndicator for T` | A blanket impl shadows the compiler's automatic `impl Trait for dyn Trait`, and `Clone for Chain` needs that to reach `dyn_clone` through the vtable. The retiring `Adapter` sidesteps the same problem the same way. |
| 16 | `runtime/chain.rs` | `impl Indicator for Box<dyn DynIndicator<In, Out>>` | Without it every consumer needs a newtype (`TypedSource`, `As<Out>`) just to reattach the associated types — which is the only reason those existed. It is what made a ~150-site migration mechanical. |
| 17 | `python/src/constructors.rs` | `Column` **borrows** NumPy's buffer; nothing copies an input column | Copying five 1.6 MB columns per `feed()` was two thirds of the call — as page faults, not `memcpy` (a 1.6 MB block is past glibc's `mmap` threshold, so it is faulted in fresh and reclaimed on free every call). Phase 8. The GIL is held for all of `feed` regardless, so borrowing costs nothing. |
| 18 | `python/src/constructors.rs` | Every output array is `np.empty` + fill-in-place (`numpy_filled` / `numpy_bools`) | `np.asarray(vec)` routes through pyo3's sequence conversion — one Python `float`/`bool` object per element for NumPy to immediately parse back out. Three separate places had this; `build_bools` and `build_multi` kept it for a year *below* the comment explaining why it was wrong for floats. |
| 19 | `python/src/constructors.rs` | `Series` newtype for bulk numeric **arguments**, not `Vec<Real>` | Same failure on the way in: pyo3 fills a `Vec<f64>` element-by-element via the sequence protocol, so `metrics.sharpe` spent 44.79 ns/sample of a ~2 ns computation at the boundary. The tell was `ndarray` and `list` costing the same. |

### Things that were tried and did **not** work

Recorded so they are not re-attempted:

* **Composing Python sources with `runtime::chain_sync` instead of re-wrapping.**
  (This is `PayloadChain`, the payload-layer combinator — *not* `runtime::Chain`,
  which is Phase 6 and did work.) The theory was sound: chaining passes a
  `PayloadValue` straight through instead of converting in and out at each level.
  Measured, it made things *worse* (18.2× → 25.0× vs TA-Lib) — `PayloadChain`
  stores and clones an 88-byte payload every sample, which costs more than the
  conversions it removes. Reverted. The lesson generalises: the payload's *width*
  was the cost, so nothing that keeps carrying one can fix it.
* **Memoising multi-output sub-trees into a `Shared` handle** (`tests/perf_bench.rs`).
  ~3%, inside the noise.
* **`E[X²] − E[X]²` for variance.** See the `stddev` section — 4.06× faster and
  wrong by 6000% at high price scale.
* **Replacing `CandleColumns::for_each`'s nested zips with an indexed loop**, every
  slice re-cut to `[..n]` first so the five bounds checks would provably fold
  away. It compiles to the *same machine code*: `close`, `sma` and `atr` all
  measured identical to the hundredth of an instruction per sample (38.57 / 79.58
  / 81.23) before and after. LLVM already canonicalises both forms to one
  induction variable.
* **Shipping the measurement scaffolding.** Three `_bench_*` `#[pyfunction]`s
  (`_bench_feed_stage`, `_bench_feed_built`, `_bench_frame_stage`) were
  registered in the `fugazi` module so each prefix of `feed()` could be priced
  from Python. They earned their keep — several tables above came from them — and
  then rotted: Phase 8 rewrote the output path and the probes kept routing
  through the old one, so they no longer emulated the product they existed to
  emulate. **Removed.** A probe that silently stops matching the code under it is
  worse than no probe, and these were in a user-facing namespace with no test or
  tool referencing them. If you need this again, write it, measure with it, and
  delete it in the same session — or put it behind a Cargo feature so it cannot
  ship. The underscore prefix is not a substitute for either.
* **Hand-rolling `WindowStats::variance`'s centred pass.** The O(period) scan
  goes through `self.iter().map(..).sum()`, and `iter()` is `a.chain(b)` — which
  looks like it must test which half it is in on every element, so a `for` loop
  over the two slices should be strictly cheaper. It is **68% worse**: 187.45 →
  315.18 instructions/sample (`benches/icount.rs`, `stddev_scan`, period 20, net
  of a control). `Sum for f64` goes through `fold`, and std **specialises
  `Chain::fold` into two tight loops** with no per-element test; a `for` loop
  drives `Iterator::next()`, which keeps it. The idiomatic spelling was already
  the fast one.

  A second trap sits next to it: summing each half separately and adding them is
  **not bit-identical**, because `(Σa) + (Σb)` groups differently from one
  running total whenever the ring wraps. The core suite passes that version — the
  fixtures do not happen to exercise a wrapped window where the last ULP shows —
  so green tests are not evidence here.
* **Batching the erasure boundary** — handing the erased chain a *slice* of
  candles so the loop runs on its side of the vtable, one indirect call per 64
  bars instead of per bar. **1.2 instructions/bar slower**
  (`benches/icount.rs`, `sma_dyn_per_sample` vs `sma_dyn_batch`). An indirect call
  with a predictable target is about two instructions; what an erased level
  actually costs is its wrapper, its `Option` handling and the 40-byte `Candle`
  move, none of which batching removes — and the chunk bookkeeping then exceeds
  the calls it saves. This is why no such method exists on `DynIndicator`.

  Worth contrasting with **fusing** the field leaf into its wrapper
  (`sma_two_levels` vs `sma_fused`), which *is* worth 9.0 instructions/bar,
  because it deletes a level rather than re-arranging when it is entered. Not
  implemented: it needs an `AnySource::Field` carrier variant threaded through
  every constructor macro, for ~11% on chains rooted directly at a bar field.

### How to measure without fooling yourself

Twelve traps, each of which produced a wrong answer in this codebase before it
was caught. Note that **five of the twelve are "you measured a stale binary"** —
by far the most common way to be confidently wrong here.

1. **`maturin develop` builds *debug*.** It is 7–10× slower than release and
   amplifies unrelated changes. One pass of Python numbers taken on debug
   wheels showed a 33% "regression" that did not exist. Use
   `maturin develop --release`, or `maturin build --release` + install — and
   note `scripts/ci-local.sh` silently reinstalls a **debug** wheel over it.
2. **`pip install --force-reinstall` serves a cached wheel** of the same
   filename. Add `--no-cache-dir` or you will benchmark the previous build.
3. **The `pixi` bench environment has its own, separate `fugazi`.**
   `pixi.toml` installs it with `editable = false`, so it is built once and
   cached; `maturin develop` refreshes `python/.venv` and leaves the bench
   interpreter on the old binary. This produced a complete, plausible, entirely
   fictional set of numbers — a 15 ns/sample per-level erasure cost, measured
   against a build predating the change that removed it, together with a
   two-hour hunt for where the "extra" cost was hiding. It also means the two
   interpreters can disagree, which is the tell.
   `tools/bench_three_tier.py` now refuses to run when the extension is older
   than any `.rs` file, and prints the reinstall command.
4. **`cargo`/`maturin` can skip a rebuild** when a file's mtime does not advance
   (this is a networked/encrypted working tree). `Finished in 0.16s` after an
   edit means nothing was rebuilt — `touch` the file and build again.
5. **A microbenchmark of a *boxed* chain must build it opaquely.** If the
   concrete types are visible at the erase call, LLVM devirtualises the whole
   chain and the benchmark measures something no runtime builder can produce.
   The first version of `benches/erasure.rs` did exactly this and reported
   **+0.4 ns/level** for a vocabulary that costs +2.5 — flattering the proposal
   by 6×. Build behind `#[inline(never)]` *and* `black_box`, and sanity-check
   that the cost grows with depth at all.
6. **Wall-clock cannot separate "more work" from "unluckier code layout".** A
   benchmark here clocked +23.8% while executing 1.61% *fewer* instructions.
   Below ~25%, or when a delta moves between runs, use
   `scripts/perf-compare.sh icount` (callgrind), which is immune to both
   contention and layout.
7. **`ls target/release/deps/x-* | head -1` sorts by hash, not time.** It will
   hand you a stale binary — one built with different codegen settings, most
   likely. This has now bitten twice: once on the criterion binaries, and again
   on `icount`, where it reported **+37% instructions on `sma_rust`**, a
   workload the change could not touch. (That impossibility is the tell: always
   include a workload the change *cannot* affect, and check it reads zero.)
   `scripts/perf-compare.sh` now deletes bench binaries before each variant
   build and refuses to guess when more than one matches, on every subcommand.

8. **Instruction counting is blind to page faults**, and page faults were the
   largest cost in the Python bindings for the whole of Phases 1–7. A fault
   retires almost no instructions and burns thousands of cycles, so `callgrind`
   sees an allocation-heavy boundary as nearly free. The symptom that finally
   gave it away: a change that *reduced* instructions per sample made wall-clock
   *worse*, which read as a contradiction for far too long. If a workload's
   instruction count and its time disagree in direction, suspect memory —
   `/usr/bin/time -v` minor faults, or re-run with `MALLOC_MMAP_THRESHOLD_`
   raised and see whether the difference evaporates.
9. **Sizing a differential against its noise floor.** `tools/icount_python.py`
   subtracts two runs of the same script at `n` and `2n` iterations, which
   cancels interpreter startup exactly. Two identical Python processes still
   differ by ~1 M instructions out of ~834 M with `PYTHONHASHSEED` fixed, and
   ~7 M without — so a workload contributing less than that yields nonsense. The
   first version reported **−42.50 instructions/sample for `talib.SMA`**, a
   negative number, because at `n=4` its whole contribution was the size of the
   noise. Two things had to be pinned to get 0.00 on the control: the hash seed,
   and OpenBLAS's spinning thread pool — `blas_thread_server` was retiring
   **36.7% of the entire profile** doing nothing, and worse, it grows with how
   long the process lives, so it landed in the differential as if it were the
   measured code.

10. **A disassembled symbol may be a dead out-of-line copy.** Phase 10 split the
   centred variance across four accumulators, and `objdump` of
   `WindowStats::variance` showed a single serial add chain unrolled by four —
   i.e. exactly the code the change was supposed to remove. The conclusion
   ("LLVM re-linearised my lanes") was wrong: the hot path is *inlined* into
   `StdDev::update`, and the out-of-line symbol that keeps the name is whatever
   copy some cold caller needed. The A/B on `stddev_tradeoff` — which carries
   the O(1) shortcut in the same binary as a control — said 15.18 → 12.29
   ns/sample, matching an isolated `rustc -O` probe of the two loop shapes to
   within a few tenths. **Two independent measurements agreed and the reading of
   the assembly did not; the assembly was what was wrong.** If you want to
   confirm a transform applied, put a control in the same binary and A/B it, or
   disassemble the caller you actually benchmarked. A symbol that merely shares
   the function's name proves nothing.

11. **A benchmark file's workloads are not independent — adding one can move the
   others.** A diagnostic workload was added to the *end* of
   `benches/three_tier.rs`, after every measured row. It cost that file's
   `aroon` row **68%**: 9.50 → 15.94 ns/sample, reproducibly, on the same
   machine minutes apart, with every other row in the table unmoved. Nothing ran
   differently — the probe cannot perturb a workload that finished before it
   started. It is a *compile*-time effect: both construct
   `Aroon<Identity<Candle>>`, and a second call site of the same monomorphisation
   changes what LLVM inlines.

   This inverts the usual advice about controls. A control is supposed to be the
   workload the change cannot touch — but *adding* the control is itself a change
   to the binary, and it bites hardest when the control shares a type with the
   thing being measured, which is exactly when it is most useful. Two defences:
   put a diagnostic in **its own bench target** (each is its own crate, so it
   cannot reach the others), and treat any absolute number from a file whose
   workload list has changed as a fresh baseline rather than a comparable one.
   Deltas *within* one binary stay valid, which is what `benches/multi_feed.rs`
   relies on.

   Note what nearly went wrong: the poisoned reading was the *later* one, so the
   natural conclusion was "the committed figure was too optimistic". It was the
   other way round — the published 8.71 was right, and the instrument had broken.

12. **A callback through a `dyn` boundary is an indirect call per sample, and
   costs more than most things it is used to avoid.** The scalar `feed` staged
   each value through `[Option<Real>]` and copied it out — the same double hop
   `write_strided` had just removed from the multi path — so the fix looked
   obvious: give `DynIndicator` a slice fold taking `flatten: fn(Option<Out>) ->
   Real`. It made things **worse**: `atr` 53.78 → 62.93 instructions/sample,
   `sma_1d` 39.76 → 47.72. The method lives in a vtable, so `flatten` could not
   be inlined and became a call per sample — 13 to 16 instructions, more than
   the staging it removed. Hardcoding the flattening in the method body instead
   turned the same change into a win (`sma_1d` 34.76, `atr` 51.79).

   **A generic parameter is free and a `fn` parameter is not**, once the method
   is dispatched dynamically. If a `dyn` method needs to vary its behaviour,
   vary it with a bound the impl resolves, not with a function the caller
   passes.

The general defence, and the one that actually caught trap 3: **measure the same
quantity by paths that share as little as possible.** Phase 6's per-level cost
was confirmed by a Rust bench, a Rust-built chain inside the extension, and a
Python-built chain driven two different ways. Agreement across four paths is
evidence; one number is an anecdote.

## Known costs, and why they are there

Some slow things are deliberate. Before "fixing" one, check here.

- **`WindowStats` dispersion reads scan the window (O(period)).** The O(1)
  `E[X²] − E[X]²` shortcut cancels away `(mean/σ)²` significant digits and was
  measurably wrong at crypto price scale — at `mean = 1e9, σ = 0.01` it clamped
  the variance to `0.0`. See the module docs in `src/indicators/stats.rs`.
  `Sma` reads only `mean` and stays O(1).
- **`Snapshot` clones are refcount bumps, not deep copies.** This is
  load-bearing rather than an optimisation: a snapshot is fed to every signal
  slot of every symbol each bar, so with a plain `Vec` the per-bar cost grew
  with the *square* of the universe. See `src/snapshot.rs`.
- **Memoising multi-output sub-trees into a `Shared` handle was tried and
  rejected.** The cache hits and one `Macd` genuinely drives both components,
  but the total moved ~3% — four `SharedComponent`s taking a mutex per bar cost
  about what the duplicated arithmetic did. Don't re-attempt it without
  re-measuring. See `tests/perf_bench.rs`.
