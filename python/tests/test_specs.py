"""Tests for the YAML-driven strategy surface: load_spec, evaluate, optimize."""

import math
import os

import pytest

import fugazi as ta


def _snaps_single(symbol, closes, volume=1000.0):
    """Build one-symbol snapshots (flat OHLC bars)."""
    return [ta.Snapshot({symbol: ta.Candle(v, v, v, v, volume)}) for v in closes]


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
    spec = ta.load_spec("!buy_and_hold { root: BTC }")
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
    root: BTC
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
    snaps = _snaps_multi(
        {
            "BTC": [90, 91, 92, 93, 95, 100, 105, 110, 112, 115],
            "ETH": [110, 108, 107, 105, 103, 100, 98, 96, 94, 92],
        }
    )
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

    snaps = _snaps_multi(
        {
            "BTC": [100, 102, 104, 106, 108, 110, 112, 114, 116, 118, 120, 122],
            "ETH": [100, 98, 96, 94, 92, 90, 88, 86, 84, 82, 80, 78],
        }
    )
    wallet = ta.PaperWallet(1000.0)
    rep = spec.run(wallet, snaps)
    assert len(rep.equity_curve) == len(snaps)
    # BTC scoring higher than ETH → long BTC / short ETH → at least two fills.
    assert len(rep.fills) >= 2


def test_a_typo_inside_a_per_symbol_template_raises_at_load():
    """A basket's `score:` is deferred per symbol, but its *shape* isn't.

    The template body is typed-parsed at load with `!arg SYM` held as a hole,
    so a misspelled tag raises here — not on the first bar of a `run()` that
    happens to quote a symbol. Same rule for a multi-asset side's `enter:`.
    """
    basket = """
    selection: !top_bottom { longs: 1, shorts: 1 }
    score: !smaa { source: !close { source: !pick { symbol: !arg SYM } }, period: 2 }
    sizing: !value 1.0
    """
    with pytest.raises(Exception, match="smaa"):
        ta.load_spec(basket)

    multi = """
    long:
      enter: !gt { lhs: !close { source: !pick { symbol: !arg SYM } }, rsh: 50 }
    """
    with pytest.raises(Exception, match="rsh"):
        ta.load_spec(multi)


def test_load_multi_and_run():
    yaml = """
    long:
      enter: !gt { lhs: !close { source: !pick { symbol: !arg SYM } }, rhs: 50 }
    """
    spec = ta.load_spec(yaml)
    assert spec.kind == "multi"

    snaps = _snaps_multi(
        {
            "BTC": [100, 101, 102, 103, 104, 105],
            "ETH": [200, 201, 202, 203, 204, 205],
        }
    )
    wallet = ta.PaperWallet(1000.0)
    rep = spec.run(wallet, snaps)
    assert len(rep.equity_curve) == len(snaps)


def test_load_portfolio_and_run():
    yaml = """
    children:
      - name: c1
        strategy: !buy_and_hold { root: BTC }
      - name: c2
        strategy: !buy_and_hold { root: ETH }
    """
    spec = ta.load_spec(yaml)
    assert spec.kind == "portfolio"

    snaps = _snaps_multi(
        {
            "BTC": [100, 101, 102, 103, 104, 105],
            "ETH": [200, 201, 202, 203, 204, 205],
        }
    )
    wallet = ta.PaperWallet(1000.0)
    rep = spec.run(wallet, snaps)
    assert len(rep.equity_curve) == len(snaps)
    # Two buy-and-holds → one fill per child.
    assert len(rep.fills) >= 2


def test_portfolio_weights_without_a_rebalance_gate_are_refused():
    """A `weights:` expression is only read on a rebalance-fire, so a portfolio
    that never fires its gate computes one every bar and applies none of them.
    The document parses (every field is well-typed) — the build is the only
    place it can be caught, and Python reaches it through the same `try_build`
    the CLI does."""
    yaml = """
    weights: !drawdown_throttle { source: !portfolio_book, max_drawdown: 0.15 }
    children:
      - strategy: !buy_and_hold { root: BTC }
      - strategy: !buy_and_hold { root: ETH }
    """
    spec = ta.load_spec(yaml)
    snaps = _snaps_multi({"BTC": [100, 101, 102], "ETH": [200, 201, 202]})
    with pytest.raises(ta.SpecError, match="rebalance_on:"):
        spec.run(ta.PaperWallet(1000.0), snaps)


@pytest.mark.parametrize(
    "gate",
    ["!every 2", "!never"],
    ids=["cadence", "explicit-never"],
)
def test_portfolio_weights_build_once_the_document_says_when(gate):
    """Any stated gate satisfies the check — what is refused is the *omitted*
    field, not an infrequent cadence. `!never` is the named opt-out: written
    down it says the drift is intended."""
    yaml = f"""
    weights: !drawdown_throttle {{ source: !portfolio_book, max_drawdown: 0.15 }}
    rebalance_on: {gate}
    children:
      - strategy: !buy_and_hold {{ root: BTC }}
      - strategy: !buy_and_hold {{ root: ETH }}
    """
    spec = ta.load_spec(yaml)
    snaps = _snaps_multi({"BTC": [100, 101, 102], "ETH": [200, 201, 202]})
    rep = spec.run(ta.PaperWallet(1000.0), snaps)
    assert len(rep.equity_curve) == len(snaps)


def test_load_spec_with_params():
    """`!param` placeholders resolve from the `params=` dict."""
    yaml = """
    root: BTC
    long:
      enter: !crosses_above
        lhs: !sma { period: !param FAST }
        rhs: !sma { period: !param SLOW }
    """
    spec = ta.load_spec(yaml, params={"FAST": 3, "SLOW": 8})
    assert spec.kind == "single"


def test_an_omitted_root_defaults_to_the_symbol_param():
    """`root:` is optional; omitted, it reads `!param SYMBOL` / `!param FREQ`.

    `kind="single"` is not optional here, and that is the point: `root:` is what
    tells a `single:` document from a `multi:` one, so a document without it is
    genuinely ambiguous and `auto` reads it as `multi`.
    """
    yaml = "long:\n  enter: !value true\n  exit: !value false\n"
    spec = ta.load_spec(yaml, params={"SYMBOL": "BTC"}, kind="single")
    assert spec.kind == "single"

    snaps = _snaps_single("BTC", [100.0, 101.0, 102.0, 103.0, 104.0])
    wallet = ta.PaperWallet(1000.0)
    rep = spec.run(wallet, snaps)
    assert rep.fills, "the defaulted root should name BTC and trade it"

    # Same document with the root spelled out — identical run.
    spelled = ta.load_spec("root: BTC\n" + yaml, kind="single")
    spelled_fills = spelled.run(ta.PaperWallet(1000.0), snaps).fills
    assert [repr(f) for f in spelled_fills] == [repr(f) for f in rep.fills]


def test_an_omitted_root_with_no_symbol_param_is_a_build_error():
    """Nothing to trade and no data to infer from — reported, never guessed."""
    spec = ta.load_spec("long:\n  enter: !value true\n", kind="single")
    with pytest.raises(ta.SpecError, match="names no symbol"):
        spec.run(ta.PaperWallet(1000.0), _snaps_single("BTC", [100.0, 101.0]))


def test_load_spec_explicit_kind_override():
    """Passing `kind=` bypasses auto-detection."""
    spec = ta.load_spec(
        "root: BTC\nlong:\n  enter: !value true\n",
        kind="single",
    )
    assert spec.kind == "single"


# ---------------------------------------------------------------------------
# meta: the open-schema key fugazi carries but never interprets
# ---------------------------------------------------------------------------


def test_spec_meta_round_trips_as_plain_python():
    """`meta:` comes back as ordinary dicts/lists/scalars, not an opaque handle."""
    yaml = """
    root: BTC
    meta:
      service: strategy-lab
      revision: 17
      live: true
      tags: [momentum, crypto]
      owner: {desk: systematic}
    long:
      enter: !value true
    """
    spec = ta.load_spec(yaml)
    assert spec.meta == {
        "service": "strategy-lab",
        "revision": 17,
        "live": True,
        "tags": ["momentum", "crypto"],
        "owner": {"desk": "systematic"},
    }


def test_spec_meta_is_none_when_absent():
    """Absent `meta:` reads None — not an empty dict a caller has to disambiguate."""
    spec = ta.load_spec("root: BTC\nlong:\n  enter: !value true\n")
    assert spec.meta is None


def test_spec_meta_is_available_on_every_shape():
    """All five shapes carry it, so a caller never has to branch on `kind`."""
    docs = {
        "single": "root: BTC\nmeta: {tag: x}\nlong:\n  enter: !value true\n",
        "pairs": (
            "left: BTC\nright: ETH\nmeta: {tag: x}\n"
            "enter: !gt {lhs: !close {source: !pick {symbol: BTC}}, rhs: !value 0}\n"
        ),
        "basket": (
            "meta: {tag: x}\nselection: !top_bottom {longs: 1, shorts: 1}\n"
            "score: !close\nsizing: !value 1.0\n"
        ),
        "multi": "meta: {tag: x}\nlong:\n  enter: !value true\n",
        "portfolio": (
            "meta: {tag: x}\nchildren:\n"
            "  - name: c1\n    strategy: !buy_and_hold {root: BTC}\n"
        ),
    }
    for kind, yaml in docs.items():
        spec = ta.load_spec(yaml)
        assert spec.kind == kind, yaml
        assert spec.meta == {"tag": "x"}, kind


def test_spec_meta_does_not_change_a_run():
    """The whole contract: adding `meta:` cannot move a number."""
    body = "root: BTC\nlong:\n  enter: !value true\n"
    snaps = _snaps_single("BTC", [100.0, 101.0, 99.0, 104.0, 108.0])

    def equity(yaml):
        return ta.load_spec(yaml).run(ta.PaperWallet(1000.0), snaps).equity_curve

    assert equity(body) == equity(body + "meta: {service: strategy-lab}\n")


def test_a_misspelled_field_is_still_an_error():
    """`meta:` widens the surface by exactly one key, not by everything."""
    with pytest.raises(ValueError, match="sizng"):
        ta.load_spec("root: BTC\nsizng: !value 1.0\nlong:\n  enter: !value true\n")


# ---------------------------------------------------------------------------
# TradingCostsConfig
# ---------------------------------------------------------------------------


def test_trading_costs_from_dict():
    """The wrapper accepts a flat leg mapping (auto-hoisted to default)."""
    c = ta.TradingCostsConfig(
        {
            "commission": {"percentage": {"rate": 0.001}},
            "spread": {"bps": {"bps": 5}},
        }
    )
    assert "TradingCostsConfig" in repr(c)


def test_trading_costs_empty_is_zero_cost():
    """Missing / empty mapping is fine — resolves to a zero-cost config."""
    ta.TradingCostsConfig()
    ta.TradingCostsConfig({})


def test_trading_costs_scoped_shape():
    """The `default:` / `by_symbol:` structured shape also works."""
    c = ta.TradingCostsConfig(
        {
            "commission": {
                "default": {"percentage": {"rate": 0.001}},
                "by_symbol": {"BTC": {"percentage": {"rate": 0.0005}}},
            },
        }
    )
    assert "scoped=1" in repr(c) or "defaults=true" in repr(c)


def test_optimize_with_cost_config_lowers_equity():
    """A cost config passed to `ta.optimize` produces a smaller final equity."""
    yaml = "!buy_and_hold { root: BTC }"
    snaps = _snaps_single("BTC", [100.0, 101.0, 102.0, 103.0, 104.0])
    baseline = ta.optimize(yaml, snaps, cash=1000.0, grid=[{}])
    with_cost = ta.optimize(
        yaml,
        snaps,
        cash=1000.0,
        grid=[{}],
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
    root: BTC
    long:
      enter: !crosses_above
        lhs: !sma { period: !param FAST }
        rhs: !sma { period: !param SLOW }
    """


def _oscillating_snaps(n=120):
    """A 120-bar drifting sine — a path the trend document actually trades.

    `_trend_snaps` is monotone enough that the crossover never fires, which is
    fine for shape assertions but leaves every return moment at zero. The
    windows here differ in volatility, which is the case the pooling tests are
    about.
    """
    return _snaps_single(
        "BTC", [100.0 * (1.0 + 0.05 * math.sin(i / 4.0) + 0.004 * i) for i in range(n)]
    )


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


def test_sweep_is_a_sequence_of_its_rows():
    """`Sweep` exposed `.rows` and nothing else, so the obvious `for row in
    sweep` / `len(sweep)` / `sweep[0]` all failed. Indexing delegates to the
    materialised list, so slices and negative indices come out standard."""
    sweep = ta.optimize(
        _trend_yaml(),
        _trend_snaps(),
        cash=1000.0,
        grid=[{"FAST": [3, 5], "SLOW": [10, 15]}],
        best_by="risk_adjusted.sharpe",
    )
    assert len(sweep) == len(sweep.rows) == 4
    assert [r.values for r in sweep] == [r.values for r in sweep.rows]
    assert sweep[0].values == sweep.rows[0].values
    assert sweep[-1].values == sweep.rows[-1].values
    assert [r.values for r in sweep[:2]] == [r.values for r in sweep.rows[:2]]
    with pytest.raises(IndexError):
        _ = sweep[99]


def test_walkforward_result_is_a_sequence_of_its_folds():
    result = ta.optimize(
        _trend_yaml(),
        _trend_snaps(),
        cash=1000.0,
        grid=[{"FAST": [3, 5], "SLOW": [10]}],
        best_by="risk_adjusted.sharpe",
        walkforward=(20, 10),
    )
    assert len(result) == len(result.folds)
    assert [f.fold for f in result] == [f.fold for f in result.folds]
    assert result[0].fold == result.folds[0].fold
    assert result[-1].fold == result.folds[-1].fold


def test_optimize_smooth_ranks_by_the_neighbourhood_average():
    """`smooth=` populates `row.smoothed` / `row.support` and reorders by them."""
    sweep = ta.optimize(
        _trend_yaml(),
        _trend_snaps(),
        cash=1000.0,
        grid=[{"FAST": [3, 5, 7], "SLOW": [10, 15, 20]}],
        metric_names=["returns.total_pct"],
        best_by="returns.total_pct",
        smooth="box:1",
    )
    assert len(sweep.rows) == 9
    supports = [row.support for row in sweep.rows]
    # Both axes are evenly spaced, so 1.0 is the ceiling here; on an irregular
    # axis a denser-than-median stretch reads above it, deliberately.
    assert all(s is not None and 0.0 < s <= 1.0 + 1e-12 for s in supports)
    # A 3x3 box:1 lattice has exactly one fully interior cell.
    assert sum(abs(s - 1.0) < 1e-9 for s in supports) == 1
    # Rows come back ranked by the smoothed key, best first.
    smoothed = [row.smoothed for row in sweep.rows]
    assert all(v is not None for v in smoothed)
    assert smoothed == sorted(smoothed, reverse=True)
    # Every row's smoothed value is the renormalized mean of its neighbours'
    # raw values — recomputed here from the axis cells, so it also pins that
    # the axis-to-lattice mapping survived the sort.
    fasts, slows = [3, 5, 7], [10, 15, 20]
    raw = {}
    got = {}
    for row in sweep.rows:
        cell = (fasts.index(row.values["FAST"]), slows.index(row.values["SLOW"]))
        raw[cell] = row.metrics["returns.total_pct"]
        got[cell] = row.smoothed
    for i in range(3):
        for j in range(3):
            near = [
                raw[(i + di, j + dj)]
                for di in (-1, 0, 1)
                for dj in (-1, 0, 1)
                if 0 <= i + di < 3 and 0 <= j + dj < 3
            ]
            assert got[(i, j)] == pytest.approx(sum(near) / len(near))


def test_optimize_smooth_min_support_drops_thin_neighbourhoods():
    sweep = ta.optimize(
        _trend_yaml(),
        _trend_snaps(),
        cash=1000.0,
        grid=[{"FAST": [3, 5, 7], "SLOW": [10, 15, 20]}],
        metric_names=["returns.total_pct"],
        best_by="returns.total_pct",
        smooth="box:1",
        smooth_min_support=1.0,
    )
    kept = [row for row in sweep.rows if row.smoothed is not None]
    assert len(kept) == 1, "only the interior cell of a 3x3 clears full support"
    # Support is still reported for the rows it rejected.
    assert all(row.support is not None for row in sweep.rows)


def test_optimize_smooth_scale_pins_the_distance_scale():
    """`smooth_scale=` reaches the kernel, and `"index"` restores the old measure."""
    kwargs = dict(
        cash=1000.0,
        # Irregular on purpose: index space and value space disagree here, and
        # the same seven values typed in a different order must not.
        grid=[{"FAST": [3, 9, 4, 8, 5, 7, 6], "SLOW": [10, 15, 20]}],
        metric_names=["returns.total_pct"],
        best_by="returns.total_pct",
        smooth="box:1",
    )

    def by_params(sweep):
        return {
            (row.values["FAST"], row.values["SLOW"]): (row.smoothed, row.support)
            for row in sweep.rows
        }

    scrambled = ta.optimize(_trend_yaml(), _trend_snaps(), **kwargs)
    kwargs["grid"] = [{"FAST": [3, 4, 5, 6, 7, 8, 9], "SLOW": [10, 15, 20]}]
    sorted_ = ta.optimize(_trend_yaml(), _trend_snaps(), **kwargs)
    assert by_params(scrambled) == by_params(sorted_)

    # `index` is the documented way back to the order-dependent measure.
    kwargs["grid"] = [{"FAST": [3, 9, 4, 8, 5, 7, 6], "SLOW": [10, 15, 20]}]
    indexed = ta.optimize(_trend_yaml(), _trend_snaps(), smooth_scale="index", **kwargs)
    assert by_params(indexed) != by_params(sorted_)

    with pytest.raises(ValueError, match="linear"):
        ta.optimize(_trend_yaml(), _trend_snaps(), smooth_scale="quadratic", **kwargs)


def test_optimize_smooth_scale_pin_for_an_unknown_axis_is_refused():
    """A pin naming no swept axis is never looked up, so it would silently
    leave the axis on the automatic choice. `best_by` and `metric_names` both
    refuse an unresolvable name; this one used not to."""
    kwargs = dict(
        cash=1000.0,
        grid=[{"FAST": [3, 5, 7], "SLOW": 15}],
        metric_names=["returns.total_pct"],
        best_by="returns.total_pct",
        smooth="box:1",
    )
    # A typo, and a name that is a scalar rather than an axis.
    for pin in ("FASTT:linear", "SLOW:log"):
        with pytest.raises(ValueError, match=pin.split(":")[0]):
            ta.optimize(_trend_yaml(), _trend_snaps(), smooth_scale=pin, **kwargs)
    # The correctly spelled pin is unaffected.
    ta.optimize(_trend_yaml(), _trend_snaps(), smooth_scale="FAST:linear", **kwargs)


def test_optimize_smooth_over_a_concrete_point_list_is_refused():
    """`grid=` takes either a Cartesian block or a list of concrete points, and
    smoothing reads a lattice *per subgrid* — so a point list is N one-point
    lattices and `smooth=` would be the identity. It used to return the raw keys
    at `support=1.0`, which is also what a fully interior point reports, so the
    alarming reading and the reassuring one were the same number.
    `smooth_min_support=` cannot catch it either: any floor in 0..=1 passes 1.0.
    """
    block = [{"FAST": [3, 5], "SLOW": [10, 15]}]
    points = [{"FAST": f, "SLOW": s} for f in (3, 5) for s in (10, 15)]
    kwargs = dict(
        cash=1000.0,
        metric_names=["returns.total_pct"],
        best_by="returns.total_pct",
        smooth="box:1",
    )
    # Same twelve-ish points, two spellings. The block smooths.
    swept = ta.optimize(_trend_yaml(), _trend_snaps(), grid=block, **kwargs)
    assert all(r.support is not None for r in swept.rows)
    # The point list is refused rather than silently returning the raw ranking.
    with pytest.raises(ValueError, match="no lattice to average over"):
        ta.optimize(_trend_yaml(), _trend_snaps(), grid=points, **kwargs)
    # And a floor does not rescue it — that is the whole reason it is an error.
    with pytest.raises(ValueError, match="no lattice to average over"):
        ta.optimize(
            _trend_yaml(),
            _trend_snaps(),
            grid=points,
            smooth_min_support=1.0,
            **kwargs,
        )
    # Without `smooth=`, a concrete point list is perfectly ordinary.
    plain = ta.optimize(
        _trend_yaml(),
        _trend_snaps(),
        grid=points,
        cash=1000.0,
        metric_names=["returns.total_pct"],
        best_by="returns.total_pct",
    )
    assert len(plain.rows) == 4


def test_optimize_smooth_support_is_none_for_a_point_with_no_lattice():
    """A lone pinned point stacked beside a swept block: the grid as a whole
    smooths, so it is allowed, but that point carries its *raw* key into the
    smoothed ranking. `support` reports `None` — not `1.0`, which would read
    exactly like a fully interior point — and a floor discards it."""
    grid = [{"FAST": [3, 5, 7], "SLOW": [10, 15, 20]}, {"FAST": 9, "SLOW": 25}]
    kwargs = dict(
        cash=1000.0,
        metric_names=["returns.total_pct"],
        best_by="returns.total_pct",
        smooth="box:1",
    )
    sweep = ta.optimize(_trend_yaml(), _trend_snaps(), grid=grid, **kwargs)
    by_cell = {(r.values["FAST"], r.values["SLOW"]): r for r in sweep.rows}
    pinned = by_cell[(9, 25)]
    assert pinned.support is None, "no neighbourhood was measured for it"
    assert pinned.smoothed == pinned.metrics["returns.total_pct"]
    assert by_cell[(5, 15)].support == pytest.approx(1.0), "the block's interior"

    floored = ta.optimize(
        _trend_yaml(), _trend_snaps(), grid=grid, smooth_min_support=0.5, **kwargs
    )
    dropped = {(r.values["FAST"], r.values["SLOW"]): r for r in floored.rows}[(9, 25)]
    assert dropped.smoothed is None, "a floor asks for evidence it cannot offer"
    assert dropped.support is None


def test_optimize_panel_axis_substitutes_a_string_member_as_a_symbol():
    """The documented shape: members are instruments, and `panel_axis=` roots
    each on its own series."""
    doc = """
    root: !pick { symbol: !param SYM }
    long:
      enter: !crosses_above
        lhs: !sma { period: !param FAST }
        rhs: !sma { period: !param SLOW }
    """
    rising = [100.0 + i * 0.3 + (10 if 30 <= i < 50 else 0) for i in range(60)]
    panel = {
        "AAA": _snaps_single("AAA", rising),
        "BBB": _snaps_single("BBB", [v * 1.1 for v in rising]),
    }
    sweep = ta.optimize(
        doc,
        panel=panel,
        panel_axis="SYM",
        grid=[{"FAST": [3, 5], "SLOW": [10, 15]}],
        cash=1000.0,
        metric_names=["returns.total_pct"],
        best_by="returns.total_pct",
    )
    assert "SYM" not in sweep.best.values, (
        "the pooled axis is reduced over, not ranked on"
    )
    assert set(sweep.best.metrics_panel) == {"AAA", "BBB"}


def test_optimize_panel_composes_with_windowed_and_shrink():
    """`panel=` and `windowed=` used to be refused as a pair, which left
    `shrink=` unable to estimate anything in a sweep: with one measurement per
    member, member disagreement and backtest noise are the same quantity.

    They compose now, and the composition is not a nested reduction — the
    windowed documents ride *beside* each member's whole-run one as within-cell
    replicates, so no pooled number moves. Both halves are asserted here: that
    the pair is accepted at all, and that accepting it changed nothing."""
    doc = """
    root: !pick { symbol: !param SYM }
    long:
      enter: !crosses_above
        lhs: !sma { period: !param FAST }
        rhs: !sma { period: !param SLOW }
    """
    rising = [100.0 + i * 0.3 + (10 if 30 <= i < 50 else 0) for i in range(120)]
    panel = {
        "AAA": _snaps_single("AAA", rising),
        "BBB": _snaps_single("BBB", [v * 1.1 for v in rising]),
    }
    kwargs = dict(
        panel=panel,
        panel_axis="SYM",
        grid=[{"FAST": [3, 5], "SLOW": [10, 15]}],
        cash=1000.0,
        metric_names=["returns.total_pct"],
        best_by="returns.total_pct",
    )
    plain = ta.optimize(doc, **kwargs)
    windowed = ta.optimize(doc, windowed=40, **kwargs)

    def cells(sweep):
        return {
            tuple(sorted(r.values.items())): r.metrics["returns.total_pct"]
            for r in sweep.rows
        }

    assert cells(plain) == cells(windowed), (
        "windowed= supplies replication for lambda and must leave every pooled "
        "cell exactly where it was"
    )

    # And `shrink=` runs on top of it rather than being unreachable.
    shrunk = ta.optimize(doc, windowed=40, shrink=True, **kwargs)
    assert "SYM" not in shrunk.best.values
    assert set(shrunk.best.metrics_panel) == {"AAA", "BBB"}


def test_optimize_shrink_refuses_what_it_cannot_do():
    """`shrink=` needs a panel to pool across and a key to rank on, and refuses
    `risk_aversion=` — which charges for the spread between members while
    `shrink=` models it, so applying both pays for the same disagreement
    twice."""
    doc = """
    root: !pick { symbol: !param SYM }
    long:
      enter: !crosses_above
        lhs: !sma { period: !param FAST }
        rhs: !sma { period: !param SLOW }
    """
    rising = [100.0 + i * 0.3 + (10 if 30 <= i < 50 else 0) for i in range(120)]
    panel = {
        "AAA": _snaps_single("AAA", rising),
        "BBB": _snaps_single("BBB", [v * 1.1 for v in rising]),
    }
    grid = [{"FAST": [3, 5], "SLOW": [10, 15]}]

    with pytest.raises(ValueError, match="panel"):
        ta.optimize(
            doc,
            _snaps_single("AAA", rising),
            grid=grid,
            cash=1000.0,
            best_by="returns.total_pct",
            shrink=True,
        )

    with pytest.raises(ValueError, match="best_by"):
        ta.optimize(
            doc,
            panel=panel,
            panel_axis="SYM",
            grid=grid,
            cash=1000.0,
            shrink=True,
        )

    with pytest.raises(ValueError, match="risk_aversion|rival"):
        ta.optimize(
            doc,
            panel=panel,
            panel_axis="SYM",
            grid=grid,
            cash=1000.0,
            windowed=40,
            best_by="returns.total_pct",
            shrink=True,
            risk_aversion=1.0,
        )


def test_optimize_panel_axis_pools_over_a_typed_parameter():
    """`panel_axis=` used to substitute the member name as a JSON *string*,
    which is right for a ticker and made every typed slot unreachable —
    `!ema { period: !param FAST }` got `"5"` and failed to build. The CLI's
    `--pooled 'FAST=[5,10,15]'` has always been typed, so the two
    surfaces disagreed on one feature. A mapping key now carries its own JSON
    type through."""
    snaps = _trend_snaps()
    # One stream, three members: pooling over a nuisance *parameter* rather
    # than over instruments. Each member needs its own feed, so the same stream
    # is handed over once per member.
    panel = {f: list(snaps) for f in (3, 5, 7)}
    sweep = ta.optimize(
        _trend_yaml(),
        panel=panel,
        panel_axis="FAST",
        grid=[{"SLOW": [10, 15]}],
        cash=1000.0,
        metric_names=["returns.total_pct"],
        best_by="returns.total_pct",
    )
    assert len(sweep.rows) == 2, "FAST is pooled over, so only SLOW is swept"
    assert set(sweep.best.values) == {"SLOW"}
    # Members are labelled by the key, rendered without JSON quoting.
    assert set(sweep.best.metrics_panel) == {"3", "5", "7"}
    # All three reported, and the pooled cell is the mean over them.
    assert sweep.best.metrics_support["returns.total_pct"] == (3, 3)
    per_member = [m["returns"]["total_pct"] for m in sweep.best.metrics_panel.values()]
    assert sweep.best.metrics["returns.total_pct"] == pytest.approx(sum(per_member) / 3)


def test_optimize_panel_key_type_is_the_key_type_not_a_parse_of_its_label():
    """A key's Python type decides, so nothing is guessed from the text: a
    member genuinely named `"5"` stays the string `"5"`. That is the reason for
    typing the keys rather than parsing the names."""
    snaps = _trend_snaps()
    with pytest.raises(Exception, match="expected a nonzero usize|invalid type"):
        ta.optimize(
            _trend_yaml(),
            panel={"5": list(snaps), "7": list(snaps)},
            panel_axis="FAST",
            grid=[{"SLOW": [10, 15]}],
            cash=1000.0,
            metric_names=["returns.total_pct"],
            best_by="returns.total_pct",
        )
    # A key that no document could hold is refused at the boundary.
    with pytest.raises(TypeError, match="str, int, float or bool"):
        ta.optimize(
            _trend_yaml(),
            panel={(3, 5): list(snaps), (7, 9): list(snaps)},
            panel_axis="FAST",
            grid=[{"SLOW": [10, 15]}],
            cash=1000.0,
        )


def test_optimize_repeated_axis_value_is_refused():
    """Two equal values sit at distance 0, so the point becomes a full-weight
    neighbour of itself — and the duplicate costs a second backtest and row."""
    common = dict(
        cash=1000.0,
        metric_names=["returns.total_pct"],
        best_by="returns.total_pct",
    )

    def swept(fast):
        return ta.optimize(
            _trend_yaml(), _trend_snaps(), grid=[{"FAST": fast, "SLOW": 15}], **common
        )

    with pytest.raises(ValueError, match="FAST"):
        swept([3, 5, 5, 7])
    # `3` and `3.0` substitute identically, so they are one point.
    with pytest.raises(ValueError, match="3.0"):
        swept([3, 3.0, 7])
    # The same grid without the repeat is untouched.
    assert len(swept([3, 5, 7]).rows) == 3


def test_optimize_support_ignores_a_single_value_axis():
    """A numeric axis with one value is not a swept dimension, so it must not
    divide every point's support by the kernel's axis weight."""
    common = dict(
        cash=1000.0,
        metric_names=["returns.total_pct"],
        best_by="returns.total_pct",
        smooth="box:1",
    )
    listed = ta.optimize(
        _trend_yaml(),
        _trend_snaps(),
        grid=[{"FAST": [3, 5, 7, 9, 11], "SLOW": [15]}],
        **common,
    )
    scalar = ta.optimize(
        _trend_yaml(),
        _trend_snaps(),
        grid=[{"FAST": [3, 5, 7, 9, 11]}],
        params={"SLOW": 15},
        **common,
    )
    assert [r.support for r in listed.rows] == [r.support for r in scalar.rows]
    assert max(r.support for r in listed.rows) == pytest.approx(1.0)


def test_optimize_smooth_rejects_an_unknown_kernel():
    with pytest.raises(ValueError, match="box:R"):
        ta.optimize(
            _trend_yaml(),
            _trend_snaps(),
            cash=1000.0,
            grid=[{"FAST": [3, 5]}],
            metric_names=["returns.total_pct"],
            best_by="returns.total_pct",
            smooth="parabola:2",
        )


def test_optimize_walkforward_folds_carry_the_smoothed_is_key():
    result = ta.optimize(
        _trend_yaml(),
        _trend_snaps(),
        cash=1000.0,
        params={"SLOW": 15},
        grid=[{"FAST": [3, 5, 7]}],
        metric_names=["returns.total_pct"],
        best_by="returns.total_pct",
        walkforward=(20, 10),
        smooth="box:1",
    )
    assert len(result.folds) >= 1
    for fold in result.folds:
        assert fold.is_smoothed is not None
        assert fold.is_support is not None


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


def _pool_windows(windows):
    """Chan's pairwise pooling of per-window `(n, mean, M2)` moments.

    `returns.stddev_bar` is the `ddof = 1` estimator, so a window's centred
    second moment is `(n - 1) * s ** 2`.
    """
    n, mean, m2 = 0, 0.0, 0.0
    for w in windows:
        n_w = w["run"]["bars"]
        mean_w = w["returns"]["mean_bar"]
        m2_w = (n_w - 1) * w["returns"]["stddev_bar"] ** 2
        total = n + n_w
        delta = mean_w - mean
        mean += delta * n_w / total
        m2 += m2_w + delta * delta * n * n_w / total
        n = total
    return n, mean, m2


def _pooled_sharpe(windows, bars_per_year, risk_free_rate=0.0):
    n, mean, m2 = _pool_windows(windows)
    if n < 2:
        return None
    vol = math.sqrt(m2 / (n - 1)) * math.sqrt(bars_per_year)
    if vol == 0.0:
        return None
    return (mean * bars_per_year - risk_free_rate) / vol


def test_windowed_metrics_pool_to_the_whole_run_figures():
    """Per-window `(run.bars, mean_bar, stddev_bar)` are sufficient statistics.

    Pooling every window of a windowed sweep reproduces the *whole-run* Sharpe
    and annualized volatility of the same grid point exactly — which is what
    lets a caller score an arbitrary union of windows (CSCV / PBO) without the
    per-point return series ever leaving the process. See `docs/METRICS.md`,
    *Pooling windows*.
    """
    bpy, rf = 365.0, 0.03
    snaps = _oscillating_snaps()
    grid = [{"FAST": [3, 5], "SLOW": [12]}]
    common = dict(
        cash=1000.0,
        grid=grid,
        metric_names=[
            "risk_adjusted.sharpe",
            "returns.annualized_volatility_pct",
            "returns.mean_bar",
        ],
        bars_per_year=bpy,
        risk_free_rate=rf,
    )
    whole = ta.optimize(_trend_yaml(), snaps, **common)
    windowed = ta.optimize(_trend_yaml(), snaps, windowed=30, **common)

    assert len(whole.rows) == len(windowed.rows) == 2
    for direct, row in zip(whole.rows, windowed.rows):
        assert direct.values == row.values  # same grid point, same order
        n, mean, m2 = _pool_windows(row.metrics_windowed)

        # The windows tile the run, so the pooled count is the run's bar count.
        assert n == sum(w["run"]["bars"] for w in row.metrics_windowed)
        assert direct.metrics["risk_adjusted.sharpe"] is not None  # not vacuous
        assert direct.metrics["returns.mean_bar"] == pytest.approx(mean, rel=1e-12)

        vol_pct = math.sqrt(m2 / (n - 1)) * math.sqrt(bpy) * 100.0
        assert direct.metrics["returns.annualized_volatility_pct"] == pytest.approx(
            vol_pct, rel=1e-12
        )
        assert direct.metrics["risk_adjusted.sharpe"] == pytest.approx(
            _pooled_sharpe(row.metrics_windowed, bpy, rf), rel=1e-12
        )


def test_windowed_pooling_is_not_the_mean_of_window_sharpes():
    """The pooled figure is the one a CSCV pass wants, and it is *not* the mean.

    Guards the reason the sufficient statistics are worth publishing: averaging
    the windows' own Sharpes answers a different question whenever the windows
    differ in volatility, so a caller that has only per-window Sharpes cannot
    recover this number.
    """
    bpy = 365.0
    sweep = ta.optimize(
        _trend_yaml(),
        _oscillating_snaps(),
        cash=1000.0,
        grid=[{"FAST": [3], "SLOW": [12]}],
        metric_names=["risk_adjusted.sharpe"],
        windowed=30,
        bars_per_year=bpy,
    )
    (row,) = sweep.rows
    pooled = _pooled_sharpe(row.metrics_windowed, bpy)
    per_window = [
        w["risk_adjusted"]["sharpe"]
        for w in row.metrics_windowed
        if w["risk_adjusted"].get("sharpe") is not None
    ]
    assert pooled is not None
    assert len(per_window) >= 2
    assert pooled != pytest.approx(sum(per_window) / len(per_window), rel=1e-6)


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
root: X
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
root: X
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
        strategy: !buy_and_hold { root: A }
      - name: hold_b
        strategy: !buy_and_hold { root: B }
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
    assert w.equity == pytest.approx(report.equity_curve[-1])


def test_portfolio_run_rejects_a_non_wallet():
    snaps = _snaps_multi({"A": [100, 101]})
    with pytest.raises(TypeError, match="must be a PaperWallet"):
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
root: X
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
    whole_rep, _ = ta.load_spec(_RESUME_YAML).run_resumable(
        ta.PaperWallet(1000.0), snaps
    )

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


_RESUME_PAIRS_YAML = """
left: A
right: B
long_spread:
  enter: !lt
    lhs: !sub { lhs: !close { source: !pick { symbol: A } }, rhs: !close { source: !pick { symbol: B } } }
    rhs: !value -2.0
  exit: !gt
    lhs: !sub { lhs: !close { source: !pick { symbol: A } }, rhs: !close { source: !pick { symbol: B } } }
    rhs: !value 0.0
"""

_RESUME_MULTI_YAML = """
long:
  enter: !crosses_above
    lhs: !ema { period: 3, source: !close }
    rhs: !ema { period: 8, source: !close }
  exit: !crosses_below
    lhs: !ema { period: 3, source: !close }
    rhs: !ema { period: 8, source: !close }
sizing: !value 0.5
rebalance_on: !every 7
"""

_RESUME_BASKET_YAML = """
selection: !top_bottom { longs: 1, shorts: 1 }
score: !rsi { period: 5, source: !close }
sizing: !value 0.5
"""

_RESUME_PORTFOLIO_YAML = """
weights: !value [0.6, 0.4]
rebalance_on: !every 7
children:
  - name: fast_a
    strategy:
      root: A
      long:
        enter: !crosses_above
          lhs: !ema { period: 3, source: !close }
          rhs: !ema { period: 8, source: !close }
        exit: !crosses_below
          lhs: !ema { period: 3, source: !close }
          rhs: !ema { period: 8, source: !close }
  - name: slow_b
    strategy:
      root: B
      long:
        enter: !lt { lhs: !rsi { period: 5, source: !close }, rhs: !value 35.0 }
        exit: !gt { lhs: !rsi { period: 5, source: !close }, rhs: !value 65.0 }
"""


def _wobbly_b(n):
    import math

    return [100.0 + 8.0 * math.cos(i * 0.27) + 0.03 * i for i in range(n)]


def _two_symbol_snaps(n):
    return _snaps_multi({"A": _wobbly(n), "B": _wobbly_b(n)})


def _assert_chunked_resume(case, yaml, snaps, splits, cash=10_000.0):
    """N-way chunked resume must be indistinguishable from one uninterrupted run.

    Rebuilds the spec **and a fresh wallet** for every chunk — the calling
    convention a live deployment actually uses, where each chunk is a separate
    process with nothing but the state JSON carried across.

    Three chunks, not two: a two-way split exercises save then restore but never
    restore then *re*-save, which is where state a resumed strategy fails to
    carry forward goes missing.
    """
    whole, _ = ta.load_spec(yaml).run_resumable(ta.PaperWallet(cash), snaps)

    bounds = [0, *splits, len(snaps)]
    state = None
    curve = []
    fills = 0
    for i, (start, end) in enumerate(zip(bounds, bounds[1:])):
        rep, state = ta.load_spec(yaml).run_resumable(
            ta.PaperWallet(cash), snaps[start:end], resume=state
        )
        curve.extend(rep.equity_curve)
        fills += len(rep.fills)

    assert len(curve) == len(whole.equity_curve), f"{case}: curve length"
    # Exact: serde's float_roundtrip keeps every f64 bit-identical through JSON.
    assert curve == whole.equity_curve, f"{case}: chunked run diverged"
    assert fills == len(whole.fills), f"{case}: fill count"


@pytest.mark.parametrize(
    "case,yaml,multi",
    [
        ("single", _RESUME_YAML, False),
        ("pairs", _RESUME_PAIRS_YAML, True),
        ("multi", _RESUME_MULTI_YAML, True),
        ("basket", _RESUME_BASKET_YAML, True),
        ("portfolio", _RESUME_PORTFOLIO_YAML, True),
    ],
    # The YAML would otherwise become the test id, several lines of it.
    ids=["single", "pairs", "multi", "basket", "portfolio"],
)
def test_run_resumable_matches_uninterrupted_run_across_three_chunks(case, yaml, multi):
    """Every spec shape, not just single — the property is what the feature is for."""
    snaps = _two_symbol_snaps(60) if multi else _snaps_single("X", _wobbly(60))
    _assert_chunked_resume(case, yaml, snaps, [20, 40])


def test_flatten_closes_the_position_in_the_wallet():
    """`flatten=True` must leave a genuinely flat book, not just a flat report."""
    hold = """
    root: X
    long:
      enter: !gt { lhs: !close, rhs: !value 0.0 }
    """
    snaps = _snaps_single("X", _wobbly(40))

    carried_wallet = ta.PaperWallet(1000.0)
    carried, _ = ta.load_spec(hold).run_resumable(carried_wallet, snaps)
    flat_wallet = ta.PaperWallet(1000.0)
    flat, state = ta.load_spec(hold).run_resumable(flat_wallet, snaps, flatten=True)

    assert len(flat.fills) == len(carried.fills) + 1, "one closing leg"
    assert abs(carried_wallet.position("X")) > 0.0, "the carried run still holds"
    assert flat_wallet.position("X") == 0.0, "the flattened wallet must be flat"

    # And resuming from that state continues from flat: it has to re-enter.
    resumed, _ = ta.load_spec(hold).run_resumable(
        ta.PaperWallet(1000.0), _snaps_single("X", _wobbly(20)), resume=state
    )
    assert resumed.fills, "a resume from a flattened state should re-enter"


def test_warm_up_advances_state_without_trading():
    """A pause gap warms the indicators but books nothing."""
    snaps = _snaps_single("X", _wobbly(60))
    wallet = ta.PaperWallet(1000.0)

    # Replay the first 30 bars as a gap: no trades, but the EMAs warm.
    state = ta.load_spec(_RESUME_YAML).warm_up(wallet, snaps[:30])
    assert wallet.funds == 1000.0, "warm_up must not spend anything"
    assert wallet.position("X") == 0.0, "warm_up must not open a position"

    # Resuming from it behaves as though those bars had been seen: a strategy
    # that had to re-warm from scratch would sit out its whole warm-up instead.
    warmed, _ = ta.load_spec(_RESUME_YAML).run_resumable(
        ta.PaperWallet(1000.0), snaps[30:], resume=state
    )
    cold, _ = ta.load_spec(_RESUME_YAML).run_resumable(
        ta.PaperWallet(1000.0), snaps[30:]
    )
    assert (
        len(warmed.fills) != len(cold.fills) or warmed.equity_curve != cold.equity_curve
    ), "a warmed resume should differ from a cold start over the same bars"


def test_run_resumable_rejects_a_stale_format_version():
    """A state file from another build is refused, not mis-parsed."""
    import json

    snaps = _snaps_single("X", _wobbly(20))
    _rep, state = ta.load_spec(_RESUME_YAML).run_resumable(
        ta.PaperWallet(1000.0), snaps
    )
    stale = json.loads(state)
    stale["format_version"] += 1

    with pytest.raises(ValueError, match="format version"):
        ta.load_spec(_RESUME_YAML).run_resumable(
            ta.PaperWallet(1000.0), snaps, resume=json.dumps(stale)
        )


def test_run_resumable_rejects_mismatched_shape():
    """Resuming a single-shape state into a pairs spec is rejected."""
    snaps = _snaps_single("X", _wobbly(20))
    _rep, state = ta.load_spec(_RESUME_YAML).run_resumable(
        ta.PaperWallet(1000.0), snaps
    )

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
root: X
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
    m = spec.evaluate(
        ta.PaperWallet(1000.0), snaps, bars_per_year=365.0, montecarlo=cfg
    )

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
    cfg = ta.MonteCarloConfig(
        permutations=150, seed=42, null="rerun", metrics=["sharpe"]
    )
    a = spec.evaluate(ta.PaperWallet(1000.0), snaps, montecarlo=cfg)["montecarlo"]
    b = spec.evaluate(ta.PaperWallet(1000.0), snaps, montecarlo=cfg)["montecarlo"]
    assert a["metrics"][0]["ci_lower"] == b["metrics"][0]["ci_lower"]
    assert a["metrics"][0]["p_value_rerun"] == b["metrics"][0]["p_value_rerun"]


# ---------------------------------------------------------------------------
# fugazi.montecarlo: the deterministic resampling primitive behind the fan chart
# ---------------------------------------------------------------------------


def test_resample_index_matrix_shape_range_and_determinism():
    a = ta.montecarlo.resample_index_matrix(
        50, 10, scheme="stationary", block=8.0, seed=3
    )
    b = ta.montecarlo.resample_index_matrix(
        50, 10, scheme="stationary", block=8.0, seed=3
    )
    assert a == b, "same seed must reproduce the whole matrix"
    assert len(a) == 10 and all(len(row) == 50 for row in a)
    assert all(0 <= i < 50 for row in a for i in row)
    c = ta.montecarlo.resample_index_matrix(
        50, 10, scheme="stationary", block=8.0, seed=4
    )
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
        permutations=perms,
        scheme="stationary",
        block=block,
        seed=seed,
        null="none",
        metrics=["returns.total_pct"],
    )
    mc = spec.evaluate(ta.PaperWallet(1000.0), snaps, montecarlo=cfg)["montecarlo"]
    samples = mc["samples"]
    col = samples["metric_names"].index("returns.total_pct")
    ci_rows = next(
        s["rows"] for s in samples["sets"] if s["estimator"] == "bootstrap_ci"
    )

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


# ---------------------------------------------------------------------------
# slot_demand / slot_demands — the tag-keyed view of `check`'s type discipline
# ---------------------------------------------------------------------------


def test_slot_demand_reports_the_output_a_slot_requires():
    assert ta.slot_demand("and", "lhs") == ["bool"]
    assert ta.slot_demand("and", "rhs") == ["bool"]
    assert ta.slot_demand("sma", "source") == ["scalar"]
    assert ta.slot_demand("atr", "source") == ["candle"]
    assert ta.slot_demand("close", "source") == ["atom"]
    assert ta.slot_demand("str_eq", "lhs") == ["str"]
    # A leading `!` is accepted, since that is how the tag is written in YAML.
    assert ta.slot_demand("!ema", "source") == ["scalar"]


def test_slot_demand_reports_alternatives_and_passthroughs():
    # Either output is accepted here.
    assert ta.slot_demand("changed", "source") == ["bool", "scalar"]
    assert ta.slot_demand("match", "on") == ["scalar", "str"]
    # A passthrough demands nothing — an empty list, not None.
    assert ta.slot_demand("unstable", "source") == []
    assert ta.slot_demand("resample", "inner") == []


def test_slot_demand_is_none_when_the_slot_holds_no_expression():
    assert ta.slot_demand("sma", "period") is None
    assert ta.slot_demand("sma", "no_such_slot") is None
    assert ta.slot_demand("no_such_tag", "source") is None
    # A book selector takes `!strategy_book` / `!portfolio_book`, not a value.
    assert ta.slot_demand("drawdown", "source") is None


def test_slot_demand_covers_the_positional_payload_pseudo_slots():
    assert ta.slot_demand("not", "source") == ["bool"]
    assert ta.slot_demand("all", "item") == ["bool"]
    assert ta.slot_demand("match", "case value") == ["scalar"]


def test_slot_demands_returns_every_slot_of_a_tag():
    assert ta.slot_demands("if_else") == {
        "cond": ["bool"],
        "then": ["scalar"],
        "otherwise": ["scalar"],
    }
    assert ta.slot_demands("keltner_upper") == {
        "source": ["scalar"],
        "candle_source": ["candle"],
    }
    # No expression slots at all.
    assert ta.slot_demands("is_weekday") == {}
    assert ta.slot_demands("no_such_tag") == {}


def test_slot_demand_agrees_with_the_grammar_descriptor():
    """Same datum, two surfaces — the descriptor's `node_output` is stamped from
    `slot_demand`, so a consumer can use either and get the same answer."""
    for tag in ta.spec_grammar()["tags"]:
        if tag["group"] != "node":
            continue
        for form in tag["forms"]:
            for field in form["fields"]:
                slot = "case value" if field["type"] == "match_cases" else field["name"]
                assert field.get("node_output") == ta.slot_demand(tag["name"], slot), (
                    f"!{tag['name']} `{field['name']}`"
                )


def test_slot_demand_names_the_tags_that_can_fill_a_slot():
    """The use it exists for: given a slot, offer only the admissible tags."""
    want = ta.slot_demand("and", "lhs")
    fillable = {
        t["name"]
        for t in ta.spec_grammar()["tags"]
        if t["group"] == "node" and t["output"] in want
    }
    assert {"gt", "crosses_above", "and", "not"} <= fillable
    assert "sma" not in fillable and "atr" not in fillable


# ---------------------------------------------------------------------------
# reads: the series a document needs in its snapshots but never trades
# ---------------------------------------------------------------------------


def test_spec_reads_lists_cross_asset_picks():
    """A regime gate on another asset shows up in `reads`, sorted and deduped."""
    yaml = """
    root: ETH
    long:
      enter: !gt
        lhs: !close {source: !pick {symbol: BTC}}
        rhs: !sma {period: 20, source: !close {source: !pick {symbol: BTC}}}
      exit: !lt
        lhs: !close {source: !pick {symbol: SOL}}
        rhs: !value 100
    """
    assert ta.load_spec(yaml).reads == ["BTC", "SOL"]


def test_spec_reads_is_empty_without_cross_asset_picks():
    """The common document reads only what it trades, and says so with `[]`."""
    spec = ta.load_spec("root: BTC\nlong:\n  enter: !value true\n")
    assert spec.reads == []


def test_spec_reads_ignores_the_documents_own_root_key():
    """Only a `symbol:` *inside a* `!pick` counts — not the traded-series key.

    Load-bearing now that `root:` is itself an expression, and usually a
    `!pick`: a naive walk would report every document's own traded symbol as a
    read.
    """
    spec = ta.load_spec(
        "root: BTC\nlong:\n  enter: !gt {lhs: !close {source: !pick {symbol: BTC}}, "
        "rhs: !value 0}\n"
    )
    # `BTC` is picked explicitly here, so it *is* a read — the point is that a
    # document naming no `!pick` at all (above) reports nothing, rather than
    # reporting its own `root:`.
    assert spec.reads == ["BTC"]


def test_spec_reads_excludes_a_root_that_is_the_only_pick():
    """The regression the key-skip exists for: a `root:` is a `!pick`, but a
    traded series is not a read-only one."""
    spec = ta.load_spec(
        "root: !pick {symbol: BTC}\nlong:\n  enter: !gt {lhs: !close, rhs: !value 0}\n"
    )
    assert spec.reads == []


def test_spec_reads_are_what_a_cross_asset_run_needs_in_its_snapshots():
    """The contract: supply every `reads` symbol and the gate resolves; omit one
    and it silently never fires. `reads` is how a caller assembling snapshots by
    hand checks which case they are in — the CLI makes the same check against
    `--series` and refuses the run, but here the snapshots are the caller's."""
    yaml = """
    root: A
    long:
      enter: !gt {lhs: !close {source: !pick {symbol: B}}, rhs: !value 100}
      exit: !never
    sizing: !value 1.0
    """
    spec = ta.load_spec(yaml)
    assert spec.reads == ["B"]

    def bar(px):
        return ta.Candle(px, px, px, px, 1000.0)

    a_closes = [10.0, 11.0, 12.0, 13.0]
    b_closes = [90.0, 99.0, 101.0, 102.0]

    def snaps(include_b):
        out = []
        for i, (a, b) in enumerate(zip(a_closes, b_closes)):
            s = ta.Snapshot()
            s.push("A", ta.Atom(bar(a), time=i * 86_400_000))
            if include_b:
                s.push("B", ta.Atom(bar(b), time=i * 86_400_000))
            out.append(s)
        return out

    with_b = spec.run(ta.PaperWallet(10_000.0), snaps(True))
    without_b = spec.run(ta.PaperWallet(10_000.0), snaps(False))
    assert len(with_b.fills) > 0
    assert len(without_b.fills) == 0


# ---------------------------------------------------------------------------
# optimize: a ruined row is reported but never selected
# ---------------------------------------------------------------------------


def _doomed_yaml():
    """A short held from the first bar and never covered, at a sweepable size.

    Swept above 1x, so the runs that use it hand `optimize` a `max_gross` to
    match: an unlevered account fits a `sizing: 3.0` short back to 1x, and a 1x
    short does not get wiped out by this rally. The `LEVERAGE` axis only means
    what its name says if the account will carry it.
    """
    return """
    root: BTC
    sizing: !param LEVERAGE
    short:
      enter: !lt
        lhs: !value 0
        rhs: !value 1
      exit: !never
    """


def _rally_snaps():
    """A rally steep enough to bury a leveraged short, with a long tail after it.

    The tail is the point: ruin pins the equity curve at zero, so the dead
    cell's return series is one `-100%` bar followed by hundreds of exact zeros,
    and a *tail* statistic reads that as the calmest account in the grid. On a
    40-bar path the wipeout would still be inside the bottom 5% and the metric
    would report it; a real run has no such luck.
    """
    px, prices = 100.0, []
    for i in range(4):
        px *= 1.01 if i % 2 else 0.99
        prices.append(px)
    for _ in range(2):
        px *= 1.25
        prices.append(px)
    for i in range(400):
        px *= 1.01 if i % 2 else 0.99
        prices.append(px)
    return _snaps_single("BTC", prices)


def test_optimize_reports_ruin_but_never_selects_it():
    """A wiped-out cell keeps its metrics and loses its candidacy.

    `best_by` is `returns.var_95` on purpose: it is *lower-is-better* and a
    ruined run's tail is mostly the flat zeros the driver pins after ruin, so on
    raw arithmetic the dead cell wins it. That is the whole class of defect —
    only the metrics anchored to terminal wealth (`cagr_pct`, `calmar`) read
    ruin, and every bar-return statistic is blind to it. The guard is on the
    ranking, not on the metric, so the number below is still there to read.
    """
    sweep = ta.optimize(
        _doomed_yaml(),
        _rally_snaps(),
        cash=1000.0,
        max_gross=3.0,
        grid=[{"LEVERAGE": [0.2, 3.0]}],
        metric_names=["returns.var_95", "returns.cagr_pct", "run.ruin_bar"],
        best_by="returns.var_95",
    )
    by_leverage = {row.values["LEVERAGE"]: row for row in sweep.rows}
    dead, alive = by_leverage[3.0], by_leverage[0.2]

    assert dead.ruined and dead.ruin_bar is not None, "the 3x short must be wiped out"
    assert not alive.ruined and alive.ruin_bar is None
    assert dead.ruin_bar == dead.metrics["run.ruin_bar"], (
        "the property and the column agree"
    )

    # The number survives — both cells report a var_95, and the dead one's is
    # the better of the two on raw arithmetic.
    assert dead.metrics["returns.var_95"] is not None
    assert dead.metrics["returns.var_95"] <= alive.metrics["returns.var_95"]
    assert dead.metrics["returns.cagr_pct"] == pytest.approx(-100.0)

    # The candidacy does not.
    assert sweep.best is not None
    assert not sweep.best.ruined
    assert sweep.best.values["LEVERAGE"] == 0.2
    assert sweep.rows[0].values["LEVERAGE"] == 0.2, "ruined rows sort last"


def test_optimize_smooth_gives_a_ruined_cell_no_weight():
    """A ruined cell has no ranking key, so it neither smooths nor contributes."""
    sweep = ta.optimize(
        _doomed_yaml(),
        _rally_snaps(),
        cash=1000.0,
        max_gross=3.0,
        grid=[{"LEVERAGE": [0.2, 0.4, 3.0]}],
        metric_names=["returns.var_95"],
        best_by="returns.var_95",
        smooth="box:1",
    )
    by_leverage = {row.values["LEVERAGE"]: row for row in sweep.rows}
    assert by_leverage[3.0].ruined
    assert by_leverage[3.0].smoothed is None, "no ranking key, so nothing to smooth"
    # Its solvent neighbour's average rests on one cell fewer.
    assert by_leverage[0.4].support < 1.0


# ---------------------------------------------------------------------------
# Exception hierarchy
# ---------------------------------------------------------------------------


def test_errors_subclass_value_error_so_old_handlers_still_catch():
    """The whole tree hangs off `ValueError`, which is what these sites raised
    before it existed — so adding resolution cannot break an existing handler."""
    for exc in (ta.FugaziError, ta.SpecError, ta.WalletError, ta.FetchError):
        assert issubclass(exc, ValueError)
    for exc in (ta.SpecError, ta.WalletError, ta.FetchError):
        assert issubclass(exc, ta.FugaziError)


def test_a_document_that_will_not_build_raises_spec_error():
    bad = "root: BTC\nlong:\n  enter: !gt { lhs: !get { key: absent }, rhs: !value 1 }"
    with pytest.raises(ta.SpecError):
        ta.load_spec(bad).run(ta.PaperWallet(1000.0), _trend_snaps())
    # ...and the `!tag > ` breadcrumb still reaches the message.
    try:
        ta.load_spec(bad).run(ta.PaperWallet(1000.0), _trend_snaps())
    except ta.SpecError as e:
        assert "at:" in str(e)


def test_an_account_refusal_raises_wallet_error_not_spec_error():
    """The distinction that motivates the split: a refused order is a property
    of the account right now, not of the strategy — so it must not be catchable
    as a SpecError, and vice versa."""
    wallet = ta.PaperWallet(100.0)
    with pytest.raises(ta.WalletError):
        wallet.adjust_funds(-500.0)
    assert not issubclass(ta.WalletError, ta.SpecError)
    assert not issubclass(ta.SpecError, ta.WalletError)


def test_call_errors_stay_type_errors():
    """`TypeError` is not rehomed under FugaziError — an ordinary Python call
    bug must not be caught by `except FugaziError`."""
    with pytest.raises(TypeError) as excinfo:
        ta.ema(ta.close(), 3).update("not a candle")
    assert not isinstance(excinfo.value, ta.FugaziError)


# ---------------------------------------------------------------------------
# Interruptibility
# ---------------------------------------------------------------------------


def _gil_share(fn):
    """Fraction of its ceiling a 1 ms-sleep companion thread reaches during `fn`.

    ~1.0 means the GIL was available throughout; a held GIL starves it to single
    digits. Returns `(share, elapsed)`.
    """
    import threading
    import time

    ticks = 0
    stop = threading.Event()

    def ticker():
        nonlocal ticks
        while not stop.is_set():
            ticks += 1
            time.sleep(0.001)

    companion = threading.Thread(target=ticker)
    companion.start()
    try:
        started = time.monotonic()
        fn()
        elapsed = time.monotonic() - started
    finally:
        stop.set()
        companion.join()
    return ticks / (elapsed * 1000), elapsed


@pytest.mark.parametrize("path", ["run", "run_resumable", "warm_up"])
def test_no_drive_path_blocks_other_python_threads(path):
    """Every way of driving a spec releases the GIL, not just `run`.

    `run_resumable` and `warm_up` were left holding it when `run` was fixed, and
    `warm_up` is the worst case: priming over months of history is the longest
    single call on the surface, and it is exactly what a live process does at
    startup while a websocket reader wants to drain.
    """
    bars = [10, 9, 8, 7, 6, 7, 9, 12, 15, 18, 21, 22, 21, 20, 18, 15, 12, 10, 8, 6]
    snaps = _snaps_single("BTC", bars * 30_000)
    spec = ta.load_spec(_trend_yaml(), params={"FAST": 3, "SLOW": 8})
    call = {
        "run": lambda: spec.run(ta.PaperWallet(1000.0), snaps),
        "run_resumable": lambda: spec.run_resumable(ta.PaperWallet(1000.0), snaps),
        "warm_up": lambda: spec.warm_up(ta.PaperWallet(1000.0), snaps),
    }[path]

    share, elapsed = _gil_share(call)
    if elapsed < 0.5:
        pytest.skip(f"machine too fast to measure ({elapsed:.2f}s)")
    assert share > 0.25, (
        f"`{path}` gave a companion thread {share:.0%} of its ceiling in "
        f"{elapsed:.2f}s — it is still holding the GIL"
    )


def test_a_long_sweep_can_be_interrupted():
    """A grid sweep is the longest thing on the surface and could not be Ctrl-C'd.

    The obvious fix — poll inside the per-row closure — fails *silently*, and
    this test is what caught it: CPython runs signal handlers on the main thread
    only, and `rayon::ThreadPool::install` blocks the caller rather than letting
    it steal, so the main thread evaluates no rows and reaches no poll. Measured
    that way, the interrupt landed at 3.00s against a 2.93s sweep — i.e. never.

    The sweep now runs on a scoped thread with the main thread as watchdog. Only
    the default `jobs` is timed here — a single-job sweep goes through the same
    pool and the same watchdog, and sizing it to run long enough would cost the
    suite ~8x the wall clock to exercise identical code.
    """
    import os
    import signal
    import subprocess
    import sys
    import time

    bars = [10, 9, 8, 7, 6, 7, 9, 12, 15, 18, 21, 22, 21, 20, 18, 15, 12, 10, 8, 6]
    snaps = _snaps_single("BTC", bars * 400)
    grid = [{"FAST": list(range(2, 22)), "SLOW": list(range(25, 55))}]  # 600 rows

    def sweep():
        return ta.optimize(
            _trend_yaml(),
            snaps,
            grid=grid,
            best_by="risk_adjusted.sharpe",
        )

    started = time.monotonic()
    sweep()
    uninterrupted = time.monotonic() - started
    if uninterrupted < 1.0:
        pytest.skip(
            f"machine too fast to time a sweep interrupt ({uninterrupted:.2f}s)"
        )

    fire_at = uninterrupted / 4
    killer = subprocess.Popen(
        [
            sys.executable,
            "-c",
            f"import os,signal,time; time.sleep({fire_at}); "
            f"os.kill({os.getpid()}, signal.SIGINT)",
        ]
    )
    started = time.monotonic()
    try:
        with pytest.raises(KeyboardInterrupt):
            sweep()
        elapsed = time.monotonic() - started
    finally:
        killer.wait()

    assert elapsed < uninterrupted / 2, (
        f"ran {elapsed:.2f}s after a signal at {fire_at:.2f}s; an "
        f"uninterrupted sweep takes {uninterrupted:.2f}s — it did not stop early"
    )


def test_a_sweep_still_parallelises_after_the_watchdog():
    """The watchdog must not cost the parallelism it sits beside: if workers had
    to touch the GIL to poll, `jobs=N` would serialise back to one."""
    import time

    bars = [10, 9, 8, 7, 6, 7, 9, 12, 15, 18, 21, 22, 21, 20, 18, 15, 12, 10, 8, 6]
    snaps = _snaps_single("BTC", bars * 200)
    grid = [{"FAST": list(range(2, 12)), "SLOW": list(range(25, 45))}]

    def timed(jobs):
        started = time.monotonic()
        rows = ta.optimize(
            _trend_yaml(),
            snaps,
            grid=grid,
            best_by="risk_adjusted.sharpe",
            jobs=jobs,
        )
        return time.monotonic() - started, rows

    serial, serial_rows = timed(1)
    parallel, parallel_rows = timed(None)
    if serial < 1.0 or os.cpu_count() in (None, 1):
        pytest.skip("not enough work or cores to observe a speedup")

    # Same answers, whichever way they were computed.
    assert [r.values for r in serial_rows] == [r.values for r in parallel_rows]
    assert serial > parallel * 1.5, (
        f"jobs=1 took {serial:.2f}s and jobs=all {parallel:.2f}s — the sweep is "
        "no longer parallel"
    )


def test_a_long_run_can_be_interrupted():
    """A run used to hold the GIL and poll nothing, so Ctrl-C in a notebook did
    nothing until the run finished on its own. The snapshots now go through
    `classes::interruptible`, which checks Python's signal handlers every 4096
    bars and ends the drive.

    The signal comes from a *separate process* on purpose: that is what a real
    Ctrl-C is. A Python timer thread would first need the GIL, which the run
    still holds — see TODO.md.

    The assertion is calibrated against this machine rather than a fixed number.
    Without the fix a `KeyboardInterrupt` still surfaces — just *after* the run
    completes, at the next bytecode boundary — so only "it stopped early" tells
    the two apart, and "early" has to mean early relative to the real duration.
    """
    import os
    import signal
    import subprocess
    import sys
    import time

    bars = [10, 9, 8, 7, 6, 7, 9, 12, 15, 18, 21, 22, 21, 20, 18, 15, 12, 10, 8, 6]
    snaps = _snaps_single("BTC", bars * 30_000)  # ~600k bars: seconds of work
    spec = ta.load_spec(_trend_yaml(), params={"FAST": 3, "SLOW": 8})

    started = time.monotonic()
    spec.run(ta.PaperWallet(1000.0), snaps)
    uninterrupted = time.monotonic() - started
    if uninterrupted < 1.0:
        pytest.skip(f"machine too fast to time an interrupt ({uninterrupted:.2f}s run)")

    fire_at = uninterrupted / 4
    killer = subprocess.Popen(
        [
            sys.executable,
            "-c",
            f"import os,signal,time; time.sleep({fire_at}); "
            f"os.kill({os.getpid()}, signal.SIGINT)",
        ]
    )
    started = time.monotonic()
    try:
        with pytest.raises(KeyboardInterrupt):
            spec.run(ta.PaperWallet(1000.0), snaps)
        elapsed = time.monotonic() - started
    finally:
        killer.wait()

    # Stopped near the signal, not merely raised once the work was done anyway.
    assert elapsed < uninterrupted / 2, (
        f"ran {elapsed:.2f}s after a signal at {fire_at:.2f}s; an uninterrupted "
        f"run of the same series takes {uninterrupted:.2f}s — the drive did not "
        "stop early, so the signal check is not reaching it"
    )
