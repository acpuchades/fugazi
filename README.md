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

The crate has three composable layers:

- **Indicators** are the numeric *sources*. Each produces a `Real` (`f64`) and
  **owns its own input source**, so composition is just nesting constructors:
  `Ema::new(Current::close(), 20)` is the EMA‑20 of the close. Leaf sources
  terminate the chain — `Identity` (raw value stream), `Value` (a constant), and
  the candle accessors under `Current` (`Current::close()`, `Current::volume()`,
  …). Bar indicators (`Atr`, `Adx`, `TrueRange`) consume a whole `Candle`.
- **Signals** are composable booleans. Comparisons are built from two sources, so
  a condition like "RSI over 70" is a single object. Combine signals with
  `and`/`or`/`xor`/`not`/`changed`.
- **Strategies** are the decision layer. A strategy is *your own type*: each bar
  it reads the input, advances its signals, and opens/closes positions on a
  `Wallet` it's handed. See [Strategies](#strategies).

The first two layers are *pure* value-producers sharing one shape: state lives
inside, `update(input)` advances one step, outputs are `None` until warmed up.

## Quick start

"Current close crosses above its EMA‑20" — defined once, fed one `Candle` per bar:

```rust
use arcana::prelude::*;
use arcana::indicators::{Current, Ema};

let mut signal = Current::close().crosses_above(Ema::new(Current::close(), 20));

# let feed: Vec<Candle> = Vec::new();
for candle in feed {
    signal.update(candle);
    if signal.is_true() {
        // entry trigger fires on the bar the close crosses above EMA-20
    }
}
```

Indicators name their source explicitly, so the standard definitions read the
way you'd expect — RSI of the close, fed one `Candle` per bar:

```rust
use arcana::prelude::*;
use arcana::indicators::{Current, Rsi};

// "RSI(14) of the close, over 70" as a single `Signal` (a `Candle`-fed `bool`).
let mut overbought = Rsi::new(Current::close(), 14).above(70.0);

# let feed: Vec<Candle> = Vec::new();
for candle in feed {
    overbought.update(candle);
    if overbought.is_true() { /* ... */ }
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

The `BoolIndicatorExt` combinators compose signals:

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

Multi-output indicators (`Macd`, `Bollinger`, `Adx`, …) produce a small value
struct, but each output also has a **component accessor** that projects that one
field back into an ordinary `Indicator<Output = Real>` — so a single line of a
multi-output indicator composes and compares exactly like any other source:

```rust
use arcana::prelude::*;
use arcana::indicators::{Bollinger, Current, Macd};

// MACD line crossing its signal line, as one composed Signal:
let macd = Macd::new(Current::close(), 12, 26, 9);
let _macd_cross = macd.line().crosses_above(macd.signal());

// "close pierces the upper Bollinger band":
let bands = Bollinger::new(Current::close(), 20, 2.0);
let _breakout = Current::close().gt(bands.upper());
```

Each accessor clones its source, so the two operands above are independent,
self-contained instances (the same clone-the-operands shape `crosses_above`
already uses) — just feed each the same `Candle` per bar.

## Strategies

The decision layer turns signals into trades. A **strategy** is *your own type*
implementing the `Strategy` trait: each bar it reads the input, advances its
signals, and opens/scales/closes positions on a `Wallet` it is handed. The
wallet — not the strategy — owns the portfolio (funds, positions, a trade
blotter), so the *same* strategy runs against the in-memory `PaperWallet` for
backtests or, because `Wallet` is a trait, a live broker / event-bus
implementation living in a downstream crate.

```rust
use arcana::prelude::*;
use arcana::indicators::{Current, Sma};

// Own your signals; act on the wallet. `update` advances the signals; `trade`
// reads them and acts. `Size` is absolute units or a fraction of funds / equity /
// current position, and `Side` gives direction — so position sizing,
// short-selling, and staying always-in-market are just what the code does.
struct GoldenCross {
    symbol: &'static str,
    enter: Box<dyn Signal>,
    exit: Box<dyn Signal>,
}

impl Strategy for GoldenCross {
    type Input = Candle;
    type Symbol = &'static str;

    fn update(&mut self, candle: Candle) {
        // Advance EVERY signal every bar (don't short-circuit, or a skipped one
        // desyncs from the price stream).
        self.enter.update(candle);
        self.exit.update(candle);
    }

    fn trade(&self, wallet: &mut dyn Wallet<&'static str>) {
        // The wallet is priced from outside; `trade` just reads signals and acts.
        if self.enter.is_true() {
            let _ = wallet.set(self.symbol, Side::Buy, Size::value_frac(1.0));
        } else if self.exit.is_true() {
            let _ = wallet.close(self.symbol);
        }
    }

    fn reset(&mut self) {
        self.enter.reset();
        self.exit.reset();
    }
}

let mut strat = GoldenCross {
    symbol: "AAPL",
    enter: Box::new(Sma::new(Current::close(), 3).crosses_above(Sma::new(Current::close(), 10))),
    exit:  Box::new(Sma::new(Current::close(), 3).crosses_below(Sma::new(Current::close(), 10))),
};
let mut wallet = PaperWallet::new(10_000.0);

# let feed: Vec<Candle> = Vec::new();
for candle in feed {
    wallet.update("AAPL", Reference(candle.close));  // price the wallet from outside
    strat.update(candle);                            // advance signals
    strat.trade(&mut wallet);                        // act
}
let _orders = wallet.orders();        // the trade blotter
```

The wallet is fed each symbol's price every bar via `wallet.update`; `set`
targets an absolute position (an opposite-side `set` reverses, `value_frac(1.0)`
is all-in), and `close` flattens. Queries return unit-tagged amounts
(`Reference` cash/equity, `Quantity` of a symbol). For **multi-asset**
strategies, make `Input` a snapshot of several symbols, feed the wallet each
symbol's price, and act on more than one symbol per `trade` — see the `pairs`
example. The trading/execution/event-bus machinery itself is out of
scope for this crate; it belongs in a downstream project that implements `Wallet`.

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
`plus_di`/`minus_di`, `Aroon` → `up`/`down`/`oscillator`. Each component also has
a same-named **accessor** (`macd.line()`/`.signal()`/`.histogram()`,
`bands.upper()`/`.middle()`/`.lower()`, `adx.adx()`, …) that returns it as a
composable `Indicator<Output = Real>` — so any one line can feed the comparison
and arithmetic builders above (see [Composition](#composition)).

Comparisons are tolerance-aware (default `1e-8`, overridable via
`Gt::with_epsilon(..)`) so floating-point noise doesn't cause spurious flips.

## Examples

Runnable example programs live in [`examples/`](examples) — run any with
`cargo run --example <name>`:

- `streaming` — an indicator over a bare `f64` price feed (`Identity` source),
  handling the `Option` warm-up.
- `candle_signal` — a compound entry rule (EMA crossover gated by an RSI filter)
  as one object, fed one `Candle` per bar.
- `multi_output` — the components of multi-line indicators three ways: the
  `BollingerValue` struct, `Macd`'s per-component public fields, and the
  component accessors composed into signals (`macd.line().crosses_above(..)`).
- `backtest` — a batch backtest over bundled monthly AAPL data: a `GoldenCross`
  strategy trading a `PaperWallet`, long/flat, versus a buy-and-hold benchmark.
- `strategy` — a long/short, always-in-the-market reversal: one strategy type
  using `wallet.set` and funds-fraction sizing.
- `pairs` — a multi-asset strategy: two symbols traded from one wallet, driven by
  a per-symbol snapshot input.

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

The strategy layer is exposed too: a `PaperWallet` you feed prices into
(`update`) and trade with `set`/`set_position`/`close`, plus `Order` and `Size`.
A "strategy" in Python is just your own code driving the wallet each bar:

```python
import arcana as ta

enter = ta.sma(ta.close(), 3).crosses_above(ta.sma(ta.close(), 10))
exit_ = ta.sma(ta.close(), 3).crosses_below(ta.sma(ta.close(), 10))
wallet = ta.PaperWallet(10_000.0)

for o, h, l, c, v in bars:
    candle = ta.Candle(o, h, l, c, v)
    wallet.update("AAPL", c)                                  # price the wallet
    went_long, went_flat = enter.update(candle), exit_.update(candle)  # advance both
    if went_long:
        wallet.set("AAPL", "buy", ta.Size.value_frac(1.0))   # size: units / funds / equity / position
    elif went_flat:
        wallet.close("AAPL")

print(wallet.funds, wallet.position("AAPL"), wallet.orders())
```

Build with `cd python && maturin develop --release`. See the
[Python README](python/README.md) for the full API.

## License

MIT — see [LICENSE](LICENSE).
