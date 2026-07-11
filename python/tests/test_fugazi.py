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
    # The condition (close > 0) is always true, so we pick if_true
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
    # always false, so we pick if_false (a constant). The ternary reads
    # Some on bar 1 even though the UNSELECTED SMA-5 hasn't warmed —
    # `warm_up_period()` is still 5 (upper bound for downstream stability
    # gates), but the actual first Some can arrive earlier.
    branch = ta.if_else(
        ta.close().lt(ta.value(0.0)),
        ta.sma(ta.close(), 5),
        ta.value(-1.0),
    )
    assert branch.warm_up_period() == 5
    assert branch.update(ta.Candle(100.0, 100.0, 100.0, 100.0, 0.0)) == -1.0


def test_unstable_signal_zeroes_unstable_period_but_forwards_output():
    entry = ta.close().crosses_above(ta.ema(ta.close(), 3))
    raw_stable = entry.stable_period()
    raw_warm = entry.warm_up_period()
    assert raw_stable > raw_warm  # ema has a real IIR tail
    wrapped = ta.unstable(entry)
    assert isinstance(wrapped, ta.Signal)
    assert wrapped.warm_up_period() == raw_warm
    assert wrapped.unstable_period() == 0
    assert wrapped.stable_period() == raw_warm

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
    warm = src.warm_up_period()
    settle = src.unstable_period()
    assert settle > 0
    m = src.unstable()
    f = ta.unstable(ta.ema(ta.close(), 5))
    assert isinstance(m, ta.Indicator)
    assert isinstance(f, ta.Indicator)
    assert m.warm_up_period() == warm
    assert m.unstable_period() == 0
    assert m.stable_period() == warm
    # Method and free-function forms are the same wrapper.
    assert f.warm_up_period() == warm
    assert f.unstable_period() == 0


def test_signal_unstable_method_matches_free_function():
    entry = ta.close().crosses_above(ta.ema(ta.close(), 3))
    warm = entry.warm_up_period()
    m = entry.unstable()
    f = ta.unstable(ta.close().crosses_above(ta.ema(ta.close(), 3)))
    assert m.warm_up_period() == warm
    assert m.unstable_period() == 0
    assert f.warm_up_period() == warm
    assert f.unstable_period() == 0
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


def test_drawdown_pipeline():
    from fugazi import metrics

    equity = [100.0, 110.0, 105.0, 90.0, 95.0, 120.0, 100.0]
    segs = metrics.drawdown_segments(equity)
    assert len(segs) == 2
    assert isinstance(segs[0], metrics.DrawdownSegment)
    assert metrics.max_drawdown(segs) == pytest.approx((110.0 - 90.0) / 110.0)
    assert metrics.max_drawdown_duration(segs) == 2
    assert metrics.average_drawdown(segs) is not None
    assert metrics.time_in_drawdown_ratio(segs, 7) == pytest.approx(4.0 / 7.0)
    assert metrics.recovery_factor(equity, 100.0) is not None


def test_reconstruct_trades_round_trip_through_wallet():
    """Fill(bar, order) built from PaperWallet.update() feeds metrics cleanly."""
    from fugazi import metrics

    w = ta.PaperWallet(1000.0)
    fills = []
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
    assert trades[0].bars_held() == 1
    assert metrics.win_rate(trades) == 1.0
    assert metrics.profit_factor(trades) is None  # no losing trade
    assert metrics.average_bars_held(trades) == pytest.approx(1.0)
    assert metrics.exposure_ratio(fills, total_bars=2) == pytest.approx(0.5)


def test_trade_and_drawdown_segment_are_frozen_readonly():
    from fugazi import metrics

    seg = metrics.drawdown_segments([100.0, 90.0, 100.0])[0]
    with pytest.raises(AttributeError):
        seg.depth_ratio = 0.0  # frozen


def test_fill_has_bar_and_order_getters():
    w = ta.PaperWallet(1000.0)
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


def test_snapshot_construct_from_mapping():
    # Both a dict of Atom and a dict of Candle work (candle → atom lifted).
    snap = ta.Snapshot({"BTC": _atom(ms=1, close=100.0), "ETH": ta.Candle(1, 2, 0.5, 50, 1)})
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
    out = btc_close.update({"BTC": _atom(ms=1, close=42.0), "ETH": _atom(ms=1, close=0.0)})
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
    assert src.warm_up_period() == 1
    assert src.unstable_period() == 0
    assert src.stable_period() == 1
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
    assert ta.Frequency("1d") > ta.Frequency("24h") or ta.Frequency("1d") == ta.Frequency("24h")


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
    assert ta.Selector(symbol="BTC").freq is None
    assert ta.Selector(freq="1h").symbol is None
    assert str(ta.Selector(freq="1h").freq) == "1h"
    assert ta.Selector(symbol="BTC", freq="1h").symbol == "BTC"
    # `freq` accepts a Frequency instance too, not just a str.
    assert ta.Selector(freq=ta.Frequency("1h")).freq == ta.Frequency("1h")


def test_selector_matches_wildcard_semantics():
    query = ta.Selector(symbol="BTC")  # freq is a wildcard
    assert query.matches(ta.Selector(symbol="BTC", freq="1h"))
    assert query.matches(ta.Selector(symbol="BTC"))
    assert not query.matches(ta.Selector(symbol="ETH", freq="1h"))
    # An empty query matches every storage entry.
    empty = ta.Selector()
    assert empty.matches(ta.Selector(symbol="BTC"))
    assert empty.matches(ta.Selector(symbol="ETH", freq="1d"))


def test_snapshot_accepts_selector_keys():
    snap = ta.Snapshot(
        {
            ta.Selector(symbol="BTC", freq="1h"): _atom(ms=1, close=100.0),
            ta.Selector(symbol="BTC", freq="1d"): _atom(ms=1, close=300.0),
        }
    )
    # Exact lookup disambiguates.
    exact = snap[ta.Selector(symbol="BTC", freq="1h")]
    assert exact.candle.close == 100.0


def test_snapshot_find_wildcards_over_freq():
    snap = ta.Snapshot(
        {
            ta.Selector(symbol="BTC", freq="1h"): _atom(ms=1, close=100.0),
            ta.Selector(symbol="ETH", freq="1h"): _atom(ms=1, close=50.0),
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
    assert snap[ta.Selector(symbol="BTC", freq="1h")].candle.close == 100.0


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
