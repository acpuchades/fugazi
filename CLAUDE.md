# CLAUDE.md

This file provides guidance to Claude Code (claude.ai/code) when working with code in this repository.

## What this is

`arcana` is a Rust library (edition 2024, no external dependencies) of **incremental** technical-analysis primitives. Every primitive owns its internal state and is advanced one sample at a time via `update()`, carrying just enough intermediate state to produce the next output in ~O(1). The same code therefore serves both live streaming and batch backtesting.

## Commands

- Build: `cargo build`
- Test (unit + integration + doctests): `cargo test`
- Single test by name: `cargo test warms_up_then_averages`
- All tests in one module: `cargo test indicators::rsi`
- One integration-test file: `cargo test --test composition`
- Lint (keep clean): `cargo clippy --all-targets`
- API docs: `cargo doc --open`

## Architecture

Three composable layers: indicators (numeric sources), signals (composable booleans), and strategies (the decision layer that trades into a wallet).

### Indicators = the numeric *sources* (`src/indicator.rs`, `src/indicators/`)

`Indicator` has associated `Input`/`Output`, `update(&mut self, Input) -> Option<Output>`, `current()`, `is_ready()`, `reset()`. Output is `Option` because most indicators need a warm-up (`None` until ready).

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
- Multi-output indicators (`Macd`, `Adx`, `Bollinger`, `Donchian`) expose one named field per output and set `Output` to a small `Copy` struct (`MacdValue`, `AdxValue`, `BollingerValue`, `DonchianValue`); single-output ones expose `value: Option<Real>`.
- `StochRsi<S>` is a type alias for `Stochastic<Rsi<S>>` — StochRSI is just the stochastic transform over an RSI source: `Stochastic::new(Rsi::new(src, 14), 14)`.

### Signals = composable *booleans* (`src/signal.rs`, `src/signals/`)

`Signal` has `update(&mut self, Input) -> bool`, `value()`, `reset()`.

- **Comparisons are built from two indicator sources**: one generic `Compare<L, R, Op>` carrier specialised by a zero-sized `Op: CompareOp`; `Gt`/`Lt`/`Ge`/`Le`/`Eq`/`Ne` are type aliases. Tolerance-aware: every comparison carries an absolute `epsilon` (default `DEFAULT_EPSILON = 1e-8`). The fluent `.gt()`/`.lt()`/… builders use the default; `Gt::with_epsilon(a, b, eps)` overrides.
- `IndicatorExt` (blanket-impl'd for every `Real`-output indicator) is the fluent builder for **operators only** — comparisons (`gt`/`lt`/`ge`/`le`/`eq`/`ne`, `above`/`below`), arithmetic (`add`/`sub`/`mul`/`div`), lookback (`lag`/`diff`/`ratio`), rolling extremum (`rolling_max`/`rolling_min`), and the composed `crosses_above`/`crosses_below`. Named indicators (`Sma`, `Bollinger`, `StdDev`, `Stochastic`, …) are **not** exposed as builder methods; construct them via their own `::new`. Do not add `.sma()`/`.bollinger()`-style builders.
- `SignalExt` (blanket-impl'd for every signal) composes signals: `and`/`or`/`xor`/`not` and the single edge primitive `changed` (a `Change` toggle detector).
- **A crossover is not a primitive**: `crosses_above(a,b)` expands to `a.gt(b).and(a.gt(b).changed())` — "comparison is true *and* it just changed". (This clones the operands, so it builds two comparison instances; correct but ~2× the source work.)

### Strategies = the decision layer (`src/strategy.rs`)

Unlike the pure layers below it, a strategy **acts**: `Strategy` has `evaluate(&mut self, Input, &mut dyn Wallet<Symbol>)` and `reset()` (associated `Input`/`Symbol`). Each bar it reads the input and opens/scales/closes positions on the **wallet handed to it** — it returns nothing and holds no portfolio state. There is deliberately **no concrete strategy / rule engine / policy traits in the crate**: every strategy is the user's own type implementing the trait (a struct holding its signals; `evaluate` calls wallet methods). This evolved through several discarded designs (pure orders-out, policy objects, a `(signal, action)` `RuleStrategy`) — do not reintroduce those without being asked.

All in `src/strategy.rs`:
- **`Wallet<Sym>` is a trait** (the portfolio interface taken as `&mut dyn`) — the single **seam** between pure arcana and a downstream execution system. arcana stays pure (ships only the in-memory paper impl); a downstream crate implements `Wallet` with a type whose `trade` *publishes to an event bus / routes to a broker*. Implementors supply three primitives — `funds()`, `position(&Sym)`, and `trade(symbol, delta, price)` (execute a signed delta) — and inherit **default methods** `open` (additive — scale in), `set` (absolute target — opposite side reverses, same side idempotent), and `close` (flat). The defaults resolve `Size` and compute the delta, so additive/absolute/relative-sizing logic lives once; only execution is per-impl. Each returns `Option<Order>` (None = nothing to trade). NB: the trading/event-bus/market system itself is **not** in arcana — it's a separate project that imports arcana; keep market/IO code out of this crate.
- **`PaperWallet<Sym>`** is the built-in **pure** `Wallet` impl: in-memory `funds` + `HashMap<Sym,Real>` positions + a blotter (`Vec<Order>`); its `trade` assumes the fill at the passed price and books it. Caller-owned; adds inherent `new`, `is_flat`, `positions()`, `equity(&impl Market)`, `orders()`, `clear_blotter()`.
- **`Size`** (the magnitude vocabulary): `Units(n)` absolute, `FundsFraction(f)` (= `f·funds/price` units), `PositionFraction(f)` (= `f·|position|`). `resolve(price, position, funds) -> magnitude`. Direction comes from `Side` (`Buy`/`Sell`, `.sign()`), not the size.
- `Order<Sym>` (`{ symbol, side, quantity }`); `Order::from_delta(symbol, delta)` builds the buy/sell for a position change (`None` within `DEFAULT_EPSILON`).
- **`Market<Sym>` trait** (`price(&self, &Sym) -> Real`) is how `Input` prices each symbol: `Candle` impls it (every symbol = its close) for single-asset; a multi-asset snapshot returns a price per symbol. Several `wallet.*` calls in one `evaluate` ⇒ multi-asset/pairs in the same type.
- Sizing/direction/short-selling/always-in-market are all just *what the strategy's code does* — no flags. Python (`python/src/lib.rs`) binds `PaperWallet`/`Order`/`Size` (sides as `"buy"`/`"sell"` strings, symbols as `str`); a Python "strategy" is plain Python code driving a `PaperWallet`.

### Generic transform ops (`src/indicators/ops.rs`)

Source-wrapping carriers, each driven by a zero-sized marker so a new operator is a trait impl, not a new type:
- `Combine<L, R, Op>` (binary, `BinaryOp`): `Add`/`Sub`/`Mul`/`Div`. `Div` yields `None` on divide-by-zero.
- `Lookback<I, Op>` (unary, relates a source to its value `period` steps ago, `LookbackOp`): `Lag` (past value), `Diff` (`x[t]-x[t-n]`), `Ratio` (`x[t]/x[t-n]`).
- `Extreme<S, Op>` (rolling extremum, `ExtremeOp` = `MaxOp`/`MinOp`): `RollingMax`/`RollingMin`.

### Shared cores (`pub(crate)`)

Bare `Real -> Real` math with **no source and no `Indicator` impl**, so both source-wrapping indicators and indicators smoothing values they compute *internally* share one implementation:
- `smoothing.rs`: `EmaState` (EMA recurrence) and `WilderState` (Wilder/RMA, mean-seed). `Ema`/`Macd` use `EmaState`; `Rma` uses `WilderState`; `Rsi` uses two (gain/loss); `Atr` = `TrueRange` + `WilderState`; `Adx` uses four.
- `stats.rs`: `WindowStats` (windowed sum + sum-of-squares → `mean`/`variance`/`stddev`) backs `Sma`/`StdDev`/`Bollinger`; `WindowExtreme<Op>` (monotonic-deque rolling extremum) backs `Extreme`/`RollingMax`/`RollingMin` and `Stochastic`.

## Conventions and gotchas

- **Composition is construction.** A new "X of Y" indicator takes its source `S: Indicator<Output = Real>` in `new`; don't add pipe combinators.
- **Use the cores, not each other's public types.** Internal smoothing of computed scalars uses `EmaState`/`WilderState` (Real recurrence). The public `Rma<S>`/`Ema<S>` wrap a *source* and can't smooth values you computed inline.
- **Adding an operator** (comparison/arithmetic/lookback): add a zero-sized marker implementing the relevant `*Op` trait plus a type alias — never a new struct or a macro. Operators sharing a folder live with their carrier (`signals/compare.rs`) or in that folder's `ops.rs` (`indicators/ops.rs`).
- Binary signal combinators (`And`/`Or`/`Xor`), comparisons, and `Combine` feed the *same* input to both sides, so they require `Input: Clone`. Use `lhs`/`rhs` naming for binary operands.
- Marker-parameterised carriers hold the op as `PhantomData<fn() -> Op>`; input-ignoring leaves (`Value`, `Field`) use `PhantomData<fn(I)>` / `fn() -> F` to satisfy the constraint rules (avoids E0207).
- `Change` is a **bidirectional** toggle detector (fires on any transition); directional events come from pairing it with a comparison (see `crosses_above`).
- Constructors `assert!(period > 0, ...)`; document warm-up length in the type's doc comment.
- A comparison/edge stays `false` until every source it depends on is warmed up.
