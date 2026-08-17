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

| Want | Run |
|---|---|
| A/B a change | `scripts/perf-compare.sh save before` on the base, then `scripts/perf-compare.sh diff before` on your branch |
| One bench group | `cargo bench --bench tree` |
| Allocations/bar, bytes/bar, peak RSS | `cargo bench --bench footprint` |
| Instruction counts (deterministic) | `scripts/perf-compare.sh icount` |

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
| `footprint` | Allocation count, bytes, and peak RSS. Not criterion — it installs a counting global allocator, which inside a criterion target would also tally criterion's own bookkeeping. |
| `icount` | A fixed workload run exactly once, for callgrind. Answers "does this change do more work?" immune to contention and to code layout. |
| `breaking` | Prototypes for the proposed breaking changes, so each is a measured number rather than an argument. |

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

<!-- BASELINE:START -->
Saved as criterion baseline `v058` (`scripts/perf-compare.sh diff v058`).

### Indicator cores — ns per `update`, 50 000 scalar samples

| Indicator | ns/sample |
|---|---:|
| `Ema(20)` | 1.65 |
| `Macd(12,26,9)` | 1.77 |
| `Sma(20)` | 5.12 |
| `Rsi(14)` | 5.59 |
| `StdDev(20)` | 20.5 |
| `Bollinger(20)` | 22.6 |
| `Atr(14)` (candle) | 26.5 |
| `Percentile(100, p50)` | 49.4 |
| `StdDev(100)` | 69.3 |

`StdDev` scaling with period is the documented O(period) centred pass, not a
bug. The `Sma` / `Ema` gap is *not* explained by that — `Sma` reads only `mean`,
which is O(1); the 3× is `VecDeque` push/pop/index overhead against `Ema`'s two
scalar registers.

### Expression-tree depth — 2 000 bars, spec-built (dyn) single-asset strategy

| depth | `update` | `is_ready` | `drive` | `is_ready` share of `drive` |
|---:|---:|---:|---:|---:|
| 1 | 0.61 ms | 0.077 ms | 0.97 ms | 8% |
| 2 | 0.97 ms | 0.216 ms | 1.59 ms | 14% |
| 4 | 1.79 ms | 0.748 ms | 2.98 ms | 25% |
| 6 | 2.48 ms | 1.575 ms | 4.61 ms | 34% |
| 8 | 3.28 ms | 2.588 ms | 6.49 ms | **40%** |

The tree is a left spine, so node count is linear in depth. `update` tracks
that (5.4× for 8× the nodes). `is_ready` does not: **33× for 8× the nodes.**
That is `Combine::unstable_bars` calling `stable_bars()` on both children *and*
`warm_up_bars()` on itself — which walks both children again — repeated per bar,
through `Box<dyn Signal>` calls LLVM cannot fold.

### End-to-end driver — 50 000 bars

| Strategy | Rust | YAML | ratio |
|---|---:|---:|---:|
| SMA crossover | 23.0 ms | 63.9 ms | 2.78× |
| MACD crossover | 36.2 ms | 82.1 ms | 2.27× |

### Universe scaling — 2 000 bars, `MultiAssetStrategy`

| symbols | `update` total | ns per symbol-bar | `drive` total | ns per symbol-bar |
|---:|---:|---:|---:|---:|
| 2 | 0.90 ms | 225 | 1.30 ms | 324 |
| 8 | 4.06 ms | 254 | 5.74 ms | 359 |
| 16 | 10.5 ms | 329 | 14.5 ms | 454 |
| 32 | 31.4 ms | 491 | 38.6 ms | 603 |
| 64 | 102 ms | 793 | 119 ms | 929 |

Per-symbol-bar cost should be **flat**. It rises 3.5× from 2 to 64 symbols —
the quadratic term is `snap.iter().find_map(...)` *inside* the per-symbol loop
in `MultiAssetStrategy::update`, an O(N) scan done N times per bar.

### `PaperWallet` — 20 000 calls

| held positions | `equity` | `update` |
|---:|---:|---:|
| 1 | 27 ns | 80 ns |
| 4 | 82 ns | 81 ns |
| 16 | 291 ns | 87 ns |
| 64 | 1 192 ns | 87 ns |

`equity` is ~18.6 ns per held position — a SipHash lookup of a `String` key into
`bars`, per position, per call. The driver calls it once per bar.

### Metrics reduction — `RunReport` → `Metrics`

| bars | total |
|---:|---:|
| 10 000 | 0.86 ms |
| 100 000 | 11.4 ms |
| 200 000 | 22.8 ms |

Of the 22.8 ms at 200k bars, ~16.8 ms is **four independent sorts of the same
return series** (`median_return`, `value_at_risk`, `conditional_value_at_risk`,
`tail_ratio` each call `sorted_asc`, 4.1–4.4 ms apiece) and a further 0.66 ms is
`drawdown_segments` recomputed by `calmar` and `recovery_factor` on top of the
copy `from_report` already built.

### Footprint — 200 000-bar single-asset run

| case | allocs/bar | bytes/bar |
|---|---:|---:|
| building `Vec<Snapshot<String>>` | 3.00 | 201.0 |
| building 8-symbol snapshots | 11.00 | 1 080 |
| building 32-symbol snapshots | 37.01 | 4 056 |
| driving the run (Rust strategy) | 5.00 | 44.0 |
| driving the run (YAML strategy) | 5.00 | 44.0 |

Sizes: `Candle` 40 B · `Atom` 88 B · snapshot entry 120 B · `Snapshot` handle 8 B.

Peak RSS 98.0 MiB = **513 B/bar**, against 7.6 MiB (40 B/bar) of actual OHLCV.
Snapshot construction alone is three allocations per bar — the `Arc` control
block, the `Vec` buffer, and a fresh `String` copy of the same symbol.

The Rust and YAML paths allocate **identically**, which is worth noting: the
2.3–2.8× driver gap above is not allocation, it is the per-bar work.
<!-- BASELINE:END -->

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

## Phase 5 — `Symbol = Arc<str>`

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

## Breaking candidates — measured, not yet done

Changes that would break the public API, **prototyped and measured** rather
than argued for. `benches/breaking.rs` holds the prototypes.

(`Sym = Arc<str>` was the third; it has since been implemented as
`fugazi::Symbol` — see *Phase 5* below.)

Read the figures as **ceilings**. Each prototype is narrower than the real
change would be, and a couple of them enjoy inlining that the general case
would not.

### 1. Shrink `DynValue`, and stop nesting erasure — highest value

`std::mem::size_of::<DynValue>()` is **88 bytes**: the enum is as wide as its
widest variant (`Atom`), so every erased `update` moves 88 bytes in and 88 back
out even when the payload is one `f64`.

Worse, the Python bindings *nest* the erasure. `sma(identity())` builds
`Source::new(Sma::new(already_erased_source, 10))` (`map_source!` in
`python/src/macros.rs`), so one sample crosses the boundary twice in each
direction — six `DynValue` conversions. Measured (`benches/three_tier.rs`):

| | ns/sample |
|---|---:|
| `Sma` native | 1.36 |
| one erasure boundary | 3.70 |
| two boundaries (what the bindings build) | **16.95** |

**A 12× tax on the engine, paid before Python is involved**, and it is what
puts the bindings 10–29× behind TA-Lib's.

Two independent fixes:

- **Box the large variants.** `DynValue::Atom(Box<Atom>)` /
  `Candle(Box<Candle>)` / `Snapshot` already behind an `Arc` would take the enum
  to ~16 bytes. Costs an allocation on the *bar-shaped* paths, which is why it
  needs measuring rather than assuming — but the `Real`/`Bool` paths, which are
  the hot ones for scalar indicators, become a register move.
- **Don't re-erase an already-erased source.** When `map_source!` wraps a
  `Source` that is itself a `Box<dyn DynIndicatorSync>`, the inner box could be
  composed with `runtime::chain` instead of being wrapped again, halving the
  conversions. This one is *not* an API break — it is contained entirely in
  `python/src/`, and should be tried first.

### 2. `Indicator::update(&mut self, input: &Self::Input)`

Prototyped as a by-reference chain computing the same SMA crossover
(`RefIndicator` in `benches/breaking.rs`).

| | per 2 000 bars |
|---|---:|
| library, by value | 121.6 µs |
| prototype, by reference | **35.0 µs** |

−71%, but **do not take that at face value**. The prototype is monomorphic,
and it fuses `Pick` + `Close` into one node, so it avoids the `Atom` round-trip
entirely rather than merely avoiding the clone. It bounds the win; it does not
predict it. What is certain from the code is the traffic removed: `Combine`
feeds the same input to *both* sides, so every binary node clones its input, and
`Pick` clones the projected 88-byte `Atom` twice per bar (once into
`Pick::value`, once for the return).

**Cost.** The largest of the three: ~60 indicators, `fugazi-derive`,
`runtime::DynIndicator`, all five strategy shapes, `python/src/`, and every doc
example. Worth doing only if the ceiling is confirmed on a narrower slice first
— converting `Pick` and `Combine` alone would test the thesis.

### 3. Index `Snapshot` for lookup

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

**Read the Python row against TA-Lib, not against Rust.** `talib.SMA(arr, 10)`
*is* a Python API — a thin Cython wrapper over the C library. So the comparison
that matters to a Python user is `fugazi py` vs `TA-Lib`, and that is the
unflattering one:

| indicator | TA-Lib (py) | fugazi rs | fugazi py | **py vs TA-Lib** | rs vs TA-Lib |
|---|---:|---:|---:|---:|---:|
| `sma` | 1.37 ns | 1.42 | 40.31 | **29×** | 1.03× |
| `ema` | 2.16 ns | **1.46** | 38.24 | **18×** | **0.67×** |
| `rsi` | 4.91 ns | 4.92 | 50.33 | **10×** | 1.00× |
| `stddev` | 3.98 ns | 10.68 | 59.04 | **15×** | 2.69× |
| `atr` | 11.81 ns | 12.98 | — | — | 1.10× |

The **Rust** engine meets the bar: it matches or beats a vectorised C library on
`sma`/`ema`/`rsi` and is within 10% on `atr`, while staying *incremental* (one
`update()` per bar, which is what lets the same code drive a live stream).

The **Python** bindings do not. A caller who reaches for `fugazi` instead of
`talib` for a batch computation pays 10–29×.

### Where the Python time goes

Measured rather than assumed — `benches/three_tier.rs` carries the intermediate
rungs, so each layer is isolated with no Python involved:

| | ns/sample | delta |
|---|---:|---:|
| `Sma` native Rust | 1.36 | — |
| … through **one** erasure boundary (`sma_erased`) | 3.70 | +2.3 |
| … through **two**, as the bindings build it (`sma_erased_nested`) | 16.95 | **+13.3** |
| … via `feed()` from Python | 56.72 | +39.8 |

Two separate problems, and the first is the one to name:

**`DynValue` is 88 bytes**, because the enum is as wide as its `Atom` variant.
Every erased `update` moves that payload in and back out. And the bindings
*nest*: `sma(identity())` wraps an already-erased source, so a sample crosses
the boundary twice each way — six `DynValue` conversions per sample. That alone
takes `Sma` from 1.36 to 16.95 ns, a **12× tax on the engine before Python is
even involved**. Boxing the large variants would shrink `DynValue` to 16 bytes;
flattening the nesting would remove conversions outright. See the breaking
candidates.

**~40 ns/sample remains unattributed** between the nested-erasure figure and
`feed()`. It is *not* the input conversion — `feed(list)` is only 9 ns/sample
slower than `feed(numpy)`, so the buffer fast path is not where the time is.
This needs a profiler rather than another guess, and is deliberately left
unexplained here rather than filled in with a plausible story.

### What `stddev` buys with its 2.7×

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

Cost, same window: 14.34 ns/sample centred vs 3.62 shortcut — **3.97×**.

So the trade is ~10.7 ns/sample for a result that stays correct to ~1e-13 where
the shortcut is wrong by **6000%**. A five-figure instrument quoted to the cent
(1e5 / 0.01) already costs the shortcut 1% of its answer, and `ZScore` divides
by that. Keep the centred pass.

There is no free lunch in between: Welford's online algorithm is O(1) and far
better conditioned than the naive shortcut, but it has no numerically stable
*removal* step, which a sliding window needs on every sample.

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
