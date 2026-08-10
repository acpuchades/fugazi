"""Parity between the YAML spec vocabulary and the Python bindings.

The Rust library, the YAML spec layer and this module are three surfaces over
one set of primitives, and nothing but this file stops them drifting: adding an
indicator to `ExprSpec` and forgetting the `#[pyfunction]` is a silent gap that
only a user hits.

The rule enforced here is not "everything must be bound" — plenty of tags
deliberately aren't. It's that every tag must be a *decision*: either reachable
from Python (as a module function, an `Indicator` / `Signal` method, or a
multi-output accessor) or listed below with a reason. A new tag fails this test
until someone classifies it, which is the point.

`ta.spec_tags()` reads the vocabulary off serde's own variant list, so the
expected side of this comparison needs no upkeep.
"""

import fugazi as ta

# --- tags reached through a method rather than a module function -------------

# Arithmetic, comparison and lookback operators hang off `Indicator`; boolean
# logic and the edge primitive hang off `Signal`. Value is the method name.
METHOD_BOUND = {
    # Indicator
    "add": "add", "sub": "sub", "mul": "mul", "div": "div",
    "lag": "lag", "diff": "diff", "ratio": "ratio", "roc": "roc",
    "rolling_max": "rolling_max", "rolling_min": "rolling_min",
    "gt": "gt", "lt": "lt", "ge": "ge", "le": "le", "eq": "eq", "ne": "ne",
    "above": "above", "below": "below",
    "crosses_above": "crosses_above", "crosses_below": "crosses_below",
    # Signal (trailing underscore where the name is a Python keyword)
    "and": "and_", "or": "or_", "xor": "xor_", "not": "not_",
    "changed": "changed",
}

# Multi-output tags: YAML has one tag per component, Python has one composite
# constructor plus accessors on the shared handle. Value is
# (module function, accessor).
COMPONENT_BOUND = {
    "macd_line": ("macd", "line"),
    "macd_signal": ("macd", "signal"),
    "macd_histogram": ("macd", "histogram"),
    "bb_upper": ("bollinger", "upper"),
    "bb_middle": ("bollinger", "middle"),
    "bb_lower": ("bollinger", "lower"),
    "keltner_upper": ("keltner", "upper"),
    "keltner_middle": ("keltner", "middle"),
    "keltner_lower": ("keltner", "lower"),
    "donchian_upper": ("donchian", "upper"),
    "donchian_middle": ("donchian", "middle"),
    "donchian_lower": ("donchian", "lower"),
    "plus_di": ("adx", "plus_di"),
    "minus_di": ("adx", "minus_di"),
    "dmi_plus_di": ("dmi", "plus_di"),
    "dmi_minus_di": ("dmi", "minus_di"),
    "aroon_up": ("aroon", "up"),
    "aroon_down": ("aroon", "down"),
    "aroon_oscillator": ("aroon", "oscillator"),
}

# Tags bound under a different module-level name.
RENAMED = {
    "sharpe": "sharpe_of",
    "sortino": "sortino_of",
    "volatility": "volatility_of",
    "max_drawdown": "max_drawdown_of",
    "calmar": "calmar_of",
}

# --- tags deliberately not reachable from Python -----------------------------

# Each entry is a reason, not an excuse: if one of these becomes bindable the
# entry should go, and if a new tag joins the list it needs a reason too.
NOT_BOUND = {
    # Position anchors: the per-leg `Position` is never handed to a Python
    # factory, so there is nothing for these to read.
    "entry": "position-anchored; Position is not exposed to Python",
    "peak": "position-anchored; Position is not exposed to Python",
    "trough": "position-anchored; Position is not exposed to Python",
    # Book fields and the two source selectors that pick which book to read.
    "strategy_book": "build-time source selector, not a value",
    "portfolio_book": "build-time source selector, not a value",
    "equity": "book-anchored; Book is not exposed to Python",
    "equity_peak": "book-anchored; Book is not exposed to Python",
    "drawdown": "book-anchored; Book is not exposed to Python",
    "return_per_bar": "book-anchored; Book is not exposed to Python",
    "trade_pnl": "book-anchored; Book is not exposed to Python",
    "trade_return": "book-anchored; Book is not exposed to Python",
    # Sizing recipes read the strategy's Book / own asset, same reason.
    "vol_target": "sizing recipe; built inside a strategy, not standalone",
    "atr_risk": "sizing recipe; built inside a strategy, not standalone",
    "drawdown_throttle": "sizing recipe; book-anchored",
    "equity_vol_target": "sizing recipe; book-anchored",
    "fractional_kelly": "sizing recipe; book-anchored",
    # Reachable from Python only through a spec document.
    "current": "the whole-Candle leaf; Python bar indicators take their source implicitly",
    "match": "multi-way dispatch; use if_else or a spec document",
    "time": "raw Timestamp leaf; the calendar accessors cover the useful reads",
    "all": "n-ary AND fold; chain .and_() instead",
    "any": "n-ary OR fold; chain .or_() instead",
    "became_true": "rising edge; compose .changed() with the condition",
    "became_false": "falling edge; compose .changed() with the condition",
    "never": "constant-false signal; use value(False)",
    "every": "periodic pulse; only meaningful as a rebalance_on: gate in a spec",
    "has_column": "schema predicate; resolved at build time in a spec",
}


def _module_names():
    return {n for n in dir(ta) if not n.startswith("_")}


def test_every_node_tag_is_bound_or_declared_unbound():
    # The value/signal split was merged into one NodeSpec vocabulary, so the
    # former expr and signal parity tests are one: every tag in the single
    # `"node"` group must be a bound constructor, a bound method / component,
    # a rename, or recorded in NOT_BOUND with a reason.
    names = _module_names()
    indicator_methods = set(dir(ta.Indicator))
    signal_methods = set(dir(ta.Signal))
    shared_methods = set(dir(ta.SharedMultiIndicator))

    unclassified = []
    for tag in ta.spec_tags()["node"]:
        if tag in names or tag in NOT_BOUND:
            continue
        if tag in RENAMED:
            assert RENAMED[tag] in names, f"!{tag} maps to missing {RENAMED[tag]}()"
            continue
        if tag in METHOD_BOUND:
            method = METHOD_BOUND[tag]
            assert method in indicator_methods or method in signal_methods, (
                f"!{tag} claims method .{method}(), which no longer exists"
            )
            continue
        if tag in COMPONENT_BOUND:
            ctor, accessor = COMPONENT_BOUND[tag]
            assert ctor in names, f"!{tag} maps to missing {ctor}()"
            assert accessor in shared_methods, (
                f"!{tag} claims accessor .{accessor}(), which no longer exists"
            )
            continue
        unclassified.append(tag)

    assert not unclassified, (
        "these spec tags have no Python counterpart and no recorded reason:\n  "
        + "\n  ".join(f"!{t}" for t in unclassified)
        + "\nBind them in python/src/lib.rs, or add them to NOT_BOUND here with "
        "a reason."
    )


def test_selection_rules_are_all_bound():
    """The `selection:` vocabulary is small and fully bound — keep it that way."""
    names = _module_names()
    missing = [t for t in ta.spec_tags()["selection"] if t not in names]
    assert not missing, f"unbound basket selection rules: {missing}"


def test_the_declared_tables_do_not_go_stale():
    """A tag that leaves the spec layer should leave these tables with it."""
    known = set()
    for group in ta.spec_tags().values():
        known.update(group)
    declared = set(NOT_BOUND) | set(METHOD_BOUND) | set(COMPONENT_BOUND) | set(RENAMED)
    stale = sorted(declared - known)
    assert not stale, (
        f"these tags are classified here but no longer exist in the spec layer: {stale}"
    )


# --------------------------------------------------------------------------
# Wallet-method parity.
#
# The tag ledgers above are derived from serde's own variant lists, so they
# cannot go stale. `Wallet` is a Rust *trait*, which Python can't reflect into,
# so this one is hand-maintained — and it exists because that gap let a real
# regression through: `set_stop` grew a `size` parameter on the Rust side and
# the binding kept passing a hardcoded whole-position size, quietly making
# partial protective exits unreachable from Python.
#
# If you add or change a `Wallet` method, update this list in the same PR.
# --------------------------------------------------------------------------

WALLET_BOUND = {
    "funds", "position", "positions", "price", "equity", "update",
    "set", "set_position", "close",
    "set_stop", "set_take_profit", "cancel_protective",
    "set_limit", "cancel_limit", "cancel",
    "adjust_funds", "poll_fills",
    # Inherent PaperWallet extras, not trait methods.
    "orders", "reset",
}

WALLET_NOT_BOUND = {
    "take_rejections": (
        "needs a bar-less rejection type; the run path already exposes the same "
        "entries on RunReport.rejections"
    ),
    "set_costs_for": (
        "needs a frequency argument to resolve a bundle; the spec path covers it "
        "via TradingCostsConfig"
    ),
}


def test_wallet_surface_matches_the_ledger():
    actual = {n for n in dir(ta.PaperWallet) if not n.startswith("_")}
    missing = WALLET_BOUND - actual
    extra = actual - WALLET_BOUND - set(WALLET_NOT_BOUND)
    assert not missing, f"ledger claims these are bound but they aren't: {sorted(missing)}"
    assert not extra, (
        f"PaperWallet gained methods the ledger doesn't record: {sorted(extra)} — "
        "add them to WALLET_BOUND, or to WALLET_NOT_BOUND with a reason"
    )


# --------------------------------------------------------------------------
# The OKX live wallet mirrors the same order-flow surface as PaperWallet, minus
# the paper-only conveniences and plus two live-only affordances. Same ledger
# discipline: change a method on the binding, update this list.
# --------------------------------------------------------------------------

OKX_WALLET_BOUND = {
    # Constructors (staticmethods).
    "demo", "mainnet",
    # Wallet reads.
    "funds", "position", "price", "equity",
    # Order flow.
    "update", "set", "set_position", "close",
    "set_stop", "set_take_profit", "cancel_protective",
    "set_limit", "cancel_limit", "cancel", "poll_fills",
    # Live-only extras.
    "refresh_account", "errors",
}

OKX_WALLET_NOT_BOUND = {
    "positions": (
        "OkxWallet doesn't override the trait default (empty), so a dict view "
        "would mislead; read one symbol at a time via position(symbol)"
    ),
    "orders": "no in-memory blotter on a live venue (paper-only convenience)",
    "reset": "a live venue has no freshly-constructed state to restore",
    "adjust_funds": "OKX takes the trait default (UnsupportedOperation)",
    "take_rejections": (
        "needs a bar-less rejection type; errors() surfaces REST-failure detail"
    ),
    "set_costs_for": "a live venue owns its own fees",
}


def test_okx_wallet_surface_matches_the_ledger():
    actual = {n for n in dir(ta.OkxWallet) if not n.startswith("_")}
    missing = OKX_WALLET_BOUND - actual
    extra = actual - OKX_WALLET_BOUND - set(OKX_WALLET_NOT_BOUND)
    assert not missing, f"ledger claims these are bound but they aren't: {sorted(missing)}"
    assert not extra, (
        f"OkxWallet gained methods the ledger doesn't record: {sorted(extra)} — "
        "add them to OKX_WALLET_BOUND, or to OKX_WALLET_NOT_BOUND with a reason"
    )


def test_protective_legs_expose_their_size():
    """The specific regression the ledger above exists to prevent."""
    w = ta.PaperWallet(1_000.0)
    w.update("A", ta.Candle(10.0, 10.0, 10.0, 10.0, 1.0))
    # Must accept a size; a TypeError here means the binding lost the parameter.
    w.set_stop("A", 9.0, ta.Size.units(1.0))
    w.set_take_profit("A", 11.0, ta.Size.units(1.0))
