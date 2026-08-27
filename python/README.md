# fugazi (Python)

[![CI](https://img.shields.io/github/actions/workflow/status/acpuchades/fugazi/ci.yml?branch=main&label=CI)](https://github.com/acpuchades/fugazi/actions/workflows/ci.yml)
[![PyPI](https://img.shields.io/pypi/v/fugazi.svg)](https://pypi.org/project/fugazi/)
[![Python versions](https://img.shields.io/pypi/pyversions/fugazi.svg)](https://pypi.org/project/fugazi/)
[![License: MIT](https://img.shields.io/pypi/l/fugazi.svg)](https://github.com/acpuchades/fugazi/blob/main/LICENSE)
[![Sponsor](https://img.shields.io/badge/sponsor-%E2%9D%A4-db61a2)](https://github.com/sponsors/acpuchades)

**One trading engine for research and production.** fugazi is a library of
**incremental** technical-analysis primitives, a strategy layer, a backtester and
a metrics suite — a Rust core, driven entirely from Python. Every indicator owns
its state and advances one sample at a time in ~O(1), so the object you research
with *is* the object you stream with. There is no vectorised research path and
separate live path to keep in sync.

```python
import fugazi as ta

def golden():
    return ta.ema(ta.close(), 12).crosses_above(ta.ema(ta.close(), 26))

entries = golden().feed(df)      # research: one boolean column over the whole frame

live = golden()
for candle in stream:            # production: the same object, one bar at a time
    if live.update(candle):
        ...
```

`feed` is not a second implementation of `update` — it *is* `update`, with the
loop moved to the Rust side. That is the whole pitch. The rest of this page is
the evidence, then the manual.

**Jump to:** [Why](#why-fugazi) · [Install](#install) · [Sixty seconds](#sixty-seconds) ·
[Indicators](#guide-indicators-and-signals) · [Trading](#guide-trading) ·
[Strategy documents](#guide-strategies-as-documents) · [Metrics](#metrics) ·
[Data](#fetching-data) · [Performance](#performance) · [Sponsor](#sponsor)

---

## Why fugazi

### The seam that usually breaks

A Python quant stack is usually two programs wearing one name. Research is
vectorised — a whole column at a time, the entire history in one C loop, indexed
by a `DatetimeIndex`. Production is event-driven — a bar arrives on a websocket
and you react to it. They are written differently, they drift, and the bugs that
result are the expensive kind: the backtest nobody can reproduce live.

fugazi removes the seam by making the *incremental* form the only form, then
making it fast enough that you don't miss the vectorised one.

| What you need | The usual answer | What that costs | fugazi |
| --- | --- | --- | --- |
| Fast indicators | `talib`, `pandas-ta` | Array-at-a-time. A live bar means recomputing the array, or writing a second implementation you now maintain twice | One `update()` per bar, and [faster than `talib`'s own bindings](#performance) on `ema` / `atr` / `macd` |
| Only the new bars | Recompute the tail with a lookback fudge factor | You guess the warm-up, and a recursive indicator never fully agrees with the one-pass answer | [`feed` never resets](#batch-api--a-whole-series-at-once): chunked calls continue the same stream, and concatenate exactly |
| A backtest | `vectorbt`, `backtesting.py` | A fill model expressed as array masks; the loop that trades live is a different program | [`Strategy(...).run(wallet, df)`](#the-declarative-strategy-builder) — the wallet is the only thing that changes |
| Live execution | A broker SDK plus glue | The strategy gets rewritten against the SDK's callbacks | [`OkxWallet` / `CoinbaseWallet` / `KrakenWallet`](#resuming-a-run-and-running-against-a-venue) go where `PaperWallet` went |
| Several symbols per bar | A DataFrame per symbol, then a join | Joining on the trading *date* manufactures cross-timezone lookahead | [`Snapshot`](#cross-asset-composition--snapshot-selector-and-pick) *is* the bar; `pick(sym)` projects one asset out |
| Non-price inputs | Bolt on a column, hope | No types, no warm-up accounting | [Overlays](#computing-overlays--deriving-columns-from-a-series): typed `get(schema, key)` readers over any joined series |
| A parameter sweep | A `for` loop over `itertools.product` | Single-threaded, and it overfits quietly | [`ta.optimize(..., jobs=N)`](#parameter-grid-optimize) with walk-forward and windowed ranking |

### The case, in eight points

**1. One object, batch and streaming.** `feed(df)` computes a whole frame;
`update(candle)` advances one bar. Same object, same state, same numbers — and
`feed` is **itself incremental**, so it never auto-resets. Feed it successive
chunks and the concatenated output equals a single pass over the whole series,
warm-up paid once. That is the property that lets a notebook and a live process
share an implementation instead of agreeing to differ.

**2. Incremental costs nothing.** The usual objection to per-bar dispatch is
speed. Measured against `talib` — TA-Lib's own bindings, the like-for-like
comparison since both cross a Python boundary — fugazi is **faster** on `macd`
(0.27×), `atr` (0.47×), `ema` (0.74×) and `dmi` (0.78×), and within noise on
`sma` (1.17×) and `rsi` (1.05×), while staying one bar at a time.
[Full table, and the two places it loses →](#performance)

**3. Composition is construction.** No pipe operator, no glue step, no DSL. An
indicator owns its source, so "EMA-20 of the SMA-10 of the close" is exactly
`ta.ema(ta.sma(ta.close(), 10), 20)` — one object, which you can feed bars, and
whose `warm_up_bars()` is computed correctly across the entire nested chain.

**4. It speaks your dataframe library.** pandas in → pandas `Series` out, index
preserved. polars in → polars out. A `list`/`dict`/NumPy array in → `ndarray`
out. Multi-line indicators return a `DataFrame`, signals a boolean `Series`.
Column names match case-insensitively, warm-up bars come back as `NaN`, and the
result assigns straight into `df[...]` because it lines up with your rows.

**5. Multi-symbol and non-price data are first-class.** The unit of input is a
`Snapshot` — every symbol's bar for one timestamp, each optionally carrying an
*overlay* bundle (funding rate, open interest, market cap, a regime label, your
own precomputed feature). Cross-asset expressions are ordinary indicators:
`ta.close(ta.pick("BTC")) - ta.close(ta.pick("ETH"))` is a spread you can hand to
anything that takes a source.

**6. The whole engine is bound, not a sampler of it.** Five strategy shapes
(single, pairs, basket, multi-asset, and a portfolio of N strategies netting onto
one account), YAML strategy documents, parameter sweeps with walk-forward
validation, Monte Carlo significance testing, cost models, bit-identical run
resuming, live venue wallets, and six data providers. **No CLI, no Rust
toolchain, no separate service** — `pip install fugazi` is the whole install, and
the wheel has no required dependencies.

**7. Unsettled numbers are refused by default.** Every indicator reports
`warm_up_bars()` *and* `unstable_bars()` — the extra samples until an IIR seed's
influence has decayed below 0.1%. An EMA-20 is defined after 1 bar and *settled*
after 71. A strategy will not trade until every wired signal is past both, so no
trade fires on a seed-contaminated value. There is exactly one opt-out,
`.unstable()`.

**8. Checked against the libraries you would otherwise be using.** Indicators are
cross-validated against **TA-Lib**, equity-curve metrics against **empyrical**,
wallet execution against **vectorbt**, and trade statistics against
**backtesting.py**. Fixtures are committed and CI runs with
`FUGAZI_REQUIRE_FIXTURES=1`, so a stale fixture fails the build instead of
silently comparing nothing. Where fugazi deliberately *disagrees* with a
reference — five of backtesting.py's headline stats answer a different question
from the field sharing their name — the divergence itself is asserted. Every
Python block on this page is executed by the test suite, so the docs cannot drift
from the wheel either.

### When fugazi is the wrong tool

Worth saying plainly, so you don't find out in week three:

- **You want plots.** It returns arrays and frames. Charting is yours —
  matplotlib over the returned `Series` works fine, but nothing here draws.
- **You need tick data or L2 microstructure.** The unit of time is a bar.
- **You want a research *framework*.** No feature store, no sklearn pipeline
  integration, no notebook widgets, no hyperparameter tracking. It is an engine
  you call, not a platform you live in.
- **You want protective stops from the `Strategy` builder.** Position-anchored
  stops aren't bound yet — [drop to the wallet loop](#guide-trading) or write the
  strategy as a [document](#guide-strategies-as-documents).
- **Your hot path is `stddev` on huge windows.** fugazi's is ~3.4× `talib`'s, on
  purpose — [the shortcut it refuses](#the-one-real-loss) returns exactly `0.0`
  for 896 of 4 981 windows on the benchmark series.

---

## Install

```sh
pip install fugazi
```

Then `import fugazi`. Prebuilt wheels are published for Linux, macOS
(Intel + Apple Silicon) and Windows; the wheel is `abi3-py311`, so one binary
serves Python 3.11 and up, and it needs **no Rust toolchain and no required
dependencies**. pandas, polars and NumPy are used when present — `feed` mirrors
whichever you hand it and falls back to plain Python lists when none is
installed.

To build from a checkout instead (for development):

```sh
pip install maturin
maturin develop --release   # editable install into the active virtualenv
```

---

## Sixty seconds

Three steps, each one further than the last. Nothing here needs the CLI — the
data providers are part of the library.

**A signal**, over real candles, computed both ways.

```python
import fugazi as ta

df = ta.Binance().fetch(symbol="BTCUSDT", freq="1d",
                        since="2023-01-01", output="pandas")

# "close crosses above its EMA-20, while RSI-14 is still under 70" — one object.
entry = (
    ta.close()
      .crosses_above(ta.ema(ta.close(), 20))
      .and_(ta.rsi(ta.close(), 14).below(70.0))
)

df["entry"] = entry.feed(df)     # a boolean column, aligned to df.index
print(df["entry"].sum(), "entry bars")
```

**A backtest.** Wire the signals onto a strategy, hand it a wallet, read the
metrics.

```python
import fugazi as ta
from fugazi.metrics import per_bar_returns, sharpe

df = ta.Yahoo().fetch(symbol="AAPL", freq="1d",
                      since="2020-01-01", output="pandas")

strat = ta.Strategy("AAPL").long_on(
    ta.sma(ta.close(), 10).crosses_above(ta.sma(ta.close(), 30)),   # enter
    ta.sma(ta.close(), 10).crosses_below(ta.sma(ta.close(), 30)),   # exit
)

wallet = ta.PaperWallet(10_000.0)
report = strat.run(wallet, df)

returns = per_bar_returns(report.equity_curve, report.initial_equity)
print(len(report.fills), "fills, sharpe", sharpe(returns, 0.0, 252.0))
```

**Live.** The same call against a real venue. One object changed.

```py
wallet = ta.OkxWallet.demo(key, secret, passphrase)   # or .mainnet(..) — real funds
report = strat.run(wallet, live_bars)
```

That's honest for a one-shot drive — replay a known history against a real
venue's prices and cost model, say. It is **not** how you keep a strategy
running: `.run()` rebuilds the strategy from scratch each call, so calling it
again as new bars arrive would silently re-warm every indicator instead of
continuing. `.feed()` doesn't reach this layer either — it's an `Indicator`/
`Signal` method, not a wallet or strategy one. The thing that actually carries
state across calls is `run_resumable`, and it wants the strategy as a
document — which is also a nudge toward the next form:

```py
state = None
while True:
    new_bars = poll_new_bars()                              # your own feed
    report, state = spec.run_resumable(wallet, new_bars, resume=state)
```

[More on resuming and going live →](#resuming-a-run-and-running-against-a-venue)

Rather keep the strategy as data than as code? The same thing as a document:

```python
import fugazi as ta

spec = ta.load_spec("""
root: AAPL
long:
  enter: !crosses_above { lhs: !sma { period: 2 }, rhs: !sma { period: 5 } }
  exit:  !crosses_below { lhs: !sma { period: 2 }, rhs: !sma { period: 5 } }
""")
wave = [10, 9, 8, 7, 6, 7, 9, 12, 15, 18, 21, 22, 21, 20, 18, 15, 12, 10, 8, 6]
snaps = [ta.Snapshot({"AAPL": ta.Candle(v, v, v, v, 1.0)}) for v in wave]
metrics = spec.evaluate(ta.PaperWallet(1000.0), snaps)
print(metrics["risk_adjusted"]["sharpe"])
```

---

## Guide: indicators and signals

You build indicators by **nesting constructors**. Every indicator is rooted at
a leaf source — usually a candle field (`close()`, `high()`, `volume()`, ...):

```python
import fugazi as ta

ema = ta.ema(ta.close(), 20)                  # EMA-20 of the close
node = ta.ema(ta.sma(ta.close(), 10), 20)     # EMA-20 of an SMA-10 — just keep nesting
```

The root decides what the indicator *consumes*. A candle-rooted indicator takes
`Candle`s (any of OHLCV); to work on a **bare stream of numbers** instead, root
it at `identity()` — the leaf that passes raw values straight through:

```python
prices = ta.rsi(ta.identity(), 14)            # RSI of a plain float series
```

Then drive it one of two ways: **streaming** (a bar at a time) or **batch** (a
whole series at once). They share the same indicators; pick by how your data
arrives.

What you feed `update()`/`feed()` follows from the root: a candle-rooted
indicator consumes **candles**, an `identity()`-rooted one consumes **plain
numbers**.

### Streaming API — one sample at a time

Feed one sample to `update()`; it returns a `float`, or `None` until warmed up.
This is the live/incremental path. Every node also has `value()` (or `is_true()` for a boolean Signal),
`is_ready()`, and `reset()`.

```python
node = ta.ema(ta.sma(ta.close(), 10), 20)        # candle-rooted

for o, h, l, c, v in bars:
    value = node.update(ta.Candle(o, h, l, c, v))   # feed a Candle -> float | None
    print(value)

prices = ta.rsi(ta.identity(), 14)               # identity-rooted
for px in [100.0, 101.5, 100.8]:
    prices.update(px)                            # feed a float
```

### Batch API — a whole series at once

`feed(data)` computes every bar in one call. For a **candle-rooted** indicator,
`data` is a dataframe with OHLCV columns — **pandas and polars both work** (also
a `dict` of columns) — and only the columns an indicator needs have to be
present:

```python
import pandas as pd      # or: import polars as pl

# df is your OHLCV frame (open/high/low/close/volume columns)
df["ema20"] = ta.ema(ta.close(), 20).feed(df)   # assigns straight back
ta.atr(14).feed(df)                             # uses high/low/close
ta.vwap(20).feed(df)                            # uses high/low/close/volume
```

Column names are matched case-insensitively (`Close`/`CLOSE`/`close`), and
`close` is required. An **`identity()`-rooted** indicator instead takes a plain
1-D series — a `list`, NumPy array, or pandas/polars `Series`:

```python
ta.ema(ta.identity(), 20).feed([100.0, 101.5, 100.8, 102.3, 101.9])
ta.ema(ta.identity(), 20).feed(df["close"])
```

(The root is the contract: a candle indicator won't silently treat a bare array
as the close, and a value indicator won't accept a frame — pick the root that
matches your data.)

The output **mirrors the input library**, one value per bar, with warm-up bars
as `NaN` (so the result lines up with your rows and assigns straight back):

| Input | Indicator | Multi-line (macd, bollinger, …) | Signal |
| --- | --- | --- | --- |
| pandas | `Series` (index preserved) | `DataFrame` (one column per line) | bool `Series` |
| polars | `Series` | `DataFrame` | bool `Series` |
| list / dict / NumPy | `ndarray` | `dict` of `ndarray`s | bool `ndarray` |

```python
ta.ema(ta.close(), 20).feed(df)            # pandas Series, df.index
ta.macd(ta.close()).feed(df)               # pandas DataFrame: macd/signal/histogram
ta.macd(ta.identity()).feed(prices_list)   # {"macd": ndarray, "signal": ndarray, ...}
```

(If NumPy isn't installed, list/dict input falls back to plain Python lists.)

`feed` is **itself incremental** — it just loops `update` over the batch through
the node's own state and never auto-resets. So calling it on successive chunks
continues the same stream: the warm-up is paid once, and the concatenated
outputs equal a single feed over the whole series. This is what lets you process
data as it arrives without recomputing history:

```python
node = ta.sma(ta.identity(), 3)
x1 = node.feed(series1)         # warms up, emits for series1
x2 = node.feed(series2)         # continues from where series1 left off
# np.concatenate([x1, x2]) == ta.sma(ta.identity(), 3).feed(series1 + series2)

node.reset()                   # call reset() to start a fresh, independent pass
```

> A source can be reused after you pass it into a constructor:
>
> ```python
> src = ta.close()
> fast = ta.ema(src, 10)
> slow = ta.ema(src, 20)   # `src` is still usable here
> ```

### The catalogue

| Constructor | Output |
| --- | --- |
| `open() high() low() close() volume() typical() median()` | the candle field |
| `identity()` | the raw value stream (root for a bare numeric series) |
| `value(x)` | a constant |
| `sma ema rma wma hma stddev (source, period)` | a value |
| `rsi(source, period=14) stochastic(source, period=14) cci(source, period=20)` | a value — the conventional period is the default |
| `skewness kurtosis zscore (source, period)` | a value (distribution shape / normalization; `kurtosis` is raw, ~3 for normal) |
| `correlation(lhs, rhs, period)` | rolling Pearson correlation in `[-1, 1]` (autocorrelation: `correlation(x, x.lag(n), period)`) |
| `covariance(lhs, rhs, period)` `beta(lhs, rhs, period)` | rolling population covariance; rolling least-squares slope of `lhs` on `rhs` |
| `percentile(source, period, pct=0.5)` | the `pct`-quantile over the window (`pct=0.5` is the rolling median), linearly interpolated like numpy's default |
| `percentile_rank(source, period)` | where the current reading sits in its own window: `count(v <= x)/period`, in `(0, 1]` |
| `get(schema, key, source=None)` | the overlay column, typed by its declaration (real→Indicator, bool→Signal, str→StrSource); `source=pick(sym)` reads another series' column |
| `get_real get_bool get_str (schema, key, source=None)` | the same read with the type **asserted** rather than inferred — each raises if the column is declared as something else, so a caller that needs an `Indicator` gets one or an error, never a `Signal` |
| `value_str(s)` | a constant string source — the `StrSource` twin of `value(x)`, for the right-hand side of `str_eq` / `str_ne` |
| `bars_since(signal)` | bars since `signal` was last true (`0` on the firing bar); `None` until it has fired once, so thresholds read false until then |
| `bars_since_high bars_since_low (source, period)` | bars since the source set a new `period`-bar extreme, in `[0, period-1]` |
| `variance_ratio(source, period, lag=2)` | Lo-MacKinlay regime classifier (`>1` trending, `<1` mean-reverting); O(period)/bar recompute |
| `stoch_rsi(source, rsi_period=14, stoch_period=14)` | a value |
| `if_else(cond, then, otherwise)` | three-source ternary; every branch advances every bar, so a branch's warm-up progresses on bars it isn't selected |
| `atr(period=14) mfi(period=14) williams_r(period=14)` | a value |
| `vwap(period)` | a value (a rolling VWAP has no conventional window) |
| `parkinson garman_klass rogers_satchell (period)` | range-based volatility estimate (uses the full candle; more efficient than close-to-close stddev) |
| `obv() ad() true_range()` | a value |
| `sar(step=0.02, max=0.2)` | a value |
| `macd(source, fast_period=12, slow_period=26, signal_period=9)` | dict `{macd, signal, histogram}` |
| `bollinger(source, period=20, k=2.0)` | dict `{upper, middle, lower}` |
| `keltner(source, ema_period=20, atr_period=10, multiplier=2.0)` | dict `{upper, middle, lower}` |
| `donchian(high, low, period=20)` | dict `{upper, middle, lower}` |
| `adx(period=14)` | dict `{plus_di, minus_di, adx}` |
| `dmi(period=14)` | dict `{plus_di, minus_di}` |
| `aroon(period=14)` | dict `{up, down, oscillator}` |
| `linreg(source, period=14)` | dict `{slope, intercept, value, r2}` — the rolling least-squares fit against the bar index (`period >= 2`) |
| `resample(every, inner)` | `inner`'s output every `every` bars (aggregated HTF candle fed to `inner`), `None` between |
| `volume_bars(threshold, inner)` `dollar_bars(threshold, inner)` | the same shape sampled on **activity** rather than elapsed bars — one bar per `threshold` units of traded quantity, or of traded notional (`typical × volume`). Emits on the completing tick, `None` between; wrap in `latch` for per-base-tick reads |
| `latch(source)` | `source`'s last `Some` output, held across `None` ticks (works on indicators and signals) |
| `unstable(x)` | Passthrough that reports `unstable_bars() = 0` for its subtree (also `.unstable()` on any Indicator or Signal) |
| `every(period)` | Signal: a pulse every `period` bars, first fire delayed to bar `period-1` — the usual [`rebalance_on`](#the-declarative-strategy-builder) gate |
| `year() month() day() hour() minute() second()` | calendar decomposition of the bar's timestamp (UTC) |
| `day_of_week() day_of_year() week_of_year() quarter()` | ISO day-of-week (1=Mon), day/week of year, calendar quarter |
| `unix_seconds() unix_millis()` | the raw stamp, as a float |
| `is_weekday() is_weekend()` | Signals over the same stamp |
| `str_eq(lhs, rhs)` `str_ne(lhs, rhs)` | Signals over two string sources — `str_eq(get_str(schema, "regime"), value_str("bull"))` |

Every calendar leaf, like every price leaf, takes an optional `source=` to
re-root it onto another series' atom stream.

Multi-line indicators return a `dict` of their named lines (or `None` while
warming up).

### Projecting one line of a multi-output indicator: `shared()`

Call `.shared()` on any multi-output indicator (`macd`, `bollinger`, `adx`,
`donchian`, `keltner`, `dmi`, `aroon`) to get a handle whose per-line accessors
return ordinary `Indicator`s that compose with the usual operators (`gt`,
`crosses_above`, `add`, …). Every accessor built off one `.shared()` handle
projects into the **same** underlying source — the multi advances at most once
per bar however many accessors read out of it, exactly like Rust's
`Macd::new(...).shared()`:

```python
# MACD line crossing its signal line, as a single composed Signal:
macd = ta.macd(ta.close(), 12, 26, 9).shared()
bullish = macd.line().crosses_above(macd.signal())

# Close pierces the Bollinger upper band:
bands = ta.bollinger(ta.close(), 20, 2.0).shared()
breakout = ta.close().gt(bands.upper())
```

The accessor names mirror the Rust API: `line()`/`signal()`/`histogram()` on a
MACD, `upper()`/`middle()`/`lower()` on Bollinger/Keltner/Donchian,
`plus_di()`/`minus_di()`/`adx()` on ADX/DMI, `up()`/`down()`/`oscillator()` on
Aroon. `component(name)` is a programmatic fallback, `names()` lists what's
available for a given handle. Calling `.shared()` returns a fresh handle owning
its own copy of the source, so the original `MultiIndicator` (with its dict-
returning `.update()` / `.feed()` API) stays usable in parallel.

### Cross-timeframe composition

`resample` + `latch` compose a higher-timeframe pipeline over a base candle
stream: `resample(N, inner)` aggregates every N base candles into one HTF
candle and runs `inner` (any candle-rooted Real source — `close()`,
`ema(close(), 20)`, …) over it, emitting `inner`'s output on the completing
tick and `None` in between. **The resample's clock stays base-timeframe**:
it's fed one base candle per `update()` and reports at that same cadence —
the emitted output marks whether the inner produced a value on a completed
bucket. Wrap the whole resample in `latch()` so per-base-tick reads see the
finished value between boundaries.

```python
# EMA-20 of the closes of every 4-bar candle, latched for per-base-tick reads.
htf_ema = ta.latch(ta.resample(4, ta.ema(ta.close(), 20)))
```

The **only correct ordering** is `resample(N, ema(...))` — with the recursive
smoother as the resample's `inner` — then `latch` on the outside; latching
*before* the recursive smoother would feed it a held (repeated) value on every
base tick, distorting the recurrence.

`unstable(x)` wraps an indicator or signal as a passthrough that reports
`unstable_bars() = 0`, telling a downstream reader of `stable_bars()`
(a strategy-readiness gate, an overlay trim) "trade through this subtree's
IIR settling tail". Available as a free function and as a method on any
Indicator or Signal — same output, same warm-up, only the reported unstable
tail changes:

```python
raw = ta.ema(ta.close(), 20)
fast = raw.unstable()           # method form; unstable_bars() -> 0
fast = ta.unstable(raw)         # equivalent free-function form
```

Safe by default, override per subtree: fugazi's readiness machinery waits for
`stable_bars()` by default (`SingleAssetStrategy::is_ready` in Rust; the
CLI's per-overlay CSV trim in `fugazi get`) — `unstable(...)` is the single
opt-out.

### Cross-asset composition — `Snapshot`, `Selector`, and `pick(...)`

To reason about more than one asset per bar, feed a **Snapshot** — a keyed
collection of `Atom`s (one per asset for the current bar) — and use `pick(...)`
to project one asset out of it. Every atom-input leaf (`close()`, `high()`,
`atr()`, `year()`, `is_weekday()`, ...) takes an optional `source=` argument
that re-roots it onto a `pick(...)`, so cross-asset expressions compose from
the same primitives as single-asset ones:

```python
import fugazi as ta

# BTC's close as a first-class indicator over Snapshot input.
btc_close = ta.close(source=ta.pick("BTC"))

# BTC/ETH close spread — arithmetic between two picks is just an indicator.
spread = ta.close(ta.pick("BTC")) - ta.close(ta.pick("ETH"))

# Feed one snapshot per bar.
snap = ta.Snapshot({
    "BTC": ta.Atom(ta.Candle(100, 101, 99, 100, 1), time=1_710_504_000_000),
    "ETH": ta.Atom(ta.Candle(60, 61, 59, 60, 1),   time=1_710_504_000_000),
})
print(spread.update(snap))          # -> 40.0
```

Snapshot keys are **Selectors** — a `(symbol?, stream?)` pair. A `Selector`
matches structurally: a `None` field on the query wildcards the corresponding
storage field, so `pick(symbol="BTC")` finds every BTC entry regardless of
stream. A bare Python `str` is coerced to `Selector.by_symbol(...)`, a
`(str, Frequency|str)` tuple to a full `(symbol, stream)` pair, so most call
sites don't need to reach for `Selector` explicitly. Cross-frequency indexes
disambiguate by giving both fields:

```python
snap = ta.Snapshot({
    ("BTC", "1h"): ta.Atom(ta.Candle(100, 101, 99, 100, 1), time=1_710_504_000_000),
    ("BTC", "1d"): ta.Atom(ta.Candle(90, 105, 88, 102, 1),  time=1_710_504_000_000),
    ("ETH", "1h"): ta.Atom(ta.Candle(60, 61, 59, 60, 1),    time=1_710_504_000_000),
})
btc_hourly = ta.close(ta.pick(symbol="BTC", freq="1h"))
any_hourly = ta.close(ta.pick(freq="1h"))              # wildcard on symbol
assert btc_hourly.update(snap) == 100.0
```

**`freq=` is checked; `stream=` is not.** A symbol's second series is not
always a cadence — dollar bars, a session id, a venue tag — so the selector's
stream half has two spellings, and the difference is a format contract.
`freq=` promises a bar cadence and is validated against the same `N<unit>`
alphabet `--frequency` uses, so `freq="1hh"` raises. The keyword-only
`stream=` promises nothing and is taken verbatim:

```python
dollar_bars = ta.close(ta.pick(symbol="BTC", stream="dollar-1e6"))
```

Both resolve to the same stream id; naming both on one `pick` is an error,
since there is no reading of two different streams on one leaf that is right.
`Selector` exposes the resolved half as `.stream`.

**Snapshot behaves like a dict of atoms**: `snap[selector]`, `snap[selector] =
atom`, `selector in snap`, `len(snap)`, `snap.keys()`. Constructors accept a
plain Python mapping, and `update()` accepts either a `Snapshot` or a bare
dict (lifted on the fly), so the surface fits both "build the frame once" and
"hand a fresh dict per bar" styles.

A `pick(...)` is *atom-emitting*, not real-emitting: it feeds any atom-input
leaf via `source=`. Compositions preserve the input domain — the arithmetic
below still consumes snapshots — and mixing a snapshot-rooted indicator with
a candle-rooted one is a `TypeError` (a candle-input and a snapshot-input
can't share a bar).

```python
# Any atom-input leaf takes source=: the price accessors and every calendar
# reader, wired to the same picked atom stream.
btc_close = ta.close(source=ta.pick("BTC"))
btc_year  = ta.year(source=ta.pick("BTC"))
ratio     = ta.close(ta.pick("BTC")) / ta.close(ta.pick("ETH"))
```

**The zero-arg `pick()` is the single-series shortcut.** With no query it
takes the sole-atom unpack on every bar: the snapshot must contain exactly one
priceable entry (its atom is what the pick emits), otherwise the call **fails
loudly** rather than silently picking whichever entry came first. That's the
"strategy authored for one asset but fed a `Snapshot`-shaped driver" case.

Calling `Snapshot.sole_atom` yourself raises a plain **`ValueError`** on an
ambiguous snapshot. It used to surface the Rust panic as a `PanicException`,
which derives from `BaseException` — so `except Exception` walked straight
past it, and catching it at all also meant swallowing your own
`KeyboardInterrupt`. Inside a *strategy*, though, this is not the error you
will meet: a document's declared symbol is a blessed root, so a bar it does
not quote on reads `None` and the strategy simply does not advance, and a
declared symbol missing from the whole stream is refused by `run()` before the
first bar, naming the symbol and what the stream does carry.

```python
# Single-series strategy, snapshot-shaped input:
close = ta.close(source=ta.pick())
snap  = ta.Snapshot({"BTC": ta.Atom(ta.Candle(1, 1, 1, 42, 1))})
assert close.update(snap) == 42.0
```

**Atom equality is by `time`.** Two atoms compare equal iff their bar-open
`Timestamp`s match — the OHLCV numbers and overlays are payload, not identity —
and atoms sort chronologically (`None` first), so mixed streams can be
deduplicated by time and sorted into run order without a custom key:

```python
a1 = ta.Atom(ta.Candle(1, 1, 1, 1, 0), time=1_000)
a2 = ta.Atom(ta.Candle(1, 1, 1, 99, 0), time=1_000)   # different price
a3 = ta.Atom(ta.Candle(1, 1, 1, 1, 0), time=2_000)
assert a1 == a2 and a1 < a3
assert len({a1, a2, a3}) == 2                          # a1 == a2, distinct from a3
```

### Computing overlays — deriving columns from a series

A **dataset** is a series (bars) plus a set of **overlays** — derived columns
computed from that series and carried on each bar's `OverlayInfo` side-channel.
`compute_overlays(series, overlays)` runs the overlay indicators over the series
and attaches the results, returning `(schema, augmented)`. Read the columns back
with `get(...)` — **use the returned schema**, the augmented atoms are bound to
it:

```python
import fugazi as ta

atoms = [ta.Atom(ta.Candle(c, c, c, c, 1_000)) for c in (10, 20, 30, 40)]

# `overlays` is a YAML doc of `name: !expr { ... }` ...
schema, out = ta.compute_overlays(atoms, "sma3: !sma { period: 3 }")
assert out[1].overlays.get_real(schema.index_of("sma3")) is None   # warming up
assert out[2].overlays.get_real(schema.index_of("sma3")) == 20.0   # mean(10,20,30)

# ... or a dict of pre-built indicators (Real / Signal → Bool / StrSource → Str).
schema, out = ta.compute_overlays(atoms, {"c": ta.close(), "hot": ta.close().above(15)})

reader = ta.get(schema, "c")            # resolve against the *returned* schema
assert [reader.update(a) for a in out][0] == 10.0
```

Existing overlay columns are preserved (same indexes) and the new columns
appended, so overlays layer over a fetched series. A computed column reads
`None` while it warms up. `Snapshot` sequences work too — each symbol's overlay
derives from its own series, warming independently:

```python
snaps = [
    ta.Snapshot({"BTC": ta.Atom(ta.Candle(b, b, b, b, 1)),
                 "ETH": ta.Atom(ta.Candle(e, e, e, e, 1))})
    for b, e in zip((10, 20, 30), (1, 2, 3))
]
schema, out = ta.compute_overlays(snaps, "sma3: !sma { period: 3 }")
assert out[2]["BTC"].overlays.get_real(schema.index_of("sma3")) == 20.0
```

### Operators

Combine value indicators into **other indicators**:

```python
ta.close().add(other)        # also: sub, mul, div  — or the + - * / operators
ta.close().lag(1)            # also: diff, ratio, roc
ta.close().rolling_max(20)   # also: rolling_min
```

...or into **signals** (booleans):

```python
fast > slow                          # also: < >= <=  — or the gt/lt/ge/le methods
fast.gt(slow, epsilon=0.5)           # absolute deadband; omit for the scale-aware default
ta.rsi(ta.close(), 14).above(70.0)   # also: below(level)
fast.crosses_above(slow)             # also: crosses_below
fast.eq(slow)                        # also: ne  — see the note on `==` below
```

A number lifts to a constant on either side, so `ta.close() > 100.0` and
`100.0 < ta.close()` both work.

> **`==` is the one operator that is not elementwise.** `a == b` is Python's
> ordinary identity comparison, so two separately-built chains over the same
> source compare `False`. Overloading it would return a `Signal` — always truthy,
> and unhashable — which would silently break `in`, `dict` and `set` for every
> indicator. Use `a.eq(b)` / `a.ne(b)` instead; they take `epsilon=` too, which
> is why the named `gt`/`lt`/`ge`/`le` twins exist alongside the operators.

Signals compose with each other and update to a `bool`:

```python
sig = a.and_(b)     # also: or_, xor_, not_(), changed()  — or  a & b | ~c
sig.update(candle)  # -> bool
```

## Guide: trading

The strategy layer is exposed two ways. For the classic single-asset shape
there's a declarative **`Strategy`** builder you `run` over a wallet (below);
for anything else, the **wallet** is a market-agnostic venue you trade into with
your own per-bar Python — no class to subclass. `PaperWallet` is the built-in,
in-memory book (funds + positions + a trade blotter); live execution belongs in
your own code, not here.

The concrete wallets — `PaperWallet`, `OkxWallet`, `CoinbaseWallet`,
`KrakenWallet` — are registered on **`ta.Wallet`**, the mirror of Rust's `Wallet`
trait, so `isinstance(w, ta.Wallet)` is how you ask and `w: ta.Wallet` is how you
annotate. It is a classification, not a base class to extend: a Python subclass of
it is not one of them, and `run` will refuse it.

```python
import fugazi as ta

wallet = ta.PaperWallet(10_000.0)          # seed with cash

wallet.update("AAPL", 185.0)               # feed the price each tick (before trading)

# set: absolute target (opposite side reverses) · set_position: absolute units · close: flat
wallet.set("AAPL", "buy", 10)                       # target 10 units (a number = units)
wallet.set("AAPL", "buy", ta.Size.value_frac(0.25)) # target 25% of equity
wallet.set("AAPL", "buy", ta.Size.position_frac(0.5))  # trim to 50% of the position
wallet.set_position("AAPL", 4)                      # drive straight to 4 units
wallet.close("AAPL")                                # flatten

wallet.funds                 # cash balance
wallet.equity                # funds + positions marked at the fed prices
wallet.position("AAPL")      # signed position (negative = short)
wallet.price("AAPL")         # last fed price (or None)
wallet.positions()           # {symbol: units}
wallet.orders()              # the blotter: list of Order(symbol, side, units)
wallet.can_short             # can this account hold a negative position?
wallet.quote_ccy             # what currency are these numbers in? (or None)
wallet.data_sources          # which providers quote this account? (list[str])
wallet.leverage("AAPL")      # how much may it hold, as a multiple of equity? (or None)
```

`can_short` is what an account *can* do, asked before trading: `True` on a
`PaperWallet` (a sell credits cash) and on `OkxWallet` (net-mode swaps), `False`
on the spot `CoinbaseWallet` and `KrakenWallet`, whose positions are owned
base-asset balances. (Kraken *does* offer shorting on margin, but that is opt-in
per order and `KrakenWallet` never asks for it, so it reports what it actually
does.) It
informs rather than enforces — a spot wallet still clamps a short target to flat
on its own — so a long/short strategy can pick a long-only path up front instead
of learning the limit from a clamped order.

`quote_ccy` is the same shape of question about the account's *unit*: `"USDT"` on
`OkxWallet` (the margin currency a linear USDⓈ-M swap settles in), whatever the
`CoinbaseWallet` or `KrakenWallet` was built against (`"USD"` by default), and
`None` on a `PaperWallet` unless you pass one — simulated money has no venue to
ask:

```python
wallet = ta.PaperWallet(10_000.0, quote_ccy="EUR")
wallet.quote_ccy             # "EUR"
```

**`None` means "unlabelled", never "no currency".** Every amount in this API is a
bare number in *some* unit, and fugazi does no FX anywhere: a run is sound only if
every price fed to it shares one numeraire. `quote_ccy` reports what that
numeraire is — to label a balance, refuse a mixed-currency universe, or reconcile
against a venue — and answering does not make mixing safe. One caveat on
`OkxWallet`: `funds` is in `quote_ccy`, but `equity` is OKX's own USD valuation of
the account, so the two differ by the USDT peg.

`data_sources` is the third question of the same shape, asked about the *feed*:
`["okx"]` on `OkxWallet`, `["coinbase"]` on `CoinbaseWallet`, `["kraken"]` on
`KrakenWallet`, `[]` on a `PaperWallet`. The names are the ones a `fugazi get` spec takes, so a live runner
can check the pairing before it drives an account off the wrong bars:

```py
wallet = ta.OkxWallet.demo(key, secret, passphrase)
assert wallet.data_sources == ["okx"]
bars = ta.fetch("okx", "BTC-USDT-SWAP", "1h", since="2024-01-01")
```

It introspects, it does not fetch — and it answers at venue granularity, which is
all a provider name has room to say: the OKX account above trades swaps, so the
matching bars are that provider's **swap** instrument id, not the `BTC-USDT` spot
pair it will also serve. Empty means "does not say", never "nothing quotes this".

### Leverage — and what `sizing` above `1.0` means

`leverage(symbol)` is the fourth question of the same shape, and the only one
that is per-symbol, because venues configure it that way. It is **reporting, not
control**: nothing here sets a venue's leverage.

A `PaperWallet` carries two leverage numbers, and they answer two questions:

```python
wallet = ta.PaperWallet(10_000.0, leverage=3.0)
wallet.deployment                  # 3.0 — what a fractional Size is multiplied by
wallet.leverage("BTC-USDT-SWAP")   # 3.0 — the ceiling, defaulted to the deployment

# Pinned apart when you mean them apart: deploy 3x, tolerate 5x on marks.
ta.PaperWallet(10_000.0, leverage=3.0, max_gross=5.0).leverage("BTC")  # 5.0
```

`leverage=` is the one that makes an **unedited** strategy trade levered:
`value_frac(1.0)` at `leverage=3.0` targets 3x equity. `max_gross` cannot do
that — a ceiling only ever *stops* a request, so on its own it leaves any
document whose sizing never exceeds `1.0` completely unchanged. It scales both
fractional sizings and neither absolute one (`Size.units`, `Size.position_frac`):
a named unit count is a specific intent. It scales a risk-denominated rule too,
so a vol-target strategy at `leverage=3.0` holds three times its target vol —
leave it at `1.0` and raise `max_gross` if you want the target untouched and
only its clipping lifted.

**That cap is the one bound both sides of the book share.** A buy is limited by
the cash it spends; a short *credits* cash, so cash alone never bounds one. Until
this existed, `sizing: 3.0` meant 1x on a long leg (quietly scaled back to what
cash could pay for) and 3x on a short leg, under one spec value — so a long/short
backtest reported a number describing neither. What the wallet bounds now is
gross notional: no fill may leave `sum(|position| * price)` above
`max_gross * equity`. For a long-only book at `1.0` that is not a new rule,
it is the old one restated, so an unlevered long backtest is unchanged.

A request above the cap is *fitted* to it rather than refused, and the fill says
so:

```py
order = report.fills[0].order
order.units             # 100.0 — what traded
order.requested_units   # 300.0 — what `sizing: 3.0` asked for
order.fill_ratio        # 0.333…
```

`requested_units` exists because "was this shrunk?" is a useless question: under
any positive commission an all-in sheds a sliver on every trade, so a flag would
fire constantly. `is_materially_fitted` applies the one threshold the whole
library uses (`ta.MATERIALLY_FITTED`, 99%), and `RunReport.materially_fitted`
asks it of a whole run:

```py
report.materially_fitted    # None, or (count, worst_ratio) — e.g. (38, 0.335)
```

That third value belongs beside `rejections` and `carry_coverage`: a *fitted*
fill is not a refusal, so it never reached `rejections`, and until this existed a
`sizing:` the account could not carry looked exactly like a signal that sized
smaller. An explicit unit count (`Size.units`, `set_position`) is never fitted —
it raises `WalletError` instead, on both sides.

**The cap bounds the result; it does not re-denominate the request.**
`value_frac(1.0)` is 1x equity on a 1x wallet and 1x equity on a 10x one —
raising `max_gross` never enlarges what a document asks for, it only stops
truncating it. So a strategy reaching for leverage says so in its own sizing.
The corollary is that sizing rules which exceed `1.0` — `vol_target` in a calm
market, `atr_risk` on a narrow range — are *already* clipped by the default cap:
on a 1,200-bar sample, a 20% vol target had 38 of 139 fills fitted (worst: 33.5%
of the request) and realized 12.4% vol at `max_gross=1`, against 15.8% and zero
fitted fills at `max_gross=3`.

On the live side, `OkxWallet` answers from the venue. It is filled for free for
any symbol the account holds a position in, and read on demand for anything else:

```py
wallet = ta.OkxWallet.demo(key, secret, passphrase)
wallet.refresh_leverage("BTC-USDT-SWAP")   # ask OKX now, and cache it
wallet.leverage("BTC-USDT-SWAP")           # 3.0
```

`CoinbaseWallet` and `KrakenWallet` answer `None` structurally — spot has
nothing borrowed to parameterise, the same fact they report as
`can_short == False`. `None` means "does not say", never `1x`.

Record the live number at connect and check it on reconcile: OKX's leverage is
set out of band, in its own UI, and can change under a running strategy. Set the
`PaperWallet`'s `max_gross` to match, and the two curves measure the same
strategy. Without that, a live equity curve is uninterpretable against the
backtest it is supposed to be tracking.

### What a levered account also pays

`max_gross` bounds the size of the book. Two more knobs make holding one cost
what it really costs, and a third decides whether the account survives:

```python
wallet = ta.PaperWallet(
    10_000.0,
    max_gross=3.0,             # may hold 3x equity
    margin_rate=0.08,          # 8%/yr on the cash it borrowed to do so
    maintenance_margin=0.10,   # force-closed if equity drops under 10% of gross
    bar_freq="1d",             # what an "annual" rate is pro-rated against
)
```

**`margin_rate`** is interest on a *negative cash balance* — what a margin
account bills for the cash it lent you, which only becomes non-zero once
`max_gross` is above `1.0`. It needs `bar_freq`: an annual rate cannot be split
across a bar of unknown length, and rather than guess a year, the wallet charges
nothing and says so.

**`maintenance_margin` is off by default, and turning it on matters more than any
cost model.** The ratio is a venue assumption — it varies by exchange, instrument
and tier — so it is yours to state. Leaving it off does not make a backtest
slightly optimistic; it makes it describe a *different strategy*. A 3x long into
a 25% drawdown that then recovers reports **+6%** unliquidated and **−60%** with
a 10% ratio, on the same document and the same bars. Forced fills come back with
`kind == "liquidation"`.

**Perpetual funding** is a cost model rather than a wallet setting, because its
rate is *data*: it changes every settlement and flips sign, so a constant is not
a conservative stand-in for it. It reads a per-bar value off an overlay column —
which `binance-vision-futures` publishes as `funding_rate`, already summed per
bar:

```py
wallet.set_costs_for_all(["BTCUSDT"], ta.TradingCostsConfig({
    "carry": {"default": {"funding": {}}},          # read `funding_rate`
    # or {"both": {"rate": 0.08}} to add an annualized leg on top
}))
...
wallet.carry_coverage()      # (bars that wanted a rate, bars that got one)
```

`carry_coverage` is there because the failure is silent: a funding model whose
column is absent charges nothing on every bar, which looks exactly like carry
being free. `(1200, 0)` means it was configured, asked twelve hundred times, and
never got an answer.

> **Getters vs methods.** State a wallet or a frozen value object *already
> holds* is an attribute, not a call: `wallet.funds`, `wallet.equity`,
> `trade.bars_held`, `order.signed_units` — including derived readings like the
> last two, which are attributes because they describe the object rather than do
> anything. Anything that takes an argument (`position(sym)`, `price(sym)`),
> materializes a collection (`positions()`, `orders()`), advances or mutates
> state (`update()`, `reset()`), or builds a new object (`shared()`,
> `unstable()`, `not_()`) is a method. The streaming reads on indicators and
> signals — `value()`, `is_true()`, `is_ready()`, `warm_up_bars()` — are
> methods too: they belong to a live object being advanced, not to a value.

The wallet is fed each symbol's price with `update(symbol, price)` and is
otherwise market-agnostic. Sizes are an absolute number of units, or
`ta.Size.funds_frac(f)` (cash) / `ta.Size.value_frac(f)` (equity; `1.0` targets
1x equity, and `f` is not capped at `1.0`) / `ta.Size.position_frac(f)`; sides
are `"buy"`/`"sell"`. A movement that
can't be carried out — no/zero price fed, or a buy beyond available funds —
raises `ValueError`. A full strategy loop — price the wallet, advance **every**
signal each bar, then act:

```python
enter = ta.sma(ta.close(), 3).crosses_above(ta.sma(ta.close(), 10))
exit_ = ta.sma(ta.close(), 3).crosses_below(ta.sma(ta.close(), 10))
wallet = ta.PaperWallet(10_000.0)

for o, h, l, c, v in bars:
    candle = ta.Candle(o, h, l, c, v)
    wallet.update("AAPL", c)                          # price the wallet
    went_long, went_flat = enter.update(candle), exit_.update(candle)
    if went_long:
        wallet.set("AAPL", "buy", ta.Size.value_frac(1.0))   # long, 1x equity
    elif went_flat:
        wallet.close("AAPL")
```

### The declarative `Strategy` builder

For the classic long/flat/short shape, skip the hand-written loop: wire
entry/exit signals (and an optional sizing multiplier) onto a `Strategy` and
`run` it over a `PaperWallet`. You get back a `RunReport` — the per-bar equity
curve and the fill blotter — that the [metrics](#metrics) functions reduce to
numbers.

```python
import fugazi as ta
from fugazi.metrics import per_bar_returns, sharpe

enter = ta.sma(ta.close(), 3).crosses_above(ta.sma(ta.close(), 10))
exit_ = ta.sma(ta.close(), 3).crosses_below(ta.sma(ta.close(), 10))

strat = (
    ta.Strategy("AAPL")
    .long_on(enter, exit_)             # long/flat; add .short_on(down, up) for always-in
    .position_sizing(ta.value(0.5))    # optional: half-position (Kelly / vol-target fit here too)
)

prices = [10, 11, 12, 11, 10, 12, 14, 16, 15, 13, 15, 17, 19, 18]
ohlcv = {
    "open": prices,
    "high": [p + 1 for p in prices],
    "low": [p - 1 for p in prices],
    "close": prices,
    "volume": [1000.0] * len(prices),
}

wallet = ta.PaperWallet(10_000.0)
report = strat.run(wallet, ohlcv)      # a pandas/polars DataFrame or an OHLCV dict

report.equity_curve                    # one marked-to-market value per bar (list)
report.equity_array                    # ...the same, as a NumPy float64 ndarray
report.fills                           # list[Fill] — the blotter, in fill order
rets = per_bar_returns(report.equity_array, report.initial_equity)
sharpe(rets, 0.0, 252.0)
```

> Every one of those is a **property that rebuilds on access** — `equity_curve`
> allocates a fresh list of a million floats on a million-bar run, and `fills` a
> fresh `Fill` object per entry. Bind once (`curve = report.equity_curve`) rather
> than reading in a loop. `equity_array` avoids the boxing entirely and is what
> the metrics want: `Series` memcpys out of a contiguous buffer and falls back to
> element-by-element extraction for a list.

The builder mirrors Rust's `SingleAssetStrategy`: `long_on` / `short_on` (a
missing `exit` never fires — right for an always-in reversal), `position_sizing`
(scales the value-fraction magnitude; a `None` reading skips that bar's trade),
`rebalance_on` (below), and the strategy's book is seeded to the wallet's opening
equity. Signals must be candle- or snapshot-rooted (a bare-value signal is
rejected). Not bound yet: position-anchored protective stops and the Rust recipe
catalogue — drop to the wallet loop above for those.

`position_sizing` answers "what size?"; **`rebalance_on` answers "act on that
size right now?"**. It is **off by default** on `Strategy`, `PairsStrategy`,
`MultiAssetStrategy` and `Portfolio` — sizing reads only on transitions, so an
open position drifts with P&L — and **on by default, every bar, on
`BasketStrategy`**, whose cross-sectional ranking *is* its sizing decision.
Not calling the method is therefore not the same as gating it off; only on a
basket do the two coincide in spirit, and there the default is the opposite one.

`ta.every(N)` is the periodic pulse these gates are usually built from — the
binding of the spec's `!every N`. Its first fire is **delayed**, so `every(5)`
fires on bar 4 (0-indexed) and every 5th bar after, each pulse closing a full
block rather than firing immediately and again 5 bars later. Any other boolean
signal works too — compose with drawdown, calendar or weight-drift conditions
for event-driven rebalancing.

```python
gated = (
    ta.Strategy("AAPL")
    .long_on(ta.close().above(0.0))
    .position_sizing(ta.value(0.5))
    .rebalance_on(ta.every(20))    # hold the half-equity target ~monthly on daily bars
)
```

### Portfolios

`Portfolio` runs **N different strategies on one account**, behind a single
aggregate equity curve and blotter — the question none of the other shapes can
answer. Children are ordinary `Strategy` / `PairsStrategy` / `BasketStrategy` /
`MultiAssetStrategy` objects:

```python
snapshots = [ta.Snapshot({"BTC": c, "ETH": c}) for c in stream]

# Root every leaf on the symbol it reads — see the note below.
btc, eth = ta.close(source=ta.pick("BTC")), ta.close(source=ta.pick("ETH"))

pf = (ta.Portfolio()
        .add("trend",  ta.Strategy("BTC").long_on(ta.ema(btc, 5).crosses_above(ta.ema(btc, 10)),
                                                  ta.ema(btc, 5).crosses_below(ta.ema(btc, 10))))
        .add("revert", ta.Strategy("ETH").long_on(ta.ema(eth, 5).crosses_below(ta.ema(eth, 10)),
                                                  ta.ema(eth, 5).crosses_above(ta.ema(eth, 10))))
        .weights([0.7, 0.3]))          # magnitudes; normalized, default equal

report = pf.run(ta.PaperWallet(10_000.0), snapshots)
report.equity_curve[-1]
```

**Root your leaves.** A portfolio always feeds children the full multi-symbol
snapshot, so a bare `ta.close()` — which works in a standalone
`Strategy(...).run(wallet, candles)`, where each bar is a one-symbol frame —
has no way to choose an asset here and raises. Wrap each leaf in
`ta.close(source=ta.pick(sym))`, as the multi-symbol strategies below do.

Each child trades its own notional **ledger** — its slice of the account's cash
and positions — and sizes against that, so `value_frac(1.0)` in a child still
means all of *that child's* capital. Every child's intent is then netted into
one order per symbol. Two consequences follow from sharing a book: children
trading one symbol in opposite directions cross internally (and pay no spread or
commission, because that part never traded), and a child's stop takes off only
its own share.

The wallet passed to `.run()` is a **cash seed only** — a portfolio trades its
own account, so costs installed on that wallet don't apply. `.rebalance_on(sig)`
pulls capital back to the target weights when `sig` fires; without it the split
drifts with P&L. Like the other builders it is immutable: `.add(...)` returns a
new portfolio.

Not bound: live accounts (`substrate`) and per-child weight *expressions* — for
those, write the portfolio as a `portfolio:` YAML document and use `load_spec`.

### Multi-symbol strategies

`PairsStrategy`, `MultiAssetStrategy` and `BasketStrategy` mirror their Rust
siblings and drive over a sequence of snapshots (`.run(wallet, snapshots)`).
Their signals are snapshot-rooted, so atom leaves are rooted per symbol with
`ta.pick(sym)`.

**`PairsStrategy` requires it.** A pair privileges neither leg, so a leaf that
named no asset — a bare `ta.close()` — has no series to read on a bar that
carries both, and the builder raises `ValueError` rather than failing on the
first bar. That includes the calendar leaves: `ta.day_of_week()` reads only the
bar's timestamp, but it still has to say whose bar, and since both legs share
the time, rooting it on either one gives the same answer
(`ta.day_of_week(ta.pick("BTC"))`). Constants (`ta.value(0.5)`) read no series
and stay legal. The YAML side refuses the same document for the same reason.

`MultiAssetStrategy` and `BasketStrategy` take **per-symbol factories** instead
— `sym -> Signal` / `sym -> Indicator` callables, so each symbol gets its own
chain rooted on itself. Passing the indicator directly is the common slip and
raises `TypeError` at wiring time; a factory that *runs* and then fails does so
inside the driver, which has no error channel, and still surfaces as a
`PanicException`.

`PairsStrategy` trades the **spread** `close(left) − close(right)`, long / flat
/ short on it. `long_spread_on` goes long `left` / short `right` (profiting as
the spread rises); `short_spread_on` is the mirror. A mean-reverting spread
visits both tails and the correct position is opposite at each, so wiring only
one side skips every excursion on the other:

```python
spread = ta.close(ta.pick("BTC")).sub(ta.close(ta.pick("ETH")))
z = ta.zscore(spread, 60)

pair = (
    ta.PairsStrategy("BTC", "ETH")
    # spread cheap -> long it, close on reversion through 0
    .long_spread_on(z.lt(ta.value(-2.0)), z.gt(ta.value(0.0)))
    # spread rich -> short it (short BTC, long ETH)
    .short_spread_on(z.gt(ta.value(2.0)), z.lt(ta.value(0.0)))
)
```

The two directions are inverse positions, so they are mutually exclusive in time
and share one capital pool at full notional; the opposite side's entry reverses
an open pair. Per-side spread levels
(`long_spread_stop_loss` / `short_spread_stop_loss` and the take-profit twins)
compare with mirrored sense — the short side stops out when the spread rises
*above* its level. `on` / `spread_stop_loss` / `spread_take_profit` remain valid
as aliases for the long-spread side.

## Guide: strategies as documents

The CLI's YAML surface (see the crate root's `strategy.yml` examples) is
available natively from Python. `ta.load_spec(text)` parses a spec
document, auto-detects its shape (single / pairs / basket / multi /
portfolio), and returns a `StrategySpec` that implements the same
`.run(wallet, snapshots)` interface as the manual [`Strategy`](#the-declarative-strategy-builder)
builder. `.evaluate(...)` is a bonus method that runs + reduces to a metrics
dict in one call.

`load_spec` validates as it loads: an unknown tag, a misspelled field or a
decidably-wrong slot type raises here, not on some later bar. That includes the
per-symbol templates — a basket's `score:` / `sizing:`, a multi-asset side's
`enter:`, a portfolio's `weights:` — whose *values* are deferred until the driver
binds a symbol but whose shape is checked up front, with each `!arg` held as a
placeholder.

```python
import fugazi as ta

spec = ta.load_spec("""
root: BTC
long:
  enter: !crosses_above
    lhs: !sma { period: 3 }
    rhs: !sma { period: 10 }
""")
assert spec.kind == "single"

snaps = [
    ta.Snapshot({"BTC": ta.Candle(v, v, v, v, 1.0)})
    for v in [10, 9, 8, 7, 6, 7, 9, 12, 15, 18, 21, 22, 21, 20, 18, 15, 12, 10, 8, 6]
]
wallet = ta.PaperWallet(1000.0)
report = spec.run(wallet, snaps)              # -> RunReport
metrics = spec.evaluate(ta.PaperWallet(1000.0), snaps)  # -> nested dict mirroring metrics.yml
```

`spec.meta` returns the document's free-form
[`meta:`](https://github.com/acpuchades/fugazi/blob/main/docs/STRATEGIES.md#metadata--meta) block as ordinary Python data —
dicts, lists, and scalars — or `None` when the document sets none. fugazi never
interprets it; it is the open-schema slot for whatever service produced or
stores the strategy, and it is available on all five shapes:

```python
spec = ta.load_spec("""
root: BTC
meta:
  service: strategy-lab
  id: 4f1c-9a2b
  tags: [momentum, crypto]
long:
  enter: !value true
""")
assert spec.meta["tags"] == ["momentum", "crypto"]
```

`spec.reads` lists the symbols the document reads through an explicit
`!pick { symbol: ... }` but never trades — a regime gate on another asset, a
spread leg. Those symbols have to be **entries in the snapshots you pass**, or
the expression resolves `None` on every bar and nothing ever fires: `Pick` reads
`None` on a bar it does not match, which is right for a listing gap and
indistinguishable, from the outside, from a series that was never supplied. The
CLI makes this check against `--series` and refuses the run; here the snapshots
are yours to construct, so the check is yours too:

```python
spec = ta.load_spec("""
root: ETH
long:
  enter: !gt
    lhs: !close { source: !pick { symbol: BTC } }
    rhs: !sma { period: 200, source: !close { source: !pick { symbol: BTC } } }
""")
assert spec.reads == ["BTC"]

bar = ta.Candle(100.0, 100.0, 100.0, 100.0, 1.0)
snap = ta.Snapshot()
snap.push("ETH", ta.Atom(bar, time=0))
snap.push("BTC", ta.Atom(bar, time=0))   # ← without this, the gate never fires
```

`spec.reads` is `[]` for the ordinary document that only reads what it trades.

Pass `windowed=N` to `.evaluate(...)` for the same windowed/rolling reductions
`run -w N` writes to `metrics.csv`/`rolling.csv`: the returned dict gains
`windowed` (non-overlapping N-bar spans — independent, for cross-window
statistics) and `rolling` (stride-1 spans — heavily autocorrelated, for a
continuous rolling-Sharpe-style curve) keys, each a list of `{"start_bar",
"end_bar", "metrics"}`. Unlike the CLI's `-w`, this takes a plain bar count —
no duration/asset-class resolution.

#### Monte Carlo significance and the resampling primitive

Pass `montecarlo=ta.MonteCarloConfig(...)` to `.evaluate(...)` for the
significance pass — bootstrap confidence intervals plus empirical-null p-values
over a resampling scheme (`iid` / `moving-block` / `stationary`). The returned
dict gains a `montecarlo` block (mirroring `metrics.yml`'s), plus the raw
per-resample metric values under `montecarlo["samples"]`.

The significance layer reduces every resample to metric rows and discards the
resampled *paths*. To draw a Monte Carlo **equity fan chart** (percentile bands
of the resampled equity paths over time) you rebuild the paths yourself from one
generic knob — the deterministic resampling index draws, exposed as
`fugazi.montecarlo`:

```text
resample_index_matrix(n, permutations, *, scheme="stationary", block=10.0, seed=0)
    -> list[list[int]]              # permutations × n, every index in 0..n
resample_indices(n, *, scheme="stationary", block=10.0, seed=0)
    -> list[int]                    # one sequence == permutation 0 of the matrix
```

The bootstrap-CI estimator draws first from the run's seed stream via the same
primitive, so calling `resample_index_matrix` with `n = len(returns)` and the
run's `permutations`/`scheme`/`block`/`seed` reproduces exactly the permutations
behind the CIs. Every scheme yields a same-length synthetic series, so each
rebuilt path is the same length as the source and maps 1:1 onto the original bar
timestamps. Nothing large crosses a process boundary — you feed scalars and
rebuild wherever you like:

```python
import numpy as np
import fugazi as ta

spec = ta.load_spec("root: BTC\nlong:\n  enter: !crosses_above"
                    " { lhs: !sma { period: 3 }, rhs: !sma { period: 10 } }")
snaps = [ta.Snapshot({"BTC": ta.Candle(v, v, v, v, 1.0)})
         for v in [10, 9, 8, 7, 6, 7, 9, 12, 15, 18, 21, 22, 21, 20, 18, 15, 12, 10, 8, 6]]

rep = spec.run(ta.PaperWallet(1000.0), snaps)
r   = np.array(ta.metrics.per_bar_returns(rep.equity_curve, rep.initial_equity))
idx = np.array(ta.montecarlo.resample_index_matrix(
        len(r), 1000, scheme="stationary", block=10, seed=0))
paths = rep.initial_equity * np.cumprod(1 + r[idx], axis=1)   # (permutations × bars)
bands = {f"p{q}": np.percentile(paths, q, axis=0).tolist() for q in (5, 25, 50, 75, 95)}
spaghetti = paths[:200].tolist()                              # optional capped overlay
```

Bar `k`'s band shares the *time axis* (position `k` ↔ `times[k]`) but is the
k-th step of a synthetic return walk — a Monte Carlo fan, not a forecast
conditioned on the real market at `times[k]`.

Preset tags (`!buy_and_hold`, `!ma_crossover`, `!rsi_reversal`,
`!donchian_breakout`, `!keltner_breakout`) work directly:

```python
spec = ta.load_spec("!buy_and_hold { root: BTC }")
```

The five shapes are auto-detected by top-level YAML key:

| Top-level key(s)        | Detected kind |
| ---                     | ---           |
| `children:`             | `portfolio`   |
| `left:` + `right:`      | `pairs`       |
| `selection:`            | `basket`      |
| `root:` or preset tag   | `single`      |
| (bare mapping)          | `multi`       |

Pass `kind="single"` / `"pairs"` / ... to override detection, and
`params={"NAME": value}` to fill `!param` placeholders in the document.

### Resuming a run, and running against a venue

`.run(wallet, snapshots)` accepts a `PaperWallet`, an `OkxWallet` or a
`CoinbaseWallet` — the same three the manual `Strategy` builder takes — for every
shape, portfolio included. Positions the account already holds are treated as the
user's own and left untouched; the strategy sizes against its own capital.

`.run_resumable(...)` is the same run with its **state** surfaced, so a long backtest
or a live deployment can stop and pick up exactly where it left off:

```python
text = """
root: BTC
long:
  enter: !crosses_above
    lhs: !sma { period: 3 }
    rhs: !sma { period: 10 }
  exit: !crosses_below
    lhs: !sma { period: 3 }
    rhs: !sma { period: 10 }
"""
snaps = [ta.Snapshot({"BTC": ta.Candle(v, v, v, v, 1.0)}) for v in prices]
january, february = snaps[:20], snaps[20:]

rep, state = ta.load_spec(text).run_resumable(ta.PaperWallet(10_000.0), january)
# `state` is a JSON string — persist it however you like.

# Later, in another process: rebuild from the document, resume from the state.
rep2, state2 = ta.load_spec(text).run_resumable(
    ta.PaperWallet(10_000.0), february, resume=state
)

# Same as never having paused.
whole, _ = ta.load_spec(text).run_resumable(ta.PaperWallet(10_000.0), snaps)
assert rep.equity_curve + rep2.equity_curve == whole.equity_curve
```

The resumed run is **bit-identical** to one that never paused — chunk a series any
number of ways and the concatenated equity curve and fills match the uninterrupted
run exactly, for all five shapes. Resuming into a different shape, or from a state
written by a different build, raises `ValueError` rather than mis-parsing; there is no
migration between state versions, so regenerate by re-running the history.

`flatten=True` closes every open position at the last bar — a real order through the
cost pipeline, so it moves cash and pays commission — and books the closing legs into
the report. The state it returns holds a genuinely flat book.

Against a **live** wallet the state's `wallet` field is `null`: the venue owns the
positions and the cash, so only the strategy's own indicator state is carried and the
account is re-read on resume. (`.evaluate(...)`'s Monte Carlo pass re-drives the spec
against its own paper wallets, so pass a paper wallet there if you use it.)

`.warm_up(wallet, snapshots, resume=None)` advances the strategy **without trading**
and returns the state alone — no report, because no run happened. It exists for the
*pause gap*: bars that elapsed while a deployment was stopped have to warm the
indicators, but must not book trades at prices nobody could have traded at. Replay the
gap through `warm_up`, hand the state to `run_resumable`, and go live — instead of
discarding the state and re-serving a long-period indicator's whole warm-up after
every pause.

```python
spec = ta.load_spec("""
root: BTC
long:
  enter: !crosses_above
    lhs: !sma { period: 3 }
    rhs: !sma { period: 10 }
""")
snaps = [ta.Snapshot({"BTC": ta.Candle(v, v, v, v, 1.0)}) for v in prices]
wallet = ta.PaperWallet(10_000.0)

# Bars that elapsed while the deployment was paused: warm the SMAs, trade nothing.
state = spec.warm_up(wallet, snaps[:20])
assert wallet.funds == 10_000.0

# Then go live from there, already warmed.
rep, state = spec.run_resumable(wallet, snaps[20:], resume=state)
```

### Parameter-grid optimize

`ta.optimize(text, snapshots, ...)` sweeps a parameter grid, ranks rows by
`--best-by`-style metric, and returns a `Sweep`:

```python
spec_yaml = """
root: BTC
long:
  enter: !crosses_above
    lhs: !sma { period: !param FAST }
    rhs: !sma { period: !param SLOW }
"""
opt_snaps = [
    ta.Snapshot({"BTC": ta.Candle(v, v, v, v, 1.0)})
    for v in [100 + i * 0.5 for i in range(40)]
]

sweep = ta.optimize(
    spec_yaml,
    opt_snaps,
    cash=1000.0,
    grid=[{"FAST": [3, 5, 7], "SLOW": [10, 15]}],
    metric_names=["risk_adjusted.sharpe", "returns.total_pct"],
    best_by="risk_adjusted.sharpe",
)
sweep.columns          # -> ["FAST", "SLOW"]
sweep.rows[0].values   # -> {"FAST": 3, "SLOW": 10}
sweep.rows[0].metrics  # -> {"risk_adjusted.sharpe": ..., "returns.total_pct": ...}
sweep.best             # -> highest-ranked row (None when best_by is unset)
```

`grid` is a list of dicts (one per subgrid; stacked subgrids union), where
values that are lists become sweep axes and `"start..end[:step]"` strings
expand to numeric ranges. An axis list must not repeat a value — the Cartesian
product would just repeat the point, at the cost of a second backtest and a
duplicate row — and `20` and `20.0` count as one value, since they substitute
identically. Pass `windowed=N` to reduce each grid point across
non-overlapping N-bar windows (`row.metrics_windowed` carries the per-window
docs), or `walkforward=(is, oos)` / `walkforward=(is, oos, embargo)` for
walk-forward validation:

```python
wf_yaml = """
root: BTC
long:
  enter: !crosses_above
    lhs: !sma { period: !param FAST }
    rhs: !sma { period: 15 }
"""
wf_snaps = [
    ta.Snapshot({"BTC": ta.Candle(v, v, v, v, 1.0)})
    for v in [100 + i * 0.5 for i in range(40)]
]

result = ta.optimize(
    wf_yaml,
    wf_snaps,
    cash=1000.0,
    grid=[{"FAST": [3, 5]}],
    best_by="risk_adjusted.sharpe",
    walkforward=(5, 3),
)
# -> WalkForwardResult with per-fold IS/OOS metrics + composite OOS equity
for fold in result.folds:
    fold.is_range, fold.oos_range     # bar ranges
    fold.values                         # winning params for that fold
    fold.is_metrics, fold.oos_metrics   # nested metrics dicts
result.composite_equity                 # stitched OOS curve
result.composite_metrics                # composite metrics doc
```

### Pooling across a panel

`panel=` fits **one** parameter set across several instruments instead of
picking the best `(params, instrument)` cell. The instrument axis is *reduced
over* rather than ranked on, so the grid stays `N` hypotheses wide rather than
`N × M` — which is the count a deflated Sharpe should be parameterized by.

Members are passed as separate snapshot streams, keyed by name. Separate
streams matter: one merged multi-instrument stream would make every member run
over the *union* timeline and see bars on which it has no quote.

```python
panel_doc = """
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

DAY = 86_400_000


def member_stream(sym, start_ms, closes):
    return [
        ta.Snapshot({sym: ta.Atom(ta.Candle(c, c, c, c, 1.0), time=start_ms + i * DAY)})
        for i, c in enumerate(closes)
    ]


wave = [100.0 + (i % 8 if i % 8 < 4 else 8 - i % 8) * 3.0 for i in range(120)]
panel = {
    # Each member is its own stream. One merged multi-instrument stream would
    # make every member run over the union timeline and see bars it has no
    # quote on.
    "AAA": member_stream("AAA", 0, wave),
    "BBB": member_stream("BBB", 0, [v * 1.1 for v in wave]),
    # Lists 40 days later than the others — a ragged panel is the normal case.
    "CCC": member_stream("CCC", 40 * DAY, wave[:80]),
}

sweep = ta.optimize(
    panel_doc,
    panel=panel,
    panel_axis="SYM",          # substitutes each member's name for !param SYM
    grid=[{"FAST": [3, 5], "SLOW": [10, 20]}],
    best_by="sharpe",
    risk_aversion=0.5,         # penalizes a set that works on only one member
    metric_names=["sharpe"],
    bars_per_year=365,
)

row = sweep.best
row.values           # the winning parameter set — no "SYM" key, it was pooled
row.metrics          # pooled means:  {"risk_adjusted.sharpe": 1.42, ...}
row.metrics_support  # support:       {"risk_adjusted.sharpe": (3, 3)}
row.metrics_panel    # per member:    {"AAA": {...}, "BBB": {...}, "CCC": {...}}
```

`panel_axis=` is optional; without it the same document runs against every
member, which is right for a sole-atom `root:` that reads whatever the bar
carries. With it, the member's key is substituted for that `!param` first, so
each member is rooted on its own series — the same thing the CLI's
`--pooled` does. A document that names a symbol *no* member's stream
carries is an error rather than a flat zero-trade backtest that still counts
toward the pooled mean.

**A member key keeps its Python type.** `str` substitutes as a JSON string,
`int`/`float` as a number, `bool` as a boolean — so the axis need not be an
instrument. Pooling over a *parameter* is the CLI's
`--pooled 'FAST=[5,10,15]'`, and it spells the same way here:

```python
param_doc = """
root: BTC
long:
  enter: !crosses_above
    lhs: !sma { period: !param FAST }
    rhs: !sma { period: !param SLOW }
sizing: !value 1.0
"""

DAY = 86_400_000
wave = [100.0 + (i % 8 if i % 8 < 4 else 8 - i % 8) * 3.0 for i in range(120)]
one_stream = [
    ta.Snapshot({"BTC": ta.Atom(ta.Candle(c, c, c, c, 1.0), time=i * DAY)})
    for i, c in enumerate(wave)
]

sweep = ta.optimize(
    param_doc,
    # One series, three members: the axis is a *parameter*, not an instrument.
    panel={f: list(one_stream) for f in (5, 10, 15)},
    panel_axis="FAST",          # reaches `period:` as the number 5, not "5"
    grid=[{"SLOW": [30, 50]}],
    best_by="sharpe",
    metric_names=["sharpe"],
    bars_per_year=365,
)

sweep.best.values              # {"SLOW": ...} — FAST was pooled over, not ranked on
set(sweep.best.metrics_panel)  # {"5", "10", "15"} — the keys, unquoted
```

Nothing is parsed out of the label, so a member genuinely named `"5"` stays the
string `"5"` — `{5: …}` and `{"5": …}` are different members. The label a
pooled cell reports is the key without JSON quoting (`"5"`, `"BTC"`). Note that
a panel member always carries its own stream, so the parameter case hands the
same series over once per member; that is the cost of members being data-keyed,
which is what keeps a ragged instrument panel from running every member over
the union timeline.

`panel=` is the same reduction as `windowed=` over a different partition, so
`risk_aversion=` composes and means the same thing.

`windowed=` **composes with** `panel=` and changes no pooled number: each member
is measured once and reduced twice, so the whole-run document every pooled cell
reads is untouched and the per-window documents ride beside it as within-cell
*replicates*. That replication is what `shrink=` needs (below); without it,
"the members disagree" and "the backtests are noisy" are the same quantity.

Two properties are worth stating outright:

- **An undefined metric stays undefined.** The pooled mean is over the members
  that *reported* — a member that never traded has no win rate and is dropped,
  not counted as zero. `metrics_support` gives `(defined, members)` so a mean
  over 2 of 30 and a mean over 30 of 30 don't read identically.
- **A ruined member disqualifies the row.** Not "drops out of the mean" —
  dropping it would *raise* the pooled score and reward a search for parameters
  that destroy an account.

#### Partial pooling (`shrink=`)

`panel=` alone is **complete** pooling: one parameter set for the whole panel,
right only when the members share an optimum. A plain `SYM=[...]` grid axis is
**no** pooling: one per member, each fit on its share of the evidence.
`shrink=True` is the middle — estimate how much of the spread between members is
real disagreement rather than backtest noise, and let each member move that far
and no further.

```python
shrink_doc = """
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

DAY_MS = 86_400_000
ramp = [100.0 + i * 0.4 + (12.0 if 60 <= i < 100 else 0.0) for i in range(160)]


def shrink_member(sym, closes):
    return [
        ta.Snapshot({sym: ta.Atom(ta.Candle(c, c, c, c, 1.0), time=i * DAY_MS)})
        for i, c in enumerate(closes)
    ]


sweep = ta.optimize(
    shrink_doc,
    grid=[{"FAST": [3, 5], "SLOW": [10, 20]}],
    panel={
        "AAA": shrink_member("AAA", ramp),
        "BBB": shrink_member("BBB", [v * 1.1 for v in ramp]),
    },
    panel_axis="SYM",
    windowed=40,           # supplies the replication lambda needs
    best_by="returns.total_pct",
    shrink=True,
    cash=1000.0,
)
assert "SYM" not in sweep.best.values   # reduced over, not ranked on
```

The sweep reads its results as a row x member table and decomposes it into a
shared parameter effect, a per-member level, and the interaction between them:

```text
lambda = interaction variance / (interaction variance + noise)
```

At `lambda = 0` the members share an optimum and every member picks the pooled
winner; at `lambda = 1` they are separate problems and each picks its own.

#### Reading the result

`lambda` is spelled **`disagreement`** on the Python side, because `lambda` is a
keyword and `sweep.shrinkage.lambda` would be a `SyntaxError`. The CSV column
and the prose keep the symbol; only the attribute differs.

```py
s = sweep.shrinkage                 # a PanelShrinkage, or None if not pooled
s.disagreement                      # lambda in 0..=1, or None (see below)
s.parameter_matters                 # did the grid move this metric at all?
s.verdict                           # the one-line reading, caveat folded in
s.support, s.cells                  # how much of the table backs it
s.residual_variance                 # None on an unreplicated table
sweep.shrunk                        # were the rows ranked on the demeaned score?
sweep.best.demeaned                 # (mean, std, defined, members) of that score
```

`disagreement` being `None` is **not** zero. It means the table carried no
within-cell replication, so disagreement and noise are the same sum of squares
and no split exists — a different statement from "the members agree perfectly".
Everything else stays defined and reported. `verdict` says so in words, which is
why it is the safe thing to print:

```py
sweep = ta.optimize(..., panel=..., best_by="sharpe")          # no windowed=
assert sweep.shrinkage.disagreement is None
assert sweep.shrinkage.verdict == "not estimable without replication"
```

Read `parameter_matters` *with* `disagreement`, never instead of it: a high
`lambda` on a grid that barely moves the metric means the members disagree about
which of several equivalent parameter sets is marginally best, which is not the
finding it looks like. `verdict` folds that caveat in, and `repr()` shows both —
so printing either one cannot mislead.

Under `walkforward=`, the same reading is available per fold and for the run:

```py
result.shrinkage            # run-wide, folds as replicates — better powered,
                            # but a description, never what a fold selected on
result.departures           # {member: folds} it departed in, most-frequent first
for fold in result.folds:
    fold.shrinkage          # this fold's own lambda, from its own in-sample data
    fold.member_winners     # {member: {axis: value}} — each member's own pick
    fold.departed           # members that differed from the pooled winner here
```

`result.departures` is the reading a run-level `lambda` flattens: "one member
went its own way in every fold" and "everyone drifted once" can produce the same
mean and mean very different things.

**Per-fold and run-wide `disagreement` will differ, and that is not a bug.**
Expect the per-fold numbers to read *lower* — 0.275 / 0.0 / 0.0 against 0.815
run-wide is an ordinary spread, not a contradiction.

Each fold estimates from sub-spans of its own in-sample window, which is what
keeps it lookahead-free: a component estimated over every fold and applied
inside fold 1 would let fold 10's data pick fold 1's winner. But a metric
measured over a short sub-span is itself noisy, that noise lands in the
denominator, and `disagreement` comes out conservative as a result.
`result.shrinkage` uses whole folds as replicates, so it is better powered — and
is a description of the run after the fact, never something a fold selected on.

A low per-fold reading beside a high run-wide one is the fold saying it cannot
yet separate disagreement from noise on its own evidence. If you render both,
label which is which; `docs/CLI.md` carries the longer version.

#### Pooling it yourself: `ScoreTable`

`shrink=` is a parameter of `optimize(panel=…)`. If you reduce across members
with your own machinery — one `optimize()` per member over that member's own
snapshots, say — there is nothing for it to be plumbed into, and reaching it
would mean giving up whatever made your own pooling worth having.

`ScoreTable` is the same estimator with the sweep taken off the front. Hand it a
row x member matrix and it gives back everything the flag would have:

```py
t = ta.ScoreTable(rows=len(grid), members=len(panel))
for r, params in enumerate(grid):
    for m, member in enumerate(panel):
        # the replicates: one reading per sub-span, NOT one per cell
        t.extend(r, m, sharpe_per_window(params, member))

d = t.decompose()                 # None if the table cannot carry the fit
d.summary.disagreement            # lambda
d.summary.verdict                 # the safe thing to print
d.demeaned                        # rows x members, member level removed
d.shrunk                          # rows x members, or None without lambda
d.selection_breadth               # (effective, rho, members, pairs)
```

`ta.ScoreTable.from_cells(cells)` builds one from a nested
`cells[row][member] -> replicates` if you already have the matrix.

Then take an argmax down each column of `shrunk` to get that member's own
parameters, and multiply your candidate count by `selection_breadth[0]` before
deflating — letting every member select for itself takes the maximum over more
draws than the candidate count alone admits.

Four rules the type signatures already imply, spelled out because getting any of
them wrong is silent:

- **Cells hold replicates, not one number.** With a single observation per cell,
  "the members disagree" and "the backtests are noisy" are the same sum of
  squares. `disagreement` is then `None` — and `shrunk` and `selection_breadth`
  with it, since there is no defensible surface. `demeaned` still works: the
  additive fit needs no replication.
- **A pair you never measured is an empty cell, never a zero.** A substituted
  zero is indistinguishable from a measurement and would sink into the fit. The
  hole survives into `demeaned`, `shrunk` and `interactions` as `None`.
- **`decompose()` returning `None` means "not enough table", not "zero".** Under
  6 populated cells, under two live rows or members, or no degrees of freedom
  left for an interaction. `populated` and `replicated_cells` say which.
- **A ragged *input* raises; a ragged *table* is ordinary.** `from_cells`
  refuses rows of differing length — an unmeasured pair is an empty sequence,
  not a missing column.

Three more things to know before using it:

- **It needs replication, and says so when it has none.** With one measurement
  per member, disagreement and noise are the same quantity, so `lambda` is
  `None` rather than a number invented out of an identification failure. Pass
  `windowed=` in a sweep; under `walkforward=` each fold splits its own
  in-sample window and needs no extra argument.
- **It refuses `risk_aversion=`.** The two are rival answers to the same
  question — `risk_aversion=` *charges* a parameter set for the spread between
  members, `shrink=` *models* it — and applying both pays for the same
  disagreement twice.
- **It hands back one parameter set per member**, in `sweep.member_winners` —
  `{member: {axis: value}}`. It always lists **every** member, including the
  ones that landed on the pooled winner, so an empty dict means the sweep was
  not shrunk and *never* that the panel agreed. For "did anyone depart", read
  `sweep.departed` (or `fold.departed`, or `result.departures`) — those do carry
  empty-means-agreed, and an empty one is a result rather than an absence.
  `sweep.independent_searches` reports how many independent searches over the
  grid those selections amounted to: `1.0` when the members agree, up to the
  member count when they share nothing. That is the factor the deflated Sharpe's
  trial count was scaled by — per-member selection searches the grid harder than
  complete pooling does, and the deflation is widened to match. On an agreeing
  panel it is exactly `1.0`, so every row's DSR is unchanged from a run without
  `shrink=`.

#### Pooled walk-forward

`panel=` composes with `walkforward=`, which is the point of it: each fold
picks one winner on the **pooled** in-sample score and applies it
out-of-sample to every member, so all the composites switch parameters on the
same dates. Running the single-stream walk-forward once per instrument would
fit a *different* parameter set to each, which is the opposite of pooling.

```python
pwf_doc = """
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

DAY = 86_400_000
wave = [100.0 + (i % 8 if i % 8 < 4 else 8 - i % 8) * 3.0 for i in range(120)]


def member_stream(sym, start_ms, closes):
    return [
        ta.Snapshot({sym: ta.Atom(ta.Candle(c, c, c, c, 1.0), time=start_ms + i * DAY)})
        for i, c in enumerate(closes)
    ]


panel = {
    "AAA": member_stream("AAA", 0, wave),
    "BBB": member_stream("BBB", 0, [v * 1.1 for v in wave]),
    "CCC": member_stream("CCC", 40 * DAY, wave[:80]),
}

result = ta.optimize(
    pwf_doc,
    panel=panel,
    panel_axis="SYM",
    grid=[{"FAST": [3, 5], "SLOW": [10, 20]}],
    walkforward=(40, 20),         # or (is, oos, embargo)
    best_by="sharpe",
    bars_per_year=365,
)
# -> PanelWalkForwardResult

result.axis_len       # length of the panel's shared clock
result.prefix_skip    # readiness bars trimmed off its head
result.members        # ["AAA", "BBB", "CCC"]

for fold in result:
    fold.is_range, fold.oos_range      # ranges on the *shared clock*
    fold.values                        # the one winner for this fold
    fold.is_support_members            # members with bars in the IS window
    fold.oos_support_members
    fold.metrics_is, fold.metrics_oos  # per-member docs, keyed by member

result.pooled("sharpe")   # (mean, std, defined, members) over the composites
for c in result.composites:
    c.member, c.equity, c.metrics      # one stitched OOS curve per member
```

**The panel may be ragged.** Instruments list at different dates, so folds are
laid out on the shared clock — the sorted union of every member's bar times —
not on any member's bar indices, and `is_range` / `oos_range` index into
*that*. A member with no bars in a fold's window contributes nothing to it and
does not shift it; `is_support_members` reports how many members each fold
actually rested on. Folds begin once the **first** member is ready rather than
the last, since waiting for every member would truncate the panel's history to
its most recent listing.

There is deliberately no single netted composite curve: netting `M` members
into one account needs a weighting and a rebalance cadence, which is an
allocation policy fugazi expresses explicitly with `portfolio:` rather than
inventing inside `optimize`.

#### How much evidence is a panel worth?

A pooled row reports `N` hypotheses rather than `N × M`, which is the honest
count — and it invites the reading that `M` members are `M` pieces of evidence.
For a panel drawn from one market's worth of instruments they are not.

```py
result.effective_breadth   # (effective, mean_correlation, members, pairs) | None
```

The reading is the standard one for an equal-weighted mean of `M` estimators
with average pairwise correlation `ρ̄`:

```text
effective = M / (1 + (M − 1)·ρ̄)
```

| Members | ρ̄ = 0.3 | ρ̄ = 0.5 | ρ̄ = 0.8 |
|---|---|---|---|
| 10 | 2.6 | 1.8 | 1.2 |
| 30 | 3.1 | 1.9 | 1.2 |
| 100 | 3.3 | 2.0 | 1.2 |

Crypto majors on a common timeframe sit near the right-hand column, so a
thirty-member panel of them is worth about **1.2** independent members and its
pooled Sharpe deserves roughly the confidence of a single backtest — not
thirty. Note how little the columns move as `M` grows: past a handful of
correlated members you are adding compute, not evidence.

Three properties worth stating:

- **Measured on the composites' own returns**, not on the members' price
  series. What a pooled figure rests on is how much the *results* co-moved; a
  strategy that trades two correlated markets at different times produces two
  more nearly independent curves than their prices would suggest, and is
  credited for it.
- **Each pair is joined on its own shared bars**, never on a global
  intersection — the same rule fold layout follows. A member that listed last
  week overlaps the rest by a fortnight; intersecting everything first would
  collapse the axis every other pair is measured on down to that. A pair with
  fewer than 30 shared bars contributes nothing rather than a coefficient
  computed from noise, and `pairs` reports how many were actually measured.
- **Reported, never applied.** What to do with it — deflate against it, widen
  an interval, or go and find less correlated members — is a decision the
  caller has the context to make and this crate does not.

`None` when fewer than two members share enough history to be correlated at
all. Reporting the member count there would be answering a question nobody
could check.

The CLI has an ergonomic twin of the plain (non-walk-forward) case:
`fugazi run --pooled 'AXIS=[...]'` reports one already-chosen parameter set's pooled
reading — the same thing `ta.optimize(..., panel=panel, grid=[{}])` computes,
with per-member `fills.csv`/`trades.csv`/`returns.csv`/`metrics.yml` written to
disk instead of read back as `row.metrics_panel`.

`smooth=` mirrors the CLI's `--smooth`: rank `best_by` by a kernel-weighted
average over each grid point's *parameter neighbourhood* rather than by the
point estimate, so a broad plateau outranks a lone spike. `"box:R"`,
`"triangle:R"` or `"gaussian:S"`, with radii in **grid steps**: distance along
an axis is the parameter gap divided by that axis' own median gap, so `1` means
one typical step of *that* axis and an evenly spaced axis behaves exactly as
"one declared position along". Each axis picks linear or log spacing by
whichever makes its own gaps more uniform; `smooth_scale=` pins it
(`"index"`, `"PERIOD:log"`, `"linear,PERIOD:log"`), with `"index"` restoring
the pre-0.65 measure between declared positions. A `NAME:SCALE` pin whose
`NAME` no subgrid sweeps is a `ValueError` — an unmatched pin is never looked
up, so it would otherwise leave the axis on the automatic choice in silence.
Non-numeric axes and
single-value axes partition rather than smooth, each subgrid is its own
lattice, and boundary points renormalize over the neighbours they have —
`smooth_min_support=` discards a row whose realized support falls below a
fraction of a fully interior point's. It composes with `risk_aversion=` (which
is folded into the key first) and applies per fold under `walkforward=`.

**Smoothing needs a lattice, and `grid=` can be given one that has none.**
`grid=` accepts a Cartesian block (`{"FAST": [3, 5, 7], "SLOW": [10, 15]}`) or
a list of concrete points (`[{"FAST": 3, "SLOW": 10}, …]`) — and since each
subgrid is its own lattice, a point list is *N one-point lattices*, not one
lattice of N points. Anyone who filters a product down to its legal
combinations (`FAST < SLOW`) is holding exactly that. With no numeric axis of
two or more values anywhere in the grid, `smooth=` is a `ValueError`: it would
otherwise return every point's raw key unchanged, and `smooth_min_support=`
could not catch it — a point with no neighbourhood has no support to compare a
floor against. Pass the swept axes as a block. A grid that *mixes* the two is
fine, and the lone pinned point reports `row.support is None` rather than a
number, because nothing was measured for it; any `smooth_min_support=` above
`0` drops it.

```python
smooth_yaml = """
root: BTC
long:
  enter: !crosses_above
    lhs: !sma { period: !param FAST }
    rhs: !sma { period: !param SLOW }
"""
smooth_snaps = [
    ta.Snapshot({"BTC": ta.Candle(v, v, v, v, 1.0)})
    for v in [100 + i * 0.5 for i in range(40)]
]

sweep = ta.optimize(
    smooth_yaml,
    smooth_snaps,
    cash=1000.0,
    grid=[{"FAST": [3, 5, 7], "SLOW": [10, 15, 20]}],
    metric_names=["returns.total_pct"],
    best_by="returns.total_pct",
    smooth="box:1",
    smooth_min_support=0.5,
)
sweep.rows[0].smoothed   # -> neighbourhood average, native orientation
sweep.rows[0].support    # -> 1.0 for a fully interior point, less at an edge,
                         #    None for a point whose subgrid has no smoothed axis
# Under walkforward=, each fold reports the key it was actually selected on:
# fold.is_smoothed / fold.is_support
```

### Costs

Trading costs load from a Python dict matching the CLI's YAML shape
(externally-tagged models: `!percentage`, `!bps`, `!volume_participation`, …):

```python
costs = ta.TradingCostsConfig({
    "commission": {"percentage": {"rate": 0.001}},
    "spread":     {"bps": {"bps": 5}},
})
cost_yaml = "!buy_and_hold { root: BTC }"
cost_snaps = [
    ta.Snapshot({"BTC": ta.Candle(v, v, v, v, 1.0)})
    for v in [100, 101, 102, 103, 104]
]
sweep = ta.optimize(cost_yaml, cost_snaps, cash=1000.0, grid=[{}], costs=costs)
```

Per-symbol / per-interval overrides use the same shape as the CLI:

```python
costs = ta.TradingCostsConfig({
    "commission": {
        "default": {"percentage": {"rate": 0.001}},
        "by_symbol": {"BTC": {"percentage": {"rate": 0.0005}}},
    }
})
```

`costs=` accepts either a `TradingCostsConfig` or a raw dict on `ta.optimize(...)`.
For `.run(wallet, snapshots)` and `.evaluate(wallet, snapshots)`, costs come from
what's pre-installed on the wallet — install them per symbol with
`set_costs_for`, before driving:

```python
wallet = ta.PaperWallet(10_000.0)
wallet.set_costs_for("BTC", {"commission": {"percentage": {"rate": 0.001}}})

wallet.update("BTC", 100.0)
wallet.set_position("BTC", 1.0)
filled = wallet.update("BTC", 100.0)
filled[0].commission          # 0.1 — what that fill actually paid
```

Resolution honours the config's `by_symbol` / `by_interval` scoping, so the same
config object can be installed on every leg and still give each its own bundle.
For a whole universe, `wallet.set_costs_for_all(["BTC", "ETH", "SOL"], config)` is
that same call in a loop — each symbol resolved separately, which is the point:
one pre-resolved bundle shared across symbols would have to pick a placeholder to
resolve against and would quietly take the `default:` leg for all of them.
Pass `freq="1d"` (or a `Frequency`) as the third argument for cadence-dependent
models such as funding rates; omit it otherwise. **A wallet with no costs
installed is frictionless**, which flatters every backtest run through it.

## Metrics

`fugazi.metrics` is the standalone reporting surface — one function per metric
so you pick only what you need. Return moments (`mean_return`, `stddev_return`,
`skewness`, `value_at_risk`, …), risk-adjusted ratios (`sharpe`, `sortino`,
`calmar`, `omega`, `ulcer_performance_index`), drawdown analytics
(`max_drawdown`, `average_drawdown`, `time_in_drawdown_ratio`,
`recovery_factor`), and round-trip trade statistics (`win_rate`,
`profit_factor`, `expectancy`, `kelly_fraction`, `average_bars_held`, …) are all
there. Values are in **natural units** — `0.15` is +15%, not `15.0` — and
ratios that can vanish (zero variance for Sharpe, no losing trade for a profit
factor, non-positive endpoints for CAGR) return `None` rather than `NaN`.

Three intermediate builders — `per_bar_returns`, `reconstruct_trades`,
`drawdown_segments` — turn the equity curve and fill blotter into what the
metric functions consume, so a caller computing several metrics builds each
intermediate once:

```python
from fugazi import metrics

equity = [10_000.0, 10_050.0, 10_100.0, 9_900.0, 10_200.0, 10_300.0]
returns = metrics.per_bar_returns(equity, initial_equity=10_000.0)

metrics.sharpe(returns, risk_free_rate=0.0, bars_per_year=252)   # ratio | None
metrics.total_return(equity, initial_equity=10_000.0)            # 0.03
metrics.max_drawdown(metrics.drawdown_segments(equity))          # fraction
```

`reconstruct_trades` walks a bar-tagged fill blotter with a signed position and
a volume-weighted entry, producing one `Trade` per closed leg. Since
`PaperWallet.update()` returns bare `Order`s (no bar), tag each with the bar
you're on using `fugazi.Fill(bar, order)` as you drive the loop:

```python
from fugazi import metrics

fills = []
wallet = ta.PaperWallet(10_000.0)
wallet.update("AAPL", candles[0])          # prime with a price for pre-flight
wallet.set_position("AAPL", 100.0)         # queued market buy
for i, c in enumerate(candles):
    for order in wallet.update("AAPL", c):
        fills.append(ta.Fill(bar=i, order=order))

trades = metrics.reconstruct_trades(fills)
metrics.win_rate(trades)                   # win fraction | None
metrics.profit_factor(trades)              # Σwins / |Σlosses| | None
metrics.exposure_ratio(fills, total_bars=len(candles))
```

### Measuring fills and curves this process didn't produce

`Order`, `Fill` and `RunReport` are plain data, so nothing requires the fills to
have come out of a live wallet loop in the same process. A blotter you *stored*
— a Parquet file, a database, a resumed run — goes straight back in:

```python
from fugazi import metrics

# rows as you persisted them: (bar, side, units, price)
rows = [(0, "buy", 1.0, 100.0), (5, "sell", 1.0, 110.0)]
fills = [
    ta.Fill(bar=bar, order=ta.Order(symbol="BTC", side=side, units=u, price=p))
    for bar, side, u, p in rows
]
trades = metrics.reconstruct_trades(fills)   # -> one closed round trip
```

`Order`'s remaining fields are optional: `kind` (`"market"` / `"stop"` /
`"take_profit"` / `"limit"`) defaults to `"market"`, and `id` / `commission`
to `0` / `0.0`.

Likewise a bare equity curve reduces to the **whole** metric tree — the same
nested dict, under the same dotted key names `evaluate()` produces — without
running anything:

```python
curve = [10_050.0, 10_100.0, 9_900.0, 10_200.0, 10_300.0]
report = ta.RunReport(equity_curve=curve, initial_equity=10_000.0)

m = ta.evaluate_report(report, bars_per_year=252.0)
m["risk_adjusted"]["sharpe"]
m["drawdown"]["max_pct"]
m["returns"]["cagr_pct"]
```

That is the entry point for a curve no `run()` in this process produced: a live
account's accrued equity, a resumed run, an externally-computed series. Pass
`fills=` as well to populate the `trades.*` section — without them a hand-built
report reads there as a run that never traded. `rejections` is always empty on a
hand-built report (a rejection carries a wallet error, which only a wallet can
raise), and the `costs.*` section is absent either way: it is a property of the
wallet that executed the run, not of the report.

> **Metrics assume a closed system.** Every function above reads the equity
> curve as pure P&L. A deposit is indistinguishable from a gain in a curve, and
> a withdrawal from a loss, so an account that takes external cash flows must
> have them neutralized — chain-linked, `r_i = (E_i - F_i) / E_{i-1} - 1` —
> before measuring. See *Cross-cutting caveats* in
> [METRICS.md](https://github.com/acpuchades/fugazi/blob/main/docs/METRICS.md).

## Fetching data

Four remote candle providers ship built in — `Binance`, `Okx`, and `Coinbase`
(crypto spot klines) and `Yahoo` (stocks, ETFs, indices, FX). Each is a client
class with one method, `fetch(...)`, returning a `polars`/`pandas` `DataFrame`
(or a `dict` of lists with `output="numpy"`):

```python
import fugazi as ta

binance = ta.Binance()                     # public endpoint, defaults
df = binance.fetch(symbol="BTCUSDT", freq="1d",
                   since="2020-01-01", until="today")

okx = ta.Okx()                             # symbols are dash-separated
df = okx.fetch(symbol="BTC-USDT", freq="1d", since="2020-01-01")

coinbase = ta.Coinbase()                   # dash-separated product ids
df = coinbase.fetch(symbol="BTC-USD", freq="1d", since="2020-01-01")

yahoo = ta.Yahoo()
df = yahoo.fetch(symbol="AAPL", freq="1d", since="2020-01-01")
```

`freq` is a bar-cadence token (`"1m"`/`"5m"`/`"1h"`/`"4h"`/`"1d"`/`"1w"`/`"1M"`);
`since`/`until` accept ISO (`"YYYY-MM-DD"`), EU (`"D-M-YYYY"`), or relative
(`"today"`, `"yesterday"`, `"Nd ago"`, `"Nw ago"`) dates, `until` is exclusive
and defaults to now. The returned frame has `time` (ISO 8601 UTC), `open`,
`high`, `low`, `close`, `volume`, and — carried through from each provider's
own API — Binance's `quote_volume`, `n_trades`, `taker_buy_base_volume`,
`taker_buy_quote_volume`; OKX's `vol_ccy` and `quote_volume` (its
day/week/month bars are UTC-aligned); Kraken's `vwap` and `n_trades`. Kraken
serves a fixed cadence set (`1m`/`5m`/`15m`/`30m`/`1h`/`4h`/`1d`/`1w`, plus a
15-day bar) and **reaches back at most 720 bars** — its API truncates `since`
from the front rather than paging backward, so ~2 years is the ceiling at
`"1d"` and 30 days at `"1h"`, whatever `since` asks for. Compare the frame's
first `time` against the `since` you passed if that distinction matters.
Coinbase carries no extras — OHLCV only,
and only the fixed cadences `1m`/`5m`/`15m`/`30m`/`1h`/`2h`/`6h`/`1d`. Yahoo candles are **split/dividend-adjusted by
default** (`ta.Yahoo(adjusted=False)` to opt out): `close` is the adjusted
price and the extra column is `raw_close` (the untouched close), or with
`adjusted=False` the OHLCV are raw and the extra is `adj_close`.

`fugazi.fetch(provider=..., symbol=..., ...)` is the provider-generic form of
the same call — handy when the provider name is itself a variable:

```python
df = ta.fetch(provider="yfinance", symbol="AAPL", freq="1d", since="2020-01-01")
```

`fugazi.tickers(provider)` is its counterpart, over the same provider ids: every
symbol that provider exposes, sorted. Every provider class answers the same
question as `.tickers()`, so a caller holding a client need not know which class
it has:

```python
ta.tickers("binance")[:3]                  # ['1000CATUSDT', '1000CHEEMSUSDT', ...]
ta.Okx().tickers()[:3]                     # ['ADA-USDT', 'AGLD-USDT', ...]
```

Worth reaching for because the same instrument is spelled differently at every
venue — `BTCUSDT` on Binance, `BTC-USDT` on OKX, `BTC-USD` on Coinbase,
`XBTUSD` on Kraken, `bitcoin` on CoinGecko — and a wrong spelling is not an error: it fetches an
empty series. `"yfinance"` is the one id that raises (`FetchError`) rather than
answering; Yahoo publishes no endpoint that enumerates its universe, as most
retail equity APIs do not.

### Overlay data (no OHLCV)

Every provider fetches through the same `.fetch(...)` method, but `CoinGecko`
returns a different *shape* of frame: data that is a property of an asset at a
point in time — market capitalisation, traded volume, supply — rather than a
price bar. It carries no price, so the frame has **no
`open`/`high`/`low`/`close`** (the OHLCV block is omitted whenever no row carries
a bar):

```python
cg = ta.CoinGecko()                        # public endpoint; COINGECKO_API_KEY if set
caps = cg.fetch(symbol="bitcoin", freq="1d", since="30d ago")
# columns: time, price, market_cap, total_volume, circulating_supply
```

`symbol` is a CoinGecko **coin id** (`"bitcoin"`, not `"BTC"` and not `"BTCUSDT"`);
`cg.tickers()` lists the vocabulary (they are slugs rather than exchange
tickers, but the method is spelled the same on every provider). `circulating_supply` is derived as
`market_cap / price`. To use these alongside prices, join the two frames on
`time` — market cap and supply are not derivable from OHLCV at all, which is the
whole reason the provider exists.

Two limits of the public tier: it serves only the **last 365 days** (a wider
`since` raises `ValueError`), and sub-hourly frequencies are rejected, because
CoinGecko only samples that finely over windows too short to backtest on. The
provider-generic `ta.fetch(provider="cg", ...)` works too — it returns the same
price-less frame.

`BinanceVision` is a different shape — a **candle** provider, reading Binance's
public historical archive at `data.binance.vision`. It returns an ordinary OHLCV
frame, deeper and cheaper than the live endpoint (one request per month, no rate
limit), at the cost of a ~2-day lag: an archive appears about two days after the
period it covers, so a fetch running to now stops at the last published file.

`market` picks which of the archive's two trees is read:

```python
spot = ta.BinanceVision()                  # market="spot" is the default
bars = spot.fetch(symbol="BTCUSDT", freq="1d", since="90d ago")
# columns: time, open, high, low, close, volume, quote_volume, n_trades,
#          taker_buy_base_volume, taker_buy_quote_volume

perp = ta.BinanceVision(market="futures")
bars = perp.fetch(symbol="BTCUSDT", freq="1d", since="90d ago")
# ... the same columns, plus funding_rate, premium_index, open_interest,
#     open_interest_value and the long/short ratios
```

They are different instruments, not two spellings of one — a perp's funding rate
belongs to the contract it is charged on, and pairing it with a spot bar would
quietly assert the two are the same thing. Spot admits the whole kline
vocabulary (`"1m"` through `"1M"`); futures is `"1h"` through `"1d"`, the range
`premiumIndexKlines` publishes. `symbol` follows `market` — a spot symbol for
`"spot"`, a perpetual contract symbol for `"futures"`. The two mostly coincide
but are not the same list, and `.tickers()` enumerates whichever one that client
reads: `spot.tickers()` and `perp.tickers()` return different vocabularies, read
from the two different live-exchange endpoints (the archive publishes no index).

Unlike CoinGecko's, these columns need no join — they ride alongside the bar.
They do aggregate differently within it, because they are different kinds of
quantity. **Funding is summed**: Binance settles it every 4–8 hours, so
`freq="1d"` is that day's total carry and `freq="8h"` is one settlement per row.
That is right because funding is an accrual rather than a level, and it means
there is nothing to forward-fill — request the cadence you trade. The rest are
levels (the premium index is a basis, open interest is a stock, the ratios are
proportions), so a bar keeps the last sample it saw. A bar may carry some and
not others — at `"1h"` only every eighth bar sees a settlement — and an absent
column reads as an absent sample rather than as a zero.

The flat `ta.fetch` carries both trees as their own provider ids —
`provider="binance-vision"` for spot and `provider="binance-vision-futures"` for
the USD-M tree — matching the CLI. The explicit `ta.BinanceVision(market=...)`
constructor stays for the `base_url` override.

## Performance

An incremental engine is usually the slow choice in Python doubly over: a
vectorised library runs one C loop with no per-sample dispatch *and* no
per-sample trip across the FFI boundary, while fugazi pays both — a Rust
function call per bar, wrapped in a Python call per batch. It turns out not to
cost much, and on four of ten indicators it's outright faster than `talib`.

### Throughput, against `talib`

`talib` — TA-Lib's own Cython bindings — is the fair baseline for a Python
caller, since both sides cross the same kind of boundary. `tools/bench_three_tier.py`
drives TA-Lib's C library, the Rust engine, and the Python bindings from one
input, 200 000 samples, median of 7:

| | TA-Lib C | fugazi (Rust) | `talib` py | fugazi (Python) | **py vs py** |
| --- | ---: | ---: | ---: | ---: | ---: |
| `sma` | 1.39 | 1.40 | 1.47 | 1.72 | 1.17× |
| `ema` | 2.05 | 1.43 | 2.22 | 1.65 | **0.74×** |
| `rsi` | 4.72 | 4.66 | 5.08 | 5.35 | 1.05× |
| `atr` | 4.85 | 4.61 | 12.98 | 6.09 | **0.47×** |
| `stddev` | 3.26 | 11.34 | 3.73 | 12.65 | 3.39× |
| `macd` | 12.95 | 1.57 | 21.31 | 5.81 | **0.27×** |
| `dmi` | 9.58 | 5.87 | 16.65 | 13.04 | **0.78×** |
| `adx` | 14.33 | 9.43 | 21.54 | 24.19 | 1.12× |
| `aroon` | 8.78 | 9.38 | 15.42 | 21.22 | 1.38× |
| `bbands` | 4.04 | 13.99 | 11.22 | 21.23 | 1.89× |

ns/sample. `atr`, `macd`, `dmi` and `ema` beat `talib` outright. `macd`, `dmi`
and `adx`'s Rust column already beats the C library — TA-Lib has no combined
entry point for them and re-derives shared state once per line, where fugazi's
multi-output indicators carry one set of states and emit every line together —
and through the bindings a fugazi `feed` returns that whole multi-output block
as *one* frame from one allocation, where `talib` returns a tuple of
independently-allocated arrays: measured alone, that difference is 10.70
ns/sample against 1.61.

### Where the boundary cost went

Early on, crossing into Python cost far more than the indicator itself — a
`feed()` call copied every input column into a fresh Rust `Vec` (four
1.6&nbsp;MB copies for an OHLCV frame is mostly page faults, not `memcpy`), and
each level of erased indicator wrapping cost ~30 ns/sample making a chain like
`sma(ema(close()))` pay for three levels no Rust caller would. Reading Python's
buffers in place instead of copying them, and folding the whole call through a
128-sample chunk rather than per-`update()` dispatch, cut the common case by
roughly half again on top of the numbers above:

| | ns/sample | vs `talib` |
| --- | ---: | ---: |
| `close()` on a frame | 4.38 | — |
| `sma(close())` on a frame | 7.72 | — |
| `atr(14)` on a frame | 14.62 | — |

[Full write-up, including the two mistaken conclusions that got corrected on the
way →](https://github.com/acpuchades/fugazi/blob/main/docs/PERFORMANCE.md)

### The one real loss

`stddev` — and `bbands`, which inherits it — is ~3.4× `talib`, deliberately.
fugazi makes a centred pass over the window instead of the O(1)
`E[X²] − E[X]²` shortcut, which cancels away significant digits. Not a corner
case: on the price series these figures are measured over, `talib.STDDEV`
returns exactly `0.0` for 896 of 4 981 windows — silently reporting *no
dispersion* — where fugazi is accurate to 5.5e-15, and `ZScore` divides by
that number.

### What this doesn't cover

Every figure above is amortised throughput — total time over 200 000 samples,
divided — which is the right measure for a backtest and overstates the cost of
a single live `update()` by roughly an order of magnitude (the Rust-side
latency numbers are in [the root README](https://github.com/acpuchades/fugazi/blob/main/README.md#latency-which-is-a-different-question)).
There's no equivalent Python-side latency benchmark yet; a `feed()` call
amortises the boundary crossing across a batch, and a single `.update(candle)`
in a live loop pays that crossing once per bar with nothing to amortise it
against.

---

## Running in parallel

A grid sweep parallelises inside Rust — `ta.optimize(..., jobs=N)` — and that is
the right tool when the thing you are varying is a parameter.

For anything else, use processes. **Every value type pickles**, so snapshots go
out and reports come back. The worker has to live at module level — that is
`multiprocessing`'s rule, not fugazi's; a child imports it by name:

```text
# workers.py
import fugazi as ta

def run_one(args):
    snaps, doc = args
    return ta.load_spec(doc).run(ta.PaperWallet(10_000.0), snaps)

# main.py
from concurrent.futures import ProcessPoolExecutor
from workers import run_one

with ProcessPoolExecutor() as pool:
    reports = list(pool.map(run_one, [(snaps, doc) for doc in documents]))
```

`Candle`, `Atom`, `Snapshot`, `Schema`, `OverlayInfo`, `Order`, `Fill`, `Size`,
`Frequency`, `Selector`, `RunReport`, `Trade` and `DrawdownSegment` all round-trip,
and a `Snapshot` carrying one symbol at two cadences survives as two entries
rather than collapsing to one.

**Threads work too.** Every way of driving a spec — `run`, `run_resumable`,
`warm_up` — releases the GIL for its duration, so a websocket reader, a heartbeat
or a UI keeps running while a long backfill grinds. Measured at ~87% of a
companion thread's expected wakeups on all three, against ~0% before.

`run` also stays interruptible: Ctrl-C ends it within milliseconds, because the
drive re-attaches every few thousand bars to poll signal handlers. The resumable
pair is chunked by *you* — that's what it is for — so its interrupt point is
between chunks, where you already stand.

Processes are still the better answer for *throughput*, since threads share one
interpreter's CPU for anything Python-side; use threads for **concurrency** —
keeping the rest of a live process responsive.

**Indicators, signals and strategies do not pickle**, by design: they own live
incremental state, and half a warmed-up EMA is not a thing to ship between
processes. Send the *description* (a YAML document, or the parameters) and build
the chain inside the worker, as above.

## Types

The wheel ships **`py.typed`** and generated stubs, so `mypy` and `pyright` see
the whole surface — parameter names, defaults, which arguments are keyword-only,
and what each call returns.

```python
fast: ta.Indicator = ta.ema(ta.close(), 12)
entries: ta.Signal = fast > ta.ema(ta.close(), 26)     # `>` yields a Signal
wallet: ta.Wallet = ta.PaperWallet(10_000.0)           # any of the three
report: ta.RunReport = ta.Strategy("BTC").long_on(entries).run(wallet, df)
curve: list[float] = report.equity_curve
```

...and the mistakes get caught where you make them:

```text
ta.ema(ta.close(), "twelve")   # Argument 2 to "ema" has incompatible type "str"
ta.optimize(doc, snaps, 1.0)   # Too many positional arguments for "optimize"
x: int = ta.sma(src, 3).value()  # expression has type "float | None"
```

`ta.Wallet` is a `Protocol` to a type checker and an `abc` with the three
concrete wallets registered at run time, so both `w: ta.Wallet` and
`isinstance(w, ta.Wallet)` do what you'd expect.

The stubs are generated from the built module by `tools/gen_python_stubs.py`, so
signatures cannot drift from it; `python/tests/test_stubs.py` regenerates and
diffs on every test run, and type-checks the result. A new binding the generator
can't classify fails that generation rather than shipping as `Any`.

## Errors

Everything fugazi refuses raises a subclass of **`ValueError`**, so an existing
`except ValueError` keeps catching exactly what it caught before. The hierarchy
only adds resolution:

```text
ValueError
└── fugazi.FugaziError
    ├── fugazi.SpecError     — a document that won't load or build
    ├── fugazi.WalletError   — an order the account refused
    └── fugazi.FetchError    — a provider that wouldn't serve the request
```

The distinction that matters in a live loop: a `SpecError` is a property of your
strategy and will fail identically on the next bar; a `WalletError` is a property
of the account *right now* and may well succeed on the next one.

```python
spec = ta.load_spec("!buy_and_hold { root: BTC }")
snaps = [ta.Snapshot({"BTC": c}) for c in stream]

try:
    report = spec.run(ta.PaperWallet(10_000.0), snaps)
except ta.SpecError:
    raise                              # the document is wrong — `at:` says where
except ta.WalletError as e:
    print("skipped this bar:", e)      # margin, spot-short, a venue hiccup
except ta.FetchError as e:
    print("retry later:", e)
```

`SpecError` messages carry the spec layer's `!tag > ` breadcrumb on an `at:` line,
so a failure four levels down names its path.

**`TypeError` is not in this tree.** Passing a `Candle` where a `float` belongs is
an ordinary Python call error, and rehoming it under `FugaziError` would make a
broad `except` swallow real bugs. Argument validation fugazi can answer on its own
— `period must be greater than 0`, a malformed `since=` — stays a bare `ValueError`
for the same reason: it isn't a spec error just because a spec might contain it.

## Documentation

| | |
| --- | --- |
| [docs/PYTHON.md](https://github.com/acpuchades/fugazi/blob/main/docs/PYTHON.md) | The Python API, in full |
| [docs/STRATEGIES.md](https://github.com/acpuchades/fugazi/blob/main/docs/STRATEGIES.md) | The strategy-file format — every YAML tag, all five document shapes |
| [docs/METRICS.md](https://github.com/acpuchades/fugazi/blob/main/docs/METRICS.md) | What each metric means and how it's computed |
| [docs/COSTS.md](https://github.com/acpuchades/fugazi/blob/main/docs/COSTS.md) | Commission, spread and slippage models |
| [docs/TRADING.md](https://github.com/acpuchades/fugazi/blob/main/docs/TRADING.md) | The execution path — bar → order → fill → closed trade |
| [docs/PERFORMANCE.md](https://github.com/acpuchades/fugazi/blob/main/docs/PERFORMANCE.md) | How the numbers above were measured, and the mistakes made getting them |
| [The Rust README](https://github.com/acpuchades/fugazi/blob/main/README.md) | The same engine from the other side |

## Sponsor

fugazi is MIT-licensed, developed in the open, and stays that way. Sponsorship buys
**position in the queue** — never access, never a feature someone else can't have.

Most of what people ask for next is bounded work with a known shape: another venue
wallet, another data provider, a metric, a sixth document shape. Issues tagged
[`sponsorable`](https://github.com/acpuchades/fugazi/issues?q=is%3Aissue+is%3Aopen+label%3Asponsorable)
carry that scope written out — funding one moves it to the front, and it ships under
MIT like the rest.

| Tier | For |
| --- | --- |
| **Individual** | It saved you a weekend and you'd like it to keep being maintained. |
| **Commercial** | You run fugazi in production. Named here, and issues you file get triaged first. |
| **Funded work** | One `sponsorable` issue, scoped and scheduled with you. |

[**Sponsor fugazi →**](https://github.com/sponsors/acpuchades)

## License

MIT — see [LICENSE](https://github.com/acpuchades/fugazi/blob/main/LICENSE).
