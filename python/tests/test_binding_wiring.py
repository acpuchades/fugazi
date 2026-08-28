"""Every binding must compute what its YAML tag computes.

`test_parity.py` proves each spec tag is *reachable* from Python — a bound
constructor, a method, a component accessor, or a declared exemption. It says
nothing about what the binding then *does*. Each one is a thin delegation
(`src_period!(hma, Hma, ...)`, `core_metrics::omega(&returns, threshold)`), and
a delegation pointing at the wrong Rust type is invisible to a reachability
guard: `ta.hma` built as an `Ema`, `ta.rma` as an `Sma`, `ta.metrics.omega` as
`sortino` and `ta.metrics.ulcer_index` as `max_drawdown` all left the suite
green.

The reference here is the **spec layer** — `ta.compute_overlays("col: !hma
{...}")` — not a hand-written number. The two reach the same Rust indicator
through genuinely different builders (the pyo3 constructor versus
`NodeSpec::try_build`), so a mis-wired binding diverges from it, while the tag
side is itself pinned against sibling confusion by `tests/tag_semantics.rs`.
That makes this the join between the two halves of the parity discipline, and
it needs no expected values of its own.

Driven off the same ledgers `test_parity.py` maintains, so a new tag is covered
the day it is bound.
"""

import inspect

import pytest

import fugazi as ta

from test_parity import COMPONENT_BOUND, METHOD_BOUND, NOT_BOUND, RENAMED

# --- the fixture ------------------------------------------------------------

# A rise, a flat stretch, a fall and a choppy tail, with volume that varies
# independently of price. Long enough for a 5-bar window plus a recursive
# smoother's settling tail; varied enough that two different indicators do not
# read alike by accident.
CLOSES = (
    [100.0 + i * 2.0 for i in range(20)]
    + [138.0] * 8
    + [138.0 - i * 3.0 for i in range(1, 11)]
    + [108.0 + (i % 5) * 2.0 - (i % 3) for i in range(1, 15)]
)


# Five days, one hour, one minute and seven seconds apart. The odd stride is
# what makes the calendar tags comparable at all: a whole-day cadence pins
# `hour`/`minute`/`second` at zero and a sub-day one never advances `month`.
# Without stamps the calendar tags read `None` on both sides and passed
# vacuously.
_EPOCH = 1_704_067_200_000  # 2024-01-01T00:00:00Z
_STRIDE = 5 * 86_400_000 + 3_600_000 + 60_000 + 7_000


def _candles():
    bars, prev = [], CLOSES[0]
    for i, close in enumerate(CLOSES):
        pad = 1.0 + (i % 5)
        bars.append(
            ta.Candle(
                prev,
                max(prev, close) + pad,
                min(prev, close) - pad,
                close,
                500.0 + 90.0 * (i % 7),
            )
        )
        prev = close
    return bars


def _atoms():
    """The same bars as timestamped atoms — what both sides are driven over."""
    return [ta.Atom(c, time=_EPOCH + i * _STRIDE) for i, c in enumerate(_candles())]


# The scalar every field of a given name is given, on both sides of the
# comparison. `fast`/`slow`/`signal` must differ or the three MACD projections
# collapse onto each other.
SCALARS = {
    "period": 5,
    "fast": 3,
    "slow": 7,
    "signal": 4,
    "k": 2.0,
    "multiplier": 2.0,
    "ema_period": 5,
    "atr_period": 4,
    "rsi_period": 5,
    "stoch_period": 5,
    "pct": 0.75,
    "lag": 2,
    "step": 0.02,
    "max": 0.2,
    "base": 10.0,
}

# A pyo3 parameter is sometimes spelled longer than the tag field it fills:
# `macd(fast_period=...)` against `!macd_line { fast: ... }`. Both sides must
# receive the *same* number or the comparison is meaningless, so the mapping is
# explicit rather than inferred.
PARAM_TO_FIELD = {
    "fast_period": "fast",
    "slow_period": "slow",
    "signal_period": "signal",
}

# Tags whose Python spelling this sweep cannot assemble generically, each with
# the reason and what covers it instead. Short by design — an entry is a hole.
UNDRIVEN = {
    "pick": "a symbol selector, not a computation over one series — covered by "
    "test_fugazi.py's cross-symbol tests",
    "get": "reads a schema column rather than the bar — covered by the "
    "compute_overlays / get tests in test_fugazi.py",
    "value": "a constant; nothing to cross-check",
    "correlation": "two sources, not one — driven below by _TWO_SOURCE",
    "covariance": "as correlation",
    "beta": "as correlation",
    "if_else": "three sources with a bool condition — covered by test_fugazi.py",
    "resample": "wraps an inner chain over a synthesized bar stream — covered "
    "by the cross-timeframe tests in test_fugazi.py",
    "volume_bars": "as resample",
    "dollar_bars": "as resample",
    "unstable": "a readiness wrapper, not a computation — covered by the "
    "unstable() parity tests in test_fugazi.py",
    "every": "a bar-counter predicate with no source",
    "bars_since": "counts bars since a *boolean* source went true; the filler "
    "has no generic condition to hand it — covered by "
    "test_fugazi.py",
    "sharpe": "embeds a whole strategy; the tag side is pinned by "
    "tests/trailing_risk.rs",
    "sortino": "as sharpe",
    "volatility": "as sharpe",
    "max_drawdown": "as sharpe",
    "calmar": "as sharpe",
}

# Two-source rolling statistics, driven explicitly.
_TWO_SOURCE = ("correlation", "covariance", "beta")

# A module constructor that returns a *multi*-output whose scalar projection
# carries the tag's name. `COMPONENT_BOUND` holds the ones whose tag differs
# from the accessor; this is the one where they coincide.
MODULE_COMPONENT = {"adx": ("adx", "adx")}


def _grammar():
    return {t["name"]: t for t in ta.spec_grammar()["tags"]}


def _fields(tag, grammar):
    """Every field the tag declares, by name — *including the optional ones*.

    Filling only the required ones is what made the first draft of this file
    compare `ta.sma(close, 5)` against a bare `!sma`, whose period defaults to
    something else entirely: almost every tag's scalars are optional.
    """
    return {
        f["name"]: f for form in grammar[tag]["forms"] for f in form.get("fields", [])
    }


def _source_demand(tag, grammar):
    """What the tag's `source` slot wants: `scalar`, `candle`, `atom` or None."""
    f = _fields(tag, grammar).get("source")
    out = (f or {}).get("node_output") or []
    return out[0] if out else None


def _yaml_body(tag, grammar, extra=()):
    """`!tag { field: value, ... }`, every known scalar spelled out."""
    parts = [f"{n}: {SCALARS[n]}" for n in _fields(tag, grammar) if n in SCALARS]
    parts.extend(extra)
    return f"!{tag} {{ {', '.join(parts)} }}" if parts else f"!{tag}"


def _from_yaml(expr):
    """The tag's readings over the fixture, via the spec layer.

    Reads whichever type the column carries — the calendar predicates are
    boolean columns, and `get_real` on one reads `None` on every bar, which
    would make a wrong binding look right.
    """
    schema, out = ta.compute_overlays(_atoms(), f"col: {expr}")
    i = schema.index_of("col")
    reads = [a.overlays.get_real(i) for a in out]
    if all(v is None for v in reads):
        reads = [a.overlays.get_bool(i) for a in out]
    return reads


def _drive(node):
    """The binding's readings over the same fixture."""
    return [node.update(a) for a in _atoms()]


def _same(a, b):
    """Whether the binding's readings match the tag's, bar for bar.

    One asymmetry is tolerated, and only one: `Signal.update` is typed `-> bool`
    and collapses an unwarmed `None` to `False` (see
    `test_a_signal_reports_false_while_unwarmed`), so where the spec side reads
    `None` the Python side is allowed to read `False`. Every other position must
    agree exactly.
    """
    if len(a) != len(b):
        return False
    for x, y in zip(a, b):
        if x is None and y is None:
            continue
        if y is None and x is False:
            continue
        if x is None or y is None:
            return False
        if x != pytest.approx(y, rel=1e-12, abs=1e-12):
            return False
    return True


# --- the sweeps -------------------------------------------------------------


def _module_tags():
    """Tags bound as a module-level function, split by whether they take a source."""
    grammar = _grammar()
    names = {n for n in dir(ta) if not n.startswith("_")}
    with_source, bar_only = [], []
    for tag in ta.spec_tags()["node"]:
        if tag in NOT_BOUND or tag in METHOD_BOUND or tag in COMPONENT_BOUND:
            continue
        if tag in UNDRIVEN:
            continue
        if tag in MODULE_COMPONENT:
            continue
        fn_name = RENAMED.get(tag, tag)
        if fn_name not in names:
            continue
        params = list(inspect.signature(getattr(ta, fn_name)).parameters)
        unknown = [
            p
            for p in params
            if p != "source" and PARAM_TO_FIELD.get(p, p) not in SCALARS
        ]
        if unknown:
            continue
        # Only a `scalar`-demanding `source` takes a real source object; a
        # `candle`/`atom` one is the bar itself and the constructor defaults it.
        if (
            params
            and params[0] == "source"
            and _source_demand(tag, grammar) == "scalar"
        ):
            with_source.append((tag, fn_name, params))
        else:
            # An `atom`/`candle` source is the bar itself: `ta.close()`,
            # `ta.obv()`, `ta.year()` all default it, so the sweep drives them
            # bare and passes only the scalars.
            bar_only.append((tag, fn_name, [p for p in params if p != "source"]))
    return grammar, with_source, bar_only


def test_no_tag_silently_escapes_the_wiring_sweep():
    """Every node tag is bound-and-driven, method-bound, or declared.

    Without this the sweeps below narrow silently: a tag whose signature grows
    a parameter the filler does not know simply stops being compared, and
    nothing goes red.
    """
    grammar, with_source, bar_only = _module_tags()
    driven = {t for t, _, _ in with_source} | {t for t, _, _ in bar_only}
    accounted = (
        driven
        | set(UNDRIVEN)
        | set(NOT_BOUND)
        | set(METHOD_BOUND)
        | set(COMPONENT_BOUND)
        | set(MODULE_COMPONENT)
        | set(_TWO_SOURCE)
    )
    missing = [t for t in ta.spec_tags()["node"] if t not in accounted]
    assert not missing, (
        "these tags are bound but no longer reach the wiring sweep — give the "
        f"filler their parameter, or declare them in UNDRIVEN: {missing}"
    )
    assert len(driven) >= 50, (
        f"only {len(driven)} tags are driven; the sweep has narrowed"
    )


def test_source_constructors_match_their_yaml_tag():
    """`ta.hma(ta.close(), 5)` must equal `!hma { period: 5 }`."""
    grammar, with_source, _ = _module_tags()
    wrong = []
    for tag, fn_name, params in with_source:
        args = [SCALARS[PARAM_TO_FIELD.get(p, p)] for p in params[1:]]
        got = _drive(getattr(ta, fn_name)(ta.close(), *args))
        want = _from_yaml(_yaml_body(tag, grammar, extra=("source: !close",)))
        if not _same(got, want):
            wrong.append(f"!{tag}: ta.{fn_name}(close, {args}) != the tag")
    assert not wrong, "these bindings disagree with their tag:\n  " + "\n  ".join(wrong)


def test_bar_constructors_match_their_yaml_tag():
    """The candle-input constructors — `ta.obv()`, `ta.atr(5)`, `ta.vwap(5)`."""
    grammar, _, bar_only = _module_tags()
    wrong = []
    for tag, fn_name, params in bar_only:
        args = [SCALARS[PARAM_TO_FIELD.get(p, p)] for p in params]
        got = _drive(getattr(ta, fn_name)(*args))
        want = _from_yaml(_yaml_body(tag, grammar))
        if not _same(got, want):
            wrong.append(f"!{tag}: ta.{fn_name}({args}) != the tag")
    assert not wrong, "these bindings disagree with their tag:\n  " + "\n  ".join(wrong)


def test_component_accessors_match_their_yaml_tag():
    """`ta.bollinger(...).shared().upper()` must equal `!bb_upper { ... }`.

    The projection is the interesting half: an accessor returning its sibling's
    field is exactly the bug `tests/tag_semantics.rs` found on the Rust side.
    """
    grammar = _grammar()
    wrong = []
    for tag, (ctor_name, accessor) in {**COMPONENT_BOUND, **MODULE_COMPONENT}.items():
        ctor = getattr(ta, ctor_name)
        params = list(inspect.signature(ctor).parameters)
        args, extra, ok = [], [], True
        for p in params:
            field = PARAM_TO_FIELD.get(p, p)
            if p == "source":
                args.append(ta.close())
                extra.append("source: !close")
            elif p == "high":
                args.append(ta.high())
                extra.append("high: !high")
            elif p == "low":
                args.append(ta.low())
                extra.append("low: !low")
            elif field in SCALARS:
                args.append(SCALARS[field])
            else:
                ok = False
        if not ok:
            continue
        got = _drive(getattr(ctor(*args).shared(), accessor)())
        want = _from_yaml(_yaml_body(tag, grammar, extra=extra))
        if not _same(got, want):
            wrong.append(f"!{tag}: ta.{ctor_name}(...).{accessor}() != the tag")
    assert not wrong, "these accessors disagree with their tag:\n  " + "\n  ".join(
        wrong
    )


def test_operator_methods_match_their_yaml_tag():
    """`ta.close().add(ta.open())` must equal `!add { lhs: !close, rhs: !open }`.

    The operator vocabulary reaches Python as *methods* on `Indicator` /
    `Signal` rather than module functions, so the two constructor sweeps above
    never see it. Shapes are read off the grammar rather than listed: a
    `lhs`/`rhs` pair is binary, a lone `source` unary, `source`+`period` a
    lookback, and so on.

    The boolean operators need boolean operands, so they get a pair of
    partially-overlapping conditions — two mutually exclusive ones would make
    `or` and `xor` the same function and the comparison would prove nothing.
    """
    grammar = _grammar()

    # Two conditions that overlap without being identical.
    def _py_bool(second):
        return ta.close().lt(ta.open()) if second else ta.close().gt(ta.open())

    _YAML_BOOL = ("!gt { lhs: !close, rhs: !open }", "!lt { lhs: !close, rhs: !open }")
    # A payload-carrying wrapper cannot hold a second YAML tag, so its operand
    # goes in the bridged `{tag: body}` spelling the loader also accepts.
    _BRIDGED_BOOL = "{ gt: { lhs: !close, rhs: !open } }"

    wrong, checked = [], 0
    for tag, method in METHOD_BOUND.items():
        fields = _fields(tag, grammar)
        names = set(fields)
        # `!not` carries its operand as a positional payload rather than a
        # field, so the demand has to be read off the form as well or it lands
        # on `Indicator` instead of `Signal` and the method does not exist.
        payload_demand = next(
            (
                form.get("payload_output")
                for form in grammar[tag]["forms"]
                if form.get("payload_output")
            ),
            None,
        )
        demand = (
            (fields.get("lhs") or fields.get("source") or {}).get("node_output")
            or payload_demand
            or []
        )
        wants_bool = demand == ["bool"]
        base = _py_bool(False) if wants_bool else ta.close()
        base_yaml = _YAML_BOOL[0] if wants_bool else "!close"
        # A tag that accepts either domain (`!changed` demands
        # `["bool", "scalar"]`) is bound on `Signal` only, so fall back to the
        # boolean operand when the numeric receiver does not carry the method.
        if not hasattr(base, method):
            base, base_yaml, wants_bool = _py_bool(False), _YAML_BOOL[0], True

        if {"lhs", "rhs"} <= names:
            rhs = _py_bool(True) if wants_bool else ta.open()
            rhs_yaml = _YAML_BOOL[1] if wants_bool else "!open"
            got = _drive(getattr(base, method)(rhs))
            want = _from_yaml(f"!{tag} {{ lhs: {base_yaml}, rhs: {rhs_yaml} }}")
        elif "period" in names:
            got = _drive(getattr(base, method)(SCALARS["period"]))
            want = _from_yaml(
                f"!{tag} {{ source: {base_yaml}, period: {SCALARS['period']} }}"
            )
        elif "level" in names:
            got = _drive(getattr(base, method)(110.0))
            want = _from_yaml(f"!{tag} {{ source: {base_yaml}, level: 110.0 }}")
        elif {"lower", "upper"} <= names:
            got = _drive(base.clamp(ta.low(), ta.high()))
            want = _from_yaml(
                f"!{tag} {{ source: {base_yaml}, lower: !low, upper: !high }}"
            )
        elif names <= {"source"} or not names:
            # `!not` carries its operand as a positional payload, not a field.
            got = _drive(getattr(base, method)())
            if "source" in names:
                want = _from_yaml(f"!{tag} {{ source: {base_yaml} }}")
            else:
                inner = _BRIDGED_BOOL if wants_bool else "{ close: null }"
                want = _from_yaml(f"!{tag} {inner}")
        else:
            continue
        checked += 1
        if not _same(got, want):
            wrong.append(f"!{tag}: .{method}() != the tag")

    # Every ledger entry, not "enough of them": a shape the classifier stops
    # recognising would otherwise drop out of the comparison silently.
    assert checked == len(METHOD_BOUND), (
        f"compared {checked} of {len(METHOD_BOUND)} method-bound tags — a shape "
        "fell through the classifier"
    )
    assert not wrong, "these methods disagree with their tag:\n  " + "\n  ".join(wrong)


def test_two_source_statistics_match_their_yaml_tag():
    """`ta.correlation(close, open, 5)` must equal `!correlation { lhs, rhs }`."""
    wrong = []
    for tag in _TWO_SOURCE:
        got = _drive(getattr(ta, tag)(ta.close(), ta.open(), 5))
        want = _from_yaml(f"!{tag} {{ lhs: !close, rhs: !open, period: 5 }}")
        if not _same(got, want):
            wrong.append(f"!{tag}")
    assert not wrong, f"these two-source statistics disagree with their tag: {wrong}"


def test_no_undriven_entry_names_a_tag_that_no_longer_exists():
    """A stale exemption reads as 'covered elsewhere' for something gone."""
    known = set(ta.spec_tags()["node"])
    stale = sorted(t for t in UNDRIVEN if t not in known)
    assert not stale, f"UNDRIVEN names tags that no longer exist: {stale}"


def test_a_signal_reports_false_while_unwarmed():
    """`Signal.update` cannot say "not ready yet", and that is load-bearing here.

    `Indicator.update` returns `None` until its sources are warm, and so does
    the spec layer — the crate's stated rule is that a comparison or edge is
    `None` until every source is warmed, "so an edge coincident with warm-up
    isn't detected". The Python `Signal` is typed `-> bool` and answers `False`
    instead, in all four of its rooted forms.

    That is why `_same` above tolerates `False` against the tag's `None`. Pinned
    explicitly so the tolerance is a recorded decision rather than a silent
    weakening: if `Signal.update` ever gains an `Optional[bool]` return, this
    test fails and the tolerance in `_same` should go with it.

    Note the collapse is confined to hand-driving a `Signal` from Python — a
    strategy's readiness is gated by `is_ready()` inside Rust, so no trade is
    taken off an unwarmed signal.
    """
    unwarmed = ta.close().gt(ta.sma(ta.close(), 5))
    reads = _drive(unwarmed)

    assert reads[0] is False, (
        "an unwarmed Signal reports False rather than None; if this changed to "
        "None, drop the None/False tolerance in `_same`"
    )
    assert all(isinstance(r, bool) for r in reads), "Signal.update is typed -> bool"

    # The same expression through the spec layer *does* distinguish the two,
    # which is the asymmetry being recorded.
    via_spec = _from_yaml(
        "!gt { lhs: !close, rhs: !sma { period: 5, source: !close } }"
    )
    assert via_spec[0] is None
    assert any(r is not None for r in via_spec), "the spec side must warm up too"
