#!/usr/bin/env python3
"""Generate empyrical + numpy reference values for fugazi's metrics test.

Reads the committed returns fixture and writes one row per (metric, expected
value) into `tests/data/metrics_expected.csv`, which the Rust test loads and
compares against `metrics::compute` cell-by-cell.

Usage (pixi, recommended):
    pixi run gen-metrics

Use pixi rather than a fresh `pip install empyrical numpy` here. empyrical has
been unmaintained since 2020 and `downside_risk` still calls `np.NINF`, which
NumPy 2.0 removed — so an unpinned install resolves happily and then dies with
`AttributeError` partway through this script. `pixi.toml` holds numpy below 2
for exactly that reason, and `pixi.lock` pins the build that produced the
committed fixture.

Constants must match `tests/metrics_validation.rs` and the returns fixture.
"""

import csv
import os

import empyrical as ep
import numpy as np

ROOT = os.path.dirname(os.path.dirname(os.path.abspath(__file__)))
IN_CSV = os.path.join(ROOT, "tests", "data", "metrics_returns.csv")
OUT_CSV = os.path.join(ROOT, "tests", "data", "metrics_expected.csv")

# Must match tests/metrics_validation.rs.
INITIAL_CASH = 10_000.0
BARS_PER_YEAR = 252
RISK_FREE_RATE = 0.0  # fractional annualized; kept at 0 to sidestep the
# per-bar-conversion ambiguity between empyrical (geometric for
# omega_ratio, per-bar arithmetic for sharpe/sortino) and fugazi
# (arithmetic across the board).


def load_returns() -> np.ndarray:
    with open(IN_CSV, newline="") as f:
        rows = list(csv.DictReader(f))
    return np.array([float(r["return"]) for r in rows])


def var_pos(returns: np.ndarray, cutoff: float) -> float:
    """Fugazi expresses VaR as a positive loss magnitude (`-percentile`)."""
    return -float(np.percentile(returns, 100 * cutoff))


def cvar_pos(returns: np.ndarray, cutoff: float) -> float:
    """CVaR = mean of the bottom-cutoff tail, expressed as a positive loss."""
    return -float(ep.conditional_value_at_risk(returns, cutoff=cutoff))


def tail_ratio(returns: np.ndarray) -> float:
    return float(np.abs(np.percentile(returns, 95)) / np.abs(np.percentile(returns, 5)))


def ulcer_index_frac(equity: np.ndarray) -> float:
    """Fractional Ulcer Index (drawdown ratios, not percent points)."""
    peak = np.maximum.accumulate(equity)
    dd = (equity - peak) / peak
    return float(np.sqrt(np.mean(dd**2)))


def biased_skew(x: np.ndarray) -> float:
    """Population g1: m3 / m2^1.5 with an `n` divisor (scipy bias=True)."""
    mean = x.mean()
    m2 = ((x - mean) ** 2).mean()
    m3 = ((x - mean) ** 3).mean()
    return float(m3 / m2**1.5)


def biased_excess_kurt(x: np.ndarray) -> float:
    """Population g2 − 3 (scipy fisher=True, bias=True)."""
    mean = x.mean()
    m2 = ((x - mean) ** 2).mean()
    m4 = ((x - mean) ** 4).mean()
    return float(m4 / m2**2 - 3.0)


def main() -> None:
    returns = load_returns()
    n = len(returns)
    equity = INITIAL_CASH * np.cumprod(1.0 + returns)
    final_equity = float(equity[-1])
    total_return = final_equity / INITIAL_CASH - 1.0
    years = n / BARS_PER_YEAR

    # Downside stddev with rf-per-bar threshold (Sortino MAR); matches
    # empyrical.downside_risk(required_return=rf_per_bar).
    rf_per_bar = RISK_FREE_RATE / BARS_PER_YEAR
    ann_downside = float(
        ep.downside_risk(
            returns, required_return=rf_per_bar, annualization=BARS_PER_YEAR
        )
    )

    fields = {
        # RunSection
        "run.bars": float(n),
        "run.initial_equity": INITIAL_CASH,
        "run.final_equity": final_equity,
        "run.bars_per_year": float(BARS_PER_YEAR),
        "run.risk_free_rate": RISK_FREE_RATE,
        # ReturnSection
        "returns.total": total_return,
        "returns.total_pct": total_return * 100.0,
        "returns.cagr_pct": ((final_equity / INITIAL_CASH) ** (1.0 / years) - 1.0)
        * 100.0,
        "returns.mean_bar": float(returns.mean()),
        "returns.median_bar": float(np.median(returns)),
        "returns.stddev_bar": float(returns.std(ddof=1)),
        "returns.best_bar": float(returns.max()),
        "returns.worst_bar": float(returns.min()),
        "returns.positive_bars_pct": float((returns > 0).sum() / n * 100.0),
        "returns.skewness": biased_skew(returns),
        "returns.kurtosis": biased_excess_kurt(returns),
        "returns.var_95": var_pos(returns, 0.05),
        "returns.cvar_95": cvar_pos(returns, 0.05),
        "returns.tail_ratio": tail_ratio(returns),
        "returns.annualized_mean_pct": float(returns.mean()) * BARS_PER_YEAR * 100.0,
        "returns.annualized_volatility_pct": float(
            ep.annual_volatility(returns, annualization=BARS_PER_YEAR)
        )
        * 100.0,
        # RiskAdjustedSection
        "risk_adjusted.sharpe": float(
            ep.sharpe_ratio(returns, risk_free=rf_per_bar, annualization=BARS_PER_YEAR)
        ),
        # empyrical's sortino_ratio has a different denominator convention
        # (it computes the downside independently of `required_return`), so
        # reproduce fugazi's formula directly against empyrical's downside_risk.
        "risk_adjusted.sortino": (
            float(returns.mean()) * BARS_PER_YEAR - RISK_FREE_RATE
        )
        / ann_downside,
        "risk_adjusted.calmar": ((final_equity / INITIAL_CASH) ** (1.0 / years) - 1.0)
        / (-float(ep.max_drawdown(returns))),
        # Omega with per-bar arithmetic rf threshold (matches fugazi's rule
        # exactly; empyrical.omega_ratio would use a geometric conversion).
        "risk_adjusted.omega": _omega(returns, rf_per_bar),
        "risk_adjusted.ulcer_index": ulcer_index_frac(equity),
        # DrawdownSection
        "drawdown.max": -float(ep.max_drawdown(returns)),
        "drawdown.max_pct": -float(ep.max_drawdown(returns)) * 100.0,
    }

    with open(OUT_CSV, "w", newline="") as f:
        w = csv.writer(f)
        w.writerow(["metric", "expected"])
        for k, v in fields.items():
            w.writerow([k, repr(float(v))])

    print(f"wrote {OUT_CSV} ({len(fields)} metrics from {n} returns)")


def _omega(returns: np.ndarray, threshold: float) -> float:
    diff = returns - threshold
    gains = float(np.maximum(diff, 0.0).sum())
    losses = float(np.maximum(-diff, 0.0).sum())
    return gains / losses if losses > 0.0 else float("nan")


if __name__ == "__main__":
    main()
