# fugazi

A Rust library of **incremental** technical-analysis primitives. Every indicator
and signal owns its internal state and is advanced one sample at a time via
`update()`, carrying just enough intermediate state to produce the next output in
~O(1). The same code works for live streaming and for batch backtesting.

- **Incremental** — feed one bar at a time; no full-history recomputation.
- **Composable** — indicators own their input source, so you build complex
  signals by nesting constructors. No glue, no remembering what to feed where.
- **Minimal dependencies**, `edition = "2024"`.

## Install

```toml
[dependencies]
fugazi = "0.24"
```

## Concepts

The crate has three composable layers:

- **Indicators** are the numeric *sources*. Each produces a `Real` (`f64`) and
  **owns its own input source**, so composition is just nesting constructors:
  `Ema::new(Current::close(), 20)` is the EMA‑20 of the close. Leaf sources
  terminate the chain — `Identity` (raw value stream), `Value` (a constant), and
  the candle accessors under `Current` (`Current::close()`, `Current::volume()`,
  …). Bar indicators (`Atr`, `Adx`, `TrueRange`, …) read the whole bar, so they
  take a `Candle`-output source too — `Atr::new(Current::candle(), 14)`,
  `Obv::new(Current::candle())`, etc. Every atom-input leaf (candle field,
  calendar accessor, overlay `Get*` reader) is **generic over an atom-emitting
  source `S` with default `Identity<Atom>`**, so the same primitives serve both
  the single-series hot path and the cross-asset case — see
  [Cross-asset composition](#cross-asset-composition) below. Every indicator
  is fed one `Atom` per bar (`Atom { candle, overlays }` — a `Candle` plus an
  optional overlay bundle); a bare `Candle` lifts to an `Atom` via
  `From<Candle> for Atom`, so `signal.update(candle.into())` is the streaming
  pattern.
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
before trusting the output. The **default is safe**: a
`SingleAssetStrategy`'s `is_ready()` compares its bar count against the
largest `stable_period()` across every wired signal and every attached
protective level, and the run driver skips its `trade()` until that clears —
so no trade fires on a seed-contaminated value. The explicit opt-out is
[`Unstable`](https://docs.rs/fugazi/latest/fugazi/indicators/struct.Unstable.html):
`.unstable()` on any source *or* signal (in YAML, `!unstable { source: <s> }`
/ `!unstable { signal: <s> }`) is a passthrough that reports
`unstable_period() = 0`, telling the readiness gate "I'm happy to trade
through this subtree's IIR settling tail". Safe by default, overridable per
subtree — the same shape as `fugazi get`'s `--keep-unstable` flag (default
trims each overlay's pre-`stable_period()` cells; the flag opts out).

## Quick start

"Current close crosses above its EMA‑20" — defined once, fed one `Candle` per bar:

```rust
use fugazi::prelude::*;
use fugazi::indicators::{Current, Ema};

let mut signal = Current::close().crosses_above(Ema::new(Current::close(), 20));

# let feed: Vec<Candle> = Vec::new();
for candle in feed {
    signal.update(candle.into());
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
    overbought.update(candle.into());
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

### Sharing one multi-output indicator across many accessors

Two accessors on the same `Bollinger` (or `Macd`, …) mean two full copies of
the indicator running independently — cheap by itself, but a crossover clones
its operands, and a strategy with a `long_on(up, down)` and `short_on(down,
up)` ends up asking the compiler to run the same multi-output indicator 8 or
16 times per bar. When the accessors all target one instance, wrap it in a
[`Shared`](https://docs.rs/fugazi/latest/fugazi/indicators/struct.Shared.html)
handle with `.shared()` and use the same accessor methods off the handle:

```rust
use fugazi::prelude::*;
use fugazi::indicators::{Current, Macd};

// One MACD, driven exactly once per bar however many accessors read out of it.
let macd = Macd::new(Current::close(), 12, 26, 9).shared();
let up = || macd.line().crosses_above(macd.signal());
let down = || macd.line().crosses_below(macd.signal());
```

Each `.line()` / `.signal()` off `macd` returns a `SharedComponent` that
borrows the same source through an `Rc<RefCell<_>>`; whichever accessor is
updated first each bar drives the underlying MACD, the rest read the cached
outputs. Behaviour is identical to the independent-clones form
(component-level tests assert it bit-for-bit); only the per-bar cost goes
down — the classical strategies (`macd_crossover`, `donchian_breakout`,
`bollinger_breakout`, `bollinger_reversion`, `keltner_breakout`) opt into
this by default, and any new strategy that stacks several accessors on one
indicator should too.

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

### Cross-asset composition

For strategies that reason about more than one instrument per bar, feed a
**`Snapshot<Sym>`** — a series of `(Option<Sym>, Option<Frequency>, Atom)`
entries — and use `Pick<Sym, S>` to project one asset out. Every atom-input
leaf composes on top verbatim through its `T::of(source)` constructor:

```rust
use fugazi::prelude::*;
use fugazi::indicators::{Close, Pick};
use fugazi::{Frequency, Selector, Snapshot};

// BTC/ETH close spread as a first-class Real-output indicator whose Input is
// Snapshot<String>. Two symbol-matching `Pick`s + arithmetic — no
// per-strategy machinery.
let mut spread = Close::of(Pick::<String>::matching(Selector::by_symbol("BTC")))
    .sub(Close::of(Pick::<String>::matching(Selector::by_symbol("ETH"))));

// Feed one snapshot per bar.
let mut snap = Snapshot::<String>::new();
snap.push(Some("BTC".into()), None, Atom::new(Candle::new(100.0, 101.0, 99.0, 100.0, 1.0)));
snap.push(Some("ETH".into()), None, Atom::new(Candle::new(60.0,  61.0, 59.0, 60.0,  1.0)));
assert_eq!(spread.update(snap), Some(40.0));
```

`Selector<Sym>` is a **partial-key predicate**, not a snapshot key:
`by_symbol("BTC")` matches every BTC entry regardless of frequency,
`by_freq(Frequency::Hour(1))` matches every hourly entry regardless of
symbol, `exact("BTC", Frequency::Hour(1))` matches a single tagged entry.
Empty selector (`Selector::default()`, both fields `None`) is the "no query"
sentinel — [`Pick::new()`](https://docs.rs/fugazi/latest/fugazi/indicators/struct.Pick.html)
uses it to trigger `Snapshot::sole_atom` (single-entry unpack, panics on 2+),
so a strategy authored around cross-asset primitives still runs cleanly on a
single-series driver that feeds size-1 snapshots via `Snapshot::of_atom`.

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
use fugazi::indicators::{Close, Pick, Sma};
use fugazi::Snapshot;

// Own your signals; act on the wallet. `update` advances the signals; `trade`
// reads them and acts. `Size` is absolute units or a fraction of funds / equity /
// current position, and `Side` gives direction — so position sizing,
// short-selling, and staying always-in-market are just what the code does.
//
// `Input = Snapshot<Sym>` — the multi-asset input frame. For a single-series
// backtest the driver feeds size-1 snapshots (`Snapshot::of_atom(atom)`) and
// the empty-selector `Pick::<Sym>::new()` inside every leaf unpacks the
// sole atom.
struct GoldenCross {
    symbol: &'static str,
    enter: Box<dyn Signal<Snapshot<&'static str>>>,
    exit: Box<dyn Signal<Snapshot<&'static str>>>,
}

impl Strategy for GoldenCross {
    type Input = Snapshot<&'static str>;
    type Symbol = &'static str;

    fn update(&mut self, snap: Snapshot<&'static str>) {
        // Advance EVERY signal every bar (don't short-circuit, or a skipped one
        // desyncs from the price stream).
        self.enter.update(snap.clone());
        self.exit.update(snap);
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

let close = || Close::of(Pick::<&'static str>::new());
let mut strat = GoldenCross {
    symbol: "AAPL",
    enter: Box::new(Sma::new(close(), 3).crosses_above(Sma::new(close(), 10))),
    exit:  Box::new(Sma::new(close(), 3).crosses_below(Sma::new(close(), 10))),
};
let mut wallet = PaperWallet::new(10_000.0);

# let feed: Vec<Candle> = Vec::new();
for candle in feed {
    wallet.update("AAPL", candle);                          // feed the wallet
    strat.update(Snapshot::of_atom(candle.into()));         // advance signals
    strat.trade(&mut wallet);                                // act
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

## Safe defaults, opt-in overrides

Numbers this library produces during a source's warm-up or IIR settling tail
are *unsettled*: they exist, but their value depends on the seed, on the
segment the window happens to start on, or on both. Every knob that could
paper over an unsettled bar is therefore biased toward **waiting**, with a
single-name **flag / wrapper / period** for the caller who has considered the
tradeoff and would rather trade through it.

- **Strategy readiness.** `SingleAssetStrategy::is_ready()` returns `true`
  only once `bars_seen` reaches the largest `stable_period()` across every
  wired signal and every attached protective level, and
  `fugazi::backtest::run` skips `trade()` until then. Wrap a subtree in
  [`Unstable`](https://docs.rs/fugazi/latest/fugazi/indicators/struct.Unstable.html)
  (`.unstable()` in Rust/Python, `!unstable { source }` / `!unstable { signal
  }` in YAML) to zero its reported `unstable_period()` and skip the wait for
  its IIR tail. `update()` and `on_fill()` still run every bar so the warm-up
  progresses; a custom `Strategy` inherits the default `is_ready() = true` and
  can override for the same effect.
- **`fugazi get` overlays.** By default the CLI trims each overlay column's
  pre-`stable_period()` cells before writing the CSV, so no downstream reader
  sees an unsettled value. `--keep-unstable` emits every sample from bar 1 —
  useful when you want the full trace for debugging or plotting.
- **Explicit periods.** Every windowed indicator takes a `period` argument
  and asserts it is strictly positive at construction; there is no "sensible
  default" that would hide the choice. Similarly `sharpe(..., rf, bpy)` takes
  an explicit risk-free rate and bars-per-year — an omission would silently
  pick numbers.

The rule when adding a new knob: pick the value that is safest when the user
forgets to think about it, and provide *one* mechanism to opt out.

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
use fugazi::Snapshot;

# struct MyStrategy;
# impl Strategy for MyStrategy {
#     type Input = Snapshot<&'static str>;
#     type Symbol = &'static str;
#     fn update(&mut self, _: Snapshot<&'static str>) {}
#     fn trade(&self, _: &mut dyn Wallet<&'static str>) {}
#     fn reset(&mut self) {}
# }
# let mut strat = MyStrategy;
# let candles: Vec<Candle> = vec![];
let mut wallet = PaperWallet::new(10_000.0);
// Each bar is a `Snapshot<Sym>` — a keyed collection of tagged atoms. For a
// single-series run, `Snapshot::single(sym, atom)` tags the sole entry with
// the trading symbol so `run` prices the wallet each bar.
let snapshots = candles
    .into_iter()
    .map(|c| Snapshot::single("AAPL", c.into()));
let report = run(&mut strat, &mut wallet, snapshots);
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
- **Trailing strategy risk** (own an embedded `Strategy`, reduce its live
  equity curve to a rolling metric over the last `period` bars): `Sharpe`,
  `Sortino`, `Volatility`, `MaxDrawdown`, `Calmar`. A trailing risk-adjusted
  estimate becomes a first-class source — read it as an overlay column
  (`fugazi get -x`) or compose it into another strategy — instead of the
  "run a strategy → dump `returns.csv` → re-join it" round-trip.
- **Sources & transforms:** `Identity`, `Value`, `Current::*` candle accessors,
  calendar accessors (`Year`/`Month`/`Day`/`Hour`/…/`DayOfWeek`/`WeekOfYear`),
  overlay readers (`GetReal`/`GetBool`/`GetStr`), `TrueRange`;
  `Add`/`Sub`/`Mul`/`Div`, `Lag`/`Diff`/`Ratio`/`Roc`,
  `RollingMax`/`RollingMin`
- **Signals:** `Gt`/`Lt`/`Ge`/`Le`/`Eq`/`Ne` comparisons, `and`/`or`/`xor`/`not`,
  `changed`, `crosses_above`/`crosses_below`
- **Cross-asset primitives:** `Snapshot<Sym>` (per-bar tagged-atom series),
  `Selector<Sym>` (partial-key matcher), `Pick<Sym, S>` (project one asset out
  of a snapshot), `Frequency` (bar cadence — `Minute(u32)`/`Hour(u32)`/…). The
  same atom-input leaves (`Close::of(source)`, `Year::of(source)`, `Atr` on
  `CurrentBar::of(source)`, `GetReal::of(schema, key, source)`) drop straight
  on top of a `Pick` for cross-asset composition.

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
# writes out/trades.csv (time,symbol,side,units,price,kind),
#        out/returns.csv (time,equity,return),
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
`window_start,window_end,<full metric catalogue under dotted metrics.yml
names>` — so R/Python can consume them interchangeably. The console prints
an extra **windowed metrics** block under `-w` showing `mean ± std` across
the non-overlapping rows, right after the whole-run block — so the single
estimate and the cross-window dispersion sit side-by-side. Non-overlapping is
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
`,`-delimited.

`run` and `optimize` measure the whole run. The strategy layer's readiness
default holds entries until every wired signal *and* every attached
protective level is past its `stable_period()` (see
[`SingleAssetStrategy::is_ready`](https://docs.rs/fugazi/latest/fugazi/strategies/struct.SingleAssetStrategy.html)),
so a seed-contaminated bar never fires a trade. Purely windowed (FIR)
strategies — SMA crossovers and the like — have `unstable_period() = 0`
throughout, so the gate elapses on the last warm-up bar and never lags them.
Users who explicitly accept the IIR settling tail on a subtree opt out by
wrapping it in `!unstable`: `!unstable { source: !ema { … } }` /
`!unstable { signal: !crosses_above { … } }` reports `unstable_period() = 0`
for that subtree while forwarding the underlying output. Same shape as
`fugazi get`'s `--keep-unstable` (default trims pre-`stable_period()` cells
from each overlay CSV column; the flag opts out) — see [Safe defaults, opt-in
overrides](#safe-defaults-opt-in-overrides).

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
  `!value <n>` (and its string twin `!value <str>` — a constant string source,
  the operand of `!str_eq` / `!str_ne`; quote a numeric-looking one, `!value
  "70"`, to get the string rather than the scalar);
  `!sma`/`!ema`/`!rma`/`!wma`/`!hma`/`!rsi`/`!stddev`/`!cci`/
  `!stochastic { source, period }`, `!stoch_rsi { source, rsi_period,
  stoch_period }`; windowed statistics `!skewness`/`!kurtosis`/`!zscore
  { source, period }`, `!correlation { lhs, rhs, period }`, and the
  Lo-MacKinlay regime classifier `!variance_ratio { source, period, lag }`
  (`> 1` trending, `< 1` mean-reverting; recomputes O(period)/bar, unlike the
  incremental rest); `!macd_line`/`!macd_signal`/`!macd_histogram { source, fast,
  slow, signal }`; `!bb_upper`/`!bb_middle`/`!bb_lower { source, period, k }`;
  `!keltner_{upper,middle,lower} { source, ema_period, atr_period, multiplier }`;
  `!donchian_{upper,middle,lower} { high, low, period }`; `!adx`/`!plus_di`/
  `!minus_di`/`!dmi_plus_di`/`!dmi_minus_di`/`!aroon_{up,down,oscillator}
  { period }`; bar indicators `!atr`/`!mfi`/`!williams_r { period }`,
  range-based volatility `!parkinson`/`!garman_klass`/`!rogers_satchell
  { period }`, `!obv`/
  `!vwap`/`!ad`/`!true_range`, `!sar { step, max }`; transforms `!add`/`!sub`/
  `!mul`/`!div { lhs, rhs }`, `!lag`/`!diff`/`!ratio`/`!roc { source, periods }`,
  `!rolling_max`/`!rolling_min { source, period }`.
- **Signals:** `!gt`/`!lt`/`!ge`/`!le`/`!eq`/`!ne { lhs, rhs, epsilon? }`,
  `!above`/`!below { source, level }`; `!crosses_above`/`!crosses_below
  { lhs, rhs }`; `!and`/`!or`/`!xor { lhs, rhs }`, `!all [ … ]`, `!any [ … ]`,
  `!not <signal>`, `!changed <signal>`, `!unstable { signal: <signal> }`
  (passthrough that reports `unstable_period() = 0` for the wrapped subtree —
  opt-in override of the "wait for the IIR tail" readiness default; there is
  also a `!unstable { source: <source> }` twin on the source side),
  `!value <bool>`; `!str_eq`/`!str_ne { lhs, rhs }` compare a `Str` overlay
  column (`lhs: !get { key: regime }`) against a string — `rhs` takes a bare
  literal (`rhs: bull`), the same constant as a source (`rhs: !value bull`), or
  a second `Str` column (`rhs: !get { key: prev_regime }`).

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

**Imports — `!import`.** Any value in the strategy can instead be loaded from
another YAML file, so a shared entry rule, a sizing recipe, or a whole side
lives in one place and is reused across strategies:

```yaml
symbol: BTC
long:
  enter: !import signals/breakout.yml     # the file's value takes this slot
  exit: !crosses_below { lhs: close, rhs: !sma { period: 20 } }
sizing: !import sizing/half-kelly.yml
```

**Paths are relative to the importing file**, not to where `fugazi` was invoked
— `strategies/btc.yml` importing `signals/breakout.yml` finds
`strategies/signals/breakout.yml` from any working directory. (Inline strategy
text has no directory of its own, so its imports resolve against the working
directory.) An imported file is an ordinary spec fragment: it may contain its
own `!import`s — resolved relative to *itself* — and its own `!param`
placeholders, because the load order is **parse → `!import` → `!param` → typed
parse**, so one `--params` table parameterises the whole imported tree. An
import cycle is an error naming the chain rather than a hang.

See [`examples/strategy.yml`](examples/strategy.yml) for a complete SMA-crossover
strategy, and [`examples/strategy.params.yml`](examples/strategy.params.yml) for
the parameterised version.

### Strategy shape prefix

The strategy positional accepts an optional shape prefix:

- `single:` (or no prefix) — a `SingleAssetStrategy` file
  (`single:@strategy.yml` ≡ `@strategy.yml`).
- `pairs:` — a two-symbol `PairsStrategy` file (`pairs:@spread.yml`);
  the document declares `left`/`right` symbols and cross-asset
  signal / level expressions rooted through `!pick { symbol, freq }`.

Any other prefix is rejected as an unknown shape. A single-asset run
feeds every candle in the input series to the strategy in `time` order.
A pairs run feeds the paired `(left, right)` atoms as one snapshot per
bar; each leg is priced and can fill independently, and the strategy
sees both symbols in the same snapshot.

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
[doc/CLI.md](doc/CLI.md):

- `fugazi check strategy <STRATEGY>` / `check overlay <SPEC>...` — a spec-only
  lint pass (no data, no wallet). Fails a CI job if the strategy or overlay
  doesn't parse and build.
- `fugazi optimize <STRATEGY> --params NAME=<axis>...` — sweep the strategy
  over a parameter grid and rank combinations by a metric.
- `fugazi get <PROVIDER>:<SYMBOL>[<FREQ>] --since ... -o candles.csv` — fetch
  OHLCV bars from `binance` or `yfinance` into a `run`-ready CSV, or
  re-process an existing CSV with `csv:PATH`. `-x/--overlay col=<source>`
  appends indicator columns computed on the fetched bars; `--params` resolves
  `!param` placeholders inside those overlay expressions.
- `fugazi list indicators` / `list sources` / `list tickers <PROVIDER> [PATTERN]`
  — the YAML tag catalogue, the `get`-provider table, and (via HTTP) the
  provider's ticker vocabulary. A provider lists thousands of symbols, so
  `tickers` takes an optional shell-style glob: `fugazi list tickers binance
  'b*'` (starts with `b`), `'*b*'` (contains `b`), `'b*usd*t'`, `'[a-c]*'`.
  Matching is case-insensitive and whole-symbol; quote the pattern so your
  shell doesn't try to expand it against your files.
- `fugazi completions <shell>` — a shell-completion script (see
  [doc/CLI.md § Shell completion](doc/CLI.md#shell-completion)).

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

`fugazi.metrics` is the reporting surface — the same one-function-per-metric
catalogue as [`fugazi::metrics`](src/metrics.rs), for computing Sharpe / Sortino
/ Calmar, drawdown analytics, and trade statistics from an equity curve and a
bar-tagged fill blotter (built with `fugazi.Fill(bar, order)`):

```python
from fugazi import metrics
equity = [10_000.0, 10_050.0, 10_100.0, 9_900.0, 10_200.0, 10_300.0]
returns = metrics.per_bar_returns(equity, initial_equity=10_000.0)
metrics.sharpe(returns, risk_free_rate=0.0, bars_per_year=252)   # ratio | None
```

Install with `pip install fugazi` (prebuilt wheels for Linux, macOS and
Windows), or build from a checkout with `cd python && maturin develop --release`.
See the [Python README](doc/PYTHON.md) for the full API.

## License

MIT — see [LICENSE](LICENSE).
