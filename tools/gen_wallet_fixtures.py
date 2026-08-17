#!/usr/bin/env python3
"""Generate vectorbt reference values for fugazi's wallet cross-validation.

Reads the committed bar series + order schedule from `tests/data/wallet_bars.csv`
and replays it through `vectorbt.Portfolio.from_orders`, writing per-bar cash,
position and equity into `tests/data/wallet_expected.csv` — which
`tests/wallet_validation.rs` compares against a `PaperWallet` driven over the
identical input.

Usage (pixi, recommended — `pixi.lock` pins the exact build that produced the
committed fixture):
    pixi run gen-wallet
    cargo test --test wallet_validation

Why vectorbt rather than a backtesting framework: `from_orders` takes explicit
per-bar size/price/fee arrays and has **no decision layer of its own**. The
fugazi order schedule is therefore *replayed* into it, not approximated by a
strategy that happens to trade similarly — so a divergence is always the
wallet's arithmetic and never a difference of opinion about when to trade.

## What is being pinned

fugazi queues a market order at bar N and fills it at bar N+1's **open**
(docs/TRADING.md §2 — the rule that keeps a backtest from trading on
information it could not have had). That is expressed here by placing the
vectorbt order one row later than the submission, priced at that row's open.
If fugazi ever filled at the signal bar's close instead, every downstream cash
and equity figure would move and this suite would go red — which is the single
most valuable thing it checks.

## Cost configurations

Five runs over the same schedule, because a single all-costs-on run would make
any mismatch unattributable:

    zero        no costs                    — pure cash / position / equity
    commission  PercentageCommission(rate)  -> vbt `fees`
    spread      FixedBpsSpread(bps)         -> vbt `slippage`, at *half* the bps
    slippage    FixedBpsSlippage(bps)       -> vbt `slippage`
    full        commission + slippage       -> vbt `fees` + `slippage`

Every one of those maps onto vectorbt exactly, so every one is an independent
check rather than a restatement of fugazi's own formula.

## Why `full` does not include the spread leg

Because it cannot, and the reason is worth writing down — it was found by this
suite going red rather than by reading the code.

fugazi applies the two price legs multiplicatively and in order
(`PaperWallet::fill_at`): `post_spread = p·(1 ± a)`, then `final =
post_spread·(1 ± b)`. vectorbt has a single adverse-price knob, so expressing
both at once means folding them into one fraction `f`. But the fold is not
side-symmetric:

    buy   (1 + a)(1 + b) − 1 =  a + b + ab
    sell  1 − (1 − a)(1 − b) =  a + b − ab

The `ab` cross-term flips sign. One `f` therefore matches fugazi's buys or its
sells, never both — with `a = 4e-4` and `b = 5e-4` the gap is 2·ab = 4e-7 of
the fill price, which is far too small to notice by eye and far too large to
hide from a 1e-9 comparison.

So spread is checked on its own, where a single leg *is* symmetric and the
mapping is exact. The composition of the two legs is fugazi's own stated rule
with no outside opinion to check it against, and it stays where it already is:
the unit tests in `src/costs/mod.rs`. Folding it in here would have meant the
generator reproducing `fill_at`'s arithmetic and then confirming that `fill_at`
agrees with it, which is not a cross-check.

Constants must match `tests/wallet_validation.rs`.
"""

import csv
import os

import numpy as np
import pandas as pd
import vectorbt as vbt
from vectorbt.portfolio.enums import Direction, SizeType

ROOT = os.path.dirname(os.path.dirname(os.path.abspath(__file__)))
IN_CSV = os.path.join(ROOT, "tests", "data", "wallet_bars.csv")
OUT_CSV = os.path.join(ROOT, "tests", "data", "wallet_expected.csv")

# Must match tests/wallet_validation.rs.
INITIAL_CASH = 10_000.0
COMMISSION_RATE = 0.001  # PercentageCommission(0.001) == vbt fees=0.001
SLIPPAGE_BPS = 5.0  # FixedBpsSlippage(5.0)      == vbt slippage=5e-4
SPREAD_BPS = 8.0  # FixedBpsSpread(8.0): half-spread is *half* of this


def load_bars() -> pd.DataFrame:
    rows = list(csv.DictReader(open(IN_CSV, newline="")))
    return pd.DataFrame(
        {
            "open": [float(r["open"]) for r in rows],
            "high": [float(r["high"]) for r in rows],
            "low": [float(r["low"]) for r in rows],
            "close": [float(r["close"]) for r in rows],
            "target": [float(r["target"]) if r["target"] else np.nan for r in rows],
        }
    )


def shift_to_fill(target: pd.Series) -> pd.Series:
    """Move each submission one bar later — fugazi fills at the *next* open.

    A submission on the final bar has no bar to fill on and is dropped, exactly
    as `PaperWallet` drops a queued move the run ends before flushing.
    """
    return target.shift(1)


def run_config(bars: pd.DataFrame, *, fees: float, slippage: float) -> vbt.Portfolio:
    size = shift_to_fill(bars["target"])
    return vbt.Portfolio.from_orders(
        close=bars["close"],
        size=size,
        size_type=SizeType.TargetAmount,
        direction=Direction.Both,
        price=bars["open"],
        fees=fees,
        slippage=slippage,
        init_cash=INITIAL_CASH,
        # Refuse rather than silently clip. A schedule vectorbt quietly shrank
        # would compare two different runs and still pass; fugazi's wallet
        # rejects an unaffordable order outright, so the reference must too.
        allow_partial=False,
        raise_reject=True,
        freq="1D",
    )


# `FixedBpsSpread(bps)` charges a *half*-spread of `bps/2` on each side, which
# is what vectorbt's one-sided adverse `slippage` fraction expresses. The `/2`
# is the whole mapping; getting it wrong is a factor-of-two error that this
# suite catches immediately.
HALF_SPREAD_FRAC = SPREAD_BPS * 1e-4 / 2.0
SLIPPAGE_FRAC = SLIPPAGE_BPS * 1e-4

CONFIGS = {
    "zero": dict(fees=0.0, slippage=0.0),
    "commission": dict(fees=COMMISSION_RATE, slippage=0.0),
    "spread": dict(fees=0.0, slippage=HALF_SPREAD_FRAC),
    "slippage": dict(fees=0.0, slippage=SLIPPAGE_FRAC),
    # Deliberately no spread — see the module docstring. Two price legs cannot
    # be folded into vectorbt's single knob without breaking buy/sell symmetry.
    "full": dict(fees=COMMISSION_RATE, slippage=SLIPPAGE_FRAC),
}


def main() -> None:
    bars = load_bars()
    n = len(bars)
    columns: dict[str, np.ndarray] = {}

    for name, kw in CONFIGS.items():
        pf = run_config(bars, **kw)
        cash = np.asarray(pf.cash(), dtype=float).reshape(-1)
        position = np.asarray(pf.assets(), dtype=float).reshape(-1)
        equity = np.asarray(pf.value(), dtype=float).reshape(-1)
        assert len(cash) == n, f"{name}: vectorbt returned {len(cash)} rows, want {n}"
        columns[f"{name}.cash"] = cash
        columns[f"{name}.position"] = position
        columns[f"{name}.equity"] = equity

        # An all-flat run would make every column trivially equal and the
        # cross-check vacuous. The schedule opens both a long and a short, so
        # the position series must take both signs.
        assert position.max() > 0.0 and position.min() < 0.0, (
            f"{name}: schedule never held both a long and a short — "
            "the fixture is not exercising what it claims to"
        )

    names = list(columns)
    with open(OUT_CSV, "w", newline="") as f:
        w = csv.writer(f)
        w.writerow(["bar", *names])
        for i in range(n):
            w.writerow([i, *(repr(float(columns[c][i])) for c in names)])

    print(f"wrote {OUT_CSV} ({n} bars x {len(names)} columns, {len(CONFIGS)} configs)")


if __name__ == "__main__":
    main()
