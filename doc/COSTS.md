# Trading costs

Every real fill has friction the historical bar doesn't show — the exchange
takes a fee, the bid/ask sits some fraction of a percent apart, a market
order shifts the tape it fills against. fugazi models those three effects
as **three composable legs** applied on top of the theoretical trigger
price, one leg at a time, in a fixed order:

**spread → slippage → commission.**

Every leg has a *no-op* default, so a run with no cost configuration is
byte-identical to the pre-costs release. Every leg has a small **model
catalogue** the user picks from, plus a **`(symbol, frequency)`-scoped
override** knob so an equities book and a crypto book can share one config.

This document walks the three legs, their models, the scope grammar, the
two shipped venue presets, and — for readers coming from another tool — a
side-by-side with how zipline, backtrader, backtesting.py and vectorbt
model the same three effects.

- [The pipeline](#the-pipeline)
- [Wiring costs into a run](#wiring-costs-into-a-run)
- [Commission](#commission)
- [Spread](#spread)
- [Slippage](#slippage)
- [Scope precedence](#scope-precedence)
- [Presets shipped in `examples/`](#presets-shipped-in-examples)
- [Reporting](#reporting)
- [Comparison with other backtesters](#comparison-with-other-backtesters)
- [Caveats](#caveats)

## The pipeline

Every fill — a market order flushed at the next bar's `open`, or a resting
stop / take-profit triggered by the bar's `[low, high]` range — goes
through the same pipeline. Let `P` be the theoretical trigger price (bar
`open` for a market fill, the trigger level for a resting leg, or the
bar's `open` on a gap through the trigger):

1. **Spread.** Add the leg's half-spread on a buy, subtract it on a sell:
   `P₁ = P ± half_spread(P, candle)`.
2. **Slippage.** Move `P₁` adverse to the trading side (buys slip up,
   sells slip down) by the slippage leg's magnitude, scaled by
   `stop_multiplier` on resting fills: `P₂ = slippage(side, P₁, units,
   candle, kind)`.
3. **Commission.** Compute `commission(notional=P₂·units, units)` and
   deduct it from the wallet's cash **separately**. It never nets into
   `P₂`.

`P₂` is what lands on `Order::price` and in `trades.csv`'s `price` column.
The commission lands on `Order::commission` and in `trades.csv`'s
`commission` column — a distinct axis so cost accounting is unambiguous.

Everything is under [`src/costs/`](../src/costs/) as three tiny traits, one
model per struct, plus a `TradingCosts` bundle that
[`PaperWallet::with_costs`](../src/strategy.rs) takes.

## Wiring costs into a run

Two surfaces: the CLI's `--costs` flag and the Rust API's
`PaperWallet::with_costs`.

### CLI: `--costs`

Repeatable, `,`-separated. Each term is one of:

- `[SCOPE:]key=value` — set (or nudge) one leg inline.
- `@file.yml` — load a whole venue preset.
- `none` — reset every leg to no-op and silence the *no cost model set*
  warning banner.

```sh
# 10 bps taker + 5 bps quoted spread, applied to every fill.
fugazi run @strategy.yml -s @candles.csv -o out/ \
    --costs 'commission=!percentage { rate: 0.001 },spread=!bps { bps: 5 }'

# Load a venue preset, then nudge one field. The dotted path addresses the
# spec tree literally, so it names the model's variant (`!percentage`) too.
fugazi run @strategy.yml -s @candles.csv -o out/ \
    --costs @examples/binance.yml,commission.percentage.rate=0.00075   # BNB discount

# Tighter spread for BTC on daily bars only; every other (symbol, freq)
# falls back to the default leg.
fugazi run @strategy.yml -s @candles.csv -o out/ \
    --costs @examples/binance.yml,'BTCUSDT[1d]:spread=!bps { bps: 3 }'

# Explicitly acknowledge the frictionless default (silences the banner).
fugazi run @strategy.yml -s @candles.csv -o out/ --costs none
```

Terms fold left-to-right; later terms deep-merge into earlier ones and
override at the same specificity. Multiple `--costs` flags on one command
line concatenate the same way as multiple `,`-separated terms in one flag.

**A leading scope distributes over the whole flag.** A `SCOPE:` on the
first inline term of a `--costs` value applies to every later inline term
in the same flag that doesn't carry its own scope — so
`--costs 'BTC:commission=…,spread=…'` sets *both* legs for BTC, not
`commission` for BTC and `spread` on the default leg. A per-term scope
still overrides (`BTC:commission=…,ETH:spread=…` keeps `spread` on ETH),
and `@file` / `none` terms are unaffected. When you actually want one
scoped term and one default-leg term, split them into two flags:
`--costs BTC:commission=… --costs spread=…`.

Validating the config without running the backtest:

```sh
fugazi check costs @examples/binance.yml,'BTC:commission=!percentage { rate: 0.0005 }'
```

### Rust: `PaperWallet::with_costs`

```rust
use fugazi::costs::{
    CommissionModel, FixedBpsSlippage, FixedBpsSpread, PercentageCommission,
    TradingCosts,
};
use fugazi::PaperWallet;

let costs = TradingCosts::new(
    Box::new(PercentageCommission::new(0.001)),  // 10 bps taker
    Box::new(FixedBpsSpread::new(5.0)),          // 5 bps full spread
    Box::new(FixedBpsSlippage::new(2.0)),        // 2 bps adverse
);
let wallet: PaperWallet<String> = PaperWallet::new(10_000.0).with_costs(costs);
```

The default `PaperWallet::new` is byte-identical to the pre-costs release
(the internal bundle is `TradingCosts::none()`), so a caller only opts in
when they want costs.

## Commission

Cash paid on top of the fill's cash flow, deducted from the wallet's
funds, and recorded on `Order::commission`. All models saturate at zero
(a negative-parameter model still charges `0.0`, never a rebate).

| Variant | Formula | Fields | Use |
|---|---|---|---|
| `none` | `0` | — | Explicit zero (same as the default). |
| `fixed` | `amount` | `amount` | Flat per-ticket. |
| `percentage` | `rate · notional` | `rate` | Percentage of trade value (crypto taker/maker fees). |
| `per_unit` | `rate · units` | `rate` | Per-share / per-contract (US equities, futures). |
| `composite` | `Σ parts[i]` | `parts: [Model, …]` | Sum of several charges (exchange fee + regulatory fee + …). |
| `max` | `max(lhs, rhs)` | `lhs`, `rhs` | Per-order minimum (IBKR's `max($1.00, $0.0035 · shares)`). |

**Notional** is `final_price × units` — the *post*-spread, *post*-slippage
price. That matters when a percentage fee sits on top of a wide spread: the
fee grows slightly with the spread, matching how a real venue bills.

Examples:

```yaml
# Binance-style: 10 bps taker, percentage of trade value.
commission: !percentage { rate: 0.001 }

# US equities: half a cent per share.
commission: !per_unit { rate: 0.005 }

# IBKR Tiered US equities: max($1.00, $0.0035/share).
commission: !max
  lhs: !per_unit { rate: 0.0035 }
  rhs: !fixed { amount: 1.0 }

# Exchange fee plus a regulatory pass-through.
commission: !composite
  parts:
    - !percentage { rate: 0.0003 }
    - !fixed { amount: 0.02 }
```

## Spread

The half-spread applied per side. Buys pay it (`+`); sells receive it
(`−`). Every model produces a *half*-spread — a `bps: 10` model is 10 bps
round-trip, 5 bps per side — because that's how a quoted market maker's
book advertises it (top-of-book = mid ± half-spread).

| Variant | Formula (half-spread) | Fields | Use |
|---|---|---|---|
| `none` | `0` | — | Explicit zero. |
| `bps` | `bps · 1e-4 · price / 2` | `bps` | Basis-point full spread; the standard shape for anything quoted in bps. |
| `absolute` | `amount / 2` | `amount` | Absolute full spread in reference currency (a Euro FX book's 1-pip spread). |

Examples:

```yaml
# 5 bps round-trip (2.5 bps per side) on a $100 tape = 2.5c per side.
spread: !bps { bps: 5 }

# 1c full spread (0.5c per side) on a dollar-tick stock.
spread: !absolute { amount: 0.01 }
```

## Slippage

Price impact, always adverse to the trading side, applied *after* the
spread. Buys slip up, sells slip down. The `stop_multiplier` field scales
the impact when the fill is a resting stop or take-profit — a triggered
stop in a fast market typically slips more than a planned entry. Default
`1.5×` (see [`DEFAULT_STOP_MULTIPLIER`](../src/costs/mod.rs)); set it to
`1.0` to model resting fills as identical to market orders.

| Variant | Formula (adverse fraction of price) | Fields | Use |
|---|---|---|---|
| `none` | `0` | — | Explicit zero. |
| `bps` | `bps · 1e-4 · stop_mult` | `bps`, `stop_multiplier` (opt.) | Constant bps impact regardless of order size. |
| `volume_participation` | `coef · (units / candle.volume)^exp · stop_mult` | `coefficient`, `exponent` (opt., default `0.5`), `stop_multiplier` (opt.) | Almgren-Chriss-style square-root impact; the standard "impact grows with participation" model. Zero-volume bars yield zero impact. |

Examples:

```yaml
# 2 bps flat impact, 3 bps on stops (1.5× default).
slippage: !bps { bps: 2 }

# Square-root impact: a fill of 1% of the bar's volume shifts the tape by
# coef · sqrt(0.01) = 0.1 · 0.1 = 1% adverse.
slippage: !volume_participation
  coefficient: 0.1
  exponent: 0.5
  stop_multiplier: 1.5

# Linear impact (exp = 1) — impact grows in lockstep with participation.
slippage: !volume_participation { coefficient: 0.05, exponent: 1.0 }
```

`volume_participation` is a *single-bar* approximation: a fill uses only
its own bar's volume, no participation cap carries across bars. For a
strategy sending genuinely-large orders, split them in strategy code.

## Scope precedence

Every leg supports per-symbol and per-frequency overrides. Resolution
picks the winning model in this order (most-specific to least-specific;
same-specificity ties are broken by insertion order — later wins):

1. `scoped[i]` — an entry matching *both* `symbol` and `freq`.
2. `by_symbol[symbol]` — set via `SYMBOL:key=…` or a preset `by_symbol` map.
3. `by_interval[freq]` — set via `[FREQ]:key=…` or a preset `by_interval` map.
4. `default` — the leg's fallback.
5. The no-op default (zero-cost) if none of the above resolves.

Each rung shadows the ones below it *per leg* — a strategy that touches
BTC on daily bars and ETH on hourly bars can share one commission default
and have separate slippage models.

The full grammar is documented under
[`--costs`](CLI.md#--costs) in `CLI.md`.

## Presets shipped in `examples/`

Two starter presets ship in the repo. Both are approximations of the
retail-default fee schedule at their respective venues; verify against
the current fee schedule before running live money.

### `examples/binance.yml` — crypto spot (Binance taker)

```yaml
commission: !percentage           # flat model → commission.default
  rate: 0.001                     # 10 bps taker (VIP 0)

spread:                           # structured: default + per-symbol overrides
  default: !bps { bps: 2 }
  by_symbol:
    BTCUSDT: !bps { bps: 1 }
    ETHUSDT: !bps { bps: 1.5 }

slippage: !volume_participation
  coefficient: 0.1
  exponent: 0.5
  stop_multiplier: 1.5
```

### `examples/ibkr.yml` — US equities (IBKR Tiered)

```yaml
commission: !max                  # max($1.00, $0.0035/share)
  lhs: !per_unit { rate: 0.0035 }
  rhs: !fixed { amount: 1.0 }

spread: !bps { bps: 2 }

slippage: !volume_participation
  coefficient: 0.05
  exponent: 0.5
  stop_multiplier: 2.0
```

## Reporting

Turning `--costs` on changes the run output in three places (and only
those — omit `--costs` and every output byte is unchanged):

- **`trades.csv`** gains a `commission` column. `price` is now the
  *post-spread, post-slippage* price. Both are per-fill.
- **`metrics.yml`** gains a `costs:` section:

  | Field | Meaning |
  |---|---|
  | `costs.total_commission` | `Σ commission` over every fill, in reference currency. |
  | `costs.total_slippage_cost` | Aggregate spread + slippage cost — computed by re-running the same strategy zero-cost and matching fills. |
  | `costs.cost_drag_pct` | Gross CAGR minus net CAGR, percentage points. `None` when either endpoint's CAGR is degenerate. |

- **Console `metrics` block** prints **gross** (frictionless) and **net**
  (priced) `sharpe` / `cagr` side by side, so the cost drag is one line
  away.

The full `costs.*` catalogue lives in
[`METRICS.md`](METRICS.md#costs----cost-model-aggregates).

## Comparison with other backtesters

fugazi's cost model is deliberately smaller than a full market-impact
research toolkit, but larger than the "one number for fees" shape most
retail Python tools take. The table below shows how the same three
effects — commission, spread, slippage — surface in a handful of
established tools.

| Tool | Commission | Spread | Slippage | Scoping |
|---|---|---|---|---|
| **fugazi** | Five models: `fixed`, `percentage`, `per_unit`, `composite` (sum), `max`. | Two: `bps` (basis-point) and `absolute`. Half applied per side. | Two: `bps` (constant), `volume_participation` (Almgren–Chriss sqrt/power impact). `stop_multiplier` for resting fills. | `(symbol, frequency)` per leg; `by_symbol` / `by_interval` / `scoped` maps, with insertion-order tie-breaks. |
| **zipline** | `PerShare(cost, min_trade_cost)`, `PerTrade(cost)`, `PerDollar(cost)`, and IBKR-shaped built-ins. Set globally via `set_commission()`. | No first-class spread model — folded into slippage. | `FixedSlippage(spread)`, `FixedBasisPointsSlippage(basis_points, volume_limit)`, `VolumeShareSlippage(volume_limit, price_impact)`. Volume-share caps participation per bar. Set via `set_slippage()`. | Per-security via `set_commission(us_equities=…, us_futures=…)`; no per-frequency knob. |
| **backtrader** | `CommissionInfo` classes with `commission`, `mult`, `margin`, `commtype` (Percentage / Fixed). Per-instrument via `broker.addcommissioninfo(...)`. | Not modeled separately — the `slip_*` params on the broker cover the effect. | `slip_perc`, `slip_fixed`, `slip_open`, `slip_match`, `slip_limit`, `slip_out` on the broker — a percentage / fixed slippage plus flags for whether to apply on open, cap by high/low, etc. | Per-instrument commission; slippage is global to the broker. |
| **backtesting.py** | One `commission` argument on the `Backtest` constructor: a percentage of trade value (fraction of trade value). | Not modeled. | Not modeled directly — fills happen at the requested price (or next bar's open for market orders). | None — one number per backtest. |
| **vectorbt** | `fees` (percentage of trade value) and `fixed_fees` (per-trade). | Not modeled. | `slippage` (percentage of price, symmetric). | Per-asset in a multi-asset Portfolio; no per-frequency dimension. |
| **QuantConnect / Lean** | `FeeModel` subclasses (per-security). Extensible in C#/Python. | Some `FillModel`s add a bid/ask spread; not universal. | `SlippageModel` subclasses (`ConstantSlippageModel`, `VolumeShareSlippageModel`, `MarketImpactSlippageModel`, …). Per-security. | Per-security; user code, not a config surface. |
| **bt (pmorissette)** | A `commissions` callable on the backtest (`(quantity, price) → cash`). | Not modeled. | Not modeled. | None. |

Where fugazi lands:

- **Spread is its own leg** rather than folded into slippage. That
  matters when reporting: `costs.total_slippage_cost` in `metrics.yml`
  aggregates the *spread + slippage* combined, and the split between the
  two configured legs is transparent from the config itself. Most Python
  tools collapse the two, so a report that says "slippage was X bps"
  can't be reconciled with the venue's actual bid/ask.
- **Scoping is `(symbol, frequency)` per leg.** That's finer than
  zipline's "per-security" and than backtrader's "per-broker", both of
  which let you set different commissions for equities vs futures but
  don't let you say "the spread on my daily BTC book is tighter than on
  my 1-minute BTC book because I'm not paying the tape's noise". fugazi
  makes both dimensions first-class because a multi-timeframe or
  cross-venue book usually needs the split.
- **`stop_multiplier` on the slippage leg** is uncommon. zipline's
  `VolumeShareSlippage` treats every fill the same; backtrader's
  `slip_*` don't distinguish resting from market. In real markets a
  triggered stop typically executes worse than a planned entry (thin
  book at the trigger, cascading stops in a fast move), so fugazi bumps
  the assumed slippage on the two resting order kinds by a
  configurable multiplier. Default `1.5×` — set it to `1.0` to opt out.
- **The default is byte-identical to zero costs.** No configuration →
  no fee, no spread, no slippage, and `metrics.yml` / `trades.csv` have
  the same schema as the pre-costs release. That's harder to say about
  tools where the cost model is baked into the fill engine —
  backtesting.py's `commission=0` is a supported value, but there's no
  way to opt *out* of the fill-price rounding it does around slippage.
  A frictionless run in fugazi is a *first-class* mode, not a special
  case.
- **What fugazi doesn't have (yet):** a per-bar participation cap that
  spans bars, an order-book / queue-position model, and a stochastic
  variant of `volume_participation` where the impact draws from a
  distribution rather than resolving to a point estimate. Those are all
  natural follow-ups. If you need any of them today, drop into the
  library and implement the three traits directly — a downstream crate
  can plug its own `SlippageModel` into `PaperWallet::with_costs`
  without touching fugazi.

## Caveats

- **Sizing math is theoretical-price based.** A `value_frac(1.0)` all-in
  entry under a non-trivial cost model overshoots funds by the cost
  overhead and is silently dropped by the wallet's queued-order
  semantics. Leave headroom by sizing under `1.0` (`0.99`, say) when a
  cost model is active.
- **`volume_participation` is a single-bar approximation.** No
  participation cap carries across bars; a fill uses only its own bar's
  volume. Not a substitute for a full market-impact model.
- **`stop_multiplier` defaults to 1.5×.** Bumping it above that widens
  the assumed slippage on triggered stops; setting it to `1.0` models
  stops as identical to market orders.
- **Commission is billed off the *final* price**, not the theoretical
  one. A percentage-of-notional fee grows slightly with the spread and
  the slippage — matching a real venue's bill, but slightly larger than
  what a mid-price notional would give.
- **The two shipped presets are approximations of retail defaults as of
  2026-07-04.** They exist to make it easy to try one. Verify against
  the current venue schedule before running live money.
