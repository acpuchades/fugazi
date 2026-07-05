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
    plus = (ta.close() + 10.0)
    assert feed(plus, closes([5.0]))[0] == pytest.approx(15.0)


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


def test_bar_indicator_atr():
    atr = ta.atr(2)
    bars = [
        ta.Candle(10, 11, 9, 10, 1),
        ta.Candle(10, 12, 8, 11, 1),
        ta.Candle(11, 13, 10, 12, 1),
    ]
    out = feed(atr, bars)
    assert out[-1] is not None and out[-1] > 0


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
    out = ta.atr(2).feed({"high": [11, 12, 13], "low": [9, 8, 10], "close": [10, 11, 12]})
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


@pytest.mark.parametrize("op", ["add", "sub", "mul", "div", "gt", "lt", "ge", "le",
                                "crosses_above", "crosses_below"])
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
    assert w.equity() == pytest.approx(600.0 + 4.0 * 120.0)
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


def test_wallet_rejects_bad_side():
    w = ta.PaperWallet(100.0)
    w.update("X", 10.0)
    with pytest.raises(ValueError):
        w.set("X", "hodl", 1.0)


def test_impossible_market_orders_never_fill():
    w = ta.PaperWallet(100.0)
    w.update("X", 50.0)
    # A queued market buy beyond funds (3 * 50 = 150 > 100) simply never fills.
    w.set("X", "buy", 3.0)
    w.update("X", 50.0)
    assert not w.positions()
    # A short sale credits cash, so selling is always feasible.
    w.set("X", "sell", 3.0)
    w.update("X", 50.0)
    assert w.position("X") == pytest.approx(-3.0)


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


def test_warm_up_and_unstable_periods():
    # Windowed: exact warm-up, no unstable tail.
    sma = ta.sma(ta.close(), 20)
    assert sma.warm_up_period() == 20
    assert sma.unstable_period() == 0
    assert sma.stable_period() == 20
    # Recursive: the EMA seeds immediately but takes time to converge.
    ema = ta.ema(ta.close(), 20)
    assert ema.warm_up_period() == 1
    assert ema.unstable_period() > 0
    assert ema.stable_period() == ema.warm_up_period() + ema.unstable_period()
    # Composition accounts for the whole chain, through signals too.
    chained = ta.ema(ta.sma(ta.close(), 10), 20)
    assert chained.warm_up_period() == 10
    sig = ta.close().crosses_above(ta.sma(ta.close(), 10))
    assert sig.warm_up_period() == 11  # comparison plus its edge detector
    assert sig.unstable_period() == 0
    # Multi-output indicators report the slowest line.
    macd = ta.macd(ta.close(), 12, 26, 9)
    assert macd.warm_up_period() == 1
    assert macd.unstable_period() > 0


def test_warm_up_matches_first_output():
    node = ta.rsi(ta.close(), 14)
    w = node.warm_up_period()
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


def test_stable_flips_true_after_stable_period_samples():
    entry = ta.close().crosses_above(ta.ema(ta.close(), 3))
    check = ta.stable(entry)
    period = entry.stable_period()
    assert period > 1
    # Fewer than `period` samples: still unstable.
    for i in range(1, period):
        check.update(ta.Candle(i, i, i, i, 0.0))
        assert check.is_true() is False
    # The `period`-th update flips it.
    check.update(ta.Candle(period, period, period, period, 0.0))
    assert check.is_true() is True


def test_stable_and_composes_into_gated_entry():
    """The canonical readiness-gated entry: `entry & stable(entry)`."""
    entry = ta.close().crosses_above(ta.ema(ta.close(), 3))
    gated = entry.and_(ta.stable(entry))
    # Feed enough bars to get past the stable period.
    bars = closes([100.0 + 0.5 * i + (i % 5) for i in range(entry.stable_period() * 2)])
    fired = any(v for v in feed(gated, bars))
    # The composed signal is a Signal and can be evaluated.
    assert isinstance(gated, ta.Signal)
    assert isinstance(fired, bool)
