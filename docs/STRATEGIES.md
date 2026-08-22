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

## The five strategy shapes

There are five document shapes, picked by an optional **prefix** on the
positional. The prefix decides which document type the YAML is deserialized
into — the expression vocabulary ([Sources](#sources) / [Signals](#signals)) is
identical across all five.

| Prefix | Shape | Document | Traded symbols |
| --- | --- | --- | --- |
| none, or `single:` | [`SingleAssetStrategy`](../src/strategies/single_asset.rs) | [Single-asset](#single-asset-documents) | one, named by `root` |
| `pairs:` | [`PairsStrategy`](../src/strategies/pairs.rs) | [Pairs](#pairs-documents) | two, named by `left` / `right` |
| `basket:` | [`BasketStrategy`](../src/strategies/basket.rs) | [Basket](#basket-documents) | N — cross-sectional rank across the input universe |
| `multi:` | [`MultiAssetStrategy`](../src/strategies/multi_asset.rs) | [Multi-asset](#multi-asset-documents) | N — same signals applied independently per symbol |
| `portfolio:` | [`Portfolio`](../src/portfolio/mod.rs) | [Portfolio](#portfolio-documents) | N — several *different* strategies sharing one account |

The first four all run **one** decision shape. `portfolio:` is the odd one: it
composes several of the others, so reach for it when a portfolio combines
different strategies rather than the same strategy across many symbols.

```sh
fugazi run @strategy.yml         --series @btc.csv --output-dir out/           # single
fugazi run pairs:@spread.yml     --series @btc.csv --series @eth.csv -o out/   # pairs
fugazi run basket:@basket.yml    --series @btc.csv --series @eth.csv \
                                 --series @sol.csv --series @ada.csv -o out/   # basket
fugazi run multi:@independent.yml --series @btc.csv --series @eth.csv \
                                 --series @sol.csv --series @ada.csv -o out/   # multi
fugazi run portfolio:@book.yml   --series @btc.csv --series @eth.csv -o out/   # portfolio
```

`fugazi optimize` accepts the same prefixes, so every shape sweeps and
walk-forwards: `fugazi optimize pairs:@spread.yml -s @btc.csv -s @eth.csv
--grid 'WINDOW=[20,40,60]'`. `fugazi check strategy` validates any of them
without data.

> This document is the syntax reference. For the surrounding CLI (`--series`,
> `--params`, output files, console output) see the
> [Command-line backtester](../README.md#command-line-backtester) section of the
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
and bar indicators (`obv`, `ad`).

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

The four **edge / passthrough wrappers** — `!changed`, `!became_true`,
`!became_false`, `!unstable` — take a third way out: a lone `source:` key,
which is the same thing spelled without the nesting.

```yaml
enter: !became_true                      # keyed — always available
  source: !below { source: !rsi { period: 14 }, level: 30 }

enter: !became_true                      # the same, inner as a map
  below: { source: !rsi { period: 14 }, level: 30 }

exit: !changed day_of_week               # bare word, when the inner needs no tag
```

Both spellings are equivalent and both are reported by `fugazi grammar` (each of
these tags carries two entries in its `forms` list). `!not` has only the map
form — it is a plain newtype, with no `source:` key.

### Parameter defaults

Two rules run through every tag's key list, and `fugazi list indicators` prints
the result of both:

**Order is required-first, then knobs, then the series slot.** A tag declares
its keys the way you would fill them in: whatever has no default, then the
parameters you actually tune, then the `source:` / `candle_source:` / `high:` /
`low:` plumbing that is right by default. `!bb_upper { period, k, source }`,
`!macd_line { fast, slow, signal, source }`, `!above { source, level }`. Tooling
reads the order straight off the grammar descriptor, so completions arrive in
that order too.

**A default is a convention, not a guess.** An indicator published with a
canonical parameterization carries it, so the terse spelling is the textbook
one:

| Tag | Default | Where it comes from |
| --- | --- | --- |
| `!rsi`, `!atr`, `!adx`, `!plus_di`, `!minus_di`, `!dmi_plus_di`, `!dmi_minus_di` | `period: 14` | Wilder |
| `!mfi`, `!williams_r`, `!stochastic`, `!aroon_*` | `period: 14` | the conventional charting default |
| `!cci` | `period: 20` | Lambert's original |
| `!donchian_*` | `period: 20` | the Turtles' 20-day breakout |
| `!bb_*` | `period: 20`, `k: 2.0` | the standard band |
| `!macd_*` | `fast: 12`, `slow: 26`, `signal: 9` | Appel |
| `!keltner_*` | `ema_period: 20`, `atr_period: 10`, `multiplier: 2.0` | the standard channel |
| `!stoch_rsi` | `rsi_period: 14`, `stoch_period: 14` | Chande & Kroll |
| `!sar` | `step: 0.02`, `max: 0.2` | Wilder |
| `!lag`, `!diff`, `!ratio`, `!roc` | `period: 1` | one bar — the first difference |
| `!percentile` | `pct: 0.5` | the median |
| `!variance_ratio` | `lag: 2` | the shortest horizon the ratio is defined over |

Everything else keeps its period **required**, because there is nothing to
default it *to*. `!sma`, `!ema`, `!wma`, `!hma`, `!rma`, `!stddev`, `!zscore`,
`!skewness`, `!kurtosis`, `!percentile_rank`, `!rolling_max`, `!rolling_min`,
`!linreg_*`, `!correlation`, `!covariance`, `!beta`, `!vwap` and the range
volatility estimators all have windows that *are* the modelling decision; a
30 invented here would be a silent one. The same reasoning applies to a
`source:` with no meaningful fallback — see
[Threshold comparisons](#threshold-comparisons--source-level) and the note under
[Transforms](#transforms).

## Metadata — `meta:`

Every fugazi document rejects unknown fields, deliberately: a typo'd `symbl:` or
`rebalance_of:` that silently became a no-op would be a much worse failure than
a rejected load. That leaves nowhere for an external service — a UI, a
scheduler, a strategy registry — to keep its own record next to a strategy it
generated or stores. **`meta:` is that place.**

```yaml
root: BTC/USDT
meta:
  service: strategy-lab
  id: 4f1c-9a2b
  revision: 17
  tags: [momentum, crypto]
  owner: { desk: systematic, contact: quant@example.invalid }
long:
  enter: !gt { lhs: !close, rhs: !sma { period: 20 } }
```

Its contents are **arbitrary and never interpreted**. No key under `meta:` is
reserved, none affects a build, a run, or a metric — adding one cannot change a
backtest. In the other direction, future fugazi fields go at the document root
next to `meta:`, so a service's `meta.tags` can never collide with a `tags:`
fugazi adds later.

It is accepted by all five strategy shapes, by each `children:` entry of a
portfolio, by a [costs document](COSTS.md), and by a `get` dataset file. Read it
back with `StrategySpec::meta()` in Rust or `spec.meta` in [Python](PYTHON.md);
a document that omits it reads `None`.

Three things worth knowing:

- **On a preset, `meta:` goes *inside* the tag.** A preset document *is* the tag
  (`!buy_and_hold { … }`), so there is no sibling position for a second key —
  write `!buy_and_hold { root: BTC, meta: { … } }`. A sibling `meta:` would
  stop the document being recognized as a preset at all.
- **A portfolio child has two of them.** `meta:` on the child entry describes
  the *slot* (why this child is in this portfolio); `meta:` inside its
  `strategy:` belongs to the nested document. Unlike `name` / `group`, neither is
  surfaced to the `weights:` expression — `meta` is opaque by contract, and a
  weight that read it would be reading data fugazi promises not to interpret.
- **An overlay column file is the exception — it takes no `meta:`.** That
  document has no envelope: every key *is* a column name, so a `meta:` field
  could only be carved out of the column namespace, taking the name away from
  anyone already using it. `meta` there stays an ordinary column. Metadata about
  a set of overlay columns belongs on the dataset file that declares them.

`meta:` rides the same load pipeline as the rest of the document, so
`!import` and [`!param`](#parameters--param) resolve inside it —
handy for sharing one metadata block across a family of strategies
(`meta: !import shared-meta.yml`). The flip side: a *literal* single-key map
spelled `{param: …}`, `{import: …}`, `{arg: …}` or `{undefined: …}` inside
`meta:` is read as a placeholder rather than as data. Nest external data one
level under a vendor key and the question never comes up.

## Single-asset documents

The default shape (no prefix, or `single:`). A mapping with these fields
(unknown fields are rejected):

| Field | Type | Default | Meaning |
| --- | --- | --- | --- |
| `root` | source (atom) | — (**required**) | the **evaluation root**: the series every `source:`-omitted leaf reads, and the instrument this document trades. `root: BTCUSDT` is sugar for `root: !pick { symbol: BTCUSDT }`. An expression like any other slot, so `!param` reaches it (see [Parameterizing the root](#parameterizing-the-root)) |
| `long` | side | none | the long entry/exit (see [Sides](#sides)) |
| `short` | side | none | the short entry/exit |
| `sizing` | source | `!value 1.0` | position-size multiplier (see [Sizing](#sizing)) |
| `rebalance_on` | signal | `!never` | resize the open position when this fires (see [Rebalance](#rebalance)) |
| `meta` | any | none | free-form metadata, never interpreted — see [Metadata](#metadata--meta) |

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
root: BTC
long:
  # `!lag` is load-bearing: the raw channel includes today's bar, so `close`
  # can never exceed it. See "Extremum sources include the current bar".
  enter:       !crosses_above
    lhs: close
    rhs: !lag { source: !rolling_max { source: high, period: 20 }, period: 1 }
  stop_loss:   !mul { lhs: peak,  rhs: !value 0.95 }   # 5% off the high since entry
  take_profit: !mul { lhs: entry, rhs: !value 1.15 }   # 15% above entry
```

```yaml
# Always-in reversal: no exits needed — each side's enter reverses the other.
root: BTC
long:
  enter:  !crosses_above { lhs: !sma { period: 5 }, rhs: !sma { period: 20 } }
short:
  enter:  !crosses_below { lhs: !sma { period: 5 }, rhs: !sma { period: 20 } }
```

```yaml
# Long/flat: an explicit exit returns to flat (no short side).
root: AAPL
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
root: BTC
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
`!equal_weight N`. (Normalizing gross is a separate question from balancing the
long side against the short one, which a basket does do by default — see
[Balancing the two sides](#balancing-the-two-sides).)

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
| `!equity_vol_target` | `{ target, window, bars_per_year, seed = 1.0 }` | vol targeting on the strategy's **own** per-bar returns |
| `!fractional_kelly` | `{ kelly_fraction, window, seed = 1.0 }` | Kelly over the last `window` closed-trade returns, scaled by `kelly_fraction`, clamped `>= 0` |

The book-anchored three (`!drawdown_throttle`, `!equity_vol_target`,
`!fractional_kelly`) measure against the book's starting equity — pass
`--cash` to match it to the wallet's starting funds, or their numbers are
meaningless.

#### `seed:` — how a self-referential sizer starts

`!equity_vol_target` and `!fractional_kelly` size on something that only exists
*because* the strategy already traded: a moving equity curve, and closed
trades. Everywhere else in fugazi a source that isn't ready yet reads `None`
and the strategy waits — but a sizing slot reading `None` **skips the trade**,
so here waiting is a deadlock. No entry ⇒ no trade ⇒ no sample ⇒ no entry.
Both recipes used to report zero fills on every shape with no warning.

`seed:` is the size to use until the recipe can size itself — the base stake
you would start at and then scale. It defaults to `1.0` (full size), and stops
applying the moment the recipe has an answer of its own, including an answer of
`0` ("no edge, stand down"). It is not a floor and not a clamp.

```yaml
sizing: !fractional_kelly
  kelly_fraction: 0.5
  window: 30
  seed: 0.25      # quarter size for the first 30 trades, then Kelly's own number
```

`seed: 0.0` restores the never-bootstraps behaviour, if a strategy is composed
so that something *else* opens the first trades.

The other recipes need no seed: `!vol_target` and `!atr_risk` read prices (which
arrive without trading), and `!drawdown_throttle` reads a drawdown that is
well-defined at zero.

### Choosing a sizing method from the command line

To compare sizing methods without maintaining a file per method, dispatch on a
`!param` with `!match`. Because `!param` resolves at *load* time and substitutes
whatever value it is handed — a scalar, or a whole tagged subtree — the `on:`
operand collapses to a constant before the strategy is ever built, and `!match`
then selects the branch:

```yaml
sizing: !match
  on: !value { param: { key: SIZING_MODE, default: vol_target } }
  cases:
    - when: vol_target
      value: !vol_target { target: 0.20, window: 30, bars_per_year: 365 }
    - when: atr_risk
      value: !atr_risk { risk_frac: 0.01, period: 14, atr_multiple: 2.0 }
    - when: kelly
      value: !fractional_kelly { kelly_fraction: 0.5, window: 30 }
  default: !value 1.0
```

```sh
fugazi run      @strategy.yml --params SIZING_MODE=atr_risk
fugazi optimize @strategy.yml --grid   'SIZING_MODE=["vol_target","atr_risk","kelly"]'
```

The `optimize` form is the useful one: a `--grid` axis is normally limited to
scalars, but here the scalar selects a whole *structure*, so one sweep ranks the
sizing methods against each other and the CSV carries a `SIZING_MODE` column
like any other axis.

Two caveats:

- **Every branch is built and advanced, not just the selected one.** So the
  readiness gate takes the **max `stable_bars` across all branches** — a run
  that selects a cheap branch still waits for the slowest one to warm up. On a
  short series that can suppress trading entirely: a `!value 1.0` branch sitting
  next to a `!vol_target { window: 10 }` branch inherits the latter's warm-up.
  For a comparison sweep this is arguably the right behaviour, since every
  variant then starts on the same bar.
- **An unmatched value falls through to `default:` silently**, so a typo in
  `--params` produces a valid run at the default sizing rather than an error.
  Give `default:` a value you would notice (`!value 0.0` never trades).

A `!param` whose value is a whole subtree is the other half of this: put the
alternatives in files and select one with `--params @sizings/atr.yml`, since a
`--params @file.yml` is parsed through the YAML tag converter and can therefore
carry a full `!vol_target { ... }` node.

## Pairs documents

`pairs:@file.yml` builds a two-leg [`PairsStrategy`](../src/strategies/pairs.rs).
The traded instrument is the **spread**, `close(left) − close(right)`, and the
strategy is long / flat / short on it:

| Direction | Legs | Profits when |
| --- | --- | --- |
| **long spread** | long `left`, short `right` | the spread rises |
| **short spread** | short `left`, long `right` | the spread falls |

Each leg is sized at half the [sized](#sizing) fraction of equity; an exit
flattens both.

| Field | Type | Default | Meaning |
| --- | --- | --- | --- |
| `left` | string | — (**required**) | the leg bought when long the spread |
| `right` | string | — (**required**) | the leg sold when long the spread |
| `long_spread` | side block | none | the long-spread side: `{ enter, exit, stop_loss, take_profit }` |
| `short_spread` | side block | none | the short-spread side, same shape |
| `enter` | signal | none | flat spelling of `long_spread.enter` |
| `exit` | signal | never fires | flat spelling of `long_spread.exit` |
| `stop_loss` | source | none | flat spelling of `long_spread.stop_loss` |
| `take_profit` | source | none | flat spelling of `long_spread.take_profit` |
| `sizing` | source | `!value 1.0` | gross-exposure multiplier (see [Sizing](#sizing)) |
| `rebalance_on` | signal | `!never` | resize both legs when this fires (see [Rebalance](#rebalance)) |
| `meta` | any | none | free-form metadata, never interpreted — see [Metadata](#metadata--meta) |

At least one side must be wired. The flat top-level keys are a shorthand for the
long-spread side, so every pre-existing pairs document keeps working unchanged;
setting both them and a `long_spread:` block is an error.

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

### Trading both tails

The document above is **long-spread only**: it acts when the spread is unusually
cheap and bets it rises. But a mean-reverting spread visits both tails, and the
correct position is *opposite* at each — at a rich spread you short it. Which leg
does the converging is irrelevant (that is what the hedge removes), but the sign
is not optional, so a one-directional document silently skips every excursion on
the other side of the mean.

Add a `short_spread:` block to pick those up. There is no flag to enable it —
wiring the block is the switch, exactly as `short:` is on a single-asset
document:

```yaml
left: BTC
right: ETH

long_spread:                     # spread cheap → expect it to rise
  enter: !below { source: *z, level: -2.0 }
  exit:  !above { source: *z, level:  0.0 }
  stop_loss: !sub { lhs: *spread_ma, rhs: !mul { lhs: *spread_sd, rhs: !value 4.0 } }

short_spread:                    # spread rich → expect it to fall
  enter: !above { source: *z, level:  2.0 }
  exit:  !below { source: *z, level:  0.0 }
  stop_loss: !add { lhs: *spread_ma, rhs: !mul { lhs: *spread_sd, rhs: !value 4.0 } }
```

Three things to know:

- **Each side needs its own conditions.** A signal is an opaque `bool` — the
  engine sees `true`, not "the z-score is −2.3" — so it cannot derive one side's
  entry by negating the other's. Write the mirror condition explicitly.
- **The short side's levels compare with mirrored sense.** It profits as the
  spread falls, so its `stop_loss` fires when the spread rises *to or above* the
  level and its `take_profit` when the spread falls *to or below* it. Note the
  `!add` above where the long side has `!sub`.
- **The two directions are mutually exclusive in time.** They are inverse
  positions — held together they would net flat — so they share one capital pool
  at full notional rather than splitting it, and the opposite side's `enter`
  firing while a pair is open **reverses** it in one order per leg.

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
| `balance_sides` | bool | `true` | equalize long and short gross — see [Balancing the two sides](#balancing-the-two-sides) |
| `rebalance_on` | signal | `!every 1` (every bar) | re-rank + resize when this fires (see [Rebalance](#rebalance)) |
| `meta` | any | none | free-form metadata, never interpreted — see [Metadata](#metadata--meta) |

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
  period: 20
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

**Only the value is deferred, not the shape.** A template body is typed-parsed
at load with each `!arg` held as a placeholder, so a misspelled tag or field
inside `score:` / `sizing:` (or a multi-asset side's `enter:`, or a portfolio's
`weights:`) is a parse error like any other — reported by `fugazi check`, by
`fugazi run` before the first bar, and by `load_spec` in Python. What the parse
can't decide is left to the build, which happens up front too: each template is
built once against a stand-in symbol when the strategy is constructed, so an
unknown `!get` column or a mistyped slot fails at start-up rather than on the
first bar that quotes a symbol.

### Selection rules

| Tag | Fields | Meaning |
| --- | --- | --- |
| `!top_bottom` | `{ longs, shorts }` | long the `longs` highest scorers, short the `shorts` lowest |
| `!threshold` | `{ long_min, short_max }` | long every score `>= long_min`, short every score `<= short_max` |
| `!quantile` | `{ long_q, short_q }` | long the top `long_q` fraction of the distribution, short the bottom `short_q` |
| `!everything` | — | the leaf: every scored symbol is eligible for either side. The implicit default, rarely written out |

All three ranking rules also take an optional **`of:`** — the candidate set they
rank *within*, defaulting to `!everything`. That is what makes them compose:

```yaml
# of the symbols scoring at least 0.5 (or at most -0.5), take the 2 best and 2 worst
selection: !top_bottom
  longs: 2
  shorts: 2
  of: !threshold { long_min: 0.5, short_max: -0.5 }
```

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
  period: 20

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

### Balancing the two sides

`sizing:` is per leg, so a basket whose two sides hold different numbers of legs
— or whose legs are sized unequally by `!vol_target` / `!atr_risk` — ends up with
more gross on one side than the other. `!top_bottom { longs: 2, shorts: 1 }` at a
flat `!value 0.5` is 1.0× gross long against 0.5× gross short: a **net +0.5×
long position that the ranking never asked for**. A market-wide rally shows up in
the P&L whether or not the longs actually outranked the shorts, which is the one
thing a cross-sectional strategy is trying not to measure.

`balance_sides:` (default `true`) removes it. At each rebalance the two sides'
target sizes are summed, the **smaller** sum becomes the target gross-per-side,
and each side is scaled to meet it. In the example above the longs drop from 0.5
to 0.25 each, so both sides carry 0.5× and the net is flat. Taking the smaller
side means balancing only ever **deleverages** — it never levers the small side
up to meet the big one, so turning it on cannot increase your exposure.

```yaml
balance_sides: false   # keep the raw per-leg sizes, net exposure and all
```

Set it `false` when the net exposure is the point — a long-biased basket that
shorts only a small hedge, say — or when you are sizing the two sides against
each other yourself in the `sizing:` expression.

Two things it does **not** do. **A one-sided selection passes through
unscaled**: with no shorts there is no counter-side to balance against, so a
long-only basket (`!top_bottom { longs: 5, shorts: 0 }`, or a `!threshold` whose
cutoffs happen to admit one side on some bars) trades exactly as it would with
the flag off. Balancing never blocks a trade. And it equalizes *intent at
rebalance*, not realized notional every bar — like all basket sizing it is read
on transition, so an already-open leg is not resized and the balance drifts with
price until the next turnover.

### Per-leg protective levels

`long:` and `short:` blocks carry `stop_loss` / `take_profit` templates applied
to **each leg on that side**, anchored to that symbol's own position — so
`!entry`, `!peak` and `!trough` mean what they do on a `single:` document:

```yaml
long:
  stop_loss: !mul { lhs: !peak, rhs: !value 0.9 }        # 10% trailing stop
short:
  take_profit: !mul { lhs: !entry, rhs: !value 0.95 }
```

These blocks take *only* the two protective keys — a basket's entries and exits
come from its `selection` rule, not from `enter` / `exit` signals.

One thing that genuinely doesn't work: `!entry` / `!peak` / `!trough`
([position sources](#position-anchored-sources-bare-words)) always read `None`
inside a `score` or `sizing` expression, which are evaluated before any position
exists. The [book-anchored sizing recipes](#sizing-recipes) *are* available
there, and read the basket's aggregate equity curve.

See [`examples/basket.yml`](../examples/basket.yml) for the annotated version.

## Multi-asset documents

`multi:@file.yml` builds an N-symbol **independent** portfolio
([`MultiAssetStrategy`](../src/strategies/multi_asset.rs)): every symbol
runs the same [`SingleAssetStrategy`](#single-asset-documents)-shaped
decision in isolation — the same entry / exit signals, the same
protective levels, the same sizing rule — and any subset of them can be
long / short / flat at once. Where a [`basket:`](#basket-documents) is
*cross-sectional* (a symbol trades because it ranks against the
others), a `multi:` is *independent* (a symbol trades because *its own*
signals fired).

| Field | Type | Default | Meaning |
| --- | --- | --- | --- |
| `long` | side | omitted | the long side — `enter` + optional `exit` / `stop_loss` / `take_profit`, each templated by `!arg SYM` |
| `short` | side | omitted | the short side, same shape as `long` |
| `sizing` | source *(template)* | `!value 1` (all-in per leg) | the per-leg size, as a fraction of equity |
| `universe` | universe rule | *floating* (every symbol in the series) | which symbols the portfolio is willing to trade — see [Universe](#universe) (shared with `basket:`) |
| `rebalance_on` | signal | `!never` | resize every held position when this fires (see [Rebalance](#rebalance)) |
| `meta` | any | none | free-form metadata, never interpreted — see [Metadata](#metadata--meta) |

`long` and `short` mirror the [single-asset side](#single-asset-documents)
grammar exactly (`enter`, `exit`, `stop_loss`, `take_profit`); the
difference is that every subtree is a **template** that gets rebuilt per
symbol with `!arg SYM` substituted, same as `score` / `sizing` in a
basket. Every atom-input leaf inside must be rooted through
`!pick { symbol: !arg SYM }`, since a multi-asset snapshot has no
implicit "sole atom" to unpack.

### A complete multi-asset portfolio

```yaml
# Same MA-crossover applied per-symbol, independent per leg.
# Equal-weighted at 25% per leg (4 legs = 100% gross).
long:
  enter: !crosses_above
    lhs: !sma { source: !close { source: !pick { symbol: !arg SYM } }, period: 5 }
    rhs: !sma { source: !close { source: !pick { symbol: !arg SYM } }, period: 20 }
  exit: !crosses_below
    lhs: !sma { source: !close { source: !pick { symbol: !arg SYM } }, period: 5 }
    rhs: !sma { source: !close { source: !pick { symbol: !arg SYM } }, period: 20 }
  stop_loss: !mul
    lhs: !entry
    rhs: !value 0.95
short:
  enter: !crosses_below
    lhs: !sma { source: !close { source: !pick { symbol: !arg SYM } }, period: 5 }
    rhs: !sma { source: !close { source: !pick { symbol: !arg SYM } }, period: 20 }
  exit: !crosses_above
    lhs: !sma { source: !close { source: !pick { symbol: !arg SYM } }, period: 5 }
    rhs: !sma { source: !close { source: !pick { symbol: !arg SYM } }, period: 20 }
sizing: !equal_weight 4
universe: !all_of [BTC, ETH, SOL, ADA]
```

```sh
fugazi run multi:@portfolio.yml \
  --series @btc.csv --series @eth.csv --series @sol.csv --series @ada.csv \
  --output-dir out/ --crypto -f 1d
```

Protective levels (`stop_loss` / `take_profit`) inside a `long` /
`short` side see the **per-symbol** [`Position`](#position-anchored-sources-bare-words)
— `!entry`, `!peak`, `!trough` compose into per-leg trailing stops
exactly as on `single:`.

## Portfolio documents

`portfolio:@file.yml` runs **N different strategies side by side on one
account**, behind a single aggregate equity curve and blotter. The other four
shapes each run a single decision rule; this one composes them.

One account is deliberate: it is what a real deployment has, so the same
document backtests and trades live (see [How capital moves](#how-capital-moves)).

Reach for it when the strategies differ. If you want the *same* rule across many
symbols, `multi:` is smaller and cheaper; if you want a cross-sectional rank,
`basket:` is.

| Key | Type | Default | Meaning |
| --- | --- | --- | --- |
| `children` | list | *required, non-empty* | the child strategies, in order |
| `weights` | expression | equal (`1/N`) | how capital is split — see [Weights](#weights) |
| `rebalance_on` | signal | `!never` | when to pull capital back to target — see [Rebalance](#rebalance) |
| `rebalance_policy` | `!proportional` \| `!largest_first` | `!proportional` | how positions are scaled down when cash alone can't fund a rebalance |
| `meta` | any | none | free-form metadata, never interpreted — see [Metadata](#metadata--meta) |

Each child is `{ name?, group?, meta?, strategy }`, where `strategy` is **any of
the other four document shapes**, routed by its distinctive top-level key
(`left`+`right` → pairs, `selection` → basket, `symbol` or a preset tag →
single, otherwise multi). The child's `meta` describes the *slot*; a `meta:`
inside its `strategy:` belongs to the nested document — see
[Metadata](#metadata--meta):

```yaml
children:
  - name: trend
    group: momentum
    strategy: !ma_crossover { root: BTC, fast: 10, slow: 30 }
  - name: spread
    strategy:
      left: BTC
      right: ETH
      enter: !lt { lhs: !zscore { source: !close { source: !pick { symbol: BTC } }, period: 20 }, rhs: !value -2 }
```

`name` is optional but must be unique (it defaults to `child_<index>`); it names
the child in errors and is available to weight expressions as `!arg CHILD_NAME`.
`group` is a free-form label, available as `!arg CHILD_GROUP` — the natural
dispatch key for "up-weight every momentum child when ADX is high".

To reuse one child spec several times with different parameters, use
`!import { path, params }` rather than repeating it: each import resolves its
own `!param`s, so the names don't collide.

### Weights

`weights:` is a single expression, instantiated **once per child** and read at
every rebalance-fire; the portfolio normalizes `w_i = N_i / Σ N_j`, so weights
are magnitudes and needn't sum to 1.

```yaml
weights: !value [0.6, 0.4]      # fixed per-child weights (child i reads w_i)
weights: !value 1.0             # any per-child constant → equal weight
weights: !fractional_kelly { kelly_fraction: 0.5, window: 30 }   # each child's own book
weights: !drawdown_throttle { source: !portfolio_book, max_drawdown: 0.15 }  # aggregate
```

The last two are the point of making this an expression rather than a fixed
policy: a bare book-reading node (`!drawdown`, `!fractional_kelly`, …) reads
**that child's own book**, so each child is weighted by its own record; adding
`source: !portfolio_book` reads the **aggregate** instead, so one gate dials
every child down together. `!fixed [...]` and `!equal_weight` are accepted as
sugar for the `!value` forms.

Per-child instantiation injects `!arg CHILD_INDEX` always, `!arg CHILD_NAME` /
`!arg CHILD_GROUP` when the child declared them, and `!arg SYM` for single-asset
children. Referencing an arg the child didn't declare is a build error.

**A non-constant `weights:` requires a `rebalance_on:`.** The expression is read
only inside a rebalance cycle, so a portfolio that never fires its gate would
build the chains, update them on every bar, and consult them on none — running
the equal-split seed and drifting with P&L, its weighting rule inert. That is
refused at build rather than reported as a backtest. Give it a cadence
(`rebalance_on: !every 28`), or write `rebalance_on: !never` to say the drift is
intended. The two constant forms are exempt: `!value [0.6, 0.4]` (and its
`!fixed` sugar) seeds the ratio at build, and `!value 1.0` (`!equal_weight`)
seeds `1/N`, so for those the seed already *is* the expression's answer.

### How capital moves

The portfolio trades **one account**. Each child owns a *ledger* rather than a
wallet — bookkeeping recording which slice of the account's cash and positions
belongs to it — and sizes against **its own** ledger, so `value_frac(1.0)` in a
child still means all of *that child's* capital, not the portfolio's. Fills are
attributed back to the children that caused them at the real fill price, so
per-child P&L is built from actual executions rather than simulated ones.

Each bar the portfolio combines every child's intent into **one order per
symbol**. Three consequences are worth knowing before you rely on the numbers:

- **Children trading one symbol in opposite directions cross internally.** Only
  the imbalance reaches the market; the offsetting part settles at the bar's
  open and pays no spread or commission, because it never traded. A portfolio
  whose children constantly trade against each other will look slightly better
  than it would live.
- **A child's stop takes off only that child's share** — but the account holds
  one resting bracket per symbol, so when several children want a stop on the
  same symbol, the one nearest to triggering is the one that rests. That is the
  one that would fire first anyway; if two would be hit on the same bar, the
  second exits a bar later.
- **A child cannot spend past its own slice**, even when a sibling is sitting on
  idle cash. The refusal appears in the run's rejection banner exactly as it
  would for a standalone strategy.

Resting limit entries are not available inside a portfolio: a limit has no owner
while it rests, so it cannot be netted, and a child asking for one is told so
rather than having the eventual fill guessed at.

Without `rebalance_on:` the split drifts with P&L, which is usually what you
want. When it fires, moving cash between children is pure bookkeeping — the
account balance never moves, only the notional split of it — so it costs
nothing and generates no orders. Only a child too *invested* to reach its
target (rather than merely cash-poor) needs positions scaled down
(`!proportional` shrinks every position uniformly; `!largest_first` closes the
biggest ones first); that lands as ordinary flow on the next bar, so such a
portfolio takes an extra fire to converge.

One caveat worth knowing: a child that hard-targets 100% invested (a naked
`!buy_and_hold`) will simply re-enter on the next bar and undo the rebalance.
Book-anchored sizing recipes respect the post-rebalance equity naturally.

### A complete portfolio

```yaml
# book.yml — trend-follow BTC, mean-revert the BTC/ETH spread, monthly rebalance
weights: !value [0.7, 0.3]
rebalance_on: !every 28
rebalance_policy: !proportional
children:
  - name: btc_trend
    strategy:
      root: BTC
      long:
        enter: !crosses_above { lhs: !sma { period: 10 }, rhs: !sma { period: 30 } }
        exit:  !crosses_below { lhs: !sma { period: 10 }, rhs: !sma { period: 30 } }
      sizing: !vol_target { target: 0.20, window: 20, bars_per_year: 365 }

  - name: btc_eth_spread
    strategy:
      left: BTC
      right: ETH
      long_spread:
        enter: !lt { lhs: !zscore { source: !close { source: !pick { symbol: BTC } }, period: 20 }, rhs: !value -2 }
        exit:  !gt { lhs: !zscore { source: !close { source: !pick { symbol: BTC } }, period: 20 }, rhs: !value 0 }
```

```sh
fugazi run portfolio:@book.yml --series @btc.csv --series @eth.csv \
                               --cash 100000 --output-dir out/
```

Two notes on writing expressions at portfolio scope. A **`rebalance_on:` gate
spans every child**, so there is no "this series" for a bare `!close` to mean —
use cadence or calendar signals (`!every`, `!monthly`), or name the asset with
`!pick { symbol: … }`. And `!entry` / `!peak` / `!trough` inside a
portfolio-level gate read an empty dummy position, since the portfolio has no
position of its own.

## Rebalance

Every strategy shape (`single:`, `pairs:`, `basket:`, `multi:`, `portfolio:`)
exposes an optional top-level `rebalance_on:` field: a **boolean signal** that
decides, on each bar, whether the strategy re-runs its sizing/selection
step. `sizing:` answers "what size?"; `rebalance_on:` answers "act on
that size *right now*?".

What "rebalance" means depends on the strategy shape:

| Strategy | On `rebalance_on` fire | Every bar (regardless of gate) |
| --- | --- | --- |
| `single:` | resize the open position to the current sizing target | entry / exit signals fire, protective levels rest |
| `pairs:` | resize both legs to the current sizing target | enter / exit spread signals fire, spread levels rest |
| `multi:` | resize every held per-symbol position to its sizing target | per-symbol entry / exit signals fire |
| `basket:` | re-run selection **and** resize | (nothing else — basket has no per-symbol entry / exit) |
| `portfolio:` | pull each child's cash slice back to its weight target (cash phase, then a position phase for anyone too invested to cover it) | every child trades its own slice |

Basket is the odd one because a cross-sectional ranker's *target set* is
itself the sizing decision — so gating selection and gating resize are
the same act. The others cleanly separate entry / exit (bar-driven
signals) from sizing (rebalance-driven).

### Defaults

| Strategy | Default `rebalance_on` | Rationale |
| --- | --- | --- |
| `single:` / `multi:` | `!never` | the gate reaches only the *resize* branch — entries, exits and protective levels fire regardless. Firing by default would make every strategy a constant-fraction rebalancer, paying turnover nobody asked for |
| `pairs:` | `!never` | the gate re-hedges both legs to equal notional. As a spread widens that adds to the losing leg, and equal notional isn't the right hedge ratio anyway — maintaining the wrong ratio continuously is worse than visible drift |
| `basket:` | `!every 1` (every bar) | the gate wraps *selection*, not just resize, so `!never` is a basket that never trades. Every periodic alternative is arbitrary — a bar count means a different horizon per cadence. "Rank and hold the top N" with no schedule stated means re-rank every bar |
| `portfolio:` | `!never` | the cash split drifts with per-child P&L, which is usually what you want; a rebalance is a real trading decision. A non-constant `weights:` must state a cadence — see [Weights](#weights) |

Omit the field to get the default. Set `!never` to opt out of
rebalancing entirely; set `!every N` for a periodic pulse (`!every 5`
for weekly on a daily strategy, `!every 20` for ~monthly). Any other
boolean signal works too — compose with drawdown, weight-drift, or
calendar signals to trigger event-driven rebalancing.

### Cadence signals

| Tag | Fires |
| --- | --- |
| `!never` | never (sugar for `!value false`) — **not** what omitting the field means; see [Defaults](#defaults) |
| `!every N` | on bar `N-1` (0-indexed), then every `N` bars — delayed first fire so `!every 5` at end of every 5-bar block |
| `!value true` / `!value false` | constants — for programmatic overrides |
| composite: `!and`, `!or`, `!xor`, `!not`, `!all`, `!any` | boolean logic over any of the above and any other signal |
| calendar / drawdown / crossover / … | any [Signal](#signals) works |

### Between rebalances

A gated strategy holds "stale" state between rebalance events. For
`basket:` under `!every 20`, a symbol whose score drops out of the
selection between rebalance bars stays in the position until the next
rebalance fires. That's the desired behavior for periodic rebalancing —
but if you also want drift protection between rebalances, compose the
gate: `!or [!every 20, !above { source: !drawdown, level: 0.1 }]`.

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
`source:` — resolves to the context's **blessed series**: the document's own
`root:` in a single-asset spec, the leg's symbol in a basket or multi-asset
one. A pairs document has no blessed series — two legs, neither privileged — so
it must root every leaf through an explicit `!pick`, and so must a portfolio's
`weights:` and any `rebalance_on:`.

Anything source-generic composes on top of a pick, not just the candle fields:
`!atr { period: 14, source: !current { source: !pick { symbol: BTC } } }` is
BTC's ATR, `!year { source: !pick { symbol: BTC } }` reads BTC's bar time.

#### Parameterizing the root

`root:` is an expression slot, so `!param` reaches into it like any other. That
is what lets **one document be swept over instruments**:

```yaml
root: !pick { symbol: !param { key: SYM, default: BTC } }
long:
  enter: !gt { lhs: !close, rhs: !sma { period: 20 } }
```

```console
$ fugazi run @strategy.yml -s @prices.csv --params SYM=ETH
$ fugazi optimize @strategy.yml -s @prices.csv --grid 'SYM=["BTC","ETH"]' --grid FAST=5..20:5
```

`optimize` prepares **one bar stream per distinct traded series** and each row
evaluates its own, so a row's metrics equal what the same document produces on
its own through `run`. Two consequences to know:

- **A root axis is a batch, not a sweep.** Rows no longer evaluate the same bars,
  so they are separate backtests rather than a like-for-like comparison of
  parameters. `optimize` warns when it sees one.
- **`--walkforward` refuses a root axis**, because a fold index would span a
  different period per row. Sweep the root or walk forward, not both. A `pairs:`
  grid also still refuses one: a pairs run evaluates the *inner join* of its two
  legs, so a different pair is a different timeline.

A root that names no symbol, or more than one, is a **build error** — `check`
reports it before any bar is read:

```console
$ fugazi check strategy @broken.yml
  status  error · root `root:` names no symbol, so there is nothing to trade or
          to slice the input by — name one, e.g. `root: !pick { symbol: BTCUSDT }`
```

`root:` may also declare the bar cadence, which joins the resolution chain one
rung below the CLI flag — `-f/--frequency` → `root:`'s `freq` → the input's
`freq` column → detection from the timestamps:

```yaml
root: !pick { symbol: BTCUSDT, freq: 4h }
```

#### Reading an asset you do not trade

**Any shape may `!pick` any symbol in the input**, including one it never
trades. This is the regime-gate shape — trade one asset only while another is in
a given state:

```yaml
# Trade ETH, but only while BTC is above its 200-day.
root: ETHUSDT
long:
  enter: !gt
    lhs: !close { source: !pick { symbol: BTCUSDT } }
    rhs: !sma { period: 200, source: !close { source: !pick { symbol: BTCUSDT } } }
  exit: !lt
    lhs: !close { source: !pick { symbol: BTCUSDT } }
    rhs: !sma { period: 200, source: !close { source: !pick { symbol: BTCUSDT } } }
```

```sh
fugazi run @gate.yml --series @eth.csv --series @btc.csv -o out/ --crypto -f 1d
```

The named series has to be **passed with `--series`** — reading it is not
fetching it. `fugazi check strategy` lists what a document reads, on a `reads`
line beside `status`, so you can see what a run will need before you have the
data:

```
result
  status  ok · symbol ETHUSDT
  reads   BTCUSDT (pass with --series)
```

A `!pick` naming a series the input does not carry is a **hard error**, not an
empty read. `Pick` resolves `None` on a bar it does not match — right for a
listing gap, and exactly wrong for a symbol that was never passed, where every
comparison downstream stays `None`, nothing ever fires, and the run completes
with zero fills and nothing said.

Two consequences worth being explicit about:

- **A read does not change what is traded.** `root:` still names the traded
  asset, and it is still the blessed series every `source:`-omitted leaf reads.
  A `!pick` under `root:` is the traded series, so it is deliberately *not*
  reported by `check`'s `reads` line.
  The same holds for a pairs document's two legs and a portfolio's children.
- **A read does not change the timeline.** The read series is *left-joined* onto
  the bars the document trades, so a bar the traded asset never had is never
  manufactured; on a bar where the read series is absent, its `!pick` reads
  `None` like any other unmatched query. Only the symbols a document actually
  names are carried, so pointing a one-symbol document at a twenty-symbol CSV
  costs nothing.

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

### Book-field leaves

Where the position-anchored leaves read the *open trade*, these read the
**book** — the strategy's running equity and trade record. Each is a composable
`Real` source usable anywhere an expression is:

| Tag | Reads |
| --- | --- |
| `!equity` | Mark-to-market equity. |
| `!equity_peak` | The running high-water mark of `!equity`. |
| `!drawdown` | Current drawdown as a fraction of the peak (`0` at a new high). |
| `!return_per_bar` | The last bar's return. |
| `!trade_pnl` | The last closed trade's P&L, in currency. |
| `!trade_return` | The last closed trade's return, as a fraction. |

Each takes an optional `source:` naming the book it reads:

- `!strategy_book` (the default) — the book of the enclosing strategy scope:
  the single/pairs/basket/multi strategy, or, inside a portfolio's `weights:`,
  the current child.
- `!portfolio_book` — the composite's **aggregate** book. Only meaningful
  inside a portfolio's `weights:`; a build error anywhere else.

```yaml
# de-risk this child when the portfolio as a whole is underwater,
# regardless of how the child itself is doing
weights: !mul
  lhs: !value 1.0
  rhs: !sub { lhs: !value 1.0, rhs: !drawdown { source: !portfolio_book } }
```

The book-anchored sizing recipes (`!drawdown_throttle`, `!equity_vol_target`,
`!fractional_kelly`) take the same `source:` — see [Sizing](#sizing).

### Trailing strategy metrics — `{ strategy, period }`

These embed a **whole strategy document** and emit its rolling performance as a
`Real`, so one strategy can trade on another's recent track record (a
regime filter, a strategy-of-strategies allocator):

`!sharpe`, `!sortino`, `!calmar`, `!volatility`, `!max_drawdown`.

Each takes a `strategy:` — any of the document shapes, or a preset tag — plus a
`period:` (the rolling window, in bars). `!sharpe` / `!sortino` additionally
take `risk_free_rate:`; every one except `!max_drawdown` takes `bars_per_year:`
to annualize.

```yaml
root: BTCUSDT
long:
  # only go long when a simple trend-follower has been working lately
  enter: !gt
    lhs: !sharpe
      period: 60
      bars_per_year: 365
      strategy: !ma_crossover { root: BTCUSDT, fast: 10, slow: 50 }
    rhs: !value 0.5
  exit: !value false
```

The embedded strategy blesses its own series, so leaves inside `strategy:` read
the symbol *it* names, not the outer document's.

### Price-series indicators — `{ period, source = close }`

`!sma`, `!ema`, `!rma` (Wilder/SMMA), `!wma`, `!hma` (Hull), `!rsi`, `!stddev`,
`!cci`, `!stochastic`.

The ones published with a conventional period carry it as a default, so `!rsi {}`
is `!rsi { period: 14 }` and `!cci {}` is `!cci { period: 20 }` — see
[Parameter defaults](#parameter-defaults). A moving average has no such
convention, so `!sma` / `!ema` / `!rma` / `!wma` / `!hma` and the plain rolling
statistics still want an explicit `period`.

`!stoch_rsi { rsi_period = 14, stoch_period = 14, source = close }` — the
stochastic of an RSI.

### Rolling statistics — `{ period, source = close }`

`!skewness`, `!kurtosis` (raw, ~3 for a normal), `!zscore`,
`!correlation { lhs, rhs, period }`, `!variance_ratio { period, lag = 2, source = close }`.

`!covariance { lhs, rhs, period }` — correlation without the normalisation, so
it keeps the units and the magnitude correlation throws away.

`!beta { lhs, rhs, period }` — the least-squares slope explaining `lhs` by
`rhs`. The order is **asset, then benchmark**, and swapping the two is a
different number, not the reciprocal. Feed returns rather than prices unless you
specifically want the price-level relationship — this takes what it is handed and
does not difference behind your back:

```yaml
# How much of ETH's move is explained by BTC's, over the last 60 bars.
!beta
  lhs: !roc { source: close, period: 1 }
  rhs: !roc { source: !close { source: !pick { symbol: BTC } }, period: 1 }
  period: 60
```

### Linear regression — `{ period, source = close }`

`!linreg_slope`, `!linreg_intercept`, `!linreg_value`, `!linreg_r2`: the
least-squares fit of `source` against the bar index, over a rolling window.
`period` must be at least 2 — one point has no slope, and a document asking for
one is a build error rather than a silent `0`.

This is the one trend primitive the rest of the grammar cannot spell. No
composition of lagged differences is a regression, so a slope has to be a tag.
The four readings answer different questions:

- `!linreg_slope` — the trend rate, in source units **per bar**.
- `!linreg_value` — the fit at the **newest** bar: a de-noised level now, the
  least-squares counterpart of a moving average.
- `!linreg_intercept` — the fit at the **oldest** bar: where the current trend
  started.
- `!linreg_r2` — in `[0, 1]`, how much of the window's variation the line
  accounts for.

The classic pairing is `slope · r²`, which discounts a steep fit that nothing
actually follows:

```yaml
# Trend rate, scale-free (per-bar slope as a fraction of level), quality-weighted.
!mul
  lhs: !div
    lhs: !linreg_slope { period: 60 }
    rhs: !linreg_value { period: 60 }
  rhs: !linreg_r2 { period: 60 }
```

Each `!linreg_*` tag builds its own fit, exactly as the `!bb_*` and `!macd_*`
lines do — four tags over one window is four windows. That is deliberate and
measured: an `Arc<Mutex<_>>`-shared source costs more per bar than recomputing
these does. See *Do not reach for `.shared()`* in
[PERFORMANCE.md](PERFORMANCE.md).

`!percentile { period, pct = 0.5, source = close }` — the `pct`-quantile over the
window, linearly interpolated (R type-7, the same convention the report-level
percentiles use). `pct: 0.5` is the rolling median. This is the adaptive-threshold
primitive: rather than a hardcoded RSI level of 70, ask the series where *its own*
80th percentile sat over the trailing year, and let it move with the regime:

```yaml
enter: !gt
  lhs: &rsi !rsi { period: 14 }
  rhs: !percentile { source: *rsi, period: 252, pct: 0.8 }
```

For the extremes, prefer `!rolling_max` / `!rolling_min` over `pct: 1.0` /
`pct: 0.0` — those are O(1) per bar; `!percentile` is O(period).

`!percentile_rank { period, source = close }` — the inverse question: where does
*today's* reading sit in its own distribution, as `count(v <= x) / period` in
`(0, 1]`. The current sample counts itself, so a fresh high reads exactly `1.0`
and a fresh low `1/period`. Useful as a cross-sectional score, since each symbol
is ranked against its own history rather than against the others, so assets of
different volatility compare fairly.

### Event timing — how long since something happened

`!bars_since { source }` — bars elapsed since `source` last read true, `0` on the
firing bar. **`source` here is a signal, not a value source** — this is the one
value-producing tag that takes a boolean input (`!if_else`'s `cond:` is the
other direction).

It reads `None` until the signal has fired at least once, which makes any
threshold against it read false until then. That is the conservative answer in
both directions: a signal that has never fired cannot gate an entry *in*, and a
clock that never started cannot time-stop a position *out*.

```yaml
# Enter on the MA cross, but only if ADX crossed 25 within the last 5 bars.
enter: !all
  - !crosses_above { lhs: !ema { period: 12 }, rhs: !ema { period: 26 } }
  - !lt
      lhs: !bars_since
        source: !crosses_above { lhs: !adx { period: 14 }, rhs: !value 25 }
      rhs: !value 5
```

`!bars_since_high` / `!bars_since_low { source = close, period }` — bars since the
source last set a new `period`-bar extreme, in `[0, period - 1]`. O(1) per bar
(they share the monotonic-deque core `!rolling_max` and `!aroon_*` use), and
unlike the general `!bars_since` their warm-up is exact, since a window always
contains its own extreme. Aroon Up is exactly
`100·(period − bars_since_high)/period` over a `period + 1` window.

### Multi-output indicators — one tag per component

Each line of a multi-output indicator is its own source tag:

| Tags | Fields (`source` defaults to `close`) |
| --- | --- |
| `!macd_line`, `!macd_signal`, `!macd_histogram` | `{ fast = 12, slow = 26, signal = 9, source }` |
| `!bb_upper`, `!bb_middle`, `!bb_lower` | `{ period = 20, k = 2.0, source }` |
| `!keltner_upper`, `!keltner_middle`, `!keltner_lower` | `{ ema_period = 20, atr_period = 10, multiplier = 2.0, source, candle_source = !current }` |
| `!donchian_upper`, `!donchian_middle`, `!donchian_lower` | `{ period = 20, high = high, low = low }` — the channel **includes the current bar**, see [below](#extremum-sources-include-the-current-bar) |
| `!adx`, `!plus_di`, `!minus_di` | `{ period = 14 }` (the ADX/DI components) |
| `!dmi_plus_di`, `!dmi_minus_di` | `{ period = 14 }` (raw +DI/−DI, no ADX smoothing) |
| `!aroon_up`, `!aroon_down`, `!aroon_oscillator` | `{ period = 14 }` |

#### Extremum sources include the current bar

`!rolling_max`, `!rolling_min` and the `!donchian_*` channel all compute their
extremum over a window that **ends on the bar being evaluated**, current bar
included. That is the conventional definition, and it is not changing — but it
makes the natural way to write a breakout a guaranteed no-op:

```yaml
# Never fires. `close <= high <= rolling_max(high)` once the current bar is in
# the window, so `close` can only ever touch the channel, never cross it.
enter: !crosses_above { lhs: close, rhs: !donchian_upper { period: 20 } }
```

The exit leg fails the same way in reverse (`close >= low >= rolling_min(low)`),
and there is no warning: the strategy builds, runs, and reports zero trades.

Compare the channel against the **previous** bar's value instead — that is what
a breakout means anyway ("today took out the last 20 days' high"):

```yaml
enter: !crosses_above
  lhs: close
  rhs: !lag { source: !donchian_upper { period: 20 }, period: 1 }
exit: !crosses_below
  lhs: close
  rhs: !lag { source: !donchian_lower { period: 10 }, period: 1 }
```

The same applies to anything else compared against its own running extremum —
`!rolling_max { source: close }` versus `close`, a `close / !rolling_max` ratio
(exactly `1.0` at every new high, so `!gt … 1.0` never fires).

The rule is not "channels need a lag" but **a series never crosses an extremum
it is inside of**. Anything bounded by the window runs into it — an `!sma` of
`close` sits below `!rolling_max { source: high }` over the same bars just as
surely as `close` does, so swapping in a smoother does not rescue the
comparison. What *is* reachable is a level offset off the channel
(`!mul { lhs: !rolling_max …, rhs: !value 0.98 }` as a support band) or an
extremum taken over a **different** asset — neither bounds the series being
compared.

### Bar indicators (consume the whole candle)

`!atr { period = 14 }`, `!mfi { period = 14 }`, `!williams_r { period = 14 }`,
`!vwap { period }`, `!sar { step = 0.02, max = 0.2 }`; the range-based volatility
estimators
`!parkinson { period }`, `!garman_klass { period }` and
`!rogers_satchell { period }`, which read more of the bar than a close-to-close
stddev does (high/low, plus open/close for the latter two, so they estimate the
same volatility from fewer bars); and the parameterless `!obv`, `!ad`,
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
| `!pow` | `{ lhs, rhs }` | `lhs` to the power `rhs`; `None` where the result is not a finite real (a negative base at a fractional exponent, `0` to a negative power, an overflow) |
| `!max`, `!min` | `{ lhs, rhs }` | the larger / smaller of two sources, **bar by bar** — not `!rolling_max`, which maximises one source over a window |
| `!clamp` | `{ source, lower, upper }` | `source` held inside a band. Both bounds are expressions. Inverted bounds collapse to `upper`, which is what the `!min`-of-`!max` form it stands for does |
| `!abs`, `!sign` | `{ source }` | absolute value; sign (`1` / `-1` / `0` at exactly zero) |
| `!sqrt` | `{ source }` | square root; `None` on negative samples |
| `!tanh`, `!sigmoid` | `{ source }` | squash the whole real line into `(-1, 1)` / `(0, 1)` — a bounded sizing response that stays smooth where a `!clamp` has corners |
| `!cum_sum`, `!cum_max`, `!cum_min` | `{ source }` | running total / extremum since the **first bar of the run**, unbounded. `!obv` and `!ad` are hard-wired instances of `!cum_sum`; `!cum_max` is what makes a drawdown of an arbitrary series, [see below](#running-accumulators--cum_sum-cum_max-cum_min) |
| `!log` | `{ source = close, base = e }` | logarithm of `source`; `None` on non-positive samples |
| `!exp` | `{ source, base = e }` | exponential of `source` (`base^x`), the inverse of `!log`; `None` where the result overflows to infinity |
| `!lag`, `!diff`, `!ratio`, `!roc` | `{ period = 1, source = close }` | lookback vs. `period` bars ago; `period` defaults to one bar, so `!roc {}` is the per-bar return and `!diff {}` the first difference |
| `!rolling_max`, `!rolling_min` | `{ period, source = close }` | rolling extremum over `period` bars — **includes the current bar**, see [below](#extremum-sources-include-the-current-bar) |
| `!if_else` | `{ cond, then, otherwise }` | ternary: `cond` is a **signal**, the branches are sources — see below |
| `!unstable` | `{ source }` or `<source>` | passthrough that reports no unstable period, so the readiness gate stops waiting for this subtree's IIR tail (one `source:` slot for any output type, signals included) |
| `!resample` | `{ every, inner, source = !current }` | aggregate every N candles of `source` (a `Candle`-output stream, defaulting to `!current`) into one higher-timeframe candle and run `inner` (any Real source) over that HTF candle; emits `inner`'s output on each completed bucket and `None` in between. `inner` is **required** — no default |
| `!latch` | `{ source }` | hold the last `Some` output of `source`; `None` before the first arrives |

The pointwise and accumulating transforms — `!clamp`, `!abs`, `!sign`,
`!sqrt`, `!tanh`, `!sigmoid`, `!exp`, `!cum_sum`, `!cum_max`, `!cum_min` — take a
**required** `source:`. They are the tags with no sensible default series: the
absolute value of a price is the price, `!tanh` and `!sigmoid` of one saturate at
`1` for every bar a real instrument ever prints, and `e^close` overflows outright.
A defaulted `close` there would build, run, and mean nothing. `!log` keeps its
default, because the log of a price is a thing people write.

#### Branching — `!if_else`

The ternary is how a source becomes conditional. `cond` picks between two real
sources:

```yaml
# An ADX-gated momentum score: the ROC when the trend is strong, 0 otherwise.
!if_else
  cond:     !above { source: !adx { period: 14 }, level: 25 }
  then:  !roc   { source: close, period: 20 }
  otherwise: !value 0
```

All three sources advance every bar — the branch that didn't fire keeps warming
up rather than stalling. The ternary reads `None` while the condition or the
branch it selects is still warming up (its reported warm-up length is the max
across all three, a safe upper bound for the readiness gate).

#### Running accumulators — `!cum_sum`, `!cum_max`, `!cum_min`

The unbounded siblings of `!rolling_max` / `!rolling_min`: no window, so they
answer over the whole run rather than the last `period` bars. Where they start is
part of their meaning — a resumed run carries the accumulator forward with the
rest of its state.

`!cum_max` is what generalises drawdown. The built-in `!drawdown` reads the
strategy's book; this reads anything:

```yaml
# Drawdown of the price itself, as a negative fraction — usable as a dip filter
# on an asset the strategy is not trading.
!sub
  lhs: !div
    lhs: &px !close { source: !pick { symbol: BTC } }
    rhs: !cum_max { source: *px }
  rhs: !value 1
```

A bar where the source reads `None` leaves the accumulator untouched *and
unreported*: the tag emits `None` for that bar and the next real sample folds
into the value carried across the gap. `None` here means "no reading this bar",
never "the total reset".

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
root: BTC
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
unstable bar counts pass through as raw composition arithmetic — higher-timeframe
sample counts, not base-bar-scaled. For an EMA-P over a resample-`every`
chain, `stable_bars() = every + settle_bars(P)` (not
`every * (1 + settle_bars(P))`); if a strategy needs base-bar-correct
stability accounting, it must feed the pipeline enough leading history for
the recursive tail to decay in HTF-sample terms.

## Signals

A **signal** produces a `bool` per bar. Both sides of a strategy take one as
`enter`/`exit`. A signal reads `false` until every source it depends on has warmed
up, so an edge coinciding with warm-up never fires a spurious first-bar trade.

### Comparisons — `{ lhs, rhs, epsilon? }`

`!gt`, `!lt`, `!ge`, `!le`, `!eq`, `!ne` compare two **sources**, with a tolerance
band so floating-point noise doesn't cause spurious flips.

Omit `epsilon` and the band is **scale-aware**: `max(1e-12, 1e-9 · larger operand)`.
That matters because a comparison's operands can be anything the grammar produces
— a five-figure price, a `[0, 1]` stochastic, a per-bar return — and a single
absolute number cannot mean the right thing for all three. At price scale a fixed
`1e-8` sits below what an f64 can even represent, so it gave no protection at all.

Set `epsilon` when you want a deadband you mean **literally**, in the operands'
own units — "ignore moves under a tick". It is absolute and is not rescaled:

```yaml
enter: !gt { lhs: !close, rhs: !sma { period: 20 }, epsilon: 0.5 }   # ignore sub-50c crossings
```

### Threshold comparisons — `{ source, level }`

`!above` (`source > level`), `!below` (`source < level`) — compare a source
against a constant, the common case of `!gt`/`!lt` against a number.

Both keys are **required**. A level is meaningless without the series it is a
level *of*: the `70` in `!above { level: 70 }` is an RSI reading, not a price, and
defaulting the omitted series to `close` built a document that runs and never
fires. Write the series out — `!above { source: !rsi {}, level: 70 }`.

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
| `!changed` | `<signal>` or `{ source }` | fires on **any** transition of the inner signal (the edge primitive) — bidirectional by design; pair it with a comparison for a directional event |
| `!became_true` | `<signal>` or `{ source }` | rising edge only (`false → true`) |
| `!became_false` | `<signal>` or `{ source }` | falling edge only (`true → false`) |
| `!has_column` | `{ name }` | schema-level check: true if the overlay column `name` exists. Lets one document run against series with and without an optional side channel |
| `!unstable` | `{ source }` or `<signal>` | passthrough wrapper that forces the reported `unstable_bars()` to `0` for the wrapped subtree. Opt-in override of the safe-by-default strategy-readiness gate (which waits for every source's `stable_bars()` before allowing a trade). One `source:` slot for any output type — the same tag wraps a real-valued source. |
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
root: !param { key: SYM, default: BTC }
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
root: BTC
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
root: BTC
long:
  enter: !crosses_above { lhs: !rsi { period: 14 }, rhs: !value 30 }  # cross up out of oversold
  exit:  !above         { source: !rsi { period: 14 }, level: 70 }    # leave on overbought
```

A Donchian breakout, always-in. **The channel includes the current bar**, so
the textbook spelling — `close` crossing above `!donchian_upper` — can never
fire, and the lag is not optional:

```yaml
root: BTC
long:
  enter: !crosses_above
    lhs: close
    rhs: !lag { source: !donchian_upper { period: 20 }, period: 1 }
short:
  enter: !crosses_below
    lhs: close
    rhs: !lag { source: !donchian_lower { period: 20 }, period: 1 }
```

See [Extremum sources include the current bar](#extremum-sources-include-the-current-bar).

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
score:  !roc { source: !close { source: !pick { symbol: !arg SYM } }, period: 60 }
sizing: !drawdown_throttle { max_drawdown: 0.25 }
```

The shipped examples:

| File | Shape | What it shows |
| --- | --- | --- |
| [`examples/strategy.yml`](../examples/strategy.yml) | single | an annotated SMA-crossover, always-in |
| [`examples/strategy.params.yml`](../examples/strategy.params.yml) | single | the same, parameterised with `!param` |
| [`examples/pairs.yml`](../examples/pairs.yml) | pairs | a BTC/ETH spread z-score with spread-level brackets |
| [`examples/basket.yml`](../examples/basket.yml) | basket | cross-sectional momentum, top/bottom-2, equal-weighted |
