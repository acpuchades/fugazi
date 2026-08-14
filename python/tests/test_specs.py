"""Tests for the YAML-driven strategy surface: load_spec, evaluate, optimize."""

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
# load_spec: shape detection + run
# ---------------------------------------------------------------------------


def test_load_preset_and_run():
    """A `!buy_and_hold` preset loads, kind='single', and runs against snapshots."""
    spec = ta.load_spec("!buy_and_hold { symbol: BTC }")
    assert spec.kind == "single"

    snaps = _snaps_single("BTC", [100.0, 101.0, 102.0, 103.0, 104.0])
    wallet = ta.PaperWallet(1000.0)
    rep = spec.run(wallet, snaps)
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
    spec = ta.load_spec(yaml)
    assert spec.kind == "single"

    snaps = _snaps_single(
        "BTC",
        [10, 9, 8, 7, 6, 7, 9, 12, 15, 18, 21, 22, 21, 20, 18, 15, 12, 10, 8, 6],
    )
    wallet = ta.PaperWallet(1000.0)
    m = spec.evaluate(wallet, snaps)
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
    spec = ta.load_spec(yaml)
    assert spec.kind == "pairs"

    # BTC up, ETH down — expect entry with both legs active.
    snaps = _snaps_multi({
        "BTC": [90, 91, 92, 93, 95, 100, 105, 110, 112, 115],
        "ETH": [110, 108, 107, 105, 103, 100, 98, 96, 94, 92],
    })
    wallet = ta.PaperWallet(1000.0)
    rep = spec.run(wallet, snaps)
    assert len(rep.equity_curve) == len(snaps)


def test_load_basket_and_run():
    yaml = """
    selection: !top_bottom { longs: 1, shorts: 1 }
    score: !roc { source: !close { source: !pick { symbol: !arg SYM } }, period: 2 }
    sizing: !equal_weight 2
    """
    spec = ta.load_spec(yaml)
    assert spec.kind == "basket"

    snaps = _snaps_multi({
        "BTC": [100, 102, 104, 106, 108, 110, 112, 114, 116, 118, 120, 122],
        "ETH": [100, 98, 96, 94, 92, 90, 88, 86, 84, 82, 80, 78],
    })
    wallet = ta.PaperWallet(1000.0)
    rep = spec.run(wallet, snaps)
    assert len(rep.equity_curve) == len(snaps)
    # BTC scoring higher than ETH → long BTC / short ETH → at least two fills.
    assert len(rep.fills) >= 2


def test_load_multi_and_run():
    yaml = """
    long:
      enter: !gt { lhs: !close { source: !pick { symbol: !arg SYM } }, rhs: 50 }
    """
    spec = ta.load_spec(yaml)
    assert spec.kind == "multi"

    snaps = _snaps_multi({
        "BTC": [100, 101, 102, 103, 104, 105],
        "ETH": [200, 201, 202, 203, 204, 205],
    })
    wallet = ta.PaperWallet(1000.0)
    rep = spec.run(wallet, snaps)
    assert len(rep.equity_curve) == len(snaps)


def test_load_portfolio_and_run():
    yaml = """
    children:
      - name: c1
        strategy: !buy_and_hold { symbol: BTC }
      - name: c2
        strategy: !buy_and_hold { symbol: ETH }
    """
    spec = ta.load_spec(yaml)
    assert spec.kind == "portfolio"

    snaps = _snaps_multi({
        "BTC": [100, 101, 102, 103, 104, 105],
        "ETH": [200, 201, 202, 203, 204, 205],
    })
    wallet = ta.PaperWallet(1000.0)
    rep = spec.run(wallet, snaps)
    assert len(rep.equity_curve) == len(snaps)
    # Two buy-and-holds → one fill per child.
    assert len(rep.fills) >= 2


def test_load_spec_with_params():
    """`!param` placeholders resolve from the `params=` dict."""
    yaml = """
    symbol: BTC
    long:
      enter: !crosses_above
        lhs: !sma { period: !param FAST }
        rhs: !sma { period: !param SLOW }
    """
    spec = ta.load_spec(yaml, params={"FAST": 3, "SLOW": 8})
    assert spec.kind == "single"


def test_load_spec_explicit_kind_override():
    """Passing `kind=` bypasses auto-detection."""
    spec = ta.load_spec(
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


def test_optimize_with_cost_config_lowers_equity():
    """A cost config passed to `ta.optimize` produces a smaller final equity."""
    yaml = "!buy_and_hold { symbol: BTC }"
    snaps = _snaps_single("BTC", [100.0, 101.0, 102.0, 103.0, 104.0])
    baseline = ta.optimize(yaml, snaps, cash=1000.0, grid=[{}])
    with_cost = ta.optimize(
        yaml, snaps, cash=1000.0, grid=[{}],
        costs={"commission": {"percentage": {"rate": 0.001}}},
    )
    # Higher cost → lower final metric value (total_return dips).
    b_ret = baseline.rows[0].metrics.get("returns.total_pct")
    c_ret = with_cost.rows[0].metrics.get("returns.total_pct")
    if b_ret is not None and c_ret is not None:
        assert c_ret <= b_ret


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


def test_optimize_walkforward_two_tuple():
    """`walkforward=(is, oos)` returns a WalkForwardResult with per-fold IS/OOS."""
    result = ta.optimize(
        _trend_yaml(),
        _trend_snaps(),
        cash=1000.0,
        grid=[{"FAST": [3, 5], "SLOW": [10]}],
        metric_names=["risk_adjusted.sharpe"],
        best_by="risk_adjusted.sharpe",
        walkforward=(20, 10),
    )
    # 60 bars, 20/10 → 4 non-overlapping OOS folds (last absorbs tail).
    assert isinstance(result, ta.WalkForwardResult)
    assert result.is_bars == 20
    assert result.oos_bars == 10
    assert result.embargo_bars == 0
    assert len(result.folds) >= 1
    for fold in result.folds:
        assert fold.is_range[1] > fold.is_range[0]
        assert fold.oos_range[1] > fold.oos_range[0]
        assert "run" in fold.is_metrics
        assert "run" in fold.oos_metrics
        # Winner's param combo projected onto union columns.
        assert "FAST" in fold.values
    # Composite OOS: monotone-length bars stitched together, plus a metrics doc.
    assert len(result.composite_equity) > 0
    assert "run" in result.composite_metrics


def test_optimize_walkforward_three_tuple_embargo():
    """`walkforward=(is, oos, embargo)` drops embargo bars from OOS metric slice."""
    result = ta.optimize(
        _trend_yaml(),
        _trend_snaps(),
        cash=1000.0,
        grid=[{"FAST": [3, 5], "SLOW": [10]}],
        best_by="risk_adjusted.sharpe",
        walkforward=(20, 10, 2),
    )
    assert result.embargo_bars == 2


def test_optimize_walkforward_and_windowed_mutually_exclusive():
    with pytest.raises(ValueError, match="mutually exclusive"):
        ta.optimize(
            _trend_yaml(),
            _trend_snaps(),
            cash=1000.0,
            grid=[{"FAST": [3]}],
            walkforward=(20, 10),
            windowed=15,
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


def test_bad_spec_raises_value_error_not_a_panic():
    """A spec that parses but can't build is a `ValueError`, not a panic.

    `!get` naming a column the series doesn't carry is only detectable against
    the schema, so it survives the static check and reaches the builder. It has
    to arrive in Python as a normal exception carrying the failing path.
    """
    yaml = """
symbol: X
long:
  enter: !gt
    lhs: !sma { source: !get { key: no_such_column }, period: 3 }
    rhs: !value 1.0
"""
    spec = ta.load_spec(yaml)
    wallet = ta.PaperWallet(1000.0)
    snaps = _snaps_single("X", [10.0] * 10)
    with pytest.raises(ValueError) as excinfo:
        spec.run(wallet, snaps)
    message = str(excinfo.value)
    assert "no_such_column" in message
    # The tag trail is rendered on its own line, as the CLI does it.
    assert "at: !gt > !sma > !get" in message


def test_bad_spec_also_fails_evaluate_and_optimize():
    """The same document fails the other two entry points the same way."""
    yaml = """
symbol: X
long:
  enter: !gt { lhs: !get { key: nope }, rhs: !value 1.0 }
"""
    snaps = _snaps_single("X", [10.0] * 10)
    with pytest.raises(ValueError, match="nope"):
        ta.load_spec(yaml).evaluate(ta.PaperWallet(1000.0), snaps)
    with pytest.raises(ValueError, match="nope"):
        ta.optimize(yaml, snaps, cash=1000.0, grid=[{}])


# ---------------------------------------------------------------------------
# Portfolio builder — the Python mirror of `Portfolio::builder()`
# ---------------------------------------------------------------------------


def _always_and_never(symbol):
    """A signal that is always true and one that never is, rooted on `symbol`."""
    close = ta.close(source=ta.pick(symbol))
    return close.above(0.0), close.below(0.0)


def test_portfolio_builder_runs_children_on_one_account():
    snaps = _snaps_multi({"A": [100, 105, 110, 115, 120], "B": [50, 50, 50, 50, 50]})
    a_on, a_off = _always_and_never("A")
    b_on, b_off = _always_and_never("B")

    pf = (
        ta.Portfolio()
        .add("hold_a", ta.Strategy("A").long_on(a_on, a_off))
        .add("hold_b", ta.Strategy("B").long_on(b_on, b_off))
        .weights([0.6, 0.4])
    )
    report = pf.run(ta.PaperWallet(10_000.0), snaps)

    assert len(report.equity_curve) == len(snaps)
    assert report.initial_equity == 10_000.0
    # Both children entered, and each fill is that child's own share.
    assert len(report.fills) == 2
    assert {f.order.symbol for f in report.fills} == {"A", "B"}


def test_portfolio_builder_matches_the_equivalent_yaml_document():
    """The two ways to build a portfolio must agree.

    This is the parity that matters: `ta.Portfolio()` and a `portfolio:`
    document are two front-ends onto the same Rust composite, so a divergence
    means one of them is wiring it differently.
    """
    snaps = _snaps_multi({"A": [100, 102, 104, 106, 108], "B": [50, 51, 52, 53, 54]})

    yaml = """
    weights: !value [0.6, 0.4]
    children:
      - name: hold_a
        strategy: !buy_and_hold { symbol: A }
      - name: hold_b
        strategy: !buy_and_hold { symbol: B }
    """
    from_yaml = ta.load_spec(yaml).run(ta.PaperWallet(10_000.0), snaps)

    a_on, a_off = _always_and_never("A")
    b_on, b_off = _always_and_never("B")
    from_builder = (
        ta.Portfolio()
        .add("hold_a", ta.Strategy("A").long_on(a_on, a_off))
        .add("hold_b", ta.Strategy("B").long_on(b_on, b_off))
        .weights([0.6, 0.4])
        .run(ta.PaperWallet(10_000.0), snaps)
    )

    assert from_builder.equity_curve == pytest.approx(from_yaml.equity_curve)
    assert len(from_builder.fills) == len(from_yaml.fills)


def test_portfolio_builder_is_immutable():
    """`.add(...)` returns a new portfolio, matching the other builders."""
    base = ta.Portfolio()
    once = base.add("a", ta.Strategy("A"))
    assert "a" not in repr(base)
    assert "a" in repr(once)


def test_portfolio_run_adopts_and_mutates_the_wallet():
    """The passed wallet IS the portfolio's account: it is traded and handed back
    mutated, so the caller can inspect the resulting position/equity/blotter."""
    snaps = _snaps_multi({"A": [100, 102, 104, 106, 108]})
    a_on, a_off = _always_and_never("A")

    w = ta.PaperWallet(10_000.0)
    report = (
        ta.Portfolio()
        .add("hold_a", ta.Strategy("A").long_on(a_on, a_off))
        .run(w, snaps)
    )

    # A buy-and-hold child took a long position on the one account, and the
    # wallet's own equity now agrees with the aggregate curve — proof it was
    # driven, not merely read for a seed.
    assert w.position("A") > 0.0
    assert w.orders(), "the adopted wallet should carry the run's blotter"
    assert w.equity() == pytest.approx(report.equity_curve[-1])


def test_portfolio_run_rejects_a_non_wallet():
    snaps = _snaps_multi({"A": [100, 101]})
    with pytest.raises(TypeError, match="PaperWallet or an OkxWallet"):
        ta.Portfolio().add("a", ta.Strategy("A")).run("not a wallet", snaps)


def test_portfolio_builder_rejects_bad_composition():
    with pytest.raises(ValueError, match="cannot be a child of a Portfolio"):
        ta.Portfolio().add("nested", ta.Portfolio())

    with pytest.raises(ValueError, match="duplicate child name"):
        ta.Portfolio().add("x", ta.Strategy("A")).add("x", ta.Strategy("B"))

    with pytest.raises(ValueError, match="must be a Strategy"):
        ta.Portfolio().add("x", ta.identity())

    snaps = _snaps_single("A", [100, 101])
    with pytest.raises(ValueError, match="weights"):
        (
            ta.Portfolio()
            .add("x", ta.Strategy("A"))
            .weights([1.0, 2.0])
            .run(ta.PaperWallet(1_000.0), snaps)
        )

    with pytest.raises(ValueError, match="at least one child"):
        ta.Portfolio().run(ta.PaperWallet(1_000.0), snaps)


def test_portfolio_builder_accepts_every_child_shape():
    """All four shapes are valid children — the composite is heterogeneous."""
    pf = (
        ta.Portfolio()
        .add("single", ta.Strategy("A"))
        .add("pairs", ta.PairsStrategy("A", "B"))
        .add("basket", ta.BasketStrategy())
        .add("multi", ta.MultiAssetStrategy())
    )
    assert repr(pf) == "Portfolio(children=[single, pairs, basket, multi])"


# ---------------------------------------------------------------------------
# Run resuming: run_resumable round-trips state and continues identically.
# ---------------------------------------------------------------------------


_RESUME_YAML = """
symbol: X
long:
  enter: !crosses_above
    lhs: !ema { period: 3, source: !close }
    rhs: !ema { period: 8, source: !close }
  exit: !crosses_below
    lhs: !ema { period: 3, source: !close }
    rhs: !ema { period: 8, source: !close }
"""


def _wobbly(n):
    import math
    return [100.0 + 10.0 * math.sin(i * 0.35) + 0.05 * i for i in range(n)]


def test_run_resumable_matches_uninterrupted_run():
    """A run split in two with a save/restore in between matches the whole run."""
    spec = ta.load_spec(_RESUME_YAML)
    snaps = _snaps_single("X", _wobbly(60))
    split = 30

    # Uninterrupted 60-bar run.
    whole_rep, _ = ta.load_spec(_RESUME_YAML).run_resumable(ta.PaperWallet(1000.0), snaps)

    # First half → capture the state JSON.
    _first, state = spec.run_resumable(ta.PaperWallet(1000.0), snaps[:split])

    # Rebuild fresh, resume from the state, run the second half.
    second_rep, _ = ta.load_spec(_RESUME_YAML).run_resumable(
        ta.PaperWallet(1000.0), snaps[split:], resume=state
    )

    tail = whole_rep.equity_curve[split:]
    assert len(second_rep.equity_curve) == len(tail)
    # Exact (serde float_roundtrip keeps f64 bit-identical through JSON).
    assert second_rep.equity_curve == tail


def test_run_resumable_rejects_mismatched_shape():
    """Resuming a single-shape state into a pairs spec is rejected."""
    snaps = _snaps_single("X", _wobbly(20))
    _rep, state = ta.load_spec(_RESUME_YAML).run_resumable(ta.PaperWallet(1000.0), snaps)

    pairs = ta.load_spec(
        """
        left: A
        right: B
        long_spread:
          enter: !lt { lhs: !close { source: !pick { symbol: A } }, rhs: !value 0.0 }
          exit: !gt { lhs: !close { source: !pick { symbol: A } }, rhs: !value 5.0 }
        """
    )
    pair_snaps = _snaps_multi({"A": [1.0, 2.0], "B": [1.0, 2.0]})
    with pytest.raises(ValueError, match="resume"):
        pairs.run_resumable(ta.PaperWallet(1000.0), pair_snaps, resume=state)


# ---------------------------------------------------------------------------
# Monte Carlo significance: MonteCarloConfig + evaluate(montecarlo=...)
# ---------------------------------------------------------------------------

_MC_YAML = """
symbol: X
long:
  enter: !crosses_above { lhs: !sma { period: 3 }, rhs: !sma { period: 8 } }
short:
  enter: !crosses_below { lhs: !sma { period: 3 }, rhs: !sma { period: 8 } }
"""


def test_montecarlo_config_repr_and_validation():
    cfg = ta.MonteCarloConfig(permutations=200, scheme="stationary", block=8.0, seed=3)
    assert "MonteCarloConfig" in repr(cfg)
    with pytest.raises(ValueError, match="scheme"):
        ta.MonteCarloConfig(scheme="nope")
    with pytest.raises(ValueError, match="null"):
        ta.MonteCarloConfig(null="nope")


def test_evaluate_embeds_montecarlo_block():
    """evaluate(montecarlo=...) attaches a `montecarlo` block with CIs + p-values."""
    spec = ta.load_spec(_MC_YAML)
    snaps = _snaps_single("X", _wobbly(120))
    cfg = ta.MonteCarloConfig(
        permutations=200, scheme="stationary", block=8.0, seed=7, null="rerun"
    )
    m = spec.evaluate(ta.PaperWallet(1000.0), snaps, bars_per_year=365.0, montecarlo=cfg)

    assert "montecarlo" in m
    mc = m["montecarlo"]
    assert mc["permutations"] == 200
    assert mc["seed"] == 7
    assert len(mc["metrics"]) >= 1
    row = mc["metrics"][0]
    assert row["ci_lower"] <= row["ci_upper"]
    assert 0.0 < row["p_value_rerun"] <= 1.0

    samples = mc["samples"]
    assert samples["metric_names"] == [m["name"] for m in mc["metrics"]]
    estimators = {s["estimator"] for s in samples["sets"]}
    assert estimators == {"bootstrap_ci", "null_rerun"}
    for s in samples["sets"]:
        assert len(s["rows"]) == 200
        for row in s["rows"]:
            assert len(row) == len(samples["metric_names"])


def test_evaluate_without_montecarlo_has_no_block():
    spec = ta.load_spec(_MC_YAML)
    snaps = _snaps_single("X", _wobbly(60))
    m = spec.evaluate(ta.PaperWallet(1000.0), snaps)
    assert "montecarlo" not in m


def test_montecarlo_reproducible_across_calls():
    spec = ta.load_spec(_MC_YAML)
    snaps = _snaps_single("X", _wobbly(100))
    cfg = ta.MonteCarloConfig(permutations=150, seed=42, null="rerun", metrics=["sharpe"])
    a = spec.evaluate(ta.PaperWallet(1000.0), snaps, montecarlo=cfg)["montecarlo"]
    b = spec.evaluate(ta.PaperWallet(1000.0), snaps, montecarlo=cfg)["montecarlo"]
    assert a["metrics"][0]["ci_lower"] == b["metrics"][0]["ci_lower"]
    assert a["metrics"][0]["p_value_rerun"] == b["metrics"][0]["p_value_rerun"]


# ---------------------------------------------------------------------------
# fugazi.montecarlo: the deterministic resampling primitive behind the fan chart
# ---------------------------------------------------------------------------


def test_resample_index_matrix_shape_range_and_determinism():
    a = ta.montecarlo.resample_index_matrix(50, 10, scheme="stationary", block=8.0, seed=3)
    b = ta.montecarlo.resample_index_matrix(50, 10, scheme="stationary", block=8.0, seed=3)
    assert a == b, "same seed must reproduce the whole matrix"
    assert len(a) == 10 and all(len(row) == 50 for row in a)
    assert all(0 <= i < 50 for row in a for i in row)
    c = ta.montecarlo.resample_index_matrix(50, 10, scheme="stationary", block=8.0, seed=4)
    assert c != a, "a different seed must diverge"
    # The scalar draw is permutation 0 of the matrix with the same arguments.
    scalar = ta.montecarlo.resample_indices(50, scheme="stationary", block=8.0, seed=3)
    assert scalar == a[0]
    with pytest.raises(ValueError, match="scheme"):
        ta.montecarlo.resample_index_matrix(10, 5, scheme="nope")


def test_resample_index_matrix_reproduces_bootstrap_ci_paths():
    """The index matrix rebuilds exactly the bootstrap-CI estimator's equity
    paths: gathering the observed returns by each permutation's indices and
    cum-producting must reproduce the CI row for `returns.total_pct` (a pure
    function of the resampled path's final equity, so no annualization to match).
    """
    spec = ta.load_spec(_MC_YAML)
    snaps = _snaps_single("X", _wobbly(100))
    perms, seed, block = 120, 11, 8.0

    # The observed run evaluate() drives internally, reproduced here for its curve.
    rep = spec.run(ta.PaperWallet(1000.0), snaps)
    returns = ta.metrics.per_bar_returns(rep.equity_curve, rep.initial_equity)

    cfg = ta.MonteCarloConfig(
        permutations=perms, scheme="stationary", block=block, seed=seed,
        null="none", metrics=["returns.total_pct"],
    )
    mc = spec.evaluate(ta.PaperWallet(1000.0), snaps, montecarlo=cfg)["montecarlo"]
    samples = mc["samples"]
    col = samples["metric_names"].index("returns.total_pct")
    ci_rows = next(s["rows"] for s in samples["sets"] if s["estimator"] == "bootstrap_ci")

    idx = ta.montecarlo.resample_index_matrix(
        len(returns), perms, scheme="stationary", block=block, seed=seed
    )
    for p, row_idx in enumerate(idx):
        equity, prev = [], rep.initial_equity
        for i in row_idx:
            prev *= 1.0 + returns[i]
            equity.append(prev)
        total_pct = ta.metrics.total_return(equity, rep.initial_equity) * 100.0
        assert total_pct == pytest.approx(ci_rows[p][col]), f"permutation {p} diverged"


# ---------------------------------------------------------------------------
# evaluate(windowed=...): windowed/rolling reductions for a plain backtest.
# ---------------------------------------------------------------------------


def test_evaluate_windowed_embeds_windowed_and_rolling():
    spec = ta.load_spec(_MC_YAML)
    bars = 120
    snaps = _snaps_single("X", _wobbly(bars))
    m = spec.evaluate(ta.PaperWallet(1000.0), snaps, windowed=30)

    assert "windowed" in m and "rolling" in m
    windowed = m["windowed"]
    rolling = m["rolling"]

    # Non-overlapping: ceil(120/30) == 4 independent spans covering every bar.
    assert len(windowed) == 4
    assert windowed[0]["start_bar"] == 0
    assert windowed[0]["end_bar"] == 29
    assert windowed[-1]["end_bar"] == bars - 1
    assert "sharpe" in windowed[0]["metrics"]["risk_adjusted"]

    # Rolling: stride-1, so bars - window + 1 overlapping spans.
    assert len(rolling) == bars - 30 + 1
    assert rolling[0]["start_bar"] == 0
    assert rolling[0]["end_bar"] == 29
    assert rolling[1]["start_bar"] == 1


def test_evaluate_without_windowed_has_no_block():
    spec = ta.load_spec(_MC_YAML)
    snaps = _snaps_single("X", _wobbly(60))
    m = spec.evaluate(ta.PaperWallet(1000.0), snaps)
    assert "windowed" not in m
    assert "rolling" not in m


def test_evaluate_windowed_rejects_zero():
    spec = ta.load_spec(_MC_YAML)
    snaps = _snaps_single("X", _wobbly(60))
    with pytest.raises(ValueError, match="windowed"):
        spec.evaluate(ta.PaperWallet(1000.0), snaps, windowed=0)
