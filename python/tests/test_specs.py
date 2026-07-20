"""Tests for the YAML-driven strategy surface: load_strategy, evaluate, optimize."""

import pytest

import fugazi as ta


def _snaps_single(symbol, closes, volume=1000.0):
    """Build one-symbol snapshots (flat OHLC bars)."""
    return [
        ta.Snapshot({symbol: ta.Candle(v, v, v, v, volume)})
        for v in closes
    ]


def _snaps_multi(series, volume=1000.0):
    """dict[sym -> list[close]] → list of snapshots."""
    n = len(next(iter(series.values())))
    out = []
    for i in range(n):
        d = {
            sym: ta.Candle(prices[i], prices[i], prices[i], prices[i], volume)
            for sym, prices in series.items()
        }
        out.append(ta.Snapshot(d))
    return out


# ---------------------------------------------------------------------------
# load_strategy: shape detection + run
# ---------------------------------------------------------------------------


def test_load_preset_and_run():
    """A `!buy_and_hold` preset loads, kind='single', and runs against snapshots."""
    spec = ta.load_strategy("!buy_and_hold { symbol: BTC }")
    assert spec.kind == "single"

    snaps = _snaps_single("BTC", [100.0, 101.0, 102.0, 103.0, 104.0])
    rep = spec.run(snaps, cash=1000.0)
    assert len(rep.equity_curve) == len(snaps)
    assert rep.initial_equity == pytest.approx(1000.0)
    # Buy-and-hold on a rising path — final equity should exceed initial.
    assert rep.equity_curve[-1] > rep.initial_equity


def test_load_single_spec_map_and_evaluate():
    """A spec-map single (symbol + long enter) loads, runs, and produces metrics."""
    yaml = """
    symbol: BTC
    long:
      enter: !crosses_above
        lhs: !sma { period: 3 }
        rhs: !sma { period: 6 }
    """
    spec = ta.load_strategy(yaml)
    assert spec.kind == "single"

    snaps = _snaps_single(
        "BTC",
        [10, 9, 8, 7, 6, 7, 9, 12, 15, 18, 21, 22, 21, 20, 18, 15, 12, 10, 8, 6],
    )
    m = spec.evaluate(snaps, cash=1000.0)
    # Metrics doc: nested dict, section keys are `run`, `returns`, ...
    assert "run" in m
    assert "returns" in m
    assert "risk_adjusted" in m
    assert m["run"]["bars"] == len(snaps)
    assert m["run"]["initial_equity"] == pytest.approx(1000.0)


def test_load_pairs_and_run():
    yaml = """
    left: BTC
    right: ETH
    enter: !crosses_above
      lhs: !close { source: !pick { symbol: BTC } }
      rhs: !close { source: !pick { symbol: ETH } }
    """
    spec = ta.load_strategy(yaml)
    assert spec.kind == "pairs"

    # BTC up, ETH down — expect entry with both legs active.
    snaps = _snaps_multi({
        "BTC": [90, 91, 92, 93, 95, 100, 105, 110, 112, 115],
        "ETH": [110, 108, 107, 105, 103, 100, 98, 96, 94, 92],
    })
    rep = spec.run(snaps, cash=1000.0)
    assert len(rep.equity_curve) == len(snaps)


def test_load_basket_and_run():
    yaml = """
    selection: !top_bottom { longs: 1, shorts: 1 }
    score: !roc { source: !close { source: !pick { symbol: !arg SYM } }, periods: 2 }
    sizing: !equal_weight 2
    """
    spec = ta.load_strategy(yaml)
    assert spec.kind == "basket"

    snaps = _snaps_multi({
        "BTC": [100, 102, 104, 106, 108, 110, 112, 114, 116, 118, 120, 122],
        "ETH": [100, 98, 96, 94, 92, 90, 88, 86, 84, 82, 80, 78],
    })
    rep = spec.run(snaps, cash=1000.0)
    assert len(rep.equity_curve) == len(snaps)
    # BTC scoring higher than ETH → long BTC / short ETH → at least two fills.
    assert len(rep.fills) >= 2


def test_load_multi_and_run():
    yaml = """
    long:
      enter: !gt { lhs: !close { source: !pick { symbol: !arg SYM } }, rhs: 50 }
    """
    spec = ta.load_strategy(yaml)
    assert spec.kind == "multi"

    snaps = _snaps_multi({
        "BTC": [100, 101, 102, 103, 104, 105],
        "ETH": [200, 201, 202, 203, 204, 205],
    })
    rep = spec.run(snaps, cash=1000.0)
    assert len(rep.equity_curve) == len(snaps)


def test_load_portfolio_and_run():
    yaml = """
    children:
      - name: c1
        strategy: !buy_and_hold { symbol: BTC }
      - name: c2
        strategy: !buy_and_hold { symbol: ETH }
    """
    spec = ta.load_strategy(yaml)
    assert spec.kind == "portfolio"

    snaps = _snaps_multi({
        "BTC": [100, 101, 102, 103, 104, 105],
        "ETH": [200, 201, 202, 203, 204, 205],
    })
    rep = spec.run(snaps, cash=1000.0)
    assert len(rep.equity_curve) == len(snaps)
    # Two buy-and-holds → one fill per child.
    assert len(rep.fills) >= 2


def test_load_strategy_with_params():
    """`!param` placeholders resolve from the `params=` dict."""
    yaml = """
    symbol: BTC
    long:
      enter: !crosses_above
        lhs: !sma { period: !param FAST }
        rhs: !sma { period: !param SLOW }
    """
    spec = ta.load_strategy(yaml, params={"FAST": 3, "SLOW": 8})
    assert spec.kind == "single"


def test_load_strategy_explicit_kind_override():
    """Passing `kind=` bypasses auto-detection."""
    spec = ta.load_strategy(
        "symbol: BTC\nlong:\n  enter: !value true\n",
        kind="single",
    )
    assert spec.kind == "single"


# ---------------------------------------------------------------------------
# TradingCostsConfig
# ---------------------------------------------------------------------------


def test_trading_costs_from_dict():
    """The wrapper accepts a flat leg mapping (auto-hoisted to default)."""
    c = ta.TradingCostsConfig({
        "commission": {"percentage": {"rate": 0.001}},
        "spread": {"bps": {"bps": 5}},
    })
    assert "TradingCostsConfig" in repr(c)


def test_trading_costs_empty_is_zero_cost():
    """Missing / empty mapping is fine — resolves to a zero-cost config."""
    ta.TradingCostsConfig()
    ta.TradingCostsConfig({})


def test_trading_costs_scoped_shape():
    """The `default:` / `by_symbol:` structured shape also works."""
    c = ta.TradingCostsConfig({
        "commission": {
            "default": {"percentage": {"rate": 0.001}},
            "by_symbol": {"BTC": {"percentage": {"rate": 0.0005}}},
        },
    })
    assert "scoped=1" in repr(c) or "defaults=true" in repr(c)


def test_run_with_costs_lowers_equity():
    """A run with a nonzero cost model produces a smaller final equity."""
    spec = ta.load_strategy("!buy_and_hold { symbol: BTC }")
    snaps = _snaps_single("BTC", [100.0, 101.0, 102.0, 103.0, 104.0])
    r0 = spec.run(snaps, cash=1000.0)
    r1 = spec.run(snaps, cash=1000.0, costs={"commission": {"percentage": {"rate": 0.001}}})
    assert r1.equity_curve[-1] < r0.equity_curve[-1]

    # A pre-built PyCostConfig works too.
    cc = ta.TradingCostsConfig({"commission": {"percentage": {"rate": 0.001}}})
    r2 = spec.run(snaps, cash=1000.0, costs=cc)
    assert r2.equity_curve[-1] == pytest.approx(r1.equity_curve[-1])


# ---------------------------------------------------------------------------
# optimize
# ---------------------------------------------------------------------------


def _trend_yaml():
    return """
    symbol: BTC
    long:
      enter: !crosses_above
        lhs: !sma { period: !param FAST }
        rhs: !sma { period: !param SLOW }
    """


def _trend_snaps():
    """A 60-bar path with a mild bump between bars 30..50 — enough for SMA crossovers."""
    prices = []
    for i in range(60):
        px = 100.0 + i * 0.3 + (10 if 30 <= i < 50 else 0)
        prices.append(px)
    return _snaps_single("BTC", prices)


def test_optimize_two_axis_grid():
    """A 2-axis grid returns rows = product of axes, with a defined best row."""
    sweep = ta.optimize(
        _trend_yaml(),
        _trend_snaps(),
        cash=1000.0,
        grid=[{"FAST": [3, 5], "SLOW": [10, 15]}],
        metric_names=["risk_adjusted.sharpe", "returns.total_pct"],
        best_by="risk_adjusted.sharpe",
    )
    assert len(sweep.rows) == 4
    assert set(sweep.columns) == {"FAST", "SLOW"}
    assert sweep.best is not None
    # The best row's metrics dict contains the requested keys.
    assert "risk_adjusted.sharpe" in sweep.best.metrics
    assert "returns.total_pct" in sweep.best.metrics
    # metric_columns is (user, resolved) pairs.
    assert all(len(pair) == 2 for pair in sweep.metric_columns)


def test_optimize_stacked_subgrids_union_columns():
    """Two subgrids with disjoint axis names produce a sparse union."""
    sweep = ta.optimize(
        _trend_yaml(),
        _trend_snaps(),
        cash=1000.0,
        params={"FAST": 3, "SLOW": 10},
        grid=[{"FAST": [3, 5]}, {"SLOW": [10, 20]}],
        metric_names=["risk_adjusted.sharpe"],
    )
    assert len(sweep.rows) == 4
    # Union is FAST + SLOW.
    assert set(sweep.columns) == {"FAST", "SLOW"}


def test_optimize_windowed_produces_per_window_metrics():
    """`windowed=N` populates `row.metrics_windowed`."""
    sweep = ta.optimize(
        _trend_yaml(),
        _trend_snaps(),
        cash=1000.0,
        grid=[{"FAST": [3, 5], "SLOW": [12]}],
        metric_names=["risk_adjusted.sharpe"],
        windowed=20,
        best_by="risk_adjusted.sharpe",
    )
    assert len(sweep.rows) == 2
    for row in sweep.rows:
        assert row.metrics_windowed is not None
        assert len(row.metrics_windowed) >= 1
        # Each entry is a Metrics dict.
        assert "run" in row.metrics_windowed[0]


def test_optimize_walkforward_raises():
    """Walkforward is not yet wired — raises NotImplementedError."""
    with pytest.raises(NotImplementedError):
        ta.optimize(
            _trend_yaml(),
            _trend_snaps(),
            cash=1000.0,
            grid=[{"FAST": [3, 5]}],
            walkforward=(20, 10),
        )


def test_optimize_range_axis_string():
    """`"start..end[:step]"` string expands to an integer range axis."""
    sweep = ta.optimize(
        _trend_yaml(),
        _trend_snaps(),
        cash=1000.0,
        params={"SLOW": 12},
        grid=[{"FAST": "3..7:2"}],
        metric_names=["risk_adjusted.sharpe"],
    )
    # 3, 5, 7 → 3 rows.
    assert len(sweep.rows) == 3
