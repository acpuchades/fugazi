"""A declared symbol absent from a *bar* vs. absent from the *stream*.

Both used to bottom out in ``Snapshot::sole_atom``. A symbol with a shorter
history panicked the run — and because pyo3 turns a Rust panic into
``PanicException``, which derives from ``BaseException``, ``except Exception``
walked straight past it. A symbol absent from the whole stream did the
opposite and completed as a zero-fill "successful" run.

The CLI has always been safe from both: it builds each snapshot from the traded
symbol's own bars. These are the paths where the caller supplies the snapshots.
"""

import math

import pytest

import fugazi as ta

N_SYMS = 9
LISTS_AT = 120
BARS = 300


def price(bar: int, k: int) -> float:
    # Oscillating, so a crossover document actually changes state.
    return 100.0 + k + 10.0 * math.sin(bar / 7.0)


def candle(px: float) -> ta.Candle:
    return ta.Candle(px, px + 1.0, px - 1.0, px, 100.0)


def one(sym: str, px: float, bar: int) -> ta.Snapshot:
    snap = ta.Snapshot()
    snap.push(sym, ta.Atom(candle(px), time=bar * 60_000))
    return snap


def late_listing_stream() -> list[ta.Snapshot]:
    """Nine symbols; ``S8USDT`` has no atom before ``LISTS_AT``."""
    out = []
    for i in range(BARS):
        snap = ta.Snapshot()
        for k in range(N_SYMS):
            if k == N_SYMS - 1 and i < LISTS_AT:
                continue
            snap.push(f"S{k}USDT", ta.Atom(candle(price(i, k)), time=i * 60_000))
        out.append(snap)
    return out


def doc(sym: str) -> str:
    return (
        f"symbol: {sym}\n"
        "long:\n"
        "  enter: !crosses_above { lhs: !close, rhs: !sma { period: 10 } }\n"
        "  exit: !crosses_below { lhs: !close, rhs: !sma { period: 10 } }\n"
    )


# ---------------------------------------------------------------------------
# Absent from a bar — ordinary, must not fail
# ---------------------------------------------------------------------------


def test_a_late_listing_runs_instead_of_raising():
    spec = ta.load_spec(doc("S8USDT"))
    report = spec.run(ta.PaperWallet(10_000.0), late_listing_stream())
    assert len(report.equity_curve) == BARS


def test_the_late_listing_does_not_trade_before_it_lists():
    spec = ta.load_spec(doc("S8USDT"))
    report = spec.run(ta.PaperWallet(10_000.0), late_listing_stream())
    assert report.fills, "never traded at all — the assertion below would be vacuous"
    assert all(f.bar >= LISTS_AT for f in report.fills)


def test_a_symbol_present_from_bar_zero_is_unaffected():
    spec = ta.load_spec(doc("S0USDT"))
    report = spec.run(ta.PaperWallet(10_000.0), late_listing_stream())
    assert len(report.equity_curve) == BARS
    assert report.fills


# ---------------------------------------------------------------------------
# Absent from the stream — bad input, must fail by name
# ---------------------------------------------------------------------------


def test_a_symbol_absent_from_the_whole_stream_is_refused():
    snaps = [one("BTCUSDT", price(i, 0), i) for i in range(BARS)]
    spec = ta.load_spec(doc("BTCUSD"))
    with pytest.raises(Exception) as excinfo:
        spec.run(ta.PaperWallet(10_000.0), snaps)
    msg = str(excinfo.value)
    assert "BTCUSD" in msg
    assert "BTCUSDT" in msg


def test_the_refusal_is_a_catchable_exception():
    # Not a `PanicException`: that derives from `BaseException`, so a caller
    # writing `except Exception` could not handle it at all.
    snaps = [one("BTCUSDT", 100.0, i) for i in range(50)]
    spec = ta.load_spec(doc("BTCUSD"))
    try:
        spec.run(ta.PaperWallet(10_000.0), snaps)
    except Exception:
        return
    pytest.fail("the run was not refused at all")


# ---------------------------------------------------------------------------
# The primitive itself
# ---------------------------------------------------------------------------


def test_sole_atom_raises_value_error_not_a_panic():
    snap = ta.Snapshot()
    snap.push("BTCUSDT", ta.Atom(candle(100.0), time=0))
    snap.push("ETHUSDT", ta.Atom(candle(60.0), time=0))
    with pytest.raises(ValueError) as excinfo:
        snap.sole_atom()
    assert "2 priceable series" in str(excinfo.value)


def test_sole_atom_is_caught_by_except_exception():
    # The whole point of the change — this used to walk straight past.
    snap = ta.Snapshot()
    snap.push("BTCUSDT", ta.Atom(candle(100.0), time=0))
    snap.push("ETHUSDT", ta.Atom(candle(60.0), time=0))
    try:
        snap.sole_atom()
    except Exception:
        return
    pytest.fail("sole_atom did not raise a catchable exception")


def test_sole_atom_still_unpacks_a_single_entry():
    snap = ta.Snapshot()
    snap.push("BTCUSDT", ta.Atom(candle(100.0), time=0))
    assert snap.sole_atom() is not None
    assert ta.Snapshot().sole_atom() is None
