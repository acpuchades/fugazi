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
    score: !roc { source: !close { source: !pick { symbol: !slot SYM } }, period: 2 }
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

    The template body is typed-parsed at load with `!slot SYM` held as a hole,
    so a misspelled tag raises here — not on the first bar of a `run()` that
    happens to quote a symbol. Same rule for a multi-asset side's `enter:`.
    """
    basket = """
    selection: !top_bottom { longs: 1, shorts: 1 }
    score: !smaa { source: !close { source: !pick { symbol: !slot SYM } }, period: 2 }
    sizing: !value 1.0
    """
    with pytest.raises(Exception, match="smaa"):
        ta.load_spec(basket)

    multi = """
    long:
      enter: !gt { lhs: !close { source: !pick { symbol: !slot SYM } }, rsh: 50 }
    """
    with pytest.raises(Exception, match="rsh"):
        ta.load_spec(multi)


def test_load_multi_and_run():
    yaml = """
    long:
      enter: !gt { lhs: !close { source: !pick { symbol: !slot SYM } }, rhs: 50 }
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


def test_score_table_reproduces_the_sweeps_own_shrinkage():
    """A hand-built table over the same scores must give the **same** answer the
    integrated sweep reports.

    This is the whole reason to expose the estimator rather than let callers
    reimplement it. Without this identity the two paths are free to drift — a
    change to the fit would move one and not the other, and nothing would say
    so. It is asserted on every component, not just lambda, because the ways
    they could diverge (a different grand mean, a different residual, a
    different support denominator) do not all show up in the headline.

    The table is reconstructed from what the sweep exposes: each row's
    per-member windowed metrics are the same replicates the sweep fed its own
    table, so a caller with the same measurements can rebuild it exactly."""
    import math

    doc = """
    root: !pick { symbol: !param SYM }
    long:
      enter: !crosses_above
        lhs: !sma { period: !param FAST }
        rhs: !sma { period: !param SLOW }
      exit: !crosses_below
        lhs: !sma { period: !param FAST }
        rhs: !sma { period: !param SLOW }
    sizing: !value 1.0
    """
    day = 86_400_000

    def cycle(sym, period, amp):
        return [
            ta.Snapshot(
                {
                    sym: ta.Atom(
                        ta.Candle(
                            *(
                                4
                                * [
                                    200.0
                                    + amp * math.sin(2 * math.pi * i / period)
                                    + i * 0.02
                                ]
                            ),
                            1.0,
                        ),
                        time=i * day,
                    )
                }
            )
            for i in range(900)
        ]

    members = ["CHOP", "TREND"]
    section, leaf = "risk_adjusted", "sharpe"
    sweep = ta.optimize(
        doc,
        panel={"CHOP": cycle("CHOP", 6, 18), "TREND": cycle("TREND", 90, 40)},
        panel_axis="SYM",
        grid=[{"FAST": [2, 3, 5, 15, 30], "SLOW": [8, 20, 45, 90]}],
        windowed=120,
        best_by="sharpe",
        metric_names=[f"{section}.{leaf}"],
        shrink=True,
        cash=10_000.0,
    )
    integrated = sweep.shrinkage
    assert integrated.disagreement is not None

    # Rebuild the same table by hand from the same per-window readings.
    #
    # `sweep.rows` is sorted by rank while the sweep fitted its own table in
    # lattice order, so this table is a row *permutation* of that one — which is
    # exactly why the identity is worth asserting on every component: every
    # field of the summary is invariant to relabelling the rows (the grand mean,
    # both margins' variances, the interaction, the counts), so a mismatch would
    # mean the two paths genuinely disagree rather than that the rows moved.
    table = ta.ScoreTable(rows=len(sweep.rows), members=len(members))
    for r, row in enumerate(sweep.rows):
        for m, name in enumerate(members):
            windows = row.metrics_panel_windowed[name]
            # `.get`, not `[…]`: an undefined metric is an **absent key**, not a
            # `None` — a window with no trades has no Sharpe at all. Those
            # readings are simply not pushed, which is the ragged table the
            # estimator is built for, and the sweep's own table has the same
            # holes because it drops them the same way.
            values = [w.get(section, {}).get(leaf) for w in windows]
            table.extend(r, m, [v for v in values if v is not None])

    rebuilt = table.decompose().summary
    assert rebuilt.disagreement == pytest.approx(integrated.disagreement)
    assert rebuilt.support == pytest.approx(integrated.support)
    assert rebuilt.cells == integrated.cells
    assert rebuilt.live_rows == integrated.live_rows
    assert rebuilt.live_members == integrated.live_members
    assert rebuilt.row_variance == pytest.approx(integrated.row_variance)
    assert rebuilt.member_variance == pytest.approx(integrated.member_variance)
    assert rebuilt.interaction_variance == pytest.approx(
        integrated.interaction_variance
    )
    assert rebuilt.residual_variance == pytest.approx(integrated.residual_variance)
    assert rebuilt.mean_replicates == pytest.approx(integrated.mean_replicates)
    assert rebuilt.balanced == integrated.balanced


def test_breadth_and_demeaned_are_named_but_still_tuples():
    """The three 4-tuples are named records now. `members` sat at index 2 in
    `effective_breadth` and index 3 in `demeaned`, each with another plausible
    count opposite it, and nothing caught a transposition.

    Naming them is only safe if it is not a break, so this pins the tuple
    behaviour that 0.86 shipped: destructuring, indexing (including negative and
    slice), length, equality against a plain tuple, and hashing. That is the
    compatibility claim, and the one thing that would silently stop being
    true."""
    peaks = {0: 1, 1: 1, 2: 4}
    table = ta.ScoreTable.from_cells(
        [
            [
                [10.0 - abs(r - peaks[m]) + d * 0.05 for d in (-1, 0, 1, 2)]
                for m in range(3)
            ]
            for r in range(6)
        ]
    )
    breadth = table.decompose().selection_breadth
    assert breadth is not None

    # Named.
    assert breadth.members == 3
    assert isinstance(breadth.effective, float)
    assert isinstance(breadth.pairs, int)

    # ...and still a tuple, in the documented order.
    effective, rho, members, pairs = breadth
    assert (effective, rho, members, pairs) == (
        breadth.effective,
        breadth.mean_correlation,
        breadth.members,
        breadth.pairs,
    )
    assert len(breadth) == 4
    assert breadth[0] == effective and breadth[-1] == pairs
    assert breadth[1:3] == (rho, members)
    assert breadth == (effective, rho, members, pairs)
    assert hash(breadth) == hash((effective, rho, members, pairs))

    # The same contract on a DemeanedScore, whose field order differs — which is
    # exactly why they are named.
    d = table.decompose()
    row_scores = d.demeaned
    assert row_scores is not None
    # `SweepRow.demeaned` is the DemeanedScore; build one via a pooled sweep is
    # covered elsewhere, so here just pin that the two records disagree about
    # where `members` sits and both say so by name.
    assert ta.PanelBreadth.__doc__ and ta.DemeanedScore.__doc__


def test_score_table_estimates_over_a_caller_built_matrix():
    """The estimator is reachable without `optimize(panel=…)`.

    `shrink=` is a parameter of the pooled sweep, which is no use to a caller
    that reduces across members with its own machinery — there is nothing to
    plumb it into. `ScoreTable` is the same estimator with the sweep taken off
    the front: hand it a row x member matrix of replicates and it hands back
    lambda, the demeaned key, and the surface each member selects off.

    The fixture has members 0 and 1 agreeing on row 1 while member 2 peaks on
    row 4, so a correct `shrunk` surface must recover *both* answers — an
    implementation that always returned the consensus would pass a test that
    only checked lambda."""
    peaks = {0: 1, 1: 1, 2: 4}
    cells = [
        [[10.0 - abs(r - peaks[m]) + d * 0.05 for d in (-1, 0, 1, 2)] for m in range(3)]
        for r in range(6)
    ]
    table = ta.ScoreTable.from_cells(cells)
    assert (table.rows, table.members) == (6, 3)
    assert table.populated == 18
    assert table.observations == 72
    assert table.replicated_cells == 18

    d = table.decompose()
    assert d is not None
    assert d.summary.disagreement > 0.9, "members that peak apart disagree"

    surface = d.shrunk
    assert surface is not None
    for m, want in peaks.items():
        column = [
            (surface[r][m], r) for r in range(table.rows) if surface[r][m] is not None
        ]
        assert max(column)[1] == want, (
            f"member {m} should select row {want} off the shrunk surface, got "
            f"{max(column)[1]}"
        )

    # Letting three unrelated members each select is three searches over the
    # grid, which is what a caller must deflate against.
    effective, _rho, members, _pairs = d.selection_breadth
    assert members == 3
    assert effective > 2.5, effective


def test_score_table_without_replication_still_demeans():
    """One observation per cell cannot separate disagreement from noise, so
    there is no lambda and no surface to select off — but the *additive* fit
    needs no replication, so the demeaned key is still there.

    That split matters: demeaning is the cheap half a caller can always have,
    and it is what makes a cross-member spread mean "ranks consistently well"
    rather than "these members are alike"."""
    peaks = {0: 1, 1: 1, 2: 4}
    table = ta.ScoreTable(rows=6, members=3)
    for r in range(6):
        for m in range(3):
            table.push(r, m, 10.0 - abs(r - peaks[m]))
    assert table.replicated_cells == 0

    d = table.decompose()
    assert d is not None
    assert d.summary.disagreement is None, "no replication identifies no lambda"
    assert d.summary.residual_variance is None
    assert d.shrunk is None, "no lambda means no defensible surface"
    assert d.selection_breadth is None, "and nothing to correlate"

    # The additive part is fitted and usable all the same.
    assert len(d.demeaned) == 6 and len(d.demeaned[0]) == 3
    assert all(v is not None for row in d.demeaned for v in row)
    assert len(d.row_effects) == 6
    assert len(d.member_effects) == 3


def test_score_table_keeps_holes_as_holes():
    """A pair you never measured is an empty cell, not a zero — end to end.

    A substituted zero is indistinguishable from a measurement, so it would sink
    into the fit and every reading downstream would silently rest on it. The
    hole has to survive into `demeaned`, `shrunk` and `interactions`."""
    table = ta.ScoreTable(rows=5, members=3)
    for r in range(5):
        for m in range(3):
            if (r, m) == (1, 2):
                continue  # member 2 never reported row 1
            table.extend(r, m, [float(r + m), float(r + m) + 1.0])
    assert table.cell(1, 2) == []
    assert table.cell_mean(1, 2) is None
    assert table.populated == 14

    d = table.decompose()
    assert d is not None
    assert d.demeaned[1][2] is None
    assert d.interactions[1][2] is None
    assert d.shrunk[1][2] is None
    # ...and the cells around it are unaffected.
    assert d.demeaned[1][0] is not None


def test_score_table_refuses_a_ragged_input_and_an_unfittable_table():
    """A ragged *input* is a bug; a ragged *table* is ordinary. The first
    raises, the second is spelled by passing an empty sequence.

    And a table too sparse to carry the fit returns `None` rather than a
    zero-filled answer — "there is not enough table" is not "the answer is
    zero"."""
    with pytest.raises(ValueError, match="every row must span the same members"):
        ta.ScoreTable.from_cells([[[1.0], [2.0]], [[3.0]]])

    # Legitimately ragged: the hole is an empty sequence, and it fits.
    ok = ta.ScoreTable.from_cells(
        [[[1.0, 1.5], [2.0, 2.5], [3.0, 3.5]] for _ in range(4)]
    )
    assert ok.decompose() is not None

    # Under the cell floor: no fit, and the counts say why.
    sparse = ta.ScoreTable(rows=2, members=2)
    sparse.push(0, 0, 1.0)
    sparse.push(1, 1, 2.0)
    assert sparse.populated == 2
    assert sparse.decompose() is None


def test_optimize_shrinkage_is_readable_and_none_is_not_zero():
    """`shrink=` used to be write-only from Python: the flag was accepted, it
    silently reordered the ranking, and nothing it computed was reachable. The
    whole observable effect of a shrunk sweep was *the ranking changed, for
    reasons nothing could display*.

    Both states are asserted here, because the distinction is the one that can
    mislead: `disagreement` is a **number** when `windowed=` supplies the
    replication, and **`None`** without it — never `0.0`. On an unreplicated
    table disagreement and noise are the same sum of squares, so no split
    exists; reading that as "the members agree perfectly" would invert the
    finding."""
    doc = """
    root: !pick { symbol: !param SYM }
    long:
      enter: !crosses_above
        lhs: !sma { period: !param FAST }
        rhs: !sma { period: !param SLOW }
      exit: !crosses_below
        lhs: !sma { period: !param FAST }
        rhs: !sma { period: !param SLOW }
    sizing: !value 1.0
    """
    rising = [100.0 + i * 0.3 + (10 if 30 <= i < 50 else 0) for i in range(240)]
    kwargs = dict(
        panel={
            "AAA": _snaps_single("AAA", rising),
            "BBB": _snaps_single("BBB", [v * 1.1 for v in rising]),
        },
        panel_axis="SYM",
        grid=[{"FAST": [3, 5], "SLOW": [10, 15]}],
        cash=1000.0,
        best_by="returns.total_pct",
    )

    # Replicated: lambda is a number, and everything around it is readable.
    replicated = ta.optimize(doc, windowed=60, **kwargs)
    s = replicated.shrinkage
    assert s is not None, "a pooled sweep must expose its decomposition"
    assert isinstance(s.disagreement, float), s.disagreement
    assert 0.0 <= s.disagreement <= 1.0, s.disagreement
    assert isinstance(s.parameter_matters, bool)
    assert isinstance(s.verdict, str) and s.verdict
    assert s.cells > 0 and 0.0 <= s.support <= 1.0
    assert s.residual_variance is not None, "replicated tables identify the residual"
    # The demeaned ranking key is reachable per row, as a 4-tuple.
    mean, std, defined, members = replicated.best.demeaned
    assert defined <= members == 2
    assert isinstance(mean, float) and isinstance(std, float)

    # Unreplicated: None, and emphatically not 0.0.
    plain = ta.optimize(doc, **kwargs)
    assert plain.shrinkage is not None, "the other components are still defined"
    assert plain.shrinkage.disagreement is None, (
        "without replication there is no lambda to report — 0.0 would read as "
        "'the members agree perfectly', which is the opposite finding"
    )
    assert plain.shrinkage.disagreement != 0.0
    assert plain.shrinkage.residual_variance is None
    assert plain.shrinkage.verdict == "not estimable without replication"
    # ...while the components that do not depend on separating the two survive.
    assert plain.shrinkage.cells > 0
    assert isinstance(plain.shrinkage.interaction_variance, float)

    # `shrunk` says which of the two orderings you are holding.
    assert ta.optimize(doc, windowed=60, shrink=True, **kwargs).shrunk is True
    assert replicated.shrunk is False


def test_panel_shrinkage_repr_cannot_show_lambda_without_its_caveat():
    """`parameter_matters` comes with `disagreement`, not optionally: a high
    lambda on a grid that barely moves the metric is not the finding it looks
    like. `verdict` folds the caveat in and `repr()` shows both, so neither can
    be printed alone and mislead."""
    doc = """
    root: !pick { symbol: !param SYM }
    long:
      enter: !crosses_above
        lhs: !sma { period: !param FAST }
        rhs: !sma { period: !param SLOW }
    """
    rising = [100.0 + i * 0.3 + (10 if 30 <= i < 50 else 0) for i in range(240)]
    sweep = ta.optimize(
        doc,
        panel={
            "AAA": _snaps_single("AAA", rising),
            "BBB": _snaps_single("BBB", [v * 1.1 for v in rising]),
        },
        panel_axis="SYM",
        grid=[{"FAST": [3, 5], "SLOW": [10, 15]}],
        windowed=60,
        cash=1000.0,
        best_by="returns.total_pct",
    )
    text = repr(sweep.shrinkage)
    assert "disagreement=" in text
    assert "parameter_matters=" in text
    assert "verdict=" in text
    # And `lambda` is not reachable under its keyword spelling, by construction.
    assert not hasattr(sweep.shrinkage, "lambda")


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


def test_optimize_shrink_returns_a_parameter_set_per_member():
    """Stage 4's point: a plain sweep can hand back one parameter set per
    member, not just a better-conditioned single answer.

    The CLI writes these to a sibling CSV; Python has no file, so
    `sweep.member_winners` is the only route to them. `independent_searches` is
    reported beside it because per-member selection searches the grid harder
    than complete pooling does, and the deflated Sharpe has already been
    widened to match."""
    import math

    doc = """
    root: !pick { symbol: !param SYM }
    long:
      enter: !crosses_above
        lhs: !sma { period: !param FAST }
        rhs: !sma { period: !param SLOW }
      exit: !crosses_below
        lhs: !sma { period: !param FAST }
        rhs: !sma { period: !param SLOW }
    sizing: !value 1.0
    """
    day = 86_400_000

    def cycle(sym, period, amp):
        return [
            ta.Snapshot(
                {
                    sym: ta.Atom(
                        ta.Candle(
                            *(
                                4
                                * [
                                    200.0
                                    + amp * math.sin(2 * math.pi * i / period)
                                    + i * 0.02
                                ]
                            ),
                            1.0,
                        ),
                        time=i * day,
                    )
                }
            )
            for i in range(900)
        ]

    # A tight cycle only a short lookback tracks, and a long one only a long
    # lookback survives — two members with genuinely different optima.
    sweep = ta.optimize(
        doc,
        panel={"CHOP": cycle("CHOP", 6, 18), "TREND": cycle("TREND", 90, 40)},
        panel_axis="SYM",
        grid=[{"FAST": [2, 3, 5, 15, 30], "SLOW": [8, 20, 45, 90]}],
        windowed=120,
        best_by="sharpe",
        shrink=True,
        cash=10_000.0,
    )

    winners = sweep.member_winners
    assert set(winners) == {"CHOP", "TREND"}, winners
    assert winners["CHOP"] != winners["TREND"], (
        f"members with different optima must get different parameters, got {winners}"
    )
    # Two unrelated ranking surfaces are two searches, and that is what the
    # trial count was scaled by.
    assert sweep.independent_searches == pytest.approx(2.0), sweep.independent_searches


def test_optimize_without_shrink_reports_no_per_member_selection():
    """The readouts are absent rather than defaulted when nothing selected per
    member: an empty dict and `None`, not a fabricated single-member answer."""
    doc = """
    root: !pick { symbol: !param SYM }
    long:
      enter: !crosses_above
        lhs: !sma { period: !param FAST }
        rhs: !sma { period: !param SLOW }
    """
    rising = [100.0 + i * 0.3 + (10 if 30 <= i < 50 else 0) for i in range(120)]
    sweep = ta.optimize(
        doc,
        panel={
            "AAA": _snaps_single("AAA", rising),
            "BBB": _snaps_single("BBB", [v * 1.1 for v in rising]),
        },
        panel_axis="SYM",
        grid=[{"FAST": [3, 5], "SLOW": [10, 15]}],
        cash=1000.0,
        best_by="returns.total_pct",
    )
    assert sweep.member_winners == {}
    assert sweep.independent_searches is None


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


_ALL_IN_YAML = """
root: X
long:
  enter: !gt { lhs: !close, rhs: !value 0.0 }
"""


def _all_in_state(snaps, seed=10_000.0):
    """Run to a fully-invested book and return (wallet, state)."""
    wallet = ta.PaperWallet(seed)
    _rep, state = ta.load_spec(_ALL_IN_YAML).run_resumable(wallet, snaps)
    return wallet, state


@pytest.mark.parametrize("seed", [0.0, 10_000.0])
@pytest.mark.parametrize("warm_first", [False, True])
def test_a_fully_invested_state_resumes_whatever_the_wallet_was_seeded_with(
    seed, warm_first
):
    """A resumed run is seeded from the account it restores, not from the number
    the caller happened to construct the wallet with.

    Both of these used to abort the interpreter. The seed reaches `Book::new`,
    whose "strictly positive" assert is a `panic!` — and a `PanicException` is a
    `BaseException`, so it walks straight past every `except Exception` in the
    calling application rather than failing one deployment.

    The trigger is ordinary, not exotic: sizing defaults to all-in, so a
    fully-invested single-asset strategy has exactly zero cash, and the
    `warm_up` -> `run_resumable` handoff a pause gap produces re-presents that
    book to a wallet whose own positions look, from the outside, like somebody
    else's.
    """
    spec = ta.load_spec(_ALL_IN_YAML)
    snaps = _snaps_single("X", _wobbly(12))
    _, state = _all_in_state(snaps[:6])

    wallet = ta.PaperWallet(seed)
    if warm_first:
        state = spec.warm_up(wallet, snaps[6:7], resume=state)
        rep, _ = spec.run_resumable(wallet, snaps[7:], resume=state)
    else:
        rep, _ = spec.run_resumable(wallet, snaps[6:], resume=state)

    # Not merely "did not crash": the resumed curve is the one an uninterrupted
    # run would have produced over those same bars.
    whole, _ = ta.load_spec(_ALL_IN_YAML).run_resumable(ta.PaperWallet(10_000.0), snaps)
    assert rep.equity_curve[-1] == pytest.approx(whole.equity_curve[-1])


def test_a_resumed_strategy_is_not_told_its_own_position_is_somebody_elses():
    """The account a resumed run is handed already holds what it opened last
    chunk. Counted as an external holding, the strategy resumes reading itself
    as flat and its own equity as the cash beside the position — which is zero
    for an all-in book.
    """
    spec = ta.load_spec(_ALL_IN_YAML)
    snaps = _snaps_single("X", _wobbly(12))
    wallet, state = _all_in_state(snaps[:6])
    held = wallet.position("X")
    assert held > 0.0 and wallet.funds == 0.0

    # Continue against that very wallet — the shape a live deployment has.
    spec.run_resumable(wallet, snaps[6:], resume=state)
    assert wallet.position("X") == pytest.approx(held), (
        "a resumed all-in strategy must not re-enter a position it already holds"
    )


def test_a_users_own_position_is_still_carved_out_on_a_resume():
    """...and the netting subtracts only what is ours. A position the strategy
    never opened stays the user's across a resume, or the fix above would hand
    them their own holdings to trade.
    """
    spec = ta.load_spec(_ALL_IN_YAML)
    snaps = _snaps_single("X", _wobbly(12))

    wallet = ta.PaperWallet(10_000.0)
    wallet.update("U", ta.Candle(100.0, 100.0, 100.0, 100.0, 1.0))
    wallet.set_position("U", 10.0)
    wallet.update("U", ta.Candle(100.0, 100.0, 100.0, 100.0, 1.0))
    assert wallet.position("U") == pytest.approx(10.0)

    _rep, state = spec.run_resumable(wallet, snaps[:6])
    spec.run_resumable(wallet, snaps[6:], resume=state)
    assert wallet.position("U") == pytest.approx(10.0), (
        "the user's own position was traded by the strategy"
    )


def test_a_capital_less_account_is_a_catchable_error_not_a_panic():
    """A cold start against an account with nothing in it is bad input, and bad
    input is reported. The distinction that matters is catchability: this must
    be reachable by `except Exception`.
    """
    with pytest.raises(Exception, match="initial equity must be strictly positive"):
        ta.load_spec(_ALL_IN_YAML).run_resumable(
            ta.PaperWallet(0.0), _snaps_single("X", _wobbly(6))
        )


def test_warming_over_no_bars_still_returns_a_resumable_state():
    """Restoring a book without advancing it — an empty pause gap, or a look at
    what a deployment is holding — must hand back a state you can resume from.

    Recomputing `last_bar` from an empty chunk answers `None`, which drops a
    position in time the state already knew.
    """
    import json

    spec = ta.load_spec(_ALL_IN_YAML)
    # Timestamped, so `last_bar` has something to carry in the first place.
    snaps = [
        ta.Snapshot({"X": ta.Atom(ta.Candle(v, v, v, v, 1000.0), None, i * 86_400_000)})
        for i, v in enumerate(_wobbly(12))
    ]
    _wallet, state = _all_in_state(snaps[:6])
    assert json.loads(state)["last_bar"] is not None

    wallet = ta.PaperWallet(10_000.0)
    warmed = spec.warm_up(wallet, [], resume=state)
    assert json.loads(warmed)["last_bar"] == json.loads(state)["last_bar"]

    # And it round-trips: the state is a real resume point, not a dead end.
    spec.run_resumable(wallet, snaps[6:], resume=warmed)


# ---------------------------------------------------------------------------
# `hold=`: per-symbol closeout targets.
# ---------------------------------------------------------------------------


def test_hold_at_zero_closes_one_position_like_flatten():
    """`hold={sym: 0.0}` is `flatten` scoped to one symbol, so on a one-symbol
    book the two must land on exactly the same account."""
    snaps = _snaps_single("X", _wobbly(40))

    flat_wallet = ta.PaperWallet(1000.0)
    flat, _ = ta.load_spec(_ALL_IN_YAML).run_resumable(flat_wallet, snaps, flatten=True)
    held_wallet = ta.PaperWallet(1000.0)
    held, _ = ta.load_spec(_ALL_IN_YAML).run_resumable(
        held_wallet, snaps, hold={"X": 0.0}
    )

    assert held_wallet.position("X") == 0.0
    assert held_wallet.funds == pytest.approx(flat_wallet.funds)
    assert len(held.fills) == len(flat.fills)
    assert held.equity_curve == flat.equity_curve


def test_hold_resizes_to_a_target_and_pins_it_across_chunks():
    """A non-zero target resizes rather than closes, and re-issuing it holds the
    position there — an absolute target, not a delta. That is what makes a
    standing operator instruction expressible without the driver owning a clock.
    """
    spec = ta.load_spec(_ALL_IN_YAML)
    snaps = _snaps_single("X", _wobbly(24))

    wallet = ta.PaperWallet(10_000.0)
    _rep, state = spec.run_resumable(wallet, snaps[:8], hold={"X": 3.0})
    assert wallet.position("X") == pytest.approx(3.0)

    # The strategy would re-enter all-in on every one of these bars; the
    # standing instruction wins each time.
    for lo in range(8, 24, 4):
        _rep, state = spec.run_resumable(
            wallet, snaps[lo : lo + 4], resume=state, hold={"X": 3.0}
        )
        assert wallet.position("X") == pytest.approx(3.0)

    # Nothing about the strategy changed underneath: with the instruction
    # dropped and the book allowed to move again, the same document sizes
    # itself back up to a full position.
    unheld = ta.PaperWallet(10_000.0)
    spec.run_resumable(unheld, snaps)
    assert unheld.position("X") > 3.0


def test_hold_leaves_symbols_it_does_not_name_alone():
    """Only the named symbols move; everything else trades as the document says."""
    yaml = """
    long:
      enter: !gt { lhs: !close, rhs: !value 0.0 }
    sizing: !value 0.4
    """
    snaps = _two_symbol_snaps(20)

    carried_wallet = ta.PaperWallet(10_000.0)
    ta.load_spec(yaml, kind="multi").run_resumable(carried_wallet, snaps)
    held_wallet = ta.PaperWallet(10_000.0)
    ta.load_spec(yaml, kind="multi").run_resumable(held_wallet, snaps, hold={"A": 1.5})

    assert held_wallet.position("A") == pytest.approx(1.5)
    assert held_wallet.position("B") == pytest.approx(carried_wallet.position("B"))


def test_hold_and_flatten_together_are_refused():
    """They are one instruction — `flatten` is `hold` at zero for everything
    open — so the combination gets an error rather than a precedence rule."""
    snaps = _snaps_single("X", _wobbly(10))
    with pytest.raises(ValueError, match="same instruction"):
        ta.load_spec(_ALL_IN_YAML).run_resumable(
            ta.PaperWallet(1000.0), snaps, flatten=True, hold={"X": 0.0}
        )


def test_an_empty_hold_is_the_same_as_none():
    """Nothing named, nothing touched — and in particular not an error, so a
    caller can pass its instruction map through unconditionally."""
    snaps = _snaps_single("X", _wobbly(20))
    wallet = ta.PaperWallet(1000.0)
    ta.load_spec(_ALL_IN_YAML).run_resumable(wallet, snaps, hold={})
    assert wallet.position("X") > 0.0


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
    # A leading `!` is accepted, since that is how the tag is written in YAML.
    assert ta.slot_demand("!ema", "source") == ["scalar"]


def test_slot_demand_reports_alternatives_and_passthroughs():
    # Either output is accepted here.
    assert ta.slot_demand("changed", "source") == ["bool", "scalar"]
    assert ta.slot_demand("match", "on") == ["scalar", "str"]
    assert ta.slot_demand("eq", "lhs") == ["scalar", "str"]
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


# ---------------------------------------------------------------------------
# check_spec: validating a document nobody has bound values for yet
# ---------------------------------------------------------------------------

# Every placeholder defaultless, including the two the 0.81 default `root:`
# spells out. This is what an authoring tool has in hand at the moment a
# strategy is saved, and `load_spec` refuses it — correctly, since it is about
# to hand back something runnable.
UNBOUND = """
root: !pick { symbol: !param SYMBOL, freq: !param FREQ }
long:
  enter: !crosses_above
    lhs: !sma { period: !param FAST }
    rhs: !sma { period: !param SLOW }
"""


def test_check_spec_validates_a_document_with_nothing_bound():
    """The case the binding exists for.

    A strategy is written once and parameterised per run, so at the moment it is
    stored its knobs have no values — and `load_spec(text, params={})` refuses
    exactly that, which is every strategy whose author wrote a knob at all. This
    pins both halves: the refusal is still there for the path that returns
    something runnable, and `check_spec` accepts the same document.
    """
    with pytest.raises(ta.SpecError, match="`FAST` is not set"):
        ta.load_spec(UNBOUND, params={})

    check = ta.check_spec(UNBOUND)
    assert check.kind == "single"
    assert [h.name for h in check.holes] == ["FAST", "FREQ", "SLOW", "SYMBOL"]


def test_check_spec_types_each_placeholder_from_where_it_sits():
    """The half worth more than the verdict.

    A required placeholder has no default to read a type off, which is why a
    caller doing this itself ends up guessing from the parameter's *name*.
    `check_spec` answers it from the parse — the slot the placeholder actually
    sits in — so `SYMBOL` is a string because `!pick`'s `symbol:` is, and `FAST`
    is a number because `!sma`'s `period:` is.
    """
    assert ta.check_spec(UNBOUND).param_types == {
        "FAST": "number",
        "FREQ": "frequency",
        "SLOW": "number",
        "SYMBOL": "symbol",
    }


def test_check_spec_reports_a_declared_type_over_an_inferred_one():
    """A `type:` declaration outranks the parse: the author said what the value
    is, and `integer` is sharper than the `number` any position could demand."""
    doc = """
    root: !pick { symbol: !param { key: SYM, type: string } }
    long:
      enter: !gt
        lhs: !sma { period: !param { key: FAST, type: integer } }
        rhs: !value 0
    """
    check = ta.check_spec(doc)
    assert check.param_types == {"FAST": "integer", "SYM": "string"}
    fast = next(h for h in check.holes if h.name == "FAST")
    assert fast.declared == "integer"
    # The declaration is finer than the demand, and both are visible.
    assert fast.demanded == ["number"]
    assert fast.origin == "param"


def test_check_spec_does_not_relax_anything_but_the_missing_values():
    """The one thing a check must not become is "validation off"."""
    for name, bad in [
        ("unknown tag", "root: BTC\nlong:\n  enter: !nope { period: !param P }\n"),
        (
            "misspelled field",
            "root: BTC\nlong:\n  enter: !gt { lhs: !sma { perioD: !param P }, rhs: !value 0 }\n",
        ),
        (
            "a Real where a Bool is required",
            "root: BTC\nlong:\n  enter: !sma { period: !param P }\n",
        ),
        (
            "a portfolio weight nothing would ever read",
            "children:\n  - name: a\n    strategy: { root: BTC, long: { enter: !value true } }\n"
            "weights: !equity\n",
        ),
    ]:
        with pytest.raises(ta.SpecError):
            ta.check_spec(bad)
            pytest.fail(f"{name} was accepted")


def test_check_spec_refuses_a_placeholder_its_positions_contradict():
    """A check that only counted placeholders could not catch this, and it is
    the one placeholder defect that is decidable with no values at all: no
    single value is both a ticker and a period, so the document can never run
    whatever the caller eventually passes."""
    with pytest.raises(ta.SpecError, match="`X` is used as number and as symbol"):
        ta.check_spec(
            "root: !pick { symbol: !param X }\n"
            "long:\n  enter: !gt { lhs: !sma { period: !param X }, rhs: !value 0 }\n"
        )


def test_check_spec_builds_a_document_that_is_fully_determined():
    """`built` is not a weaker verdict on the parse — it says whether the
    *extra* check ran. It catches what a typed parse structurally cannot (a leaf
    with no asset to read in a shape holding more than one), so it runs whenever
    nothing is left undetermined, and is skipped where building would report a
    document error for a document whose only gap is an input nobody supplied."""
    determined = (
        "root: BTC\nlong:\n  enter: !gt { lhs: !sma { period: 20 }, rhs: !value 0 }\n"
    )
    check = ta.check_spec(determined)
    assert check.built and check.holes == []

    for name, text in [
        ("a root left to the input", determined.replace("root: BTC\n", "")),
        (
            "an overlay column only real data supplies",
            "root: BTC\nlong:\n  enter: !gt { lhs: !get { key: funding }, rhs: !value 0 }\n",
        ),
        (
            "a placeholder standing for a whole expression",
            "root: BTC\nlong:\n  enter: !gt { lhs: !param SIG, rhs: !value 0 }\n",
        ),
    ]:
        assert not ta.check_spec(text, kind="single").built, name

    # The build is also where a pair sized on a price-reading leaf with no
    # `source:` is caught — the case `check`'s build was added for.
    with pytest.raises(ta.SpecError):
        ta.check_spec(
            "left: BTC\nright: ETH\nlong_spread:\n"
            "  enter: !gt { lhs: !close { source: !pick { symbol: BTC } }, rhs: !value 0.0 }\n"
            "sizing: !vol_target { target: 0.2, window: 30, bars_per_year: 365 }\n"
        )


def test_check_spec_handles_a_placeholder_inside_a_deferred_template():
    """A basket's `score:` is not reached by the typed parse — it is held as a
    raw tree and re-parsed per symbol at build time. A placeholder inside one is
    the case that breaks if check mode stops covering the build, and baskets are
    exactly what a parameterised authoring surface produces."""
    check = ta.check_spec(
        "universe: !any_of [BTC, ETH]\n"
        "selection: !top_bottom { longs: 1, shorts: 0 }\n"
        "score: !sma { period: !param LOOK }\nsizing: !value 1.0\n"
    )
    assert check.kind == "basket"
    assert check.param_types == {"LOOK": "number"}
    assert check.built


def test_check_spec_leaves_nothing_behind_between_calls():
    """The observation ledger is a thread-local the parse appends to. A check
    that raised must not hand its placeholders to the next one — which is how a
    caller validating in a loop (or a pool worker reused across requests) would
    see them."""
    doc = "root: !pick { symbol: !param KEPT }\nlong:\n  enter: !value true\n"
    with pytest.raises(ta.SpecError):
        ta.check_spec(
            "root: !pick { symbol: !param LEAKED }\nlong:\n  enter: !nope {}\n"
        )
    assert ta.check_spec(doc).param_types == {"KEPT": "symbol"}

    # ...and the same after one that succeeded and built.
    ta.check_spec(
        "root: BTC\nlong:\n  enter: !gt { lhs: !sma { period: 5 }, rhs: !value 0 }\n"
    )
    assert ta.check_spec(doc).param_types == {"KEPT": "symbol"}


def test_check_spec_does_not_leave_check_mode_on_for_the_next_load():
    """`check_mode()` is a thread-local RAII guard, and the one thing it must
    never do is outlive the check that took it: a leaked guard would put the
    *next* ordinary `load_spec` on the hole-aware path, which is how a caller
    validating in a loop — or a process-pool worker reused across requests —
    would silently start accepting documents that have no values.

    Pinned on both exits, because the error path is the one a worker hits
    first."""
    unset = "root: BTC\nlong:\n  enter: !gt { lhs: !sma { period: !param P }, rhs: !value 0 }\n"

    ta.check_spec(UNBOUND)  # a check that passed, holes and all
    with pytest.raises(ta.SpecError, match="`P` is not set"):
        ta.load_spec(unset, params={})

    with pytest.raises(ta.SpecError):  # ...and one that raised
        ta.check_spec("root: BTC\nlong:\n  enter: !nope {}\n")
    with pytest.raises(ta.SpecError, match="`P` is not set"):
        ta.load_spec(unset, params={})

    # ...and one that went all the way through the build, which the guard also
    # spans (a deferred template body is re-parsed there).
    ta.check_spec(
        "universe: !any_of [BTC, ETH]\n"
        "selection: !top_bottom { longs: 1, shorts: 0 }\n"
        "score: !sma { period: !param LOOK }\nsizing: !value 1.0\n"
    )
    with pytest.raises(ta.SpecError, match="`P` is not set"):
        ta.load_spec(unset, params={})


def test_check_spec_types_only_the_placeholders_it_could_not_resolve():
    """What `param_types` covers, stated as a test because it bounds what a
    caller can build on it: a `!param` carrying a `default:` is *resolved*, not
    held, so it is not a hole and does not appear — declaration and all.

    That is the right set for the question the report answers ("what does the
    caller still owe, and of what type"), and it is exactly the set with no
    default to read a type off. A form that types *every* knob still reads the
    defaulted ones from their defaults."""
    doc = (
        "root: BTC\nlong:\n  enter: !gt\n"
        "    lhs: !sma { period: !param { key: FAST, default: 10, type: integer } }\n"
        "    rhs: !sma { period: !param SLOW }\n"
    )
    check = ta.check_spec(doc)
    assert check.param_types == {"SLOW": "number"}
    assert [h.name for h in check.holes] == ["SLOW"]


def test_the_two_spellings_of_the_default_root_are_not_the_same_document():
    """Omitting `root:` and spelling the 0.81 default out are different
    documents to a caller with no values, and the difference is `default:
    null`.

    The spliced default's placeholders are *optional*: they resolve to null, the
    `!pick` collapses to the sole-atom selector, and `load_spec` has always
    taken that document with nothing bound — so a form built on `param_types`
    is not offered a symbol box for it. The bare canonical spelling is
    *required*, which `load_spec` refuses and `check_spec` reports as the two
    typed holes it is."""
    omitted = "long:\n  enter: !value true\n"
    spelled = (
        "root: !pick { symbol: !param SYMBOL, freq: !param FREQ }\n"
        "long:\n  enter: !value true\n"
    )

    assert ta.load_spec(omitted, params={}, kind="single").kind == "single"
    assert ta.check_spec(omitted, kind="single").param_types == {}

    with pytest.raises(ta.SpecError, match="`FREQ` is not set"):
        ta.load_spec(spelled, params={}, kind="single")
    assert ta.check_spec(spelled).param_types == {
        "SYMBOL": "symbol",
        "FREQ": "frequency",
    }

    # And a `root:`-less document is only single-asset if the caller says so —
    # structurally it is a `multi:` one, and `kind="auto"` reads it that way in
    # both surfaces rather than one of them guessing differently.
    assert ta.check_spec(omitted).kind == "multi"
    assert ta.load_spec(omitted, params={}).kind == "multi"


def test_check_spec_built_is_a_claim_about_the_document_not_its_values():
    """`built=True` is not "this will load once you supply values". The build
    ran with a typed zero standing in each hole, so it says the document is
    *constructible as written* — a supplied value is validated when it is
    supplied, and can still be refused."""
    doc = "root: BTC\nlong:\n  enter: !gt { lhs: !sma { period: !param P }, rhs: !value 0 }\n"
    check = ta.check_spec(doc)
    assert check.built and check.param_types == {"P": "number"}

    # `number` is the coarse demand — a period is a `NonZeroUsize`, and each of
    # these is a number the document will not take.
    for bad in (0, -3, 2.5):
        with pytest.raises(ta.SpecError):
            ta.load_spec(doc, params={"P": bad})
    assert ta.load_spec(doc, params={"P": 20}).kind == "single"


def test_check_spec_binds_the_values_it_is_given():
    """`params=` is not all-or-nothing: a bound placeholder is substituted and
    type-checked exactly as `load_spec` does it, and only the rest stay holes."""
    assert ta.check_spec(UNBOUND, params={"FAST": 3, "SLOW": 8}).param_types == {
        "FREQ": "frequency",
        "SYMBOL": "symbol",
    }
    with pytest.raises(ta.SpecError):
        ta.check_spec(UNBOUND, params={"FAST": "not a period"})


def test_check_spec_reports_the_series_a_document_reads():
    """Same walk, and same meaning, as `StrategySpec.reads`: with no data in
    hand a check cannot say these are present, only that they are required."""
    check = ta.check_spec(
        "root: BTC\nlong:\n  enter: !gt\n"
        "    lhs: !close { source: !pick { symbol: ETH } }\n    rhs: !value 0\n"
    )
    assert check.reads == ["ETH"]


def test_check_spec_refuses_imports_when_told_to():
    """`base_dir` / `imports` / `import_root` mean what they mean on
    `load_spec`, including `imports=False` removing filesystem access outright
    rather than merely narrowing it."""
    with pytest.raises(ta.SpecError, match="!import is disabled"):
        ta.check_spec("root: BTC\nlong:\n  enter: !import other.yml\n", imports=False)


def test_check_spec_types_a_symbol_slot_as_a_symbol_and_a_freq_as_a_frequency():
    """The two placeholders of the default `root:` sit side by side in `!pick`,
    are both `String` slots, and want opposite things from a caller. Typing them
    both `string` was the answer that made downstream guess from the parameter's
    *name*.

    They are refined types now — `SymbolName` / `FreqToken` — so the demand
    comes off the parse with no declaration anywhere. `stream:` stays `string`
    because it genuinely promises nothing, which is the same format contract the
    two spellings already had."""
    check = ta.check_spec(
        "root: !pick { symbol: !param SYMBOL, freq: !param FREQ }\n"
        "long: { enter: !value true }\n"
    )
    assert check.param_types == {"SYMBOL": "symbol", "FREQ": "frequency"}

    opaque = ta.check_spec(
        "root: BTC\nlong:\n  enter: !gt\n"
        "    lhs: !close { source: !pick { symbol: !param S, stream: !param ST } }\n"
        "    rhs: !value 0\n"
    )
    assert opaque.param_types == {"S": "symbol", "ST": "string"}


def test_a_refinement_agrees_with_what_it_refines_but_not_with_its_sibling():
    """`symbol` and `frequency` refine `string`, so neither contradicts it — one
    value satisfies both, and the finer one is what to ask a caller for. Two
    different refinements do contradict: no string is a ticker *and* a cadence.
    """
    both = ta.check_spec(
        "root: !pick { symbol: !param X }\nlong:\n  enter: !gt\n"
        "    lhs: !close { source: !pick { stream: !param X } }\n    rhs: !value 0\n"
    )
    (hole,) = both.holes
    assert hole.required_type == "symbol"
    assert hole.used == ["string", "symbol"], (
        "the coarse demand is recorded, not reported"
    )
    assert hole.demanded == ["symbol"]

    with pytest.raises(ta.SpecError, match="used as symbol and as frequency"):
        ta.check_spec(
            "root: !pick { symbol: !param X, freq: !param X }\nlong: { enter: !value true }\n"
        )


def test_a_coarser_type_declaration_still_fits_a_refined_slot():
    """`type: string` on a `!pick { symbol: }` has been the documented way to
    keep a numeric ticker a string since before the refined types existed. It
    must not have become a contradiction."""
    check = ta.check_spec(
        "root: !pick { symbol: !param { key: X, type: string } }\nlong: { enter: !value true }\n"
    )
    (hole,) = check.holes
    assert hole.declared == "string" and hole.demanded == ["symbol"]

    with pytest.raises(
        ta.SpecError, match="declared `symbol` but used where a frequency"
    ):
        ta.check_spec(
            "root: !pick { freq: !param { key: X, type: symbol } }\nlong: { enter: !value true }\n"
        )


def test_the_implicit_root_coerces_a_numeric_ticker_and_checks_the_cadence():
    """The default `root:` declares its own placeholders' types, which buys
    coercion on the one path that could not ask for it: a document that never
    wrote a `root:` had no placeholder body to hang `type:` on."""
    doc = "long:\n  enter: !value true\n"
    spec = ta.load_spec(doc, params={"SYMBOL": 123}, kind="single")
    assert spec.kind == "single", "a numeric ticker is stringified, not rejected"

    with pytest.raises(ta.SpecError, match="`FREQ`.*is not a bar cadence"):
        ta.load_spec(doc, params={"SYMBOL": "BTC", "FREQ": "1hh"}, kind="single")

    # And unset still resolves to the sole-series root rather than being coerced.
    assert ta.load_spec(doc, kind="single").kind == "single"


def test_spec_grammar_reports_the_refined_string_types():
    """The same types reach a caller that builds a form over the grammar rather
    than over a checked document — the field's Rust type is the single source,
    so the two surfaces cannot disagree about what a slot wants."""
    grammar = ta.spec_grammar()
    found = []

    def walk(node):
        if isinstance(node, dict):
            if node.get("name") == "pick" and "forms" in node:
                found.append(node)
            else:
                for value in node.values():
                    walk(value)
        elif isinstance(node, list):
            for value in node:
                walk(value)

    walk(grammar)
    (pick,) = found
    types = {f["name"]: f["type"] for f in pick["forms"][0]["fields"]}
    assert types["symbol"] == "symbol"
    assert types["freq"] == "frequency"
    assert types["stream"] == "str", "an opaque id promises nothing and says so"


def test_a_declared_universe_is_a_list_of_symbols():
    """`!all_of [BTC, ETH]` is a list of *symbols*, and the three published
    surfaces say so from one source — the field is a `Vec<SymbolName>`, so the
    grammar payload, the JSON schema's item format, and a checked document
    cannot disagree about it."""
    grammar = ta.spec_grammar()
    tags = []

    def walk(node):
        if isinstance(node, dict):
            if "forms" in node:
                tags.append(node)
            else:
                for value in node.values():
                    walk(value)
        elif isinstance(node, list):
            for value in node:
                walk(value)

    walk(grammar)
    for name in ("all_of", "any_of"):
        (tag,) = [t for t in tags if t.get("name") == name]
        assert tag["forms"][0]["payload"] == "symbol_list", name

    universe = ta.spec_document_json_schema()["$defs"]["universe"]
    items = universe["oneOf"][0]["properties"]["all_of"]["items"]
    assert items == {"type": "string", "format": "symbol"}

    check = ta.check_spec(
        "universe: !all_of [BTC, ETH]\n"
        "selection: !top_bottom { longs: 1, shorts: 0 }\n"
        "score: !sma { period: 20 }\nsizing: !value 1.0\n"
    )
    assert check.kind == "basket" and check.built


def test_a_refined_hole_answers_with_a_value_its_own_format_accepts():
    """A hole stands in for a value, and the stand-in has to satisfy the slot it
    stands in — the same rule that makes an integer hole answer `1` rather than
    `0` so a `NonZeroUsize` period parses.

    Regression: a cadence hole answered the generic `""`, which reached the
    build as `invalid frequency ""`. A document parameterising its cadence — the
    other half of the default `root:` — could not be checked at all."""
    for name, doc in [
        (
            "cadence",
            "root: BTC\nlong:\n  enter: !gt\n"
            "    lhs: !close { source: !pick { freq: !param F } }\n    rhs: !value 0\n",
        ),
        (
            "symbol",
            "root: BTC\nlong:\n  enter: !gt\n"
            "    lhs: !close { source: !pick { symbol: !param S } }\n    rhs: !value 0\n",
        ),
    ]:
        check = ta.check_spec(doc)
        assert check.built, f"{name}: nothing left it undetermined, so it must build"


def test_an_empty_symbol_is_refused_but_an_unset_one_is_not():
    """An empty symbol matches no bar, so the leaf reads `None` for the whole run
    and the backtest reports a plausible zero-fill rather than failing.

    Refused at *build*, and the second half is why: a checked document's unset
    symbol is a hole standing in for a value, so refusing the empty string at
    parse would refuse exactly the documents `check_spec` exists for."""
    with pytest.raises(ta.SpecError, match="`symbol` is empty"):
        ta.check_spec(
            "root: BTC\nlong:\n  enter: !gt\n"
            '    lhs: !close { source: !pick { symbol: "" } }\n    rhs: !value 0\n'
        )

    assert ta.check_spec(
        "root: BTC\nlong:\n  enter: !gt\n"
        "    lhs: !close { source: !pick { symbol: !param S } }\n    rhs: !value 0\n"
    ).built
