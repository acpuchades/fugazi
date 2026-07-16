# Strategy files

A **strategy file** is the declarative input to the `fugazi run` backtester. It
describes what to trade and the signals that open and close the positions. The
file is a YAML mirror of the library's composition API — every tag maps
one-to-one to a fugazi constructor — so a strategy you can write in Rust by
nesting constructors you can also write in a file, and vice versa.

```sh
fugazi run @strategy.yml --series @candles.csv --output-dir out/
```

The strategy is the positional argument and follows the `@` convention the data
flags use: `@file.yml` loads a file, anything else is treated as
inline content (handy for one-offs, e.g. `'{ symbol: BTC, long: { enter: !crosses_above { lhs: !sma { period: 3 }, rhs: !sma { period: 8 } } } }'`).

## The three strategy shapes

There are three document shapes, picked by an optional **prefix** on the
positional. The prefix decides which document type the YAML is deserialized
into — the expression vocabulary ([Sources](#sources) / [Signals](#signals)) is
identical across all three.

| Prefix | Shape | Document | Traded symbols |
| --- | --- | --- | --- |
| none, or `single:` | [`SingleAssetStrategy`](../src/strategies/single_asset.rs) | [Single-asset](#single-asset-documents) | one, named by `symbol` |
| `pairs:` | [`PairsStrategy`](../src/strategies/pairs.rs) | [Pairs](#pairs-documents) | two, named by `left` / `right` |
| `basket:` | [`BasketStrategy`](../src/strategies/basket.rs) | [Basket](#basket-documents) | N — **whatever the input series carry** |

```sh
fugazi run @strategy.yml         --series @btc.csv --output-dir out/           # single
fugazi run pairs:@spread.yml     --series @btc.csv --series @eth.csv -o out/   # pairs
fugazi run basket:@basket.yml    --series @btc.csv --series @eth.csv \
                                 --series @sol.csv --series @ada.csv -o out/   # basket
```

`fugazi optimize` currently supports the single-asset shape only; `pairs:` and
`basket:` documents run under `fugazi run` (and validate under
`fugazi check strategy`).

> This document is the syntax reference. For the surrounding CLI (`--series`,
> `--params`, output files, console output) see the
> [Command-line backtester](README.md#command-line-backtester) section of the
> README. For the library API the vocabulary mirrors, see the rest of the README.

> **Single-series and cross-asset.** Every existing strategy YAML keeps
> working unchanged. Under the hood the CLI feeds each strategy a per-bar
> `Snapshot<String>` (a `(symbol, freq, atom)` series) instead of a raw
> `Atom`; when a strategy is run against a single-series driver it
> receives a size-1 snapshot per bar and every atom-input leaf (`close`,
> `!ema { source: close, ... }`, `!year`, `!is_weekday`, …) is rooted
> through an implicit empty-selector `Pick` that unpacks the sole atom.
> Cross-asset composition through YAML is spelled with the explicit
> `!pick { symbol, freq }` source tag on any atom-input leaf — e.g.
> `!close { source: !pick { symbol: BTC } }` in a signal reads BTC's close
> out of a multi-symbol snapshot, and `!pick { symbol: BTC, freq: 1h }`
> disambiguates a cross-frequency snapshot.

## Format: tags, maps, and bare words

The document deserializes into a tree of **externally-tagged enums**: a source,
signal, or strategy node is identified by a single key naming its kind. There are
two equivalent spellings, and you can mix them freely in one file:

| Spelling | Example | Notes |
| --- | --- | --- |
| **YAML tag** | `!sma { source: close, period: 20 }` | the idiomatic form |
| **Map form** | `sma: { source: close, period: 20 }` | a single-key mapping — identical meaning |
| **Bare word** | `close`, `obv` | for a node that takes no parameters |
| **Scalar** | `!value 100`, `!value true` | for a node that wraps a single value |

The tag form and the single-key-map form are interchangeable because the loader
normalizes `!tag value` into `{tag: value}` before typing the document. The bare
word is just the map/tag with no body, used for parameterless leaves (`close`)
and bar indicators (`obv`, `vwap`).

The format is always YAML, in either block or flow (inline) style. JSON is a
subset of YAML, so a JSON-shaped document still parses — it just lands on the map
form throughout, since JSON has no tags: `{"sma": {"source": "close", "period": 20}}`.

### Nesting

Composition *is* nesting. A source's `source`/`lhs`/`rhs` field holds another
source; a logic signal's operands hold other signals. Because each nested node
sits on its own YAML node, it can carry its own tag:

```yaml
enter: !crosses_above
  lhs: !sma { source: close, period: 5 }
  rhs: !sma { source: close, period: 20 }
```

**One caveat:** YAML forbids two tags on a single node, so you cannot write
`!not !below { … }`. For the unary wrappers `!not` and `!changed`, give the inner
signal in map form (or as a nested block):

```yaml
# OK — inner signal in map form
enter: !not { below: { source: !rsi { period: 14 }, level: 30 } }

# also OK — inner signal as a nested block
enter: !not
  below:
    source: !rsi { period: 14 }
    level: 30

# ERROR — two tags on one node
enter: !not !below { source: !rsi { period: 14 }, level: 30 }
```

## Single-asset documents

The default shape (no prefix, or `single:`). A mapping with these fields
(unknown fields are rejected):

| Field | Type | Default | Meaning |
| --- | --- | --- | --- |
| `symbol` | string | — (**required**) | the instrument to trade |
| `long` | side | none | the long entry/exit (see [Sides](#sides)) |
| `short` | side | none | the short entry/exit |
| `sizing` | source | `!value 1.0` | position-size multiplier (see [Sizing](#sizing)) |

The strategy wires up whichever of `long`/`short` you provide; omitting both
yields a strategy that never trades.

### Sides

A side (`long` or `short`) is a mapping with an entry signal and an optional exit:

| Field | Type | Default | Meaning |
| --- | --- | --- | --- |
| `enter` | signal | — (**required**) | open/reverse into the position when this fires |
| `exit` | signal | never fires | flatten the position to flat when this fires |
| `stop_loss` | source | none | a price **level**; flatten when the bar moves adversely through it |
| `take_profit` | source | none | a price **level**; flatten when the bar moves favourably through it |

Entries are all-in and reversal-capable: an opposite-side `enter` reverses an
open position. That is why `exit` **defaults to never-fire** — for an always-in
long/short reversal, the opposite side's `enter` already does the flip, so an
explicit flatten-to-flat exit would be dead. Give a side an `exit` only when you
want a flat rest (long/flat, or long/short with a pause between trades).

#### Protective stops

`stop_loss` and `take_profit` are **price levels** (sources), not signals. For a
long, the `stop_loss` fires when the bar's `low` reaches it and the `take_profit`
when the bar's `high` does (mirrored for a short); the position flattens, filled
at the level — or at the bar's `open` when it gaps past the level (opens already
beyond it). Build the level from the
[position-anchored sources](#position-anchored-sources): `entry` is the entry
price (a fixed stop), `peak` / `trough` the running extreme since entry (a
**trailing** stop, which tracks completed bars and so reacts the bar after a new
extreme). They are checked every bar, so they fire intra-bar, independently of
`enter`/`exit`.

```yaml
# Long on a breakout, with a 5% trailing stop and a fixed 15% take-profit.
symbol: BTC
long:
  enter:       !crosses_above { lhs: close, rhs: !rolling_max { source: high, period: 20 } }
  stop_loss:   !mul { lhs: peak,  rhs: !value 0.95 }   # 5% off the high since entry
  take_profit: !mul { lhs: entry, rhs: !value 1.15 }   # 15% above entry
```

```yaml
# Always-in reversal: no exits needed — each side's enter reverses the other.
symbol: BTC
long:
  enter:  !crosses_above { lhs: !sma { period: 5 }, rhs: !sma { period: 20 } }
short:
  enter:  !crosses_below { lhs: !sma { period: 5 }, rhs: !sma { period: 20 } }
```

```yaml
# Long/flat: an explicit exit returns to flat (no short side).
symbol: AAPL
long:
  enter:  !crosses_above { lhs: !sma { period: 50 }, rhs: !sma { period: 200 } }
  exit:   !crosses_below { lhs: !sma { period: 50 }, rhs: !sma { period: 200 } }
```

#### Trading costs

Costs are configured on the command line — the strategy YAML doesn't spell
them out — so the same strategy can be evaluated against several venue
schedules without editing the spec. See [CLI § `--costs`](CLI.md#--costs)
for the full grammar and the model catalogue; two things worth knowing when
reading a costed `metrics.yml`:

- **Fill pipeline: spread → slippage → commission.** Every fill starts
  from the theoretical trigger price (bar `open` for a market order, the
  trigger level — or the `open` on a gap — for a stop/take-profit), then:
  1. Half-spread is applied — buys pay it, sells receive it.
  2. Slippage is applied — always adverse to the *trading side* (buys slip
     up, sells slip down), regardless of whether the trade opens a fresh
     position or closes an existing one. So a stop-out on a losing short
     doesn't get "free" slippage — the aggressor-side rule matches real
     tape behaviour.
  3. Commission is computed from the *final* price × units and recorded
     separately (`fills.csv`'s `commission` column,
     [`metrics.costs.total_commission`](CLI.md#costs--cost-aggregates)) —
     never netted into `price`.

- **Stop-slippage multiplier.** A triggered stop or take-profit in a fast,
  gapping market realistically slips more than a planned market entry, so
  the two shipped slippage models (`bps`, `volume_participation`) carry a
  `stop_multiplier` field (default `1.5×`) applied on top of the market
  figure when [`OrderKind`](#protective-stops) is `Stop` / `TakeProfit`.
  Set it to `1.0` to model stops as identical to market fills; set it
  higher (`2.0`–`3.0`) for a crypto book with recurrent gap risk.

  This is why costed backtests will typically show a higher cost figure
  for stop-out heavy strategies than a naïve "same slippage for every
  fill" model would — the multiplier is doing its job.

## Sizing

Every shape takes a top-level `sizing:` field — a **source** whose current value
scales the position each entry (or reversal) is opened at. The position is sized
as a fraction of *equity*, and `sizing` is the multiplier on that fraction:
`!value 1.0` (the default) is all-in, `!value 0.5` a fixed half position, and any
real-valued expression makes the size dynamic.

```yaml
symbol: BTC
long:
  enter: !crosses_above { lhs: !sma { period: 5 }, rhs: !sma { period: 20 } }
sizing: !vol_target { target: 0.20, window: 30, bars_per_year: 365 }
```

Sizing is a **magnitude only** — direction comes from the side that entered. It
is read on transitions (not rebalanced mid-position), and it is folded into the
readiness gate, so a strategy waits for its sizing chain to warm up like any
other source. A `None` reading (a source still warming, a division by zero)
**skips the whole trade for that bar** — the safe default. Compose a fallback
into the expression if you'd rather trade through it.

On a pair, both legs are scaled together — each leg enters at half the sized
fraction, so `!value 1.0` is 1.0× gross and dollar-neutral. On a basket, `sizing`
is *per leg* and **not normalized**: an N-leg basket at 100% gross wants
`!equal_weight N`.

### Sizing recipes

Six ready-made tags, usable anywhere a source fits but meant for `sizing:`.
`!equal_weight` is a constant; `!vol_target` and `!atr_risk` read prices; the
last three read the strategy's own equity curve and closed-trade history (its
*book*), so they work on single, pairs, and basket documents alike — on a pair or
basket the book tracks the aggregate equity across all legs.

| Tag | Fields | Meaning |
| --- | --- | --- |
| `!equal_weight` | `<n_legs>` (scalar) | constant `1 / n_legs` — the balanced-basket one-liner |
| `!vol_target` | `{ target, window, bars_per_year }` | inverse realized vol: `target / annualized_stddev(log returns, window)` |
| `!atr_risk` | `{ risk_frac, period, atr_multiple }` | fixed per-trade risk: `risk_frac · close / (atr_multiple · ATR(period))` |
| `!drawdown_throttle` | `{ max_drawdown }` | de-lever linearly as the drawdown deepens; `0` at `max_drawdown`, clamped to `[0, 1]` |
| `!equity_vol_target` | `{ target, window, bars_per_year }` | vol targeting on the strategy's **own** per-bar returns |
| `!fractional_kelly` | `{ kelly_fraction, window }` | Kelly over the last `window` closed-trade returns, scaled by `kelly_fraction`, clamped `>= 0` |

The book-anchored three (`!drawdown_throttle`, `!equity_vol_target`,
`!fractional_kelly`) measure against the book's starting equity — pass
`--cash` to match it to the wallet's starting funds, or their numbers are
meaningless.

## Pairs documents

`pairs:@file.yml` builds a two-leg [`PairsStrategy`](../src/strategies/pairs.rs):
one enter/exit signal pair driving both legs at once. Entering goes **long
`left` / short `right`**, each leg sized at half the [sized](#sizing) fraction of
equity; exiting flattens both.

| Field | Type | Default | Meaning |
| --- | --- | --- | --- |
| `left` | string | — (**required**) | the long leg on entry |
| `right` | string | — (**required**) | the short leg on entry |
| `enter` | signal | — (**required**) | open the pair when this fires |
| `exit` | signal | never fires | flatten both legs |
| `stop_loss` | source | none | a **spread level**; flatten when the spread falls to it |
| `take_profit` | source | none | a **spread level**; flatten when the spread rises to it |
| `sizing` | source | `!value 1.0` | gross-exposure multiplier (see [Sizing](#sizing)) |

Two things distinguish a pairs document from a single-asset one:

- **Every atom-input leaf must be rooted through `!pick`.** A bare `close` uses
  the implicit sole-atom unpack, which panics on a multi-symbol snapshot. Write
  `!close { source: !pick { symbol: BTC } }` — see [Cross-asset sources](#cross-asset-sources).
- **`stop_loss` / `take_profit` are levels on the *spread*, not on a price.** The
  strategy's internal spread is always the raw `close(left) − close(right)` diff,
  so a level expression has to land in those units (`spread_ma − 4·spread_sd`,
  say — not a percentage of an entry price).

```yaml
# Long the BTC−ETH spread when its 60-bar z-score drops below −2σ; close on
# reversion through 0, or on a spread level far outside the band.
left: BTC
right: ETH

enter: !below
  source: &z !div
    lhs: !sub
      lhs: &spread !sub
        lhs: !close { source: !pick { symbol: BTC } }
        rhs: !close { source: !pick { symbol: ETH } }
      rhs: &spread_ma !sma { period: 60, source: *spread }
    rhs: &spread_sd !stddev { period: 60, source: *spread }
  level: -2.0

exit: !above { source: *z, level: 0.0 }

stop_loss: !sub { lhs: *spread_ma, rhs: !mul { lhs: *spread_sd, rhs: !value 4.0 } }
```

See [`examples/pairs.yml`](../examples/pairs.yml) for the annotated version.

## Basket documents

`basket:@file.yml` builds an N-symbol cross-sectional
[`BasketStrategy`](../src/strategies/basket.rs). Each bar it **scores every
symbol**, ranks them, and turns that ranking into a long/short/flat side per
symbol — the classic cross-sectional momentum / value / carry shape.

| Field | Type | Default | Meaning |
| --- | --- | --- | --- |
| `selection` | selection rule | — (**required**) | how ranked scores become sides |
| `score` | source *(template)* | — (**required**) | the per-symbol ranking value |
| `sizing` | source *(template)* | — (**required**) | the per-leg size, as a fraction of equity |
| `universe` | universe rule | *floating* (every symbol seen) | which symbols the basket is willing to trade — see [Universe](#universe) |

**By default the universe is not declared in the file** — it is exactly the set
of symbols the `--series` inputs carry. The basket builds a fresh score and
sizing chain for each symbol the first time it appears, so one document covers a
4-symbol universe and a 40-symbol one unchanged. Symbols missing a bar at some
timestamp simply don't appear in that bar's snapshot, drop out of the ranking,
and rejoin when they resume.

An explicit [`universe:`](#universe) field opts the basket into a declared
symbol list — strict (`!all_of`, errors on absence) or lax (`!any_of`, silently
skips absent / unready).

### `!arg SYM` — the per-symbol placeholder

`score` and `sizing` are **templates**: their tree is captured verbatim at load
and rebuilt once per symbol, with `!arg SYM` resolving to that symbol's name. So
this score…

```yaml
score: !roc
  source: !close { source: !pick { symbol: !arg SYM } }
  periods: 20
```

…becomes `!pick { symbol: BTC }` on BTC's chain, `!pick { symbol: ETH }` on ETH's,
and so on. As in a pair, every atom-input leaf inside `score` / `sizing` must be
rooted through a `!pick` — there's no implicit single-asset root in a multi-symbol
snapshot.

The `!arg` grammar mirrors [`!param`](#parameters--param), and the two are
resolved in different passes (`!param` once at load from `--params`, `!arg` per
symbol at build), so they compose freely inside one tree:

- `!arg SYM` — bare-string shorthand;
- `!arg { key: SYM }` — the same, explicit;
- `!arg { key: SYM, default: BTC }` — with a fallback.

`SYM` is the only argument the basket driver supplies.

### Selection rules

| Tag | Fields | Meaning |
| --- | --- | --- |
| `!top_bottom` | `{ longs, shorts }` | long the `longs` highest scorers, short the `shorts` lowest |
| `!threshold` | `{ long_min, short_max }` | long every score `>= long_min`, short every score `<= short_max` |
| `!quantile` | `{ long_q, short_q }` | long the top `long_q` fraction of the distribution, short the bottom `short_q` |

`!top_bottom` gives a fixed leg count (so `!equal_weight` is exact);
`!threshold` and `!quantile` let the leg count float with the data, so the gross
exposure floats with it too unless the sizing expression compensates.

Symbols that aren't selected are flattened. A symbol keeps its side across bars
if the ranking doesn't change — transitions only fire when the target side
actually differs, so an unchanged selection doesn't churn the wallet.

### Universe

By default the basket is *floating* — it picks up any symbol the `--series`
inputs carry and rolls with typos and gaps. `universe:` opts into a declared
symbol list so a missing name is caught instead of silently trading a smaller
basket:

| Tag | Fields | On absent listed symbol | On unready listed symbol |
| --- | --- | --- | --- |
| `!all_of` | `[sym, sym, …]` | **panics** on the first bar it's missing | `is_ready()` waits — basket skips `trade` until every listed symbol has both scored and sized |
| `!any_of` | `[sym, sym, …]` | silently ignored this bar | silently ignored this bar |

Both tags **filter discovery** to the listed set: symbols outside the universe
never get a per-symbol chain built, and any `--series` input for them is dropped
at the basket boundary (the wallet still marks them, but the basket won't
trade them).

```yaml
universe: !all_of [BTC, ETH, SOL, ADA]   # strict — a missing feed panics
# — or —
universe: !any_of [BTC, ETH, SOL, ADA]   # lax — a missing feed silently skips
```

Use `!all_of` when the universe list is authoritative and a gap means the data
feed is broken; use `!any_of` when the same document should run across
overlapping subsets. Omit the field for the default floating behaviour.

### A complete basket

```yaml
# Cross-sectional momentum: long the 2 strongest, short the 2 weakest,
# equal-weighted at 25% per leg (4 legs = 100% gross).
selection: !top_bottom { longs: 2, shorts: 2 }

score: !roc
  source: !close { source: !pick { symbol: !arg SYM } }
  periods: 20

sizing: !equal_weight 4
```

```sh
fugazi run basket:@basket.yml \
  --series @btc.csv --series @eth.csv --series @sol.csv --series @ada.csv \
  --output-dir out/ --crypto -f 1d
```

Costs stay on the command line and are resolved per symbol, so a scoped
`--costs 'BTC:0.001,ETH:0.0005'` applies per leg — see
[CLI § `--costs`](CLI.md#--costs).

**Not yet wired:** per-side protective levels (a basket has no `stop_loss` /
`take_profit` slot, and the `entry` / `peak` / `trough`
[position sources](#position-anchored-sources-bare-words) always read `None`
inside a `score` / `sizing` expression), and `fugazi optimize` on a basket
document. The [book-anchored sizing recipes](#sizing-recipes) *are* wired, and
read the basket's aggregate equity curve.

See [`examples/basket.yml`](../examples/basket.yml) for the annotated version.

## Sources

A **source** produces a `Real` per bar (`Output = Real`). Any field named
`source`, `lhs`, `rhs`, `high`, or `low` takes one. Where a source has a `source`
field it **defaults to `close`** (and `donchian_*`'s `high`/`low` default to the
`high`/`low` candle fields), so `!sma { period: 20 }` is the SMA of the close.

### Candle-field leaves (bare words)

`close`, `high`, `low`, `open`, `volume`, `typical` (HLC/3), `median` (HL/2).
The whole current bar is `!current` — the default `source:` for every
bar-consuming tag below, and the leaf you name explicitly when composing
cross-timeframe pipelines (`!resample { every: 4, source: !current }`).

Each of them also takes an optional `source:` — the *atom* it reads its fields
out of, which is how a leaf is re-rooted onto another asset
(`!close { source: !pick { symbol: BTC } }`) or another timeframe. Omitted, it
reads the bar of the strategy's own symbol. The same applies to every
calendar leaf and to `!get`.

### Cross-asset sources

`!pick { symbol, freq }` projects one asset's bar out of the multi-symbol
snapshot the CLI feeds each bar. It is the `source:` of an atom-input leaf, not a
source on its own:

```yaml
# The BTC/ETH close spread — the same shape a pairs or basket document uses.
!sub
  lhs: !close { source: !pick { symbol: BTC } }
  rhs: !close { source: !pick { symbol: ETH } }
```

Both fields are optional: `symbol` names the asset, `freq` disambiguates a
cross-frequency snapshot (`!pick { symbol: BTC, freq: 1h }`, the same `N<unit>`
alphabet `--frequency` uses). An empty `!pick {}` — and every leaf that omits
`source:` — unpacks the *sole* entry of the snapshot, which is why single-asset
documents never mention `!pick` at all. That implicit unpack **panics on a
multi-symbol snapshot**, which is the tripwire for a single-asset spec
accidentally pointed at multi-asset input; pairs and basket documents must root
every leaf through an explicit `!pick`.

Anything source-generic composes on top of a pick, not just the candle fields:
`!atr { period: 14, source: !current { source: !pick { symbol: BTC } } }` is
BTC's ATR, `!year { source: !pick { symbol: BTC } }` reads BTC's bar time.

### Constant

`!value <n>` — a constant source. (Tuple form: the scalar is the body, e.g.
`!value 100`.)

### Position-anchored sources (bare words)

`entry`, `peak`, `trough`. These read the **current position**, so they are only
meaningful inside a side's `stop_loss` / `take_profit` (or a custom `exit`):

- `entry` — the price the position was opened at (a fixed stop/target anchor);
- `peak` — the running high since entry (a long trailing-stop anchor);
- `trough` — the running low since entry (a short trailing-stop anchor).

They read as `None` (the level is inactive) while flat, and `peak`/`trough`
restart on each new entry.

### Price-series indicators — `{ source = close, period }`

`!sma`, `!ema`, `!rma` (Wilder/SMMA), `!wma`, `!hma` (Hull), `!rsi`, `!stddev`,
`!cci`, `!stochastic`.

`!stoch_rsi { source = close, rsi_period, stoch_period }` — the stochastic of an
RSI.

### Multi-output indicators — one tag per component

Each line of a multi-output indicator is its own source tag:

| Tags | Fields (`source` defaults to `close`) |
| --- | --- |
| `!macd_line`, `!macd_signal`, `!macd_histogram` | `{ source, fast, slow, signal }` |
| `!bb_upper`, `!bb_middle`, `!bb_lower` | `{ source, period, k }` |
| `!keltner_upper`, `!keltner_middle`, `!keltner_lower` | `{ source, candle_source = !current, ema_period, atr_period, multiplier }` |
| `!donchian_upper`, `!donchian_middle`, `!donchian_lower` | `{ high = high, low = low, period }` |
| `!adx`, `!plus_di`, `!minus_di` | `{ period }` (the ADX/DI components) |
| `!dmi_plus_di`, `!dmi_minus_di` | `{ period }` (raw +DI/−DI, no ADX smoothing) |
| `!aroon_up`, `!aroon_down`, `!aroon_oscillator` | `{ period }` |

### Bar indicators (consume the whole candle)

`!atr { period }`, `!mfi { period }`, `!williams_r { period }`,
`!sar { step, max }`; and the parameterless `!obv`, `!vwap`, `!ad`,
`!true_range` (usable as bare words). Each accepts an optional `source:`
field for the underlying candle stream, defaulting to `!current` — set it
when composing across timeframes (e.g. `!atr { period: 14, source:
!resample { every: 4 } }`). The `!keltner_*` tags likewise take an
optional `candle_source:` for the ATR leg (also defaults to `!current`).

### Calendar sources

Every calendar leaf decomposes the bar's timestamp and emits a `Real`; each takes
the same optional `source:` (an atom source — a `!pick`, typically) as the
candle-field leaves, so bare `!year` reads the strategy's own bar:

`!year`, `!month` (1–12), `!day` (1–31), `!hour` (0–23), `!minute`, `!second`,
`!day_of_week` (ISO: 1 = Monday … 7 = Sunday), `!day_of_year`, `!week_of_year`
(ISO), `!quarter` (1–4), `!unix_seconds`, `!unix_millis`. The raw timestamp
itself is `!time`.

They read `None` when the bar carries no time — synthetic bars, or an
unparseable time label. CSV-loaded and remotely-fetched bars always carry one.
Daily-and-higher bars conventionally sit at 00:00 UTC, so `!hour` / `!minute` /
`!second` are identically `0` there.

Anything beyond a raw field is a composition: "is it Monday" is
`!eq { lhs: !day_of_week, rhs: !value 1 }`, "before the open" is
`!lt { lhs: !hour, rhs: !value 9 }`. The two ready-made calendar *signals* are
`!is_weekday` / `!is_weekend` (see [Signals](#signals)).

### Overlay columns — `!get`

`!get { key, source }` reads one **overlay column** — a non-OHLCV column carried
alongside the bar (an `--series` CSV's extra columns, or a provider's extras like
Binance's `quote_volume` or Yahoo's `adj_close`). The column's declared type in
the stream's schema decides what `!get` builds into:

- a numeric column → a source, usable anywhere a source is (`!sma { source: !get { key: funding_rate }, period: 7 }`);
- a boolean column → a signal, usable directly as an `enter` / `exit` (see [Signals](#signals));
- a string column → a `Str` source, comparable with `!str_eq` / `!str_ne`.

An unknown key, or a type that doesn't fit the position it's used in, is a
build-time error. `source:` re-roots the read on another asset, exactly as on the
candle-field leaves.

### Transforms

| Tags | Fields | Meaning |
| --- | --- | --- |
| `!add`, `!sub`, `!mul`, `!div` | `{ lhs, rhs }` | arithmetic over two sources (`div` → none on /0) |
| `!log` | `{ source = close, base = e }` | logarithm of `source`; `None` on non-positive samples |
| `!lag`, `!diff`, `!ratio`, `!roc` | `{ source = close, periods }` | lookback vs. `periods` bars ago |
| `!rolling_max`, `!rolling_min` | `{ source = close, period }` | rolling extremum over `period` bars |
| `!if_else` | `{ cond, if_true, if_false }` | ternary: `cond` is a **signal**, the branches are sources — see below |
| `!unstable` | `{ source }` | passthrough that reports no unstable period, so the readiness gate stops waiting for this subtree's IIR tail (the signal-side twin is `!unstable { signal }`) |
| `!resample` | `{ every, inner, source = !current }` | aggregate every N candles of `source` (a `Candle`-output stream, defaulting to `!current`) into one higher-timeframe candle and run `inner` (any Real source) over that HTF candle; emits `inner`'s output on each completed bucket and `None` in between. `inner` is **required** — no default |
| `!latch` | `{ source }` | hold the last `Some` output of `source`; `None` before the first arrives |

#### Branching — `!if_else`

The ternary is how a source becomes conditional. `cond` picks between two real
sources:

```yaml
# An ADX-gated momentum score: the ROC when the trend is strong, 0 otherwise.
!if_else
  cond:     !above { source: !adx { period: 14 }, level: 25 }
  if_true:  !roc   { source: close, periods: 20 }
  if_false: !value 0
```

All three sources advance every bar — the branch that didn't fire keeps warming
up rather than stalling. The ternary reads `None` while the condition or the
branch it selects is still warming up (its reported warm-up length is the max
across all three, a safe upper bound for the readiness gate).

#### Cross-timeframe composition — `!resample` + `!latch`

There is no dedicated cross-timeframe tag; compose `!resample` and `!latch`
directly. `!resample { every: N, inner: <source> }` runs `inner` over the
higher-timeframe candle emitted every N base bars — `inner: close` projects
the HTF close, `inner: !ema { period: 20, source: close }` runs an EMA-20
that recurses over HTF closes, and so on. The optional `source:` field
selects the base `Candle` stream `every` reads from (defaults to
`!current`). On base ticks in between, the resample emits `None` and any
recursive smoother inside `inner` naturally does not advance. Wrap the
whole resample in `!latch { source }` so per-base-tick reads see the
finished higher-timeframe value between boundaries.

The **only correct ordering** is resample (with the recursive smoother as its
`inner`) → latch: latching *before* the recursive smoother would feed it a
held (repeated) value on every base tick, distorting the recurrence.

```yaml
# Base bars: 1h. Higher timeframe: 4h. Enter long when the 1h close crosses
# above the EMA-20 computed on 4h candles.
symbol: BTC
long:
  enter: !crosses_above
    lhs: close
    rhs: !latch
      source: !resample
        every: 4
        inner: !ema { period: 20, source: close }
```

**The resample's clock stays base-timeframe.** It's fed one base candle per
tick and reports at that same cadence — the emitted `Option<Real>` marks
*whether* the inner just produced a value on a completed bucket. Warm-up and
unstable-period pass through as raw composition arithmetic — higher-timeframe
sample counts, not base-bar-scaled. For an EMA-P over a resample-`every`
chain, `stable_period() = every + settle_period(P)` (not
`every * (1 + settle_period(P))`); if a strategy needs base-bar-correct
stability accounting, it must feed the pipeline enough leading history for
the recursive tail to decay in HTF-sample terms.

## Signals

A **signal** produces a `bool` per bar. Both sides of a strategy take one as
`enter`/`exit`. A signal reads `false` until every source it depends on has warmed
up, so an edge coinciding with warm-up never fires a spurious first-bar trade.

### Comparisons — `{ lhs, rhs, epsilon? }`

`!gt`, `!lt`, `!ge`, `!le`, `!eq`, `!ne` compare two **sources**. `epsilon` is an
optional absolute tolerance (default `1e-8`) so floating-point noise doesn't cause
spurious flips.

### Threshold comparisons — `{ source = close, level }`

`!above` (`source > level`), `!below` (`source < level`) — compare a source
against a constant, the common case of `!gt`/`!lt` against a number.

### Crossovers — `{ lhs, rhs }`

`!crosses_above`, `!crosses_below` — fire on the bar `lhs` crosses over/under
`rhs` (the comparison is true *and* it just changed). Operands are sources.

### String comparisons — `{ lhs, rhs }`

`!str_eq`, `!str_ne` — compare a string-typed source (in practice a
`!get { key: … }` on a string [overlay column](#overlay-columns--get)) against a
string literal: `!str_eq { lhs: !get { key: session }, rhs: US }`.

### Calendar signals (bare words)

`!is_weekday` (Mon–Fri), `!is_weekend` (Sat/Sun). Both read `false` when the bar
carries no timestamp. Every other calendar predicate is a comparison against a
[calendar source](#calendar-sources) — `!eq { lhs: !day_of_week, rhs: !value 1 }`
for Monday, `!lt { lhs: !hour, rhs: !value 9 }` for a pre-open window.

### Boolean overlay columns

`!get { key }` used in a signal position reads a **boolean** overlay column
directly as a signal (a `Real` or `Str` column there is a build-time error — put
those behind a comparison or `!str_eq` instead). The signal-side form takes only
`key`; it reads the strategy's own asset.

### Boolean logic

| Tag | Form | Meaning |
| --- | --- | --- |
| `!and`, `!or`, `!xor` | `{ lhs, rhs }` | combine two **signals** |
| `!all` | `[ … ]` | AND-fold of a list of signals (empty ⇒ always true) |
| `!any` | `[ … ]` | OR-fold of a list of signals (empty ⇒ always false) |
| `!not` | `<signal>` | negation (see the [nesting caveat](#nesting)) |
| `!changed` | `<signal>` | fires on any transition of the inner signal (the edge primitive) |
| `!unstable` | `{ signal: <signal> }` | passthrough wrapper that forces the reported `unstable_period()` to `0` for the wrapped subtree. Opt-in override of the safe-by-default strategy-readiness gate (which waits for every source's `stable_period()` before allowing a trade). A source-side twin `!unstable { source: <source> }` does the same for real-valued sources. |
| `!value` | `<bool>` | a constant boolean leaf — `!value true` / `!value false` (same tag as the numeric `!value`; typed by position) |

```yaml
# A compound entry: EMA crossover, gated by RSI not being overbought.
enter: !all
  - !crosses_above { lhs: !ema { period: 12 }, rhs: !ema { period: 26 } }
  - !below { source: !rsi { period: 14 }, level: 70 }
```

## Parameters — `!param`

Any value in the document can be a **placeholder** resolved at run time with
`--params`, so one file covers many variations (periods, thresholds, the traded
symbol) without editing:

```yaml
symbol: !param { key: SYM, default: BTC }
long:
  enter: !crosses_above
    lhs: !sma { source: close, period: !param { key: FAST } }              # required
    rhs: !sma { source: close, period: !param { key: SLOW, default: 8 } }  # optional
```

- `!param { key: NAME }` — **required**; a missing value is an error.
- `!param { key: NAME, default: V }` — **optional**; falls back to `V`.
- `!param NAME` — bare-string shorthand for `!param { key: NAME }`.
- Map form: `{ param: { key: NAME, default: V } }`.

Placeholders are substituted on the untyped document *before* it is typed, so a
param can stand in anywhere — including where a number is required.

`--params` is a `,`-separated list of terms, exactly like `--series` (and itself
repeatable): `NAME=value` sets one, `@file.yml` loads a whole
`NAME: value` mapping. Terms apply left-to-right, so a later one wins. A
`NAME=value` value is parsed as a scalar (so `FAST=5` is a number, `SYM=BTC`
a string).

```sh
fugazi run @strategy.params.yml \
  --params @params.yml,FAST=5 \
  --series @candles.csv --output-dir out/
```

`!param`'s sibling is [`!arg`](#arg-sym--the-per-symbol-placeholder), which a
basket document uses to stamp the current symbol into its per-symbol score and
sizing chains. The two resolve in different passes — `!param` once at load, `!arg`
once per symbol at build — so one tree can carry both.

## Reusing signals — YAML anchors

A signal or level that appears in more than one place can be defined once with
a YAML anchor (`&name`) and reused elsewhere with an alias (`*name`). Anchors
are a native YAML feature — the parser inlines each alias with the anchored
subtree before typed deserialization, so the strategy sees exactly the same
tree it would have without the anchors.

The one YAML rule is that `*name` must appear **after** `&name` in the
document. The natural pattern is to attach the anchor at the first use site —
the earliest field that references the subtree — and alias it from every
later site:

```yaml
symbol: BTC
long:
  enter: &cross_up !crosses_above { lhs: !sma { period: 3 }, rhs: !sma { period: 8 } }
  exit:  &cross_dn !crosses_below { lhs: !sma { period: 3 }, rhs: !sma { period: 8 } }
short:
  enter: *cross_dn
  exit:  *cross_up
```

Anchors compose with `!param`: the parser inlines aliases first, so a `!param`
inside an anchored subtree is substituted at every reuse site in the same pass.

## Complete examples

An RSI mean-reversion, long/flat:

```yaml
symbol: BTC
long:
  enter: !crosses_above { lhs: !rsi { period: 14 }, rhs: !value 30 }  # cross up out of oversold
  exit:  !above         { source: !rsi { period: 14 }, level: 70 }    # leave on overbought
```

A Donchian breakout, always-in:

```yaml
symbol: BTC
long:
  enter: !crosses_above { lhs: close, rhs: !donchian_upper { period: 20 } }
short:
  enter: !crosses_below { lhs: close, rhs: !donchian_lower { period: 20 } }
```

The same SMA crossover as a one-line inline (flow-style) spec — tags work inside
flow mappings too, so this is handy as an inline `<STRATEGY>` positional literal
(`fugazi run '…'`):

```yaml
{ symbol: ETH, long: { enter: !crosses_above { lhs: !sma { period: 5 }, rhs: !sma { period: 20 } } } }
```

A quantile basket over whatever universe the series carry — long the top decile
by 60-bar momentum, short the bottom decile, de-levering as the drawdown deepens
(`basket:@…`):

```yaml
selection: !quantile { long_q: 0.1, short_q: 0.1 }
score:  !roc { source: !close { source: !pick { symbol: !arg SYM } }, periods: 60 }
sizing: !drawdown_throttle { max_drawdown: 0.25 }
```

The shipped examples:

| File | Shape | What it shows |
| --- | --- | --- |
| [`examples/strategy.yml`](../examples/strategy.yml) | single | an annotated SMA-crossover, always-in |
| [`examples/strategy.params.yml`](../examples/strategy.params.yml) | single | the same, parameterised with `!param` |
| [`examples/pairs.yml`](../examples/pairs.yml) | pairs | a BTC/ETH spread z-score with spread-level brackets |
| [`examples/basket.yml`](../examples/basket.yml) | basket | cross-sectional momentum, top/bottom-2, equal-weighted |
