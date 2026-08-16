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

## Phase 1 — profile, shared-component locks, metrics dedup

Three changes, measured together against `v058`. Attribution below is by which
benchmarks each could possibly touch.

### What changed

- **F7 — `[profile.release] lto = "thin"`, `codegen-units = 1`.** See the
  measured config table in `Cargo.toml`: `codegen-units = 1` is the setting that
  matters; thin LTO on its own *regressed* `tree/update` at every depth. Cost is
  +26 s on an incremental rebuild (42 s untuned → 45 s thin → 68 s both).
- **F2 — `SharedComponent::warm_up_bars` / `unstable_bars` no longer lock.**
  Both are structural properties of the source, fixed once built; they are now
  read once in `SharedComponent::new` and stored. `Strategy::is_ready` walks the
  tree calling exactly these two on every bar, so the lock traffic was
  proportional to tree size × bars for a pair of constants.
- **F8 — the metrics reduction sorts once.** `spec::metrics::from_report` now
  builds one `sorted_asc` copy and calls `*_of_sorted` backs, and passes the
  `max_drawdown` it already has to `calmar` / `recovery_factor` instead of
  letting each recompute `drawdown_segments`.

### Results

| benchmark | before | after | change |
|---|---:|---:|---:|
| `metrics/from_report/200000` | 22.85 ms | 9.64 ms | **−57.7%** |
| `metrics/from_report/100000` | 11.42 ms | 4.68 ms | −55.0% |
| `driver/macd_crossover/rust` | 36.25 ms | 18.93 ms | **−46.1%** |
| `wallet/equity/1` | 0.54 ms | 0.26 ms | −46.5% |
| `wallet/equity/64` | 23.83 ms | 14.66 ms | −34.3% |
| `driver/sma_crossover/yaml` | 63.94 ms | 50.43 ms | −19.9% |
| `driver/macd_crossover/yaml` | 82.11 ms | 63.85 ms | −17.1% |
| `driver/sma_crossover/rust` | 23.05 ms | 18.65 ms | −15.9% |
| `tree/drive/8` | 6.49 ms | 5.97 ms | −10.5% |
| `indicators/candle/atr_14` | 1.33 ms | 0.42 ms | −67.7% |
| `indicators/scalar/stddev_20` | 1.03 ms | 0.80 ms | −15.6% |
| `tree/is_ready/*` | — | — | **unchanged** |

**Attribution.** `driver/sma_crossover/rust` (−15.9%) has no `.shared()` and no
metrics, so it is F7 alone. `driver/macd_crossover/rust` (−46.1%) is the same
strategy shape with four `SharedComponent`s, so **F2 is worth ≈ −38% on top of
F7** for a `.shared()`-composed strategy. The YAML MACD side builds two
independent `Macd`s and holds no `Shared`; it moved only by the F7 amount, which
confirms the split. `metrics/parts/*` moved only by the F7 amount too, so the
−57.7% on `from_report` is the deduplication, not the compiler.

**`tree/is_ready` did not move**, at any depth. The compiler cannot devirtualize
that recursion, which is why F1 needs a code fix rather than a flag.

**Footprint unchanged** — Phase 1 touched compute only (still 3.00 allocs/bar to
build snapshots, 5.00 allocs/bar to drive, 98.0 MiB peak RSS).

### Correction to a previously-recorded conclusion

`tests/perf_bench.rs` recorded that the shared-handle memo "moves ~3%" because
"four `SharedComponent`s taking a mutex per bar costs about what the duplicated
arithmetic did", and concluded the gap was the type-erasure layer. The mutex
cost was real but it was **not in `update`** — it was in the readiness walk. That
note has been amended in place rather than deleted, since the memo experiment
itself still stands.

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
