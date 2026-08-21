"""Python-side performance probe.

The Rust `benches/` measure the engine. This measures what a *Python* caller
actually pays, which is a different number: every `StrategySpec.run(...)` first
converts a Python sequence into `Vec<Snapshot<Sym>>` across the FFI boundary,
and that conversion allocates per symbol per bar. For a caller driving the
library from a notebook, the boundary can cost more than the backtest.

Run it with the project venv:

    python/.venv/bin/python python/bench/bench_run.py

Reports median-of-N wall clock, split into the two halves that scale
differently — snapshot construction (boundary) and the run itself (engine) — so
a change can be attributed to one or the other. Not a pass/fail gate and not
wired into CI; it is the Python twin of `scripts/perf-compare.sh`.
"""

from __future__ import annotations

import gc
import statistics
import sys
import time

import fugazi


BARS = 20_000
REPS = 5


def synth_candles(n: int) -> list[tuple[float, float, float, float, float]]:
    """The same deterministic LCG walk the Rust benches use, so the two sides
    are driving identical input."""
    out = []
    px = 100.0
    s = 0x5EED_1234_5678_9ABC
    mask = (1 << 64) - 1
    for _ in range(n):
        s = (s * 6364136223846793005 + 1442695040888963407) & mask
        noise = ((s >> 33) / 0xFFFFFFFF) - 0.5
        ret = 0.0002 + 0.01 * noise
        open_, close = px, px * (1.0 + ret)
        out.append(
            (open_, max(open_, close) * 1.001, min(open_, close) * 0.999, close, 1000.0)
        )
        px = close
    return out


SMA_YAML = """
symbol: {sym}
long:
  enter: !crosses_above
    lhs: !sma {{ source: close, period: 5 }}
    rhs: !sma {{ source: close, period: 20 }}
  exit: !crosses_below
    lhs: !sma {{ source: close, period: 5 }}
    rhs: !sma {{ source: close, period: 20 }}
"""


def build_snapshots(symbols: list[str], bars: int) -> list[dict]:
    """Snapshots as plain dicts — the shape a notebook user naturally builds,
    and the one that exercises the FFI symbol conversion hardest (one Python
    `str` key per symbol per bar)."""
    candles = synth_candles(bars)
    out = []
    for b in range(bars):
        row = {}
        for i, sym in enumerate(symbols):
            o, h, low, c, v = candles[(b + i * 7) % bars]
            row[sym] = fugazi.Candle(o, h, low, c, v)
        out.append(row)
    return out


def timed(fn, reps: int = REPS) -> float:
    """Median of `reps`, with the GC quiesced so a collection landing inside
    one sample does not become the reported number."""
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
    print(f"fugazi {getattr(fugazi, '__version__', '?')}  bars={BARS} reps={REPS}\n")
    print(f"{'case':<44}{'median s':>11}{'us/bar':>11}")

    for n_syms in (1, 8, 32):
        symbols = [f"SYMBOL{i:04d}" for i in range(n_syms)]
        rows = build_snapshots(symbols, BARS)

        # Boundary only: hand the same rows across FFI and throw the result
        # away. `Snapshot` construction is what a run pays before it starts.
        def convert():
            return [fugazi.Snapshot(r) for r in rows]

        t_conv = timed(convert)
        print(
            f"{f'snapshot conversion, {n_syms} sym':<44}"
            f"{t_conv:>11.4f}{t_conv * 1e6 / BARS:>11.2f}"
        )

        # Whole run from Python, single-asset spec on the first symbol.
        spec = fugazi.load_spec(SMA_YAML.format(sym=symbols[0]))
        snaps = [fugazi.Snapshot(r) for r in rows]

        def run():
            w = fugazi.PaperWallet(10_000.0)
            return spec.run(w, snaps)

        t_run = timed(run)
        print(
            f"{f'run (spec, pre-built snapshots), {n_syms} sym':<44}"
            f"{t_run:>11.4f}{t_run * 1e6 / BARS:>11.2f}"
        )

    return 0


if __name__ == "__main__":
    sys.exit(main())
