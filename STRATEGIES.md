# Strategy files

A **strategy file** is the declarative input to the `fugazi run` backtester. It
describes one [`SingleAssetStrategy`](src/strategies/single_asset.rs): a traded
symbol plus the boolean signals that open and close its long/short positions. The
file is a YAML mirror of the library's composition API — every tag maps
one-to-one to a fugazi constructor — so a strategy you can write in Rust by
nesting constructors you can also write in a file, and vice versa.

```sh
fugazi run @strategy.yml --series @candles.csv --output-dir out/
```

The strategy is the positional argument and follows the `@` convention the data
flags use: `@file.yml` loads a file, anything else is treated as
inline content (handy for one-offs, e.g. `'{ symbol: BTC, long: { enter: !crosses_above { lhs: !sma { period: 3 }, rhs: !sma { period: 8 } } } }'`).

> This document is the syntax reference. For the surrounding CLI (`--series`,
> `--params`, output files, console output) see the
> [Command-line backtester](README.md#command-line-backtester) section of the
> README. For the library API the vocabulary mirrors, see the rest of the README.

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

## Top-level structure

A strategy document is a mapping with these fields (unknown fields are rejected):

| Field | Type | Default | Meaning |
| --- | --- | --- | --- |
| `symbol` | string | — (**required**) | the instrument to trade |
| `long` | side | none | the long entry/exit (see [Sides](#sides)) |
| `short` | side | none | the short entry/exit |

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
at the level (clamped into the bar's range when price gaps clean through). Build
the level from the [position-anchored sources](#position-anchored-sources):
`entry` is the entry price (a fixed stop), `peak` / `trough` the running
extreme since entry (a **trailing** stop). They are checked every bar, so they
fire intra-bar, independently of `enter`/`exit`.

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

## Sources

A **source** produces a `Real` per bar (`Output = Real`). Any field named
`source`, `lhs`, `rhs`, `high`, or `low` takes one. Where a source has a `source`
field it **defaults to `close`** (and `donchian_*`'s `high`/`low` default to the
`high`/`low` candle fields), so `!sma { period: 20 }` is the SMA of the close.

### Candle-field leaves (bare words)

`close`, `high`, `low`, `open`, `volume`, `typical` (HLC/3), `median` (HL/2).

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
| `!keltner_upper`, `!keltner_middle`, `!keltner_lower` | `{ source, ema_period, atr_period, multiplier }` |
| `!donchian_upper`, `!donchian_middle`, `!donchian_lower` | `{ high = high, low = low, period }` |
| `!adx`, `!plus_di`, `!minus_di` | `{ period }` (the ADX/DI components) |
| `!dmi_plus_di`, `!dmi_minus_di` | `{ period }` (raw +DI/−DI, no ADX smoothing) |
| `!aroon_up`, `!aroon_down`, `!aroon_oscillator` | `{ period }` |

### Bar indicators (consume the whole candle)

`!atr { period }`, `!mfi { period }`, `!williams_r { period }`,
`!sar { step, max }`; and the parameterless `!obv`, `!vwap`, `!ad`,
`!true_range` (usable as bare words).

### Transforms

| Tags | Fields | Meaning |
| --- | --- | --- |
| `!add`, `!sub`, `!mul`, `!div` | `{ lhs, rhs }` | arithmetic over two sources (`div` → none on /0) |
| `!lag`, `!diff`, `!ratio`, `!roc` | `{ source = close, periods }` | lookback vs. `periods` bars ago |
| `!rolling_max`, `!rolling_min` | `{ source = close, period }` | rolling extremum over `period` bars |

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

### Boolean logic

| Tag | Form | Meaning |
| --- | --- | --- |
| `!and`, `!or`, `!xor` | `{ lhs, rhs }` | combine two **signals** |
| `!all` | `[ … ]` | AND-fold of a list of signals (empty ⇒ always true) |
| `!any` | `[ … ]` | OR-fold of a list of signals (empty ⇒ always false) |
| `!not` | `<signal>` | negation (see the [nesting caveat](#nesting)) |
| `!changed` | `<signal>` | fires on any transition of the inner signal (the edge primitive) |
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
flow mappings too, so this is handy as a `--strategy '…'` literal:

```yaml
{ symbol: ETH, long: { enter: !crosses_above { lhs: !sma { period: 5 }, rhs: !sma { period: 20 } } } }
```

See [`examples/strategy.yml`](examples/strategy.yml) for an annotated
SMA-crossover strategy and [`examples/strategy.params.yml`](examples/strategy.params.yml)
for its parameterised version.
