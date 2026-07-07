# fugazi (Python)

Python bindings for [`fugazi`](..), a library of **incremental**,
**composable** technical-analysis primitives.

- **Incremental** — every indicator and signal carries its own state and is
  advanced one sample at a time with `update()`, in ~O(1) and with no
  full-history recomputation. The same object serves live streaming and batch
  backtesting.
- **Composable** — indicators own their input source, so you build complex
  indicators and signals by **nesting constructors**. There is no pipe or glue
  step: an "EMA of an SMA of the close" is literally `ta.ema(ta.sma(ta.close(),
  10), 20)`, and a trade condition is a single object you can feed bars.

## Install

```sh
pip install fugazi
```

Then `import fugazi`. Prebuilt wheels are published for Linux, macOS
(Intel + Apple Silicon) and Windows.

To build from a checkout instead (for development):

```sh
pip install maturin
maturin develop --release   # editable install into the active virtualenv
```

## Quick start

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
ta.vwap().feed(df)                              # uses high/low/close/volume
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

## Indicators

| Constructor | Output |
| --- | --- |
| `open() high() low() close() volume() typical() median()` | the candle field |
| `identity()` | the raw value stream (root for a bare numeric series) |
| `value(x)` | a constant |
| `sma ema rma wma hma rsi stddev stochastic cci (source, period)` | a value |
| `stoch_rsi(source, rsi_period=14, stoch_period=14)` | a value |
| `atr mfi williams_r (period)` | a value |
| `obv() vwap() ad() true_range()` | a value |
| `sar(step=0.02, max=0.2)` | a value |
| `macd(source, fast=12, slow=26, signal=9)` | dict `{macd, signal, histogram}` |
| `bollinger(source, period=20, k=2.0)` | dict `{upper, middle, lower}` |
| `keltner(source, ema_period=20, atr_period=10, multiplier=2.0)` | dict `{upper, middle, lower}` |
| `donchian(high, low, period)` | dict `{upper, middle, lower}` |
| `adx(period)` | dict `{plus_di, minus_di, adx}` |
| `dmi(period)` | dict `{plus_di, minus_di}` |
| `aroon(period)` | dict `{up, down, oscillator}` |
| `resample(every, inner)` | `inner`'s output every `every` bars (aggregated HTF candle fed to `inner`), `None` between |
| `latch(source)` | `source`'s last `Some` output, held across `None` ticks (works on indicators and signals) |
| `unstable(x)` | Passthrough that reports `unstable_period() = 0` for its subtree (also `.unstable()` on any Indicator or Signal) |

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
`unstable_period() = 0`, telling a downstream reader of `stable_period()`
(a strategy-readiness gate, an overlay trim) "trade through this subtree's
IIR settling tail". Available as a free function and as a method on any
Indicator or Signal — same output, same warm-up, only the reported unstable
tail changes:

```python
raw = ta.ema(ta.close(), 20)
fast = raw.unstable()           # method form; unstable_period() -> 0
fast = ta.unstable(raw)         # equivalent free-function form
```

Safe by default, override per subtree: fugazi's readiness machinery waits for
`stable_period()` by default (`SingleAssetStrategy::is_ready` in Rust; the
CLI's per-overlay CSV trim in `fugazi get`) — `unstable(...)` is the single
opt-out.

## Operators

Combine value indicators into **other indicators**:

```python
ta.close().add(other)        # also: sub, mul, div  — or the + - * / operators
ta.close().lag(1)            # also: diff, ratio, roc
ta.close().rolling_max(20)   # also: rolling_min
```

...or into **signals** (booleans):

```python
fast.gt(slow)                        # also: lt, ge, le, eq, ne  (optional epsilon=...)
ta.rsi(ta.close(), 14).above(70.0)   # also: below(level)
fast.crosses_above(slow)             # also: crosses_below
```

Signals compose with each other and update to a `bool`:

```python
sig = a.and_(b)     # also: or_, xor_, not_(), changed()  — or  a & b | ~c
sig.update(candle)  # -> bool
```

## Example

"Fast EMA crosses above slow EMA while RSI is not already overbought" — one
signal, usable either way:

```python
import fugazi as ta

def golden():
    return (
        ta.ema(ta.close(), 12)
          .crosses_above(ta.ema(ta.close(), 26))
          .and_(ta.rsi(ta.close(), 14).below(70.0))
    )

# streaming: react bar by bar
signal = golden()
for bar in stream:
    if signal.update(bar):
        print("entry signal")

# batch: a boolean Series/array over the whole frame
entries = golden().feed(df)
```

## Trading: the wallet

The strategy layer is exposed as a **wallet** you trade into. There is no
strategy class to subclass — a "strategy" in Python is just your own code that,
each bar, reads signals and calls wallet methods. `PaperWallet` is the built-in,
in-memory book (funds + positions + a trade blotter); live execution belongs in
your own code, not here.

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
wallet.position("AAPL")      # signed position (negative = short)
wallet.price("AAPL")         # last fed price (or None)
wallet.positions()           # {symbol: units}
wallet.equity()              # funds + positions marked at the fed prices
wallet.orders()              # the blotter: list of Order(symbol, side, units)
```

The wallet is fed each symbol's price with `update(symbol, price)` and is
otherwise market-agnostic. Sizes are an absolute number of units, or
`ta.Size.funds_frac(f)` (cash) / `ta.Size.value_frac(f)` (equity; `1.0` is
all-in) / `ta.Size.position_frac(f)`; sides are `"buy"`/`"sell"`. A movement that
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
        wallet.set("AAPL", "buy", ta.Size.value_frac(1.0))   # all-in long
    elif went_flat:
        wallet.close("AAPL")
```

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
wallet.set_position("AAPL", 100.0)         # queued market buy
for i, c in enumerate(candles):
    for order in wallet.update("AAPL", c):
        fills.append(ta.Fill(bar=i, order=order))

trades = metrics.reconstruct_trades(fills)
metrics.win_rate(trades)                   # win fraction | None
metrics.profit_factor(trades)              # Σwins / |Σlosses| | None
metrics.exposure_ratio(fills, total_bars=len(candles))
```

## Fetching data

Two remote candle providers ship built in — `Binance` (crypto spot klines) and
`Yahoo` (stocks, ETFs, indices, FX). Each is a client class with one method,
`candles(...)`, returning a `polars`/`pandas` `DataFrame` (or a `dict` of lists
with `output="numpy"`):

```python
import fugazi as ta

binance = ta.Binance()                     # public endpoint, defaults
df = binance.candles(symbol="BTCUSDT", freq="1d",
                     since="2020-01-01", until="today")

yahoo = ta.Yahoo()
df = yahoo.candles(symbol="AAPL", freq="1d", since="2020-01-01")
```

`freq` is a bar-cadence token (`"1m"`/`"5m"`/`"1h"`/`"4h"`/`"1d"`/`"1w"`/`"1M"`);
`since`/`until` accept ISO (`"YYYY-MM-DD"`), EU (`"D-M-YYYY"`), or relative
(`"today"`, `"yesterday"`, `"Nd ago"`, `"Nw ago"`) dates, `until` is exclusive
and defaults to now. The returned frame has `time` (ISO 8601 UTC), `open`,
`high`, `low`, `close`, `volume`, and — carried through from each provider's
own API — Binance's `quote_volume`, `n_trades`, `taker_buy_base_volume`,
`taker_buy_quote_volume`; Yahoo's `adj_close` (split- and dividend-adjusted).

`fugazi.fetch(provider=..., symbol=..., ...)` is the provider-generic form of
the same call — handy when the provider name is itself a variable:

```python
df = ta.fetch(provider="yfinance", symbol="AAPL", freq="1d", since="2020-01-01")
```
