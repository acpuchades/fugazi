"""Smoke tests for the fugazi Python bindings."""

import math

import pytest

import fugazi as ta


def feed(node, bars):
    """Feed a list of Candles, returning the list of outputs."""
    return [node.update(c) for c in bars]


def closes(values):
    """Build candles from a list of close prices (flat OHLC, unit volume)."""
    return [ta.Candle(v, v, v, v, 1.0) for v in values]


def test_candle_fields():
    c = ta.Candle(1.0, 4.0, 0.5, 3.0, 1000.0)
    assert c.open == 1.0
    assert c.high == 4.0
    assert c.low == 0.5
    assert c.close == 3.0
    assert c.volume == 1000.0
    assert c.typical() == pytest.approx((4.0 + 0.5 + 3.0) / 3.0)
    assert c.median() == pytest.approx((4.0 + 0.5) / 2.0)


def test_sma_warms_up_then_averages():
    sma = ta.sma(ta.close(), 3)
    out = feed(sma, closes([1.0, 2.0, 3.0, 4.0]))
    assert out[0] is None
    assert out[1] is None
    assert out[2] == pytest.approx(2.0)  # mean(1,2,3)
    assert out[3] == pytest.approx(3.0)  # mean(2,3,4)
    assert sma.is_ready()
    assert sma.value() == pytest.approx(3.0)


def test_log_defaults_to_natural_and_accepts_base():
    # Default base: natural log.
    ln = ta.log(ta.close())
    out = feed(ln, closes([1.0, math.e, 10.0, 100.0]))
    assert out[0] == pytest.approx(0.0)
    assert out[1] == pytest.approx(1.0)
    assert out[2] == pytest.approx(math.log(10.0))
    assert out[3] == pytest.approx(math.log(100.0))

    # Explicit base via the fluent method.
    log10 = ta.close().log(10.0)
    out = feed(log10, closes([1.0, 10.0, 1000.0]))
    assert out == [pytest.approx(0.0), pytest.approx(1.0), pytest.approx(3.0)]

    # Non-positive inputs yield None (log undefined).
    ln2 = ta.log(ta.close())
    assert feed(ln2, closes([-1.0, 0.0, 1.0])) == [None, None, pytest.approx(0.0)]


def test_log_rejects_invalid_base():
    with pytest.raises(ValueError):
        ta.log(ta.close(), base=0.0)
    with pytest.raises(ValueError):
        ta.log(ta.close(), base=1.0)
    with pytest.raises(ValueError):
        ta.close().log(-2.0)


def test_exp_defaults_to_natural_and_inverts_log():
    # Default base: natural exponential.
    e = ta.exp(ta.close())
    out = feed(e, closes([0.0, 1.0, 2.0]))
    assert out[0] == pytest.approx(1.0)
    assert out[1] == pytest.approx(math.e)
    assert out[2] == pytest.approx(math.e**2)

    # Explicit base via the fluent method, and the round trip through `log`.
    exp2 = ta.close().exp(2.0)
    assert feed(exp2, closes([1.0, 3.0, 10.0])) == [
        pytest.approx(2.0),
        pytest.approx(8.0),
        pytest.approx(1024.0),
    ]
    round_trip = ta.exp(ta.log(ta.close()))
    assert feed(round_trip, closes([2.0, 42.0])) == [
        pytest.approx(2.0),
        pytest.approx(42.0),
    ]

    # A result too large to represent yields None, an underflow is a value.
    over = ta.exp(ta.close())
    assert feed(over, closes([1e6, -1000.0])) == [None, pytest.approx(0.0)]


def test_exp_rejects_invalid_base():
    with pytest.raises(ValueError):
        ta.exp(ta.close(), base=0.0)
    with pytest.raises(ValueError):
        ta.exp(ta.close(), base=1.0)
    with pytest.raises(ValueError):
        ta.close().exp(-2.0)


def test_composition_ema_of_sma():
    """Composition is construction: an EMA of an SMA of the close."""
    node = ta.ema(ta.sma(ta.close(), 3), 2)
    out = feed(node, closes([1.0, 2.0, 3.0, 4.0, 5.0]))
    # SMA-3 ready at index 2; EMA seeds there, then updates.
    assert out[1] is None
    assert out[2] is not None
    assert math.isfinite(out[-1])


def test_source_is_reusable_after_composition():
    """Passing a source into a constructor clones it; the source stays usable."""
    src = ta.close()
    a = ta.ema(src, 3)
    b = ta.sma(src, 3)
    bars = closes([1.0, 2.0, 3.0, 4.0])
    feed(a, bars)
    feed(b, bars)
    assert a.value() is not None
    assert b.value() == pytest.approx(3.0)


def test_rsi_above_signal():
    sig = ta.rsi(ta.close(), 2).above(70.0)
    fired = any(feed(sig, closes([10.0, 11.0, 12.0, 13.0, 14.0])))
    assert fired
    assert isinstance(sig.is_true(), bool)


def test_crosses_above_fires_once():
    sig = ta.close().crosses_above(ta.value(2.0))
    states = feed(sig, closes([1.0, 1.5, 2.5, 3.0]))
    assert states == [False, False, True, False]


def test_signal_combination_operators():
    overbought = ta.rsi(ta.close(), 2).above(70.0)
    rising = ta.close().crosses_above(ta.value(13.5))
    combined = overbought.and_(rising)
    feed(combined, closes([10.0, 11.0, 12.0, 13.0, 14.0]))
    assert isinstance(combined.is_true(), bool)


def test_not_inverts_each_step():
    bars = closes([10.0, 11.0, 12.0, 13.0, 14.0])
    plain = ta.rsi(ta.close(), 2).above(70.0)
    inverted = ta.rsi(ta.close(), 2).above(70.0).not_()
    for plain_state, inv_state in zip(feed(plain, bars), feed(inverted, bars)):
        assert inv_state == (not plain_state)
    # operator form builds the same thing
    assert isinstance((~ta.rsi(ta.close(), 2).above(70.0)), ta.Signal)


def test_arithmetic_operators():
    spread = ta.high().sub(ta.low())
    out = feed(spread, [ta.Candle(1, 5, 2, 3, 1) for _ in range(2)])
    assert out[-1] == pytest.approx(3.0)
    # numbers are lifted to constants, and dunders work
    plus = ta.close() + 10.0
    assert feed(plus, closes([5.0]))[0] == pytest.approx(15.0)


def test_comparison_operators_build_signals():
    """`>` `<` `>=` `<=` are the operator spelling of `gt`/`lt`/`ge`/`le`, so
    they read like the arithmetic dunders right beside them."""
    bars = closes([1.0, 2.0, 3.0, 4.0])
    fast, slow = ta.close(), ta.sma(ta.close(), 2)
    assert isinstance(fast > slow, ta.Signal)
    assert feed(fast > slow, bars) == feed(fast.gt(slow), bars)
    assert feed(fast <= slow, bars) == feed(fast.le(slow), bars)
    # A bare number lifts on either side; Python reflects the ordering itself,
    # so `2.0 < ind` resolves to `ind.__gt__(2.0)` with no `__r*__` twin.
    assert feed(ta.close() > 2.0, bars) == [False, False, True, True]
    assert feed(2.0 < ta.close(), bars) == [False, False, True, True]


def test_comparison_operators_do_not_cost_hashability():
    """Regression: filling `tp_richcompare` makes CPython null `tp_hash` unless
    the type declares one, so adding `>` would otherwise have silently made
    every Indicator unusable as a dict key or set member."""
    a, b = ta.close(), ta.ema(ta.close(), 3)
    assert hash(a) == hash(a)
    assert len({a, b, a}) == 2
    assert {a: "x"}[a] == "x"


def test_equality_stays_identity_not_elementwise():
    """`==` is deliberately *not* overloaded — a Signal from it would be truthy
    and unhashable. The elementwise form is `.eq()`, which also takes epsilon."""
    a = ta.close()
    assert (a == a) is True
    assert (a == ta.close()) is False  # separately built, not the same object
    assert isinstance(a.eq(ta.close()), ta.Signal)
    assert feed(a.eq(3.0, epsilon=0.5), closes([1.0, 3.0, 9.0])) == [
        False,
        True,
        False,
    ]


def test_macd_returns_named_dict():
    node = ta.macd(ta.close(), 2, 4, 2)
    out = feed(node, closes([1.0, 2.0, 3.0, 4.0, 5.0, 6.0]))
    last = out[-1]
    assert set(last.keys()) == {"macd", "signal", "histogram"}
    assert last["histogram"] == pytest.approx(last["macd"] - last["signal"])


def test_bollinger_bands_ordered():
    node = ta.bollinger(ta.close(), 3, 2.0)
    out = feed(node, closes([1.0, 2.0, 3.0, 4.0]))
    band = out[-1]
    assert band["lower"] <= band["middle"] <= band["upper"]


def test_shared_multi_projects_named_components():
    """`.shared()` returns a handle whose per-line accessors project the
    underlying multi as ordinary Real-output indicators."""
    macd = ta.macd(ta.close(), 2, 4, 2).shared()
    assert set(macd.names()) == {"macd", "signal", "histogram"}
    line = macd.line()
    signal = macd.signal()
    histogram = macd.histogram()
    # Composable — same operators every other Real source supports.
    _cross = line.crosses_above(signal)
    # Value equivalence against a bare MultiIndicator on the same input.
    reference = ta.macd(ta.close(), 2, 4, 2)
    bars = closes([1.0, 2.0, 3.0, 4.0, 5.0, 6.0])
    for c in bars:
        got_line = line.update(c)
        got_signal = signal.update(c)
        got_hist = histogram.update(c)
        ref = reference.update(c)
        if ref is None:
            assert got_line is None
            continue
        assert got_line == pytest.approx(ref["macd"])
        assert got_signal == pytest.approx(ref["signal"])
        assert got_hist == pytest.approx(ref["histogram"])


def test_shared_multi_advances_source_once_per_bar():
    """The whole point of `.shared()`: three accessors that project the same
    underlying MACD produce the *same* output as one that fed a single
    reference. A bare-clone pattern would drift because each accessor would
    independently advance its own MACD copy — the shared handle prevents
    that."""
    macd = ta.macd(ta.close(), 2, 4, 2).shared()
    line, signal, histogram = macd.line(), macd.signal(), macd.histogram()

    reference = ta.macd(ta.close(), 2, 4, 2)
    for c in closes([1.0, 2.0, 3.0, 4.0, 5.0, 6.0, 7.0]):
        # Deliberately update in a nonobvious order to catch any hidden
        # coupling between the first-updated accessor and the source advance.
        got_signal = signal.update(c)
        got_hist = histogram.update(c)
        got_line = line.update(c)
        ref = reference.update(c)
        if ref is None:
            assert got_line is got_signal is got_hist is None
        else:
            assert got_line == pytest.approx(ref["macd"])
            assert got_signal == pytest.approx(ref["signal"])
            assert got_hist == pytest.approx(ref["histogram"])


def test_shared_bollinger_bands_project_correctly():
    bands = ta.bollinger(ta.close(), 3, 2.0).shared()
    upper, middle, lower = bands.upper(), bands.middle(), bands.lower()
    reference = ta.bollinger(ta.close(), 3, 2.0)
    for c in closes([1.0, 2.0, 3.0, 4.0, 5.0]):
        u, m, l = upper.update(c), middle.update(c), lower.update(c)
        ref = reference.update(c)
        if ref is None:
            assert u is None
        else:
            assert u == pytest.approx(ref["upper"])
            assert m == pytest.approx(ref["middle"])
            assert l == pytest.approx(ref["lower"])


def test_shared_unknown_component_errors():
    macd = ta.macd(ta.close(), 2, 4, 2).shared()
    with pytest.raises(ValueError):
        macd.component("nonexistent_field")
    with pytest.raises(ValueError):
        macd.upper()  # not a MACD field


def test_bar_indicator_atr():
    atr = ta.atr(2)
    bars = [
        ta.Candle(10, 11, 9, 10, 1),
        ta.Candle(10, 12, 8, 11, 1),
        ta.Candle(11, 13, 10, 12, 1),
    ]
    out = feed(atr, bars)
    assert out[-1] is not None and out[-1] > 0


def test_range_volatility_estimators():
    bars = [
        ta.Candle(10, 11, 9, 10, 1),
        ta.Candle(10, 12, 8, 11, 1),
        ta.Candle(11, 13, 10, 12, 1),
    ]
    for ctor in (ta.parkinson, ta.garman_klass, ta.rogers_satchell):
        out = feed(ctor(2), bars)
        assert out[-1] is not None and out[-1] > 0, ctor.__name__

    # Flat OHLC bars → zero range → zero volatility.
    flat = [ta.Candle(10, 10, 10, 10, 1)] * 3
    for ctor in (ta.parkinson, ta.garman_klass, ta.rogers_satchell):
        out = feed(ctor(2), flat)
        assert out[-1] == 0.0, ctor.__name__


def test_reset_clears_state():
    sma = ta.sma(ta.close(), 2)
    feed(sma, closes([1.0, 2.0]))
    assert sma.is_ready()
    sma.reset()
    assert not sma.is_ready()
    assert sma.value() is None


def test_feed_plain_input_returns_numpy_with_nan_warmup():
    np = pytest.importorskip("numpy")
    out = ta.sma(ta.identity(), 3).feed([1.0, 2.0, 3.0, 4.0, 5.0])
    assert isinstance(out, np.ndarray)
    assert np.isnan(out[0]) and np.isnan(out[1])
    assert out[2] == pytest.approx(2.0) and out[4] == pytest.approx(4.0)


def test_feed_matches_streaming_on_ready_bars():
    np = pytest.importorskip("numpy")
    prices = [1.0, 2.0, 3.0, 4.0, 5.0]
    streamed = feed(ta.sma(ta.close(), 3), closes(prices))
    oneshot = ta.sma(ta.identity(), 3).feed(prices)
    for s, o in zip(streamed, oneshot):
        assert (s is None and np.isnan(o)) or s == pytest.approx(o)


def test_identity_streams_raw_floats():
    """An identity-rooted node consumes a bare float stream, not candles."""
    sma = ta.sma(ta.identity(), 3)
    out = [sma.update(x) for x in [1.0, 2.0, 3.0, 4.0]]
    assert out[1] is None
    assert out[2] == pytest.approx(2.0) and out[3] == pytest.approx(3.0)


# ---------------------------------------------------------------------------
# Root fusing
#
# A leaf built by `ta.close()` / `ta.identity()` is carried both erased (in
# `src`) and concrete (in `root`), so a wrapping constructor can rebuild itself
# over the concrete leaf and save an erasure level. The whole optimisation is
# sound only if the two describe the same leaf, so what needs pinning is that
# fusing is *invisible*: same field, same numbers, same readiness.
# ---------------------------------------------------------------------------

# Every field distinct on every bar, so a fused root that reads the wrong one
# gives a wrong number instead of an accidental match.
FUSABLE_BARS = [
    ta.Candle(10.0, 40.0, 5.0, 30.0, 1000.0),
    ta.Candle(11.0, 41.0, 6.0, 31.0, 1100.0),
    ta.Candle(12.0, 42.0, 7.0, 32.0, 1200.0),
]

BAR_FIELDS = {
    "open": lambda c: c.open,
    "high": lambda c: c.high,
    "low": lambda c: c.low,
    "close": lambda c: c.close,
    "volume": lambda c: c.volume,
    "typical": lambda c: c.typical(),
    "median": lambda c: c.median(),
}


@pytest.mark.parametrize("field", list(BAR_FIELDS))
def test_a_fused_bar_root_reads_the_field_it_names(field):
    """Fusing dispatches on a field *tag*, one arm per field — so the arms are
    exactly the kind of table that can be miswired. Every field is checked
    through both a free-function constructor and a method."""
    values = [BAR_FIELDS[field](c) for c in FUSABLE_BARS]
    leaf = getattr(ta, field)

    averaged = feed(ta.sma(leaf(), 2), FUSABLE_BARS)
    assert averaged[2] == pytest.approx((values[1] + values[2]) / 2.0)

    lagged = feed(leaf().lag(1), FUSABLE_BARS)
    assert lagged[2] == pytest.approx(values[1])


@pytest.mark.parametrize("field", list(BAR_FIELDS))
def test_fusing_a_bar_root_changes_nothing_a_caller_can_see(field):
    """The unfused path, for the same numbers.

    `+ 0.0` is exact and its node is not a fusable root, so the same
    constructor takes its `None` arm — the pre-fusing behaviour.
    """
    leaf = getattr(ta, field)
    for build in (lambda s: ta.sma(s, 2), lambda s: s.lag(1), lambda s: s.roc(1)):
        fused = build(leaf())
        unfused = build(leaf() + 0.0)
        assert feed(fused, FUSABLE_BARS) == feed(unfused, FUSABLE_BARS), field
        assert fused.warm_up_bars() == unfused.warm_up_bars(), field
        assert fused.stable_bars() == unfused.stable_bars(), field


def test_fusing_an_identity_root_changes_nothing_a_caller_can_see():
    """`ta.identity()` is the other fusable root, and the only `Real`-domain
    one — it feeds raw floats rather than candles."""
    values = [1.0, 2.0, 3.0, 4.0]
    fused = ta.sma(ta.identity(), 2)
    unfused = ta.sma(ta.identity() + 0.0, 2)
    assert [fused.update(x) for x in values] == [unfused.update(x) for x in values]
    assert fused.warm_up_bars() == unfused.warm_up_bars()


def test_a_fused_root_is_still_reusable_as_a_source():
    """Fusing rebuilds over a *clone* of the root, so the leaf a caller holds
    is untouched and can go into a second chain."""
    leaf = ta.close()
    fast = ta.sma(leaf, 2)
    slow = ta.sma(leaf, 3)
    assert feed(fast, FUSABLE_BARS)[2] == pytest.approx(31.5)
    assert feed(slow, FUSABLE_BARS)[2] == pytest.approx(31.0)
    # And the leaf itself still reads closes.
    assert feed(leaf, FUSABLE_BARS) == [30.0, 31.0, 32.0]


def test_candle_rooted_feed_rejects_bare_series():
    """The old leniency is gone: a candle indicator needs a frame, not an array."""
    with pytest.raises(TypeError):
        ta.sma(ta.close(), 3).feed([1.0, 2.0, 3.0])


def test_identity_rooted_feed_rejects_frame():
    with pytest.raises(TypeError):
        ta.sma(ta.identity(), 3).feed({"close": [1.0, 2.0, 3.0]})


def test_mixing_domains_raises():
    """A candle-rooted and a value-rooted source cannot be combined."""
    with pytest.raises(TypeError):
        ta.close().add(ta.identity())
    with pytest.raises(TypeError):
        ta.close().crosses_above(ta.identity())


def test_feed_dict_of_columns_is_numpy():
    np = pytest.importorskip("numpy")
    out = ta.atr(2).feed(
        {"high": [11, 12, 13], "low": [9, 8, 10], "close": [10, 11, 12]}
    )
    assert isinstance(out, np.ndarray)
    assert np.isnan(out[0]) and out[-1] > 0


def test_feed_signal_returns_numpy_bools():
    np = pytest.importorskip("numpy")
    states = ta.identity().crosses_above(2.0).feed([1.0, 1.5, 2.5, 3.0])
    assert isinstance(states, np.ndarray) and states.dtype == bool
    assert states.tolist() == [False, False, True, False]


def test_feed_multi_plain_input_is_dict_of_arrays():
    np = pytest.importorskip("numpy")
    out = ta.macd(ta.identity(), 2, 4, 2).feed([1.0, 2.0, 3.0, 4.0, 5.0])
    assert set(out.keys()) == {"macd", "signal", "histogram"}
    assert all(isinstance(col, np.ndarray) for col in out.values())
    assert out["histogram"][-1] == pytest.approx(out["macd"][-1] - out["signal"][-1])


def test_feed_continues_from_state_unless_reset():
    np = pytest.importorskip("numpy")
    node = ta.sma(ta.identity(), 2)
    node.feed([1.0, 2.0])
    # without reset, feed continues from warmed-up state
    assert node.feed([3.0])[0] == pytest.approx(2.5)
    node.reset()
    assert np.isnan(node.feed([3.0])[0])


def test_feed_chunks_chain_like_one_continuous_stream():
    """Consecutive feed() calls continue the same stream: chunked == one-shot."""
    np = pytest.importorskip("numpy")
    s1, s2 = [1.0, 2.0, 3.0], [4.0, 5.0, 6.0]
    node = ta.sma(ta.identity(), 3)
    chunked = np.concatenate([node.feed(s1), node.feed(s2)])
    oneshot = ta.sma(ta.identity(), 3).feed(s1 + s2)
    assert np.allclose(chunked, oneshot, equal_nan=True)


def test_feed_missing_close_column_raises():
    with pytest.raises(ValueError):
        ta.sma(ta.close(), 2).feed({"high": [1, 2, 3]})


def test_feed_mismatched_column_lengths_raises():
    with pytest.raises(ValueError):
        ta.atr(2).feed({"close": [1, 2, 3], "high": [1, 2]})


def test_feed_numpy_array_in_numpy_out():
    np = pytest.importorskip("numpy")
    out = ta.sma(ta.identity(), 2).feed(np.array([1.0, 2.0, 3.0]))
    assert isinstance(out, np.ndarray)
    assert out[-1] == pytest.approx(2.5)


def test_feed_pandas_returns_series_with_index():
    pd = pytest.importorskip("pandas")
    df = pd.DataFrame(
        {"high": [11, 12, 13], "low": [9, 8, 10], "close": [10, 11, 12]},
        index=pd.RangeIndex(100, 103),
    )
    out = ta.atr(2).feed(df)
    assert isinstance(out, pd.Series)
    assert list(out.index) == [100, 101, 102]  # index preserved
    assert out.iloc[-1] > 0
    # a bare Series works for an identity-rooted indicator, index preserved
    s_out = ta.sma(ta.identity(), 2).feed(pd.Series([1.0, 2.0, 3.0]))
    assert isinstance(s_out, pd.Series) and s_out.iloc[-1] == pytest.approx(2.5)


def test_feed_pandas_multi_returns_dataframe():
    pd = pytest.importorskip("pandas")
    df = pd.DataFrame({"close": [1.0, 2.0, 3.0, 4.0, 5.0]}, index=pd.RangeIndex(5, 10))
    out = ta.bollinger(ta.close(), 3).feed(df)
    assert isinstance(out, pd.DataFrame)
    assert list(out.columns) == ["upper", "middle", "lower"]
    assert list(out.index) == [5, 6, 7, 8, 9]


def test_feed_polars_returns_series_and_dataframe():
    pl = pytest.importorskip("polars")
    df = pl.DataFrame({"high": [11, 12, 13], "low": [9, 8, 10], "close": [10, 11, 12]})
    out = ta.atr(2).feed(df)
    assert isinstance(out, pl.Series) and out[-1] > 0
    multi = ta.bollinger(ta.close(), 2).feed(df)
    assert isinstance(multi, pl.DataFrame)
    assert multi.columns == ["upper", "middle", "lower"]


def test_feed_dataframe_capitalized_columns():
    pd = pytest.importorskip("pandas")
    df = pd.DataFrame({"High": [11, 12, 13], "Low": [9, 8, 10], "Close": [10, 11, 12]})
    out = ta.atr(2).feed(df)
    assert isinstance(out, pd.Series) and out.iloc[-1] > 0


def test_zero_period_raises():
    with pytest.raises(ValueError):
        ta.sma(ta.close(), 0)


def test_bad_operand_type_raises():
    with pytest.raises(TypeError):
        ta.close().add("not a number")


# ---------------------------------------------------------------------------
# Type checking enforced at the Python boundary
#
# A node is rooted either in the candle domain (consumes Candles) or the value
# domain (identity(), consumes floats). update()/feed() require the matching
# input, operators refuse to cross domains, and a constant (value()/number) is
# neutral and adopts its partner's domain.
# ---------------------------------------------------------------------------

ONE_CANDLE = ta.Candle(1.0, 2.0, 0.5, 1.5, 100.0)


def test_update_candle_rooted_rejects_non_candle():
    """A candle-rooted node's update() wants a Candle, not a float/frame/str."""
    node = ta.sma(ta.close(), 2)
    for bad in (1.0, "x", {"close": [1.0]}, [1.0, 2.0]):
        with pytest.raises(TypeError):
            node.update(bad)
    assert node.update(ONE_CANDLE) is None  # a real Candle is accepted


def test_update_identity_rooted_rejects_non_number():
    """An identity-rooted node's update() wants a float, not a Candle/str."""
    node = ta.sma(ta.identity(), 2)
    for bad in (ONE_CANDLE, "x"):
        with pytest.raises(TypeError):
            node.update(bad)
    assert node.update(1.0) is None  # a real float is accepted


def test_update_multi_enforces_domain():
    candle_macd = ta.macd(ta.close(), 2, 4, 2)
    with pytest.raises(TypeError):
        candle_macd.update(1.0)
    value_macd = ta.macd(ta.identity(), 2, 4, 2)
    with pytest.raises(TypeError):
        value_macd.update(ONE_CANDLE)


def test_update_signal_enforces_domain():
    candle_sig = ta.close().above(1.0)
    with pytest.raises(TypeError):
        candle_sig.update(1.0)
    value_sig = ta.identity().above(1.0)
    with pytest.raises(TypeError):
        value_sig.update(ONE_CANDLE)


def test_feed_signal_enforces_domain():
    np = pytest.importorskip("numpy")
    candle_sig = ta.close().above(1.0)
    with pytest.raises(TypeError):
        candle_sig.feed([1.0, 2.0, 3.0])  # candle signal needs a frame
    value_sig = ta.identity().above(1.0)
    with pytest.raises(TypeError):
        value_sig.feed({"close": [1.0, 2.0]})  # value signal needs a 1-D series
    # the matching shapes work
    assert isinstance(value_sig.feed([1.0, 2.0, 3.0]), np.ndarray)


@pytest.mark.parametrize(
    "op",
    [
        "add",
        "sub",
        "mul",
        "div",
        "gt",
        "lt",
        "ge",
        "le",
        "crosses_above",
        "crosses_below",
    ],
)
def test_operators_refuse_to_cross_domains(op):
    candle, value = ta.close(), ta.identity()
    with pytest.raises(TypeError):
        getattr(candle, op)(value)
    with pytest.raises(TypeError):
        getattr(value, op)(candle)


def test_signal_combinators_refuse_to_cross_domains():
    candle_sig = ta.close().above(1.0)
    value_sig = ta.identity().above(1.0)
    for combine in ("and_", "or_", "xor_"):
        with pytest.raises(TypeError):
            getattr(candle_sig, combine)(value_sig)


# ---------------------------------------------------------------------------
# Bar-only vs side-channel sources must stay combinable
#
# These pin behaviour that a planned optimisation could silently break, so they
# are written to fail loudly rather than to describe the present implementation.
#
# Every candle-rooted source is currently fed a whole `Atom` — the bar plus its
# side channels (`time`, `overlays`) — even though only the overlay readers
# (`get*`) and the calendar leaves need those. The plan (docs/PERFORMANCE.md, P1)
# is to carry the bar alone where that is all a chain reads, which splits today's
# single candle domain in two.
#
# The hazard is *combination*. `close()` and `get_real(...)` are the same domain
# today, so pairing them just works; afterwards they are different domains and
# the bar-only side has to be lifted to the atom side rather than rejected.
# Reject it and previously-valid user code starts raising — which is why these
# assert on *values*, not merely that construction succeeds.
#
# That failure mode is not hypothetical: `test_operators_refuse_to_cross_domains`
# below shows the rejection path is live today for a genuine clash
# (`close()` against `identity()`). A split that forgot to lift would route these
# pairings down that same path, and these tests would raise `TypeError`.
# ---------------------------------------------------------------------------


def _overlay_frame():
    """Two bars carrying one Real overlay column, plus the schema for it."""
    b = ta.SchemaBuilder()
    b.add_real("adj")
    schema = b.finish()
    bars = [
        ta.Atom(ta.Candle(10.0, 10.0, 10.0, 10.0, 1.0), ta.OverlayInfo(schema, [2.0])),
        ta.Atom(ta.Candle(20.0, 20.0, 20.0, 20.0, 1.0), ta.OverlayInfo(schema, [5.0])),
    ]
    return schema, bars


@pytest.mark.parametrize("op,want", [("add", [12.0, 25.0]), ("sub", [8.0, 15.0])])
def test_bar_field_combines_with_overlay_column(op, want):
    """`close() <op> get_real(adj)` — a bar-only source against a side-channel one."""
    schema, bars = _overlay_frame()
    combined = getattr(ta.close(), op)(ta.get_real(schema, "adj"))
    got = [combined.update(bar) for bar in bars]
    assert got == pytest.approx(want)


def test_overlay_column_combines_with_bar_field_in_either_order():
    """Order must not matter: the lift has to work from both operand positions."""
    schema, bars = _overlay_frame()
    left = ta.get_real(schema, "adj").add(ta.close())
    right = ta.close().add(ta.get_real(schema, "adj"))
    assert [left.update(b) for b in bars] == pytest.approx(
        [right.update(b) for b in bars]
    )


def test_bar_field_compares_against_overlay_column():
    """The signal side of the same pairing, which takes a different code path."""
    schema, bars = _overlay_frame()
    sig = ta.close().gt(ta.get_real(schema, "adj"))
    assert [sig.update(b) for b in bars] == [True, True]


def test_bar_field_combines_with_calendar_leaf():
    """Calendar leaves read `atom.time`, so they stay on the side-channel side.

    `close() + year()` is the tightest form of the hazard: the left operand needs
    only the bar, the right needs a field of the atom that a bar cannot carry.
    """
    ts = 1_710_506_096_000  # 2024-03-15T14:34:56Z
    bar = ta.Atom(ta.Candle(7.0, 7.0, 7.0, 7.0, 1.0), None, ts)
    assert ta.close().add(ta.year()).update(bar) == pytest.approx(2031.0)


@pytest.mark.parametrize(
    "leaf,want",
    [
        ("open", 10.0),
        ("high", 14.0),
        ("low", 8.0),
        ("close", 12.0),
        ("volume", 100.0),
        ("typical", (14.0 + 8.0 + 12.0) / 3.0),
        ("median", (14.0 + 8.0) / 2.0),
    ],
)
def test_bar_rooted_and_atom_rooted_fields_agree(leaf, want):
    """`ta.close()` and `ta.close(source=...)` are now *different* code paths.

    The `source=`-omitted form reads the bar directly (the cheap domain); an
    explicit `source=` goes through the core's atom-rooted `Field`. They must
    produce identical numbers, so this pins them against each other and against
    a hand-computed value — a divergence in one accessor would otherwise show up
    only as a wrong backtest.
    """
    candle = ta.Candle(10.0, 14.0, 8.0, 12.0, 100.0)
    bar = ta.Atom(candle)
    snap = ta.Snapshot({"X": bar})

    bar_rooted = getattr(ta, leaf)()
    atom_rooted = getattr(ta, leaf)(source=ta.pick("X"))

    assert bar_rooted.update(bar) == pytest.approx(want)
    assert atom_rooted.update(snap) == pytest.approx(want)


def test_value_is_domain_neutral():
    """A constant adopts its partner's domain on either side; never clashes."""
    # right operand, both domains
    assert isinstance(ta.rsi(ta.close(), 2).gt(ta.value(70.0)), ta.Signal)
    assert isinstance(ta.rsi(ta.identity(), 2).gt(ta.value(70.0)), ta.Signal)
    # left operand, both domains
    assert isinstance(ta.value(50.0).lt(ta.close()), ta.Signal)
    assert isinstance(ta.value(50.0).lt(ta.identity()), ta.Signal)
    # a bare number behaves identically to value()
    assert isinstance(ta.rsi(ta.identity(), 2).gt(70.0), ta.Signal)


def test_value_matches_number_streaming():
    """value(k) and the bare number k compute the same comparison."""
    bars = closes([10.0, 20.0, 30.0])
    with_value = feed(ta.close().gt(ta.value(15.0)), bars)
    with_number = feed(ta.close().gt(15.0), bars)
    assert with_value == with_number == [False, True, True]


def test_keltner_rejects_identity_source():
    """Keltner reads ATR internally, so its source must be candle-rooted."""
    with pytest.raises(TypeError):
        ta.keltner(ta.identity())


def test_donchian_rejects_mixed_domain_sources():
    with pytest.raises(TypeError):
        ta.donchian(ta.high(), ta.identity(), 3)


# --- distribution-shape + normalization indicators -------------------------


def test_skewness_warms_up_and_signs_the_tail():
    sk = ta.skewness(ta.close(), 3)
    out = feed(sk, closes([0.0, 0.0, 3.0]))
    assert out[0] is None and out[1] is None
    # Window {0, 0, 3}: m2=2, m3=2, skew = 2 / 2**1.5.
    assert out[2] == pytest.approx(2.0 / 2.0**1.5)
    # A constant window has no dispersion → 0.0, not NaN.
    assert feed(ta.skewness(ta.close(), 3), closes([5.0, 5.0, 5.0]))[2] == 0.0


def test_kurtosis_is_raw_not_excess():
    ku = ta.kurtosis(ta.close(), 3)
    out = feed(ku, closes([-1.0, 0.0, 1.0]))
    assert out[0] is None and out[1] is None
    # Window {-1, 0, 1}: m2=2/3, m4=2/3, kurtosis = (2/3)/(2/3)**2 = 1.5 (raw).
    assert out[2] == pytest.approx(1.5)
    # `fugazi.metrics.kurtosis` is the separate, excess metric over returns.
    assert ta.metrics.kurtosis([0.01, -0.02, 0.03, -0.01]) is not None


def test_zscore_measures_distance_from_windowed_mean():
    z = ta.zscore(ta.close(), 3)
    out = feed(z, closes([2.0, 4.0, 6.0]))
    assert out[0] is None and out[1] is None
    # Latest 6 vs. mean 4, population stddev sqrt(8/3).
    assert out[2] == pytest.approx(2.0 / math.sqrt(8.0 / 3.0))


def test_percentile_interpolates_like_numpy_default():
    med = ta.percentile(ta.close(), 4, 0.5)
    out = feed(med, closes([1.0, 2.0, 3.0, 4.0]))
    assert out[:3] == [None, None, None]
    # Sorted [1, 2, 3, 4] -> R type-7 / numpy median is 2.5.
    assert out[3] == pytest.approx(2.5)


def test_percentile_extremes_agree_with_rolling_max_and_min():
    bars = closes([4.0, 1.0, 9.0, 7.0, 2.0])
    lo = feed(ta.percentile(ta.close(), 5, 0.0), bars)[-1]
    hi = feed(ta.percentile(ta.close(), 5, 1.0), bars)[-1]
    assert lo == pytest.approx(1.0)
    assert hi == pytest.approx(9.0)


def test_percentile_rejects_an_out_of_range_pct():
    with pytest.raises(ValueError):
        ta.percentile(ta.close(), 4, 1.5)


def test_percentile_rank_counts_the_current_sample():
    rank = ta.percentile_rank(ta.close(), 4)
    out = feed(rank, closes([10.0, 20.0, 30.0, 25.0]))
    # 25 sits above 10 and 20, and counts itself: 3 of 4.
    assert out[3] == pytest.approx(0.75)
    # A fresh high is at-or-above everything in the window.
    assert rank.update(ta.Candle(99.0, 99.0, 99.0, 99.0, 1.0)) == pytest.approx(1.0)


def test_bars_since_is_none_until_the_signal_first_fires():
    bs = ta.bars_since(ta.close().gt(ta.value(10.0)))
    out = feed(bs, closes([1.0, 2.0, 42.0, 3.0, 4.0, 99.0]))
    assert out[0] is None and out[1] is None  # never fired yet
    assert out[2] == pytest.approx(0.0)  # fires
    assert out[3] == pytest.approx(1.0)
    assert out[4] == pytest.approx(2.0)
    assert out[5] == pytest.approx(0.0)  # fires again


def test_bars_since_threshold_reads_false_before_the_first_fire():
    # The safety property: a never-fired signal can't gate an entry in.
    fresh = ta.bars_since(ta.close().gt(ta.value(1e9))).lt(ta.value(5.0))
    feed(fresh, closes([1.0, 2.0, 3.0]))
    assert not fresh.is_true()


def test_bars_since_high_reports_the_argmax_offset():
    bs = ta.bars_since_high(ta.close(), 3)
    out = feed(bs, closes([5.0, 3.0, 1.0, 9.0, 2.0]))
    assert out[0] is None and out[1] is None
    assert out[2] == pytest.approx(2.0)  # the 5.0 was two bars back
    assert out[3] == pytest.approx(0.0)  # new high now
    assert out[4] == pytest.approx(1.0)
    assert feed(ta.bars_since_low(ta.close(), 3), closes([1.0, 3.0, 5.0]))[
        -1
    ] == pytest.approx(2.0)


def test_correlation_bounds_and_domain_check():
    # y = x fed to both legs → perfect positive correlation.
    c = ta.correlation(ta.close(), ta.close(), 3)
    out = feed(c, closes([1.0, 2.0, 3.0]))
    assert out[0] is None and out[1] is None
    assert out[2] == pytest.approx(1.0)
    # Two Real sources correlate one against its own lag (autocorrelation).
    ac = ta.correlation(ta.close(), ta.close().lag(1), 3)
    assert feed(ac, closes([1.0, 2.0, 3.0, 4.0]))[-1] == pytest.approx(1.0)
    # Mixing domains (candle-rooted vs. value-rooted) is a TypeError.
    with pytest.raises(TypeError):
        ta.correlation(ta.close(), ta.identity(), 3)


def test_pointwise_transforms():
    assert feed(ta.close().abs(), closes([-1.0, 2.0, -3.0])) == [1.0, 2.0, 3.0]
    assert feed(abs(ta.close()), closes([-4.0])) == [4.0]
    # Zero has no direction: `sign` answers 0 there, unlike `math.copysign`.
    assert feed(ta.close().sign(), closes([-2.0, 0.0, 5.0])) == [-1.0, 0.0, 1.0]
    # A negative input is outside sqrt's domain, so it reads `None`.
    assert feed(ta.close().sqrt(), closes([4.0, -1.0, 9.0])) == [2.0, None, 3.0]
    assert feed(ta.close().tanh(), closes([0.0, 1.0])) == pytest.approx(
        [0.0, math.tanh(1.0)]
    )
    assert feed(ta.close().sigmoid(), closes([0.0, 1.0])) == pytest.approx(
        [0.5, 1.0 / (1.0 + math.exp(-1.0))]
    )


def test_pow_min_max_and_clamp():
    assert feed(ta.close().pow(ta.value(2.0)), closes([2.0, 3.0])) == [4.0, 9.0]
    assert feed(ta.close() ** 2.0, closes([2.0, 3.0])) == [4.0, 9.0]
    assert feed(2.0 ** ta.close(), closes([3.0])) == [8.0]
    # `(-8) ** (1/3)` has no real answer, so it reads `None` rather than a NaN.
    assert feed(ta.close() ** (1.0 / 3.0), closes([-8.0])) == [None]
    # Python's three-argument pow() has no elementwise reading.
    with pytest.raises(ValueError):
        pow(ta.close(), 2.0, 3.0)
    # Pairwise, not windowed: `max` compares two sources on the same bar, where
    # `rolling_max` maximises one source over a window.
    assert feed(ta.close().max(2.0), closes([1.0, 5.0])) == [2.0, 5.0]
    assert feed(ta.close().min(2.0), closes([1.0, 5.0])) == [1.0, 2.0]
    assert feed(ta.close().clamp(0.0, 1.0), closes([-1.0, 0.5, 7.0])) == [0.0, 0.5, 1.0]
    # Inverted bounds collapse to `upper` — what the min-of-max form does.
    assert feed(ta.close().clamp(1.0, 0.0), closes([0.5])) == [0.0]


def test_running_accumulators():
    assert feed(ta.close().cum_sum(), closes([1.0, 2.0, 3.0])) == [1.0, 3.0, 6.0]
    assert feed(ta.close().cum_max(), closes([1.0, 3.0, 2.0])) == [1.0, 3.0, 3.0]
    assert feed(ta.close().cum_min(), closes([3.0, 1.0, 2.0])) == [3.0, 1.0, 1.0]
    # The drawdown of an arbitrary series, which `cum_max` is what makes possible.
    dd = ta.close() / ta.close().cum_max() - 1.0
    assert feed(dd, closes([10.0, 20.0, 15.0])) == pytest.approx([0.0, 0.0, -0.25])


def test_covariance_and_beta():
    # x against itself: the covariance of a series with itself is its variance,
    # population form — for {1,2,3}, ((-1)^2 + 0 + 1^2)/3 = 2/3.
    cov = feed(ta.covariance(ta.close(), ta.close(), 3), closes([1.0, 2.0, 3.0]))
    assert cov[0] is None and cov[1] is None
    assert cov[2] == pytest.approx(2.0 / 3.0)
    # `lhs = 2 * rhs`, so the slope explaining lhs by rhs is 2 ...
    b = ta.beta(ta.close() * 2.0, ta.close(), 3)
    assert feed(b, closes([1.0, 2.0, 3.0]))[2] == pytest.approx(2.0)
    # ... and 0.5 the other way round. Beta is directional, not symmetric.
    rb = ta.beta(ta.close(), ta.close() * 2.0, 3)
    assert feed(rb, closes([1.0, 2.0, 3.0]))[2] == pytest.approx(0.5)
    # A flat benchmark measures no sensitivity, rather than an infinity.
    flat = ta.beta(ta.close(), ta.close() * 0.0 + 7.0, 3)
    assert feed(flat, closes([1.0, 2.0, 3.0]))[2] == pytest.approx(0.0)


def test_linreg_fits_a_ramp():
    # y = 2x + 1 over bars 0,1,2: slope 2, the fit at the oldest bar is 1, at
    # the newest 5, and a straight line explains all of it.
    fit = ta.linreg(ta.close(), 3).shared()
    bars = closes([1.0, 3.0, 5.0])
    assert feed(fit.slope(), bars)[2] == pytest.approx(2.0)
    # One handle, one underlying fit — it has already consumed those bars, so
    # each further reading gets its own.
    assert feed(ta.linreg(ta.close(), 3).shared().intercept(), bars)[
        2
    ] == pytest.approx(1.0)
    assert feed(ta.linreg(ta.close(), 3).shared().value(), bars)[2] == pytest.approx(
        5.0
    )
    assert feed(ta.linreg(ta.close(), 3).shared().r2(), bars)[2] == pytest.approx(1.0)
    assert ta.linreg(ta.close(), 3).shared().names() == [
        "slope",
        "intercept",
        "value",
        "r2",
    ]
    # A single point has no slope; the guard is a ValueError, not a panic.
    with pytest.raises(ValueError):
        ta.linreg(ta.close(), 1)


def test_variance_ratio_classifies_regime():
    vr = ta.variance_ratio(ta.close(), 5, 2)
    # Prices {0,1,3,6,10}: accelerating returns → trending → VR = 32/15 > 1.
    out = feed(vr, closes([0.0, 1.0, 3.0, 6.0, 10.0]))
    assert out[3] is None
    assert out[4] == pytest.approx(32.0 / 15.0)
    # Prices {0,1,3,4,6}: constant 2-period returns → mean reversion → VR = 0.
    mr = feed(ta.variance_ratio(ta.close(), 5, 2), closes([0.0, 1.0, 3.0, 4.0, 6.0]))
    assert mr[4] == pytest.approx(0.0)
    # Constraints surface as ValueError, not a Rust panic.
    with pytest.raises(ValueError):
        ta.variance_ratio(ta.close(), 10, 1)
    with pytest.raises(ValueError):
        ta.variance_ratio(ta.close(), 3, 2)


# --- strategy layer: Wallet ------------------------------------------------


def test_wallet_set_position_is_absolute_and_books_funds():
    w = ta.PaperWallet(1_000.0)
    w.update("AAPL", 100.0)
    # A market order only queues -- nothing books yet, and it returns None.
    assert w.set_position("AAPL", 3.0) is None
    assert w.position("AAPL") == pytest.approx(0.0)
    # The next update fills it at that bar's open (100).
    w.update("AAPL", 100.0)
    assert w.position("AAPL") == pytest.approx(3.0)
    order = w.orders()[-1]
    assert order.symbol == "AAPL"
    assert order.side == "buy"
    assert order.units == pytest.approx(3.0)
    # Scale in to a new target, again filled on the next bar.
    w.set_position("AAPL", 5.0)
    w.update("AAPL", 100.0)
    assert w.position("AAPL") == pytest.approx(5.0)
    assert w.funds == pytest.approx(1_000.0 - 5.0 * 100.0)


def test_wallet_blotter_retention_is_bounded_with_an_opt_out():
    # The blotter is a reporting artifact, so it is bounded by default rather
    # than growing forever in a long-lived run.
    assert ta.PaperWallet(1_000.0).retention is not None

    def churn(w, n):
        # set_position takes a *target*, so the sign has to alternate for each
        # call to book a fill.
        w.update("AAPL", 100.0)
        for i in range(n):
            w.set_position("AAPL", 1.0 if i % 2 == 0 else -1.0)
            w.update("AAPL", 100.0)

    bounded = ta.PaperWallet(1_000_000.0)
    bounded.retention = 4
    churn(bounded, 40)
    assert len(bounded.orders()) <= 8, "blotter grew past the trim threshold"
    assert bounded.orders()[-1].units == pytest.approx(2.0), "newest fill must survive"

    # None is the named opt-out: keep the whole in-process history.
    unbounded = ta.PaperWallet(1_000_000.0)
    unbounded.retention = None
    assert unbounded.retention is None
    churn(unbounded, 40)
    assert len(unbounded.orders()) == 40

    # Tightening the limit trims on the spot.
    unbounded.retention = 0
    assert unbounded.orders() == []


def test_wallet_poll_fills_and_cancel():
    w = ta.PaperWallet(1_000.0)
    w.update("AAPL", 100.0)
    # A paper wallet never has out-of-band fills (the method exists for parity
    # with live wallets, which buffer async fills there).
    assert w.poll_fills() == []
    w.set_position("AAPL", 3.0)
    w.update("AAPL", 100.0)
    order = w.orders()[-1]
    # Every booked order exposes the id of its submission.
    assert isinstance(order.id, int)
    # Cancelling a known (already-filled) or unknown id is a safe no-op.
    w.cancel(order.id)
    w.cancel(9999)
    assert w.poll_fills() == []


def test_wallet_set_is_absolute_and_reverses():
    w = ta.PaperWallet(10_000.0)
    w.update("X", 50.0)
    w.set("X", "buy", 4.0)
    w.update("X", 50.0)  # fills the +4 at the open
    assert w.position("X") == pytest.approx(4.0)
    # Re-targeting the same side is idempotent: the queued fill is a no-op.
    n = len(w.orders())
    w.set("X", "buy", 4.0)
    w.update("X", 50.0)
    assert len(w.orders()) == n
    # Opposite side reverses: +4 -> -4 = sell 8.
    w.set("X", "sell", 4.0)
    w.update("X", 50.0)
    order = w.orders()[-1]
    assert order.side == "sell"
    assert order.units == pytest.approx(8.0)
    assert w.position("X") == pytest.approx(-4.0)


def test_wallet_limit_order_fills_at_its_price_or_better():
    """A resting limit is the entry counterpart to `set_stop`."""
    w = ta.PaperWallet(10_000.0)
    w.update("X", ta.Candle(100, 101, 99, 100, 1000))
    assert w.set_limit("X", "buy", ta.Size.units(5.0), 98.0) is None  # working

    # Bar never reaches the limit.
    assert w.update("X", ta.Candle(100, 102, 98.5, 101, 1000)) == []
    assert w.position("X") == 0.0

    # Bar trades through it — fills at the limit.
    fills = w.update("X", ta.Candle(100, 101, 97, 99, 1000))
    assert len(fills) == 1
    assert fills[0].kind == "limit"
    assert fills[0].price == pytest.approx(98.0)
    assert w.position("X") == pytest.approx(5.0)


def test_wallet_limit_order_can_be_cancelled():
    w = ta.PaperWallet(10_000.0)
    w.update("X", ta.Candle(100, 101, 99, 100, 1000))
    w.set_limit("X", "buy", ta.Size.units(5.0), 98.0)
    w.cancel_limit("X")
    assert w.update("X", ta.Candle(100, 101, 90, 95, 1000)) == []
    assert w.position("X") == 0.0


def test_okx_wallet_constructs_and_reads_empty_cache():
    # Construction is offline (no REST call until refresh_account / update), so
    # this exercises the binding surface without touching the network.
    w = ta.OkxWallet.demo("key", "secret", "passphrase")
    assert w.funds == 0.0
    assert w.equity == 0.0
    assert w.position("BTC-USDT-SWAP") == 0.0
    assert w.price("BTC-USDT-SWAP") is None
    assert w.errors() == []
    # The isolated-margin / mainnet variants construct just as cleanly.
    ta.OkxWallet.demo("key", "secret", "passphrase", td_mode="isolated")
    ta.OkxWallet.mainnet("key", "secret", "passphrase")


def test_can_short_reports_what_each_account_can_hold():
    # Introspection, not enforcement: the paper account shorts freely, an OKX
    # swap account does too, and Coinbase spot says so up front instead of
    # leaving a caller to discover it from a clamped order.
    assert ta.PaperWallet(10_000.0).can_short is True
    assert ta.OkxWallet.demo("key", "secret", "passphrase").can_short is True
    assert "can_short" in dir(ta.CoinbaseWallet)


def test_quote_ccy_reports_the_unit_each_account_counts_in():
    # `None` is "unlabelled", never "no currency": simulated money has no venue
    # to ask, so the paper wallet declines to guess rather than assuming dollars.
    assert ta.PaperWallet(10_000.0).quote_ccy is None
    # Labelling is descriptive only — same funds, same behaviour, one more fact.
    labelled = ta.PaperWallet(10_000.0, quote_ccy="EUR")
    assert labelled.quote_ccy == "EUR"
    assert labelled.funds == 10_000.0
    # The old positional call is untouched, which is what keeps this additive.
    assert ta.PaperWallet(10_000.0).funds == 10_000.0
    # A live account answers from its venue: USDⓈ-M swaps settle in USDT.
    assert ta.OkxWallet.demo("key", "secret", "passphrase").quote_ccy == "USDT"
    assert "quote_ccy" in dir(ta.CoinbaseWallet)


def test_data_sources_names_the_providers_that_quote_each_account():
    # Introspection about the *feed*, the third question of the same shape as
    # `can_short` and `quote_ccy`. A paper account is fed by whoever ran it, so
    # it names nobody rather than guessing; `[]` is "does not say".
    assert ta.PaperWallet(10_000.0).data_sources == []
    # A live account names its venue, using the id `ta.fetch` takes. Venue
    # granularity only: this account trades swaps, so the matching bars are
    # `okx:BTC-USDT-SWAP`, not the spot pair the same provider also serves.
    assert ta.OkxWallet.demo("key", "secret", "passphrase").data_sources == ["okx"]
    assert "data_sources" in dir(ta.CoinbaseWallet)


def test_run_rejects_a_non_wallet():
    # `.run(...)` accepts one of the supported wallet types; anything else is a
    # clear TypeError from the wallet-dispatch (not a downstream failure). Match
    # the stable stem so adding a wallet kind doesn't break this.
    strat = ta.Strategy("X")
    with pytest.raises(TypeError, match="must be a PaperWallet"):
        strat.run(
            "not a wallet",
            {"open": [], "high": [], "low": [], "close": [], "volume": []},
        )


def test_wallet_relative_sizing():
    w = ta.PaperWallet(1_000.0)
    w.update("X", 25.0)
    # 10% of funds / price 25 = 4 units, resolved at the fill (open 25)
    w.set("X", "buy", ta.Size.funds_frac(0.1))
    w.update("X", 25.0)
    assert w.orders()[-1].units == pytest.approx(4.0)
    # set to 50% of the position -> sell 2
    w.set("X", "buy", ta.Size.position_frac(0.5))
    w.update("X", 25.0)
    trimmed = w.orders()[-1]
    assert trimmed.side == "sell"
    assert trimmed.units == pytest.approx(2.0)


def test_wallet_value_fraction_flips_all_in():
    w = ta.PaperWallet(1_000.0)
    w.update("X", 100.0)
    w.set("X", "buy", ta.Size.value_frac(1.0))  # all-in: 1000 / 100 = 10 units
    w.update("X", 100.0)
    assert w.position("X") == pytest.approx(10.0)
    # equity is still 1000; one set flips all-in short -> -10 units
    w.set("X", "sell", ta.Size.value_frac(1.0))
    w.update("X", 100.0)
    assert w.orders()[-1].units == pytest.approx(20.0)
    assert w.position("X") == pytest.approx(-10.0)


def test_wallet_close_and_equity():
    w = ta.PaperWallet(1_000.0)
    w.update("X", 100.0)
    w.set("X", "buy", 4.0)
    w.update("X", 100.0)  # fill: funds 600, +4 units
    w.update("X", 120.0)
    assert w.equity == pytest.approx(600.0 + 4.0 * 120.0)
    w.close("X")
    w.update("X", 120.0)  # fills the close at the open 120
    assert not w.positions()
    assert w.funds == pytest.approx(1_080.0)
    assert [o.side for o in w.orders()] == ["buy", "sell"]


def test_wallet_drives_a_python_strategy():
    """A 'strategy' is just Python code acting on the wallet each bar."""
    fast = ta.sma(ta.close(), 2)
    slow = ta.sma(ta.close(), 4)
    enter = ta.sma(ta.close(), 2).crosses_above(ta.sma(ta.close(), 4))
    exit_ = ta.sma(ta.close(), 2).crosses_below(ta.sma(ta.close(), 4))
    del fast, slow

    w = ta.PaperWallet(1_000.0)
    # Decline (fast below slow), then a rally that up-crosses (buy), then a
    # drop that down-crosses (close). A first-bar cross coinciding with warm-up
    # is deliberately not signalled, so the data must cross *after* warm-up.
    for c in closes([10, 9, 8, 7, 8, 10, 12, 13, 11, 9, 7]):
        w.update("X", c.close)  # price the wallet each bar
        # Advance both signals every bar; never short-circuit one with the other.
        entered = enter.update(c)
        exited = exit_.update(c)
        if entered:
            w.set("X", "buy", ta.Size.value_frac(1.0))
        elif exited:
            w.close("X")
    assert [o.side for o in w.orders()] == ["buy", "sell"]


def _ohlcv(prices):
    """A flat-OHLC OHLCV dict from a list of close prices."""
    return {
        "open": list(prices),
        "high": [p + 0.5 for p in prices],
        "low": [p - 0.5 for p in prices],
        "close": list(prices),
        "volume": [1000.0] * len(prices),
    }


def test_strategy_builder_runs_and_reports():
    # Declarative twin of the imperative loop above: an always-in SMA(2)/SMA(4)
    # crossover reversal, driven by Strategy.run.
    enter = ta.sma(ta.close(), 2).crosses_above(ta.sma(ta.close(), 4))
    down = ta.sma(ta.close(), 2).crosses_below(ta.sma(ta.close(), 4))
    strat = ta.Strategy("BTC").long_on(enter, down).short_on(down, enter)

    prices = [14, 13, 12, 11, 10, 11, 13, 15, 17, 15, 12, 9, 7, 9, 12, 15]
    wallet = ta.PaperWallet(10_000.0)
    rep = strat.run(wallet, _ohlcv(prices))

    assert len(rep.equity_curve) == len(prices)
    assert rep.initial_equity == pytest.approx(10_000.0)
    assert len(rep.fills) >= 1
    # The report's blotter matches the wallet's (run mutates the wallet).
    assert len(rep.fills) == len(wallet.orders())
    assert rep.fills[0].bar >= 1  # never fills on the signal's own bar
    assert rep.fills[0].order.side in ("buy", "sell")


def test_strategy_sizing_and_metrics_pipeline():
    from fugazi.metrics import per_bar_returns, total_return

    enter = ta.sma(ta.close(), 2).crosses_above(ta.sma(ta.close(), 4))
    down = ta.sma(ta.close(), 2).crosses_below(ta.sma(ta.close(), 4))
    prices = [10, 11, 12, 11, 10, 12, 14, 16, 15, 13, 15, 17]

    # Half-position sizing scales the value-fraction magnitude.
    strat = ta.Strategy("BTC").long_on(enter, down).position_sizing(ta.value(0.5))
    wallet = ta.PaperWallet(10_000.0)
    rep = strat.run(wallet, _ohlcv(prices))

    rets = per_bar_returns(rep.equity_curve, rep.initial_equity)
    assert len(rets) == len(prices)
    # The report feeds the metrics functions directly (the whole point).
    tr = total_return(rep.equity_curve, rep.initial_equity)
    assert isinstance(tr, float)


def test_strategy_rejects_a_real_rooted_signal():
    # A strategy trades over candles; a bare-value (Real) signal has no market
    # context and is rejected.
    with pytest.raises(ValueError):
        ta.Strategy("BTC").long_on(ta.identity().above(5.0))


# ---------------------------------------------------------------------------
# Preset catalogue: `buy_and_hold`, `ma_crossover`, `rsi_reversal`,
# `donchian_breakout`, `keltner_breakout` — each returns a preset Strategy
# whose recipe dispatches to the Rust `fugazi::strategies` catalogue.
# ---------------------------------------------------------------------------


def test_buy_and_hold_preset_runs_end_to_end():
    strat = ta.buy_and_hold("BTC")
    prices = [100, 102, 105, 108, 112, 115]
    wallet = ta.PaperWallet(10_000.0)
    rep = strat.run(wallet, _ohlcv(prices))
    # Enters on bar 1, holds through: at least one fill, equity rises.
    assert len(rep.fills) >= 1
    assert rep.equity_curve[-1] > rep.initial_equity


def test_ma_crossover_preset_matches_the_manual_build_shape():
    # A preset ma_crossover should trade the same golden/death crosses as the
    # manual builder with the same fast/slow. We assert convergence of the
    # blotters' fill counts on a golden-then-death path.
    manual = (
        ta.Strategy("BTC")
        .long_on(
            ta.sma(ta.close(), 2).crosses_above(ta.sma(ta.close(), 4)),
            ta.sma(ta.close(), 2).crosses_below(ta.sma(ta.close(), 4)),
        )
        .short_on(
            ta.sma(ta.close(), 2).crosses_below(ta.sma(ta.close(), 4)),
            ta.sma(ta.close(), 2).crosses_above(ta.sma(ta.close(), 4)),
        )
    )
    preset = ta.ma_crossover("BTC", fast=2, slow=4)
    prices = [14, 13, 12, 11, 12, 14, 16, 18, 15, 12]

    w1 = ta.PaperWallet(10_000.0)
    r1 = manual.run(w1, _ohlcv(prices))
    w2 = ta.PaperWallet(10_000.0)
    r2 = preset.run(w2, _ohlcv(prices))
    # Both trade non-trivially and land the same total number of fills.
    assert len(r1.fills) >= 1
    assert len(r1.fills) == len(r2.fills)


def test_all_catalogue_presets_construct_and_produce_snapshots():
    # Smoke test: every catalogue preset builds a Strategy that runs against
    # a paper wallet without panicking, and produces an equity curve.
    presets = [
        ta.buy_and_hold("BTC"),
        ta.ma_crossover("BTC", fast=2, slow=4),
        ta.rsi_reversal("BTC", period=3),
        ta.donchian_breakout("BTC", period=3),
        ta.keltner_breakout("BTC", ema_period=3, atr_period=3),
    ]
    prices = [10, 12, 11, 14, 15, 13, 16, 18, 17, 19, 21, 18, 15, 12, 14, 17]
    for strat in presets:
        wallet = ta.PaperWallet(10_000.0)
        rep = strat.run(wallet, _ohlcv(prices))
        assert len(rep.equity_curve) == len(prices)


def test_trailing_risk_of_strategy_indicators_construct_and_read():
    # Trailing-risk-of-strategy indicators embed a preset Strategy, drive it
    # against a private wallet, and read a rolling metric over its equity
    # curve. Smoke-test that each variant builds and produces at least one
    # Some reading over a modest window.
    strat = ta.ma_crossover("BTC", fast=2, slow=4)
    period = 3
    bpy = 252.0
    # Build one instance of each — verify no panic on construction.
    inds = [
        ta.sharpe_of(strat, period=period, bars_per_year=bpy),
        ta.sortino_of(strat, period=period, bars_per_year=bpy),
        ta.volatility_of(strat, period=period, bars_per_year=bpy),
        ta.max_drawdown_of(strat, period=period),
        ta.calmar_of(strat, period=period, bars_per_year=bpy),
    ]
    prices = [10, 11, 12, 11, 10, 12, 14, 16, 15, 13, 15, 17, 19, 21]
    # Trailing indicators consume Snapshot, so feed a per-bar dict tagged
    # with the strategy's symbol (the embedded strategy prices its own
    # wallet from that entry).
    for ind in inds:
        readings = []
        for p in prices:
            v = ind.update(
                {"BTC": ta.Candle(open=p, high=p, low=p, close=p, volume=0.0)}
            )
            if v is not None:
                readings.append(v)
        # Every metric should have produced at least one Some over a
        # 14-bar path with period=3.
        assert len(readings) > 0, f"no readings from trailing indicator {ind}"


def test_preset_strategy_rejects_builder_methods():
    # Preset strategies carry their catalogue recipe; layering `.long_on()`
    # etc. on top makes no sense — should raise a clear error.
    preset = ta.buy_and_hold("BTC")
    # An always-true signal from close > 0 (candle-rooted, matches Strategy's
    # snapshot input) — the shape a real user would pass.
    always_true = ta.close().gt(ta.value(-1.0))
    with pytest.raises(ValueError, match="preset"):
        preset.long_on(always_true)
    with pytest.raises(ValueError, match="preset"):
        preset.short_on(always_true)
    with pytest.raises(ValueError, match="preset"):
        preset.position_sizing(ta.value(0.5))


# ---------------------------------------------------------------------------
# Multi-symbol strategies: PairsStrategy, MultiAssetStrategy, BasketStrategy —
# each drives over a sequence of snapshots (dict[sym -> Candle]).
# ---------------------------------------------------------------------------


def _msnaps(series):
    """A list of Snapshots from dict[sym -> list[close]] (flat OHLC candles)."""
    n = len(next(iter(series.values())))
    out = []
    for i in range(n):
        d = {
            sym: ta.Candle(prices[i], prices[i], prices[i], prices[i], 1000.0)
            for sym, prices in series.items()
        }
        out.append(ta.Snapshot(d))
    return out


def test_pairs_strategy_runs_and_reports():
    # Enter (long BTC / short ETH) when BTC's close crosses above ETH's;
    # exit when it crosses back below. Both signals are snapshot-rooted.
    enter = ta.close(ta.pick("BTC")).crosses_above(ta.close(ta.pick("ETH")))
    exit_ = ta.close(ta.pick("BTC")).crosses_below(ta.close(ta.pick("ETH")))
    strat = ta.PairsStrategy("BTC", "ETH").on(enter, exit_)

    snaps = _msnaps(
        {
            "BTC": [10, 11, 9, 8, 10, 13, 15, 14, 11, 9, 8],
            "ETH": [10, 10, 10, 10, 10, 10, 10, 10, 10, 10, 10],
        }
    )
    wallet = ta.PaperWallet(10_000.0)
    rep = strat.run(wallet, snaps)

    assert len(rep.equity_curve) == len(snaps)
    assert rep.initial_equity == pytest.approx(10_000.0)
    # An up-cross opens both legs (long BTC + short ETH) → at least two fills.
    assert len(rep.fills) >= 2
    assert len(rep.fills) == len(wallet.orders())
    assert {f.order.symbol for f in rep.fills} == {"BTC", "ETH"}


def test_pairs_strategy_trades_both_spread_directions():
    # The spread close(BTC) - close(ETH) crosses zero in both directions.
    # Long-spread when BTC leads, short-spread when it lags.
    spread = ta.close(ta.pick("BTC")).sub(ta.close(ta.pick("ETH")))
    long_only = ta.PairsStrategy("BTC", "ETH").long_spread_on(
        spread.gt(ta.value(1.0)), spread.lt(ta.value(0.0))
    )
    both = long_only.short_spread_on(
        spread.lt(ta.value(-1.0)), spread.gt(ta.value(0.0))
    )

    snaps = _msnaps(
        {
            "BTC": [10, 12, 13, 10, 8, 7, 8, 11, 13, 12, 9, 7],
            "ETH": [10, 10, 10, 10, 10, 10, 10, 10, 10, 10, 10, 10],
        }
    )

    def min_btc_position(report):
        """Lowest signed BTC holding across the run, replayed from fills."""
        units = 0.0
        lowest = 0.0
        for fill in report.fills:
            if fill.order.symbol != "BTC":
                continue
            units += fill.order.units if fill.order.side == "buy" else -fill.order.units
            lowest = min(lowest, units)
        return lowest

    long_report = long_only.run(ta.PaperWallet(10_000.0), snaps)
    both_report = both.run(ta.PaperWallet(10_000.0), snaps)

    # A sell on BTC alone proves nothing (closing a long also sells) — the
    # signed position is what separates the two directions.
    assert min_btc_position(long_report) > -1e-9
    assert min_btc_position(both_report) < -1e-9
    assert len(both_report.fills) > len(long_report.fills)


def test_pairs_short_spread_alias_methods_still_work():
    # `on` / `spread_stop_loss` / `spread_take_profit` remain valid spellings
    # of the long-spread side.
    spread = ta.close(ta.pick("BTC")).sub(ta.close(ta.pick("ETH")))
    strat = (
        ta.PairsStrategy("BTC", "ETH")
        .on(spread.gt(ta.value(1.0)), spread.lt(ta.value(0.0)))
        .spread_stop_loss(ta.value(-5.0))
        .spread_take_profit(ta.value(5.0))
    )
    snaps = _msnaps({"BTC": [10, 12, 13, 9], "ETH": [10, 10, 10, 10]})
    rep = strat.run(ta.PaperWallet(10_000.0), snaps)
    assert len(rep.equity_curve) == len(snaps)


def test_multi_asset_strategy_trades_symbols_independently():
    # The same SMA(2)/SMA(4) crossover reversal run independently per symbol,
    # via per-symbol factories rooted on each symbol with pick(sym).
    def up(sym):
        return ta.sma(ta.close(ta.pick(sym)), 2).crosses_above(
            ta.sma(ta.close(ta.pick(sym)), 4)
        )

    def down(sym):
        return ta.sma(ta.close(ta.pick(sym)), 2).crosses_below(
            ta.sma(ta.close(ta.pick(sym)), 4)
        )

    strat = (
        ta.MultiAssetStrategy()
        .long_on(up, down)
        .short_on(down, up)
        .position_sizing(lambda sym: ta.value(0.5))
    )
    snaps = _msnaps(
        {
            "BTC": [14, 13, 12, 11, 10, 11, 13, 15, 17, 15, 12, 9, 7, 9, 12, 15],
            "ETH": [20, 21, 22, 23, 24, 23, 21, 19, 17, 19, 22, 25, 27, 25, 22, 19],
        }
    )
    wallet = ta.PaperWallet(10_000.0)
    rep = strat.run(wallet, snaps)

    assert len(rep.equity_curve) == len(snaps)
    # Both symbols trade at least once (they cross on opposite schedules).
    assert {f.order.symbol for f in rep.fills} == {"BTC", "ETH"}
    assert len(rep.fills) == len(wallet.orders())


def test_multi_asset_factory_type_error_surfaces():
    # A factory that returns a non-Signal raises when the symbol is first seen.
    strat = ta.MultiAssetStrategy().long_on(lambda sym: "not a signal")
    wallet = ta.PaperWallet(10_000.0)
    with pytest.raises(BaseException):
        strat.run(wallet, _msnaps({"BTC": [1, 2, 3]}))


def test_basket_strategy_selects_top_and_bottom():
    # Cross-sectional momentum: score each symbol by 1-bar rate of change,
    # long the top, short the bottom, half-weight each leg, rebalanced daily.
    strat = (
        ta.BasketStrategy()
        .scored_by(lambda sym: ta.close(ta.pick(sym)).roc(1))
        .sized_by(lambda sym: ta.value(0.5))
        .top_bottom(1, 1)
    )
    # AAA rising, CCC falling, BBB flat → AAA long, CCC short each bar.
    snaps = _msnaps(
        {
            "AAA": [10, 11, 12, 13, 14, 15, 16, 17],
            "BBB": [10, 10, 10, 10, 10, 10, 10, 10],
            "CCC": [10, 9, 8, 7, 6, 5, 4, 3],
        }
    )
    wallet = ta.PaperWallet(10_000.0)
    rep = strat.run(wallet, snaps)

    assert len(rep.equity_curve) == len(snaps)
    assert len(rep.fills) >= 2
    # Only the extreme movers get traded; the flat middle is never selected.
    assert "BBB" not in {f.order.symbol for f in rep.fills}


def test_basket_balance_sides_and_universe_chain():
    # The builder methods chain and the side-balanced + declared-universe
    # variant still runs end-to-end. `balance_sides()` defaults its argument
    # to True, matching the Rust default.
    strat = (
        ta.BasketStrategy()
        .scored_by(lambda sym: ta.close(ta.pick(sym)).roc(1))
        .sized_by(lambda sym: ta.value(1.0))
        .top_bottom(1, 1)
        .balance_sides()
        .any_of(["AAA", "BBB", "CCC"])
    )
    snaps = _msnaps(
        {
            "AAA": [10, 11, 12, 13, 14, 15],
            "BBB": [10, 10, 10, 10, 10, 10],
            "CCC": [10, 9, 8, 7, 6, 5],
        }
    )
    wallet = ta.PaperWallet(10_000.0)
    rep = strat.run(wallet, snaps)
    assert len(rep.equity_curve) == len(snaps)


def test_basket_selection_composes_via_of():
    # top_bottom(2, 2) OF threshold(85, 15): the inner threshold admits
    # AAA, BBB on the long side (>= 85) and DDD on the short side (<= 15);
    # CCC (80) sits in the gap, so the ranked pick never sees it — where a
    # bare top_bottom(2, 2) would have shorted it. Proves the `of=` inner
    # actually narrows the pool.
    strat = (
        ta.BasketStrategy()
        .scored_by(lambda sym: ta.close(ta.pick(sym)))
        .sized_by(lambda sym: ta.value(0.2))
        .top_bottom(2, 2, of=ta.threshold(85.0, 15.0))
    )
    snaps = _msnaps(
        {
            "AAA": [100, 100, 100, 100],
            "BBB": [90, 90, 90, 90],
            "CCC": [80, 80, 80, 80],
            "DDD": [10, 10, 10, 10],
        }
    )
    wallet = ta.PaperWallet(10_000.0)
    rep = strat.run(wallet, snaps)
    traded = {f.order.symbol for f in rep.fills}
    assert {"AAA", "BBB", "DDD"} <= traded
    assert "CCC" not in traded  # gated out by the inner threshold


def test_basket_selection_install_via_seam_and_everything_leaf():
    # The general `.selection(...)` seam installs a rule built from the free
    # constructors, and `ta.everything()` is an explicit full-universe leaf.
    strat = (
        ta.BasketStrategy()
        .scored_by(lambda sym: ta.close(ta.pick(sym)).roc(1))
        .sized_by(lambda sym: ta.value(0.5))
        .selection(ta.top_bottom(1, 1, of=ta.everything()))
    )
    snaps = _msnaps(
        {
            "AAA": [10, 11, 12, 13, 14],
            "BBB": [10, 10, 10, 10, 10],
            "CCC": [10, 9, 8, 7, 6],
        }
    )
    wallet = ta.PaperWallet(10_000.0)
    rep = strat.run(wallet, snaps)
    assert len(rep.equity_curve) == len(snaps)
    assert "BBB" not in {f.order.symbol for f in rep.fills}


# ---------------------------------------------------------------------------
# Unrooted leaves and per-symbol factories: what the builders refuse.
#
# Every case below used to raise `PanicException` — a Rust panic bridged across
# the FFI boundary. `PanicException` derives from `BaseException`, so
# `except Exception` walked straight past it and a caller could not handle any
# of this without also swallowing their own `KeyboardInterrupt`. Each is now an
# ordinary exception raised by the builder, before the run starts.
# ---------------------------------------------------------------------------


def test_pairs_refuses_a_leaf_that_named_no_asset():
    # A pairs strategy blesses neither leg, so `close()` has no series to read
    # and the bar carries both. The YAML side refuses the same document at
    # build time (`Root::ambiguous("pairs")`); this is the Python mirror.
    with pytest.raises(ValueError, match="privileges neither leg"):
        ta.PairsStrategy("BTC", "ETH").long_spread_on(
            ta.close() > ta.sma(ta.close(), 20)
        )


def test_pairs_refuses_an_unrooted_level_source():
    # Same rule on the source slots, not just the signal ones.
    with pytest.raises(ValueError, match="privileges neither leg"):
        ta.PairsStrategy("BTC", "ETH").long_spread_stop_loss(ta.close())


def test_pairs_refuses_an_unrooted_calendar_leaf_but_takes_a_rooted_one():
    # A calendar leaf reads only the bar's timestamp, so *semantically* it does
    # not care which leg it rides — but it still has to say. Rooting it on
    # either leg is the spelling, and both legs share the time, so the answer
    # is the same one.
    with pytest.raises(ValueError, match="privileges neither leg"):
        ta.PairsStrategy("BTC", "ETH").rebalance_on(ta.day_of_week() > 3)

    strat = (
        ta.PairsStrategy("BTC", "ETH")
        .long_spread_on(ta.close(ta.pick("BTC")) > ta.close(ta.pick("ETH")))
        .rebalance_on(ta.day_of_week(ta.pick("BTC")) > 3)
    )
    rep = strat.run(
        ta.PaperWallet(10_000.0),
        _msnaps(
            {
                "BTC": [10, 11, 12, 11, 10, 9, 10, 12],
                "ETH": [10, 10, 10, 10, 10, 10, 10, 10],
            }
        ),
    )
    assert len(rep.equity_curve) == 8


def test_pairs_still_accepts_a_constant_sizing_multiplier():
    # A constant reads no series at all, so it is not ambiguous and must keep
    # working — the refusal is scoped to candle- and atom-rooted leaves.
    strat = ta.PairsStrategy("BTC", "ETH").position_sizing(ta.value(0.5))
    assert isinstance(strat, ta.PairsStrategy)


@pytest.mark.parametrize(
    "wire",
    [
        lambda arg: ta.BasketStrategy().scored_by(arg),
        lambda arg: ta.BasketStrategy().sized_by(arg),
        lambda arg: ta.MultiAssetStrategy().long_on(arg),
        lambda arg: ta.MultiAssetStrategy().short_on(arg),
        lambda arg: ta.MultiAssetStrategy().position_sizing(arg),
    ],
)
def test_per_symbol_slots_reject_a_non_callable(wire):
    # Passing the indicator itself, rather than a `sym -> Indicator` factory,
    # is the common slip: each symbol needs its own chain rooted on that
    # symbol, so the slot takes a function of the symbol.
    with pytest.raises(TypeError, match="per-symbol factory"):
        wire(ta.rsi(ta.close(), 14))


def test_per_symbol_slots_reject_a_non_callable_exit_too():
    # The optional second argument is a factory on the same terms as the first.
    with pytest.raises(TypeError, match="per-symbol factory"):
        ta.MultiAssetStrategy().long_on(
            lambda sym: ta.close(ta.pick(sym)) > ta.value(1.0),
            ta.close() < ta.value(1.0),
        )


def test_wallet_rejects_bad_side():
    w = ta.PaperWallet(100.0)
    w.update("X", 10.0)
    with pytest.raises(ValueError):
        w.set("X", "hodl", 1.0)


def test_impossible_market_orders_never_fill():
    w = ta.PaperWallet(100.0)
    w.update("X", 50.0)
    # A market buy beyond funds (3 * 50 = 150 > 100) is pre-flighted at
    # submission against last close — the wallet raises synchronously
    # instead of queuing an order that would never fill.
    with pytest.raises(ValueError, match="insufficient funds"):
        w.set("X", "buy", 3.0)
    assert not w.positions()
    # A short sale credits cash, so the *cash* rule can never bound it — which is
    # why the leverage rule has to. 3 units at 50 is 150 of gross against 100 of
    # equity, and an unlevered wallet refuses it just as it refused the buy.
    with pytest.raises(ValueError, match="gross exposure limit"):
        w.set("X", "sell", 3.0)
    assert not w.positions()
    # 2 units short is 100 of gross against 100 of equity — exactly 1x, the
    # mirror image of the 2-unit long the cash rule allows.
    w.set("X", "sell", 2.0)
    w.update("X", 50.0)
    assert w.position("X") == pytest.approx(-2.0)


def test_one_sizing_value_means_one_exposure_on_both_sides():
    """Same document, same account, same bars — only the side differs.

    `sizing: 3.0` used to mean two different things. A buy was bounded by the
    cash it spent, so the long leg was quietly scaled back to 1x; a sale
    *credits* cash, so nothing bounded the short and it took the full 3x. What
    bounds both is gross notional.
    """
    bars = [
        ta.Snapshot(
            {
                "S": ta.Atom(
                    ta.Candle(100, 100, 100, 100, 1),
                    None,
                    1_700_000_000_000 + i * 86_400_000,
                )
            }
        )
        for i in range(4)
    ]

    def carried(side, sizing, **kw):
        spec = f"root: S\n{side}:\n  enter: !gt {{ lhs: !close, rhs: 0 }}\nsizing: {sizing}\n"
        w = ta.PaperWallet(10_000.0, **kw)
        report = ta.load_spec(spec).run(w, bars)
        gross = abs(w.position("S")) * 100.0 / w.equity
        return gross, report

    long_gross, long_report = carried("long", 3.0)
    short_gross, short_report = carried("short", 3.0)
    assert long_gross == pytest.approx(short_gross), (
        f"long took {long_gross:.2f}x and short took {short_gross:.2f}x under one spec value"
    )
    assert long_gross == pytest.approx(1.0)

    # Neither is a rejection — the fill happened, at a size nobody asked for —
    # so the ask rides on the order instead.
    for report in (long_report, short_report):
        assert not report.rejections
        order = report.fills[0].order
        assert order.units == pytest.approx(100.0)
        assert order.requested_units == pytest.approx(300.0)
        assert order.fill_ratio == pytest.approx(1 / 3)

    # And the knob lifts both by the same multiple, which is what makes a 3x
    # live account comparable to a backtest at all.
    for side in ("long", "short"):
        gross, report = carried(side, 3.0, max_gross=3.0)
        assert gross == pytest.approx(3.0), f"{side} carried {gross:.2f}x"
        order = report.fills[0].order
        assert order.units == pytest.approx(order.requested_units)


def test_leverage_is_readable_off_every_wallet():
    """`None` is "does not say", never `1x` — and a paper wallet does say,
    because the cap is a rule it enforces rather than a label it was handed."""
    assert ta.PaperWallet(1_000.0).leverage("S") == pytest.approx(1.0)
    assert ta.PaperWallet(1_000.0, max_gross=2.5).leverage("S") == pytest.approx(2.5)
    # Answered for a symbol it has never been fed: the cap is a property of the
    # account, not of the instrument.
    assert ta.PaperWallet(1_000.0, max_gross=2.5).leverage(
        "never-seen"
    ) == pytest.approx(2.5)
    # A spot venue answers None structurally, as it answers False to can_short.
    # Checked on the class: constructing one needs a real EC private key.
    assert "leverage" in dir(ta.CoinbaseWallet)
    # A live swap wallet has one, but has not been able to ask yet.
    okx = ta.OkxWallet.demo("key", "secret", "passphrase")
    assert okx.leverage("BTC-USDT-SWAP") is None
    assert okx.can_short


def _levered_bars(prices, funding=0.0005):
    """One symbol, a `funding_rate` column, one bar a day."""
    sch = ta.SchemaBuilder()
    sch.add_real("funding_rate")
    schema = sch.finish()
    day = 86_400_000
    t0 = 1_700_000_000_000
    return [
        ta.Snapshot(
            {
                "S": ta.Atom(
                    ta.Candle(p, p * 1.001, p * 0.999, p, 1.0),
                    ta.OverlayInfo(schema, [funding]),
                    t0 + i * day,
                )
            }
        )
        for i, p in enumerate(prices)
    ]


_LEVERED = "root: S\nlong:\n  enter: !gt { lhs: !close, rhs: 0 }\nsizing: 3.0\n"


def test_a_margin_call_is_the_difference_between_a_win_and_a_wipeout():
    """The gap that makes an unliquidated levered backtest describe a different
    strategy. A 3x long into a drawdown that then recovers reports a profit if
    nothing closes it out — but the account it describes did not survive to see
    the recovery."""
    # Down 26%, then all the way back and beyond.
    prices = (
        [100.0, 100.0]
        + [100 - 2.0 * i for i in range(14)]
        + [74 + 2.0 * i for i in range(20)]
    )
    bars = _levered_bars(prices)

    survives = ta.PaperWallet(10_000.0, max_gross=3.0)
    ta.load_spec(_LEVERED).run(survives, bars)

    closed = ta.PaperWallet(10_000.0, max_gross=3.0, maintenance_margin=0.10)
    report = ta.load_spec(_LEVERED).run(closed, bars)

    assert survives.equity > 10_000.0, (
        f"unliquidated should profit, got {survives.equity}"
    )
    assert closed.equity < 10_000.0, (
        f"liquidated should not recover, got {closed.equity}"
    )
    # And the forced legs say what they were, so the blotter can be read.
    kinds = {f.order.kind for f in report.fills}
    assert "liquidation" in kinds, kinds
    assert closed.maintenance_margin == pytest.approx(0.10)
    # Off unless asked for — the ratio is a venue assumption, not a default.
    assert ta.PaperWallet(1_000.0).maintenance_margin is None


def test_funding_is_charged_from_the_series_and_counted():
    """Funding's rate is data, so it is read per bar off an overlay column — and
    the wallet counts whether it ever actually arrived, because a model that
    silently charges nothing looks exactly like carry being free."""
    bars = _levered_bars([100.0] * 60, funding=0.0005)

    free = ta.PaperWallet(10_000.0, max_gross=3.0)
    ta.load_spec(_LEVERED).run(free, bars)
    assert free.carry_coverage() == (0, 0), "nothing asked for a rate"

    charged = ta.PaperWallet(10_000.0, max_gross=3.0)
    charged.set_costs_for_all(
        ["S"], ta.TradingCostsConfig({"carry": {"default": {"funding": {}}}})
    )
    ta.load_spec(_LEVERED).run(charged, bars)

    wanted, got = charged.carry_coverage()
    assert wanted == got > 0, f"the column was there on every bar: {(wanted, got)}"
    assert charged.equity < free.equity, "funding should cost the long side something"

    # A model whose column is absent charges nothing — and says so, rather than
    # leaving the run indistinguishable from one with no carry at all.
    blind = ta.PaperWallet(10_000.0, max_gross=3.0)
    blind.set_costs_for_all(
        ["S"],
        ta.TradingCostsConfig(
            {"carry": {"default": {"funding": {"column": "absent"}}}}
        ),
    )
    ta.load_spec(_LEVERED).run(blind, bars)
    wanted, got = blind.carry_coverage()
    assert wanted > 0 and got == 0, (
        f"expected wanted-but-never-got, saw {(wanted, got)}"
    )
    assert blind.equity == pytest.approx(free.equity)


def test_margin_interest_is_measured_from_the_bars_when_it_can_be():
    """A wallet not told a cadence still charges correctly when the *bars* carry
    times: the interval is measured from consecutive stamps, so declaring `1d`
    over daily bars is the same answer arrived at twice."""
    bars = _levered_bars([100.0] * 60)

    measured = ta.PaperWallet(10_000.0, max_gross=3.0, margin_rate=0.08)
    ta.load_spec(_LEVERED).run(measured, bars)

    declared = ta.PaperWallet(10_000.0, max_gross=3.0, margin_rate=0.08, bar_freq="1d")
    ta.load_spec(_LEVERED).run(declared, bars)

    assert measured.equity < 10_000.0, "borrowing 20k at 8% should cost something"
    assert measured.equity == pytest.approx(declared.equity), (
        "daily bars measure to the cadence they were declared with"
    )
    assert declared.margin_rate == pytest.approx(0.08)
    # An unlevered book never borrows, so it is never billed.
    flat = ta.PaperWallet(10_000.0, margin_rate=0.08, bar_freq="1d")
    ta.load_spec("root: S\nlong:\n  enter: !gt { lhs: !close, rhs: 0 }\n").run(
        flat, bars
    )
    assert flat.equity == pytest.approx(10_000.0)


def test_margin_interest_refuses_to_guess_when_nothing_can_measure_it():
    """With no times on the bars *and* no declared cadence there is nothing to
    pro-rate an annual rate over, so the wallet charges nothing rather than
    inventing a year length."""
    untimed = [
        ta.Snapshot({"S": ta.Atom(ta.Candle(100.0, 100.1, 99.9, 100.0, 1.0))})
        for _ in range(60)
    ]
    wallet = ta.PaperWallet(10_000.0, max_gross=3.0, margin_rate=0.08)
    ta.load_spec(_LEVERED).run(wallet, untimed)
    assert wallet.equity == pytest.approx(10_000.0)


def test_margin_settings_reject_nonsense():
    for bad in (-0.1, float("inf"), float("nan")):
        with pytest.raises(ValueError, match="margin_rate"):
            ta.PaperWallet(1_000.0, margin_rate=bad)
    for bad in (0.0, 1.5, float("inf"), float("nan")):
        with pytest.raises(ValueError, match="maintenance_margin"):
            ta.PaperWallet(1_000.0, maintenance_margin=bad)


def test_max_gross_must_be_finite_and_positive():
    for bad in (0.0, -1.0, float("inf"), float("nan")):
        with pytest.raises(ValueError, match="max_gross"):
            ta.PaperWallet(1_000.0, max_gross=bad)


def test_order_carries_fill_price():
    w = ta.PaperWallet(1_000.0)
    w.update("X", 100.0)
    w.set_position("X", 2.0)  # queued
    w.update("X", 100.0)  # fills at this bar's open
    assert w.orders()[-1].price == pytest.approx(100.0)


def test_update_returns_the_fill_stream():
    w = ta.PaperWallet(1_000.0)
    w.update("X", 100.0)
    assert w.set("X", "buy", 2.0) is None  # queued (working)
    fills = w.update("X", 100.0)  # fills at this bar's open
    assert len(fills) == 1
    assert fills[0].side == "buy"
    assert fills[0].price == pytest.approx(100.0)
    assert fills[0].kind == "market"


def test_resting_stop_fills_at_the_level():
    w = ta.PaperWallet(10_000.0)
    w.update("X", 100.0)
    w.set("X", "buy", 1.0)
    w.update("X", 100.0)  # long 1 @ 100
    w.set_stop("X", 90.0)
    # A bar that trades down through 90 (opening above) fills at the level.
    fills = w.update("X", ta.Candle(95.0, 96.0, 88.0, 89.0, 0.0))
    assert len(fills) == 1
    assert fills[0].side == "sell"
    assert fills[0].price == pytest.approx(90.0)
    assert fills[0].kind == "stop"
    assert not w.positions()


def test_resting_stop_gaps_to_the_open():
    w = ta.PaperWallet(10_000.0)
    w.update("X", 100.0)
    w.set("X", "buy", 1.0)
    w.update("X", 100.0)
    w.set_stop("X", 90.0)
    # Gaps down opening at 85, already below the stop -> fills at the open.
    fills = w.update("X", ta.Candle(85.0, 86.0, 84.0, 84.0, 0.0))
    assert fills[0].price == pytest.approx(85.0)
    assert fills[0].kind == "stop"
    assert not w.positions()
    # A cancelled bracket no longer fires.
    w.set("X", "buy", 1.0)
    w.update("X", 100.0)
    w.set_take_profit("X", 110.0)
    w.cancel_protective("X")
    assert w.update("X", ta.Candle(105.0, 115.0, 104.0, 108.0, 0.0)) == []


def test_warm_up_and_unstable_bars():
    # Windowed: exact warm-up, no unstable tail.
    sma = ta.sma(ta.close(), 20)
    assert sma.warm_up_bars() == 20
    assert sma.unstable_bars() == 0
    assert sma.stable_bars() == 20
    # Recursive: the EMA seeds immediately but takes time to converge.
    ema = ta.ema(ta.close(), 20)
    assert ema.warm_up_bars() == 1
    assert ema.unstable_bars() > 0
    assert ema.stable_bars() == ema.warm_up_bars() + ema.unstable_bars()
    # Composition accounts for the whole chain, through signals too.
    chained = ta.ema(ta.sma(ta.close(), 10), 20)
    assert chained.warm_up_bars() == 10
    sig = ta.close().crosses_above(ta.sma(ta.close(), 10))
    assert sig.warm_up_bars() == 11  # comparison plus its edge detector
    assert sig.unstable_bars() == 0
    # Multi-output indicators report the slowest line.
    macd = ta.macd(ta.close(), 12, 26, 9)
    assert macd.warm_up_bars() == 1
    assert macd.unstable_bars() > 0


def test_warm_up_matches_first_output():
    node = ta.rsi(ta.close(), 14)
    w = node.warm_up_bars()
    out = feed(node, closes([100.0 + 0.5 * i + (i % 3) for i in range(w + 3)]))
    assert all(v is None for v in out[: w - 1])
    assert all(v is not None for v in out[w - 1 :])


def test_resample_emits_on_the_nth_bar_only():
    node = ta.resample(4, ta.close())
    out = feed(node, closes([float(i) for i in range(1, 9)]))
    # None on 1..3 and 5..7; Some(close) at 4 and 8.
    for i, v in enumerate(out, start=1):
        if i % 4 == 0:
            assert v == pytest.approx(float(i))
        else:
            assert v is None


def test_resample_ema_recurses_over_htf_closes():
    """`ema(close(), 3)` inside `resample(4, ...)` should agree with the same
    EMA fed only the resampled closes."""
    node = ta.resample(4, ta.ema(ta.close(), 3))
    reference = ta.ema(ta.identity(), 3)
    prices = [100.0 + 0.5 * i for i in range(24)]
    got_at_boundary = []
    ref_at_boundary = []
    for i, p in enumerate(prices, start=1):
        v = node.update(ta.Candle(p, p, p, p, 0.0))
        if i % 4 == 0:
            got_at_boundary.append(v)
            ref_at_boundary.append(reference.update(p))
    assert len(got_at_boundary) == 6
    for got, ref in zip(got_at_boundary, ref_at_boundary):
        # Warm-up bars are None on both sides; matched values elsewhere.
        assert (got is None and ref is None) or got == pytest.approx(ref)


def test_resample_zero_rejects():
    with pytest.raises(ValueError, match="greater than zero"):
        ta.resample(0, ta.close())


def test_latch_holds_last_source_value_between_none_ticks():
    node = ta.latch(ta.resample(3, ta.close()))
    prices = [1.0, 2.0, 3.0, 4.0, 5.0, 6.0]
    out = [node.update(ta.Candle(p, p, p, p, 0.0)) for p in prices]
    assert out[0] is None
    assert out[1] is None
    assert out[2] == pytest.approx(3.0)
    assert out[3] == pytest.approx(3.0)
    assert out[4] == pytest.approx(3.0)
    assert out[5] == pytest.approx(6.0)


def test_latch_of_signal_returns_signal():
    entry = ta.close().crosses_above(ta.value(2.0))
    latched = ta.latch(entry)
    assert isinstance(latched, ta.Signal)


def test_if_else_selects_by_condition():
    # Trend-gated: return the close when close > 100, else the constant 0.
    cond = ta.close().gt(ta.value(100.0))
    branch = ta.if_else(cond, ta.close(), ta.value(0.0))
    assert isinstance(branch, ta.Indicator)
    # Below the gate → 0; above → the close itself.
    assert branch.update(ta.Candle(99.0, 99.0, 99.0, 99.0, 0.0)) == 0.0
    assert branch.update(ta.Candle(101.0, 101.0, 101.0, 101.0, 0.0)) == 101.0
    assert branch.update(ta.Candle(105.0, 105.0, 105.0, 105.0, 0.0)) == 105.0


def test_if_else_waits_for_the_selected_branch_to_warm():
    # The condition (close > 0) is always true, so we pick then
    # (SMA-5, warm-up 5). Ternary reads None for the first four bars while
    # the SELECTED branch is warming, publishes on bar 5.
    branch = ta.if_else(
        ta.close().gt(ta.value(0.0)),
        ta.sma(ta.close(), 5),
        ta.value(99.0),
    )
    for _ in range(4):
        assert branch.update(ta.Candle(100.0, 100.0, 100.0, 100.0, 0.0)) is None
    # Fifth bar: the SMA-5 has warmed; the ternary can publish.
    assert branch.update(ta.Candle(100.0, 100.0, 100.0, 100.0, 0.0)) == 100.0


def test_if_else_publishes_early_when_selected_branch_is_fast():
    # Same shape but the condition picks the fast branch: close < 0 is
    # always false, so we pick otherwise (a constant). The ternary reads
    # Some on bar 1 even though the UNSELECTED SMA-5 hasn't warmed —
    # `warm_up_bars()` is still 5 (upper bound for downstream stability
    # gates), but the actual first Some can arrive earlier.
    branch = ta.if_else(
        ta.close().lt(ta.value(0.0)),
        ta.sma(ta.close(), 5),
        ta.value(-1.0),
    )
    assert branch.warm_up_bars() == 5
    assert branch.update(ta.Candle(100.0, 100.0, 100.0, 100.0, 0.0)) == -1.0


def test_unstable_signal_zeroes_unstable_bars_but_forwards_output():
    entry = ta.close().crosses_above(ta.ema(ta.close(), 3))
    raw_stable = entry.stable_bars()
    raw_warm = entry.warm_up_bars()
    assert raw_stable > raw_warm  # ema has a real IIR tail
    wrapped = ta.unstable(entry)
    assert isinstance(wrapped, ta.Signal)
    assert wrapped.warm_up_bars() == raw_warm
    assert wrapped.unstable_bars() == 0
    assert wrapped.stable_bars() == raw_warm

    # The wrapper is a passthrough — same boolean state per bar as the raw.
    bars = closes([100.0 + 0.5 * i + (i % 5) for i in range(raw_stable * 2)])
    plain = ta.close().crosses_above(ta.ema(ta.close(), 3))
    for c in bars:
        assert wrapped.update(c) == plain.update(c)


# ---------------------------------------------------------------------------
# Schema / OverlayInfo / Atom / Get indicator
# ---------------------------------------------------------------------------


def _schema(*keys):
    b = ta.SchemaBuilder()
    for k in keys:
        b.add(k)
    return b.finish()


def test_schema_builder_registers_columns_and_freezes():
    b = ta.SchemaBuilder()
    assert b.add("vol_20") == 0
    assert b.add("regime") == 1
    assert b.add("vol_20") == 0  # idempotent
    assert len(b) == 2
    schema = b.finish()
    assert len(schema) == 2
    assert schema.index_of("vol_20") == 0
    assert schema.index_of("missing") is None
    assert "regime" in schema
    assert "missing" not in schema
    # The builder is spent after finish.
    with pytest.raises(ValueError):
        b.add("late")


def test_overlay_info_length_mismatch_raises():
    schema = _schema("a", "b")
    with pytest.raises(ValueError):
        ta.OverlayInfo(schema, [1.0])
    ov = ta.OverlayInfo(schema, [0.1, 0.2])
    assert ov.get(0) == pytest.approx(0.1)
    assert ov.get_by_key("b") == pytest.approx(0.2)
    assert ov.get_by_key("missing") is None


def test_atom_carries_overlays_or_is_bare():
    schema = _schema("regime")
    candle = ta.Candle(100.0, 101.0, 99.0, 100.5, 1_000.0)
    bare = ta.Atom(candle)
    assert bare.overlays is None
    assert bare.time is None
    assert bare.candle.close == pytest.approx(100.5)
    overlays = ta.OverlayInfo(schema, [1.0])
    with_ov = ta.Atom(candle, overlays)
    assert with_ov.overlays is not None
    assert with_ov.overlays.get(0) == pytest.approx(1.0)


def test_atom_carries_optional_time():
    """`ta.Atom(candle, time=<UTC ms>)` and `.time` round-trip."""
    candle = ta.Candle(100.0, 101.0, 99.0, 100.5, 1_000.0)
    # 2024-03-15 12:34:56 UTC
    stamped = ta.Atom(candle, time=1_710_506_096_000)
    assert stamped.time == 1_710_506_096_000
    assert stamped.overlays is None
    # Overlays + time together.
    schema = _schema("regime")
    overlays = ta.OverlayInfo(schema, [1.0])
    both = ta.Atom(candle, overlays, time=1_710_506_096_000)
    assert both.time == 1_710_506_096_000
    assert both.overlays is not None


# 2024-03-15 12:34:56 UTC — a Friday, Q1, DOY 75.
_TS_2024_03_15 = 1_710_506_096_000


def _timed(candle_kwargs, time_ms=_TS_2024_03_15):
    c = ta.Candle(**candle_kwargs)
    return ta.Atom(c, time=time_ms)


def test_calendar_sources_decompose_atom_time():
    bar_kwargs = dict(open=1.0, high=1.0, low=1.0, close=1.0, volume=0.0)
    atom = _timed(bar_kwargs)
    checks = [
        (ta.year(), 2024.0),
        (ta.month(), 3.0),
        (ta.day(), 15.0),
        (ta.hour(), 12.0),
        (ta.minute(), 34.0),
        (ta.second(), 56.0),
        (ta.day_of_week(), 5.0),  # Friday
        (ta.day_of_year(), 75.0),
        (ta.week_of_year(), 11.0),  # ISO 8601 week
        (ta.quarter(), 1.0),
        (ta.unix_seconds(), 1_710_506_096.0),
        (ta.unix_millis(), 1_710_506_096_000.0),
    ]
    for source, want in checks:
        assert source.update(atom) == pytest.approx(want)


def test_calendar_source_none_on_untimed_atom():
    """A bare Candle → Atom has no time; calendar reads stay `None`."""
    candle = ta.Candle(1.0, 1.0, 1.0, 1.0, 0.0)
    assert ta.year().update(candle) is None
    assert ta.day_of_week().update(candle) is None


def test_calendar_sources_compose_with_operators():
    """`day_of_week().eq(1)` = Monday. Composes like any other source."""
    fri = _timed(dict(open=1.0, high=1.0, low=1.0, close=1.0, volume=0.0))
    mon = _timed(
        dict(open=1.0, high=1.0, low=1.0, close=1.0, volume=0.0),
        time_ms=_TS_2024_03_15 + 3 * 86_400_000,  # 2024-03-18 (Mon)
    )
    is_monday = ta.day_of_week().eq(1)
    is_monday.update(fri)
    assert is_monday.is_true() is False
    is_monday.update(mon)
    assert is_monday.is_true() is True


def test_is_weekday_and_is_weekend_signals():
    bar_kwargs = dict(open=1.0, high=1.0, low=1.0, close=1.0, volume=0.0)
    fri = _timed(bar_kwargs)  # 2024-03-15 Fri
    sat = _timed(bar_kwargs, time_ms=_TS_2024_03_15 + 86_400_000)

    wd = ta.is_weekday()
    wd.update(fri)
    assert wd.is_true() is True
    wd.update(sat)
    assert wd.is_true() is False

    we = ta.is_weekend()
    we.update(fri)
    assert we.is_true() is False
    we.update(sat)
    assert we.is_true() is True

    # No `atom.time` → both read False (signals-are-False-while-warming).
    bare = ta.Candle(1.0, 1.0, 1.0, 1.0, 0.0)
    assert ta.is_weekday().update(bare) is False
    assert ta.is_weekend().update(bare) is False


def test_get_indicator_reads_overlay_by_key():
    schema = _schema("vol_20")
    node = ta.get(schema, "vol_20")
    candle = ta.Candle(100.0, 101.0, 99.0, 100.5, 1_000.0)
    # Bare candle: no overlays → reader stays None.
    assert node.update(candle) is None
    # Atom with matching-schema overlays: reads the value.
    ov = ta.OverlayInfo(schema, [0.12])
    assert node.update(ta.Atom(candle, ov)) == pytest.approx(0.12)


def test_get_indicator_returns_none_on_schema_mismatch():
    schema_a = _schema("vol_20", "regime")
    schema_b = _schema("regime", "vol_20")  # same keys, different order
    node = ta.get(schema_a, "vol_20")  # index 0 in A
    candle = ta.Candle(100.0, 101.0, 99.0, 100.5, 1_000.0)
    ov_b = ta.OverlayInfo(schema_b, [1.0, 0.12])  # 0.12 lives at index 1 here
    # Mismatched schema: refuse the read rather than return 1.0 (index 0 of B).
    assert node.update(ta.Atom(candle, ov_b)) is None


def test_get_indicator_composes_with_scalar_ops():
    """Overlay values compose with the rest of the fluent operator surface."""
    schema = _schema("regime")
    signal = ta.get(schema, "regime").above(0.5)
    candle = ta.Candle(100.0, 101.0, 99.0, 100.5, 0.0)
    ov_on = ta.OverlayInfo(schema, [1.0])
    ov_off = ta.OverlayInfo(schema, [0.0])
    signal.update(ta.Atom(candle, ov_on))
    assert signal.is_true() is True
    signal.update(ta.Atom(candle, ov_off))
    assert signal.is_true() is False


def test_get_unknown_key_raises_at_construction():
    schema = _schema("vol_20")
    with pytest.raises(ValueError):
        ta.get(schema, "missing")


# ---------------------------------------------------------------------------
# Typed overlays: Real | Bool | Str
# ---------------------------------------------------------------------------


def _typed_schema():
    """A schema with one column of each supported type."""
    b = ta.SchemaBuilder()
    b.add_real("vol_20")
    b.add_bool("risk_on")
    b.add_str("regime")
    return b.finish()


def _typed_candle():
    return ta.Candle(100.0, 101.0, 99.0, 100.5, 1_000.0)


def test_schema_builder_typed_adds_and_type_of():
    schema = _typed_schema()
    assert schema.type_of_key("vol_20") == "real"
    assert schema.type_of_key("risk_on") == "bool"
    assert schema.type_of_key("regime") == "str"
    assert schema.type_of_key("missing") is None
    # By-index lookup mirrors by-key.
    assert schema.type_of(0) == "real"
    assert schema.type_of(1) == "bool"
    assert schema.type_of(2) == "str"
    assert schema.type_of(99) is None
    # `add()` is a back-compat alias for `add_real()`.
    b = ta.SchemaBuilder()
    b.add("x")
    assert b.finish().type_of_key("x") == "real"


def test_schema_builder_rejects_type_mismatch_on_reregister():
    b = ta.SchemaBuilder()
    b.add_real("x")
    with pytest.raises(ValueError):
        b.add_bool("x")


def test_overlay_info_heterogeneous_values_and_typed_accessors():
    schema = _typed_schema()
    ov = ta.OverlayInfo(schema, [0.12, True, "bull"])
    # Polymorphic `get` returns the native Python type per slot.
    assert ov.get(0) == pytest.approx(0.12)
    assert ov.get(1) is True
    assert ov.get(2) == "bull"
    # By-key polymorphic.
    assert ov.get_by_key("regime") == "bull"
    # Typed accessors return None on a type mismatch.
    assert ov.get_real(0) == pytest.approx(0.12)
    assert ov.get_real(1) is None
    assert ov.get_bool(1) is True
    assert ov.get_bool(0) is None
    assert ov.get_str(2) == "bull"
    assert ov.get_str(0) is None


def test_overlay_info_rejects_wrong_python_types_at_construction():
    schema = _typed_schema()
    # str in the Real slot.
    with pytest.raises(ValueError):
        ta.OverlayInfo(schema, ["oops", True, "bull"])
    # True/False in the Real slot: rejected (would otherwise silently coerce to 1/0).
    with pytest.raises(ValueError):
        ta.OverlayInfo(schema, [True, True, "bull"])
    # float in the Str slot.
    with pytest.raises(ValueError):
        ta.OverlayInfo(schema, [0.12, True, 0.5])


def test_get_polymorphic_dispatches_on_declared_column_type():
    schema = _typed_schema()
    real_node = ta.get(schema, "vol_20")
    bool_node = ta.get(schema, "risk_on")
    str_node = ta.get(schema, "regime")
    assert isinstance(real_node, ta.Indicator)
    assert isinstance(bool_node, ta.Signal)
    assert isinstance(str_node, ta.StrSource)


def test_get_typed_constructors_reject_type_mismatches():
    schema = _typed_schema()
    # get_real requires a Real column.
    with pytest.raises(ValueError):
        ta.get_real(schema, "risk_on")
    # get_bool requires a Bool column.
    with pytest.raises(ValueError):
        ta.get_bool(schema, "vol_20")
    # get_str requires a Str column.
    with pytest.raises(ValueError):
        ta.get_str(schema, "vol_20")


def test_get_typed_constructors_read_matching_columns():
    schema = _typed_schema()
    candle = _typed_candle()

    real_node = ta.get_real(schema, "vol_20")
    bool_node = ta.get_bool(schema, "risk_on")
    str_node = ta.get_str(schema, "regime")

    ov = ta.OverlayInfo(schema, [0.15, True, "bull"])
    atom = ta.Atom(candle, ov)
    assert real_node.update(atom) == pytest.approx(0.15)
    assert bool_node.update(atom) is True
    assert str_node.update(atom) == "bull"


def test_str_source_eq_signal_fires_on_match():
    schema = _typed_schema()
    candle = _typed_candle()
    signal = ta.get_str(schema, "regime").eq("bull")

    on = ta.Atom(candle, ta.OverlayInfo(schema, [0.0, False, "bull"]))
    off = ta.Atom(candle, ta.OverlayInfo(schema, [0.0, False, "bear"]))
    signal.update(on)
    assert signal.is_true() is True
    signal.update(off)
    assert signal.is_true() is False


def test_str_source_ne_signal_is_inverse_of_eq():
    schema = _typed_schema()
    candle = _typed_candle()
    ne = ta.get_str(schema, "regime").ne("bull")

    on = ta.Atom(candle, ta.OverlayInfo(schema, [0.0, False, "bull"]))
    off = ta.Atom(candle, ta.OverlayInfo(schema, [0.0, False, "bear"]))
    ne.update(on)
    assert ne.is_true() is False
    ne.update(off)
    assert ne.is_true() is True


def test_str_eq_free_function_matches_the_fluent_method():
    schema = _typed_schema()
    candle = _typed_candle()
    fluent = ta.get_str(schema, "regime").eq("bull")
    free = ta.str_eq(ta.get_str(schema, "regime"), "bull")

    atom = ta.Atom(candle, ta.OverlayInfo(schema, [0.0, False, "bull"]))
    fluent.update(atom)
    free.update(atom)
    assert fluent.is_true() == free.is_true() is True


def test_value_str_is_a_constant_str_source():
    c = ta.value_str("bull")
    assert isinstance(c, ta.StrSource)
    candle = _typed_candle()
    # A constant reads from any atom (or a bare candle); its value is the literal.
    assert c.update(candle) == "bull"
    assert c.value() == "bull"


def test_str_eq_accepts_two_str_sources():
    # Two StrSource operands compose the same way a StrSource + literal does.
    schema = _typed_schema()
    candle = _typed_candle()
    lhs = ta.get_str(schema, "regime")
    rhs = ta.value_str("bull")
    sig = ta.str_eq(lhs, rhs)

    atom = ta.Atom(candle, ta.OverlayInfo(schema, [0.0, False, "bull"]))
    sig.update(atom)
    assert sig.is_true() is True


def test_all_three_types_compose_into_one_and_signal():
    """The end-to-end shape a strategy would use: gate an entry on one
    overlay of each type — Real threshold, Bool flag, Str regime match."""
    schema = _typed_schema()
    candle = _typed_candle()
    gate = (
        ta.get_bool(schema, "risk_on")
        .and_(ta.get_str(schema, "regime").eq("bull"))
        .and_(ta.get_real(schema, "vol_20").gt(0.15))
    )

    def atom(vol, risk_on, regime):
        return ta.Atom(candle, ta.OverlayInfo(schema, [vol, risk_on, regime]))

    # All three conditions align — fires.
    gate.update(atom(0.20, True, "bull"))
    assert gate.is_true() is True
    # risk_on off — doesn't fire.
    gate.update(atom(0.20, False, "bull"))
    assert gate.is_true() is False
    # Regime is bear — doesn't fire.
    gate.update(atom(0.20, True, "bear"))
    assert gate.is_true() is False
    # vol below threshold — doesn't fire.
    gate.update(atom(0.10, True, "bull"))
    assert gate.is_true() is False


def test_get_bool_reads_bool_overlay_as_a_signal_directly():
    schema = _typed_schema()
    candle = _typed_candle()
    signal = ta.get_bool(schema, "risk_on")

    signal.update(ta.Atom(candle, ta.OverlayInfo(schema, [0.0, True, "bull"])))
    assert signal.is_true() is True
    signal.update(ta.Atom(candle, ta.OverlayInfo(schema, [0.0, False, "bull"])))
    assert signal.is_true() is False


def test_str_source_returns_none_on_bare_candle():
    """A `Str`-typed reader has nothing to yield when the atom has no
    overlays — matches the Real/Bool readers' behaviour."""
    schema = _typed_schema()
    src = ta.get_str(schema, "regime")
    assert src.update(_typed_candle()) is None


# ---------------------------------------------------------------------------
# unstable() as a fluent method (parity with Rust's IndicatorExt/BoolIndicatorExt)
# ---------------------------------------------------------------------------


def test_indicator_unstable_method_matches_free_function():
    src = ta.ema(ta.close(), 5)
    warm = src.warm_up_bars()
    settle = src.unstable_bars()
    assert settle > 0
    m = src.unstable()
    f = ta.unstable(ta.ema(ta.close(), 5))
    assert isinstance(m, ta.Indicator)
    assert isinstance(f, ta.Indicator)
    assert m.warm_up_bars() == warm
    assert m.unstable_bars() == 0
    assert m.stable_bars() == warm
    # Method and free-function forms are the same wrapper.
    assert f.warm_up_bars() == warm
    assert f.unstable_bars() == 0


def test_signal_unstable_method_matches_free_function():
    entry = ta.close().crosses_above(ta.ema(ta.close(), 3))
    warm = entry.warm_up_bars()
    m = entry.unstable()
    f = ta.unstable(ta.close().crosses_above(ta.ema(ta.close(), 3)))
    assert m.warm_up_bars() == warm
    assert m.unstable_bars() == 0
    assert f.warm_up_bars() == warm
    assert f.unstable_bars() == 0
    # The wrappers pass through — same boolean state per bar as the plain entry.
    plain = ta.close().crosses_above(ta.ema(ta.close(), 3))
    bars = closes([float(i + 1) for i in range(warm + 5)])
    for c in bars:
        assert m.update(c) == plain.update(c)


# ---------------------------------------------------------------------------
# fugazi.metrics submodule (parity with fugazi::metrics)
# ---------------------------------------------------------------------------


def test_metrics_submodule_is_importable():
    from fugazi import metrics

    assert metrics.sharpe is not None
    assert metrics.Trade is not None
    assert metrics.DrawdownSegment is not None


def test_per_bar_returns_and_total_return():
    from fugazi import metrics

    eq = [100.0, 105.0, 110.0, 121.0]
    rets = metrics.per_bar_returns(eq, 100.0)
    # Per-bar returns are seeded from initial_equity, so bar 0 = (100-100)/100 = 0.
    assert rets == pytest.approx([0.0, 0.05, 5.0 / 105.0, 11.0 / 110.0])
    assert metrics.total_return(eq, 100.0) == pytest.approx(0.21)
    assert metrics.cagr(eq, 100.0, 252.0) > 1.0


def test_sharpe_and_sortino_return_none_on_zero_variance():
    from fugazi import metrics

    flat = [0.0] * 20
    assert metrics.sharpe(flat, 0.0, 252.0) is None
    assert metrics.sortino(flat, 0.0, 252.0) is None


def test_probabilistic_and_deflated_sharpe():
    from fugazi import metrics

    returns = [0.010 if i % 2 == 0 else -0.008 for i in range(200)]
    observed = metrics.sharpe(returns, 0.0, 252.0)
    assert observed is not None

    # PSR at benchmark == observed Sharpe puts the z-stat at 0 → 0.5.
    psr_at_observed = metrics.probabilistic_sharpe(returns, 0.0, 252.0, observed)
    assert psr_at_observed == pytest.approx(0.5, abs=1e-9)

    psr_at_zero = metrics.probabilistic_sharpe(returns, 0.0, 252.0, 0.0)
    assert 0.0 <= psr_at_zero <= 1.0
    # Selecting from many candidates → higher benchmark → strictly lower DSR.
    dsr = metrics.deflated_sharpe(returns, 0.0, 252.0, 50, 0.25)
    assert dsr is not None and dsr < psr_at_zero

    # Degenerate: no selection, or non-positive trial variance.
    assert metrics.deflated_sharpe(returns, 0.0, 252.0, 1, 0.25) is None
    assert metrics.deflated_sharpe(returns, 0.0, 252.0, 50, 0.0) is None
    assert metrics.probabilistic_sharpe([0.0] * 20, 0.0, 252.0, 0.0) is None


def test_expected_max_sharpe():
    from fugazi import metrics

    returns = [0.010 if i % 2 == 0 else -0.008 for i in range(200)]

    # The benchmark DSR tests against, readable on its own — and testing PSR
    # against it by hand must land exactly on DSR.
    sr0 = metrics.expected_max_sharpe(50, 0.25)
    assert sr0 == pytest.approx(1.138151546710174, abs=1e-12)
    assert metrics.deflated_sharpe(returns, 0.0, 252.0, 50, 0.25) == (
        metrics.probabilistic_sharpe(returns, 0.0, 252.0, sr0)
    )

    # Linear in the trial dispersion, increasing in the trial count.
    assert metrics.expected_max_sharpe(100, 0.25) == pytest.approx(
        metrics.expected_max_sharpe(100, 1.0) / 2.0, abs=1e-12
    )
    assert metrics.expected_max_sharpe(1000, 0.25) > metrics.expected_max_sharpe(
        10, 0.25
    )

    # Same `None` domain as `deflated_sharpe`: no maximum over one trial, no
    # null to beat without dispersion.
    assert metrics.expected_max_sharpe(1, 0.25) is None
    assert metrics.expected_max_sharpe(50, 0.0) is None
    assert metrics.expected_max_sharpe(50, -0.1) is None


def test_drawdown_pipeline():
    from fugazi import metrics

    equity = [100.0, 110.0, 105.0, 90.0, 95.0, 120.0, 100.0]
    segs = metrics.drawdown_segments(equity)
    assert len(segs) == 2
    assert isinstance(segs[0], metrics.DrawdownSegment)
    assert metrics.max_drawdown(segs) == pytest.approx((110.0 - 90.0) / 110.0)
    # Longest recovery, not the deepest drop's fall: segment 0 is underwater for
    # bars 2, 3 and 4. Mirrors `drawdown_segments_cover_multiple_stretches`.
    assert metrics.max_drawdown_duration(segs) == 3
    assert metrics.average_drawdown(segs) is not None
    assert metrics.time_in_drawdown_ratio(segs, 7) == pytest.approx(4.0 / 7.0)
    assert metrics.recovery_factor(equity, 100.0) is not None


def test_reconstruct_trades_round_trip_through_wallet():
    """Fill(bar, order) built from PaperWallet.update() feeds metrics cleanly."""
    from fugazi import metrics

    w = ta.PaperWallet(1000.0)
    fills = []
    w.update("BTC", 100.0)  # prime the wallet with a price for pre-flight
    w.set_position("BTC", 1.0)  # queue market buy
    for i, price in enumerate([100.0, 110.0]):
        for o in w.update("BTC", price):
            fills.append(ta.Fill(bar=i, order=o))
        if i == 0:
            w.close("BTC")  # queue flatten for the next bar
    assert len(fills) == 2
    trades = metrics.reconstruct_trades(fills)
    assert metrics.total_trades(trades) == 1
    assert trades[0].pnl == pytest.approx(10.0)
    assert trades[0].bars_held == 1
    assert metrics.win_rate(trades) == 1.0
    assert metrics.profit_factor(trades) is None  # no losing trade
    assert metrics.average_bars_held(trades) == pytest.approx(1.0)
    assert metrics.exposure_ratio(fills, total_bars=2) == pytest.approx(0.5)


def test_single_strategy_rebalance_on_resizes_the_open_position():
    """Mirrors SingleAssetStrategy::rebalance_on — off by default, resizes on fire."""
    prices = [100.0 + i for i in range(20)]
    always = ta.close().above(0.0)
    strat = ta.Strategy("BTC").long_on(always).position_sizing(ta.value(0.5))

    base = strat.run(ta.PaperWallet(10_000.0), _ohlcv(prices))
    gated = strat.rebalance_on(ta.close().above(0.0)).run(
        ta.PaperWallet(10_000.0), _ohlcv(prices)
    )
    # Ungated: sized once at entry, then the position drifts with P&L.
    assert len(base.fills) == 1
    # Gated: re-sized to the half-equity target as equity moves.
    assert len(gated.fills) > len(base.fills)


def test_order_is_constructible_and_feeds_metrics():
    """Stored fills — ones no wallet in this process produced — go back in."""
    from fugazi import metrics

    # The blotter as a consumer would have persisted it and read it back.
    rows = [(0, "buy", 1.0, 100.0), (1, "sell", 1.0, 110.0)]
    fills = [
        ta.Fill(bar=bar, order=ta.Order(symbol="BTC", side=side, units=u, price=p))
        for bar, side, u, p in rows
    ]
    trades = metrics.reconstruct_trades(fills)
    assert metrics.total_trades(trades) == 1
    assert trades[0].pnl == pytest.approx(10.0)
    assert metrics.exposure_ratio(fills, total_bars=2) == pytest.approx(0.5)


def test_reconstruct_trades_keeps_symbols_apart():
    """A multi-symbol blotter reconstructs one leg per symbol, never across.

    Through 0.63.1 this walked every fill with a single shared position, so
    BBB's sell "closed" AAA's long and P&L subtracted one asset's price from
    another's — three trades out of these four fills, including a -4500 loss
    that never happened.
    """
    from fugazi import metrics

    rows = [
        (1, "AAA", "buy", 50.0, 100.0),
        (1, "BBB", "sell", 500.0, 10.0),
        (5, "BBB", "buy", 500.0, 9.0),
        (5, "AAA", "sell", 50.0, 110.0),
    ]
    fills = [
        ta.Fill(bar=bar, order=ta.Order(symbol=sym, side=side, units=u, price=p))
        for bar, sym, side, u, p in rows
    ]
    trades = metrics.reconstruct_trades(fills)

    assert metrics.total_trades(trades) == 2
    # Emitted in closing order, so BBB's short leads.
    short, long_ = trades
    assert (short.entry_price, short.exit_price) == pytest.approx((10.0, 9.0))
    assert (long_.entry_price, long_.exit_price) == pytest.approx((100.0, 110.0))
    # Both legs win; the fabricated pairing was the only loser.
    assert all(t.pnl > 0 for t in trades)
    assert metrics.win_rate(trades) == pytest.approx(1.0)


def test_order_constructor_defaults_and_round_trips():
    o = ta.Order(symbol="BTC", side="buy", units=2.0, price=50.0)
    assert (o.symbol, o.side, o.units, o.price) == ("BTC", "buy", 2.0, 50.0)
    assert o.kind == "market" and o.id == 0 and o.commission == 0.0
    assert o.signed_units == 2.0
    # Every field a getter reports is a field the constructor accepts back.
    full = ta.Order(
        symbol="ETH",
        side="sell",
        units=1.5,
        price=20.0,
        kind="stop",
        id=7,
        commission=0.25,
    )
    again = ta.Order(
        symbol=full.symbol,
        side=full.side,
        units=full.units,
        price=full.price,
        kind=full.kind,
        id=full.id,
        commission=full.commission,
    )
    assert repr(again) == repr(full)
    assert again.signed_units == -1.5
    assert "commission=0.25" in repr(full)


def test_order_constructor_rejects_bad_side_and_kind():
    with pytest.raises(ValueError):
        ta.Order(symbol="BTC", side="hold", units=1.0, price=1.0)
    with pytest.raises(ValueError):
        ta.Order(symbol="BTC", side="buy", units=1.0, price=1.0, kind="iceberg")


def test_wallet_fills_expose_commission():
    """The commission leg is readable — zero on an uncosted wallet, not absent."""
    w = ta.PaperWallet(1000.0)
    w.update("BTC", 100.0)
    w.set_position("BTC", 1.0)
    fills = w.update("BTC", 100.0)
    assert len(fills) == 1
    assert fills[0].commission == 0.0


def test_set_costs_for_stamps_commission_on_fills():
    """A costed wallet charges the fill and reports what it charged."""
    w = ta.PaperWallet(1000.0)
    w.set_costs_for("BTC", {"commission": {"percentage": {"rate": 0.001}}})
    w.update("BTC", 100.0)
    w.set_position("BTC", 1.0)
    fills = w.update("BTC", 100.0)
    assert len(fills) == 1
    assert fills[0].commission == pytest.approx(0.1)  # 1 unit * 100 * 0.1%
    # The charge came out of the account, not just the record.
    assert w.funds == pytest.approx(1000.0 - 100.0 - 0.1)


def test_set_costs_for_accepts_a_config_object_and_a_frequency():
    costs = ta.TradingCostsConfig({"commission": {"percentage": {"rate": 0.002}}})
    w = ta.PaperWallet(1000.0)
    w.set_costs_for("BTC", costs, freq="1d")
    w.set_costs_for("ETH", costs, freq=ta.Frequency("4h"))
    w.update("BTC", 100.0)
    w.set_position("BTC", 1.0)
    assert w.update("BTC", 100.0)[0].commission == pytest.approx(0.2)


def test_set_costs_for_is_per_symbol():
    """Only the symbol it was installed for pays."""
    w = ta.PaperWallet(10_000.0)
    w.set_costs_for("BTC", {"commission": {"percentage": {"rate": 0.01}}})
    for sym in ("BTC", "ETH"):
        w.update(sym, 100.0)
        w.set_position(sym, 1.0)
    charged = {f.symbol: f.commission for f in w.update("BTC", 100.0)}
    charged.update({f.symbol: f.commission for f in w.update("ETH", 100.0)})
    assert charged["BTC"] == pytest.approx(1.0)
    assert charged["ETH"] == 0.0


def test_every_is_a_delayed_periodic_pulse():
    """ta.every(N) mirrors !every N: fires on bar N-1, then every N bars."""
    pulse = ta.every(3)
    fired = [bool(pulse.update(ta.Candle(1.0, 1.0, 1.0, 1.0, 1.0))) for _ in range(9)]
    assert fired == [False, False, True, False, False, True, False, False, True]
    with pytest.raises(ValueError):
        ta.every(0)


def test_every_gates_a_strategy_rebalance():
    """The pulse lifts into a snapshot-rooted rebalance slot on every shape."""
    prices = [100.0 + i for i in range(20)]
    strat = (
        ta.Strategy("BTC").long_on(ta.close().above(0.0)).position_sizing(ta.value(0.5))
    )
    often = strat.rebalance_on(ta.every(1)).run(
        ta.PaperWallet(10_000.0), _ohlcv(prices)
    )
    rarely = strat.rebalance_on(ta.every(10)).run(
        ta.PaperWallet(10_000.0), _ohlcv(prices)
    )
    assert len(often.fills) > len(rarely.fills) >= 1


def test_evaluate_report_from_a_bare_equity_curve():
    """A curve no run() produced reduces to the same tree evaluate() returns."""
    from fugazi import metrics

    curve = [101.0, 103.0, 99.0, 105.0, 108.0]
    report = ta.RunReport(equity_curve=curve, initial_equity=100.0)
    assert report.equity_curve == curve
    assert report.initial_equity == 100.0
    assert report.fills == [] and report.rejections == []

    m = ta.evaluate_report(report, bars_per_year=252.0)
    # Same keys, same values as calling the individual metrics by hand.
    returns = metrics.per_bar_returns(curve, 100.0)
    segments = metrics.drawdown_segments(curve)
    assert m["run"]["bars"] == 5
    assert m["run"]["initial_equity"] == 100.0
    assert m["run"]["final_equity"] == 108.0
    assert m["returns"]["total"] == pytest.approx(metrics.total_return(curve, 100.0))
    assert m["drawdown"]["max"] == pytest.approx(metrics.max_drawdown(segments))
    assert m["risk_adjusted"]["sharpe"] == pytest.approx(
        metrics.sharpe(returns, 0.0, 252.0)
    )
    # No fills — the trades section reads as a run that never traded.
    assert m["trades"]["total"] == 0


def test_evaluate_report_round_trips_a_real_run():
    """A report rebuilt from its own parts reduces to the same metric tree."""
    from fugazi import metrics

    enter = ta.sma(ta.close(), 2).crosses_above(ta.sma(ta.close(), 4))
    down = ta.sma(ta.close(), 2).crosses_below(ta.sma(ta.close(), 4))
    strat = ta.Strategy("BTC").long_on(enter, down).short_on(down, enter)
    prices = [14, 13, 12, 11, 10, 11, 13, 15, 17, 15, 12, 9, 7, 9, 12, 15]
    report = strat.run(ta.PaperWallet(10_000.0), _ohlcv(prices))
    assert len(report.fills) >= 1

    direct = ta.evaluate_report(report, bars_per_year=252.0)
    rebuilt = ta.RunReport(
        equity_curve=report.equity_curve,
        initial_equity=report.initial_equity,
        fills=report.fills,
    )
    assert ta.evaluate_report(rebuilt, bars_per_year=252.0) == direct
    # Fills carried through, so the trades section is populated — the whole
    # point of accepting them rather than only a bare curve.
    assert direct["trades"]["total"] == metrics.total_trades(
        metrics.reconstruct_trades(report.fills)
    )
    assert direct["trades"]["total"] >= 1


def test_a_ruined_run_is_terminal_and_says_so():
    """Ruin crosses the FFI as a run outcome, not as an inferrable blank cell.

    A short held into a rally is the shortest path to insolvency: the loss is
    unbounded above, so no leverage knob or cost model is involved. Everything
    asserted here is a consequence of `backtest::run` pinning the curve at zero
    — Python adds no logic of its own, which is the parity claim.
    """
    enter = ta.close().gt(ta.value(-1.0))  # always true
    never = ta.close().lt(ta.value(-1.0))  # never true
    strat = ta.Strategy("X").short_on(enter, never)
    prices = [100, 100, 150, 260, 320, 400, 450, 500, 600]
    report = strat.run(ta.PaperWallet(10_000.0), _ohlcv(prices))

    ruin = report.ruin_bar
    assert ruin is not None, f"a fully-invested short into a 6x rise is ruin: {report}"
    assert "ruin_bar" in repr(report)

    # One entry per bar still, pinned at zero from ruin on, and nothing traded
    # after it.
    assert len(report.equity_curve) == len(prices)
    assert all(e > 0.0 for e in report.equity_curve[:ruin])
    assert all(e == 0.0 for e in report.equity_curve[ruin:])
    assert all(f.bar <= ruin for f in report.fills)

    m = ta.evaluate_report(report, bars_per_year=252.0)
    assert m["run"]["ruin_bar"] == ruin
    assert m["run"]["final_equity"] == 0.0
    assert m["returns"]["total_pct"] == pytest.approx(-100.0)
    # The two numbers the defect made meaningless: a >100% drawdown, and a CAGR
    # that vanished instead of reading -100%.
    assert m["drawdown"]["max_pct"] == pytest.approx(100.0)
    assert m["returns"]["cagr_pct"] == pytest.approx(-100.0)

    # A hand-built report carries the field too, so a caller reconstructing one
    # from a live account's curve can say the account was wiped out.
    rebuilt = ta.RunReport(
        equity_curve=report.equity_curve,
        initial_equity=report.initial_equity,
        fills=report.fills,
        ruin_bar=ruin,
    )
    assert rebuilt.ruin_bar == ruin
    assert ta.evaluate_report(rebuilt, bars_per_year=252.0) == m


def test_a_solvent_run_reports_no_ruin():
    """The default, and the constraint: nothing changes for a run that lived."""
    enter = ta.sma(ta.close(), 2).crosses_above(ta.sma(ta.close(), 4))
    down = ta.sma(ta.close(), 2).crosses_below(ta.sma(ta.close(), 4))
    strat = ta.Strategy("BTC").long_on(enter, down)
    report = strat.run(
        ta.PaperWallet(10_000.0), _ohlcv([14, 13, 12, 11, 10, 11, 13, 15])
    )
    assert report.ruin_bar is None
    # Absent from the document entirely, so `run.ruin_bar` present == ruined.
    assert "ruin_bar" not in ta.evaluate_report(report, bars_per_year=252.0)["run"]
    # A bare curve defaults to solvent without the caller passing anything.
    assert (
        ta.RunReport(equity_curve=[100.0, 101.0], initial_equity=100.0).ruin_bar is None
    )


def test_trade_and_drawdown_segment_are_frozen_readonly():
    from fugazi import metrics

    seg = metrics.drawdown_segments([100.0, 90.0, 100.0])[0]
    with pytest.raises(AttributeError):
        seg.depth_ratio = 0.0  # frozen


def test_fill_has_bar_and_order_getters():
    w = ta.PaperWallet(1000.0)
    w.update("BTC", 100.0)  # prime the wallet with a price for pre-flight
    w.set_position("BTC", 1.0)  # queued
    fills = w.update("BTC", 100.0)  # fills at the next update's open
    assert len(fills) == 1
    f = ta.Fill(bar=42, order=fills[0])
    assert f.bar == 42
    assert f.order.symbol == "BTC"
    assert f.order.side == "buy"


# ---------------------------------------------------------------------------
# Atom equality-by-time and ordering.
# ---------------------------------------------------------------------------


def _atom(ms=None, close=1.0):
    return ta.Atom(ta.Candle(1.0, 2.0, 0.5, close, 100.0), time=ms)


def test_atom_equality_is_by_time():
    # Two atoms with the same bar-open time are equal regardless of prices.
    assert _atom(ms=1_000_000, close=1.0) == _atom(ms=1_000_000, close=9999.0)
    # Different times → not equal.
    assert _atom(ms=1_000_000) != _atom(ms=1_000_001)
    # Undated atoms compare equal to each other (None == None convention).
    assert _atom(ms=None) == _atom(ms=None, close=42.0)
    # An atom compared to any non-Atom is not-equal (no crash).
    assert (_atom(ms=1) == "not an atom") is False


def test_atom_orders_chronologically():
    unsorted = [
        _atom(ms=200),
        _atom(ms=None),  # None sorts first (like Option's derived order)
        _atom(ms=100),
        _atom(ms=300),
    ]
    times = [a.time for a in sorted(unsorted)]
    assert times == [None, 100, 200, 300]


def test_atom_is_hashable_by_time():
    # Hashable → usable in sets/dicts; two atoms at the same time collide.
    s = {_atom(ms=1), _atom(ms=2), _atom(ms=1, close=99.0)}
    assert len(s) == 2


# ---------------------------------------------------------------------------
# Snapshot dict-like surface.
# ---------------------------------------------------------------------------


def test_snapshot_dict_like_operations():
    snap = ta.Snapshot()
    assert len(snap) == 0
    assert snap.is_empty()

    btc = _atom(ms=1_000, close=100.0)
    eth = _atom(ms=1_000, close=50.0)
    snap["BTC"] = btc
    snap["ETH"] = eth
    assert len(snap) == 2
    assert not snap.is_empty()
    assert "BTC" in snap
    assert "SOL" not in snap
    # Keys are Selectors now; a bare str is coerced to Selector.by_symbol.
    assert set(snap.keys()) == {ta.Selector(symbol="BTC"), ta.Selector(symbol="ETH")}
    assert snap["BTC"].candle.close == 100.0
    assert snap.get("SOL") is None


def test_snapshot_iterates_its_keys():
    """Regression: with `__len__` + `__getitem__` and no `__iter__`, `list(snap)`
    fell into Python's legacy sequence protocol and probed `snap[0]`, which
    `coerce_selector` rejected — so iterating reported a *key type* error for
    something the caller never asked to index."""
    snap = ta.Snapshot(
        {"BTC": _atom(ms=1, close=100.0), "ETH": _atom(ms=1, close=50.0)}
    )
    assert list(snap) == snap.keys()
    assert [snap[k].candle.close for k in snap] == [100.0, 50.0]
    assert [a.candle.close for a in snap.values()] == [100.0, 50.0]
    assert {k.symbol: a.candle.close for k, a in snap.items()} == {
        "BTC": 100.0,
        "ETH": 50.0,
    }


def test_schema_iterates_its_column_names():
    b = ta.SchemaBuilder()
    b.add_real("funding")
    b.add_bool("halted")
    schema = b.finish()
    assert list(schema) == schema.keys() == ["funding", "halted"]
    assert [k for k in schema if k in schema] == ["funding", "halted"]


def test_snapshot_construct_from_mapping():
    # Both a dict of Atom and a dict of Candle work (candle → atom lifted).
    snap = ta.Snapshot(
        {"BTC": _atom(ms=1, close=100.0), "ETH": ta.Candle(1, 2, 0.5, 50, 1)}
    )
    assert snap["BTC"].candle.close == 100.0
    assert snap["ETH"].candle.close == 50.0


def test_snapshot_missing_key_raises():
    snap = ta.Snapshot({"BTC": _atom(ms=1)})
    with pytest.raises(KeyError):
        _ = snap["ETH"]


# ---------------------------------------------------------------------------
# Pick + cross-asset composition.
# ---------------------------------------------------------------------------


def _snap(pairs, ms=None):
    return ta.Snapshot({k: _atom(ms=ms, close=v) for k, v in pairs.items()})


def test_pick_projects_named_asset():
    btc_close = ta.close(source=ta.pick("BTC"))
    out = btc_close.update(_snap({"BTC": 100.0, "ETH": 50.0}))
    assert out == pytest.approx(100.0)


def test_pick_dict_input_works_like_snapshot():
    # A plain dict[str, Atom|Candle] is auto-lifted into a Snapshot on the fly.
    btc_close = ta.close(source=ta.pick("BTC"))
    out = btc_close.update(
        {"BTC": _atom(ms=1, close=42.0), "ETH": _atom(ms=1, close=0.0)}
    )
    assert out == pytest.approx(42.0)


def test_btc_eth_close_spread():
    # The headline expression: BTC/ETH close spread as a first-class indicator.
    spread = ta.close(ta.pick("BTC")) - ta.close(ta.pick("ETH"))
    out = spread.update(_snap({"BTC": 100.0, "ETH": 60.0}))
    assert out == pytest.approx(40.0)


def test_missing_asset_yields_none():
    spread = ta.close(ta.pick("BTC")) - ta.close(ta.pick("ETH"))
    # BTC missing → both sides can't unify → None.
    assert spread.update(_snap({"ETH": 60.0})) is None


def test_ema_over_pick_composes():
    # An EMA over BTC's close reads the projected close each bar.
    node = ta.ema(ta.close(source=ta.pick("BTC")), 2)
    snaps = [_snap({"BTC": v, "ETH": 100.0}) for v in [10.0, 11.0, 12.0, 13.0]]
    outs = [node.update(s) for s in snaps]
    # EMA seeds on the first bar the source emits Some (source's warm-up = 1),
    # so every output is Some(finite float) — but the value drifts from the
    # naive close toward the smoothed one over subsequent bars.
    assert all(o is not None and math.isfinite(o) for o in outs)
    assert outs[0] == pytest.approx(10.0)
    # By bar 4 the smoothed value has moved past the seed toward the newer bars.
    assert outs[-1] > outs[0]


def test_calendar_over_pick_reads_projected_time():
    year_of_btc = ta.year(source=ta.pick("BTC"))
    # 2024-03-15 12:00 UTC.
    ms = 1_710_504_000_000
    out = year_of_btc.update(_snap({"BTC": 100.0}, ms=ms))
    assert out == pytest.approx(2024.0)


def test_cross_domain_mismatch_is_typeerror():
    # Snapshot-rooted + candle-rooted can't be combined; the domain seams error.
    snap_side = ta.close(ta.pick("BTC"))
    candle_side = ta.close()
    with pytest.raises(TypeError):
        _ = snap_side + candle_side


def test_atom_source_metadata():
    src = ta.pick("BTC")
    assert src.warm_up_bars() == 1
    assert src.unstable_bars() == 0
    assert src.stable_bars() == 1
    assert src.value() is None
    src.update(_snap({"BTC": 100.0}))
    assert src.value() is not None
    assert src.value().candle.close == 100.0
    src.reset()
    assert src.value() is None


# ---------------------------------------------------------------------------
# Frequency + Selector construction and coercion.
# ---------------------------------------------------------------------------


def test_frequency_roundtrip():
    assert str(ta.Frequency("1h")) == "1h"
    assert str(ta.Frequency("15m")) == "15m"
    assert str(ta.Frequency("1M")) == "1M"


def test_frequency_orders_by_duration_not_variant():
    # 120 minutes > 1 hour — total order is by seconds-per-bar, not variant tag.
    assert ta.Frequency("120m") > ta.Frequency("1h")
    assert ta.Frequency("1d") > ta.Frequency("24h") or ta.Frequency(
        "1d"
    ) == ta.Frequency("24h")


def test_frequency_rejects_bad_tokens():
    with pytest.raises(ValueError):
        ta.Frequency("garbage")
    with pytest.raises(ValueError):
        ta.Frequency("0h")


def test_selector_construction_forms():
    # Everything's optional; the empty selector is legal and stands for the
    # no-query single-entry unpack.
    assert ta.Selector().is_empty()
    assert ta.Selector(symbol="BTC").symbol == "BTC"
    assert ta.Selector(symbol="BTC").stream is None
    assert ta.Selector(stream="1h").symbol is None
    assert ta.Selector(stream="1h").stream == "1h"
    assert ta.Selector(symbol="BTC", stream="1h").symbol == "BTC"
    # `stream` accepts a Frequency instance too — a cadence is one spelling of a
    # stream id, and it is stored as its token.
    assert ta.Selector(stream=ta.Frequency("1h")).stream == "1h"
    # And an id that is not a duration at all, which is the point of the type.
    assert ta.Selector(symbol="BTC", stream="dollar-1e6").stream == "dollar-1e6"


def test_selector_matches_wildcard_semantics():
    query = ta.Selector(symbol="BTC")  # stream is a wildcard
    assert query.matches(ta.Selector(symbol="BTC", stream="1h"))
    assert query.matches(ta.Selector(symbol="BTC"))
    assert not query.matches(ta.Selector(symbol="ETH", stream="1h"))
    # An empty query matches every storage entry.
    empty = ta.Selector()
    assert empty.matches(ta.Selector(symbol="BTC"))
    assert empty.matches(ta.Selector(symbol="ETH", stream="1d"))
    # A stream query discriminates two streams of one symbol — the case a
    # closed enum of durations could not express.
    dollars = ta.Selector(symbol="BTC", stream="dollar-1e6")
    assert dollars.matches(ta.Selector(symbol="BTC", stream="dollar-1e6"))
    assert not dollars.matches(ta.Selector(symbol="BTC", stream="1d"))


def test_snapshot_accepts_selector_keys():
    snap = ta.Snapshot(
        {
            ta.Selector(symbol="BTC", stream="1h"): _atom(ms=1, close=100.0),
            ta.Selector(symbol="BTC", stream="1d"): _atom(ms=1, close=300.0),
        }
    )
    # Exact lookup disambiguates.
    exact = snap[ta.Selector(symbol="BTC", stream="1h")]
    assert exact.candle.close == 100.0


def test_snapshot_find_wildcards_over_freq():
    snap = ta.Snapshot(
        {
            ta.Selector(symbol="BTC", stream="1h"): _atom(ms=1, close=100.0),
            ta.Selector(symbol="ETH", stream="1h"): _atom(ms=1, close=50.0),
        }
    )
    # A symbol-only query wildcards freq — finds the BTC entry.
    hit = snap.find(ta.Selector(symbol="BTC"))
    assert hit is not None
    assert hit.candle.close == 100.0


def test_snapshot_tuple_key_coerces_to_selector():
    snap = ta.Snapshot()
    snap[("BTC", "1h")] = _atom(ms=1, close=100.0)
    # Round-tripped through Selector::exact.
    assert snap[ta.Selector(symbol="BTC", stream="1h")].candle.close == 100.0


def test_pick_no_query_unpacks_single_entry_snapshot():
    # Single-series ergonomics: `ta.pick()` with no args reads the sole atom.
    close = ta.close(source=ta.pick())
    snap = ta.Snapshot({"BTC": _atom(ms=1, close=42.0)})
    assert close.update(snap) == pytest.approx(42.0)


def test_pick_no_query_none_on_empty_snapshot():
    close = ta.close(source=ta.pick())
    assert close.update(ta.Snapshot()) is None


def test_pick_no_query_raises_on_multi_entry_snapshot():
    # A no-query pick fed a multi-asset snapshot is a wiring bug: loud failure
    # (Rust panic surfaced as a Python RuntimeError from PyO3).
    close = ta.close(source=ta.pick())
    snap = ta.Snapshot(
        {"BTC": _atom(ms=1, close=100.0), "ETH": _atom(ms=1, close=60.0)}
    )
    with pytest.raises(BaseException):  # pyo3 panic surfaces as PanicException
        close.update(snap)


def test_pick_by_freq_wildcards_symbol():
    # A snapshot keyed by (symbol, freq); a freq-only pick reads the first
    # matching entry irrespective of symbol.
    snap = ta.Snapshot(
        {
            ("BTC", "1h"): _atom(ms=1, close=100.0),
            ("ETH", "1d"): _atom(ms=1, close=50.0),
        }
    )
    hourly = ta.close(source=ta.pick(freq="1h"))
    assert hourly.update(snap) == pytest.approx(100.0)


def test_pick_exact_disambiguates_between_frequencies():
    snap = ta.Snapshot(
        {
            ("BTC", "1h"): _atom(ms=1, close=100.0),
            ("BTC", "1d"): _atom(ms=1, close=300.0),
        }
    )
    hourly = ta.close(source=ta.pick(symbol="BTC", freq="1h"))
    daily = ta.close(source=ta.pick(symbol="BTC", freq="1d"))
    assert hourly.update(snap) == pytest.approx(100.0)
    assert daily.update(snap) == pytest.approx(300.0)


# --- get(source=) : cross-series overlay reads ------------------------------


def _overlay_schema():
    b = ta.SchemaBuilder()
    b.add_real("val")
    b.add_bool("flag")
    b.add_str("regime")
    return b.finish()


def _overlay_atom(schema, px, val=None, flag=None, regime=None):
    return ta.Atom(
        ta.Candle(px, px, px, px, 1.0), ta.OverlayInfo(schema, [val, flag, regime])
    )


def _two_symbol_snaps(schema, n=4):
    return [
        ta.Snapshot(
            {
                "T": _overlay_atom(schema, 100 + i, val=0.0, flag=False, regime="flat"),
                "M": _overlay_atom(
                    schema, 50 + i, val=1.5 + i, flag=True, regime="bull"
                ),
            }
        )
        for i in range(n)
    ]


def test_get_source_reads_another_series_overlay_column():
    # The gap this closes: `close(source=pick(...))` could read another series'
    # candle fields, but `get` had no way to reach its overlay columns.
    schema = _overlay_schema()
    g = ta.get(schema, "val", source=ta.pick("M"))
    assert [g.update(s) for s in _two_symbol_snaps(schema)] == [1.5, 2.5, 3.5, 4.5]


def test_get_without_source_is_unchanged():
    schema = _overlay_schema()
    assert ta.get_real(schema, "val").update(_overlay_atom(schema, 1.0, val=9.0)) == 9.0


def test_get_source_preserves_type_polymorphism():
    schema = _overlay_schema()
    snap = _two_symbol_snaps(schema)[0]
    assert isinstance(ta.get(schema, "val", source=ta.pick("M")), ta.Indicator)
    assert isinstance(ta.get(schema, "flag", source=ta.pick("M")), ta.Signal)
    assert isinstance(ta.get(schema, "regime", source=ta.pick("M")), ta.StrSource)
    assert ta.get(schema, "flag", source=ta.pick("M")).update(snap) is True
    assert ta.get(schema, "regime", source=ta.pick("M")).update(snap) == "bull"


def test_str_eq_composes_over_a_sourced_str_column():
    # A snapshot-rooted Str source has to be comparable, or reading a `regime`
    # column cross-series would be useless. The literal adopts its partner's
    # domain, same as on the Real side.
    schema = _overlay_schema()
    sig = ta.str_eq(ta.get_str(schema, "regime", source=ta.pick("M")), "bull")
    assert sig.update(_two_symbol_snaps(schema)[0]) is True


def test_get_source_yielding_an_overlayless_atom_reads_none():
    # Not a panic: a bare Candle has no overlay side channel at all.
    schema = _overlay_schema()
    bare = ta.Snapshot({"M": ta.Candle(1.0, 1.0, 1.0, 1.0, 1.0)})
    assert ta.get(schema, "val", source=ta.pick("M")).update(bare) is None


def test_get_source_still_rejects_an_unknown_key():
    schema = _overlay_schema()
    for ctor in (ta.get, ta.get_real):
        with pytest.raises(ValueError, match="unknown overlay key"):
            ctor(schema, "nope", source=ta.pick("M"))


# --- CoinGecko (price-less / overlay series) ------------------------------
#
# The live fetch is exercised by the README block (test_readme.py, which skips
# on an HTTP error). Everything here is offline: each guard rejects the call
# before any request goes out.


def test_coingecko_constructs():
    assert ta.CoinGecko() is not None
    assert ta.CoinGecko(api_key="demo", vs_currency="eur") is not None


def test_yahoo_constructs_with_adjusted_param():
    # Candles are split/dividend-adjusted by default; `adjusted=False` opts out.
    # Construction is offline — no fetch — so this just proves the knob is wired.
    assert ta.Yahoo() is not None
    assert ta.Yahoo(adjusted=False) is not None
    assert ta.Yahoo(adjusted=True, base_url="http://localhost:0") is not None


def test_fetch_accepts_coingecko():
    # Every provider shares one `SeriesSource` now, so `fetch()` dispatches cg
    # like any other — a price-less frame is fine (the OHLCV block is omitted
    # when no row carries a bar). Proven offline: the sub-hourly guard fires
    # from inside CoinGecko, so reaching "unsupported interval" (not "unknown
    # provider") shows the call routed through rather than being rejected.
    with pytest.raises(ValueError, match="unsupported interval"):
        ta.fetch(provider="cg", symbol="bitcoin", freq="5m")


def test_coingecko_rejects_sub_hourly():
    # CoinGecko only samples that finely over windows too short to backtest on;
    # silently serving daily data for a "5m" request is the failure mode this
    # guard exists to prevent.
    with pytest.raises(ValueError, match="unsupported interval"):
        ta.CoinGecko().fetch(symbol="bitcoin", freq="5m", since="2026-07-08")


# --- BinanceVision (price-less / overlay series) --------------------------
#
# Offline guards only; the summing / bucketing behaviour is pinned by the
# wiremock suite in tests/sources_binance_vision.rs.


def test_binance_vision_constructs():
    assert ta.BinanceVision() is not None
    assert ta.BinanceVision(base_url="http://localhost:1") is not None


def test_fetch_advertises_both_binance_vision_ids():
    # The market rides in the provider id rather than a `market` kwarg, so the
    # flat fetch() carries both trees the way the CLI does.
    with pytest.raises(ValueError, match="binance-vision, binance-vision-futures"):
        ta.fetch(provider="nope", symbol="BTCUSDT")


def test_fetch_binance_vision_futures_id_routes_to_the_futures_tree():
    # Reaching the futures tree's own sub-hourly guard proves the id routed
    # there — spot has no such constraint, and an unknown id would have failed
    # earlier with "unknown provider".
    with pytest.raises(ValueError, match="unsupported interval"):
        ta.fetch(provider="binance-vision-futures", symbol="BTCUSDT", freq="15m")


def test_binance_vision_futures_rejects_sub_hourly():
    # Funding settles every 4-8h, so a 15m bucket is empty on almost every bar
    # — a column of zeros reading as "no carry" rather than "no data". Spot has
    # no such constraint: it is klines only, and Binance publishes those at
    # every cadence.
    with pytest.raises(ValueError, match="unsupported interval"):
        ta.BinanceVision("futures").fetch(
            symbol="BTCUSDT", freq="15m", since="2024-01-01"
        )


def test_binance_vision_market_must_be_spot_or_futures():
    with pytest.raises(ValueError, match="must be 'spot' or 'futures'"):
        ta.BinanceVision("perp")


# ---------------------------------------------------------------------------
# compute_overlays: derive overlay columns from indicator specs and attach
# them onto Atoms / Snapshots (the dataset "overlays" step).
# ---------------------------------------------------------------------------


def _bars(closes):
    """A list of overlay-free Atoms with the given close prices."""
    return [ta.Atom(ta.Candle(c, c, c, c, 1_000.0)) for c in closes]


def test_compute_overlays_real_column_atoms_yaml():
    atoms = _bars([10.0, 20.0, 30.0, 40.0])
    schema, out = ta.compute_overlays(atoms, "sma3: !sma { period: 3 }")

    assert schema.keys() == ["sma3"]
    i = schema.index_of("sma3")
    assert len(out) == 4
    # Warm-up bars read None (not a sentinel).
    assert out[0].overlays.get_real(i) is None
    assert out[1].overlays.get_real(i) is None
    # SMA(10, 20, 30) = 20, then SMA(20, 30, 40) = 30.
    assert out[2].overlays.get_real(i) == pytest.approx(20.0)
    assert out[3].overlays.get_real(i) == pytest.approx(30.0)


def test_compute_overlays_result_feeds_into_get():
    atoms = _bars([10.0, 20.0, 30.0, 40.0])
    schema, out = ta.compute_overlays(atoms, "sma3: !sma { period: 3 }")

    # A reader built against the *returned* schema reads the computed values.
    reader = ta.get(schema, "sma3")
    read = [reader.update(a) for a in out]
    assert read[0] is None and read[1] is None
    assert read[2] == pytest.approx(20.0)
    assert read[3] == pytest.approx(30.0)


def test_compute_overlays_str_column():
    atoms = _bars([1.0, 2.0])
    schema, out = ta.compute_overlays(atoms, "label: !value bull")
    assert schema.type_of_key("label") == "str"
    i = schema.index_of("label")
    assert out[0].overlays.get_str(i) == "bull"


def test_compute_overlays_bool_column_via_dict():
    atoms = _bars([10.0, 20.0, 30.0])
    schema, out = ta.compute_overlays(atoms, {"hot": ta.close().above(15.0)})
    assert schema.type_of_key("hot") == "bool"
    i = schema.index_of("hot")
    assert out[0].overlays.get_bool(i) is False
    assert out[1].overlays.get_bool(i) is True


def test_compute_overlays_merges_and_extends_existing():
    # Atoms already carry a `vol` overlay bound to one schema.
    existing = _schema("vol")
    atoms = []
    for k, c in enumerate([10.0, 20.0, 30.0, 40.0]):
        candle = ta.Candle(c, c, c, c, 1_000.0)
        atoms.append(ta.Atom(candle, ta.OverlayInfo(existing, [float(k)])))

    schema, out = ta.compute_overlays(atoms, "sma3: !sma { period: 3 }")
    assert schema.keys() == ["vol", "sma3"]

    vol_i = schema.index_of("vol")
    sma_i = schema.index_of("sma3")
    # Pre-existing column preserved & unchanged on every bar, including the
    # new column's warm-up bars.
    for k, a in enumerate(out):
        assert a.overlays.get_real(vol_i) == pytest.approx(float(k))
    assert out[0].overlays.get_real(sma_i) is None
    assert out[2].overlays.get_real(sma_i) == pytest.approx(20.0)


def test_compute_overlays_snapshots_multi_symbol():
    snaps = []
    btc = [10.0, 20.0, 30.0, 40.0]
    eth = [1.0, 2.0, 3.0, 4.0]
    for b, e in zip(btc, eth):
        snaps.append(
            ta.Snapshot(
                {
                    "BTC": ta.Atom(ta.Candle(b, b, b, b, 1.0)),
                    "ETH": ta.Atom(ta.Candle(e, e, e, e, 1.0)),
                }
            )
        )

    schema, out = ta.compute_overlays(snaps, "sma3: !sma { period: 3 }")
    i = schema.index_of("sma3")
    assert len(out) == 4

    # Each symbol carries its own-series SMA; they warm independently but here
    # both warm at bar index 2.
    assert out[2]["BTC"].overlays.get_real(i) == pytest.approx(20.0)
    assert out[2]["ETH"].overlays.get_real(i) == pytest.approx(2.0)
    assert out[1]["BTC"].overlays.get_real(i) is None


def test_compute_overlays_snapshots_resolve_cross_symbol_picks():
    # A `!pick { symbol: ... }` naming *another* series must resolve against the
    # real multi-symbol snapshot. This used to yield an empty column on every
    # bar — silently, and indistinguishably from a warming indicator.
    btc = [10.0, 20.0, 30.0, 40.0]
    eth = [100.0, 90.0, 80.0, 70.0]
    snaps = [
        ta.Snapshot(
            {
                "BTC": ta.Atom(ta.Candle(b, b, b, b, 1.0), time=i * 86_400_000),
                "ETH": ta.Atom(ta.Candle(e, e, e, e, 1.0), time=i * 86_400_000),
            }
        )
        for i, (b, e) in enumerate(zip(btc, eth))
    ]

    schema, out = ta.compute_overlays(
        snaps,
        """
        own: !close
        eth_close: !close { source: !pick { symbol: ETH } }
        spread: !sub { lhs: !close, rhs: !close { source: !pick { symbol: ETH } } }
        """,
    )

    def col(sym, name):
        i = schema.index_of(name)
        return [s.get(sym).overlays.get_real(i) for s in out]

    # A bare leaf still reads its own series...
    assert col("BTC", "own") == pytest.approx(btc)
    assert col("ETH", "own") == pytest.approx(eth)
    # ...while the explicit pick reads ETH on *both* series' rows.
    assert col("BTC", "eth_close") == pytest.approx(eth)
    assert col("ETH", "eth_close") == pytest.approx(eth)
    # And both readings compose in one expression.
    assert col("BTC", "spread") == pytest.approx([b - e for b, e in zip(btc, eth)])
    assert col("ETH", "spread") == pytest.approx([0.0] * 4)


def test_compute_overlays_pick_of_absent_symbol_stays_empty():
    # A typo'd symbol must read empty, never fall through to another series.
    snaps = [
        ta.Snapshot({"BTC": ta.Atom(ta.Candle(c, c, c, c, 1.0), time=i * 86_400_000)})
        for i, c in enumerate([10.0, 20.0, 30.0])
    ]
    schema, out = ta.compute_overlays(
        snaps, "typo: !close { source: !pick { symbol: NOPE } }"
    )
    i = schema.index_of("typo")
    assert [s.get("BTC").overlays.get_real(i) for s in out] == [None, None, None]


def test_compute_overlays_dict_of_prebuilt_indicators():
    atoms = _bars([10.0, 20.0, 30.0])
    schema, out = ta.compute_overlays(
        atoms, {"c": ta.close(), "r": ta.rsi(ta.close(), 2)}
    )
    assert schema.type_of_key("c") == "real"
    assert schema.type_of_key("r") == "real"
    # `c` is just the close, available every bar.
    assert out[0].overlays.get_real(schema.index_of("c")) == pytest.approx(10.0)


def test_compute_overlays_params_passthrough():
    atoms = _bars([10.0, 20.0, 30.0])
    schema, out = ta.compute_overlays(
        atoms, "r: !sma { period: !param P }", params={"P": 2}
    )
    i = schema.index_of("r")
    # SMA(2): warm at bar index 1 → mean(10, 20) = 15.
    assert out[1].overlays.get_real(i) == pytest.approx(15.0)


def test_compute_overlays_empty_series_returns_schema():
    schema, out = ta.compute_overlays([], "sma3: !sma { period: 3 }")
    assert schema.keys() == ["sma3"]
    assert out == []


def test_compute_overlays_rejects_non_indicator_dict_value():
    atoms = _bars([1.0, 2.0])
    with pytest.raises(TypeError, match="Indicator"):
        ta.compute_overlays(atoms, {"x": 5.0})


def test_compute_overlays_rejects_bad_overlays_type():
    atoms = _bars([1.0, 2.0])
    with pytest.raises(TypeError, match="YAML string or a dict"):
        ta.compute_overlays(atoms, 42)


# ── overlay-only atoms: a series that is not a price ─────────────────────────


def _funding_overlays(rate: float):
    b = ta.SchemaBuilder()
    b.add_real("funding_rate")
    schema = b.finish()
    return ta.OverlayInfo(schema, [rate])


def test_atom_without_a_candle_is_overlay_only():
    atom = ta.Atom(overlays=_funding_overlays(0.0003), time=0)
    assert atom.candle is None
    assert not atom.is_priceable


def test_atom_with_a_candle_stays_priceable():
    atom = ta.Atom(ta.Candle(1.0, 2.0, 0.5, 1.5, 10.0))
    assert atom.candle is not None
    assert atom.candle.close == 1.5
    assert atom.is_priceable


def test_an_atom_carrying_neither_is_rejected():
    with pytest.raises(ValueError, match="carries no data"):
        ta.Atom()


def test_protective_legs_take_off_only_their_share():
    """A stop can carry a size, so several owners can protect one position.

    Reduce-only: the size is clamped to the position, so a leg flattens but
    never flips. Omitting it keeps the whole-position behaviour.
    """
    w = ta.PaperWallet(10_000.0)
    bar = ta.Candle(100.0, 100.0, 100.0, 100.0, 1.0)
    w.update("A", bar)
    w.set("A", "buy", ta.Size.units(10.0))
    w.update("A", bar)
    assert w.position("A") == 10.0

    w.set_stop("A", 95.0, ta.Size.units(4.0))
    w.update("A", ta.Candle(100.0, 100.0, 90.0, 92.0, 1.0))
    assert w.position("A") == 6.0, "only the leg's own share should come off"

    # No size -> the whole position, as before.
    w.set_stop("A", 95.0)
    w.update("A", ta.Candle(94.0, 94.0, 90.0, 92.0, 1.0))
    assert w.position("A") == 0.0


def test_adjust_funds_credits_and_refuses_overdraft():
    w = ta.PaperWallet(1_000.0)
    w.adjust_funds(250.0)
    assert w.funds == 1_250.0
    w.adjust_funds(-250.0)
    assert w.funds == 1_000.0
    with pytest.raises(ValueError):
        w.adjust_funds(-5_000.0)


# ---------------------------------------------------------------------------
# Pickling
#
# The point is `multiprocessing` / `joblib` / `ProcessPoolExecutor` — the
# standard way a Python caller fans work over cores. None of it worked before,
# because pickle stores a type by `module.qualname` and every pyclass answered
# `builtins`.
# ---------------------------------------------------------------------------


def _pickle_cases():
    import fugazi.metrics as mm

    order = ta.Order(
        symbol="BTC",
        side="buy",
        units=2.0,
        price=100.0,
        kind="limit",
        id=7,
        commission=0.5,
    )
    fill = ta.Fill(bar=3, order=order)
    trades = mm.reconstruct_trades(
        [
            ta.Fill(
                bar=0, order=ta.Order(symbol="B", side="buy", units=1.0, price=10.0)
            ),
            ta.Fill(
                bar=1, order=ta.Order(symbol="B", side="sell", units=1.0, price=12.0)
            ),
        ]
    )
    b = ta.SchemaBuilder()
    b.add_real("funding")
    b.add_bool("halted")
    b.add_str("regime")
    schema = b.finish()
    overlays = ta.OverlayInfo(schema, [0.01, True, "bull"])
    atom = ta.Atom(
        candle=ta.Candle(1.0, 2.0, 0.5, 1.5, 10.0), overlays=overlays, time=1
    )
    snapshot = ta.Snapshot()
    snapshot.push("BTC", atom)
    # Two entries under one symbol at different cadences: the case a `dict`
    # round-trip would silently collapse, which is why `Snapshot.__reduce__`
    # replays `push` rather than the mapping constructor.
    snapshot.push(("BTC", "1h"), atom)
    return {
        "Candle": ta.Candle(1.0, 2.0, 0.5, 1.5, 10.0),
        "Schema": schema,
        "OverlayInfo": overlays,
        "Atom": atom,
        "Snapshot": snapshot,
        "Frequency": ta.Frequency("1h"),
        "Selector": ta.Selector(symbol="BTC", stream=ta.Frequency("1d")),
        "Size": ta.Size.value_frac(0.5),
        "Order": order,
        "Fill": fill,
        "RunReport": ta.RunReport(
            equity_curve=[100.0, 110.0, 90.0], initial_equity=100.0, fills=[fill]
        ),
        "Trade": trades[0],
        "DrawdownSegment": mm.drawdown_segments([100.0, 110.0, 90.0, 120.0])[0],
    }


@pytest.mark.parametrize("name", sorted(_pickle_cases()))
def test_value_types_round_trip_through_pickle(name):
    import pickle

    obj = _pickle_cases()[name]
    assert repr(pickle.loads(pickle.dumps(obj))) == repr(obj)


@pytest.mark.parametrize("name", sorted(_pickle_cases()))
def test_value_types_survive_deepcopy(name):
    import copy

    obj = _pickle_cases()[name]
    assert repr(copy.deepcopy(obj)) == repr(obj)


def test_pickled_types_name_a_real_module():
    """The regression that made all of the above impossible: without
    `module = "fugazi"` on the pyclass, every type reported `builtins` and
    pickle could not resolve it back."""
    import fugazi.metrics as mm

    for obj in _pickle_cases().values():
        mod = type(obj).__module__
        assert mod in ("fugazi", "fugazi.metrics"), (type(obj), mod)
    # The submodule must answer its *dotted* name, or a `__reduce__` pointing
    # into it fails with "import of module 'metrics' failed".
    assert mm.__name__ == "fugazi.metrics"


def test_size_reports_what_it_was_built_as():
    """`Size` was write-only — four constructors in, nothing out. `kind`/`value`
    are what `__reduce__` reconstructs from, so they cannot silently drift."""
    for kind, ctor in [
        ("units", ta.Size.units),
        ("funds_frac", ta.Size.funds_frac),
        ("value_frac", ta.Size.value_frac),
        ("position_frac", ta.Size.position_frac),
    ]:
        size = ctor(0.25)
        assert size.kind == kind
        assert size.value == pytest.approx(0.25)


def test_overlay_info_reports_what_it_was_built_from():
    b = ta.SchemaBuilder()
    b.add_real("funding")
    b.add_bool("halted")
    b.add_str("regime")
    schema = b.finish()
    info = ta.OverlayInfo(schema, [0.01, True, "bull"])
    assert info.schema.keys() == schema.keys()
    assert info.values == [0.01, True, "bull"]
    # An absent slot round-trips as None, not as a zero.
    assert ta.OverlayInfo(schema, [None, True, "bull"]).values == [None, True, "bull"]


@pytest.mark.parametrize("name", sorted(_pickle_cases()))
def test_pickling_works_off_the_creating_thread(name):
    """The regression that kept `Atom`/`OverlayInfo`/`Snapshot` unpicklable.

    They carried pyo3's `unsendable` long after `OverlayInfo` moved from `Rc` to
    `Arc`. That flag makes pyo3 assert the accessing thread on *every* method
    call, `__reduce__` included — and `multiprocessing` pickles on its queue
    feeder **thread**, so the marker turned a working `pickle.dumps` into a
    panic that hung the pool. A plain main-thread round-trip cannot see it; this
    can.
    """
    import pickle
    import threading

    obj = _pickle_cases()[name]
    result: list[object] = []

    def round_trip():
        try:
            result.append(repr(pickle.loads(pickle.dumps(obj))))
        except BaseException as exc:  # noqa: BLE001 - reported below
            result.append(exc)

    worker = threading.Thread(target=round_trip)
    worker.start()
    worker.join(timeout=30)
    assert not worker.is_alive(), f"{name}: pickling hung on a non-creating thread"
    assert result, f"{name}: worker produced nothing"
    assert not isinstance(result[0], BaseException), f"{name}: {result[0]!r}"
    assert result[0] == repr(obj)


def test_a_snapshot_round_trip_keeps_two_cadences_of_one_symbol():
    """`Snapshot(mapping)` would collapse them into one dict key, so
    `__reduce__` replays `push` instead."""
    import pickle

    atom = ta.Atom(candle=ta.Candle(1.0, 2.0, 0.5, 1.5, 10.0))
    snap = ta.Snapshot()
    snap.push(("BTC", "1h"), atom)
    snap.push(("BTC", "1d"), atom)
    assert len(snap) == 2
    restored = pickle.loads(pickle.dumps(snap))
    assert restored.keys() == snap.keys()
    assert len(restored) == 2


def test_equity_array_matches_equity_curve():
    """`equity_array` is the same numbers written straight into a NumPy buffer —
    no intermediate list, and the form `Series` takes its fast memcpy path over."""
    np = pytest.importorskip("numpy")
    rep = ta.RunReport(equity_curve=[100.0, 110.0, 90.0], initial_equity=100.0)
    arr = rep.equity_array
    assert isinstance(arr, np.ndarray)
    assert arr.dtype == np.float64
    assert arr.tolist() == rep.equity_curve
    # Same answer downstream, whichever form you hand the metrics.
    assert ta.metrics.per_bar_returns(arr, 100.0) == pytest.approx(
        ta.metrics.per_bar_returns(rep.equity_curve, 100.0)
    )


def test_equity_curve_stays_a_list_so_plus_still_concatenates():
    """Why `equity_curve` was *not* switched to an ndarray: `+` concatenates two
    lists and adds two arrays elementwise, and the chunked-run examples in the
    README rely on concatenation. `equity_array` is the opt-in instead."""
    rep = ta.RunReport(equity_curve=[1.0, 2.0], initial_equity=1.0)
    assert isinstance(rep.equity_curve, list)
    assert rep.equity_curve + rep.equity_curve == [1.0, 2.0, 1.0, 2.0]


def test_module_reports_its_version():
    """`fugazi.__version__` is `CARGO_PKG_VERSION` from `python/Cargo.toml`,
    which a release bump already touches — so this adds no new place to sync,
    and this test is what proves it stays in step with the wheel metadata."""
    import re

    assert re.fullmatch(r"\d+\.\d+\.\d+", ta.__version__), ta.__version__


def test_unpickling_helpers_stay_exported():
    """They must be on the package, not merely on the extension module: a
    `__reduce__` resolves `fugazi._rebuild_*`, and maturin's shim populates the
    package with `from .fugazi import *`, which honours `__all__`."""
    for name in (
        "_rebuild_schema",
        "_rebuild_size",
        "_rebuild_order",
        "_rebuild_run_report",
    ):
        assert name in ta.__all__, f"{name} missing from __all__ — pickling will break"
        assert hasattr(ta, name)


def test_shared_multi_is_a_named_collection():
    """`.shared()` is the one place Rust's typing does not survive the boundary:
    Rust has a distinct type per multi, Python has one class carrying the union
    of every accessor — so `bollinger(...).shared().adx()` type-checks and fails
    at call time. `names()` is the only honest source of truth, and the container
    protocol makes that the natural thing to reach for."""
    macd = ta.macd(ta.close(), 2, 4, 2).shared()
    assert list(macd) == macd.names() == ["macd", "signal", "histogram"]
    assert len(macd) == 3
    assert "signal" in macd and "adx" not in macd
    # Subscript is `component()` — same projection, spelled as the lookup it is.
    # Each handle gets its own multi: projections off *one* `.shared()` handle
    # share the underlying state, so feeding one advances the other.
    bars = closes([1.0, 2.0, 3.0, 4.0, 5.0, 6.0])
    by_subscript = ta.macd(ta.close(), 2, 4, 2).shared()["signal"]
    by_method = ta.macd(ta.close(), 2, 4, 2).shared().component("signal")
    assert feed(by_subscript, bars) == feed(by_method, bars)
    with pytest.raises(ValueError, match="adx"):
        _ = macd["adx"]

    bands = ta.bollinger(ta.close(), 3, 2.0).shared()
    assert list(bands) == ["upper", "middle", "lower"]
    assert "macd" not in bands


def test_set_costs_for_all_resolves_per_symbol():
    """The point of the loop: `by_symbol` scoping still gives each symbol its own
    bundle. A single pre-resolved bundle — Rust's `with_costs` shape — could not,
    because resolving needs a symbol to resolve *against*, so it would have to
    pick a placeholder and silently take the `default:` leg."""
    # Scoping is per *leg*, not top-level — see docs/COSTS.md.
    config = {
        "commission": {
            "default": {"percentage": {"rate": 0.001}},
            "by_symbol": {"ETH": {"percentage": {"rate": 0.05}}},
        }
    }
    wallet = ta.PaperWallet(100_000.0)
    wallet.set_costs_for_all(["BTC", "ETH"], config)

    paid = {}
    for symbol in ("BTC", "ETH"):
        wallet.update(symbol, ta.Candle(100.0, 100.0, 100.0, 100.0, 1.0))
        wallet.set_position(symbol, 1.0)
        fills = wallet.update(symbol, ta.Candle(100.0, 100.0, 100.0, 100.0, 1.0))
        paid[symbol] = fills[0].commission

    assert paid["BTC"] == pytest.approx(0.1)  # default leg: 0.1% of 100
    assert paid["ETH"] == pytest.approx(5.0)  # by_symbol leg: 5% of 100


def test_set_costs_for_all_matches_the_per_symbol_loop():
    """It *is* the loop, so it had better agree with it."""
    config = {"commission": {"percentage": {"rate": 0.002}}}
    symbols = ["BTC", "ETH", "SOL"]

    bulk = ta.PaperWallet(100_000.0)
    bulk.set_costs_for_all(symbols, config)
    one_by_one = ta.PaperWallet(100_000.0)
    for symbol in symbols:
        one_by_one.set_costs_for(symbol, config)

    for wallet in (bulk, one_by_one):
        for symbol in symbols:
            wallet.update(symbol, ta.Candle(50.0, 50.0, 50.0, 50.0, 1.0))
            wallet.set_position(symbol, 2.0)
            wallet.update(symbol, ta.Candle(50.0, 50.0, 50.0, 50.0, 1.0))
    assert [o.commission for o in bulk.orders()] == [
        o.commission for o in one_by_one.orders()
    ]


def test_set_costs_for_all_refuses_a_repeated_symbol():
    """In a call whose whole argument is a universe, a repeat is a typo."""
    wallet = ta.PaperWallet(1000.0)
    with pytest.raises(ValueError, match="more than once"):
        wallet.set_costs_for_all(
            ["BTC", "ETH", "BTC"], {"commission": {"percentage": {"rate": 0.1}}}
        )
