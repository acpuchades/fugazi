"""Every `fugazi.metrics` function must agree with the run report it describes.

Each binding in `python/src/metrics.rs` is a one-line delegation —
`core_metrics::omega(&returns, threshold)` — and `test_parity.py` does not look
at metrics at all. Thirty-five of them are never called by name anywhere in the
suite, so pointing one at the wrong core function was free: `ta.metrics.omega`
wired to `sortino` and `ta.metrics.ulcer_index` to `max_drawdown` both left the
604-test suite green.

The reference is the **aggregated metrics document** that `StrategySpec.evaluate`
returns. That document is assembled by the Rust pipeline from the same run, and
is itself cross-validated against empyrical and backtesting.py by
`tests/metrics_validation.rs` and `tests/trade_metrics_validation.rs`, with
`tests/metrics_coverage.rs` guaranteeing every field has a reference or a
written exemption. So comparing the standalone function against the document
inherits all of that, and needs no expected values of its own — a mis-wired
delegation shows up as a disagreement with the run it is supposed to summarise.
"""

import pytest

import fugazi as ta

BARS_PER_YEAR = 252.0
RISK_FREE = 0.0
CASH = 10_000.0

_YAML = """
root: BTC
long:
  enter: !crosses_above { lhs: !sma { period: 2 }, rhs: !sma { period: 5 } }
  exit:  !crosses_below { lhs: !sma { period: 2 }, rhs: !sma { period: 5 } }
short:
  enter: !crosses_below { lhs: !sma { period: 2 }, rhs: !sma { period: 5 } }
  exit:  !crosses_above { lhs: !sma { period: 2 }, rhs: !sma { period: 5 } }
"""

# An always-in reversal over a path that trends up, rolls over and chops, so the
# run produces winners *and* losers, long *and* short trades, and more than one
# drawdown — otherwise half the trade metrics are degenerate and agree with each
# other for want of anything to disagree about.
_CLOSES = [
    100.0,
    102,
    101,
    105,
    103,
    108,
    107,
    112,
    110,
    115,
    113,
    109,
    111,
    107,
    104,
    108,
    106,
    110,
    112,
    109,
] * 3


def _snapshots():
    out, prev = [], _CLOSES[0]
    for close in _CLOSES:
        out.append(
            {
                "BTC": ta.Candle(
                    prev, max(prev, close) + 1, min(prev, close) - 1, close, 1_000.0
                )
            }
        )
        prev = close
    return out


@pytest.fixture(scope="module")
def run():
    """`(document, report, derived inputs)` from one backtest."""
    spec = ta.load_spec(_YAML)
    snaps = _snapshots()
    doc = spec.evaluate(ta.PaperWallet(CASH), snaps, bars_per_year=BARS_PER_YEAR)
    report = ta.load_spec(_YAML).run(ta.PaperWallet(CASH), snaps)
    eq, init = report.equity_curve, report.initial_equity
    return {
        "doc": doc,
        "equity": eq,
        "initial": init,
        "returns": ta.metrics.per_bar_returns(eq, init),
        "trades": ta.metrics.reconstruct_trades(report.fills),
        "segments": ta.metrics.drawdown_segments(eq),
        "fills": report.fills,
        "bars": len(eq),
    }


def _cases(r):
    """`document key -> (value the standalone function computes)`.

    Percentages are the document's own presentation; the functions return
    fractions, so the mapping scales where the key says `_pct`.
    """
    m, rets, trades, segs = ta.metrics, r["returns"], r["trades"], r["segments"]
    eq, init, bars = r["equity"], r["initial"], r["bars"]
    return {
        # --- per-bar return distribution ---
        "returns.mean_bar": m.mean_return(rets),
        "returns.median_bar": m.median_return(rets),
        "returns.stddev_bar": m.stddev_return(rets),
        "returns.best_bar": m.best_return(rets),
        "returns.worst_bar": m.worst_return(rets),
        "returns.positive_bars_pct": m.positive_bars_ratio(rets) * 100.0,
        "returns.skewness": m.skewness(rets),
        "returns.kurtosis": m.kurtosis(rets),
        "returns.var_95": m.value_at_risk(rets, 0.95),
        "returns.cvar_95": m.conditional_value_at_risk(rets, 0.95),
        "returns.tail_ratio": m.tail_ratio(rets),
        # --- compound returns ---
        "returns.total": m.total_return(eq, init),
        "returns.total_pct": m.total_return(eq, init) * 100.0,
        "returns.cagr_pct": m.cagr(eq, init, BARS_PER_YEAR) * 100.0,
        "returns.annualized_mean_pct": m.annualized_return(rets, BARS_PER_YEAR) * 100.0,
        "returns.annualized_volatility_pct": m.annualized_volatility(
            rets, BARS_PER_YEAR
        )
        * 100.0,
        # --- risk-adjusted ---
        "risk_adjusted.sharpe": m.sharpe(rets, RISK_FREE, BARS_PER_YEAR),
        "risk_adjusted.sortino": m.sortino(rets, RISK_FREE, BARS_PER_YEAR),
        "risk_adjusted.calmar": m.calmar(eq, init, BARS_PER_YEAR),
        "risk_adjusted.omega": m.omega(rets, 0.0),
        "risk_adjusted.ulcer_index": m.ulcer_index(eq),
        "risk_adjusted.ulcer_performance_index": m.ulcer_performance_index(
            eq, init, RISK_FREE, BARS_PER_YEAR
        ),
        "risk_adjusted.probabilistic_sharpe": m.probabilistic_sharpe(
            rets, RISK_FREE, BARS_PER_YEAR, 0.0
        ),
        # --- drawdown ---
        "drawdown.max": m.max_drawdown(segs),
        "drawdown.max_pct": m.max_drawdown(segs) * 100.0,
        "drawdown.avg": m.average_drawdown(segs),
        "drawdown.avg_pct": m.average_drawdown(segs) * 100.0,
        "drawdown.count": m.drawdown_count(segs),
        "drawdown.max_duration_bars": m.max_drawdown_duration(segs),
        "drawdown.avg_duration_bars": m.average_drawdown_duration(segs),
        "drawdown.time_in_drawdown_pct": m.time_in_drawdown_ratio(segs, bars) * 100.0,
        "drawdown.recovery_factor": m.recovery_factor(eq, init),
        # --- trades ---
        "trades.total": m.total_trades(trades),
        "trades.wins": m.winning_trades(trades),
        "trades.losses": m.losing_trades(trades),
        "trades.flat": m.flat_trades(trades),
        "trades.long_trades": m.long_trades(trades),
        "trades.short_trades": m.short_trades(trades),
        "trades.max_consecutive_wins": m.max_consecutive_wins(trades),
        "trades.max_consecutive_losses": m.max_consecutive_losses(trades),
        "trades.win_rate_pct": m.win_rate(trades) * 100.0,
        "trades.profit_factor": m.profit_factor(trades),
        "trades.payoff_ratio": m.payoff_ratio(trades),
        "trades.expectancy": m.expectancy(trades),
        "trades.kelly_fraction": m.kelly_fraction(trades),
        "trades.average_win": m.average_win(trades),
        "trades.average_loss": m.average_loss(trades),
        "trades.largest_win": m.largest_win(trades),
        "trades.largest_loss": m.largest_loss(trades),
        "trades.average_return_pct": m.average_trade_return(trades) * 100.0,
        "trades.average_bars": m.average_bars_held(trades),
        "trades.min_bars": m.min_bars_held(trades),
        "trades.max_bars": m.max_bars_held(trades),
        "trades.exposure_pct": m.exposure_ratio(r["fills"], bars) * 100.0,
    }


def _lookup(doc, key):
    section, field = key.split(".", 1)
    return doc[section][field]


def test_every_standalone_metric_matches_the_run_document(run):
    """The standalone function and the aggregated document must agree."""
    doc = run["doc"]
    wrong = []
    for key, got in _cases(run).items():
        want = _lookup(doc, key)
        if got is None or want is None:
            if got is not want:
                wrong.append(f"{key}: standalone={got!r} document={want!r}")
            continue
        if got != pytest.approx(want, rel=1e-9, abs=1e-9):
            wrong.append(f"{key}: standalone={got!r} document={want!r}")
    assert not wrong, (
        "these `fugazi.metrics` functions disagree with the run they summarise "
        "— check the delegation in python/src/metrics.rs:\n  " + "\n  ".join(wrong)
    )


def test_the_comparison_is_not_vacuous(run):
    """A degenerate run would make most metrics `None` or zero and agree trivially."""
    cases = _cases(run)
    numeric = [v for v in cases.values() if isinstance(v, (int, float))]
    assert len(numeric) >= 45, f"only {len(numeric)} metrics produced a number"
    assert sum(1 for v in numeric if v not in (0, 0.0)) >= 35, (
        "too many metrics read zero — the fixture is not exercising them"
    )
    # Winners and losers, longs and shorts, more than one drawdown.
    assert run["doc"]["trades"]["wins"] > 0 and run["doc"]["trades"]["losses"] > 0
    assert (
        run["doc"]["trades"]["long_trades"] > 0
        and run["doc"]["trades"]["short_trades"] > 0
    )
    assert run["doc"]["drawdown"]["count"] > 1


def test_every_registered_metric_is_compared_or_declared(run):
    """A new `reg!` entry must join the mapping above, or say why not.

    Without this the sweep narrows silently: a function added to
    `python/src/metrics.rs` is simply never compared, which is the state the
    whole file exists to end.
    """
    # Reachable but not summarised by a run document, each with its reason.
    NOT_IN_DOCUMENT = {
        "per_bar_returns": "an input to the others, used to build the comparison",
        "reconstruct_trades": "as per_bar_returns",
        "drawdown_segments": "as per_bar_returns",
        "deflated_sharpe": "needs a trial count the document does not model",
        "deflated_sharpe_from_stats": "as deflated_sharpe",
        "probabilistic_sharpe_from_stats": "the moment-wise form; the document "
        "carries only the series form",
        "expected_max_sharpe": "a benchmark input to deflated_sharpe, not a run "
        "measurement",
        "Trade": "a class, not a metric",
        "DrawdownSegment": "a class, not a metric",
    }
    registered = {
        n
        for n in dir(ta.metrics)
        if not n.startswith("_") and n not in ("annotations",)
    }
    compared = {k.split(".", 1)[1] for k in _cases(run)}
    # Map document field names back to function names where they differ.
    compared |= {
        "positive_bars_ratio",
        "total_return",
        "cagr",
        "annualized_return",
        "annualized_volatility",
        "value_at_risk",
        "conditional_value_at_risk",
        "mean_return",
        "median_return",
        "stddev_return",
        "best_return",
        "worst_return",
        "max_drawdown",
        "average_drawdown",
        "drawdown_count",
        "max_drawdown_duration",
        "average_drawdown_duration",
        "time_in_drawdown_ratio",
        "recovery_factor",
        "total_trades",
        "winning_trades",
        "losing_trades",
        "flat_trades",
        "win_rate",
        "average_trade_return",
        "average_bars_held",
        "min_bars_held",
        "max_bars_held",
        "exposure_ratio",
        "ulcer_index",
        "ulcer_performance_index",
        "probabilistic_sharpe",
        "omega",
        "sharpe",
        "sortino",
        "calmar",
        "skewness",
        "kurtosis",
        "tail_ratio",
        "profit_factor",
        "payoff_ratio",
        "expectancy",
        "kelly_fraction",
        "average_win",
        "average_loss",
        "largest_win",
        "largest_loss",
        "max_consecutive_wins",
        "max_consecutive_losses",
        "long_trades",
        "short_trades",
    }
    missing = sorted(registered - compared - set(NOT_IN_DOCUMENT))
    assert not missing, (
        "these `fugazi.metrics` functions are registered but never compared "
        f"against a run: {missing}"
    )
