#!/usr/bin/env python3
"""Generate backtesting.py reference values for fugazi's trade-metrics test.

Runs a scripted order schedule through
[backtesting.py](https://kernc.github.io/backtesting.py/) and writes two files:

  * `tests/data/trade_metrics_fills.csv`    — the resulting fill blotter and
    equity curve, i.e. the **input** both sides consume.
  * `tests/data/trade_metrics_expected.csv` — one `(metric, expected)` row per
    reference figure, from backtesting.py's own statistics.

`tests/metrics_validation.rs` covers everything that can be derived from an
equity curve, because empyrical takes a returns series and nothing else. That
leaves the whole of `trades.*` — win rate, profit factor, payoff, Kelly,
exposure, streaks, durations — with no external reference at all. This fills
that gap: backtesting.py is the one reference library of the four that reports
trade-level statistics computed from a blotter.

Usage (pixi, recommended):
    pixi run gen-trades
    cargo test --test trade_metrics_validation

## Why the generator emits the input too

backtesting.py decides its own fills. Rather than try to make fugazi reproduce
them, the blotter backtesting.py actually produced is written out and *fed to
fugazi as input*. Both sides therefore see byte-identical fills by construction,
and any divergence is a disagreement about the statistics — which is the thing
under test — rather than about which trades happened.

## Scope: non-overlapping round trips only

The schedule never adds to a position, never partially reduces one, and never
reverses through zero. That is not laziness, it is the boundary of what the two
libraries can honestly be compared on:

  * fugazi reconstructs trades at **volume-weighted average cost**
    (`metrics::reconstruct_trades`).
  * backtesting.py splits per entry order, FIFO.

For flat -> position -> flat round trips the two conventions provably coincide,
so every trade lines up one-for-one. For adds and partial closes they do not,
and a comparison there would be measuring the difference in bookkeeping rather
than an error. Those paths stay covered by the unit tests in `src/metrics.rs`.

## Zero commission, deliberately

`COMMISSION = 0.0`, and that is load-bearing rather than convenient.
backtesting.py reports trade PnL **net of commission**; fugazi's
`reconstruct_trades` computes PnL from fill prices alone and books commission
separately (`metrics::costs_section`). With commission on, every PnL-derived
figure below would differ by a known amount and the suite would be testing that
offset rather than the statistics. With it off the two agree exactly.

The cost pipeline is not going uncovered as a result — `tests/wallet_validation.rs`
checks commission, spread and slippage against vectorbt, one leg at a time.

Constants must match `tests/trade_metrics_validation.rs`.
"""

import csv
import os

import numpy as np
import pandas as pd
from backtesting import Backtest, Strategy

ROOT = os.path.dirname(os.path.dirname(os.path.abspath(__file__)))
FILLS_CSV = os.path.join(ROOT, "tests", "data", "trade_metrics_fills.csv")
OUT_CSV = os.path.join(ROOT, "tests", "data", "trade_metrics_expected.csv")

# Must match tests/trade_metrics_validation.rs.
INITIAL_CASH = 100_000.0
COMMISSION = 0.0  # see the module docstring — this must stay zero
BARS_PER_YEAR = 252.0
RISK_FREE_RATE = 0.0
N = 240
SEED = 20260818

# bar -> action. Strictly flat -> position -> flat, with varied holding periods
# and both sides represented, so win/loss streaks, durations and the drawdown
# segments are all non-trivial. Every entry has a matching close before the next
# entry; `schedule_is_non_overlapping` below enforces that rather than trusting
# it.
SCHEDULE = {
    10: "long", 24: "close",
    31: "short", 39: "close",
    46: "long", 72: "close",
    80: "short", 88: "close",
    95: "long", 101: "close",
    112: "long", 140: "close",
    150: "short", 163: "close",
    171: "long", 176: "close",
    184: "short", 205: "close",
    212: "long", 231: "close",
}


def schedule_is_non_overlapping() -> None:
    """Refuse to generate from a schedule the comparison would not be valid on.

    An `add` or a reversal would silently move this suite from "the two agree"
    to "the two book trades differently", which is exactly the failure the
    scope note above exists to prevent.
    """
    open_side = None
    for bar in sorted(SCHEDULE):
        action = SCHEDULE[bar]
        if action == "close":
            assert open_side is not None, f"bar {bar}: close with no open position"
            open_side = None
        else:
            assert open_side is None, (
                f"bar {bar}: {action} while already {open_side} — the schedule must "
                "be flat -> position -> flat (see the module docstring)"
            )
            open_side = action
    assert open_side is None, "schedule must end flat"


def make_bars() -> pd.DataFrame:
    """A fixed-seed price path with a real drawdown structure.

    A pure random walk tends to produce one dominant drawdown, which would make
    `avg_duration_bars` and `count` almost degenerate. The slow sinusoidal drift
    guarantees several distinct peak -> trough -> recovery stretches.
    """
    rng = np.random.default_rng(SEED)
    steps = rng.normal(loc=0.0003, scale=0.011, size=N)
    drift = 0.0009 * np.sin(np.arange(N) * 2.0 * np.pi / 55.0)
    close = 100.0 * np.cumprod(1.0 + steps + drift)
    gap = rng.normal(0.0, 0.002, size=N)
    open_ = close * (1.0 + gap)
    high = np.maximum(open_, close) * (1.0 + np.abs(rng.normal(0.0, 0.003, N)))
    low = np.minimum(open_, close) * (1.0 - np.abs(rng.normal(0.0, 0.003, N)))
    return pd.DataFrame(
        {
            "Open": open_,
            "High": high,
            "Low": low,
            "Close": close,
            "Volume": rng.uniform(1e6, 3e6, size=N),
        },
        index=pd.date_range("2020-01-01", periods=N, freq="D"),
    )


class Scripted(Strategy):
    """Executes SCHEDULE and nothing else — no indicators, no decisions."""

    def init(self):
        pass

    def next(self):
        action = SCHEDULE.get(len(self.data) - 1)
        if action == "long":
            self.buy()
        elif action == "short":
            self.sell()
        elif action == "close" and self.position:
            self.position.close()


def longest_underwater_run(equity: np.ndarray) -> int:
    """Longest stretch of bars strictly below a prior peak.

    An independent implementation of fugazi's `max_drawdown_duration`
    (`metrics::max_drawdown_duration`, which maxes each segment's
    `underwater_bars`). backtesting.py reports a *time* delta from peak to
    recovery, which is one bar longer by construction — it counts the recovery
    bar, fugazi counts only the bars actually below the peak. `main` asserts
    that relationship rather than leaving it to be rediscovered.
    """
    peak = equity[0]
    run = best = 0
    for e in equity:
        if e > peak:
            peak = e
            run = 0
        elif e < peak:
            run += 1
            best = max(best, run)
        else:
            run = 0
    return best


def underwater_runs(equity: np.ndarray) -> list[int]:
    """Every peak -> recovery stretch's length in bars, in order."""
    peak = equity[0]
    runs: list[int] = []
    run = 0
    for e in equity:
        if e > peak:
            if run:
                runs.append(run)
            peak = e
            run = 0
        elif e < peak:
            run += 1
    if run:
        runs.append(run)
    return runs


def fills_from_trades(trades: pd.DataFrame) -> list[tuple[int, float, float]]:
    """`(bar, signed_units, price)` per fill, entry and exit, in bar order.

    Valid only because the schedule never overlaps: each trade contributes an
    opening fill and a closing fill of the same magnitude, and no two trades are
    ever open at once.
    """
    fills: list[tuple[int, float, float]] = []
    for t in trades.itertuples():
        fills.append((int(t.EntryBar), float(t.Size), float(t.EntryPrice)))
        fills.append((int(t.ExitBar), -float(t.Size), float(t.ExitPrice)))
    fills.sort(key=lambda f: f[0])
    return fills


def main() -> None:
    schedule_is_non_overlapping()
    bars = make_bars()
    bt = Backtest(
        bars,
        Scripted,
        cash=INITIAL_CASH,
        commission=COMMISSION,
        trade_on_close=False,  # fugazi's rule: fill at the *next* bar's open
        margin=1.0,  # no leverage, matching `PaperWallet`'s no-margin rule
        finalize_trades=True,  # close anything still open, so no trade is lost
    )
    stats = bt.run()
    trades = stats["_trades"]
    equity = np.asarray(stats["_equity_curve"]["Equity"], dtype=float)

    assert len(trades) == sum(1 for a in SCHEDULE.values() if a != "close"), (
        f"backtesting.py booked {len(trades)} trades, schedule asks for "
        f"{sum(1 for a in SCHEDULE.values() if a != 'close')}"
    )
    assert (trades["Size"] > 0).any() and (trades["Size"] < 0).any(), (
        "schedule must produce both long and short trades"
    )
    assert (trades["PnL"] > 0).any() and (trades["PnL"] < 0).any(), (
        "schedule must produce both winners and losers, or win-rate-derived "
        "metrics are degenerate"
    )

    # ---- the input both sides consume -------------------------------------
    fills = fills_from_trades(trades)
    by_bar: dict[int, list[tuple[float, float]]] = {}
    for bar, units, price in fills:
        by_bar.setdefault(bar, []).append((units, price))
    with open(FILLS_CSV, "w", newline="") as f:
        w = csv.writer(f)
        w.writerow(["bar", "equity", "fill_units", "fill_price"])
        for i in range(len(bars)):
            entries = by_bar.get(i, [])
            assert len(entries) <= 1, f"bar {i}: {len(entries)} fills, schedule overlaps"
            units, price = entries[0] if entries else ("", "")
            w.writerow(
                [
                    i,
                    repr(float(equity[i])),
                    repr(float(units)) if units != "" else "",
                    repr(float(price)) if price != "" else "",
                ]
            )

    # ---- the reference values ---------------------------------------------
    pnl = trades["PnL"].to_numpy(dtype=float)
    ret = trades["ReturnPct"].to_numpy(dtype=float)
    held = (trades["ExitBar"] - trades["EntryBar"]).to_numpy(dtype=float)
    wins, losses = pnl[pnl > 0.0], pnl[pnl < 0.0]

    # backtesting.py reports durations as Timedeltas over a 1-day index, so
    # `.days` is a bar count.
    max_dd_days = int(stats["Max. Drawdown Duration"].days)
    runs = underwater_runs(equity)
    assert max_dd_days == longest_underwater_run(equity) + 1, (
        "backtesting.py's Max. Drawdown Duration is expected to be exactly one "
        "bar longer than fugazi's underwater-bar count (it includes the "
        "recovery bar); got "
        f"{max_dd_days} vs {longest_underwater_run(equity)}"
    )

    # Four of backtesting.py's headline statistics answer a different question
    # from the fugazi field that shares their name. Each is checked here against
    # the value fugazi's definition implies, so the divergence is recorded and
    # asserted rather than discovered by someone comparing two reports:
    #
    #   Profit Factor     backtesting.py sums trade *ReturnPct*; fugazi sums
    #                     trade *PnL*. They differ whenever position sizes do.
    #   Avg. Trade [%]    backtesting.py takes the *geometric* mean of
    #                     (1 + ReturnPct); fugazi takes the arithmetic mean.
    #   Exposure Time     backtesting.py counts the entry and exit bars both as
    #                     held (x - e + 1 per round trip); fugazi counts the
    #                     span between the fills (x - e).
    #   Avg. Trade Dur.   backtesting.py rounds to whole days via Timedelta;
    #                     fugazi keeps the fractional bar count.
    #
    # The *matching* fields below are the stronger evidence — `Win Rate [%]`,
    # `Kelly Criterion`, `# Trades`, `Max. Trade Duration` and
    # `Equity Final [$]` are taken straight from backtesting.py and agree to
    # 1e-9 with no reinterpretation at all.
    bt_exposure = float(stats["Exposure Time [%]"])
    fugazi_exposure = float(held.sum()) / len(bars) * 100.0
    assert abs(bt_exposure - fugazi_exposure - len(trades) / len(bars) * 100.0) < 1e-9, (
        "expected backtesting.py's exposure to exceed fugazi's by exactly one "
        f"bar per round trip; got {bt_exposure} vs {fugazi_exposure}"
    )
    assert abs(float(stats["Profit Factor"]) - wins.sum() / -losses.sum()) > 1e-6, (
        "backtesting.py's Profit Factor now agrees with the PnL-weighted one — "
        "the note above is stale, re-check which convention it uses"
    )

    fields = {
        # --- straight from backtesting.py's own statistics -----------------
        "trades.total": float(stats["# Trades"]),
        "trades.win_rate_pct": float(stats["Win Rate [%]"]),
        "trades.kelly_fraction": float(stats["Kelly Criterion"]),
        "trades.max_bars": float(stats["Max. Trade Duration"].days),
        "run.final_equity": float(stats["Equity Final [$]"]),
        # --- derived from backtesting.py's trade table ---------------------
        # Its summary has no currency-denominated trade aggregates (everything
        # is a percentage), and four of its percentages use a different
        # convention from fugazi's like-named field (see above). Both cases
        # read the same table backtesting.py computes its own summary from.
        "trades.wins": float(len(wins)),
        "trades.losses": float(len(losses)),
        "trades.long_trades": float((trades["Size"] > 0).sum()),
        "trades.short_trades": float((trades["Size"] < 0).sum()),
        "trades.average_win": float(wins.mean()),
        "trades.average_loss": float(losses.mean()),
        "trades.largest_win": float(pnl.max()),
        "trades.largest_loss": float(pnl.min()),
        "trades.expectancy": float(pnl.mean()),
        "trades.payoff_ratio": float(wins.mean() / -losses.mean()),
        "trades.profit_factor": float(wins.sum() / -losses.sum()),
        "trades.average_return_pct": float(ret.mean() * 100.0),
        "trades.average_bars": float(held.mean()),
        "trades.min_bars": float(held.min()),
        "trades.exposure_pct": fugazi_exposure,
        "trades.total_fills": float(len(fills)),
        # --- derived from the equity curve, to fugazi's stated definitions --
        # No third-party opinion is available for these: backtesting.py counts
        # peak-to-recovery inclusive of the recovery bar (asserted above), and
        # reports no segment count at all.
        "drawdown.max_duration_bars": float(longest_underwater_run(equity)),
        "drawdown.count": float(len(runs)),
        "drawdown.avg_duration_bars": float(np.mean(runs)),
        "drawdown.time_in_drawdown_pct": float(sum(runs) / len(equity) * 100.0),
        # backtesting.py *does* have an opinion on mean drawdown depth, and it
        # is the same one: the mean of each segment's (peak - trough) / peak.
        # Reported positive, as fugazi reports it.
        "drawdown.avg_pct": -float(stats["Avg. Drawdown [%]"]),
        "drawdown.avg": -float(stats["Avg. Drawdown [%]"]) / 100.0,
    }

    assert not any(np.isnan(v) for v in fields.values()), "a reference value is NaN"

    with open(OUT_CSV, "w", newline="") as f:
        w = csv.writer(f)
        w.writerow(["metric", "expected"])
        for k, v in fields.items():
            w.writerow([k, repr(float(v))])

    print(
        f"wrote {FILLS_CSV} ({len(bars)} bars, {len(fills)} fills)\n"
        f"wrote {OUT_CSV} ({len(fields)} metrics from {len(trades)} trades)"
    )


if __name__ == "__main__":
    main()
