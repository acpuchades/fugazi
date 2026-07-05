# fugazi

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
fugazi = "0.11"
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
Every indicator also reports its exact `warm_up_period()` (samples until the
first output, accounting for the whole composed chain) and its
`unstable_period()` — `0` for windowed indicators, and for the recursive ones
(EMA, RSI, ATR, ADX, …) the extra samples until the seeding's influence has
decayed below 0.1%. `stable_period()` is their sum: how much history to feed
before trusting the output. The `Stable` **bool signal** — `.stable()` on any
source *or* signal — reports whether that pipeline is past its
`stable_period()`, so composing an entry as
`entry.and(entry.clone().stable())` (or, in YAML, `!all [<entry>, !stable {
signal: <entry> }]`) prevents any trade from firing on a seed-contaminated
value.

## Quick start

"Current close crosses above its EMA‑20" — defined once, fed one `Candle` per bar:

```rust
use fugazi::prelude::*;
use fugazi::indicators::{Current, Ema};

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
use fugazi::prelude::*;
use fugazi::indicators::{Current, Rsi};

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
use fugazi::indicators::{Current, Ema, Sma};

let _ema_of_sma = Ema::new(Sma::new(Current::close(), 10), 20); // EMA of an SMA
```

The `IndicatorExt` fluent builders turn sources into other sources and signals:

```rust
use fugazi::prelude::*;
use fugazi::indicators::{Current, Ema};

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
use fugazi::prelude::*;
use fugazi::indicators::{Current, Ema, Rsi};

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
use fugazi::prelude::*;
use fugazi::indicators::{Bollinger, Current, Macd};

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

### Cross-timeframe composition

Two primitives compose directly for running an indicator on candles **coarser**
than the base stream — no dedicated wrapper needed. `Resample<S>` buckets
`every` base candles into a single higher-timeframe [`Candle`] (emits `Some`
only on the completing tick, `None` between), and `Latch<S>` re-emits the last
`Some` output on `None` ticks so a per-base-tick consumer sees the finished
higher-timeframe value between boundaries.

```rust
use fugazi::prelude::*;
use fugazi::indicators::{Current, Ema, Latch, Resample};

// "1× base bar's close crosses above an EMA-20 computed on 4-bar candles."
let _sig = Current::close().crosses_above(
    Latch::new(Ema::new(Resample::new(Current::candle(), 4).close(), 20)),
);
```

The **only correct ordering** is Resample → recursive smoother → Latch:
latching *before* an EMA / RSI / ATR would feed it a held (repeated) value on
every base tick, distorting the recurrence. Warm-up and unstable-period pass
through as raw composition arithmetic (higher-timeframe sample counts, not
base-bar scaled) — if a strategy needs base-bar-correct stability accounting,
it must feed the pipeline enough leading history for the recursive tail to
decay in HTF terms.

## Strategies

The decision layer turns signals into trades. A **strategy** is *your own type*
implementing the `Strategy` trait: each bar it reads the input, advances its
signals, and opens/scales/closes positions on a `Wallet` it is handed. The
wallet — not the strategy — owns the portfolio (funds, positions, a trade
blotter), so the *same* strategy runs against the in-memory `PaperWallet` for
backtests or, because `Wallet` is a trait, a live broker / event-bus
implementation living in a downstream crate.

```rust
use fugazi::prelude::*;
use fugazi::indicators::{Current, Sma};

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
    wallet.update("AAPL", candle);  // feed the wallet this bar from outside
    strat.update(candle);           // advance signals
    strat.trade(&mut wallet);       // act
}
let _orders = wallet.orders();        // the trade blotter
```

The wallet is fed each symbol's bar every tick via `wallet.update` (its `close`
marks to market, its `[low, high]` bounds fills); `set` targets an absolute
position (an opposite-side `set` reverses, `value_frac(1.0)` is all-in), and
`close` flattens. Queries return unit-tagged amounts
(`Reference` cash/equity, `Units` of a symbol). For **multi-asset**
strategies, make `Input` a snapshot of several symbols, feed the wallet each
symbol's price, and act on more than one symbol per `trade` — see the `pairs`
example. The trading/execution/event-bus machinery itself is out of
scope for this crate; it belongs in a downstream project that implements `Wallet`.

## Backtest & metrics

The per-bar loop above (update the wallet, `on_fill`, `update` the strategy,
`trade` it, record equity) is what `fugazi::backtest::run` does for you. It
takes any `impl Wallet<Sym>`, so the same primitive drives a `PaperWallet`
backtest or a downstream live-broker impl unchanged — it isn't backtest-only,
hence the neutral `run` name — and returns a `RunReport` with the equity curve
and every booked `Fill`:

```rust
use fugazi::prelude::*;
use fugazi::backtest::run;

# struct MyStrategy;
# impl Strategy for MyStrategy {
#     type Input = Candle;
#     type Symbol = &'static str;
#     fn update(&mut self, _: Candle) {}
#     fn trade(&self, _: &mut dyn Wallet<&'static str>) {}
#     fn reset(&mut self) {}
# }
# let mut strat = MyStrategy;
# let candles: Vec<Candle> = vec![];
let mut wallet = PaperWallet::new(10_000.0);
let report = run(&mut strat, &mut wallet, "AAPL", candles);
// report.equity_curve : Vec<Real>   — one mark-to-market point per bar
// report.fills        : Vec<Fill<_>> — every booked order, bar-indexed
```

The `fugazi::metrics` module then reduces that report to numbers **one function
per metric** — no aggregate `compute`. Three intermediate builders
(`per_bar_returns`, `reconstruct_trades`, `drawdown_segments`) turn the raw
artefacts into the shapes each metric family consumes; the metrics themselves
are the classic catalogue (`sharpe`, `sortino`, `calmar`, `omega`,
`ulcer_index` / `ulcer_performance_index`, `max_drawdown`, `win_rate`,
`profit_factor`, `expectancy`, `value_at_risk` / `conditional_value_at_risk`,
`skewness`, `kurtosis`, …). Call whichever you need:

```rust
use fugazi::backtest::RunReport;
use fugazi::metrics::{per_bar_returns, drawdown_segments, sharpe, max_drawdown};

# let report: RunReport<&'static str> = RunReport {
#     equity_curve: vec![10_000.0, 10_100.0, 10_050.0],
#     fills: vec![],
#     initial_equity: 10_000.0,
# };
let returns  = per_bar_returns(&report.equity_curve, report.initial_equity);
let segments = drawdown_segments(&report.equity_curve);

let _sharpe = sharpe(&returns, /*rf=*/ 0.0, /*bars_per_year=*/ 252.0);
let _max_dd = max_drawdown(&segments);
```

This is what the `fugazi` CLI backtester sits on top of — it drives `run`, then
aggregates every metric into a YAML report.

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

## Command-line backtester

The crate ships a `fugazi` binary that loads a strategy from YAML, assembles
candle data from one or more CSV series, runs it through a `PaperWallet`, and
writes the result files:

```sh
cargo run --bin fugazi -- run \
  @examples/strategy.yml \
  --series @examples/candles.csv \
  --output-dir out/
# writes out/trades.csv (time;symbol;side;units;price;kind),
#        out/returns.csv (time;equity;return),
#        and out/metrics.yml (whole-run summary)
```

**No plots.** `fugazi` deliberately emits data files only — plotting is a
post-hoc analysis (see [Analyzing a run in R](#analyzing-a-run-in-r) below).

The strategy is a **positional** argument (not a flag) and follows the same `@`
convention as `--series`: `@file` loads a file, anything else is treated as inline
content — handy for quick one-offs, e.g.
`'{ symbol: BTC, long: { enter: !crosses_above { lhs: !sma { period: 3 }, rhs: !sma { period: 8 } } } }'`. The format is YAML — block or flow
(inline) style, both fine. (JSON is a subset of YAML, so a JSON-shaped document
still parses: a `!sma { … }` tag is just the singleton map `{"sma": …}`.)

Flags: `<STRATEGY>` (positional, `@file` or inline), `--series <spec>`
(repeatable), `--output-dir <dir>`, `--cash <amount>` (default `10000`),
`--params <spec>` (repeatable — see below), calendar shortcuts
(`--stocks`/`--forex`/`--crypto` + `--frequency`) or explicit `--bars-per-year`,
and `--risk-free-rate <rate>` for the annualized rf. `--costs <spec>`
(repeatable, same `,`-separated `key=value` / `@file.yml` shape as `--params`)
wires a commission/spread/slippage model into every fill — venue presets ship
in [`examples/binance.yml`](examples/binance.yml) and
[`examples/ibkr.yml`](examples/ibkr.yml); omit for a frictionless backtest
identical to the pre-costs release. `-w/--windowed <N>` reduces the run in
`N`-bar windows *for post-hoc analysis* — `metrics.yml` (whole-run) is still
written, and adding `-w N` writes two extra CSVs at window length `N`:
`metrics.csv` (non-overlapping windows, one row each) and `rolling.csv`
(rolling stride-1 windows, one row each). Both share the same columns —
`window_start;window_end;<full metric catalogue under dotted metrics.yml
names>` — so R/Python can consume them interchangeably. Non-overlapping is
right for cross-window aggregation (independent samples → the sample stddev is
meaningful); rolling is right for continuous plotting (a smooth curve — the
metrics.csv equivalent to pyfolio's rolling-Sharpe chart). The same
`-w/--windowed <N>` exists on `optimize`: each grid point is evaluated in
non-overlapping windows only (the ranker's mean±k·std needs the independence),
every `-m` metric becomes two CSV columns (`<name>_mean` / `<name>_std`), and
`--best-by` ranks by the windowed mean — rewarding parameter sets that perform
consistently across regimes rather than in one lucky stretch. Add
`-k/--risk-aversion <K>` to rank conservatively: the mean is shifted *against*
each grid point by `K` standard deviations before sorting (`mean − K·std` for
higher-is-better metrics, `mean + K·std` for lower-is-better ones), so
`sharpe 2.0 ± 3.0` no longer outranks `1.8 ± 0.2`. Output files are
`;`-delimited for Excel.

`run` and `optimize` measure the whole run — the strategy layer is
opinion-free about stability. A strategy that wants entries held off until
every source it consults has settled composes the check at the entry with
`!stable`: `enter: !all [<entry>, !stable { signal: <entry> }]` fires the
inner `<entry>` only once its whole chain is past its `stable_period()`. For
purely windowed (FIR) strategies — SMA crossovers and the like — `!stable`
delays to the same bar the signal first defines, so it is a no-op.
(`fugazi get`'s `--keep-unstable` is unrelated — it disables the per-overlay
trim of pre-`stable_period()` cells in the emitted CSV.)

Console output is a two-line banner (the constant tool identity, then the active
command) followed by four blocks: an **inputs** block of the execution params
(strategy, params in effect, candle period start→end, starting capital, output
dir), a **trades** block listing each fill (time, symbol, side, quantity, price,
kind — a symbol is per-trade, never a run-level field), a **result** block (bars,
trade count, capital start→end with absolute and percent change, then start/finish
timestamps with elapsed runtime), and a **metrics** block echoing the headline
lines of `metrics.yml`. `-q` silences all of it (the result files are still
written).

**Data — `--series`.** Each `--series` is a `,`-separated list of terms:
`key=value` adds a constant column, `@file.csv` loads a CSV's columns and rows
(each file's column delimiter — `;`, `,`, tab or `|` — is autodetected from its
header). Within a series the literals broadcast across the file's rows; across
several `--series` the tables are full-outer-joined on `(symbol, time)` into one
long frame. So a symbol-less OHLCV file gets `symbol=BTC,@candles.csv`, a file
with its own `symbol` column loads as `@multi.csv`, and extra fields (e.g.
fundamentals) ride along on a second series. Required columns: `time`, `symbol`,
and `open`/`high`/`low`/`close` (`volume` optional); `time` is sorted as an
opaque token (dates, epochs — anything sortable).

**Strategy — `strategy.yml`.** A `symbol` plus `long`/`short` sides (each an
`enter` signal and an optional `exit`). A side's `exit`
defaults to never-fire, which is exactly right for an always-in long/short
reversal (the opposite side's `enter` reverses the position); give an `exit` only
for a flat rest. Signals and sources are written
with YAML **tags** — `!sma { source: close, period: 5 }` — while candle-field
leaves are bare words (`close`, `high`, `volume`, …). Omitted `source` defaults
to `close`. The vocabulary mirrors the library one-to-one:

- **Sources:** leaves `close`/`high`/`low`/`open`/`volume`/`typical`/`median`,
  `!value <n>`; `!sma`/`!ema`/`!rma`/`!wma`/`!hma`/`!rsi`/`!stddev`/`!cci`/
  `!stochastic { source, period }`, `!stoch_rsi { source, rsi_period,
  stoch_period }`; `!macd_line`/`!macd_signal`/`!macd_histogram { source, fast,
  slow, signal }`; `!bb_upper`/`!bb_middle`/`!bb_lower { source, period, k }`;
  `!keltner_{upper,middle,lower} { source, ema_period, atr_period, multiplier }`;
  `!donchian_{upper,middle,lower} { high, low, period }`; `!adx`/`!plus_di`/
  `!minus_di`/`!dmi_plus_di`/`!dmi_minus_di`/`!aroon_{up,down,oscillator}
  { period }`; bar indicators `!atr`/`!mfi`/`!williams_r { period }`, `!obv`/
  `!vwap`/`!ad`/`!true_range`, `!sar { step, max }`; transforms `!add`/`!sub`/
  `!mul`/`!div { lhs, rhs }`, `!lag`/`!diff`/`!ratio`/`!roc { source, periods }`,
  `!rolling_max`/`!rolling_min { source, period }`.
- **Signals:** `!gt`/`!lt`/`!ge`/`!le`/`!eq`/`!ne { lhs, rhs, epsilon? }`,
  `!above`/`!below { source, level }`; `!crosses_above`/`!crosses_below
  { lhs, rhs }`; `!and`/`!or`/`!xor { lhs, rhs }`, `!all [ … ]`, `!any [ … ]`,
  `!not <signal>`, `!changed <signal>`, `!stable { signal: <signal> }` (`true`
  once the inner signal is past its `stable_period()`), `!value <bool>`.

**Parameters — `!param`.** Any value in the strategy can be a placeholder resolved
at run time with `--params` (repeatable), so one file covers many variations
(periods, thresholds, the traded symbol):

```yaml
symbol: !param { key: SYM, default: BTC }
long:
  enter: !crosses_above
    lhs: !sma { source: close, period: !param { key: FAST } }          # required
    rhs: !sma { source: close, period: !param { key: SLOW, default: 8 } }  # optional
```

`--params` is a `,`-separated list of terms, exactly like `--series` (and itself
repeatable): `NAME=value` sets one, and `@file.yml` loads a whole
`NAME: value` mapping (see [`examples/params.yml`](examples/params.yml)). Terms
apply left-to-right, so a later one wins:

```sh
cargo run --bin fugazi -- run @examples/strategy.params.yml \
  --params @examples/params.yml,FAST=5 \
  --series @examples/candles.csv --output-dir out/
```

A `default` makes a param optional; without one, a missing value is an error.
`!param NAME` is shorthand for `!param { key: NAME }`.
A `NAME=value` value is parsed as a scalar (so `FAST=5` is a number, `SYM=BTC`
a string), then substituted before the strategy is typed — so a param can stand in
anywhere, including where a number is required.

See [`examples/strategy.yml`](examples/strategy.yml) for a complete SMA-crossover
strategy, and [`examples/strategy.params.yml`](examples/strategy.params.yml) for
the parameterised version.

### Multi-series batch mode

When the input frame carries more than one `(symbol, freq)` group — e.g.
`fugazi get`'s output covering several tickers, or several vendor CSVs
joined into one `--series` — `run` iterates the strategy across each
group in parallel on a rayon pool. There's no separate subcommand; the
trigger is the shape of the input.

Two CLI-managed template variables get substituted per iteration:
`%SYMBOL` (the iteration's symbol, path-normalized) and `%FREQ` (the
iteration's effective bar cadence, or an empty string when detection
fails and no `-f` was set). Reference them in `--params` values and
`--output-dir`; the CLI folds them in per group. The `%`-prefixed name
space is reserved — you cannot declare `--params %FOO=…` yourself.

```sh
cargo run --bin fugazi -- run @examples/strategy.params.yml \
  --series @multi.csv \
  --params SYMBOL=%SYMBOL \
  --output-dir out/
# writes out/{trades,returns,metrics}.csv with leading `symbol` / `freq`
# columns (loose sigils become row-identifier columns).
```

The strategy carries `symbol: !param SYMBOL` and each iteration builds
its own spec with the group's symbol substituted in. Output layout
follows the fully-expanded `--output-dir`:

- `--output-dir out/` — all iterations share `out/`; `symbol`/`freq`
  columns appear on every CSV; `metrics.csv` has one row per iteration
  (no `metrics.yml`, since the shape is now tabular).
- `--output-dir out/%SYMBOL/` — each symbol's iterations land in
  `out/<sym>/`; the bucket's shape decides whether that dir gets a
  single-doc `metrics.yml` or a tabular `metrics.csv`.

Row order across the aggregated CSVs is `(symbol, freq, time)`, with
`freq` sorted by duration (`1h < 4h < 1d`). A rayon pool sized by
`-j/--jobs` runs the iterations in parallel; a hardcoded-symbol strategy
on a multi-symbol frame silently skips the mismatched groups.

`--single` (the default) uses `SingleAssetStrategy` in the iteration
loop. `--multiple` is reserved for a future `MultiAssetStrategy`
(portfolio / pairs — one strategy that sees several symbols at once) and
errors out today.

### Analyzing a run in R

`fugazi` writes the numbers; you plot them in whatever you already use for
analysis. A minimal R session that produces pyfolio-style
cumulative-returns / rolling-Sharpe / underwater plots from a `-w 126` run:

```r
library(readr)
library(ggplot2)

returns <- read_delim("out/returns.csv", delim = ";")
rolling <- read_delim("out/rolling.csv", delim = ";")
metrics <- read_delim("out/metrics.csv", delim = ";")   # non-overlapping — for cross-window stats

# Cumulative returns: equity rebased to the seed cash.
returns$cum <- returns$equity / returns$equity[1]
ggplot(returns, aes(as.Date(time), cum)) + geom_line() +
  geom_hline(yintercept = 1, linetype = "dashed") +
  labs(x = NULL, y = "Cumulative returns (×)")

# Rolling Sharpe: each row of rolling.csv is one window; window_end is the anchor.
ggplot(rolling, aes(as.Date(window_end), risk_adjusted.sharpe)) + geom_line() +
  geom_hline(yintercept = 0) +
  geom_hline(yintercept = mean(rolling$risk_adjusted.sharpe, na.rm = TRUE),
             linetype = "dashed", colour = "steelblue") +
  labs(x = NULL, y = "Rolling Sharpe")

# Underwater plot: drawdown from the running peak.
returns$peak <- cummax(returns$equity)
returns$dd   <- (returns$equity - returns$peak) / returns$peak
ggplot(returns, aes(as.Date(time), dd)) + geom_area(alpha = 0.35, fill = "#c44e52") +
  geom_line(colour = "#c44e52") + geom_hline(yintercept = 0) +
  scale_y_continuous(labels = scales::percent) +
  labs(x = NULL, y = "Drawdown")

# Cross-window Sharpe distribution — independent samples (non-overlapping),
# so the sample stddev actually means something.
mean(metrics$risk_adjusted.sharpe, na.rm = TRUE)
sd  (metrics$risk_adjusted.sharpe, na.rm = TRUE)
```

**Which CSV do you want?** `rolling.csv` for plots (smooth, continuous
curve); `metrics.csv` for cross-window statistics (mean ± stddev, quantiles,
regime-conditioning). The rolling series is heavily autocorrelated —
adjacent rows share `N-1` of `N` bars — so `sd()` on it drastically
understates variability; treat it as a plotting artefact, not a sample.

Both files share the same columns (dotted `metrics.yml` names), so the same
plotting code works on either.

**Other subcommands.** Alongside `run` the binary carries a few utility
subcommands — briefly listed here, fully documented in
[CLI.md](CLI.md):

- `fugazi check strategy <STRATEGY>` / `check overlay <SPEC>...` — a spec-only
  lint pass (no data, no wallet). Fails a CI job if the strategy or overlay
  doesn't parse and build.
- `fugazi optimize <STRATEGY> --params NAME=<axis>...` — sweep the strategy
  over a parameter grid and rank combinations by a metric.
- `fugazi get <PROVIDER>:<SYMBOL>[<FREQ>] --since ... -o candles.csv` — fetch
  OHLCV bars from `binance` or `yfinance` into a `run`-ready CSV, or
  re-process an existing CSV with `file:PATH`. `-x/--overlay col=<source>`
  appends indicator columns computed on the fetched bars.
- `fugazi list indicators` / `list sources` / `list tickers <PROVIDER>` — the
  YAML tag catalogue, the `get`-provider table, and (via HTTP) the
  provider's ticker vocabulary.
- `fugazi completions <shell>` — a shell-completion script (see
  [CLI.md § Shell completion](CLI.md#shell-completion)).

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
import fugazi as ta

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
import fugazi as ta

enter = ta.sma(ta.close(), 3).crosses_above(ta.sma(ta.close(), 10))
exit_ = ta.sma(ta.close(), 3).crosses_below(ta.sma(ta.close(), 10))
wallet = ta.PaperWallet(10_000.0)

for o, h, l, c, v in bars:
    candle = ta.Candle(o, h, l, c, v)
    wallet.update("AAPL", candle)                             # feed the wallet this bar
    went_long, went_flat = enter.update(candle), exit_.update(candle)  # advance both
    if went_long:
        wallet.set("AAPL", "buy", ta.Size.value_frac(1.0))   # size: units / funds / equity / position
    elif went_flat:
        wallet.close("AAPL")

print(wallet.funds, wallet.position("AAPL"), wallet.orders())
```

Install with `pip install fugazi` (prebuilt wheels for Linux, macOS and
Windows), or build from a checkout with `cd python && maturin develop --release`.
See the [Python README](python/README.md) for the full API.

## License

MIT — see [LICENSE](LICENSE).
