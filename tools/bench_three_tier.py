#!/usr/bin/env python3
"""Three-tier indicator throughput: TA-Lib vs fugazi (Rust) vs fugazi (Python).

Answers two questions the project cares about separately:

  1. Does the **Rust** engine match or beat TA-Lib? TA-Lib is a mature C
     library and the natural bar for a from-scratch Rust implementation.
  2. How much does a **Python** caller give up over that Rust engine? That gap
     is FFI overhead, and keeping it small is what makes the bindings usable.

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

Rust numbers come from `cargo bench --bench three_tier`, which this script
invokes and parses, so both tiers run the same input length.
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

# Keep in sync with benches/three_tier.rs and tools/gen_talib_fixtures.py.
SMA_P = 10
EMA_P = 10
RSI_P = 14
STDDEV_P = 10
ATR_P = 14


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


def timed(fn, reps: int = REPS) -> float:
    times = []
    for _ in range(reps):
        gc.collect()
        gc.disable()
        t = time.perf_counter()
        fn()
        times.append(time.perf_counter() - t)
        gc.enable()
    return statistics.median(times)


def main() -> int:
    o, h, lo, c = synth(N)

    print(f"n = {N:,} samples, median of {REPS}\n")

    # ---- tier 1: TA-Lib (vectorised C) --------------------------------------
    talib_ns = {
        "sma": timed(lambda: talib.SMA(c, SMA_P)) * 1e9 / N,
        "ema": timed(lambda: talib.EMA(c, EMA_P)) * 1e9 / N,
        "rsi": timed(lambda: talib.RSI(c, RSI_P)) * 1e9 / N,
        "stddev": timed(lambda: talib.STDDEV(c, STDDEV_P)) * 1e9 / N,
        "atr": timed(lambda: talib.ATR(h, lo, c, ATR_P)) * 1e9 / N,
    }

    # ---- tier 2: fugazi Rust ------------------------------------------------
    env = dict(os.environ)
    env["FUGAZI_THREE_TIER_N"] = str(N)
    proc = subprocess.run(
        ["cargo", "bench", "--bench", "three_tier", "--", "--emit-json"],
        cwd=ROOT, capture_output=True, text=True, env=env,
    )
    rust_ns: dict[str, float] = {}
    for line in proc.stdout.splitlines():
        line = line.strip()
        if line.startswith("{") and '"ns_per_sample"' in line:
            rec = json.loads(line)
            rust_ns[rec["name"]] = rec["ns_per_sample"]
    if not rust_ns:
        print("could not read Rust numbers; cargo said:\n", proc.stderr[-2000:])
        return 1

    # ---- tier 3: fugazi Python ---------------------------------------------
    import fugazi as fz

    def py_scalar(build):
        def run():
            ind = build()
            ind.feed(c)
        return run

    py_ns = {
        "sma": timed(py_scalar(lambda: fz.sma(fz.identity(), SMA_P))) * 1e9 / N,
        "ema": timed(py_scalar(lambda: fz.ema(fz.identity(), EMA_P))) * 1e9 / N,
        "rsi": timed(py_scalar(lambda: fz.rsi(fz.identity(), RSI_P))) * 1e9 / N,
        "stddev": timed(py_scalar(lambda: fz.stddev(fz.identity(), STDDEV_P))) * 1e9 / N,
    }

    # ---- report -------------------------------------------------------------
    print(f"{'indicator':<10}{'TA-Lib':>12}{'fugazi rs':>12}{'fugazi py':>12}"
          f"{'rs/TA-Lib':>12}{'py/rs':>9}")
    print(f"{'':<10}{'ns/sample':>12}{'ns/sample':>12}{'ns/sample':>12}")
    for k in ("sma", "ema", "rsi", "stddev", "atr"):
        t = talib_ns.get(k)
        r = rust_ns.get(k)
        p = py_ns.get(k)
        row = f"{k:<10}{t:>12.2f}" if t else f"{k:<10}{'—':>12}"
        row += f"{r:>12.2f}" if r else f"{'—':>12}"
        row += f"{p:>12.2f}" if p else f"{'—':>12}"
        row += f"{r / t:>11.2f}x" if (t and r) else f"{'—':>12}"
        row += f"{p / r:>8.1f}x" if (r and p) else f"{'—':>9}"
        print(row)

    print(
        "\nTA-Lib is vectorised (one C call per array); fugazi is incremental\n"
        "(one update() per sample), which is what lets the same code drive a\n"
        "live stream. Read `rs/TA-Lib` as the price of that design, and\n"
        "`py/rs` as the FFI overhead a Python caller adds on top."
    )
    return 0


if __name__ == "__main__":
    sys.exit(main())
