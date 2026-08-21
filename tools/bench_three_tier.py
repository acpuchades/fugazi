#!/usr/bin/env python3
"""Four-tier indicator throughput: TA-Lib (C and Python) vs fugazi (Rust and Python).

Answers two questions the project cares about separately, and — this is the
point of having *four* tiers rather than three — gives each question the
baseline that actually matches it:

  1. Does the **Rust** engine match or beat TA-Lib? Measured against
     **native TA-Lib C** (`tools/bench_talib_native.c`), because comparing a
     Rust library against a Python-wrapped C library credits fugazi with the
     wrapper's overhead.
  2. How much does a **Python** caller give up? Measured against **`talib`, the
     Cython bindings**, because both sides cross a Python boundary there. That
     is the comparison a Python user actually faces.

Using the Python bindings as the baseline for *both* was a real error even so.
It is a small one — measured cleanly, `talib.ATR` costs 5.40 ns/sample against
native `TA_ATR`'s 4.83, about 12% — but it is an error in the flattering
direction, and it is free to not make.

(An earlier revision of this docstring claimed the ATR wrapper cost 2.5x, from
runs taken while the machine was loaded. It does not. That figure was wrong in
fugazi's favour when quoted as a reason the comparison mattered, which is why
the `repeat`/best-of logic in `main` now exists.)

**The run decides for itself when to stop.** The reported figure is the minimum
over every sample taken, and the justification for a minimum is that it converges
with more sampling — so a fixed pass count is the wrong stopping rule, because it
cannot say whether one more pass would have moved a cell. It does move them: a
table quoting `aroon` at 9.63 ns/sample was re-measured at 9.30 by the same
binary on a quieter minute. So `main` keeps taking passes until no cell's minimum
has improved by more than 1% for three consecutive passes, and says so; if it
hits `FUGAZI_BENCH_MAX_PASSES` first it prints a loud non-convergence banner
rather than a table that looks like every other table.

The comparison is deliberately *not* apples-to-apples in one respect, and the
report says so: TA-Lib is **vectorised** — one call computes a whole array,
with the loop in C and no per-sample dispatch. fugazi is **incremental** — one
`update()` per bar, which is what lets the same code drive a live stream. A
vectorised batch API will always win a batch benchmark; the question is by how
much, and whether the incremental design is paying an acceptable price for
what it buys.

Usage:

    pixi run -e bench bench

The `bench` environment exists because this script needs `talib` *and* a built
`fugazi` wheel importable from one interpreter. Sourcing them from two package
managers is what used to require an `LD_LIBRARY_PATH=<gcc-lib>/lib` prefix to
get conda's `talib` extension to find a compatible libstdc++; one environment
holding both needs no such prefix. Building the wheel uses `cargo` from the
ambient PATH, same as `maturin develop`.

Rust numbers come from `cargo bench --bench three_tier` and native TA-Lib from a
`cc` build of `tools/bench_talib_native.c`; this script invokes and parses both,
so every tier runs the same input length over the same LCG walk. The native tier
**skips** (rather than fails) when `cc` or TA-Lib's headers are missing, since it
needs a C toolchain the rest of the project does not.

**The extension is checked for staleness before anything is timed**, and the run
aborts if it is out of date. `pixi.toml` installs `fugazi` into this environment
with `editable = false`, so the wheel is built once and cached — a later
`maturin develop` refreshes `python/.venv` and leaves *this* interpreter on the
old binary. That has already produced a full set of plausible, entirely
fictional numbers (a 15 ns/sample per-erasure-level cost measured against a
build that predated the fix removing it). Timestamps are a blunt check, but the
failure they catch is silent and total, and the fix is one command.
"""

from __future__ import annotations

import gc
import json
import os
import subprocess
import sys
import time

import numpy as np
import talib

ROOT = os.path.dirname(os.path.dirname(os.path.abspath(__file__)))
N = 200_000
REPS = 7
WARMUP = 2

# Keep in sync with benches/three_tier.rs and tools/gen_talib_fixtures.py.
SMA_P = 10
EMA_P = 10
RSI_P = 14
STDDEV_P = 10
ATR_P = 14
# Multi-output. Keep in sync with tools/bench_talib_native.c.
MACD_FAST, MACD_SLOW, MACD_SIGNAL = 12, 26, 9
BBANDS_P, BBANDS_K = 20, 2.0
AROON_P = 14
DMI_P = 14
CORREL_P = 20
LINREG_P = 14

# The rows the report prints, in order. Scalar first, then the multi-output
# block this comparison exists for.
SCALAR = ("sma", "ema", "rsi", "stddev", "atr", "correlation", "linreg_slope")
MULTI = ("macd", "bbands", "aroon", "dmi", "adx")


def newest_source_mtime() -> tuple[float, str]:
    """The most recently touched Rust source the extension is built from."""
    newest, where = 0.0, ""
    for sub in ("src", "python/src", "fugazi-derive/src"):
        for dirpath, _, files in os.walk(os.path.join(ROOT, sub)):
            for f in files:
                if not f.endswith(".rs"):
                    continue
                p = os.path.join(dirpath, f)
                m = os.path.getmtime(p)
                if m > newest:
                    newest, where = m, os.path.relpath(p, ROOT)
    return newest, where


def check_extension_fresh() -> int:
    """Abort unless the imported `fugazi` extension is newer than its sources.

    See the module docstring: a cached non-editable wheel in this environment is
    invisible from the Python side and silently invalidates every number below.
    """
    import fugazi as fz

    so = fz.__file__
    pkg = os.path.dirname(so)
    built = max(
        (os.path.getmtime(os.path.join(pkg, f)) for f in os.listdir(pkg) if f.endswith(".so")),
        default=0.0,
    )
    newest, where = newest_source_mtime()
    if built >= newest:
        return 0
    age = (newest - built) / 60.0
    print(
        f"the `fugazi` extension in this environment is stale — {where} is "
        f"{age:.0f} min newer than the installed binary.\n"
        f"  installed: {pkg}\n\n"
        "Rebuild and reinstall it here before benchmarking:\n\n"
        "    cd python && uv run --no-project --python .venv/bin/python \\\n"
        "        maturin build --release\n"
        "    uv pip install --python .pixi/envs/bench/bin/python --no-deps \\\n"
        "        --force-reinstall --no-cache-dir target/wheels/fugazi-*.whl\n",
        file=sys.stderr,
    )
    return 1


def synth(n: int) -> tuple[np.ndarray, np.ndarray, np.ndarray, np.ndarray]:
    """The same deterministic LCG walk the Rust benches use, so every tier is
    fed identical numbers."""
    px = 100.0
    s = 0x5EED_1234_5678_9ABC
    mask = (1 << 64) - 1
    o = np.empty(n)
    h = np.empty(n)
    lo = np.empty(n)
    c = np.empty(n)
    for i in range(n):
        s = (s * 6364136223846793005 + 1442695040888963407) & mask
        noise = ((s >> 33) / 0xFFFFFFFF) - 0.5
        ret = 0.0002 + 0.01 * noise
        open_, close = px, px * (1.0 + ret)
        o[i], c[i] = open_, close
        h[i] = max(open_, close) * 1.001
        lo[i] = min(open_, close) * 0.999
        px = close
    return o, h, lo, c


def talib_native(n: int) -> dict[str, float]:
    """Compile and run the native TA-Lib C tier, or `{}` if that isn't possible.

    The baseline for fugazi's **Rust** row. Returns empty rather than raising
    when `cc` or TA-Lib's headers are absent: a C toolchain is needed by nothing
    else here, so a missing one should cost this one column, not the whole run.

    Headers and `libta_lib` come from the same environment that provides `talib`,
    found relative to this interpreter — the conda-style prefix two levels up
    from `sys.executable`. `LD_LIBRARY_PATH` is set for the child because the
    library is a shared object outside the default search path.
    """
    src = os.path.join(ROOT, "tools", "bench_talib_native.c")
    if not os.path.exists(src):
        return {}
    prefix = os.path.dirname(os.path.dirname(os.path.abspath(sys.executable)))
    inc, lib = os.path.join(prefix, "include"), os.path.join(prefix, "lib")
    if not os.path.exists(os.path.join(inc, "ta-lib", "ta_libc.h")):
        return {}

    import shutil
    import tempfile

    cc = shutil.which("cc") or shutil.which("gcc")
    if not cc:
        return {}

    with tempfile.TemporaryDirectory() as tmp:
        exe = os.path.join(tmp, "talib_native")
        build = subprocess.run(
            [cc, "-O2", "-o", exe, src, f"-I{inc}", f"-L{lib}", "-lta_lib", "-lm"],
            capture_output=True, text=True,
        )
        if build.returncode != 0:
            print("native TA-Lib tier did not build:\n", build.stderr[-800:],
                  file=sys.stderr)
            return {}
        env = dict(os.environ)
        env["LD_LIBRARY_PATH"] = lib + os.pathsep + env.get("LD_LIBRARY_PATH", "")
        run = subprocess.run([exe, f"--n={n}"], capture_output=True, text=True, env=env)
        if run.returncode != 0:
            print("native TA-Lib tier did not run:\n", run.stderr[-800:], file=sys.stderr)
            return {}

    out: dict[str, list[float]] = {}
    for line in run.stdout.splitlines():
        line = line.strip()
        if line.startswith("{") and '"ns_per_sample"' in line:
            rec = json.loads(line)
            out[rec["name"]] = rec.get("samples") or [rec["ns_per_sample"]]
    return out


def timed_samples(fn, reps: int = REPS, warmup: int = WARMUP) -> list[float]:
    """Median wall time over `reps` runs, after `warmup` discarded ones.

    The warm-up is load-bearing: a cold process on this machine reports TA-Lib's
    SMA at 1.99 ns/sample against a warm 1.38, a 44% error from CPU frequency
    ramp and cold caches. Because it inflates the *baseline*, leaving it in
    flatters fugazi. Every tier discards identically — see
    `tools/bench_talib_native.c` and `benches/three_tier.rs`.
    """
    for _ in range(warmup):
        fn()
    times = []
    for _ in range(reps):
        gc.collect()
        gc.disable()
        t = time.perf_counter()
        fn()
        times.append((time.perf_counter() - t) * 1e9 / N)
        gc.enable()
    return sorted(times)


def rust_tier(n: int) -> dict[str, float]:
    """fugazi's Rust engine, via `cargo bench --bench three_tier --emit-json`."""
    env = dict(os.environ)
    env["FUGAZI_THREE_TIER_N"] = str(n)
    proc = subprocess.run(
        ["cargo", "bench", "--bench", "three_tier", "--", "--emit-json"],
        cwd=ROOT, capture_output=True, text=True, env=env,
    )
    out: dict[str, list[float]] = {}
    for line in proc.stdout.splitlines():
        line = line.strip()
        if line.startswith("{") and '"ns_per_sample"' in line:
            rec = json.loads(line)
            out[rec["name"]] = rec.get("samples") or [rec["ns_per_sample"]]
    if not out:
        print("could not read Rust numbers; cargo said:\n", proc.stderr[-2000:])
    return out


def python_tier(which: str, n: int) -> dict[str, list[float]]:
    """Run one Python tier in its **own process** and parse its samples back.

    Not a nicety. The two Python tiers used to run in this process, one after
    the other, and they perturb each other through the shared heap: measured on
    a quiet machine, `talib.MACD` costs **29.60 ns/sample** in a fresh process
    and **7.51** in one where fugazi has already run — a 4x swing in fixed C
    code, caused entirely by what the other tier left in the allocator.

    That silently coupled the `py vs py` column to fugazi's own allocation
    behaviour, so a change to how fugazi allocates moved its own baseline. It
    was caught when a `(lines, n)` allocation in `feed_into_columns` dropped
    `talib`'s reported cost by 3x and produced the impossible reading of
    `talib` through Python beating the TA-Lib C library it calls.

    So each tier now gets a clean interpreter, imports only its own library, and
    reports samples on stdout — the same shape `rust_tier` and `talib_native`
    already used, and for the same reason.
    """
    proc = subprocess.run(
        [sys.executable, os.path.abspath(__file__), f"--tier={which}", f"--n={n}"],
        cwd=ROOT, capture_output=True, text=True,
    )
    out: dict[str, list[float]] = {}
    for line in proc.stdout.splitlines():
        line = line.strip()
        if line.startswith("{") and '"ns_per_sample"' in line:
            rec = json.loads(line)
            out[rec["name"]] = rec.get("samples") or [rec["ns_per_sample"]]
    if not out:
        print(f"could not read the {which} tier:\n", proc.stderr[-2000:], file=sys.stderr)
    return out


def talib_py_tier(c, h, lo) -> dict[str, float]:
    """TA-Lib through its Cython bindings — the baseline for the Python row."""
    return {
        "sma": timed_samples(lambda: talib.SMA(c, SMA_P)),
        "ema": timed_samples(lambda: talib.EMA(c, EMA_P)),
        "rsi": timed_samples(lambda: talib.RSI(c, RSI_P)),
        "stddev": timed_samples(lambda: talib.STDDEV(c, STDDEV_P)),
        "atr": timed_samples(lambda: talib.ATR(h, lo, c, ATR_P)),
        # Both legs are the close series in all three tiers: the paired
        # window's cost does not depend on the values, and the cheapest
        # possible second leg keeps this a measurement of the window rather
        # than of its operands.
        # `correlation` stands for the paired family (`covariance` / `beta`
        # share its core and its cost); `talib.BETA` differences its inputs
        # internally, so it is not a like-for-like partner.
        "correlation": timed_samples(lambda: talib.CORREL(c, c, CORREL_P)),
        # fugazi's LinReg produces four readings per bar and projects one;
        # `talib.LINEARREG_SLOPE` fills one array. The gap is that extra work,
        # which is what a caller wanting a slope actually pays.
        "linreg_slope": timed_samples(lambda: talib.LINEARREG_SLOPE(c, LINREG_P)),
        # One call, every line — the shape a fugazi multi-output `update` has.
        "macd": timed_samples(lambda: talib.MACD(
            c, fastperiod=MACD_FAST, slowperiod=MACD_SLOW, signalperiod=MACD_SIGNAL)),
        "bbands": timed_samples(lambda: talib.BBANDS(
            c, timeperiod=BBANDS_P, nbdevup=BBANDS_K, nbdevdn=BBANDS_K, matype=0)),
        "aroon": timed_samples(lambda: talib.AROON(h, lo, AROON_P)),
        # TA-Lib has no combined DI pair and no combined ADX triple, so a caller
        # who wants them pays for each call — and each re-derives the same
        # Wilder-smoothed true range. `Dmi`/`Adx` carry one set of Wilder states
        # and emit the lines together. Timing both calls is the comparison, not
        # a distortion of it; see the note in tools/bench_talib_native.c.
        "dmi": timed_samples(lambda: (
            talib.PLUS_DI(h, lo, c, DMI_P), talib.MINUS_DI(h, lo, c, DMI_P))),
        "adx": timed_samples(lambda: (
            talib.PLUS_DI(h, lo, c, DMI_P), talib.MINUS_DI(h, lo, c, DMI_P),
            talib.ADX(h, lo, c, DMI_P))),
    }


def fugazi_py_tier(c, h, lo, o) -> dict[str, float]:
    """fugazi's Python bindings, driven exactly as a user would.

    Scalar indicators take a 1-D series; `atr` consumes whole bars, so it takes a
    dict of OHLCV columns — the same shape `talib.ATR`'s three arrays carry, and
    what `frame_to_candles` accepts.

    ATR was missing from this tier for a while, which read as "the binding does
    not exist". It does; the tier only fed 1-D series and a candle-rooted
    indicator rejects those.
    """
    import fugazi as fz

    def scalar(build):
        def run():
            build().feed(c)
        return run

    frame = {"open": o, "high": h, "low": lo, "close": c}

    return {
        "sma": timed_samples(scalar(lambda: fz.sma(fz.identity(), SMA_P))),
        "ema": timed_samples(scalar(lambda: fz.ema(fz.identity(), EMA_P))),
        "rsi": timed_samples(scalar(lambda: fz.rsi(fz.identity(), RSI_P))),
        "stddev": timed_samples(scalar(lambda: fz.stddev(fz.identity(), STDDEV_P))),
        "atr": timed_samples(lambda: fz.atr(ATR_P).feed(frame)),
        "correlation": timed_samples(
            lambda: fz.correlation(fz.identity(), fz.identity(), CORREL_P).feed(c)
        ),
        "linreg_slope": timed_samples(
            lambda: fz.linreg(fz.identity(), LINREG_P).shared().slope().feed(c)
        ),
        # `PyMulti.feed` returns every line as its own column from one pass, so
        # these are the same unit of work as the `talib` calls above.
        "macd": timed_samples(lambda: fz.macd(
            fz.identity(), MACD_FAST, MACD_SLOW, MACD_SIGNAL).feed(c)),
        "bbands": timed_samples(lambda: fz.bollinger(
            fz.identity(), BBANDS_P, BBANDS_K).feed(c)),
        "aroon": timed_samples(lambda: fz.aroon(AROON_P).feed(frame)),
        "dmi": timed_samples(lambda: fz.dmi(DMI_P).feed(frame)),
        "adx": timed_samples(lambda: fz.adx(DMI_P).feed(frame)),
    }


def pooled(passes: list[dict[str, list[float]]]) -> dict[str, list[float]]:
    """Every sample from every pass, per key, ascending."""
    out: dict[str, list[float]] = {}
    for p in passes:
        for k, xs in p.items():
            out.setdefault(k, []).extend(xs)
    return {k: sorted(v) for k, v in out.items()}


def best_of(pool: dict[str, list[float]]) -> dict[str, float]:
    """The reported figure: the minimum sample.

    Not a mean or a median. Contention and frequency scaling are **one-sided** —
    they can only make a run slower — so the fastest sample is the one least
    polluted by them, and more sampling makes it converge. A mean over a drifting
    window converges on nothing in particular: this machine has produced 5.42,
    12.04, 13.50 and 17.70 ns/sample for `talib.ATR`, which is fixed C code.
    """
    return {k: xs[0] for k, xs in pool.items()}


def run_worker(which: str, n: int) -> int:
    """One tier, one process, samples on stdout. See `python_tier`."""
    global N
    N = n
    o, h, lo, c = synth(n)
    if which == "talib_py":
        samples = talib_py_tier(c, h, lo)
    elif which == "fugazi_py":
        samples = fugazi_py_tier(c, h, lo, o)
    else:
        print(f"unknown tier {which!r}", file=sys.stderr)
        return 2
    for name, xs in samples.items():
        listed = ",".join(f"{x:.4f}" for x in xs)
        print(f'{{"name":"{name}","ns_per_sample":{xs[0]:.4f},"samples":[{listed}]}}')
    return 0


def main() -> int:
    for arg in sys.argv[1:]:
        if arg.startswith("--tier="):
            n = N
            for a in sys.argv[1:]:
                if a.startswith("--n="):
                    n = int(a.split("=", 1)[1])
            return run_worker(arg.split("=", 1)[1], n)

    if check_extension_fresh() != 0:
        return 1

    # How many full passes to take before reporting the per-cell **minimum**.
    #
    # Not paranoia, and not the same job as `WARMUP`. The warm-up handles CPU
    # frequency ramp *within* a process; this handles contention *between*
    # processes, which on a shared machine is the larger effect — the first pass
    # after any other activity reads 30-40% slow on the tiers that are a fresh
    # subprocess (native C, Rust), while later passes agree to ~2%.
    #
    # Minimum rather than median because contention is strictly one-sided: it
    # can only ever make a run slower, so the fastest observation is the least
    # polluted. A median folds the contended passes back in.
    repeat = int(os.environ.get("FUGAZI_BENCH_REPEAT", "5"))

    # ...and how many more it may take if the minimum is still falling.
    #
    # A fixed pass count is the wrong stopping rule for a statistic whose whole
    # justification is that more sampling makes it converge. Reporting after
    # exactly `repeat` passes says nothing about whether pass `repeat + 1` would
    # have moved a cell by 20%, and on this machine it does: a run quoted at
    # `aroon` 9.63 ns/sample was re-measured at 9.30 by the same binary, because
    # the first run's minimum had simply not converged. A number nobody can
    # reproduce is not a number.
    #
    # So: keep taking passes until **no cell's minimum has improved by more than
    # `TOL` for `STABLE_PASSES` consecutive passes**, then stop. That is a
    # statement about the reported figures themselves — each one has survived
    # three further attempts to beat it — rather than about how long the loop ran.
    max_passes = int(os.environ.get("FUGAZI_BENCH_MAX_PASSES", "30"))
    TOL = 0.01
    STABLE_PASSES = 3

    o, h, lo, c = synth(N)

    print(f"n = {N:,} samples, {REPS} reps x >= {repeat} passes, reporting the "
          f"minimum\nsampling until no cell improves by > {TOL:.0%} for "
          f"{STABLE_PASSES} consecutive passes\n")
    print(f"load average at start: {open('/proc/loadavg').read().split()[0]}\n")

    # Round-robin, not tier-by-tier: the machine drifts on a timescale of
    # minutes, so measuring all of one tier and then all of another compares two
    # different machines. Interleaving puts every tier in the same conditions.
    passes: dict[str, list[dict[str, list[float]]]] = {
        "talib_c": [], "talib_py": [], "fugazi_rs": [], "fugazi_py": [],
    }

    def minima() -> dict[str, float]:
        """Every tier's per-indicator minimum so far, flattened to one dict."""
        out = {}
        for tier, ps in passes.items():
            for k, xs in pooled(ps).items():
                out[f"{tier}/{k}"] = xs[0]
        return out

    stable, taken, converged = 0, 0, False
    prev = {}
    while taken < max_passes:
        taken += 1
        print(f"  pass {taken}"
              f"{f' (stable {stable}/{STABLE_PASSES})' if stable else ''}",
              file=sys.stderr)
        passes["talib_c"].append(talib_native(N))
        passes["fugazi_rs"].append(rust_tier(N))
        passes["talib_py"].append(python_tier("talib_py", N))
        passes["fugazi_py"].append(python_tier("fugazi_py", N))

        cur = minima()
        if taken >= repeat:
            # Relative improvement of the worst-moving cell. Only cells present
            # in both snapshots count; a tier that skipped cannot destabilise it.
            moved = max(
                ((prev[k] - v) / prev[k] for k, v in cur.items()
                 if k in prev and prev[k] > 0),
                default=0.0,
            )
            stable = stable + 1 if moved <= TOL else 0
            if stable >= STABLE_PASSES:
                converged = True
                break
        prev = cur

    print(f"\nload average at end:   {open('/proc/loadavg').read().split()[0]}")
    if converged:
        print(f"converged after {taken} passes — no cell improved by more than "
              f"{TOL:.0%} over the last {STABLE_PASSES}.\n")
    else:
        # Never silently. A capped run's figures are upper bounds that were still
        # falling when the loop gave up, which is a different claim from the one
        # the table normally makes.
        print(f"*** DID NOT CONVERGE in {taken} passes (cap "
              f"FUGAZI_BENCH_MAX_PASSES={max_passes}). The figures below are "
              f"still falling — treat them as upper bounds, and re-run on a "
              f"quieter machine before quoting them. ***\n")

    pool = {tier: pooled(ps) for tier, ps in passes.items()}
    native_ns = best_of(pool["talib_c"])
    talib_ns = best_of(pool["talib_py"])
    rust_ns = best_of(pool["fugazi_rs"])
    py_ns = best_of(pool["fugazi_py"])
    if not rust_ns:
        return 1

    # Every sample kept, so the distribution can be re-analysed or re-plotted
    # without re-running the benchmark — and so the chart is reproducible from
    # committed data rather than from a number someone typed.
    raw_path = os.path.join(ROOT, "docs", "assets", "performance-samples.json")
    os.makedirs(os.path.dirname(raw_path), exist_ok=True)
    with open(raw_path, "w") as f:
        json.dump(
            {"n": N, "reps": REPS, "passes": taken, "converged": converged,
             "tol": TOL, "stable_passes": STABLE_PASSES,
             "unit": "ns_per_sample", "samples": pool},
            f, indent=1, sort_keys=True,
        )
    print(f"raw samples -> {os.path.relpath(raw_path, ROOT)}\n")

    # ---- report -------------------------------------------------------------
    # Each ratio is against the baseline that matches its tier: Rust against
    # native C, Python against the Python bindings. Mixing them is what made
    # fugazi's ATR look like a win when it is a loss.
    if not native_ns:
        print("native TA-Lib tier skipped (no C toolchain / TA-Lib headers);\n"
              "the rs/C column cannot be shown, and rs must NOT be judged\n"
              "against the Python-binding column instead.\n")

    print(f"{'':<10}{'TA-Lib C':>11}{'TA-Lib py':>11}{'fugazi rs':>11}"
          f"{'fugazi py':>11}{'rs vs C':>10}{'py vs py':>10}")
    print(f"{'indicator':<10}{'ns/samp':>11}{'ns/samp':>11}{'ns/samp':>11}"
          f"{'ns/samp':>11}{'(engine)':>10}{'(bindings)':>10}")
    for k in SCALAR + MULTI:
        if k == MULTI[0]:
            print(f"{'-- multi-output ' + '-' * 48}")
        nat = native_ns.get(k)
        t = talib_ns.get(k)
        r = rust_ns.get(k)
        p = py_ns.get(k)
        cell = lambda v: f"{v:>11.2f}" if v else f"{'—':>11}"
        row = f"{k:<10}" + cell(nat) + cell(t) + cell(r) + cell(p)
        row += f"{r / nat:>9.2f}x" if (nat and r) else f"{'—':>10}"
        row += f"{p / t:>9.2f}x" if (t and p) else f"{'—':>10}"
        print(row)

    # Spread is reported next to the figures on purpose: it is what says whether
    # a difference between two of them means anything.
    print(f"\n{'':<10}{'spread (max/min over all samples)':<40}")
    for tier, label in (("talib_c", "TA-Lib C"), ("fugazi_rs", "fugazi rs"),
                        ("talib_py", "TA-Lib py"), ("fugazi_py", "fugazi py")):
        worst = max((xs[-1] / xs[0]) for xs in pool[tier].values() if xs)
        print(f"{'':<10}{label:<14}up to {worst:.2f}x")

    print(
        "\nTA-Lib is vectorised (one C call per array); fugazi is incremental\n"
        "(one update() per sample), which is what lets the same code drive a\n"
        "live stream. `rs vs C` is the price of that design, measured against\n"
        "the C library itself. `py vs py` is what a Python caller gives up\n"
        "against the Python API they'd otherwise reach for.\n"
        "\nBoth ratios: lower is better, < 1.00x means fugazi is faster."
    )
    return 0


if __name__ == "__main__":
    sys.exit(main())
