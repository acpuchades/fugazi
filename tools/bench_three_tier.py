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
import statistics
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

    out: dict[str, float] = {}
    for line in run.stdout.splitlines():
        line = line.strip()
        if line.startswith("{") and '"ns_per_sample"' in line:
            rec = json.loads(line)
            out[rec["name"]] = rec["ns_per_sample"]
    return out


def timed(fn, reps: int = REPS, warmup: int = WARMUP) -> float:
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
        times.append(time.perf_counter() - t)
        gc.enable()
    return statistics.median(times)


def rust_tier(n: int) -> dict[str, float]:
    """fugazi's Rust engine, via `cargo bench --bench three_tier --emit-json`."""
    env = dict(os.environ)
    env["FUGAZI_THREE_TIER_N"] = str(n)
    proc = subprocess.run(
        ["cargo", "bench", "--bench", "three_tier", "--", "--emit-json"],
        cwd=ROOT, capture_output=True, text=True, env=env,
    )
    out: dict[str, float] = {}
    for line in proc.stdout.splitlines():
        line = line.strip()
        if line.startswith("{") and '"ns_per_sample"' in line:
            rec = json.loads(line)
            out[rec["name"]] = rec["ns_per_sample"]
    if not out:
        print("could not read Rust numbers; cargo said:\n", proc.stderr[-2000:])
    return out


def talib_py_tier(c, h, lo) -> dict[str, float]:
    """TA-Lib through its Cython bindings — the baseline for the Python row."""
    return {
        "sma": timed(lambda: talib.SMA(c, SMA_P)) * 1e9 / N,
        "ema": timed(lambda: talib.EMA(c, EMA_P)) * 1e9 / N,
        "rsi": timed(lambda: talib.RSI(c, RSI_P)) * 1e9 / N,
        "stddev": timed(lambda: talib.STDDEV(c, STDDEV_P)) * 1e9 / N,
        "atr": timed(lambda: talib.ATR(h, lo, c, ATR_P)) * 1e9 / N,
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
        "sma": timed(scalar(lambda: fz.sma(fz.identity(), SMA_P))) * 1e9 / N,
        "ema": timed(scalar(lambda: fz.ema(fz.identity(), EMA_P))) * 1e9 / N,
        "rsi": timed(scalar(lambda: fz.rsi(fz.identity(), RSI_P))) * 1e9 / N,
        "stddev": timed(scalar(lambda: fz.stddev(fz.identity(), STDDEV_P))) * 1e9 / N,
        "atr": timed(lambda: fz.atr(ATR_P).feed(frame)) * 1e9 / N,
    }


def best_of(passes: list[dict[str, float]]) -> dict[str, float]:
    """Per-key minimum across passes. See the note on `repeat` in `main`."""
    out: dict[str, float] = {}
    for p in passes:
        for k, v in p.items():
            if k not in out or v < out[k]:
                out[k] = v
    return out


def main() -> int:
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
    repeat = int(os.environ.get("FUGAZI_BENCH_REPEAT", "3"))

    o, h, lo, c = synth(N)

    print(f"n = {N:,} samples, median of {REPS}, best of {repeat} passes\n")

    native_p, talib_p, rust_p, py_p = [], [], [], []
    for _ in range(repeat):
        native_p.append(talib_native(N))
        talib_p.append(talib_py_tier(c, h, lo))
        rust_p.append(rust_tier(N))
        py_p.append(fugazi_py_tier(c, h, lo, o))
    native_ns = best_of(native_p)
    talib_ns = best_of(talib_p)
    rust_ns = best_of(rust_p)
    py_ns = best_of(py_p)
    if not rust_ns:
        return 1

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
    for k in ("sma", "ema", "rsi", "stddev", "atr"):
        nat = native_ns.get(k)
        t = talib_ns.get(k)
        r = rust_ns.get(k)
        p = py_ns.get(k)
        cell = lambda v: f"{v:>11.2f}" if v else f"{'—':>11}"
        row = f"{k:<10}" + cell(nat) + cell(t) + cell(r) + cell(p)
        row += f"{r / nat:>9.2f}x" if (nat and r) else f"{'—':>10}"
        row += f"{p / t:>9.2f}x" if (t and p) else f"{'—':>10}"
        print(row)

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
