#!/usr/bin/env python3
"""Generate `python/fugazi/__init__.pyi` from the built extension module.

Why generated
-------------
The surface is ~166 functions and ~311 class members. Hand-writing that once is
a day; keeping it honest across every release is the part that fails. So the
*shape* — parameter names, defaults, which are keyword-only, what exists at all
— is introspected from the module itself and cannot drift. Only the **types**
are curated here, and `TYPES` below is checked for completeness at generation
time: a new binding fails this script until someone classifies it, the same
discipline `python/tests/test_parity.py` applies to spec tags.

Run it with `python tools/gen_python_stubs.py` after a `maturin develop`.
`python/tests/test_stubs.py` regenerates and diffs, so CI fails on a stale stub.
"""

from __future__ import annotations

import inspect
import pathlib
import sys
import types

import fugazi as ta
import fugazi.metrics as ta_metrics
import fugazi.montecarlo as ta_montecarlo

ROOT = pathlib.Path(__file__).resolve().parent.parent
OUT = ROOT / "python" / "fugazi" / "__init__.pyi"
METRICS_OUT = ROOT / "python" / "fugazi" / "metrics.pyi"
MONTECARLO_OUT = ROOT / "python" / "fugazi" / "montecarlo.pyi"

# --------------------------------------------------------------------------
# Type vocabulary
# --------------------------------------------------------------------------

# A series argument: anything the `Series` / `Column` extractor accepts — a
# NumPy array, a pandas/polars Series, or any sequence of floats.
SERIES = "Sequence[float] | Any"
# A frame argument for `feed()`: a DataFrame, a dict of columns, or a 1-D series.
FRAME = "Any"
# What `feed()` gives back: pandas/polars Series when given one, else ndarray,
# else a plain list when NumPy is absent.
FED = "Any"

#: Parameter types, by name. Applied wherever a more specific rule does not.
BY_PARAM = {
    "period": "int",
    "fast_period": "int",
    "slow_period": "int",
    "signal_period": "int",
    "ema_period": "int",
    "atr_period": "int",
    "rsi_period": "int",
    "stoch_period": "int",
    "lag": "int",
    "symbol": "str",
    "symbols": "Sequence[str]",
    "freq": "str | Frequency | None",
    # An opaque stream id: any str, or a Frequency whose token is used.
    "stream": "str | Frequency | None",
    "key": "str",
    "name": "str",
    "schema": "Schema",
    "epsilon": "float | None",
    "level": "float",
    "base": "float",
    "k": "float",
    "multiplier": "float",
    "step": "float",
    "max": "float",
    "cash": "float",
    "max_gross": "float | None",
    "leverage": "float",
    "margin_rate": "float",
    "maintenance_margin": "float | None",
    "bar_freq": "str | Frequency | None",
    "initial_equity": "float",
    "bars_per_year": "float",
    "risk_free_rate": "float",
    "seconds_per_bar": "float | None",
    "risk_aversion": "float",
    "smooth_min_support": "float",
    "windowed": "int | None",
    "jobs": "int | None",
    "text": "str",
    "kind": "str",
    "best_by": "str | None",
    "smooth": "str | None",
    "smooth_scale": "str | None",
    "base_dir": "str | None",
    "import_root": "str | None",
    "imports": "bool",
    "shrink": "bool",
    "metric_names": "list[str] | None",
    "params": "Mapping[str, Any] | None",
    "grid": "Sequence[Mapping[str, Any]] | None",
    "walkforward": "tuple[int, int] | tuple[int, int, int] | None",
    "costs": "TradingCostsConfig | Mapping[str, Any] | None",
    "montecarlo": "MonteCarloConfig | None",
    "wallet": "Wallet",
    "snapshots": "Sequence[Snapshot | Mapping[str, Atom | Candle]]",
    "resume": "str | None",
    "flatten": "bool",
    "hold": "Mapping[str, float] | None",
    # `run_resumable(rebalance=...)`. Every `rebalance_on` builder names its
    # parameter `signal`, so this name is unambiguously the boolean.
    "rebalance": "bool",
    "report": "RunReport",
    "equity_curve": SERIES,
    "returns": SERIES,
    "fills": "Sequence[Fill] | None",
    "ruin_bar": "int | None",
    "since": "str",
    "until": "str | None",
    "output": "str",
    "api_key": "str | None",
    "vs_currency": "str | None",
    "base_url": "str | None",
    "user_agent": "str | None",
    "adjusted": "bool",
    "market": "str",
    "permutations": "int",
    "scheme": "str",
    "block": "float",
    "seed": "int",
    "ci_level": "float",
    "null": "str",
    "metrics": "list[str] | None",
}

#: Return type by module-level function name. Every registered function must
#: appear here or in one of the prefix rules below — see `classify_return`.
RETURNS = {
    # --- leaves -----------------------------------------------------------
    **{
        n: "Indicator"
        for n in (
            "open high low close volume typical median identity value sma ema rma wma hma rsi "
            "stddev skewness kurtosis zscore correlation percentile percentile_rank bars_since "
            "bars_since_high bars_since_low variance_ratio stochastic cci log exp atr parkinson "
            "garman_klass rogers_satchell mfi williams_r obv vwap ad true_range resample latch "
            "volume_bars dollar_bars "
            "unstable if_else get_real year month day hour minute second day_of_week day_of_year "
            "week_of_year quarter unix_seconds unix_millis covariance beta"
        ).split()
    },
    **{n: "Signal" for n in "is_weekday is_weekend every get_bool".split()},
    **{
        n: "MultiIndicator"
        for n in "adx dmi aroon sar macd bollinger keltner donchian stoch_rsi linreg".split()
    },
    "value_str": "StrSource",
    "get_str": "StrSource",
    "get": "Indicator | Signal | StrSource",
    "pick": "AtomSource",
    "compute_overlays": "Any",
    # --- strategy presets and selection ------------------------------------
    **{
        n: "Strategy"
        for n in "buy_and_hold ma_crossover rsi_reversal donchian_breakout keltner_breakout".split()
    },
    **{n: "Selection" for n in "everything top_bottom threshold quantile".split()},
    **{
        n: "Indicator"
        for n in "sharpe_of sortino_of volatility_of max_drawdown_of calmar_of".split()
    },
    # --- spec / data --------------------------------------------------------
    "load_spec": "StrategySpec",
    "check_spec": "SpecCheck",
    "optimize": "Sweep | WalkForwardResult | PanelWalkForwardResult",
    "evaluate_report": "dict[str, Any]",
    "fetch": "Any",
    "tickers": "list[str]",
    "slot_demand": "list[str] | None",
    "slot_demands": "dict[str, list[str] | None]",
    "spec_tags": "dict[str, list[str]]",
    "spec_grammar": "dict[str, Any]",
    "spec_json_schema": "dict[str, Any]",
    "spec_document_json_schema": "dict[str, Any]",
    # --- unpickling entry points -------------------------------------------
    "_rebuild_schema": "Schema",
    "_rebuild_snapshot": "Snapshot",
    "_rebuild_size": "Size",
    "_rebuild_order": "Order",
    "_rebuild_run_report": "RunReport",
}

#: Return type by `Class.member`. Members not listed fall back to `MEMBER_RULES`.
MEMBER_RETURNS = {
    ("Indicator", "update"): "float | None",
    ("Indicator", "value"): "float | None",
    ("Indicator", "feed"): FED,
    ("Signal", "update"): "bool",
    ("Signal", "feed"): FED,
    ("Signal", "is_true"): "bool",
    ("StrSource", "update"): "str | None",
    ("StrSource", "value"): "str | None",
    ("StrSource", "feed"): FED,
    # A wallet counts as it charges, so it always has a figure; a `RunReport`
    # may have been built by hand, and `None` there is "does not say".
    ("RunReport", "carry_coverage"): "tuple[int, int] | None",
    # `(count, worst_ratio)` over the materially-fitted fills, or `None` when
    # every fill got what it asked for.
    ("RunReport", "materially_fitted"): "tuple[int, float] | None",
    ("MultiIndicator", "update"): "dict[str, float] | None",
    ("MultiIndicator", "value"): "dict[str, float] | None",
    ("MultiIndicator", "feed"): FED,
    ("MultiIndicator", "shared"): "SharedMultiIndicator",
    ("AtomSource", "update"): "Atom | None",
    ("AtomSource", "value"): "Atom | None",
    ("Snapshot", "get"): "Atom | None",
    ("Snapshot", "find"): "Atom | None",
    ("Snapshot", "sole_atom"): "Atom | None",
    ("Snapshot", "keys"): "list[Selector]",
    ("Snapshot", "values"): "list[Atom]",
    ("Snapshot", "items"): "list[tuple[Selector, Atom]]",
    ("Schema", "keys"): "list[str]",
    ("Schema", "index_of"): "int | None",
    ("Schema", "type_of"): "str | None",
    ("Schema", "type_of_key"): "str | None",
    ("SchemaBuilder", "finish"): "Schema",
    ("OverlayInfo", "get"): "float | bool | str | None",
    ("OverlayInfo", "get_by_key"): "float | bool | str | None",
    ("OverlayInfo", "get_real"): "float | None",
    ("OverlayInfo", "get_bool"): "bool | None",
    ("OverlayInfo", "get_str"): "str | None",
    ("OverlayInfo", "values"): "list[float | bool | str | None]",
    ("SharedMultiIndicator", "names"): "list[str]",
    # `check_spec`'s two records. The type labels are the crate's own
    # (`number` / `string` / `bool` / `list` / `table` / `expression`, and the
    # four `type:` declarations), so they stay `str` rather than a Literal that
    # would have to be resynchronised every time the grammar grows one.
    ("SpecCheck", "holes"): "list[SpecHole]",
    ("SpecCheck", "param_types"): "dict[str, str | None]",
    ("SpecCheck", "built"): "bool",
    ("SpecHole", "name"): "str",
    ("SpecHole", "origin"): "str",
    ("SpecHole", "declared"): "str | None",
    ("SpecHole", "demanded"): "list[str]",
    ("SpecHole", "used"): "list[str]",
    ("SpecHole", "required_type"): "str | None",
    ("StrategySpec", "run"): "RunReport",
    ("StrategySpec", "run_resumable"): "tuple[RunReport, str]",
    ("StrategySpec", "warm_up"): "str",
    ("StrategySpec", "evaluate"): "dict[str, Any]",
    ("Sweep", "best"): "SweepRow | None",
    ("Sweep", "rows"): "list[SweepRow]",
    # `{member: {axis: value}}` under `shrink=` — each member's own parameters.
    ("Sweep", "member_winners"): "dict[str, dict[str, Any]]",
    # How many independent searches over the grid those selections amounted to.
    ("Sweep", "independent_searches"): "float | None",
    ("Sweep", "shrinkage"): "PanelShrinkage | None",
    ("Sweep", "shrunk"): "bool",
    # `(mean, std, defined, members)` of the member-demeaned score — the same
    # 4-tuple layout as `PanelWalkForwardResult.breadth`.
    ("SweepRow", "demeaned"): "DemeanedScore | None",
    ("PanelFold", "shrinkage"): "PanelShrinkage | None",
    ("PanelFold", "member_winners"): "dict[str, dict[str, Any]]",
    ("PanelFold", "departed"): "list[str]",
    ("PanelWalkForwardResult", "shrinkage"): "PanelShrinkage | None",
    # `{member: folds}`, most-frequent first.
    ("PanelWalkForwardResult", "departures"): "dict[str, int]",
    # `lambda` is a Python keyword, so the quantity is spelled out rather than
    # escaped — see `PanelShrinkage.disagreement`.
    ("PanelShrinkage", "disagreement"): "float | None",
    ("PanelShrinkage", "parameter_matters"): "bool",
    ("PanelShrinkage", "verdict"): "str",
    ("PanelShrinkage", "support"): "float",
    ("PanelShrinkage", "cells"): "int",
    ("PanelShrinkage", "live_rows"): "int",
    ("PanelShrinkage", "live_members"): "int",
    ("PanelShrinkage", "row_variance"): "float",
    ("PanelShrinkage", "member_variance"): "float",
    ("PanelShrinkage", "interaction_variance"): "float",
    ("PanelShrinkage", "residual_variance"): "float | None",
    ("PanelShrinkage", "mean_replicates"): "float",
    ("PanelShrinkage", "balanced"): "bool",
    # The estimator, reachable without `optimize(panel=)`.
    ("ScoreTable", "rows"): "int",
    ("ScoreTable", "members"): "int",
    ("ScoreTable", "populated"): "int",
    ("ScoreTable", "observations"): "int",
    ("ScoreTable", "replicated_cells"): "int",
    ("ScoreTable", "cell"): "list[float]",
    ("ScoreTable", "cell_mean"): "float | None",
    ("ScoreTable", "extend"): "None",
    ("ScoreTable", "from_cells"): "ScoreTable",
    ("ScoreTable", "decompose"): "PanelDecomposition | None",
    ("PanelDecomposition", "summary"): "PanelShrinkage",
    # `rows x members`, `None` where the cell is unpopulated.
    ("PanelDecomposition", "demeaned"): "list[list[float | None]]",
    ("PanelDecomposition", "shrunk"): "list[list[float | None]] | None",
    ("PanelDecomposition", "interactions"): "list[list[float | None]]",
    ("PanelDecomposition", "row_effects"): "list[float | None]",
    ("PanelDecomposition", "member_effects"): "list[float | None]",
    ("PanelDecomposition", "grand_mean"): "float",
    ("PanelDecomposition", "selection_breadth"): "PanelBreadth | None",
    # Named records that still iterate/index/compare as their 4-tuple — see
    # `test_breadth_and_demeaned_are_named_but_still_tuples`.
    ("PanelBreadth", "effective"): "float",
    ("PanelBreadth", "mean_correlation"): "float",
    ("PanelBreadth", "members"): "int",
    ("PanelBreadth", "pairs"): "int",
    ("DemeanedScore", "mean"): "float",
    ("DemeanedScore", "std"): "float",
    ("DemeanedScore", "defined"): "int",
    ("DemeanedScore", "members"): "int",
    ("Sweep", "departed"): "list[str]",
    # `{member: [metrics per window]}` — the estimator's replicates.
    ("SweepRow", "metrics_panel_windowed"): "dict[str, list[dict[str, Any]]] | None",
    ("WalkForwardResult", "folds"): "list[WalkForwardFold]",
    ("RunReport", "fills"): "list[Fill]",
    ("RunReport", "rejections"): "list[Rejected]",
    ("RunReport", "equity_curve"): "list[float]",
    ("RunReport", "equity_array"): "Any",
    ("RunReport", "ruin_bar"): "int | None",
    ("RunReport", "attribution"): "Attribution | None",
    # The per-child decomposition of a composite run.
    ("Attribution", "fills"): "list[ChildFill]",
    ("Attribution", "equity"): "list[list[float]]",
    ("Attribution", "child_count"): "int",
    ("Attribution", "child_equity"): "list[float]",
    ("ChildFill", "child"): "int",
    ("ChildFill", "crossed"): "bool",
    ("ChildFill", "order"): "Order",
    # --- value types --------------------------------------------------------
    ("Candle", "open"): "float",
    ("Candle", "high"): "float",
    ("Candle", "low"): "float",
    ("Candle", "volume"): "float",
    ("Atom", "candle"): "Candle | None",
    ("Atom", "overlays"): "OverlayInfo | None",
    ("Atom", "time"): "int | None",
    ("Atom", "is_priceable"): "bool",
    ("OverlayInfo", "schema"): "Schema",
    ("Selector", "stream"): "str | None",
    ("Fill", "order"): "Order",
    ("Size", "value"): "float",
    ("Size", "units"): "Size",
    ("Size", "funds_frac"): "Size",
    ("Size", "value_frac"): "Size",
    ("Size", "position_frac"): "Size",
    ("StrSource", "eq"): "Signal",
    ("StrSource", "ne"): "Signal",
    # --- builder shapes that name their own class ---------------------------
    **{
        ("BasketStrategy", m): "BasketStrategy"
        for m in (
            "all_of any_of balance_sides quantile scored_by sized_by threshold top_bottom"
        ).split()
    },
    **{
        ("MultiAssetStrategy", m): "MultiAssetStrategy" for m in "all_of any_of".split()
    },
    **{
        ("PairsStrategy", m): "PairsStrategy"
        for m in (
            "on spread_stop_loss spread_take_profit long_spread_stop_loss "
            "long_spread_take_profit short_spread_stop_loss short_spread_take_profit"
        ).split()
    },
    # --- providers and live wallets -----------------------------------------
    ("Binance", "tickers"): "list[str]",
    ("BinanceVision", "tickers"): "list[str]",
    ("CoinGecko", "tickers"): "list[str]",
    ("Coinbase", "tickers"): "list[str]",
    ("Kraken", "tickers"): "list[str]",
    ("Okx", "tickers"): "list[str]",
    ("Yahoo", "tickers"): "list[str]",
    ("OkxWallet", "demo"): "OkxWallet",
    ("OkxWallet", "mainnet"): "OkxWallet",
    ("CoinbaseWallet", "mainnet"): "CoinbaseWallet",
    ("KrakenWallet", "mainnet"): "KrakenWallet",
    # --- sweep / walk-forward rows ------------------------------------------
    ("SweepRow", "metrics_windowed"): "list[dict[str, Any]] | None",
    # Pooled (`panel=`) rows: per-member documents keyed by member name, and the
    # `(defined, members)` support behind each pooled mean.
    ("SweepRow", "metrics_panel"): "dict[str, dict[str, Any]] | None",
    ("SweepRow", "metrics_support"): "dict[str, tuple[int, int] | None] | None",
    ("SweepRow", "ruin_bar"): "int | None",
    ("SweepRow", "ruined"): "bool",
    ("SweepRow", "smoothed"): "float | None",
    ("SweepRow", "support"): "int | None",
    ("WalkForwardFold", "fold"): "int",
    ("WalkForwardFold", "is_range"): "tuple[int, int]",
    ("WalkForwardFold", "oos_range"): "tuple[int, int]",
    ("WalkForwardFold", "is_smoothed"): "float | None",
    ("WalkForwardFold", "is_support"): "int | None",
    ("WalkForwardResult", "cash"): "float",
    ("WalkForwardResult", "composite_equity"): "list[float]",
    ("WalkForwardResult", "composite_fills"): "list[Fill]",
    ("WalkForwardResult", "embargo_bars"): "int",
    ("WalkForwardResult", "is_bars"): "int",
    ("WalkForwardResult", "oos_bars"): "int",
    ("WalkForwardResult", "prefix_skip"): "int",
    ("PanelFold", "fold"): "int",
    ("PanelFold", "is_range"): "tuple[int, int]",
    ("PanelFold", "oos_range"): "tuple[int, int]",
    ("PanelFold", "is_smoothed"): "float | None",
    ("PanelFold", "is_support"): "float | None",
    ("PanelFold", "is_support_members"): "int",
    ("PanelFold", "oos_support_members"): "int",
    ("PanelFold", "metrics_is"): "dict[str, dict[str, Any]]",
    ("PanelFold", "metrics_oos"): "dict[str, dict[str, Any]]",
    ("PanelWalkForwardResult", "axis_len"): "int",
    ("PanelWalkForwardResult", "cash"): "float",
    ("PanelWalkForwardResult", "composites"): "list[MemberComposite]",
    (
        "PanelWalkForwardResult",
        "effective_breadth",
    ): "tuple[float, float, int, int] | None",
    ("PanelWalkForwardResult", "embargo_bars"): "int",
    ("PanelWalkForwardResult", "folds"): "list[PanelFold]",
    ("PanelWalkForwardResult", "is_bars"): "int",
    ("PanelWalkForwardResult", "members"): "list[str]",
    ("PanelWalkForwardResult", "oos_bars"): "int",
    ("PanelWalkForwardResult", "prefix_skip"): "int",
    ("PanelWalkForwardResult", "pooled"): "tuple[float, float, int, int] | None",
    ("MemberComposite", "member"): "str",
    ("MemberComposite", "equity"): "list[float]",
    ("MemberComposite", "fills"): "list[Fill]",
    # --- fugazi.metrics result types ----------------------------------------
    **{("Trade", m): "int" for m in "entry_bar exit_bar bars_held".split()},
    **{
        ("Trade", m): "float" for m in "entry_price exit_price pnl return_ratio".split()
    },
    **{
        ("DrawdownSegment", m): "int"
        for m in "peak_bar trough_bar duration_bars underwater_bars".split()
    },
    ("DrawdownSegment", "depth_ratio"): "float",
}

#: Fallbacks by member name, applied when `MEMBER_RETURNS` has no entry.
MEMBER_RULES = {
    "warm_up_bars": "int",
    "unstable_bars": "int",
    "stable_bars": "int",
    "is_ready": "bool",
    "is_empty": "bool",
    "reset": "None",
    "can_short": "bool",
    "quote_ccy": "str | None",
    "data_sources": "list[str]",
    # `None` is "this wallet does not say" — a spot venue has no such concept,
    # and a live one may not have been able to ask yet.
    "leverage": "float | None",
    "refresh_leverage": "float",
    "margin_rate": "float",
    # `None` = no margin call is modelled, which is the default.
    "maintenance_margin": "float | None",
    # (bars that wanted a carry rate, bars that got one)
    "carry_coverage": "tuple[int, int]",
    "equity": "float",
    "funds": "float",
    "position": "float",
    "price": "float | None",
    "positions": "dict[str, float]",
    "orders": "list[Order]",
    "poll_fills": "list[Order]",
    "errors": "list[str]",
    "refresh_account": "None",
    "adjust_funds": "None",
    "set_retention": "None",
    "retention": "int | None",
    "set_costs_for": "None",
    "set_costs_for_all": "None",
    "cancel": "None",
    "cancel_limit": "None",
    "cancel_protective": "None",
    "close": "Order | None",
    "set": "Order | None",
    "set_position": "Order | None",
    "set_stop": "Order | None",
    "set_take_profit": "Order | None",
    "set_limit": "Order | None",
    "update": "list[Order]",
    "run": "RunReport",
    "meta": "Any",
    "reads": "list[str]",
    "kind": "str",
    "names": "list[str]",
    "component": "Indicator",
    "typical": "float",
    "median": "float",
    "matches": "bool",
    "push": "None",
    "add": "int",
    "add_real": "int",
    "add_bool": "int",
    "add_str": "int",
    "fetch": "Any",
    "signed_units": "float",
    "commission": "float",
    "units": "float",
    "requested_units": "float",
    "fill_ratio": "float",
    "is_materially_fitted": "bool",
    "deployment": "float",
    "side": "str",
    "id": "int",
    "bar": "int",
    "symbol": "str",
    "error": "str",
    "initial_equity": "float",
    "columns": "list[str]",
    "metric_columns": "list[tuple[str, str]]",
}

#: Builder methods that return a reconfigured copy of their own class.
SELF_RETURNING = {
    "long_on",
    "short_on",
    "position_sizing",
    "rebalance_on",
    "selection",
    "universe",
    "score",
    "sizing",
    "long_spread_on",
    "short_spread_on",
    "add",
    "weights",
    "weight_shares",
    "position_rebalancer",
    "unstable",
}

#: Classes whose builder methods are copy-on-write.
BUILDER_CLASSES = {
    "Strategy",
    "PairsStrategy",
    "MultiAssetStrategy",
    "BasketStrategy",
    "Portfolio",
}

# Indicator/Signal operator methods, whose result type is fixed by the operator.
INDICATOR_TO_SIGNAL = {
    "gt",
    "lt",
    "ge",
    "le",
    "eq",
    "ne",
    "above",
    "below",
    "crosses_above",
    "crosses_below",
}
INDICATOR_TO_INDICATOR = {
    "add",
    "sub",
    "mul",
    "div",
    "pow",
    "lag",
    "diff",
    "ratio",
    "roc",
    "rolling_max",
    "rolling_min",
    "max",
    "min",
    "clamp",
    "log",
    "exp",
    "abs",
    "sign",
    "sqrt",
    "tanh",
    "sigmoid",
    "cum_sum",
    "cum_max",
    "cum_min",
    "unstable",
}
SIGNAL_TO_SIGNAL = {"and_", "or_", "xor_", "not_", "changed", "unstable"}

OPERAND = "Indicator | float"

HEADER = '''"""Type stubs for the `fugazi` extension module.

GENERATED by `tools/gen_python_stubs.py` — do not edit by hand. Regenerate with:

    python tools/gen_python_stubs.py

Signatures (names, defaults, keyword-only boundaries) are introspected from the
built module, so they cannot drift from it. The types are curated in the
generator, which refuses to run if a binding it does not classify appears.
"""

from collections.abc import Iterator, Mapping, Sequence
from typing import Any, Protocol

__version__: str
DEFAULT_TOLERANCE_ABS: float
DEFAULT_TOLERANCE_REL: float
MATERIALLY_FITTED: float

class FugaziError(ValueError): ...
class SpecError(FugaziError): ...
class WalletError(FugaziError): ...
class FetchError(FugaziError): ...
'''


#: pyo3 exposes a `#[getter]` as a `getset_descriptor`, not a `property`, so
#: `isinstance(attr, property)` misses every one of them and they would be
#: emitted as plain (assignable) attributes on classes that are frozen.
GETSET = (types.GetSetDescriptorType, types.MemberDescriptorType)


def docstring(obj: object) -> str:
    """The leading paragraph of `obj`'s docstring, whitespace-normalised.

    A first *line* usually cuts mid-sentence, and the stub is what a type checker
    shows on hover — so take everything up to the first blank line and join it.
    """
    para: list[str] = []
    for line in (inspect.getdoc(obj) or "").splitlines():
        if not line.strip():
            break
        para.append(line.strip())
    return " ".join(para).replace('"""', "'''")


def indent_doc(text: str, pad: str) -> str:
    """Render a docstring body, wrapped, at indentation `pad`."""
    import textwrap

    wrapped = textwrap.wrap(text, width=84 - len(pad)) or [""]
    quote = '"""'
    if len(wrapped) == 1:
        return f"{pad}{quote}{wrapped[0]}{quote}"
    body = [f"{pad}{quote}{wrapped[0]}"] + [f"{pad}{w}" for w in wrapped[1:]]
    return "\n".join(body) + f"\n{pad}{quote}"


def fmt_default(value: object) -> str:
    return "..." if value is not inspect.Parameter.empty else ""


def param_type(owner: str, func: str, name: str) -> str:
    """The annotation for one parameter."""
    if owner in ("Indicator",) and name in ("other", "lower", "upper"):
        return OPERAND
    if owner == "Signal" and name == "other":
        return "Signal"
    if name == "source":
        # A leaf takes an AtomSource (`pick(...)`); a wrapper takes an Indicator.
        return (
            "AtomSource | None"
            if func in RETURNS and func in LEAF_NAMES
            else "Indicator"
        )
    if name in ("lhs", "rhs"):
        return OPERAND
    if name == "sample":
        return "Candle | Atom | Snapshot | Mapping[str, Any] | float"
    if name == "data":
        return FRAME
    if name in ("enter", "exit", "signal"):
        return "Signal | None" if name == "exit" else "Signal"
    if name == "size":
        return "Size | float | None"
    if name in ("trigger", "target", "delta", "fraction", "value", "funds"):
        return "float"
    if name == "index":
        return "int"
    if name == "atom":
        return "Atom"
    if name == "candle":
        return "Candle | None"
    if name == "overlays":
        return "OverlayInfo | None"
    if name == "time":
        return "int | None"
    if name in ("values", "mapping", "children"):
        return "Any"
    if name == "order":
        return "Order"
    if name == "entries":
        return "int | None"
    return BY_PARAM.get(name, "Any")


LEAF_NAMES: set[str] = set()


def signature_text(owner: str, name: str, obj: object, ret: str) -> str | None:
    try:
        sig = inspect.signature(obj)
    except (TypeError, ValueError):
        return None
    parts: list[str] = []
    seen_kwonly = False
    for p in sig.parameters.values():
        if p.name == "self":
            parts.append("self")
            continue
        if p.kind is p.VAR_KEYWORD:
            parts.append(f"**{p.name}: Any")
            continue
        if p.kind is p.KEYWORD_ONLY and not seen_kwonly:
            parts.append("*")
            seen_kwonly = True
        ann = param_type(owner, name, p.name)
        if p.default is not p.empty:
            parts.append(f"{p.name}: {ann} = ...")
        else:
            parts.append(f"{p.name}: {ann}")
    return f"({', '.join(parts)}) -> {ret}"


def member_return(cls_name: str, name: str) -> tuple[str, bool]:
    """The annotation, and whether a rule actually matched.

    The second half matters: `feed` is legitimately `Any`, so "the answer is
    Any" cannot stand in for "nobody classified this".
    """
    if (cls_name, name) in MEMBER_RETURNS:
        return MEMBER_RETURNS[(cls_name, name)], True
    if cls_name == "Indicator" and name in INDICATOR_TO_SIGNAL:
        return "Signal", True
    if cls_name == "Indicator" and name in INDICATOR_TO_INDICATOR:
        return "Indicator", True
    if cls_name == "Signal" and name in SIGNAL_TO_SIGNAL:
        return "Signal", True
    if cls_name == "SharedMultiIndicator":
        return "Indicator", True
    if cls_name in BUILDER_CLASSES and name in SELF_RETURNING:
        return cls_name, True
    if cls_name in (
        "Sweep",
        "SweepRow",
        "WalkForwardResult",
        "WalkForwardFold",
        "PanelFold",
        "MemberComposite",
    ) and name in (
        "values",
        "metrics",
        "is_metrics",
        "oos_metrics",
        "composite_metrics",
    ):
        return "dict[str, Any]", True
    if name in MEMBER_RULES:
        return MEMBER_RULES[name], True
    return "Any", False


def emit_class(name: str, cls: type) -> tuple[list[str], list[str]]:
    """Return (stub lines, unclassified member names)."""
    lines = [f"class {name}:"]
    unknown: list[str] = []
    body: list[str] = []

    if summary := docstring(cls):
        body.append(indent_doc(summary, "    "))

    init = signature_text(name, "__init__", cls, "None")
    if init is not None and init != "() -> None":
        # `inspect.signature(cls)` describes the *call*, so it carries no `self`.
        body.append(f"    def __init__(self, {init[1:]}: ...")

    for member in sorted(m for m in dir(cls) if not m.startswith("_")):
        attr = inspect.getattr_static(cls, member, None)
        ret, matched = member_return(name, member)
        if not matched:
            unknown.append(f"{name}.{member}")
        if isinstance(attr, (property, *GETSET)):
            body.append("    @property")
            body.append(f"    def {member}(self) -> {ret}: ...")
            continue
        obj = getattr(cls, member)
        sig = signature_text(name, member, obj, ret)
        if sig is None:
            body.append(f"    {member}: {ret}")
            continue
        if isinstance(attr, staticmethod):
            body.append("    @staticmethod")
            body.append(f"    def {member}{sig}: ...")
        else:
            if not sig.startswith("(self"):
                sig = "(self, " + sig[1:] if sig[1] != ")" else "(self) -> " + ret
            body.append(f"    def {member}{sig}: ...")

    for dunder, sig in dunders_for(name, cls):
        body.append(f"    def {dunder}{sig}: ...")

    if not body:
        body.append("    ...")
    lines.extend(body)
    return lines, unknown


DUNDER_RETURNS = {
    "__len__": "int",
    "__contains__": "bool",
    "__iter__": "Iterator[Any]",
    "__hash__": "int",
    "__repr__": "str",
    "__str__": "str",
    "__bool__": "bool",
}


def dunders_for(cls_name: str, cls: type) -> list[tuple[str, str]]:
    """Operator/protocol dunders worth stating — the ones a checker uses."""
    out: list[tuple[str, str]] = []
    binary_ind = {
        "__add__": "Indicator",
        "__sub__": "Indicator",
        "__mul__": "Indicator",
        "__truediv__": "Indicator",
        "__radd__": "Indicator",
        "__rsub__": "Indicator",
        "__rmul__": "Indicator",
        "__rtruediv__": "Indicator",
        "__gt__": "Signal",
        "__lt__": "Signal",
        "__ge__": "Signal",
        "__le__": "Signal",
    }
    if cls_name == "Indicator":
        for d, r in binary_ind.items():
            if hasattr(cls, d):
                out.append((d, f"(self, other: {OPERAND}) -> {r}"))
        # `**` carries Python's optional third `pow()` argument, so it does not
        # fit the binary shape above; the extension rejects a non-`None` one.
        for d in ("__pow__", "__rpow__"):
            if hasattr(cls, d):
                out.append(
                    (d, f"(self, other: {OPERAND}, modulo: Any = ...) -> Indicator")
                )
        if hasattr(cls, "__abs__"):
            out.append(("__abs__", "(self) -> Indicator"))
    if cls_name == "Signal":
        for d in ("__and__", "__or__", "__xor__"):
            if hasattr(cls, d):
                out.append((d, "(self, other: Signal) -> Signal"))
        if hasattr(cls, "__invert__"):
            out.append(("__invert__", "(self) -> Signal"))
    if cls_name == "Snapshot":
        out.append(
            ("__getitem__", "(self, key: str | Selector | tuple[str, str]) -> Atom")
        )
        out.append(
            (
                "__setitem__",
                "(self, key: str | Selector | tuple[str, str], atom: Atom) -> None",
            )
        )
    if cls_name == "Sweep":
        out.append(("__getitem__", "(self, index: int | slice) -> Any"))
    if cls_name == "WalkForwardResult":
        out.append(("__getitem__", "(self, index: int | slice) -> Any"))
    if cls_name == "PanelWalkForwardResult":
        out.append(("__getitem__", "(self, index: int | slice) -> Any"))
    if cls_name == "SharedMultiIndicator":
        out.append(("__getitem__", "(self, name: str) -> Indicator"))
    for d, r in DUNDER_RETURNS.items():
        if d in ("__repr__", "__str__"):
            continue
        if cls.__dict__.get(d) is not None or (
            hasattr(cls, d) and getattr(cls, d) is not getattr(object, d, None)
        ):
            if d == "__contains__":
                out.append((d, "(self, key: Any) -> bool"))
            elif d == "__iter__":
                out.append((d, f"(self) -> {r}"))
            elif d in ("__len__", "__hash__", "__bool__"):
                out.append((d, f"(self) -> {r}"))
    return out


def emit_module(
    mod, names: list[str], returns: dict[str, str], owner: str
) -> tuple[list[str], list[str]]:
    lines: list[str] = []
    unknown: list[str] = []
    for name in names:
        obj = getattr(mod, name)
        ret = returns.get(name)
        if ret is None:
            unknown.append(name)
            ret = "Any"
        sig = signature_text(owner, name, obj, ret)
        if sig is None:
            lines.append(f"{name}: Any")
            continue
        if summary := docstring(obj):
            lines.append(f"def {name}{sig}:")
            lines.append(indent_doc(summary, "    "))
            lines.append("    ...")
        else:
            lines.append(f"def {name}{sig}: ...")
    return lines, unknown


def main() -> int:
    global LEAF_NAMES
    # A "leaf" takes an optional `AtomSource`; a wrapper takes an `Indicator`.
    LEAF_NAMES = {
        n
        for n in RETURNS
        if (sig := try_sig(getattr(ta, n, None))) is not None
        and "source" in sig.parameters
        and sig.parameters["source"].default is not inspect.Parameter.empty
    }

    classes = sorted(
        n
        for n in ta.__all__
        if isinstance(getattr(ta, n), type)
        and not issubclass(getattr(ta, n), BaseException)
    )
    functions = sorted(
        n
        for n in ta.__all__
        if callable(getattr(ta, n)) and not isinstance(getattr(ta, n), type)
    )

    out = [HEADER]
    unclassified: list[str] = []

    # `Wallet` is an ABCMeta instance, not a pyclass; state it as a Protocol so a
    # checker accepts any of the three concrete wallets where one is wanted.
    out.append(emit_wallet_protocol())

    for name in classes:
        if name == "Wallet":
            continue
        lines, unknown = emit_class(name, getattr(ta, name))
        out.append("\n".join(lines) + "\n")
        unclassified.extend(unknown)

    fn_lines, unknown = emit_module(ta, functions, RETURNS, "")
    unclassified.extend(unknown)
    out.append("\n".join(fn_lines) + "\n")

    if unclassified:
        print(
            "gen_python_stubs: these bindings have no entry in the type tables:\n  "
            + "\n  ".join(sorted(unclassified))
            + "\n\nAdd them to RETURNS / MEMBER_RETURNS / MEMBER_RULES in "
            "tools/gen_python_stubs.py.",
            file=sys.stderr,
        )
        return 1

    OUT.parent.mkdir(parents=True, exist_ok=True)
    OUT.write_text("\n".join(out))

    # Submodules: metrics is ~59 numeric reducers, montecarlo 2.
    # `Trade` and `DrawdownSegment` *live* on this submodule, so they are
    # declared here — importing them from `.` would be a redefinition and a lie
    # about where they come from. `Fill` genuinely lives on the parent.
    metric_members = [
        n
        for n in dir(ta_metrics)
        if not n.startswith("_") and callable(getattr(ta_metrics, n))
    ]
    metric_classes = sorted(
        n for n in metric_members if isinstance(getattr(ta_metrics, n), type)
    )
    metric_names = sorted(n for n in metric_members if n not in metric_classes)
    metric_returns = {n: metrics_return(n) for n in metric_names}

    class_lines: list[str] = []
    for name in metric_classes:
        emitted, unknown = emit_class(name, getattr(ta_metrics, name))
        class_lines.append("\n".join(emitted) + "\n")
        unclassified.extend(unknown)

    lines, _ = emit_module(ta_metrics, metric_names, metric_returns, "")
    METRICS_OUT.write_text(
        '"""Type stubs for `fugazi.metrics`. GENERATED — see tools/gen_python_stubs.py."""\n\n'
        "from collections.abc import Sequence\n"
        "from typing import Any\n\n"
        "from . import Fill as Fill\n\n"
        + "\n".join(class_lines)
        + "\n".join(lines)
        + "\n"
    )
    if unclassified:
        print(
            "gen_python_stubs: unclassified in fugazi.metrics: "
            + ", ".join(sorted(unclassified)),
            file=sys.stderr,
        )
        return 1

    mc_names = sorted(
        n
        for n in dir(ta_montecarlo)
        if not n.startswith("_") and callable(getattr(ta_montecarlo, n))
    )
    lines, _ = emit_module(ta_montecarlo, mc_names, {n: "Any" for n in mc_names}, "")
    MONTECARLO_OUT.write_text(
        '"""Type stubs for `fugazi.montecarlo`. GENERATED — see tools/gen_python_stubs.py."""\n\n'
        "from typing import Any\n\n" + "\n".join(lines) + "\n"
    )

    (OUT.parent / "py.typed").write_text("")
    print(f"wrote {OUT.relative_to(ROOT)} ({len(OUT.read_text().splitlines())} lines)")
    print(f"wrote {METRICS_OUT.relative_to(ROOT)}")
    print(f"wrote {MONTECARLO_OUT.relative_to(ROOT)}")
    return 0


def emit_wallet_protocol() -> str:
    """Emit `Wallet` as a `Protocol`, not as the plain class it is at run time.

    At run time `fugazi.Wallet` is an `abc.ABCMeta` with the three concrete
    wallets `register()`ed, which is what makes `isinstance(w, ta.Wallet)` work.
    A type checker does not follow `register()`, so a nominal class here would
    reject `w: ta.Wallet = ta.PaperWallet(...)` — the exact annotation the ABC
    exists to enable.

    A structural `Protocol` gives the checker the same answer the runtime gives,
    derived from the same source: the members come off `PaperWallet`, and the set
    is whatever they all share, so this cannot claim more than they implement.
    """
    shared = set.intersection(
        *(
            {m for m in dir(getattr(ta, c)) if not m.startswith("_")}
            for c in ("PaperWallet", "OkxWallet", "CoinbaseWallet", "KrakenWallet")
        )
    )
    lines = [
        "class Wallet(Protocol):",
        indent_doc(
            "Anything `Strategy.run` / `StrategySpec.run` will trade into: "
            "`PaperWallet`, `OkxWallet`, `CoinbaseWallet` or `KrakenWallet`. "
            "Mirrors the Rust `Wallet` trait. At run time this is an ABC with "
            "each registered on it, so `isinstance(w, fugazi.Wallet)` works too.",
            "    ",
        ),
    ]
    paper = ta.PaperWallet
    for member in sorted(shared):
        attr = inspect.getattr_static(paper, member, None)
        ret, _ = member_return("PaperWallet", member)
        if isinstance(attr, (property, *GETSET)):
            lines.append("    @property")
            lines.append(f"    def {member}(self) -> {ret}: ...")
            continue
        sig = signature_text("PaperWallet", member, getattr(paper, member), ret)
        if sig is None:
            lines.append(f"    {member}: {ret}")
        else:
            lines.append(f"    def {member}{sig}: ...")
    return "\n".join(lines) + "\n"


def metrics_return(name: str) -> str:
    if name == "reconstruct_trades":
        return "list[Trade]"
    if name == "drawdown_segments":
        return "list[DrawdownSegment]"
    if name == "per_bar_returns":
        return "list[float]"
    return "float"


def try_sig(obj):
    if obj is None:
        return None
    try:
        return inspect.signature(obj)
    except (TypeError, ValueError):
        return None


if __name__ == "__main__":
    sys.exit(main())
