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

import inspect
import re

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


def test_grammar_docstring_documents_every_record_key():
    """`spec_grammar()`'s docstring must list exactly the keys a real record
    carries. It tells consumers to guard on `schema_version` for shape changes,
    so the self-documentation has to track the shape — `payload` (schema_version
    2) was the one that slipped. Pinned here so it can't recur."""
    block = re.search(r"```text\n(.*?)```", ta.spec_grammar.__doc__, re.S)
    assert block, "spec_grammar() docstring has no ```text``` key block"
    # Each key line begins with the key as a lowercase identifier; continuation
    # lines (e.g. wrapped enum values) start with a quote/indent and are skipped.
    documented = {
        line.split()[0]
        for line in block.group(1).splitlines()
        if line.split() and re.fullmatch(r"[a-z_]+", line.split()[0])
    }
    record_keys = set(ta.spec_grammar()["tags"][0].keys())
    assert documented == record_keys, (
        f"docstring keys {sorted(documented)} != record keys {sorted(record_keys)} — "
        "update the spec_grammar() docstring in python/src/spec.rs"
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
# Constructor ↔ descriptor parity (Gap 2, option **(a)** — the full mapping).
#
# The YAML tags were flattened — `macd_line` / `bb_upper` are top-level tags
# carrying the full param set — but the Python API kept coarser struct-returning
# constructors (`macd()` → {line, signal, histogram}). So a constructor maps to a
# *set* of component tags that share one field list. `CONSTRUCTORS` records that
# mapping, plus the one genuine param↔field rename (MACD's `*_period` ↔ the terse
# `fast`/`slow`/`signal`; every other constructor aligns by name).
#
# The test then asserts, across the whole mapped set, that each constructor's
# `inspect.signature` params exist as fields on its tags and that every default
# equals the descriptor's — which single-sources the duplicated constants
# (MACD 12/26/9, Bollinger 20/2.0, …). Those live once in the `spec::expr` consts
# feeding the serde `#[serde(default)]` and the descriptor; pyo3 can't reference a
# const-path default (it renders `...` in `__text_signature__` and would defeat
# this check), so the Python literals are *test-pinned* here. Drift is now a CI
# failure either way.
CONSTRUCTORS = {
    # constructor: (component tags, {py_param: tag_field renames})
    "macd": (
        ["macd_line", "macd_signal", "macd_histogram"],
        {"fast_period": "fast", "slow_period": "slow", "signal_period": "signal"},
    ),
    "bollinger": (["bb_upper", "bb_middle", "bb_lower"], {}),
    "keltner": (["keltner_upper", "keltner_middle", "keltner_lower"], {}),
    "donchian": (["donchian_upper", "donchian_middle", "donchian_lower"], {}),
    "adx": (["adx", "plus_di", "minus_di"], {}),
    "dmi": (["dmi_plus_di", "dmi_minus_di"], {}),
    "aroon": (["aroon_up", "aroon_down", "aroon_oscillator"], {}),
    "stoch_rsi": (["stoch_rsi"], {}),
    "sar": (["sar"], {}),
    # Same-named single-output constructors align 1:1, no rename.
    "sma": (["sma"], {}),
    "ema": (["ema"], {}),
    "rsi": (["rsi"], {}),
    "atr": (["atr"], {}),
    "cci": (["cci"], {}),
    "stochastic": (["stochastic"], {}),
}


def test_constructor_signatures_match_the_descriptor():
    grammar = {t["name"]: t for t in ta.spec_grammar()["tags"]}
    for ctor_name, (tags, renames) in CONSTRUCTORS.items():
        ctor = getattr(ta, ctor_name)
        params = inspect.signature(ctor).parameters
        for py_param, param in params.items():
            field_name = renames.get(py_param, py_param)
            for tag in tags:
                fields = {f["name"]: f for f in grammar[tag]["fields"]}
                assert field_name in fields, (
                    f"{ctor_name}({py_param}) maps to field {field_name!r}, absent from !{tag}"
                )
                # Every constructor default must equal the descriptor's, so the
                # duplicated constant can't drift.
                if param.default is not inspect.Parameter.empty:
                    descriptor_default = fields[field_name]["default"]
                    assert descriptor_default is not None, (
                        f"{ctor_name}({py_param}={param.default!r}) has a default but "
                        f"!{tag}.{field_name} carries none in the descriptor"
                    )
                    assert param.default == descriptor_default, (
                        f"default drift: {ctor_name}({py_param}={param.default!r}) vs "
                        f"!{tag}.{field_name} = {descriptor_default!r} — reconcile the pyo3 "
                        "literal with the serde const in spec::expr"
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
    "adjust_funds", "poll_fills", "set_costs_for",
    # Inherent PaperWallet extras, not trait methods.
    "orders", "reset",
}

WALLET_NOT_BOUND = {
    "take_rejections": (
        "needs a bar-less rejection type; the run path already exposes the same "
        "entries on RunReport.rejections"
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


# --------------------------------------------------------------------------
# The Coinbase live wallet mirrors the same order-flow surface as the OKX one,
# minus the OKX-only demo constructor (Coinbase has no demo environment) — it is
# spot, so it constructs only via `mainnet`. Same ledger discipline.
# --------------------------------------------------------------------------

COINBASE_WALLET_BOUND = {
    # Constructor (staticmethod). No `demo`: Coinbase has no demo environment.
    "mainnet",
    # Wallet reads.
    "funds", "position", "price", "equity",
    # Order flow.
    "update", "set", "set_position", "close",
    "set_stop", "set_take_profit", "cancel_protective",
    "set_limit", "cancel_limit", "cancel", "poll_fills",
    # Live-only extras.
    "refresh_account", "errors",
}

COINBASE_WALLET_NOT_BOUND = {
    "positions": (
        "the Rust wallet enumerates only marked products, so a dict view would "
        "mislead; read one symbol at a time via position(symbol)"
    ),
    "orders": "no in-memory blotter on a live venue (paper-only convenience)",
    "reset": "a live venue has no freshly-constructed state to restore",
    "adjust_funds": "Coinbase takes the trait default (UnsupportedOperation)",
    "take_rejections": (
        "needs a bar-less rejection type; errors() surfaces REST-failure detail"
    ),
    "set_costs_for": "a live venue owns its own fees",
}


def test_coinbase_wallet_surface_matches_the_ledger():
    actual = {n for n in dir(ta.CoinbaseWallet) if not n.startswith("_")}
    missing = COINBASE_WALLET_BOUND - actual
    extra = actual - COINBASE_WALLET_BOUND - set(COINBASE_WALLET_NOT_BOUND)
    assert not missing, f"ledger claims these are bound but they aren't: {sorted(missing)}"
    assert not extra, (
        f"CoinbaseWallet gained methods the ledger doesn't record: {sorted(extra)} — "
        "add them to COINBASE_WALLET_BOUND, or to COINBASE_WALLET_NOT_BOUND with a reason"
    )


def test_protective_legs_expose_their_size():
    """The specific regression the ledger above exists to prevent."""
    w = ta.PaperWallet(1_000.0)
    w.update("A", ta.Candle(10.0, 10.0, 10.0, 10.0, 1.0))
    # Must accept a size; a TypeError here means the binding lost the parameter.
    w.set_stop("A", 9.0, ta.Size.units(1.0))
    w.set_take_profit("A", 11.0, ta.Size.units(1.0))
