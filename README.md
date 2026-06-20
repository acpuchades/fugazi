# arcana

A Rust library of **incremental** technical-analysis primitives. Every indicator
and signal owns its internal state and is advanced one sample at a time via
`update()`, carrying just enough intermediate state to produce the next output in
~O(1). The same code works for live streaming and for batch backtesting.

- **Incremental** — feed one bar at a time; no full-history recomputation.
- **Composable** — indicators own their input source, so you build complex
  signals by nesting constructors. No glue, no remembering what to feed where.
- **Zero dependencies**, `edition = "2024"`.

## Install

```toml
[dependencies]
arcana = "0.1"
```

## Concepts

The crate has two composable layers:

- **Indicators** are the numeric *sources*. Each produces a `Real` (`f64`) and
  **owns its own input source**, so composition is just nesting constructors:
  `Ema::new(Current::close(), 20)` is the EMA‑20 of the close. Leaf sources
  terminate the chain — `Identity` (raw value stream), `Value` (a constant), and
  the candle accessors under `Current` (`Current::close()`, `Current::volume()`,
  …). Bar indicators (`Atr`, `Adx`, `TrueRange`) consume a whole `Candle`.
- **Signals** are composable booleans. Comparisons are built from two sources, so
  a condition like "RSI over 70" is a single object. Combine signals with
  `and`/`or`/`xor`/`not`/`changed`.

Both share the same shape: state lives inside, `update(input)` advances one step,
outputs are `None` until warmed up.

## Quick start

"Current close crosses above its EMA‑20" — defined once, fed one `Candle` per bar:

```rust
use arcana::prelude::*;
use arcana::indicators::{Current, Ema};

let mut signal = Current::close().crosses_above(Ema::new(Current::close(), 20));

# let feed: Vec<Candle> = Vec::new();
for candle in feed {
    if signal.update(candle) {
        // entry trigger fires on the bar the close crosses above EMA-20
    }
}
```

Indicators name their source explicitly, so the standard definitions read the
way you'd expect — RSI of the close, fed one `Candle` per bar:

```rust
use arcana::prelude::*;
use arcana::indicators::{Current, Rsi};

// "RSI(14) of the close, over 70" as a single Signal<Input = Candle>.
let mut overbought = Rsi::new(Current::close(), 14).above(70.0);

# let feed: Vec<Candle> = Vec::new();
for candle in feed {
    if overbought.update(candle) { /* ... */ }
}
```

Working on a bare `f64` price stream instead of candles? `Identity` is the leaf
that passes raw values straight through, so the same indicator consumes `Real`
directly — `Rsi::new(Identity::new(), 14)`.

## Composition

Indicators nest — composition *is* construction:

```rust
use arcana::indicators::{Current, Ema, Sma};

let _ema_of_sma = Ema::new(Sma::new(Current::close(), 10), 20); // EMA of an SMA
```

The `IndicatorExt` fluent builders turn sources into other sources and signals:

```rust
use arcana::prelude::*;
use arcana::indicators::{Current, Ema};

// arithmetic over two sources, and lookback ops (lag / diff / ratio)
let _spread   = Ema::new(Current::close(), 10).sub(Ema::new(Current::close(), 30));
let _momentum = Current::close().diff(1);          // x[t] - x[t-1]
let _change   = Current::close().ratio(1);         // x[t] / x[t-1]

// comparisons (tolerance-aware) -> signals
let _above = Current::close().gt(Ema::new(Current::close(), 50));
let _cross = Ema::new(Current::close(), 10).crosses_above(Ema::new(Current::close(), 30));
```

The `SignalExt` combinators compose signals:

```rust
use arcana::prelude::*;
use arcana::indicators::{Current, Ema, Rsi};

let _entry = Current::close()
    .crosses_above(Ema::new(Current::close(), 20))
    .and(Rsi::new(Current::close(), 14).below(70.0));
```

A *crossover* is not a special type — it is "the comparison is true **and** it
just changed", i.e. `a.gt(b).and(a.gt(b).changed())`, which `crosses_above`
builds for you. `changed()` is the single edge primitive (it fires on any
toggle).

## What's included

- **Moving averages / smoothing:** `Sma`, `Ema`, `Rma` (Wilder/SMMA), `Wma`,
  `Hma` (Hull)
- **Oscillators / momentum:** `Rsi`, `Macd`, `Stochastic` / `StochRsi`,
  `WilliamsR`, `Cci`, `Roc`, `StdDev`
- **Trend / volatility:** `Atr`, `Adx`, `Dmi` (+DI/−DI), `Aroon`, `Bollinger`,
  `Donchian`, `Keltner`, `Sar` (Parabolic SAR)
- **Volume:** `Obv`, `Vwap`, `Ad` (Chaikin A/D), `Mfi`
- **Sources & transforms:** `Identity`, `Value`, `Current::*` candle accessors,
  `TrueRange`; `Add`/`Sub`/`Mul`/`Div`, `Lag`/`Diff`/`Ratio`/`Roc`,
  `RollingMax`/`RollingMin`
- **Signals:** `Gt`/`Lt`/`Ge`/`Le`/`Eq`/`Ne` comparisons, `and`/`or`/`xor`/`not`,
  `changed`, `crosses_above`/`crosses_below`

Multi-line indicators expose their components as fields and a value struct:
`Bollinger`/`Donchian`/`Keltner` → `upper`/`middle`/`lower`, `Macd` →
`macd`/`signal`/`histogram`, `Adx` → `plus_di`/`minus_di`/`adx`, `Dmi` →
`plus_di`/`minus_di`, `Aroon` → `up`/`down`/`oscillator`.

Comparisons are tolerance-aware (default `1e-8`, overridable via
`Gt::with_epsilon(..)`) so floating-point noise doesn't cause spurious flips.

## Examples

Runnable example programs live in [`examples/`](examples) — run any with
`cargo run --example <name>`:

- `streaming` — an indicator over a bare `f64` price feed (`Identity` source),
  handling the `Option` warm-up.
- `candle_signal` — a compound entry rule (EMA crossover gated by an RSI filter)
  as one object, fed one `Candle` per bar.
- `multi_output` — reading multi-line indicators two ways: the `BollingerValue`
  struct and `Macd`'s per-component public fields.
- `backtest` — a batch backtest over bundled monthly AAPL data: an SMA crossover
  driving a long/flat equity curve versus buy-and-hold.

A `cargo test` checks that every example still compiles.

## Python

Python bindings are available in [`python/`](python). Same model — compose by
nesting constructors, then either feed one `Candle` at a time or compute a whole
series in one shot with `feed(df)`. The output mirrors the input (pandas in →
pandas out, polars in → polars out, else a NumPy array):

```python
import arcana as ta

# streaming
signal = ta.close().crosses_above(ta.ema(ta.close(), 20))
for o, h, l, c, v in bars:
    if signal.update(ta.Candle(o, h, l, c, v)):
        ...  # entry trigger

# batch over a DataFrame (pandas or polars)
df["ema20"] = ta.ema(ta.close(), 20).feed(df)
```

Build with `cd python && maturin develop --release`. See the
[Python README](python/README.md) for the full API.

## License

MIT — see [LICENSE](LICENSE).
