#!/usr/bin/env python3
"""One-shot generator for the committed wallet-test input fixture.

Writes `tests/data/wallet_bars.csv` — the bar series **and** the order schedule
that `gen_wallet_fixtures.py` and `tests/wallet_validation.rs` both replay. One
file rather than two because the schedule is only meaningful against these
exact bars: it is indexed by bar, and the whole point of the cross-check is
that both sides consume an identical input.

Columns:
    bar, open, high, low, close, volume, target

`target` is the **position the schedule asks for**, in units, submitted at the
close of that bar and therefore filled at the *next* bar's open (fugazi's queue
rule; see docs/TRADING.md §2). An empty cell means "no submission this bar".

The schedule is hand-written rather than drawn, because its job is to walk the
paths that matter — open long, add to a long, partial reduce, full exit, flat
gap, open short, reverse straight through zero — not to look like a strategy.
The prices around it are a fixed-seed draw so the file is reproducible.

Rerun only when the fixture needs to be replaced, and regenerate
`wallet_expected.csv` alongside it:

    pixi run gen-bars && pixi run gen-wallet
"""

import csv
import os

import numpy as np

ROOT = os.path.dirname(os.path.dirname(os.path.abspath(__file__)))
OUT = os.path.join(ROOT, "tests", "data", "wallet_bars.csv")

N = 60
SEED = 20260817
START_PRICE = 100.0
DAILY_VOL = 0.012

# bar -> target position in units. Deliberately covers, in order: a flat run
# before the first entry, an entry, an add, a partial reduce, a full exit, a
# flat gap, a short entry, a short add, a reversal through zero to long, and a
# final exit — with the last bars flat so the closing equity is pure cash.
#
# Sizes stay well inside the cash budget: `PaperWallet` refuses a buy it cannot
# fund (no margin), and vectorbt would silently clip such an order instead of
# refusing it, so an over-large size would compare two different runs rather
# than fail. `tests/wallet_validation.rs` asserts no rejection occurred, which
# is what actually holds this constraint.
SCHEDULE = {
    5: 40.0,
    12: 65.0,
    19: 30.0,
    26: 0.0,
    33: -35.0,
    40: -55.0,
    47: 25.0,
    54: 0.0,
}


def main() -> None:
    rng = np.random.default_rng(SEED)
    steps = rng.normal(loc=0.0002, scale=DAILY_VOL, size=N)
    close = START_PRICE * np.cumprod(1.0 + steps)

    # An open that is genuinely distinct from the previous close (so a fill at
    # the open is visibly not a fill at the signal bar's close), and a high/low
    # that always bracket both — `fill_at` rejects a theoretical price outside
    # the bar's range, so the fixture must never manufacture one.
    gap = rng.normal(loc=0.0, scale=0.003, size=N)
    open_ = close * (1.0 + gap)
    high = np.maximum(open_, close) * (1.0 + np.abs(rng.normal(0.0, 0.004, N)))
    low = np.minimum(open_, close) * (1.0 - np.abs(rng.normal(0.0, 0.004, N)))
    volume = rng.uniform(5e5, 2e6, size=N)

    with open(OUT, "w", newline="") as f:
        w = csv.writer(f)
        w.writerow(["bar", "open", "high", "low", "close", "volume", "target"])
        for i in range(N):
            target = SCHEDULE.get(i)
            w.writerow(
                [
                    i,
                    repr(float(open_[i])),
                    repr(float(high[i])),
                    repr(float(low[i])),
                    repr(float(close[i])),
                    repr(float(volume[i])),
                    "" if target is None else repr(float(target)),
                ]
            )
    print(f"wrote {OUT} ({N} bars, {len(SCHEDULE)} submissions, seed={SEED})")


if __name__ == "__main__":
    main()
