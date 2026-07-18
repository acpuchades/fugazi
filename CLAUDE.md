# CLAUDE.md

Guidance for Claude Code in this repo.

## What this is

`fugazi` is a Rust library (edition 2024) of **incremental** technical-analysis primitives. Every primitive owns its state and advances one sample at a time via `update()` in ~O(1) — same code for live streaming and batch backtesting.

Unconditional deps: `serde`+`serde_json`, `time`, `statrs` (Φ/Φ⁻¹ for PSR/DSR). Default-on features: **`sources`** (remote providers), **`runtime`** (type-erasure vocabulary in `fugazi::runtime`), **`cli`** (binary; implies both). New unconditional deps are judgment calls — reach for closed-form first.

## Commands

- Build: `cargo build`; Test: `cargo test`; Lint: `cargo clippy --all-targets` (keep clean); Docs: `cargo doc --open`

### Bumping the version — sync **four** places (`cargo build` only catches Rust drift)

1. `Cargo.toml` (workspace root, `X.Y.Z`)
2. `python/Cargo.toml` (pyo3 cdylib, `X.Y.Z`)
3. `python/pyproject.toml` (wheel metadata, `X.Y.Z` — what `pip install fugazi` sees)
4. `README.md` — `## Install` snippet, `fugazi = "X.Y"` (major.minor)

Then `cargo build --workspace` (updates `Cargo.lock`), commit five files (three manifests + README + Lock), tag `vX.Y.Z`, push. `python/README.md` has no version string.

## Architecture

Three composable layers: indicators (numeric sources), signals (`Indicator<Output = bool>`), strategies (decision layer trading into a wallet).

### Indicators — numeric sources (`src/indicator.rs`, `src/indicators/`)

`Indicator` has `Input`/`Output`, `update(&mut self, Input) -> Option<Output>`, `value()`, `is_ready()`, `reset()`, plus:

- **`warm_up_period()`** — *exact* samples before first `Some`. Wrappers add on top; binary carriers take max. `tests/warm_up.rs` asserts exactness — add new indicators to that battery.
- **`unstable_period()`** (default `0`) — extra samples IIR smoothers need for seed's residual to decay below `SETTLE_TOLERANCE = 1e-3`. Wrappers sum into source's.
- **`stable_period()`** = warm-up + unstable.

Output is `Option` (warm-up → `None`).

**Defining choice: price-series indicators own their source, generic over it** — `Ema<S>`, `Sma<S>`, `Rma<S>`, `Rsi<S>`, `Macd<S>` where `S: Indicator<Output = Real>`, `Input = S::Input`. Composition = nesting constructors: `Ema::new(Current::close(), 20)`, `Ema::new(Sma::new(src, 10), 20)`, `Rsi::new(Identity::new(), 14)`. **No pipe/`then`/`Chain`** — chaining *is* construction.

- **Leaf sources**: `Value<I>` (constant), `Identity<I = Real>` (passthrough; `Identity::<Atom>::new()` = default atom source), `Current` candle accessors (`Current::close()`/`Current::volume()`; via `Field<F, S>`/`CandleField`). Every source-generic leaf — `Field<F, S>` (`Close`/`High`/`Low`/`Open`/`Volume`/`Typical`/`Median`), `CurrentBar<S>`, `Calendar<F, S>`, `CurrentTime<S>`, `IsWeekday<S>`, `IsWeekend<S>` — is generic over `S: Indicator<Output = Atom>` (default `Identity<Atom>`). `T::new()` uses default, `T::of(source)` re-roots on custom.

- **Cross-asset sources** (`time.rs` + `snapshot.rs` + `indicators/pick.rs`):
  - **`Frequency`** — bar cadence enum (`Minute(u32)`/`Hour`/`Day`/`Week`/`Month`), totally ordered by duration, `FromStr` on `N<unit>` (`m`/`h`/`d`/`w`/`M`). `sources::Interval` is provider-side twin.
  - **`Selector { symbol: Option<String>, freq: Option<Frequency> }`** — partial key. `None` = wildcard; shorthands `by_symbol`/`by_freq`/`exact`.
  - **`Snapshot<K>`** — newtype `HashMap<K, Atom>` with `get`/`insert`/`iter`/`FromIterator` + `sole_atom(&self)` (unique in size-1, **panics on 2+**, `None` on empty). `impl Snapshot<Selector>` adds `find(query)`.
  - **`Pick<S = Identity<Snapshot<Selector>>>`** projects one asset: `Output = Atom`. `Pick::new()` (empty selector, sole-atom), `Pick::matching(selector)`, `Pick::of(selector, source)`. Non-empty selector → `None` on no match; empty **panics** on 2+ snapshot. **`Atom` equality is by `time`**; `Ord` sorts chronologically with `None` first.

  Python: `ta.Frequency("1h")`, `ta.Selector(symbol="BTC", freq="1h")`, `ta.Snapshot({...})`, `ta.pick(...)`. Snapshot keys accept `str`/`Frequency`/`(str, freq)`/`Selector`.

- **Position-anchored sources** (`indicators/position.rs`): strategy owns shared `Position` (`Rc<RefCell<PositionState>>`) with signed `size`, `entry`, `peak`/`trough` since entry. Accessors `.entry()`/`.peak()`/`.trough()` return `PositionField` (`Indicator<Output = Real>`, `None` while flat) — stops/targets become expressions like `position.entry().sub(Atr::new(14).mul(...))`. Only meaningful inside a strategy driving the `Position`.

- **Bar indicators** consume whole `Candle`: `Atr`, `Adx`, `TrueRange`, range vol estimators (`Parkinson` H/L; `GarmanKlass` adds O/C, clamped ≥0; `RogersSatchell` drift-independent — each rolls mean of per-bar OHLC estimator via `WindowStats` then sqrt), volume (`Obv`, `Vwap`, `Ad`, `Mfi`). Cumulative ones anchor at construction; `reset()` re-anchors. Yang-Zhang absent — overnight gap meaningless on 24/7 crypto. Two-source: `Donchian<H, L>`.

- **Windowed stats over Real**: `Skewness`/`Kurtosis` (standardized moments; kurtosis raw, ~3 for normal), `ZScore` (`(x−SMA)/stddev`), two-source `Correlation` (rolling Pearson; autocorrelation via `Correlation::new(x.clone(), x.lag(n), period)`). O(1)/bar off `WindowStats`/`WindowCovariance`. **`VarianceRatio` is the deliberate exception** — Lo-MacKinlay regime classifier over first differences (`1.0` random-walk null, `>1` trending, `<1` mean-reverting), **O(`period`)/bar** (retains window in `VecDeque`). Asserts `lag ≥ 2`, `period ≥ lag + 2`.

- **Calendar sources** (`indicators/calendar.rs`) decompose `atom.time` (UTC ms): `Year`/`Month`/`Day`/`Hour`/`Minute`/`Second`/`DayOfWeek` (ISO 1=Mon..7=Sun)/`DayOfYear`/`WeekOfYear`/`Quarter`/`UnixSeconds`/`UnixMillis` on `Calendar<F, S> + CalendarField`. Plus `CurrentTime` leaf and bool `IsWeekday`/`IsWeekend`. CSV loader and remote providers set `Atom::time`; `None` only for synthetic atoms. Daily+ bars at 00:00 UTC. YAML: bare `!year`/…/`!time`/`!is_weekday`/`!is_weekend`. Uses `time` crate — the unconditional dep.

- **`Unstable<S>`** (`indicators/unstable.rs`): passthrough forwarding everything to `S` *except* `unstable_period() = 0`. Fluent `.unstable()` on both extension traits. YAML: `!unstable { source | signal }`. Opt-in override for readiness gate.

- `Real = f64` and `Candle` (OHLCV) in `src/market.rs` alongside `Atom`/`OverlayInfo`/`Schema`. `src/types.rs` is a facade re-exporting `time`/`market`/`snapshot`.

- **Multi-output indicators** (`Macd`, `Adx`, `Bollinger`, `Donchian`, `Keltner`, `Aroon`, `Dmi`) expose named fields; `Output` is `Copy` struct. Each has **component accessor per output** (`macd.line()`/`.signal()`/`.histogram()`, `bands.upper()`/`.middle()`/`.lower()`, `dmi.plus_di()`, …) returning `Component<Self>` — field projected as `Indicator<Output = Real>`: `macd.line().crosses_above(macd.signal())`. Accessors **clone** source. Bodies via **`component_accessors!` macro** — don't hand-write.

- `StochRsi<S>` = alias for `Stochastic<Rsi<S>>`.

### Signals — boolean indicators (`src/signal.rs`, `src/indicators/{compare,logic,ext}.rs`)

**A signal is just `Indicator<Output = bool>`** — no second trait hierarchy. `Signal` is thin marker `trait Signal: Indicator<Input = Candle, Output = bool>` (blanket, `?Sized`) so strategies hold `Box<dyn Signal>`. `None` until warmed; read as `bool` (false until ready) via `BoolIndicatorExt::is_true`.

- **Comparisons**: aliases `Gt`/`Lt`/`Ge`/`Le`/`Eq`/`Ne` for `Combine<L, R, GtOp>` etc. Op carries absolute `epsilon` (default `1e-8`); `Gt::with_epsilon(a, b, eps)` overrides.
- **Boolean logic**: `And`/`Or`/`Xor` are `Combine<...>`; `Not` and `Change` are dedicated unary carriers; `Const<In>` is constant-bool leaf; `Every<In>(period)` is a **periodic pulse** — fires `true` every `period` bars with a *delayed* first fire on bar `period - 1` (0-indexed). Canonical `rebalance_on` cadence source. YAML: `!every N`; `!never` is sugar for `!value false`.
- **`IndicatorExt`** (blanket over Real-output): fluent builder for **operators only** — comparisons (`gt`/`lt`/`ge`/`le`/`eq_to`/`ne_to`, `above`/`below` — `eq_to`/`ne_to` avoid `PartialEq` collision), arithmetic (`add`/`sub`/`mul`/`div`), lookback (`lag`/`diff`/`ratio`/`roc`), rolling extremum (`rolling_max`/`rolling_min`), `unstable`, `crosses_above`/`crosses_below`. Named indicators are **not** builder methods; use `::new`. Don't add `.sma()`-style builders.
- **`BoolIndicatorExt`** (blanket over `Indicator<Output = bool>`, `?Sized`): `is_true()`, `and`/`or`/`xor`/`not`, edge primitive `changed`, `unstable`.
- **Crossover is not a primitive**: `crosses_above(a,b)` = `a.gt(b).and(a.gt(b).changed())` (clones operands, ~2× source work).

### Strategies — decision layer (`src/strategy.rs`, `src/wallet.rs`)

`Strategy` trait: `update(&mut self, Input)`; `on_fill(&mut self, &Order<Symbol>)` (default no-op); `is_ready(&self) -> bool` (safe-by-default, default `true`); `trade(&self, &mut dyn Wallet<Symbol>)` (`&self`, price-free, serial); `reset()`; assoc `Input`/`Symbol`.

Each bar the driver: feed each symbol to wallet, route each fill to every strategy's `on_fill`, `update` each strategy, then `trade` each *only if* `is_ready()`. Deliberately **no one-shot `evaluate`**.

**`SingleAssetStrategy<Sym>`** (`strategies/single_asset.rs`) is the concrete `Strategy` (Input = Candle) — long/flat/short driven by boolean signals, sized against equity. Four `Box<dyn Signal>` slots (open/close long, open/close short), default `Const::<Candle>::new(false)`.

- **Readiness gate.** `is_ready()` = `bars_seen >= max(stable_period())` across every wired signal, protective level, sizing indicator. Wrap subtree in `Unstable` to contribute `0`.
- **Builders.** `long_on(enter, exit)`, `short_on(enter, exit)` (opposite-side entry reverses), `buy_and_hold(symbol)`.
- **Protective levels.** Per-side stop/take-profit as `Box<dyn Indicator<Output = Real>>` via `long_stop_loss`/`long_take_profit`/`short_*`, built against `position()`. E.g. `position.entry().mul(Value::new(0.95))` (fixed), `position.peak().mul(Value::new(0.95))` (trailing), `position.entry().sub(Atr::new(14).mul(Value::new(2.0)))` (ATR).
- **`trade` sequence.** Read sizing → skip on `None` → entries (sizing-scaled, reversal-capable) → flatten-to-flat signal exits → **rebalance gate** (resize held to sizing target when signal fires) → rest protective on active side.
- **`rebalance_on(signal)`** (default `Const::false` — never). On bars where the gate fires, `wallet.set(sym, held_side, value_frac(size))` re-affirms the current sizing target — idempotent when target unchanged, market resize otherwise. Lets vol-targeted / Kelly-scaled strategies adjust an open position when the target drifts.
- **Order semantics.** Entries and signal exits are **market** (`set`/`close` then `cancel_protective`), filled next bar at `open`. Protective stops are **resting** orders the wallet owns; strategy re-submits each bar (idempotent, latest-wins); wallet triggers and prices (at level, or `open` on gap). Trailing tracks *completed* bars.
- **Not a rule engine.** Don't add `(signal, action)` tables without being asked.

**`src/strategies/` catalogue** = free functions returning `SingleAssetStrategy` (`ma_crossover`, `rsi_reversal`, `donchian_breakout`, `keltner_breakout`, … under `trend`/`mean_reversion`/`momentum`/`volume`/`composite`). `ZScoreReversion` doesn't fit and stays its own bespoke `Strategy`.

**Sizing.** `value_frac(m)` with `m` = current value of **position-sizing indicator** (via `position_sizing(...)`, default `Value::new(1.0)`). Magnitude only, read on transitions, folded into readiness gate. Sized against equity so one `set` reverses and re-sizes exactly — no `enter_all_in`, no scale-in/out primitives.

<a id="book-anchor"></a>**Book anchor.** Alongside `Position`, `SingleAssetStrategy` owns shared `Book` (`Rc<RefCell<BookState>>`, `src/indicators/book.rs`) tracking strategy-lifetime state (cash, position units, MTM equity, running peak, per-bar returns, closed-trade summaries staged pending → active so per-close accessors emit `Some` only on closing bar). Six `BookField` accessors: `book.equity()`/`.equity_peak()`/`.drawdown()`/`.return_per_bar()`/`.trade_pnl()`/`.trade_return()`. Rolling `Sma`/`StdDev` over `book.trade_return()` reads "over last N closed trades". `with_initial_equity(sym, cash)` is what CLI `--cash` threads through (default `1.0`).

`PairsStrategy` owns `Book<Sym>` similarly (`with_initial_equity(left, right, cash)`); both legs feed one cash and mark to market together.

**`BasketStrategy<Sym>`** (`strategies/basket.rs`) — N-symbol cross-sectional. Reads whole `Snapshot<Sym>` each bar: scores every symbol via per-symbol scoring source, applies **selection closure** (`Fn(&HashMap<Sym, Real>) -> HashMap<Sym, Side>`).

- **Selection.** Ready-made: `strategies::basket::{top_bottom, threshold, quantile}` + builders `.top_bottom(longs, shorts)`/`.threshold(long_min, short_max)`/`.quantile(long_q, short_q)`. `.selection(closure)` = escape hatch.
- **Floating universe.** Two factories (`Fn(&Sym) -> impl Indicator<Input=Snapshot<Sym>, Output=Real> + 'static`) called once per symbol on first sight, rooting leaves via `Pick::matching(Selector::by_symbol(sym.clone()))`.
- **Declared universe** (opt-in): `.all_of([sym, ...])` strict — panics when a listed symbol is absent on any bar, gates `is_ready()` on every listed symbol scoring *and* sizing `Some`; `.any_of([sym, ...])` lax — restricts discovery to the listed subset but silently skips absent / unready members (readiness stays `true`, filtered inside `trade`). Both filter non-listed symbols out at discovery (no chain built). Enum lives at `strategies::basket::Universe` (`Floating` default).
- **Sizing.** Per-leg `ValueFraction`, **no auto-normalization** — use `sizing::equal_weight(n_legs)` for 100% gross.
- **Costs** stay on wallet — `PaperWallet::set_costs_for(sym, ...)`.
- **Per-symbol readiness.** Under `Floating` / `any_of`: `is_ready() = true` unconditionally; enforced inside `trade()` by only ranking symbols whose score read `Some` this bar. Under `all_of`: `is_ready()` blocks until every listed symbol has both scored and sized `Some` — the driver skips `trade` while the universe warms.
- **`rebalance_on(signal)`** (default `Every::new(1)` — every bar). Gates the whole selection + resize step; a less-frequent cadence (`!every 20` for weekly on a daily strategy, or a drawdown-triggered signal) lets the basket hold its picks between rebalance events. Because basket's selection *is* the sizing decision, gating selection is the natural rebalance semantics — the same knob shape as on the other strategies but with a fire-every-bar default to preserve pre-refactor behavior.
- **State.** Per-symbol `Position` (+ per-symbol per-leg protective chains lazily built on first sight — reference the symbol's own Position, same shape as MultiAssetStrategy) + shared `Book<Sym>`. Seed `with_initial_equity(cash)`.
- **Dollar neutrality.** `.dollar_neutral()` (YAML `dollar_neutral: true`) scales per-symbol sizes at each rebalance so `Σ long_sizes == Σ short_sizes`; the smaller-side sum is the target gross-per-side (never levers up). A one-sided selection this bar skips the whole rebalance — the hedge is undefined.
- **Per-leg protective.** `.long_stop_loss(|sym, &Position| level)` / `.long_take_profit(...)` / `.short_stop_loss(...)` / `.short_take_profit(...)` per-symbol factories, plus YAML `long: { stop_loss: ..., take_profit: ... }` / `short: { ... }` using `BasketSideSpec` templates with `!arg SYM` and `!entry` / `!peak` / `!trough` anchored to *that* symbol's Position. Idempotent re-submit on each fire, `cancel_protective` on flatten.
- **Transitions** = market orders, only when target side differs.
- **Not shipped**: Python bindings.

**`MultiAssetStrategy<Sym>`** (`strategies/multi_asset.rs`) — N-symbol **independent** portfolio (not cross-sectional): every symbol runs the same `SingleAssetStrategy`-shaped decision in isolation — same four signal slots, same protective-level slots, same sizing — and any subset can be long / short / flat at once. Sibling to `BasketStrategy`; reach for it when a symbol's fate depends only on its own signals, not on how it ranks against the rest.

- **Factories.** Every slot is a per-symbol factory (`Fn(&Sym) -> Signal` / `Fn(&Sym) -> Real source`), plus level factories that additionally receive the per-symbol `Position` (`Fn(&Sym, &Position) -> Level`) so `position.entry()` / `.peak()` / `.trough()` inside compose exactly as on `SingleAssetStrategy`. Sizing factory takes `&Sym` only. Chains are built lazily on first sight, filtered by [`Universe`](#basket-universe).
- **Universe** knob (reused from `basket::Universe`): `.all_of([...])` strict / `.any_of([...])` lax / floating default. Same semantics as basket — declared universes filter symbol discovery and, under `all_of`, panic on missing + gate `is_ready()` until every listed leg is past its own `stable_period`.
- **Per-symbol readiness.** Under floating / `any_of`: a symbol trades once *its own* chains have settled (gated inside `trade`); under `all_of`: `is_ready()` blocks until every listed leg is past its own `stable_period`.
- **State.** Per-symbol `Position` + shared `Book<Sym>` (aggregate equity across all legs). Same book-anchored sizing recipes apply.
- **`rebalance_on(signal)`** (default `Const::false` — never). On fire, resizes every held per-symbol position to its current sizing target. Same knob shape as single/pairs. Entry/exit signals fire every bar regardless.
- **Not shipped** (yet): `optimize` support (bails), Python bindings, YAML trailing-risk wrapping.

**`Portfolio<Sym>`** (`src/portfolio/`) — composite `Strategy<Input=Snapshot<Sym>, Symbol=Sym>` that runs N heterogeneous child strategies through a shared **`PortfolioWallet<Sym>`**, one `PaperWallet` per child behind an aggregate outward view. Plugs into `backtest::run` unchanged: driver sees a normal `Wallet<Sym>`, gets a normal `RunReport<Sym>` (aggregate equity curve + concatenated blotter), so every metric / windowing / walk-forward reduction falls out for free. Sibling to `BasketStrategy` / `MultiAssetStrategy`; reach for it when a portfolio combines *different* strategies rather than the same shape across many symbols.

- **Composition.** `Portfolio` + `PortfolioWallet` share their interior via `Rc<RefCell<PortfolioInner<Sym>>>` — same pattern as `Position` / `Book`. Caller does `let mut wallet = portfolio.wallet_view(); backtest::run(&mut portfolio, &mut wallet, snaps)`. `Portfolio::trade(_wallet)` ignores its `&mut dyn Wallet` argument and reaches through the shared inner instead — the passed wallet is a live-broker seam handed by the driver, but for a well-formed run it's just `portfolio.wallet_view()`.
- **Per-child seam.** Inside `trade`, each child sees a `SubWalletHandle<Sym>` implementing `Wallet<Sym>` — reads (`funds`/`equity`/`position`/`price`) delegate to *that* child's sub-wallet (so `value_frac(1.0)` sizes against the child's allocated equity, not the aggregate), mutations forward to the same sub-wallet.
- **Fill routing.** Sub-wallets each mint their own `OrderId` starting at 0, so ids collide across subs. `SubWalletHandle` mints a portfolio-wide id on every submit and records `(sub_idx, sub_local_id) → portfolio_id` in `PortfolioInner::sub_to_pf` + `portfolio_id → sub_idx` in `owners`. `PortfolioWallet::update` fans to every sub, translates fill ids on the way out, and `Portfolio::on_fill` uses `owners` to dispatch each fill to *only* its owning child (a stop firing on child A's position never reaches child B's `on_fill`).
- **`WeightPolicy` (`portfolio::policy`).** `trait WeightPolicy { observe(&mut, &[ChildSample]); weights(&self, n) -> Vec<Real>; warm_up_period() -> usize; reset(&mut) }`. Two built-ins ship: **`Fixed(Vec<Real>)`** and **`EqualWeight`**. `weights(n)` is queried at build to split the initial cash budget across children *and* re-queried on every rebalance-fire bar to compute new target equities. The `observe` hook is called every bar with per-child `equity`/`funds` samples — reserved for adaptive policies (inverse-vol, performance-weighted) that need rolling stats.
- **`rebalance_on(signal)`.** Default is `Const::false` — never rebalance, drift with P&L (opt-in). Wire a snapshot signal (`Every::new(28)` for monthly on daily bars, `!or [!every 28, drawdown_gate]` for scheduled + drawdown-triggered) via `.rebalance_on(...)` on the builder to turn on the loop. Ordering: children `trade()` first (against pre-rebalance equity for `value_frac` sizing), then one rebalance cycle runs. Each cycle is two phases: **cash phase** shifts free cash between sub-wallets (contributors donate `min(|Δ|, funds)` via `Wallet::adjust_funds`, receivers split the pot proportionally); **position phase** covers what cash couldn't — for each contributor with residual shortfall, submit proportional `set_position` scale-downs across its held positions so the freed cash lands next bar and can be donated on the following fire cycle. Convergence takes one fire when cash phase covers the whole rebalance (a partial-sizing strategy with cash headroom); when contributors are fully invested, it takes an extra fire per cycle to drain the freed cash through cash phase. A contributor's own `on_fill` sees each downsize and its next `trade()` call may react per its own logic — book-anchored sizing recipes (`equity_vol_target`, `fractional_kelly`, `drawdown_throttle`) naturally respect the post-rebalance equity; strategies that hard-target 100% invested (a naked `buy_and_hold`) will re-enter and undo the rebalance on the next bar.
- **State.** `PortfolioInner<Sym>` owns `subs: Vec<PaperWallet<Sym>>` + id-tracking tables + optional `TradingCosts` cloned per sub (see the `Clone` impl on `Box<dyn CommissionModel/SpreadModel/SlippageModel>` via `clone_box`). Portfolio itself owns children, policy, rebalance signal, bars_seen, and the seed allocations captured at build.
- **Wallet-trait discipline.** Both rebalance phases go through the `Wallet<Sym>` trait — cash phase calls `Wallet::adjust_funds(delta)` (paper impls it as a direct funds mutation; live impls override selectively via the venue's deposit / withdrawal / sub-account transfer API, else inherit the default `Err(UnsupportedOperation)`), and position phase submits scale-downs via `Wallet::set_position`. Portfolios of wallets that don't support `adjust_funds` fall through cleanly: unsuccessful debits fold into the contributor's shortfall so the position phase still closes the gap via `set_position`, and if a receiver credit fails after contributor debits succeeded, the pot is refunded symmetrically to keep total equity conserved. Position phase is universally supported (uses only `set_position`), so live-broker portfolios without cash-transfer support fully rebalance through orders.
- **Readiness.** `is_ready() = bars_seen >= policy.warm_up_period() && bars_seen >= rebalance.stable_period() && every child is ready` — the safe-defaults rule aggregated over every leg *and* the rebalance signal.
- **YAML.** `portfolio:` prefix + `PortfolioSpec` (`src/cli/spec/portfolio.rs`) mirror the composite. `children:` is an ordered list of `{ name, strategy }` slots where `strategy:` accepts any of the four shapes (single / pairs / basket / multi) routed by distinctive top-level key (`left`+`right` → pairs, `selection` → basket, `symbol` → single, otherwise multi; a preset tag like `!ma_crossover` also picks single). `weights:` is optional (`!fixed [w1, …]` or `!equal_weight`, default equal). `rebalance_on:` is optional (any `SignalSpec` — `!every N`, `!never`, compound signals — default `!never`); the signal-anchor and `Book` handed to `SignalSpec::build` at the portfolio level are dummies (`Position::new()`, `Book::new(initial_equity)`), so `!entry` / `!drawdown` inside a portfolio-level rebalance signal read empty — use snapshot / calendar / cadence signals instead. The natural way to reuse one child spec N times with distinct params is `!import { path, params }`, since it isolates each import's `!param` names from the outer table. Wired into `run.rs` (`run_portfolio`) and `optimize.rs` (`Portfolio` arm on `run_multi_symbol` + walkforward — `DynPortfolio` captures per-child + rebalance-signal stable/warm at build for the layout probe). Costs: the unscoped `--costs` default is installed as every sub-wallet's fallback bundle at build; per-symbol scoped `--costs SYM:…` bundles are then installed on every sub via `Portfolio::install_costs_for(sym, ...)` (walks `universe_from_snapshots` and calls `PaperWallet::set_costs_for` on each sub), so whichever child ends up filling a given symbol books at the right rate. Verified by `install_costs_for_scopes_by_symbol_across_sub_wallets` in `tests/portfolio.rs`.
- **Not shipped** (yet): inverse-vol / performance-weighted `WeightPolicy` built-ins, Python bindings, alternative position-phase policies (e.g. unwinding weakest performers first rather than proportionally).

`src/strategy.rs` carries only the `Strategy` trait. `Wallet` vocabulary lives in **`src/wallet.rs`** so downstream broker crates don't drag `Strategy` machinery.

- **`Wallet<Sym>`** (`&mut dyn`) — single **seam** to downstream execution. Priced **from outside**: `update(symbol, candle) -> Vec<Order>` feeds bar per tick (`close` marks, `[low, high]` bounds fills), returns fills booked. Query: `funds()`/`position(&Sym)`/`price(&Sym)`/`equity()`.
- **Submitting ≠ filling.** Market moves (`set_position`, `set` (Side + Size, opposite reverses), `close`) return `Ack::Filled(Order)` or `Ack::Working(OrderId)`. Resting protective: `set_stop`/`set_take_profit` (idempotent, latest-wins; wallet reads side from position) + `cancel_protective(&sym)`.
- **`PaperWallet` timing.** Queues market moves, fills at *next* bar's `open`; protective fill when bar trades through trigger (at level, or `open` on gap). Backtest never fills on signal's own bar.
- **Errors.** `WalletError` (`UnknownPrice`, `InvalidPrice`, `PriceOutOfRange`, `InsufficientFunds`, `UnsupportedOperation`) returned by live impl; `PaperWallet` silently drops infeasible queued fills. `UnsupportedOperation` is the default return of `Wallet::adjust_funds` — impls override selectively when their venue supports the operation.
- **No explicit-price primitive on trait, no `trade(delta)`** — scale-in is `set_position(position + delta)`.
- **Unit-tagged amounts.** `Reference(Real)` (quote/funds), `Units<Sym> { symbol, amount }`. `Order<Sym>` = `{ symbol, side, units, price, kind, id }`; `OrderKind` = `Market`/`Stop`/`TakeProfit`. `Order::from_delta(...)` returns `None` within `DEFAULT_EPSILON`.
- **`PaperWallet<Sym>`** — in-memory: market moves queue one per symbol (latest wins); resting stops register one bracket. `update` marks bar, flushes queued at `open`, matches resting against `[low, high]` (stop precedence; fill flattens+OCO-cancels bracket). Resting fill price provably in `[low, high]` so `PriceOutOfRange` unreachable.
- **`Ack<Sym>`** (`Filled(Order) | Working(OrderId)`), **`OrderId(u64)`** wallet-minted. Execution **synchronous**; live fills between bars reach strategy via `on_fill`.
- **`Size`**: `Units(n)`, `FundsFraction(f)` (`f·funds/price`), `ValueFraction(f)` (`f·equity/price`; `1.0` flips cleanly on reversal), `PositionFraction(f)` (`f·|position|`, adjust-only). Direction from `Side`.
- No `Market` trait: wallet holds prices. Python binds `PaperWallet`/`Order`/`Size` (sides `"buy"`/`"sell"`; `WalletError` → `ValueError`).

### Run driver (`src/backtest.rs`)

**`fugazi::backtest::run(&mut strategy, &mut wallet, snapshots) -> RunReport<Sym>`** walks a `Strategy` over a snapshot stream through any `impl Wallet<Sym>` (live too).

**Per-bar.** For each tagged entry in `Snapshot<Sym>` (`(symbol, freq, atom)` where `symbol: Some`): `wallet.update(symbol, atom.candle)`, route fills to `on_fill`, append bar-tagged to blotter. Untagged entries skipped for wallet pricing but visible to strategy. Then `strategy.update(snap)` → `strategy.trade(wallet)` iff `is_ready()` → push `wallet.equity().0` to curve.

`run<Sym, S, W, I, A>` where `A: Into<Snapshot<Sym>>`. `Vec<Atom>`/`Vec<Candle>` produce untagged size-1 snapshots; single-series callers use `Snapshot::single(sym, atom)`. `RunReport<Sym> { equity_curve, fills, initial_equity }` — `fills` are `Fill<Sym> { bar, order }`. `Fill`/`RunReport` re-exported at crate root; `run` namespaced.

### Metrics — one function per metric (`src/metrics.rs`)

**No aggregate `compute`.** Every metric is its own `pub fn`. Three **intermediate builders**:

- **`per_bar_returns(equity, initial_equity) -> Vec<Real>`** — for return-moment / risk-adjusted.
- **`reconstruct_trades<Sym>(fills) -> Vec<Trade>`** — walks blotter with signed position and volume-weighted entry; one closed leg = one `Trade { entry_bar, exit_bar, side, units, entry_price, exit_price, pnl, return_ratio }`.
- **`drawdown_segments(equity) -> Vec<DrawdownSegment>`** — one peak→trough→recovery per drop; `{ peak_bar, trough_bar, depth_ratio, duration_bars, underwater_bars }`.

**Catalogue.** Return moments (`total_return`, `cagr`, mean/median/stddev/best/worst_return, `positive_bars_ratio`, `skewness`, `kurtosis`, `value_at_risk`/`conditional_value_at_risk`, `tail_ratio`, `annualized_return`/`_volatility`); risk-adjusted (`sharpe`, `sortino`, `calmar`, `omega`, `ulcer_index`, `ulcer_performance_index`); Sharpe corrections (`probabilistic_sharpe` — Bailey/LdP 2012; `deflated_sharpe` — 2014 against expected max-SR under normal null across `n_trials`; `*_from_stats` variants take pre-aggregated stats so `optimize` computes DSR per row without re-scanning); drawdown (`max_drawdown`/`_duration`, `average_drawdown`/`_duration`, `drawdown_count`, `time_in_drawdown_ratio`, `recovery_factor`); trade-level (`total_trades`, winning/losing/flat/long/short_trades, `max_consecutive_wins`/`_losses`, `win_rate`, `profit_factor`, `payoff_ratio`, `expectancy`, `kelly_fraction`, average/largest_win/loss, `average_trade_return`, `average_bars_held`/`min`/`max`, `exposure_ratio`).

**Values in natural units** (`0.15` = +15%). Vanishing-denom ratios return `Option<Real>`; always-defined ones return `Real` (`0.0` on empty). PSR/DSR use `statrs`.

**Library core stays lean** — no `Metrics` struct in library. **No plotting.** CLI emits data files only: `fills.csv` (one row per booked order), `trades.csv` (one row per closed round-trip via `reconstruct_trades`; header-only when nothing closes, e.g. buy-and-hold), `returns.csv`, `metrics.yml`; under `-w LEN` also `metrics.csv` + `rolling.csv`.

**CLI `Metrics` document** (`src/cli/metrics.rs`) carries serde derives + YAML names (`sharpe`, `max_pct`, `annualized_mean_pct`). Populated by `metrics::from_report<Sym>(&RunReport<Sym>, bars_per_year, risk_free_rate) -> Metrics`. Downstream in CLI:

- **`MetricKey`** — validated-once dotted-path handle; `from_name(name, sample)` + `.resolve(&Metrics)`.
- **`report_slice`** — sub-run over bar range; shared measurement primitive.
- **`windowed_from_report`** / **`rolling_from_report`** — twin reductions (non-overlapping vs rolling stride-1). Under `-w LEN`, `run` emits both.
- **`optimize -w`** uses only non-overlapping: each `-m` becomes `_mean`/`_std` columns; `--best-by` ranks by mean shifted by `-k/--risk-aversion` stddevs, direction-aware.
- **`optimize --walkforward IS,OS[,Embargo]`** — rolling WFO, mutex with `-w`. Grammar per `WindowSpec` × 3. Grid-wide `max(stable_period)` skipped at head (opt-out `--keep-unstable`); fixed IS, stride OS, last fold's OOS absorbs trailing bars. One backtest/row, `report_slice`/fold → IS/OOS metrics; `--best-by` picks winner; composite OOS = stitched winners' OOS slices, running-total scaled. Emits `-o out/wf.csv` + sibling `.composite_oos_equity.csv` + `.composite_oos_metrics.yml`. Embargo drops OOS-metric bars only (state rolls). Pairs/basket rejected.
- **`selection.deflated_sharpe` on `optimize`** — per-row DSR against grid-wide null (`N` = trials, `Var[SR]` = sample variance of grid's annualized Sharpes). Omitted if <2 rows have defined Sharpe or trial variance is zero.

`Trade`/`DrawdownSegment` re-exported at crate root.

### Generic transform ops (`src/indicators/ops.rs`)

Source-wrapping carriers driven by operator types (new op = trait impl, not new type):

- **`Combine<L, R, Op>`** (binary, `BinaryOp`): one carrier for all binary ops, generic via `Lhs`/`Rhs`/`Output`, op **by value**. Serves arithmetic `Add`/`Sub`/`Mul`/`Div` (`Div` → `None` on /0), comparisons (op carries epsilon), boolean logic. Needs `Op: Default`; comparisons get `with_epsilon`.
- **`Lookback<I, Op>`** (unary, `LookbackOp`, zero-sized markers): `Lag`, `Diff`, `Ratio`, `Roc`.
- **`Extreme<S, Op>`** (rolling, `ExtremeOp`): `RollingMax`/`RollingMin`.

**`IfElse<Cond, T, F>`** — three-source ternary. `Cond: Indicator<Output=bool>` picks: `Some(true)` → `if_true`, `Some(false)` → `if_false`, `None` propagates. All three advanced every bar (never short-circuited). `warm_up_period()`/`stable_period()` report max across three (safe worst case), but **first `Some` can arrive earlier** — cond + selected branch settled is enough. Intentional; `IfElse` is excluded from `tests/warm_up.rs` exact battery. Fluent `.if_else(t, f)` on `BoolIndicatorExt`. YAML: `!if_else { cond, if_true, if_false }`.

### Shared cores (`pub(crate)`)

Bare `Real -> Real` math, **no source, no `Indicator` impl**, shared:

- `smoothing.rs`: `EmaState`, `WilderState` (mean-seed). `Ema`/`Macd` use `EmaState`; `Rma` uses `WilderState`; `Rsi` uses two; `Atr` = `TrueRange` + `WilderState`; `Adx` uses four.
- `stats.rs`: `WindowStats` (sum + sum-of-squares → mean/variance/stddev) backs `Sma`/`StdDev`/`Bollinger`; `WindowExtreme<Op>` (monotonic-deque) backs `Extreme`/`RollingMax`/`RollingMin`/`Stochastic`.

<a id="the-two-provider-traits"></a>### Remote providers — two traits by shape (`src/sources/`)

Behind `sources` feature. Both `Send + Sync`, use RPITIT (`impl Future`), take **objects/enums, not strings**, share one `SourceError` and `Interval`.

- **`CandleSource`** — `atoms(symbol, interval, since, until) -> Vec<Atom>`, ascending by `time`. Impls: `Binance` (`binance_schema()`: `quote_volume`/`n_trades`/`taker_buy_base_volume`/`taker_buy_quote_volume`), `Yahoo` (`yahoo_schema()`: `adj_close`). Every atom in one fetch shares `Arc<Schema>` — pick with `schema_of`.
- **`OverlaySource`** — `overlays(symbol, interval, since, until) -> Vec<OverlayRow>`, plus `schema()` stable before fetch. For **per-bar side-channel data with no OHLCV**. Impls: `CoinGecko` (`coingecko_schema()`: `price`/`market_cap`/`total_volume`/`circulating_supply`, last derived) and `CoinMarketCap` (`coinmarketcap_schema()`: `price`/`volume_24h`/`market_cap`/`circulating_supply`/`total_supply`; **paid tier only**, auth via `X-CMC_PRO_API_KEY` from `CMC_PRO_API_KEY`).

**Why two traits, not a flag.** `Atom::candle` isn't `Option`, and `Wallet::update` marks price from bar fed — so overlay-through-`CandleSource` would synthesise a candle flowing into `Current::close()` and MTM. `OverlayRow { time, overlays }` has no candle *field*, mistake unrepresentable. A provider with both impls both. `OverlayRow` equality/ordering **by `time` alone**.

**How overlay data reaches a strategy.** Fetch overlay to its own CSV, `--series` joins by `(symbol, time)`. Read with `!get { key: market_cap }`:

```text
fugazi get binance:BTCUSDT[1d]                       -o prices.csv
fugazi get cg:BTCUSDT=bitcoin[1d]                    -o caps.csv
fugazi run @strategy.yml -s @prices.csv -s @caps.csv -o out/
```

`OUT=QUERY` remap lines up join key. Cross-sectional `BasketStrategy` is the natural consumer.

**CoinGecko specifics.** `market_chart/range` picks granularity from window length (~5-min ≤1d, hourly ≤90d, daily beyond). Client rejects sub-hourly, paginates hourly in 80-day windows, buckets onto requested cadence keeping **first** sample per bucket. Weekly floors to Monday, monthly to 1st via calendar (epoch day 0 = Thursday would silently break Monday joins). `User-Agent` **mandatory**. Public tier serves **last 365 days** only. `COINGECKO_API_KEY` = demo key.

## Safe defaults, opt-in overrides

Numbers during warm-up or IIR settling are *unsettled*. Every knob that could paper over an unsettled bar biases toward **waiting**, with one named opt-out:

- **Strategy readiness.** `Strategy::is_ready()` gates `trade()`; `SingleAssetStrategy` overrides. Opt-out: `Unstable<S>` (`.unstable()` / YAML `!unstable { source | signal }` / Python `.unstable()`).
- **Position sizing.** `position_sizing(indicator)` (default `Value::new(1.0)`) scales `value_frac`. `None` from sizing indicator *skips whole `trade()` call*. Five recipes in `fugazi::indicators::sizing`: **price-based** — `vol_target(target, window, bars_per_year)` / `atr_risk(risk_frac, period, atr_multiple)` (single-asset shortcuts; both read the strategy's own asset via `Pick::<Sym>::new()` and panic on multi-symbol snapshots — inside a `BasketStrategy`, use the source-generic twins `vol_target_of(source, ...)` / `atr_risk_of(source, ...)` with a per-leg `Pick::matching(Selector::by_symbol(sym.clone()))`, or in YAML `!vol_target { source: !pick { symbol: !arg SYM }, ... }`); **book-anchored** — `drawdown_throttle(&book, max_drawdown)` (clamped `[0,1]`), `equity_vol_target(&book, target, window, bpy)`, `fractional_kelly(&book, kelly_fraction, window)` (clamped `>= 0`). YAML tags: `!vol_target`/`!atr_risk`/`!drawdown_throttle`/`!equity_vol_target`/`!fractional_kelly`.
- **`fugazi get` overlays.** CLI trims each column's pre-`stable_period()` cells. Opt-out: `--keep-unstable`.
- **`-w/--windowed` duration form.** `-w 1d`/`-w 1w`/… demands explicit `AssetClass` (`--stocks`/`--forex`/`--crypto`) and resolvable bar cadence. Opt-out: plain bar-count `-w N`.
- **Explicit periods.** Windowed constructors take explicit `period` (`> 0`); risk-adjusted metrics take explicit rf-rate and bars-per-year.

Adding a knob that touches unsettled data: safest default, one opt-out.

## Conventions and gotchas

- **Composition is construction.** New "X of Y" takes source `S` in `new` (or `of` for source-generic leaves) with right output constraint. Don't add pipe combinators.
- **Use the cores, not each other's public types.** Internal smoothing uses `EmaState`/`WilderState`; `Rma<S>`/`Ema<S>` wrap a *source* and can't smooth inline values.
- **Adding an operator**: `*Op` type impl'ing trait plus type alias — never a macro. Arithmetic/boolean/lookback are zero-sized `Default` markers; comparisons carry `epsilon`.
- `Combine` feeds *same* input to both sides, requires `Input: Clone`. Use `lhs`/`rhs` naming. Holds op **by value**; `Lookback`/`Extreme` hold zero-sized op as `PhantomData<fn() -> Op>`. Marker leaves use `PhantomData<fn(I)>` / `fn() -> F` for constraint rules (avoids E0207); `Identity<I>` uses `PhantomData<fn(I) -> I>`.
- `Change` is **bidirectional** toggle detector; directional events come from pairing with comparison.
- Constructors `assert!(period > 0, ...)`; document warm-up; implement `warm_up_period()` to match exactly (plus `unstable_period()` when smoothing recursively).
- Comparison/edge is **`None` until** every source is warmed; `And`/`Or` `None` until both ready — so an edge coincident with warm-up isn't detected (no spurious first-bar trade).

## CLI internals (`src/cli/`)

One binary (`fugazi`); layout by concern:

- **`main.rs`** — clap defs, subcommand dispatch.
- **`run.rs`, `optimize.rs`, `backtest.rs`** — user-facing drivers sit on pure `backtest` (`run_iteration`, `evaluate`, `evaluate_windowed`). `backtest.rs` owns no IO.
- **`get.rs`** — `fugazi get`. Grammar: `<provider>:[OUT=]<symbol>[[OFREQ=]<freq>,...]`. **Left = emitted, right = fetched.** `OUT=` decouples emitted `symbol` from provider id (`cg:BTCUSDT=bitcoin[1d]`) — makes `--series` join line up. `OFREQ=` decouples emitted `freq` from fetched cadence; **relabels, doesn't resample**. Two pipelines by `resolve_mode`, **never mixed**: `run_candles` (OHLCV + `-x`) and `run_overlay_columns` (`OverlaySource`, no OHLCV; `-x` rejected). `get --params` resolves `!param` inside `-x/--overlay`.
- **`spec/`** — YAML mirror of composition API:
  - `expr.rs` — `ExprSpec` (value-producing enum; polymorphic over `DynType` for `!current`/`!pick`/`!time`/`!get`/`!if_else`/`!value`); `default_source`/`default_high`/`default_low`/`default_bar_source` helpers; **`ValueLit`** — `!value` payload, number (→ `Value`, `Real`) or string (→ `ValueStr`, `Str`; quoting picks type). Uses `serde_norway::Value` bridge (`#[serde(untagged)]` buffering can't see YAML tags).
  - `signal.rs` — `SignalSpec` + `StrOperand` (rhs of `!str_eq`/`!str_ne`).
  - `template.rs` — `SpecTemplate<T>`: captures raw `serde_json::Value`; `.build(&args)` runs `!arg` then typed-parses. Untagged in YAML. Two-pass: `!param` at load, `!arg` each `.build()`. Keyed on distinct singleton-object keys.
  - `strategy.rs` — `SideSpec`, `SingleStrategySpec`, `DynSingleStrategy`.
  - `preset.rs` — `StrategyPreset` (externally-tagged: `!buy_and_hold`/`!ma_crossover`/`!rsi_reversal`/`!donchian_breakout`/`!keltner_breakout`) and `StrategyRef` (`Spec | Preset` bridge). `optimize` = `SingleStrategySpec`-only.
  - `trailing.rs` — `!sharpe`/`!sortino`/`!volatility`/`!max_drawdown`/`!calmar`. Wraps non-`Clone` `Sharpe<S>` etc. in `RebuildIndicator` rebuilding on clone. `strategy:` is `AnyStrategyRef` (`Single | Pairs | Basket`); bridge routes by distinctive key.
  - `pairs.rs` — `PairsStrategySpec`, `DynPairsStrategy`.
  - `basket.rs` — `BasketStrategySpec` + `SelectionRuleSpec` (`!top_bottom`/`!threshold`/`!quantile`) + `UniverseSpec` (`!all_of [sym, ...]` / `!any_of [sym, ...]`). Fields: `selection`, `score: SpecTemplate<ExprSpec>`, `sizing: SpecTemplate<ExprSpec>`, optional `universe: UniverseSpec` (default floating). `.build(initial_equity, schema)` clones templates into per-symbol factories resolving `!arg SYM`, installs universe on the concrete `BasketStrategy`. `!entry`/`!peak`/`!trough` read dummy `Position` in score/sizing (always `None`). Shared `Book` wired: book-anchored sizing recipes work per-symbol against aggregate. `!equal_weight <N>` = sugar. Wired into `run.rs` (`run_basket`) and `optimize.rs` (via `run_multi_symbol` — sweeps + walkforward).
  - `multi_asset.rs` — `MultiAssetStrategySpec` + `MultiSideSpec` (`enter: SpecTemplate<SignalSpec>`, `exit`, `stop_loss`, `take_profit`), `sizing: SpecTemplate<ExprSpec>`, optional `universe: UniverseSpec` (reused from `basket.rs`). No `symbol:` field — multi-asset runs across many by construction. `.build(initial_equity, schema)` wires per-side signal / level / sizing factories on the concrete `MultiAssetStrategy`; long/short side factories are called once per symbol on first sight, protective-level factories additionally receive the per-symbol `Position`. Wired into `run.rs` (`run_multi` + `run_iteration_multi`) via the `multi:` prefix and `optimize.rs` (`Multi` arm on `run_multi_symbol` — sweeps + walkforward).
  - `portfolio.rs` — `PortfolioSpec` + `PortfolioChildSpec` + `PortfolioChildStrategy` (`Single | Pairs | Basket | Multi`, routed by distinctive top-level key via `try_from = "serde_norway::Value"` — a widened cousin of `trailing::AnyStrategyRef`) + `WeightPolicySpec` (`!fixed [w1, …]` / `!equal_weight`, default equal). No `symbol:` field — portfolio names its children, each of which names symbols. `.build(cash, schema, costs)` splits the cash by policy, builds each typed child at its allocated share (so book-anchored sizing inside a child reads against the *child*'s slice, not the aggregate), captures each child's stable/warm-up periods **before** boxing (the erased `Box<dyn Strategy>` doesn't expose them — this is the only chance) so `DynPortfolio::stable_period` can aggregate for the `optimize --walkforward` layout probe. Costs (v1): the composite `PortfolioWallet` takes one uniform bundle, resolved unscoped from `--costs`. Wired into `run.rs` (`run_portfolio` + `run_iteration_portfolio` — which uses its own `measured_report_portfolio` since Portfolio must be driven through `wallet_view()`, not a `PaperWallet`) and `optimize.rs` (`Portfolio` arm on `run_multi_symbol` — sweeps + walkforward, both).
  - `mod.rs` — shared `load_value(text, params, base)` (`parse → !import → !param → typed parse`).
- **`costs/`** — `--costs`:
  - `spec.rs` — CLI-arg parsing into `CostSpec`; `CostTerm` + `split_top_commas`/`parse_term`.
  - `config.rs` — `CostConfig`, `LegConfig<T>`, `ScopedEntry<T>`, typed `CommissionSpec`/`SpreadSpec`/`SlippageSpec` (**externally tagged** — `!percentage { rate: 0.001 }`, never `kind: percentage`). Dotted `--costs` setter is *literal* address (`commission.percentage.rate=0.00075` nudges; `commission=!percentage { … }` replaces). Wrong variant = hard error. `MODEL_VARIANTS`+`is_model` let untyped passes recognize model nodes.
- **`dyn_indicator.rs`** — facade re-exporting **`fugazi::runtime`** (`DynIndicator` + `DynValue` (`Real | Bool | Atom | Candle | Str | Time | Snapshot<String>`) + `DynType` + `Adapter` blanket + `AsReal`/`AsBool`/`AsCandle`/`AsAtom`/`AsStr` + `chain`/`unstable_wrap`). **New YAML-visible indicators plug in via `dyn_indicator::wrap(...)`.**
- **`csv_source.rs`** — local CSV candle source for `fugazi get csv:PATH`.
- **`data.rs`** — `--series` data frame (`@file.csv` + inline, full-joined on `symbol`+`time`).
- **`overlay.rs`** — `--overlay` parsing for `fugazi get`.
- **`calendar.rs`** — `Frequency`, `AssetClass` (`trading_days_per_year`/`trading_hours_per_day`/`trading_seconds_per_bar`), `Scope`, `ScopedFrequency`, `parse_scope`/`parse_scope_parts`, **`WindowSpec`** (`-w`: `Bars(NonZeroUsize) | Duration(Frequency)`), **`parse_time_to_millis`** (RFC3339 / `YYYY-MM-DD [HH:MM:SS]` / epoch s or ms), **`detect_frequency_from_atoms`**.
- **`metrics.rs`** — CLI `Metrics` doc + `MetricKey` + `resolve_metric`.
- **`input.rs`** — `@file`-or-inline `Source`; **`base_dir()`** — dir relative `!import` resolves against.
- **`glob.rs`** — shell glob (`b*`/`*b*`/`?`/`[a-z]`/`[!abc]`/`\*`), **case-insensitive, whole-string**. Hand-rolled to avoid regex deps.
- **`imports.rs`** — `!import` pass: replaces `!import <path>` with whole imported doc. Paths **relative to importing doc**. Runs **before `!param`**. Cycles = hard error. Object form `!import { path: file.yml, params: { KEY: value, ... } }` resolves the imported subtree's `!param` placeholders against the inline table first (via `params::substitute_partial`); anything not covered falls through to the outer `--params` pass. Inline values are themselves subtrees — may nest `!import`s (resolved against the *outer* dir) or `!param`s (left for the outer pass). The natural way to import one shared child N times in a portfolio spec with N distinct parameterizations without name-clashing global keys.
- **`params.rs`, `args.rs`, `convert.rs`, `list.rs`, `completions.rs`, `pool.rs`, `style.rs`** — auxiliary. `params::substitute` and `args::substitute` share walker, differ only in sentinel key.

## Python bindings (`python/src/lib.rs`)

**Type-erased mirror** of Rust library (pyo3 cdylib, `fugazi-python` → `fugazi`). Python can't carry source generics across FFI, so everything is erase-then-dispatch via **`fugazi::runtime`** (`DynIndicator`+`DynValue`, plus `DynIndicatorSync` subtrait adding `Send + Sync` and deep clone via `runtime::wrap_sync`). Output-typed carriers = `TypedSource<In, Out>` newtypes: `Source<I>`, `StrSource<I>`, `AtomBox<I>`, `SignalBox<I>` (flattens warm-up `None` to `Some(false)`). Multi-output stays local as `DynMulti<I>`/`MultiBox<I>`. `AnySource`/`AnySignal`/`AnyMulti` record input domain (candle/value/snapshot-rooted); `map_source!`/`combine_sources!`/`source_to_signal!`/`sources_to_signal!`/`map_signal!`/`combine_signals!`/`map_multi!`/`combine_multi!` macros dispatch. **Rule: mirror constructors use those macros; never name concrete `Ema<Sma<Current, …>, …>`.**

### Parity discipline

**When a Rust API is added/extended/renamed, mirror it in `python/src/lib.rs` in the same PR** — drift is silent.

- **New indicator/signal/operator** → `#[pyfunction]`, register in `#[pymodule] fn fugazi`, smoke test in `python/tests/test_fugazi.py`. Single-output real-source use `src_period!`; bar-only `bar_period!`/`bar_noarg!`; multi-output `bar_period_multi!` or hand-written. New fluent method → `#[pymethods]` on `PyIndicator`/`PySignal`.
- **New metric fn** → `#[pyfunction]`, name to `register_metrics_module`. `Option<Real>` stays; `Real` → `f64`.
- **New field on `Trade`/`DrawdownSegment`/`Order`** → `#[getter]` on `Py*` + update `__repr__`.
- **New remote provider** → `Py*` client + register + `fetch(provider=…)` branch. `OverlaySource` `fetch` branch **redirects with error** (documented as candle frames).
- **Changes to `Candle`/`Atom`/`OverlayInfo`/`Schema`/`SchemaBuilder`** → update `Py*` field-for-field.

**Partially bound — single-asset builder + `run`.** `PyStrategy` mirrors `SingleAssetStrategy` — `Strategy(sym).long_on(...).short_on(...).position_sizing(src).run(wallet, candles)` → `PyRunReport`. `AtomLift` bridges candle-rooted Python signals to snapshot-rooted strategy layer. **Not bound** (don't add without asking): position-anchored protective levels (`Position` uses `Rc<RefCell>`, not `Send+Sync`), `PairsStrategy`/`BasketStrategy`, `src/strategies/` catalogue.

**Intentionally not bound**: `Strategy` trait as subclassable, `src/strategies/` recipes as ctors, `run_iteration`/`evaluate*`, trailing risk indicators, the CLI, `Wallet` trait (only `PaperWallet`), Rust-internal types (`Position`, `PositionField`, `Ack`, `OrderId`, `Reference`, `Units`).

Layout (grep by section header): type-erasing carriers → domain enums + macros → Python classes (`PyCandle`/`PySchema`/`PySchemaBuilder`/`PyOverlayInfo`/`PyAtom`/`PyIndicator`/`PySignal`/`PyMulti`/`PyWallet`/`PyOrder`/`PySize`) → strategy layer (`PyWallet` + `PyStrategy` + `PyRunReport`; `AtomLift`) → metrics (`PyFill`/`PyTrade`/`PyDrawdownSegment` + `#[pyfunction]` per metric; submodule injected into `sys.modules["fugazi.metrics"]`) → constructors (leaf sources, macro invocations, hand-written `macd`/`bollinger`/`keltner`/`donchian`/`stoch_rsi`, `resample`/`latch`, `unstable`, `get`) → remote sources (`PyBinance`/`PyYahoo`/`PyCoinGecko`/`PyCoinMarketCap`/`fetch`) → `#[pymodule] fn fugazi`.

Cargo: `python/Cargo.toml` depends on `fugazi_core = { package = "fugazi", … default-features = false, features = ["sources"] }`; `pyo3 = "0.29"` with `abi3-py39`. Test: `maturin develop` then `pytest python/tests/`. `test_readme.py` runs code blocks in `python/README.md`.

## Existing helpers — grep before writing new code

| Concern | Reuse | Location |
|---|---|---|
| Bracket-split `SYMBOL[FREQ]:` / full scope | `calendar::parse_scope_parts(text)` / `parse_scope(text)` | `src/cli/calendar.rs` |
| Interval token / Frequency / time-column ms | `calendar::parse_interval` / `Frequency::from_str` / `parse_time_to_millis` | `src/cli/calendar.rs` |
| Auto-detect bar cadence | `calendar::detect_frequency_from_atoms(...)` | `src/cli/calendar.rs` |
| Parse `-w` / `--walkforward` | `WindowSpec::from_str` + `.resolve(bar_freq, class)`; `WalkForwardSpec::from_str` + `.resolve(...) -> (is,oos,emb)` | `src/cli/calendar.rs` |
| Built-strategy readiness + full `RunReport` | `DynSingleStrategy::{stable_period, warm_up_period}` (→ `SingleAssetStrategy`); `backtest::measured_report(spec, atoms, cash, costs)` | `src/cli/spec/strategy.rs`, `src/cli/backtest.rs` |
| Trading seconds a bar of `freq` spans | `class.trading_seconds_per_bar(freq)` | `src/cli/calendar.rs` |
| Shared overlay schema of atom stream | `fugazi::sources::schema_of(&atoms)` | `src/sources/mod.rs` |
| Fetch OHLCV | `CandleSource::atoms(...)` — `Binance`, `Yahoo` | `src/sources/mod.rs` |
| Fetch per-bar cols no OHLCV | `OverlaySource::overlays(...)` — `CoinGecko`, `CoinMarketCap`. Don't bolt onto `CandleSource` | `src/sources/mod.rs` |
| Provider schemas | `*::*_schema()` (`OnceLock`) | `src/sources/{binance,yahoo,coingecko,coinmarketcap}.rs` |
| Join overlay CSV onto price CSV | Two `get` → two `-s`; `DataFrame::insert` full-joins | `src/cli/data.rs` |
| CSV delimiter probe | `csv_source::detect_delimiter(path)` | `src/cli/csv_source.rs` |
| Shell glob (case-insensitive, whole-string) | `glob::Pattern::from_str(pat)` + `.matches(text)` | `src/cli/glob.rs` |
| Load `@file` or inline; YAML → JSON value | `input::Source::{File, Inline}` + `.read()`; `input::parse_value(text)` | `src/cli/input.rs` |
| Load whole strategy doc | `spec::load_value(text, &params, base)`; `*StrategySpec::from_text_with_params_in` | `src/cli/spec/mod.rs` |
| Load-time `!param` / `!import` substitution | `params::substitute` / `imports::resolve(value, base)` | `src/cli/{params,imports}.rs` |
| Dir relative `!import` resolves against | `input::Source::base_dir()` | `src/cli/input.rs` |
| Build-time `!arg` substitution | `args::substitute(value, &args)` | `src/cli/args.rs` |
| Defer spec subtree until args ready | `SpecTemplate<T>` + `.build(&args)` | `src/cli/spec/template.rs` |
| Constant leaf: number or string | `!value 70` / `!value bull` | `src/cli/spec/expr.rs` |
| Three-source ternary | `IfElse::new(cond, t, f)` / `.if_else(t, f)` | `src/indicators/if_else.rs` |
| Multi-output accessor bodies | `component_accessors!` macro | `src/indicators/component.rs` |
| Real recurrence for internal smoothing | `EmaState` / `WilderState` | `src/indicators/smoothing.rs` |
| Windowed sum/variance/stddev; rolling extremum | `WindowStats` / `WindowExtreme<Op>` | `src/indicators/stats.rs` |
| Position tracking inside strategy | `SingleAssetStrategy::position()`; `BasketStrategy::position(&sym)` | `src/indicators/position.rs`, `src/strategies/*.rs` |
| Sizing recipes | `indicators::sizing::{equal_weight, vol_target, vol_target_of, atr_risk, atr_risk_of, drawdown_throttle, equity_vol_target, fractional_kelly}` (`*_of` variants take a caller-supplied atom source for the basket per-leg case) | `src/indicators/sizing.rs` |
| Cross-sectional rank → `Side` | `strategies::basket::{top_bottom, threshold, quantile}`; `.selection(closure)` | `src/strategies/basket.rs` |
| Declared basket universe (strict vs. lax) | `BasketStrategy::{all_of, any_of}` on the Rust builder; YAML `universe: !all_of [...] \| !any_of [...]` on `BasketStrategySpec` | `src/strategies/basket.rs`, `src/cli/spec/basket.rs` |
| Strategy-lifetime equity/trade tracking | `SingleAssetStrategy::book()`/`PairsStrategy::book()`/`BasketStrategy::book()` + `BookField` accessors | `src/indicators/book.rs`, `src/strategies/*.rs` |
| Composite Strategy over N heterogeneous children with per-child sub-wallets | `Portfolio::builder().add(name, strategy).weights(policy).rebalance_on(signal).costs(costs).build()`; `portfolio.wallet_view()` returns the aggregate `PortfolioWallet<Sym>` for `backtest::run` | `src/portfolio/mod.rs`, `src/portfolio/wallet.rs` |
| Portfolio YAML surface | `PortfolioSpec` (fields: `children`, `weights`, `rebalance_on`) + `PortfolioChildSpec` + `PortfolioChildStrategy` + `WeightPolicySpec`; `portfolio:` prefix on `run`/`optimize`; `backtest::{measured_report_portfolio, evaluate_portfolio, evaluate_windowed_portfolio, run_iteration_portfolio}`; runner `run::run_portfolio` | `src/cli/spec/portfolio.rs`, `src/cli/{backtest,run,optimize,input,main}.rs` |
| Portfolio weight policies (init cash split + rebalance target) | `portfolio::policy::{WeightPolicy, Fixed, EqualWeight, ChildSample}` | `src/portfolio/policy.rs` |
| Portfolio two-phase rebalance | `Portfolio::builder().rebalance_on(signal)` — each fire runs cash phase (`Wallet::adjust_funds`) then position phase (`Wallet::set_position` scale-downs for residual shortfall); phased design converges in one fire when cash covers, more fires when contributors are fully invested | `src/portfolio/mod.rs`, `src/portfolio/wallet.rs::rebalance_cash_to` |
| Clone a `TradingCosts` bundle (Box<dyn Model>) | `TradingCosts::clone()` — every model impls `clone_box(&self) -> Box<dyn Model>` and the `Box<dyn Model>: Clone` impl calls it | `src/costs/mod.rs` |
| Partial `!param` pass (only rewrites keys in table; leaves defaults/unknowns for outer pass) | `params::substitute_partial(value, &table)` — used by `imports::resolve` for `!import`'s inline `params:` | `src/cli/params.rs` |
| Resolve metric name once, reuse | `MetricKey::from_name(name, sample)` + `.resolve(&metrics)` | `src/cli/metrics.rs` |
| Wrap indicator as `DynIndicator` / zero unstable / typed view / chain | `runtime::{wrap, unstable_wrap, AsReal/AsBool/AsCandle/AsAtom/AsStr, chain}` | `src/runtime.rs` |
| Full-run backtest → `Metrics`; slice a report | `backtest::{evaluate, evaluate_windowed, run_iteration}`; `metrics::report_slice` | `src/cli/{backtest,metrics}.rs` |
| Returns / trades / drawdown segments from a report | `metrics::{per_bar_returns, reconstruct_trades, drawdown_segments}` | `src/metrics.rs` |
| Python: domain-preserving wrap / combine / bool build | `map_source!`, `combine_sources!`/`sources_to_signal!`/`combine_signals!`/`combine_multi!`, `source_to_signal!` | `python/src/lib.rs` |
| Python: register metric on `fugazi.metrics` | Add to `reg!(...)` in `register_metrics_module` | `python/src/lib.rs` |

**Rule:** if you're about to write a private helper whose name looks like something on that table, grep first.
