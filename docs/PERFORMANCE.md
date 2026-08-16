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

## Measurement conditions

The first pass of these numbers was taken on a machine with other builds
running, and was wrong in an instructive way — see *A measurement that lied*
below. Everything in the results table was **re-measured on a quiet machine**,
with the baseline and the current tree built from **identical codegen settings**
(`CARGO_PROFILE_BENCH_LTO=false CARGO_PROFILE_BENCH_CODEGEN_UNITS=16`) so the
comparison isolates the code change rather than the profile.

Method, if you need to reproduce it:

```
git worktree add ../fugazi-base v0.58.0
cp -r benches ../fugazi-base/            # add the [[bench]] entries too
# then, with the same CARGO_PROFILE_BENCH_* on both sides:
cargo bench --bench tree --bench driver --bench metrics --bench multi_asset
scripts/perf-compare.sh icount ../fugazi-base
```

## A measurement that lied

`driver/sma_crossover/rust` clocked **+10.9% to +23.8% slower** after the
readiness change — on a quiet machine, reproducibly, with codegen equalised. It
looked like a real regression worth chasing.

It was not. Callgrind says that workload executes **1.61% fewer instructions**
than before:

| workload | v0.58.0 | now | instructions |
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

## Baseline — v0.58.0 (`da252ff`)

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

## Results — v0.58.0 → v0.59.0

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

| benchmark | v0.58.0 | now | change |
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

### The profile change (F7), separately

Still **not established**. Compile cost is solid — incremental rebuild after
touching `src/lib.rs`: 42 s untuned, 45 s with `lto = "thin"`, 68 s with both.
The runtime half was measured across different contention windows and its
headline reading ("thin LTO alone regresses `tree/update` by up to 14%") is not
a plausible compiler effect. The settings are kept on the general argument that
the hot path is generic code crossing module boundaries plus trait objects.
**To settle it: use `scripts/perf-compare.sh icount` across profile variants of
the same source** — that removes both contention and layout from the comparison.

### Correction to a previously-recorded conclusion

`tests/perf_bench.rs` recorded that the shared-handle memo "moves ~3%" because
"four `SharedComponent`s taking a mutex per bar costs about what the duplicated
arithmetic did", and concluded the residual gap was the type-erasure layer. The
mutex cost was real but **not in `update`** — it was in the readiness walk. That
note is amended in place rather than deleted; the memo experiment it describes
still stands.

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
