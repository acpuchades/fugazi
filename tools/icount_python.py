#!/usr/bin/env python3
"""Instructions per sample, through the Python boundary, deterministically.

    pixi run -e bench python3 tools/icount_python.py --only=sma,atr
    pixi run -e bench python3 tools/icount_python.py --list

Wall-clock on this machine is not good enough to answer "how far is fugazi's
Python surface from `talib`'s?". The gap being chased is single-digit
nanoseconds per sample; the machine is shared, and the numbers move by more than
that between passes. `callgrind` counts instructions retired, which is immune to
contention and reproduces exactly — so an A/B is trustworthy at a granularity
wall-clock cannot reach here.

# The trap this tool exists to avoid

The obvious way to use callgrind on a Python workload is to run the script and
subtract a "control" script that does everything except the work. That is what
was tried first, and it produced **0.05 instructions/sample for `talib.SMA`** —
impossible, since a single pass has to load each element at least once.

The reason is scale. Interpreter startup plus imports is ~0.9 G instructions.
Vectorised C over 200 000 samples is ~0.1 G. Subtracting two ~1 G numbers to
recover a 0.1 G signal only works if the control matches the signal run to
better than 10% — and it did not, because `import talib` and `import fugazi`
pull in different amounts of machinery. The residue was noise, and it happened
to land near zero.

**So the differential here is in the iteration count, not against a separate
script.** The same script runs at `n` and `2n` iterations of the identical loop;
everything outside the loop — startup, imports, building the input arrays — is
bit-identical between the two runs and cancels in the subtraction, exactly, with
no assumption that two different scripts resemble each other:

    instr/sample = (I(2n) - I(n)) / (n * samples)

That also means the input construction cost never contaminates the result, so
each workload can set up however is natural.

# The noise floor, and why `--n` is as large as it is

Callgrind is deterministic in principle; a Python process is not quite. Two
identical runs of the same worker differ by **~1 M instructions** out of ~834 M
even with `PYTHONHASHSEED` fixed (measured: 833 613 093 vs 834 542 049). Leaving
the seed unset widens that to ~7 M, so the tool sets it — but it does not
eliminate it, and the residue does not shrink with effort.

That floor sets the minimum credible signal. `talib.SMA` retires roughly 4
instructions/sample, so at 100 000 samples × 4 iterations its whole contribution
is ~1.6 M — the same size as the noise. That is not a hypothetical: the first
run of this tool reported **-42.50 instructions/sample** for `talib.SMA`, a
negative number, because `I(2n)` happened to land below `I(n)`.

So the defaults put the signal two orders of magnitude above the floor
(200 000 × 20 × 4 ≈ 16 M for the cheapest workload, ~180 M for a fugazi one), and
`--reps` takes the **minimum** of repeated endpoint measurements, on the grounds
that perturbations add instructions rather than remove them. If you lower `--n`
or `--samples`, check `noop` still reads ~0.00 — that workload exists precisely
to make the floor visible instead of letting it hide inside a real number.

# Reading the output

`ref` is the same measurement for the matching `talib` call, so the `x` column is
the like-for-like ratio a Python user actually experiences. `--` in `ref` means
the workload has no `talib` counterpart (`close` is a raw field read, which
`talib` has no equivalent for).

Instructions are not nanoseconds: `talib`'s inner loops are SIMD and retire more
work per instruction than fugazi's scalar `update()`, so a 1.0 ratio here is
*better* than parity in wall-clock terms, not equal to it. Use this tool for
"did my change help, and by how much", and `tools/bench_three_tier.py` for "what
does a user see". They answer different questions and will not agree.
"""

from __future__ import annotations

import argparse
import os
import re
import subprocess
import sys
import tempfile

REPO = os.path.dirname(os.path.dirname(os.path.abspath(__file__)))

# Each workload is a (setup, loop-body) pair evaluated in the worker process.
# `SAMPLES` rows are prepared once in the setup; only the body is measured.
#
# The fugazi bodies rebuild the chain each iteration on purpose — that is what a
# caller does, and `feed` over a fresh chain is the whole product. Keeping a
# chain alive and calling `reset()` would measure something nobody runs.
WORKLOADS: dict[str, tuple[str, str, str | None]] = {
    # name: (setup, body, talib reference body)
    #
    # The control. Both sides do nothing, so both columns must read ~0.00 — that
    # is the check that the iteration-count differential is actually cancelling
    # startup, and it is the only way to see the noise floor described above
    # rather than have it hide inside a real measurement. If this is not ~0.00,
    # nothing else in the table means anything.
    "noop": (
        "",
        "pass",
        "pass",
    ),
    "close": (
        "",
        "ta.close().feed(frame)",
        None,
    ),
    "sma": (
        "",
        "ta.sma(ta.close(), 14).feed(frame)",
        "talib.SMA(close, 14)",
    ),
    "ema": (
        "",
        "ta.ema(ta.close(), 14).feed(frame)",
        "talib.EMA(close, 14)",
    ),
    "rsi": (
        "",
        "ta.rsi(ta.close(), 14).feed(frame)",
        "talib.RSI(close, 14)",
    ),
    "atr": (
        "",
        "ta.atr(14).feed(frame)",
        "talib.ATR(high, low, close, 14)",
    ),
    "stddev": (
        "",
        "ta.stddev(ta.close(), 14).feed(frame)",
        "talib.STDDEV(close, 14)",
    ),
    # The scalar path: a 1-D value stream, no bar fields. `talib` sees the same
    # array, so this is the one comparison with no frame handling on either side.
    "sma_1d": (
        "",
        "ta.sma(ta.identity(), 14).feed(close)",
        "talib.SMA(close, 14)",
    ),
}

WORKER = r'''
import sys
import numpy as np

n = int(sys.argv[1])
samples = int(sys.argv[2])
mode = sys.argv[3]

# A deterministic random walk, so two runs of this script at different `n` see
# byte-identical inputs and the subtraction is exact.
rng = np.random.default_rng(20260817)
close = 100.0 + np.cumsum(rng.standard_normal(samples) * 0.5)
high = close + np.abs(rng.standard_normal(samples)) * 0.3
low = close - np.abs(rng.standard_normal(samples)) * 0.3
open_ = close + rng.standard_normal(samples) * 0.1
volume = np.abs(rng.standard_normal(samples)) * 1000.0

if mode == "fugazi":
    import fugazi as ta
    frame = {"open": open_, "high": high, "low": low,
             "close": close, "volume": volume}
else:
    import talib

SETUP

for _ in range(n):
    BODY
'''


def worker_source(setup: str, body: str) -> str:
    return WORKER.replace("SETUP", setup).replace("BODY", body)


IR = re.compile(r"^summary:\s+([0-9]+)", re.M)

# `talib` retires roughly a tenth of the instructions fugazi does per sample (its
# inner loops are vectorised C; fugazi's are a scalar `update()` per bar). At a
# shared iteration count its whole contribution would sit near the noise floor —
# which is how the first version of this tool reported a *negative* instruction
# count for `talib.SMA`. So the reference side runs proportionally longer, to put
# both signals in the same place relative to the floor rather than to make the
# two runs superficially symmetric.
REF_SCALE = 8


def icount(path: str, n: int, samples: int, mode: str, reps: int) -> int:
    """Instructions retired by one worker run, from callgrind's own summary.

    The minimum over `reps` runs: interference adds instructions (a signal
    handler, a page fault taking a slower path) and does not remove them, so the
    smallest observation is the closest to the un-perturbed cost.
    """
    env = dict(os.environ)
    # Import-time dict and set iteration order is hash-dependent, and that
    # changes how many instructions the imports retire. Fixing the seed takes
    # run-to-run spread from ~7 M to ~1 M; see the module docstring.
    env["PYTHONHASHSEED"] = "0"
    # OpenBLAS starts a pool of spinning worker threads when numpy is imported.
    # `callgrind_annotate` showed `blas_thread_server` retiring **36.7% of the
    # entire profile** — 466 M instructions of spin-wait, doing nothing for any
    # workload here. Worse than useless: the spin scales with how long the
    # process lives, so the `2n` run accumulates more of it than the `n` run and
    # it lands in the differential as if it were the measured code. Pinning the
    # pool to one thread removes it, and is the reason these numbers are stable
    # at all.
    env["OPENBLAS_NUM_THREADS"] = "1"
    env["OMP_NUM_THREADS"] = "1"
    best = None
    for _ in range(reps):
        with tempfile.NamedTemporaryFile(suffix=".out") as out:
            cmd = [
                "valgrind", "--tool=callgrind",
                f"--callgrind-out-file={out.name}",
                "--quiet",
                sys.executable, path, str(n), str(samples), mode,
            ]
            proc = subprocess.run(cmd, capture_output=True, text=True, env=env)
            if proc.returncode != 0:
                sys.stderr.write(proc.stderr[-4000:])
                raise SystemExit(f"worker failed (n={n}, mode={mode})")
            with open(out.name) as f:
                text = f.read()
        m = IR.search(text)
        if not m:
            raise SystemExit("callgrind emitted no summary line")
        v = int(m.group(1))
        best = v if best is None else min(best, v)
    assert best is not None
    return best


def measure(setup: str, body: str, mode: str, n: int, samples: int, reps: int) -> float:
    """Instructions per sample attributable to `body`.

    Two runs, `n` and `2n` iterations. Everything outside the loop is identical
    and cancels; what is left is `n` iterations of the body.
    """
    # A *private* directory, not a temp file in `/tmp` — CPython puts the
    # script's own directory at the head of `sys.path`, so a worker sitting
    # directly in `/tmp` imports any `/tmp/<stdlib name>.py` that happens to
    # exist in preference to the real module. This machine is shared, and a
    # stray `/tmp/copy.py` broke `import pandas` (via `dataclasses` → `copy`)
    # with a traceback that pointed at pandas and had nothing to do with it.
    with tempfile.TemporaryDirectory() as d:
        path = os.path.join(d, "worker.py")
        with open(path, "w") as f:
            f.write(worker_source(setup, body))
        lo = icount(path, n, samples, mode, reps)
        hi = icount(path, 2 * n, samples, mode, reps)
    return (hi - lo) / (n * samples)


def check_extension_fresh() -> None:
    """Refuse to measure a build that predates the source.

    This has produced a fictional result twice. The bench environment installs
    the wheel non-editable, so nothing rebuilds it implicitly; and because the
    wheel's filename carries the version, a `maturin build` after a version bump
    lands under a *new* name while the old wheel sits beside it, so an
    `unzip fugazi-<old>.whl` still succeeds and silently installs the previous
    build. Comparing mtimes is the only check that catches both.
    """
    import fugazi  # noqa: PLC0415 — imported here so --list works without it

    so = os.path.join(os.path.dirname(fugazi.__file__), "fugazi.abi3.so")
    built = os.path.getmtime(so)
    newest, newest_path = 0.0, ""
    for root, _, files in os.walk(os.path.join(REPO, "python", "src")):
        for name in files:
            if name.endswith(".rs"):
                p = os.path.join(root, name)
                t = os.path.getmtime(p)
                if t > newest:
                    newest, newest_path = t, p
    if newest > built:
        raise SystemExit(
            f"the installed extension is older than {os.path.relpath(newest_path, REPO)}\n"
            f"  {so}\n"
            "rebuild it before measuring, e.g.\n"
            "  cd python && maturin build --release -o /tmp/fz_wheels\n"
            "then unzip *the wheel that build just printed* over the installed .so."
        )


def main() -> int:
    ap = argparse.ArgumentParser()
    ap.add_argument("--only", default="", help="comma-separated workload names")
    ap.add_argument("--list", action="store_true")
    # `n` is large enough that even the cheapest workload's contribution is ~16 M
    # instructions against a ~1 M noise floor. See the module docstring.
    ap.add_argument("--n", type=int, default=20, help="loop iterations (doubled for the second run)")
    ap.add_argument("--samples", type=int, default=200_000)
    ap.add_argument("--reps", type=int, default=2, help="runs per endpoint; the minimum is kept")
    args = ap.parse_args()

    if args.list:
        for name, (_, body, ref) in WORKLOADS.items():
            print(f"{name:10s} {body}" + (f"   vs {ref}" if ref else ""))
        return 0

    names = [s for s in args.only.split(",") if s] or list(WORKLOADS)
    unknown = [n for n in names if n not in WORKLOADS]
    if unknown:
        raise SystemExit(f"unknown workload(s): {', '.join(unknown)}")
    check_extension_fresh()

    print(f"# callgrind instructions/sample, {args.samples} samples, "
          f"{args.n} vs {2 * args.n} iterations, best of {args.reps}")
    print(f"{'workload':10s} {'fugazi':>10s} {'talib':>10s} {'ratio':>8s}")
    for name in names:
        setup, body, ref = WORKLOADS[name]
        fz = measure(setup, body, "fugazi", args.n, args.samples, args.reps)
        if ref is None:
            print(f"{name:10s} {fz:10.2f} {'--':>10s} {'--':>8s}", flush=True)
            continue
        tl = measure(setup, ref, "talib", args.n * REF_SCALE, args.samples, args.reps)
        ratio = f"{fz / tl:7.2f}x" if abs(tl) > 0.01 else "--"
        print(f"{name:10s} {fz:10.2f} {tl:10.2f} {ratio:>8s}", flush=True)
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
