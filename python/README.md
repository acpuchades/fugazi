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

## Indicators

| Constructor | Output |
| --- | --- |
| `open() high() low() close() volume() typical() median()` | the candle field |
| `identity()` | the raw value stream (root for a bare numeric series) |
| `value(x)` | a constant |
| `sma ema rma wma hma rsi stddev stochastic cci (source, period)` | a value |
| `skewness kurtosis zscore (source, period)` | a value (distribution shape / normalization; `kurtosis` is raw, ~3 for normal) |
| `correlation(lhs, rhs, period)` | rolling Pearson correlation in `[-1, 1]` (autocorrelation: `correlation(x, x.lag(n), period)`) |
| `percentile(source, period, pct)` | the `pct`-quantile over the window (`pct=0.5` is the rolling median), linearly interpolated like numpy's default |
| `percentile_rank(source, period)` | where the current reading sits in its own window: `count(v <= x)/period`, in `(0, 1]` |
| `get(schema, key, source=None)` | the overlay column, typed by its declaration (real→Indicator, bool→Signal, str→StrSource); `source=pick(sym)` reads another series' column |
| `bars_since(signal)` | bars since `signal` was last true (`0` on the firing bar); `None` until it has fired once, so thresholds read false until then |
| `bars_since_high bars_since_low (source, period)` | bars since the source set a new `period`-bar extreme, in `[0, period-1]` |
| `variance_ratio(source, period, lag)` | Lo-MacKinlay regime classifier (`>1` trending, `<1` mean-reverting); O(period)/bar recompute |
| `stoch_rsi(source, rsi_period=14, stoch_period=14)` | a value |
| `atr mfi williams_r vwap (period)` | a value |
| `parkinson garman_klass rogers_satchell (period)` | range-based volatility estimate (uses the full candle; more efficient than close-to-close stddev) |
| `obv() ad() true_range()` | a value |
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
| `unstable(x)` | Passthrough that reports `unstable_bars() = 0` for its subtree (also `.unstable()` on any Indicator or Signal) |
| `every(period)` | Signal: a pulse every `period` bars, first fire delayed to bar `period-1` — the usual [`rebalance_on`](#the-declarative-strategy-builder) gate |

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

Snapshot keys are **Selectors** — a `(symbol?, freq?)` pair. A `Selector`
matches structurally: a `None` field on the query wildcards the corresponding
storage field, so `pick(symbol="BTC")` finds every BTC entry regardless of
frequency. A bare Python `str` is coerced to `Selector.by_symbol(...)`, a
`(str, Frequency|str)` tuple to a full `(symbol, freq)` pair, so most call
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
runs `Snapshot.sole_atom` on every bar: the snapshot must contain exactly one
entry (its atom is what the pick emits), otherwise the call **panics loudly**
(a Python `RuntimeError` translated from the Rust panic). That's the
"strategy authored for one asset but fed a `Snapshot`-shaped driver" case —
the loud failure catches multi-asset input that would otherwise silently pick
whichever entry the HashMap iterator happened to hand back.

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

## Operators

Combine value indicators into **other indicators**:

```python
ta.close().add(other)        # also: sub, mul, div  — or the + - * / operators
ta.close().lag(1)            # also: diff, ratio, roc
ta.close().rolling_max(20)   # also: rolling_min
```

...or into **signals** (booleans):

```python
fast.gt(slow)                        # also: lt, ge, le, eq, ne
fast.gt(slow, epsilon=0.5)           # absolute deadband; omit for the scale-aware default
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

The strategy layer is exposed two ways. For the classic single-asset shape
there's a declarative **`Strategy`** builder you `run` over a wallet (below);
for anything else, the **wallet** is a market-agnostic venue you trade into with
your own per-bar Python — no class to subclass. `PaperWallet` is the built-in,
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
wallet.equity                # funds + positions marked at the fed prices
wallet.position("AAPL")      # signed position (negative = short)
wallet.price("AAPL")         # last fed price (or None)
wallet.positions()           # {symbol: units}
wallet.orders()              # the blotter: list of Order(symbol, side, units)
wallet.can_short             # can this account hold a negative position?
wallet.quote_ccy             # what currency are these numbers in? (or None)
```

`can_short` is what an account *can* do, asked before trading: `True` on a
`PaperWallet` (a sell credits cash) and on `OkxWallet` (net-mode swaps), `False`
on the spot `CoinbaseWallet`, whose positions are owned base-asset balances. It
informs rather than enforces — a spot wallet still clamps a short target to flat
on its own — so a long/short strategy can pick a long-only path up front instead
of learning the limit from a clamped order.

`quote_ccy` is the same shape of question about the account's *unit*: `"USDT"` on
`OkxWallet` (the margin currency a linear USDⓈ-M swap settles in), whatever the
`CoinbaseWallet` was built against (`"USD"` by default), and `None` on a
`PaperWallet` unless you pass one — simulated money has no venue to ask:

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

report.equity_curve                    # one marked-to-market value per bar
report.fills                           # list[Fill] — the blotter, in fill order
rets = per_bar_returns(report.equity_curve, report.initial_equity)
sharpe(rets, 0.0, 252.0)
```

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

## YAML strategy specs — `load_spec`, `optimize`, walkforward

The CLI's YAML surface (see the crate root's `strategy.yml` examples) is
available natively from Python. `ta.load_spec(text)` parses a spec
document, auto-detects its shape (single / pairs / basket / multi /
portfolio), and returns a `StrategySpec` that implements the same
`.run(wallet, snapshots)` interface as the manual [`Strategy`](#the-declarative-strategy-builder)
builder. `.evaluate(...)` is a bonus method that runs + reduces to a metrics
dict in one call.

```python
import fugazi as ta

spec = ta.load_spec("""
symbol: BTC
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

spec = ta.load_spec("symbol: BTC\nlong:\n  enter: !crosses_above"
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
spec = ta.load_spec("!buy_and_hold { symbol: BTC }")
```

The five shapes are auto-detected by top-level YAML key:

| Top-level key(s)        | Detected kind |
| ---                     | ---           |
| `children:`             | `portfolio`   |
| `left:` + `right:`      | `pairs`       |
| `selection:`            | `basket`      |
| `symbol:` or preset tag | `single`      |
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
symbol: BTC
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
symbol: BTC
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
symbol: BTC
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
expand to numeric ranges. Pass `windowed=N` to reduce each grid point across
non-overlapping N-bar windows (`row.metrics_windowed` carries the per-window
docs), or `walkforward=(is, oos)` / `walkforward=(is, oos, embargo)` for
walk-forward validation:

```python
wf_yaml = """
symbol: BTC
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

### Costs

Trading costs load from a Python dict matching the CLI's YAML shape
(externally-tagged models: `!percentage`, `!bps`, `!volume_participation`, …):

```python
costs = ta.TradingCostsConfig({
    "commission": {"percentage": {"rate": 0.001}},
    "spread":     {"bps": {"bps": 5}},
})
cost_yaml = "!buy_and_hold { symbol: BTC }"
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
day/week/month bars are UTC-aligned). Coinbase carries no extras — OHLCV only,
and only the fixed cadences `1m`/`5m`/`15m`/`30m`/`1h`/`2h`/`6h`/`1d`. Yahoo candles are **split/dividend-adjusted by
default** (`ta.Yahoo(adjusted=False)` to opt out): `close` is the adjusted
price and the extra column is `raw_close` (the untouched close), or with
`adjusted=False` the OHLCV are raw and the extra is `adj_close`.

`fugazi.fetch(provider=..., symbol=..., ...)` is the provider-generic form of
the same call — handy when the provider name is itself a variable:

```python
df = ta.fetch(provider="yfinance", symbol="AAPL", freq="1d", since="2020-01-01")
```

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
`cg.ids()` lists the vocabulary. `circulating_supply` is derived as
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
`premiumIndexKlines` publishes. `symbol` is a contract symbol, which mostly
coincides with the spot vocabulary but is not the same list; `spot.symbols()`
enumerates it.

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
