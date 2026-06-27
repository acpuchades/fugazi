# CLAUDE.md

This file provides guidance to Claude Code (claude.ai/code) when working with code in this repository.

## What this is

`fugazi` is a Rust library (edition 2024, no external dependencies) of **incremental** technical-analysis primitives. Every primitive owns its internal state and is advanced one sample at a time via `update()`, carrying just enough intermediate state to produce the next output in ~O(1). The same code therefore serves both live streaming and batch backtesting.

## Commands

- Build: `cargo build`
- Test (unit + integration + doctests): `cargo test`
- Single test by name: `cargo test warms_up_then_averages`
- All tests in one module: `cargo test indicators::rsi`
- One integration-test file: `cargo test --test composition`
- Lint (keep clean): `cargo clippy --all-targets`
- API docs: `cargo doc --open`

## Architecture

Three composable layers: indicators (numeric sources), signals (boolean-valued indicators — `Indicator<Output = bool>`), and strategies (the decision layer that trades into a wallet).

### Indicators = the numeric *sources* (`src/indicator.rs`, `src/indicators/`)

`Indicator` has associated `Input`/`Output`, `update(&mut self, Input) -> Option<Output>`, `value()` (the latest output, matching the public `value` field), `is_ready()`, `reset()`. Output is `Option` because most indicators need a warm-up (`None` until ready).

The defining design choice: **price-series indicators own their input source** and are generic over it — `Ema<S>`, `Sma<S>`, `Rma<S>`, `Rsi<S>`, `Macd<S>` where `S: Indicator<Output = Real>`, with `Input = S::Input`. So composition is just nesting constructors:

```rust
Ema::new(Current::close(), 20)          // EMA-20 of the close (Input = Candle)
Ema::new(Sma::new(src, 10), 20)         // EMA of an SMA
Rsi::new(Identity::new(), 14)           // RSI of a raw Real stream
```

There is **no pipe/`then`/`Chain`** — chaining *is* construction.

- **Leaf sources** terminate the chain: `Value` (constant), `Identity` (raw `Real` passthrough), and the candle accessors under `Current` (`Current::close()`, `Current::volume()`, …; built on the `Field<F>` / `CandleField` carrier in `candle.rs`).
- **Bar indicators** consume the whole `Candle` directly (they are not "of a price"): `Atr`, `Adx`, `TrueRange`, and the volume indicators `Obv`, `Vwap`, `Ad` (Chaikin A/D line), `Mfi` (money-flow index). These take only parameters, or none, e.g. `Atr::new(14)`, `Obv::new()`, `Mfi::new(14)`. The cumulative ones (`Obv`/`Vwap`/`Ad`) anchor at construction; `reset()` re-anchors — e.g. at a session boundary for `Vwap`.
- **Two-source indicators**: `Donchian<H, L>` takes a high source and a low source, e.g. `Donchian::new(Current::high(), Current::low(), 20)`.
- `Real = f64` and `Candle` (OHLCV) live in `src/types.rs`.
- Multi-output indicators (`Macd`, `Adx`, `Bollinger`, `Donchian`, `Keltner`, `Aroon`, `Dmi`) expose one named field per output and set `Output` to a small `Copy` struct (`MacdValue`, `AdxValue`, …); single-output ones expose `value: Option<Real>`. Each also has a **component accessor per output** (`macd.line()`/`.signal()`/`.histogram()`, `bands.upper()`/`.middle()`/`.lower()`, `adx.adx()`, `dmi.plus_di()`, …) returning a `Component<Self>` — a single field projected back into an `Indicator<Output = Real>`, so one output of a struct-valued indicator composes and compares like any other source: `macd.line().crosses_above(macd.signal())`, `Current::close().gt(bands.upper())`. Each accessor **clones** the source (one independently-advanced instance per component, like `crosses_above`'s operand clone). The `Component` carrier (`indicators/component.rs`) holds the source plus a `fn(Output) -> Real` selector — one generic carrier, no per-field marker types.
- `StochRsi<S>` is a type alias for `Stochastic<Rsi<S>>` — StochRSI is just the stochastic transform over an RSI source: `Stochastic::new(Rsi::new(src, 14), 14)`.

### Signals = boolean-valued *indicators* (`src/signal.rs`, `src/indicators/{compare,logic,ext}.rs`)

A **signal is just an `Indicator<Output = bool>`** — there is no second trait hierarchy. `Signal` is a thin marker, `trait Signal: Indicator<Input = Candle, Output = bool>` (blanket-impl'd, `?Sized`), naming "a boolean condition over a `Candle`" so a strategy can hold one as `Box<dyn Signal>`. Like any indicator a signal is `None` until warmed; read it as a plain `bool` (false until ready) with `BoolIndicatorExt::is_true` (= `value().unwrap_or(false)`). There is no `src/signals/` module — the comparison/logic/ext pieces live under `indicators/` and are imported from there.

- **Comparisons are bool-output binary ops** over two real sources: aliases `Gt`/`Lt`/`Ge`/`Le`/`Eq`/`Ne` for `Combine<L, R, GtOp>` etc. (`indicators/compare.rs`). The op is **value-carrying** — each holds an absolute `epsilon` (default `DEFAULT_EPSILON = 1e-8`); the fluent `.gt()`/`.lt()`/… builders use the default, `Gt::with_epsilon(a, b, eps)` overrides.
- **Boolean logic** (`indicators/logic.rs`): `And`/`Or`/`Xor` are `Combine<L, R, AndOp>` etc. over two bool sources; `Not` and `Change` are dedicated unary bool-output carriers; `Const<In>` is the constant-bool leaf (the twin of `Value`).
- `IndicatorExt` (blanket over every `Real`-output indicator, in `indicators/ext.rs`) is the fluent builder for **operators only** — comparisons (`gt`/`lt`/`ge`/`le`/`eq`/`ne`, `above`/`below`), arithmetic (`add`/`sub`/`mul`/`div`), lookback (`lag`/`diff`/`ratio`/`roc`), rolling extremum (`rolling_max`/`rolling_min`), and the composed `crosses_above`/`crosses_below`. Named indicators (`Sma`, `Bollinger`, …) are **not** builder methods; construct via their own `::new`. Do not add `.sma()`/`.bollinger()`-style builders.
- `BoolIndicatorExt` (blanket over every `Indicator<Output = bool>`, `?Sized` so it works on `Box<dyn Signal>`) adds the bool view `is_true()` and the combinators `and`/`or`/`xor`/`not` + the single edge primitive `changed` (a `Change` toggle detector).
- **A crossover is not a primitive**: `crosses_above(a,b)` expands to `a.gt(b).and(a.gt(b).changed())` — "comparison is true *and* it just changed". (Clones the operands, so ~2× the source work.)

### Strategies = the decision layer (`src/strategy.rs`)

Unlike the pure layers below it, a strategy **acts**, in two phases: `Strategy` has `update(&mut self, Input)` (advance its signals/indicators — touches only `&mut self`, so updates across strategies are independent and parallelizable), `trade(&self, &mut dyn Wallet<Symbol>)` (read that state and open/adjust/close positions — `&self`, *price-free*; trades against a shared wallet are serial since sizing resolves against its running state), and `reset()` (associated `Input`/`Symbol`). A driver does, each bar: feed the wallet its prices, `update` every strategy, then `trade` each. There is deliberately **no one-shot `evaluate`**. Almost every classical single-asset strategy shares one shape — a long/flat/short position driven by a few boolean signals, sized all-in — so the crate ships **`SingleAssetStrategy<Sym>`** (`strategies/single_asset.rs`): a concrete `Strategy` (Input = Candle) holding four `Box<dyn Signal>` slots (open/close long, open/close short, each defaulting to constant-false) and built with three builders — `long_on(enter, exit)`, `short_on(enter, exit)` (chainable; an opposite-side entry reverses an open position) and the `buy_and_hold(symbol)` constructor. Its `trade` runs entries first (all-in, reversal-capable) then flatten-to-flat exits. (This is the generic `(signal, action)` shape earlier designs deliberately avoided — a policy-object/`RuleStrategy`/combined-`evaluate` lineage; it was reintroduced **on request**. `SingleAssetStrategy` is still just "a concrete type implementing the trait", parameterised over its signals — not a rule engine. Don't add policy traits or a `(signal, action)` table beyond it without being asked.) The `src/strategies/` **catalogue of classical strategies** is then a set of **free-function specializations** returning a `SingleAssetStrategy` (`ma_crossover`, `rsi_reversal`, `donchian_breakout`, `keltner_breakout`, … grouped under `trend`/`mean_reversion`/`momentum`/`volume`/`composite`): long/flat = `new(sym).long_on(enter, exit)`, always-in long/short = `new(sym).long_on(up, down).short_on(down, up)`. The one strategy that doesn't fit the long/flat/short, all-in mould — `ZScoreReversion` (reads its `z` indicator directly; long/short with a flat rest) — stays its own bespoke `Strategy` type. Shared code is limited to tiny mechanical helpers (`is_long`/`is_short`, taking the position's `.amount`) in `strategies/mod.rs`. Positions size all-in via `value_frac(1.0)`, which survives a reversal (equity, unlike cash), so one `set` reverses and re-sizes all-in exactly — no `enter_all_in` helper.

All in `src/strategy.rs`:
- **`Wallet<Sym>` is a trait** (the portfolio interface taken as `&mut dyn`) — the single **seam** between pure fugazi and a downstream execution system. fugazi stays pure (ships only the in-memory paper impl); a downstream crate implements `Wallet` with a type whose `set_position` *publishes to an event bus / routes to a broker*. The wallet is **priced from outside**: it carries no market view; `update(symbol, price)` feeds each symbol's worth every tick (fugazi is agnostic to where prices come from), and `funds()`/`position(&Sym)`/`price(&Sym)`/`equity()` query it. The single execution primitive is `set_position(Quantity)` (drive a symbol to an absolute signed-unit target); `set` (a `Side` + `Size`, absolute target — opposite side reverses) and `close` (flat) are **default methods** over it, resolving `Size` once so only execution is per-impl. Movements return `Result<Option<Order>, WalletError>` — `Ok(None)` is "nothing to trade", and `WalletError` (`UnknownPrice`, `InvalidPrice` for a non-positive price, `InsufficientFunds` for a no-margin overdraft) flags an impossible move instead of silently no-op'ing. There is deliberately **no `trade(delta)` primitive and no additive `open`** — scale-in is `set_position(position + delta)`. NB: the trading/event-bus/market system itself is **not** in fugazi — it's a separate project that imports fugazi; keep market/IO code out of this crate.
- **Unit-tagged amounts** keep reference currency and instrument units from mixing: `Reference(Real)` (quote/funds currency — `funds`/`equity`/`price`) and `Quantity<Sym> { symbol, amount }` (signed instrument units — `position`, `set_position`). `Order` stays plain `Real` (its `symbol`+`side` already imply the unit).
- **`PaperWallet<Sym>`** is the built-in **pure** `Wallet` impl: in-memory `funds` + `HashMap<Sym,Real>` positions + a `HashMap<Sym,Real>` price map + a blotter (`Vec<Order>`); its `set_position` assumes the fill at the symbol's last fed price and books it. Caller-owned; adds inherent `new`, `is_flat`, `positions()`, `orders()`, `clear_blotter()` (`equity()` is the trait method, arg-free).
- **`Size`** (the magnitude vocabulary): `Units(n)` absolute, `FundsFraction(f)` (= `f·funds/price`, cash), `ValueFraction(f)` (= `f·equity/price`, all-in/target-weight; `1.0` flips cleanly on a reversal), `PositionFraction(f)` (= `f·|position|`, adjust-only). `resolve(price, position, funds, equity) -> magnitude`. Direction comes from `Side` (`Buy`/`Sell`, `.sign()`), not the size.
- `Order<Sym>` (`{ symbol, side, quantity }`); `Order::from_delta(symbol, delta)` builds the buy/sell for a position change (`None` within `DEFAULT_EPSILON`).
- There is **no `Market` trait**: the wallet holds its own fed prices, so a multi-asset input just feeds several symbols via `update` and a strategy's `trade` acts on several symbols in one call (multi-asset/pairs in the same type).
- Sizing/direction/short-selling/always-in-market are all just *what the strategy's code does* — no flags. Python (`python/src/lib.rs`) binds `PaperWallet`/`Order`/`Size` (sides as `"buy"`/`"sell"` strings, symbols as `str`; `update`/`set`/`set_position`/`close`, `WalletError` → `ValueError`); a Python "strategy" is plain Python code driving a `PaperWallet`.

### Generic transform ops (`src/indicators/ops.rs`)

Source-wrapping carriers, each driven by an operator type so a new operator is a trait impl, not a new type:
- `Combine<L, R, Op>` (binary, `BinaryOp`): **one carrier for all binary ops**, generic over the op's input/output via associated `Lhs`/`Rhs`/`Output` and holding the op **by value**. Serves arithmetic `Add`/`Sub`/`Mul`/`Div` (`Real,Real→Real`; `Div` → `None` on /0), the comparisons in `indicators/compare.rs` (`Real,Real→bool`, the op carrying its epsilon) and the boolean logic in `indicators/logic.rs` (`bool,bool→bool`). `Combine::new` needs `Op: Default`; comparison ops also get `Combine::with_epsilon`.
- `Lookback<I, Op>` (unary, relates a source to its value `period` steps ago, `LookbackOp`, zero-sized markers): `Lag` (past value), `Diff` (`x[t]-x[t-n]`), `Ratio` (`x[t]/x[t-n]`), `Roc`.
- `Extreme<S, Op>` (rolling extremum, `ExtremeOp` = `MaxOp`/`MinOp`): `RollingMax`/`RollingMin`.

### Shared cores (`pub(crate)`)

Bare `Real -> Real` math with **no source and no `Indicator` impl**, so both source-wrapping indicators and indicators smoothing values they compute *internally* share one implementation:
- `smoothing.rs`: `EmaState` (EMA recurrence) and `WilderState` (Wilder/RMA, mean-seed). `Ema`/`Macd` use `EmaState`; `Rma` uses `WilderState`; `Rsi` uses two (gain/loss); `Atr` = `TrueRange` + `WilderState`; `Adx` uses four.
- `stats.rs`: `WindowStats` (windowed sum + sum-of-squares → `mean`/`variance`/`stddev`) backs `Sma`/`StdDev`/`Bollinger`; `WindowExtreme<Op>` (monotonic-deque rolling extremum) backs `Extreme`/`RollingMax`/`RollingMin` and `Stochastic`.

## Conventions and gotchas

- **Composition is construction.** A new "X of Y" indicator takes its source `S: Indicator<Output = Real>` in `new`; don't add pipe combinators.
- **Use the cores, not each other's public types.** Internal smoothing of computed scalars uses `EmaState`/`WilderState` (Real recurrence). The public `Rma<S>`/`Ema<S>` wrap a *source* and can't smooth values you computed inline.
- **Adding an operator** (arithmetic/comparison/boolean/lookback): add an `*Op` type implementing the relevant `*Op` trait plus a type alias — never a macro. Arithmetic/boolean/lookback ops are zero-sized `Default` markers; comparison ops are value structs carrying `epsilon`. They live by their carrier — `indicators/{compare,logic}.rs` for the `Combine`-based ones, `indicators/ops.rs` for `Lookback`/`Extreme`.
- `Combine` (arithmetic, comparisons, and `And`/`Or`/`Xor`) feeds the *same* input to both sides, so it requires `Input: Clone`. Use `lhs`/`rhs` naming for binary operands.
- `Combine` holds its op **by value** (so a comparison can carry epsilon); `Lookback`/`Extreme` hold a zero-sized op as `PhantomData<fn() -> Op>`. Input-ignoring leaves (`Value`, `Const`, `Field`) use `PhantomData<fn(I)>` / `fn() -> F` to satisfy the constraint rules (avoids E0207).
- `Change` is a **bidirectional** toggle detector (fires on any transition); directional events come from pairing it with a comparison (see `crosses_above`).
- Constructors `assert!(period > 0, ...)`; document warm-up length in the type's doc comment.
- A comparison/edge is **`None` until** every source it depends on is warmed up (it reads `false` via `.is_true()`); a boolean op (`And`/`Or`/…) is likewise `None` until both sources are ready, so an edge that would coincide with warm-up is not detected (no spurious first-bar trade).
