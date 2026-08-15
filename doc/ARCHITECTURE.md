# Architecture (deep reference)

Detailed internals for `fugazi`. [CLAUDE.md](../CLAUDE.md) is the load-bearing
summary — the invariants, conventions, and the "grep before writing" table. This
file is the depth behind it: read the relevant section before touching a
subsystem. [doc/CONTRIBUTING.md](CONTRIBUTING.md) is the change procedure.

Three composable layers: **indicators** (numeric sources), **signals**
(`Indicator<Output = bool>`), **strategies** (decision layer trading into a
wallet).

## Indicators — numeric sources (`src/indicator.rs`, `src/indicators/`)

`Indicator` has `Input`/`Output`, `update(&mut self, Input) -> Option<Output>`,
`value()`, `is_ready()`, `reset()`, `save_state()`/`load_state()` (default no-op;
see *Run resuming*), plus:

- **`warm_up_period()`** — *exact* samples before first `Some`. Wrappers add on
  top; binary carriers take max. `tests/warm_up.rs` asserts exactness — add new
  indicators to that battery.
- **`unstable_period()`** (default `0`) — extra samples IIR smoothers need for
  the seed's residual to decay below `SETTLE_TOLERANCE = 1e-3`. Wrappers sum into
  the source's.
- **`stable_period()`** = warm-up + unstable.

Output is `Option` (warm-up → `None`).

**Defining choice: price-series indicators own their source, generic over it** —
`Ema<S>`, `Sma<S>`, `Rma<S>`, `Rsi<S>`, `Macd<S>` where `S: Indicator<Output =
Real>`, `Input = S::Input`. Composition = nesting constructors:
`Ema::new(Current::close(), 20)`, `Ema::new(Sma::new(src, 10), 20)`,
`Rsi::new(Identity::new(), 14)`. **No pipe/`then`/`Chain`** — chaining *is*
construction.

### Leaf sources

`Value<I>` (constant), `Identity<I = Real>` (passthrough; `Identity::<Atom>::new()`
= default atom source), `Current` candle accessors (`Current::close()` /
`Current::volume()`; via `Field<F, S>` / `CandleField`). Every source-generic
leaf — `Field<F, S>` (`Close`/`High`/`Low`/`Open`/`Volume`/`Typical`/`Median`),
`CurrentBar<S>`, `Calendar<F, S>`, `CurrentTime<S>`, `IsWeekday<S>`,
`IsWeekend<S>` — is generic over `S: Indicator<Output = Atom>` (default
`Identity<Atom>`). `T::new()` uses the default, `T::of(source)` re-roots on a
custom source.

### Cross-asset sources (`time.rs` + `snapshot.rs` + `indicators/pick.rs`)

- **`Frequency`** — bar cadence enum (`Minute(u32)`/`Hour`/`Day`/`Week`/`Month`),
  totally ordered by duration, `FromStr` on `N<unit>` (`m`/`h`/`d`/`w`/`M`).
  `sources::Interval` is the provider-side twin.
- **`Selector { symbol: Option<String>, freq: Option<Frequency> }`** — partial
  key. `None` = wildcard; shorthands `by_symbol`/`by_freq`/`exact`.
- **`Snapshot<K>`** — newtype `HashMap<K, Atom>` with `get`/`insert`/`iter`/
  `FromIterator` + `sole_atom(&self)` (unique in size-1, **panics on 2+**, `None`
  on empty) + `lone_atom(&self)` (the **non-panicking** twin — `None` unless
  exactly one priceable entry; backs `Pick::rooted`'s fallback, where a 2+
  snapshot means "the blessed leg is absent this bar", not "mis-wired").
  `impl Snapshot<Selector>` adds `find(query)`.
- **`Pick<S = Identity<Snapshot<Selector>>>`** projects one asset: `Output =
  Atom`. Three modes: `Pick::new()` (empty selector → sole-atom, **panics** on
  2+); `Pick::matching(selector)` (strict structural match → `None` when absent —
  the explicit cross-asset form); **`Pick::rooted(selector)`** (match, else fall
  back to `lone_atom` — the *blessed-series* root a context installs for
  `source:`-omitted leaves; the fallback keeps untagged size-1 snapshots, i.e.
  `Vec<Candle>` drivers, resolving). `Pick::of(selector, source)` re-roots any of
  them. **`Atom` equality is by `time`**; `Ord` sorts chronologically with `None`
  first. **`PickAny<S = Identity<Snapshot<Sym>>>`** is the symbol-agnostic
  sibling: no selector, always returns the first entry's atom (`snap.any_atom()`),
  `None` on empty. Used by every calendar accessor's default source path.

Python: `ta.Frequency("1h")`, `ta.Selector(symbol="BTC", freq="1h")`,
`ta.Snapshot({...})`, `ta.pick(...)`. Snapshot keys accept `str`/`Frequency`/
`(str, freq)`/`Selector`.

### Position-anchored sources (`indicators/position.rs`)

The strategy owns a shared `Position` (`Arc<Mutex<PositionState>>` — the lock
stays uncontended in single-threaded use, ~5ns per access, and the composition is
`Send + Sync` for pyo3 and parallel-optimize use) with signed `size`, `entry`,
`peak`/`trough` since entry. Accessors `.entry()`/`.peak()`/`.trough()` return
`PositionField` (`Indicator<Output = Real>`, `None` while flat) — stops/targets
become expressions like `position.entry().sub(Atr::new(14).mul(...))`. Only
meaningful inside a strategy driving the `Position`.

### Bar indicators — consume whole `Candle`

`Atr`, `Adx`, `TrueRange`, range vol estimators (`Parkinson` H/L; `GarmanKlass`
adds O/C, clamped ≥0; `RogersSatchell` drift-independent — each rolls the mean of
a per-bar OHLC estimator via `WindowStats` then sqrt), volume (`Obv`, `Ad`, `Mfi`,
rolling `Vwap` over `period`). Cumulative ones (`Obv`, `Ad`) anchor at
construction; `reset()` re-anchors. **`Vwap` is rolling, not session-anchored** —
the crate has no session concept. Yang-Zhang absent — overnight gap meaningless on
24/7 crypto. Two-source: `Donchian<H, L>`.

### Windowed stats over Real

`Skewness`/`Kurtosis` (standardized moments; kurtosis raw, ~3 for normal),
`ZScore` (`(x−SMA)/stddev`), two-source `Correlation` (rolling Pearson;
autocorrelation via `Correlation::new(x.clone(), x.lag(n), period)`). O(1)/bar off
`WindowStats`/`WindowCovariance`. **`VarianceRatio` is the deliberate exception** —
Lo-MacKinlay regime classifier over first differences (`1.0` random-walk null,
`>1` trending, `<1` mean-reverting), **O(`period`)/bar** (retains window in
`VecDeque`). Asserts `lag ≥ 2`, `period ≥ lag + 2`.

### Calendar sources (`indicators/calendar.rs`)

Decompose `atom.time` (UTC ms): `Year`/`Month`/`Day`/`Hour`/`Minute`/`Second`/
`DayOfWeek` (ISO 1=Mon..7=Sun)/`DayOfYear`/`WeekOfYear`/`Quarter`/`UnixSeconds`/
`UnixMillis` on `Calendar<F, S> + CalendarField`. Plus `CurrentTime` leaf and bool
`IsWeekday`/`IsWeekend`. CSV loader and remote providers set `Atom::time`; `None`
only for synthetic atoms. Daily+ bars at 00:00 UTC. YAML: bare `!year`/…/`!time`/
`!is_weekday`/`!is_weekend`. **Default atom source is `PickAny`, not `Pick`** —
every entry in a bar's snapshot shares `atom.time`, so picking any one is stable,
and the sole-atom-panic `Pick` behavior would break multi-symbol contexts. Price
leaves (`!close`/`!high`/…) resolve through the **blessed root** instead (see
*Blessed series* in CLAUDE.md) because they *do* depend on which asset. An explicit
`source: !pick { symbol: ... }` is honored on any calendar accessor.

### `Unstable<S>` (`indicators/unstable.rs`)

Passthrough forwarding everything to `S` *except* `unstable_period() = 0`. Fluent
`.unstable()` on both extension traits. YAML: `!unstable { source }`. Opt-in
override for the readiness gate.

`Real = f64` and `Candle` (OHLCV) live in `src/market.rs` alongside `Atom`/
`OverlayInfo`/`Schema`. `src/types.rs` re-exports `time`/`market`/`snapshot`.

### Multi-output indicators

`Macd`, `Adx`, `Bollinger`, `Donchian`, `Keltner`, `Aroon`, `Dmi` expose named
fields; `Output` is a `Copy` struct. Each has a **component accessor per output**
(`macd.line()`/`.signal()`/`.histogram()`, `bands.upper()`/`.middle()`/`.lower()`,
`dmi.plus_di()`, …) returning `Component<Self>` — the field projected as
`Indicator<Output = Real>`: `macd.line().crosses_above(macd.signal())`. Accessors
**clone** the source, so two accessors run two independent computations.
**`.shared()`** (→ `Shared<Self>`, whose accessors return `SharedComponent`) is the
fix: every accessor on one handle borrows the same source and advances it at most
once per bar. The `strategies/` catalogue uses it; **`src/spec/` does not**, so a
YAML-built `!macd_line`/`!macd_signal` crossover still does 2× the work. Bodies via
the **`component_accessors!` macro** — don't hand-write.

`StochRsi<S>` = alias for `Stochastic<Rsi<S>>`.

## Signals — boolean indicators (`src/signal.rs`, `src/indicators/{compare,logic,ext}.rs`)

**A signal is just `Indicator<Output = bool>`** — no second trait hierarchy.
`Signal` is a thin marker `trait Signal: Indicator<Input = Candle, Output = bool>`
(blanket, `?Sized`) so strategies hold `Box<dyn Signal>`. `None` until warmed; read
as `bool` (false until ready) via `BoolIndicatorExt::is_true`.

- **Comparisons**: aliases `Gt`/`Lt`/`Ge`/`Le`/`Eq`/`Ne` for `Combine<L, R,
  GtOp>` etc. The op carries an absolute `epsilon` (default `1e-8`);
  `Gt::with_epsilon(a, b, eps)` overrides.
- **Boolean logic**: `And`/`Or`/`Xor` are `Combine<...>`; `Not` and `Change` are
  dedicated unary carriers; `Const<In>` is a constant-bool leaf; `Every<In>(period)`
  is a **periodic pulse** — fires `true` every `period` bars with a *delayed* first
  fire on bar `period - 1` (0-indexed). The canonical `rebalance_on` cadence source.
  YAML: `!every N`; `!never` is sugar for `!value false`.
- **`IndicatorExt`** (blanket over Real-output): fluent builder for **operators
  only** — comparisons (`gt`/`lt`/`ge`/`le`/`eq_to`/`ne_to`, `above`/`below` —
  `eq_to`/`ne_to` avoid `PartialEq` collision), arithmetic (`add`/`sub`/`mul`/`div`),
  lookback (`lag`/`diff`/`ratio`/`roc`), rolling extremum (`rolling_max`/
  `rolling_min`), `unstable`, `crosses_above`/`crosses_below`. Named indicators are
  **not** builder methods; use `::new`. Don't add `.sma()`-style builders.
- **`BoolIndicatorExt`** (blanket over `Indicator<Output = bool>`, `?Sized`):
  `is_true()`, `and`/`or`/`xor`/`not`, edge primitive `changed`, `unstable`.
- **Crossover *is* a primitive**: `CrossesAbove<L, R>` / `CrossesBelow<L, R>`
  (`indicators/crosses.rs`) hold one comparison state plus a previous-value slot,
  so each operand advances once per bar. Behaviour is byte-identical to the old
  composed `a.gt(b).and(a.gt(b).changed())` form, which cloned both operands and
  did ~2× the source work.

## Strategies — decision layer (`src/strategy.rs`, `src/wallet.rs`)

`Strategy` trait: `update(&mut self, Input)`; `on_fill(&mut self, &Order<Symbol>)`
(default no-op); `on_reject(&mut self, &Rejection<Symbol>)` (default no-op — the
failure-side twin; `trade()` returns `()`, and an `Ack::Working` order can still
fail later at fill time); `is_ready(&self) -> bool` (safe-by-default, default
`true`); `trade(&self, &mut dyn Wallet<Symbol>)` (`&self`, price-free, serial);
`reset()`; assoc `Input`/`Symbol`.

Each bar the driver: feed each symbol to the wallet, route each fill to every
strategy's `on_fill`, drain `take_rejections()` to `on_reject`, `update` each
strategy, then `trade` each *only if* `is_ready()`. Deliberately **no one-shot
`evaluate`**.

`src/strategy.rs` carries only the `Strategy` trait. `Wallet` vocabulary lives in
**`src/wallet.rs`** so downstream broker crates don't drag `Strategy` machinery in.

### `SingleAssetStrategy<Sym>` (`strategies/single_asset.rs`)

The concrete `Strategy` (Input = Candle) — long/flat/short driven by boolean
signals, sized against equity. Four `Box<dyn Signal>` slots (open/close long,
open/close short), default `Const::<Candle>::new(false)`.

- **Readiness gate.** `is_ready()` = `bars_seen >= max(stable_period())` across
  every wired signal, protective level, and sizing indicator. Wrap a subtree in
  `Unstable` to contribute `0`.
- **Builders.** `long_on(enter, exit)`, `short_on(enter, exit)` (opposite-side
  entry reverses), `buy_and_hold(symbol)`.
- **Protective levels.** Per-side stop/take-profit as `Box<dyn Indicator<Output =
  Real>>` via `long_stop_loss`/`long_take_profit`/`short_*`, built against
  `position()`. E.g. `position.entry().mul(Value::new(0.95))` (fixed),
  `position.peak().mul(Value::new(0.95))` (trailing),
  `position.entry().sub(Atr::new(14).mul(Value::new(2.0)))` (ATR).
- **`trade` sequence.** Read sizing → skip on `None` → entries (sizing-scaled,
  reversal-capable) → flatten-to-flat signal exits → **rebalance gate** (resize
  held to sizing target when signal fires) → rest protective on the active side.
- **`rebalance_on(signal)`** (default `Const::false` — never). On bars where the
  gate fires, `wallet.set(sym, held_side, value_frac(size))` re-affirms the current
  sizing target — idempotent when unchanged, market resize otherwise. Lets
  vol-targeted / Kelly-scaled strategies adjust an open position when the target
  drifts.
- **Order semantics.** Entries and signal exits are **market** (`set`/`close` then
  `cancel_protective`), filled next bar at `open`. Protective stops are **resting**
  orders the wallet owns; the strategy re-submits each bar (idempotent, latest-wins);
  the wallet triggers and prices them (at level, or `open` on gap). Trailing tracks
  *completed* bars.
- **Not a rule engine.** Don't add `(signal, action)` tables without being asked.

**`src/strategies/` catalogue** = free functions returning `SingleAssetStrategy`
(`ma_crossover`, `rsi_reversal`, `donchian_breakout`, `keltner_breakout`, … under
`trend`/`mean_reversion`/`momentum`/`volume`/`composite`). `ZScoreReversion`
doesn't fit and stays its own bespoke `Strategy`.

**Sizing.** `value_frac(m)` with `m` = the current value of the **position-sizing
indicator** (via `position_sizing(...)`, default `Value::new(1.0)`). Magnitude
only, read on transitions, folded into the readiness gate. Sized against equity so
one `set` reverses and re-sizes exactly — no `enter_all_in`, no scale-in/out
primitives.

**Book anchor.** Alongside `Position`, `SingleAssetStrategy` owns a shared `Book`
(`Arc<Mutex<BookState>>`, `src/indicators/book.rs`) tracking strategy-lifetime
state (cash, position units, MTM equity, running peak, per-bar returns,
closed-trade summaries staged pending → active so per-close accessors emit `Some`
only on the closing bar). Six `BookField` accessors: `book.equity()`/
`.equity_peak()`/`.drawdown()`/`.return_per_bar()`/`.trade_pnl()`/`.trade_return()`.
Rolling `Sma`/`StdDev` over `book.trade_return()` reads "over last N closed trades".
`with_initial_equity(sym, cash)` is what CLI `--cash` threads through (default
`1.0`).

### `PairsStrategy`

Owns `Book<Sym>` similarly (`with_initial_equity(left, right, cash)`); both legs
feed one cash and mark to market together.

**Long / flat / short on the spread** (`close(left) − close(right)`), *not*
one-directional. Long-spread = long left / short right (profits as the spread
rises); short-spread = the mirror. Both sides have their own enter/exit + spread
stop/target (`long_spread_on` / `short_spread_on` / `long_spread_stop_loss` /
`short_spread_stop_loss` / … ; `on` / `spread_stop_loss` / `spread_take_profit`
remain as long-side aliases). **Level comparisons are sign-aware** — the long side
stops when `spread <= stop` and targets at `spread >= target`, the short side is
the exact mirror. `open_side() -> Option<Direction>`; the opposite side's entry
**reverses** an open pair. The two directions are inverse positions, mutually
exclusive in time, sharing one capital pool at full notional. YAML: `long_spread:`
/ `short_spread:` blocks (reusing `spec::strategy::SideSpec`), with the flat
top-level `enter`/`exit`/`stop_loss`/`take_profit` kept as the long-spread spelling;
setting both is a build error, as is wiring neither side. No `symmetric:` flag —
wiring `short_spread:` *is* the switch.

Rationale for the four-side design (superseding "two signals + swap left/right"):
signals are written against explicit `!pick { symbol: … }` leaves, so swapping
would negate only the *internal* spread and leave the user's `enter` expression
reading the same — the mirror would fire simultaneously with the original and net
flat.

### `BasketStrategy<Sym>` (`strategies/basket.rs`)

N-symbol cross-sectional. Reads the whole `Snapshot<Sym>` each bar: scores every
symbol via a per-symbol scoring source, applies a `Selection` impl
(`select(&scores) -> Sides { long, short }`, with a derived side-per-symbol `pick`).

- **Selection.** Trait `strategies::basket::Selection<Sym>`. **Core method is
  `select(&scores) -> Sides { long: HashSet, short: HashSet }`** — two candidate
  sets that *may overlap*; derived `long_set`/`short_set`/`pick` (the last projects
  to one `Side` per symbol, **long wins** on overlap, so the strategy is never
  handed a symbol on both sides). **Composable by narrowing.** Every built-in is
  generic over an inner `S: Selection<Sym>` defaulting to the `Everything` leaf
  (every scored symbol eligible for *either* side — the total overlap every rule
  subsets from). `T::new(...)` roots on that leaf, `T::of(inner, ...)` re-roots — so
  `TopBottom::of(Threshold::new(0.5, -0.5), 2, 2)` is "top-2/bottom-2 of the
  threshold survivors". Disjointness is resolved once at `pick`. Built-ins:
  `TopBottom` / `Threshold` / `Quantile` (each `<S = Everything>`). Builders
  `.top_bottom(l, s)` / `.threshold(min, max)` / `.quantile(lq, sq)` install the
  leaf-rooted forms. `.selection(impl)` = the general seam — any `Selection<Sym>`
  impl (or a `Fn(&HashMap<Sym, Real>) -> HashMap<Sym, Side>` closure, via the
  blanket impl). `DynSelection<Sym>(Box<dyn Selection<Sym>>)` is the erased-inner
  newtype for dynamically-composed chains. Free functions
  `strategies::basket::{top_bottom, threshold, quantile}` remain callable.
- **Floating universe.** Two factories (`Fn(&Sym) -> impl Indicator<Input=Snapshot<Sym>,
  Output=Real> + 'static`) called once per symbol on first sight, rooting leaves via
  `Pick::matching(Selector::by_symbol(sym.clone()))`.
- **Declared universe** (opt-in): `.all_of([sym, ...])` strict — panics when a
  listed symbol is absent on any bar, gates `is_ready()` on every listed symbol
  scoring *and* sizing `Some`; `.any_of([sym, ...])` lax — restricts discovery to
  the listed subset but silently skips absent / unready members. Universe is a
  **trait** `strategies::basket::Universe { admits(&sym) -> bool; required() ->
  &[Sym]; }` with three impls: `Floating` (default), `AllOf<Sym>` (strict),
  `AnyOf<Sym>` (lax). `.universe(custom_impl)` installs any `Box<dyn Universe<Sym>>`.
- **Sizing.** Per-leg `ValueFraction`, **no auto-normalization** — use
  `sizing::equal_weight(n_legs)` for 100% gross.
- **Costs** stay on the wallet — `PaperWallet::set_costs_for(sym, ...)`.
- **Per-symbol readiness.** Under `Floating` / `any_of`: `is_ready() = true`
  unconditionally, enforced inside `trade()` by only ranking symbols whose score
  read `Some` this bar. Under `all_of`: `is_ready()` blocks until every listed
  symbol has both scored and sized `Some`.
- **`rebalance_on(signal)`** (default `Every::new(1)` — every bar). Gates the whole
  selection + resize step; because basket's selection *is* the sizing decision,
  gating selection is the natural rebalance semantics.
- **State.** Per-symbol `Position` (+ per-symbol per-leg protective chains lazily
  built on first sight) + shared `Book<Sym>`. Seed `with_initial_equity(cash)`.
- **Dollar neutrality.** `.dollar_neutral()` (YAML `dollar_neutral: true`) scales
  per-symbol sizes at each rebalance so `Σ long_sizes == Σ short_sizes`; the
  smaller-side sum is the target gross-per-side (never levers up). A one-sided
  selection this bar skips the whole rebalance.
- **Per-leg protective.** `.long_stop_loss(|sym, &Position| level)` /
  `.long_take_profit(...)` / `.short_stop_loss(...)` / `.short_take_profit(...)`
  per-symbol factories, plus YAML `long: { stop_loss: ..., take_profit: ... }` /
  `short: { ... }` using `BasketSideSpec` templates with `!arg SYM` and `!entry` /
  `!peak` / `!trough` anchored to *that* symbol's Position.
- **Python**: `ta.BasketStrategy().scored_by(fn).sized_by(fn).top_bottom(l, s)`
  (or `.threshold` / `.quantile`), `.dollar_neutral()`, `.rebalance_on(sig)`,
  `.all_of` / `.any_of`, `.run(wallet, snapshots)`. Composable selections mirror
  Rust (`of=` inner rule, free constructors `ta.top_bottom`/`ta.threshold`/
  `ta.quantile`/`ta.everything`). The `.selection(closure)` escape hatch and
  per-leg protective levels are **not** bound.

### `MultiAssetStrategy<Sym>` (`strategies/multi_asset.rs`)

N-symbol **independent** portfolio (not cross-sectional): every symbol runs the
same `SingleAssetStrategy`-shaped decision in isolation — same four signal slots,
same protective-level slots, same sizing — and any subset can be long / short /
flat at once. Reach for it when a symbol's fate depends only on its own signals,
not on how it ranks against the rest.

- **Factories.** Every slot is a per-symbol factory (`Fn(&Sym) -> Signal` /
  `Fn(&Sym) -> Real source`), plus level factories that additionally receive the
  per-symbol `Position` (`Fn(&Sym, &Position) -> Level`) so `position.entry()` /
  `.peak()` / `.trough()` compose exactly as on `SingleAssetStrategy`. Sizing
  factory takes `&Sym` only. Chains are built lazily on first sight, filtered by
  `Universe`.
- **Universe** knob (reused from `basket::Universe`): `.all_of([...])` strict /
  `.any_of([...])` lax / `.universe(custom_impl)` / floating default.
- **Per-symbol readiness.** Under floating / `any_of`: a symbol trades once *its
  own* chains have settled (gated inside `trade`); under `all_of`: `is_ready()`
  blocks until every listed leg is past its own `stable_period`.
- **State.** Per-symbol `Position` + shared `Book<Sym>` (aggregate equity).
- **`rebalance_on(signal)`** (default `Const::false` — never). On fire, resizes
  every held per-symbol position to its current sizing target. Entry/exit signals
  fire every bar regardless.
- **Python**: `ta.MultiAssetStrategy().long_on(fn, fn).short_on(fn, fn)
  .position_sizing(fn).rebalance_on(sig).all_of([...])` + `.run(wallet, snapshots)`.
  Signal / sizing slots are per-symbol Python callables; position-anchored
  protective levels are **not** bound.

### `Portfolio<Sym>` (`src/portfolio/`)

Composite `Strategy<Input=Snapshot<Sym>, Symbol=Sym>` that runs N heterogeneous
child strategies, netting their intents onto **the one wallet `backtest::run`
hands it** (paper or live). Plugs into `backtest::run` like any other strategy —
normal `Wallet<Sym>` in, normal `RunReport<Sym>` out — so every metric / windowing
/ walk-forward reduction falls out for free. Reach for it when a portfolio combines
*different* strategies rather than the same shape across many symbols.

- **One account, N ledgers (netting).** Each child owns a `Ledger` (notional cash
  + positions) and trades a `LedgerWallet` whose reads come from that ledger, so
  `value_frac(1.0)` still means "all of *my* equity" and no strategy code changes.
  `Portfolio::trade(wallet)` records children's intents, then `net_and_submit(wallet)`
  turns them into **one order per symbol** on the passed wallet; the driver's fills
  reach `Portfolio::on_fill`, which calls `attribute_fill` to move the ledgers that
  caused them. **Core invariant: Σ ledger cash == account cash, and per symbol Σ
  ledger positions == account position** — never moved by intent, only by real
  fills; `Portfolio::assert_books_balance(&wallet)` checks it. Netted **buys** go
  out as `Size::value_frac` calibrated to the intended target (equity is unchanged
  by a fill, so several symbols in one bar resolve against the same number, and it's
  the one size the wallet will `shrink_buy_to_fit`). Sells and short targets use
  `set_position`. **The blotter (`report.fills`) is account-level**; per-child truth
  stays in the ledgers and each child's own `on_fill`.
- **Crossing.** Two children on opposite sides of a symbol in one bar net down:
  only the imbalance reaches the market; the offsetting part is crossed internally
  at the bar's **open** and **pays no commission**. A crossing-heavy portfolio
  therefore books slightly lower costs than it would live — the documented price of
  netting rather than grossing up.
- **Per-child stops.** Protective legs carry a `Size`, so a child's stop takes off
  only its own share. The account holds one bracket per symbol, so each bar the
  portfolio rests whichever child leg is **nearest to triggering**; two hitting in
  one bar means the second fires a bar late.
- **Hard cap.** A child may not spend past its ledger cash even when a sibling has
  idle cash; refused as `InsufficientFunds`. Keeps per-child equity honest.
- **Not supported inside a portfolio:** resting limit entries (`set_limit`) and
  `cancel` — a resting limit has no owner while it rests, so `LedgerWallet` refuses.
  `adjust_funds` on a `LedgerWallet` moves that child's notional slice.
- **Threading.** `Portfolio` shares its interior (`PortfolioInner`) with its
  children's `LedgerWallet`s via `Arc<Mutex<PortfolioInner<Sym>>>` — so the
  composite is `Send`. The cost is that **a child strategy must be `Send`**.
  `backtest::run(&mut portfolio, &mut wallet, snaps)` is the driver; `portfolio.run(snaps)`
  is a convenience that builds a fresh `PaperWallet` at the seed. There is **no
  `wallet_view`, no owned substrate, no mis-pairing guard**. Internal crosses are
  booked in `Portfolio::update(snap)` at that bar's `open`. Child hard-cap
  rejections reach the owning child via `on_reject` but are **not** in
  `report.rejections` — the account never saw them.
- **Per-child seam.** Inside `trade`, each child sees a `LedgerWallet<Sym>`: reads
  (`funds`/`equity`/`position`) come from *that* child's `Ledger`, while `price`
  comes from `PortfolioInner.marks` (a per-symbol cache refreshed each `update(snap)`
  from the snapshot's closes). Mutations **record intent** rather than executing.
- **Fill routing.** Every intent is acked with a portfolio-wide `OrderId` recorded
  in `owners`. `attribute_fill` splits a raw account fill into **one synthetic
  `Order` per child share** and dispatches each to *only* the owning child's
  `on_fill` (market fill: pro-rata by intent; protective fill: wholly the child
  whose leg was rested). The raw account fill lands in `report.fills`.
- **`WeightPolicy` (`portfolio::policy`).** `trait WeightPolicy { observe;
  weights(&self, n) -> Vec<Real>; warm_up_period; reset }`. Two built-ins:
  **`Fixed(Vec<Real>)`** and **`EqualWeight`**. `weights(n)` splits the initial cash
  budget across children and, when no per-child weight-share indicators are
  installed, is re-queried at each rebalance-fire.
- **Weight-share indicators (adaptive weighting).**
  `PortfolioBuilder::weight_shares(Vec<Box<dyn Indicator<Input=Snapshot<Sym>,
  Output=Real>>>)` installs one real-valued chain per child. Advanced every bar; at
  each rebalance-fire the portfolio reads each's `.value().unwrap_or(0.0).max(0.0)`
  and normalizes `w_i = N_i / Σ N_j` — that vector wins over `WeightPolicy::weights`.
  When every share reads `0`, the policy is the fallback. **YAML — `weights:` is one
  unified expression** (`SpecTemplate<NodeSpec>`, no `WeightPolicySpec` enum):
  omitted → equal weight; `!value [w0, w1, ...]` → per-child indexed constants
  ("fixed weights"); `!value 1.0` → any per-child constant normalizes to `1/N`; any
  other expression → dynamic. `!fixed [...]` / `!equal_weight` are load-time sugar
  rewritten to their `!value` equivalents. Per-child instantiation supplies auto-args
  `!arg CHILD_INDEX` (always), `!arg CHILD_NAME` / `!arg CHILD_GROUP` (when declared),
  `!arg SYM` (single-asset children). Child names must be unique after defaulting to
  `child_<idx>`. **Book source is explicit via `source:`** — bare `!drawdown` /
  `!equity` / `!fractional_kelly` reads the *child's own* `Book`; `source:
  !portfolio_book` reads the aggregate; `source: !strategy_book` spells the default.
  Both source-selector tags are build-time only (bare use is a build error).
- **`rebalance_on(signal)`.** Default `Const::false` — never rebalance, drift with
  P&L. Wire a snapshot signal (`Every::new(28)`, `!or [!every 28, drawdown_gate]`)
  to turn on the loop. Children `trade()` first (against pre-rebalance equity), then
  one rebalance cycle: **cash phase** shifts free cash between sub-wallets
  (contributors donate `min(|Δ|, funds)` via `Wallet::adjust_funds`, receivers split
  proportionally); **position phase** covers what cash couldn't via the installed
  `PositionRebalancer` impl (default `Proportional`, also `LargestFirst`;
  `PortfolioBuilder::position_rebalancer(...)`) submitting `Wallet::set_position`.
  Convergence takes one fire when cash covers, an extra fire per cycle when
  contributors are fully invested.
- **State.** `PortfolioInner<Sym>` (`src/portfolio/netting.rs`) owns `ledgers` +
  `marks` + this bar's `intents` + per-child `protective` levels + `pending` flow +
  seeds + id-tracking tables. `Portfolio` owns children, policy, rebalance signal,
  bars_seen, total `initial_equity`, and an **aggregate `Book<Sym>`** (marked each
  `update()` from `Σ child_equity` via `Book::mark_equity`, exposed via
  `Portfolio::book()`, passed as the `portfolio_book` build arg).
- **Wallet-trait discipline.** Both rebalance phases go through the `Wallet<Sym>`
  trait. Wallets that don't support `adjust_funds` fall through cleanly: failed
  debits fold into the contributor's shortfall for the position phase; a failed
  receiver credit refunds the pot symmetrically. Position phase is universally
  supported (`set_position` only).
- **Readiness.** `is_ready() = bars_seen >= policy.warm_up_period() && bars_seen >=
  rebalance.stable_period() && every child is ready`.
- **YAML.** `portfolio:` prefix + `PortfolioSpec` (`src/spec/portfolio.rs`).
  `children:` is an ordered list of `{ name, strategy }` slots; `strategy:` accepts
  any of the four shapes routed by distinctive top-level key (`left`+`right` →
  pairs, `selection` → basket, `symbol` / preset tag → single, else multi).
  `rebalance_on:` optional (any boolean `NodeSpec`, default `!never`); the
  signal-anchor and `Book` handed to `NodeSpec::build` at the portfolio level are
  dummies, so `!entry`/`!drawdown` read empty — use snapshot / calendar / cadence
  signals. `rebalance_policy:` optional (`!proportional` | `!largest_first`, default
  `!proportional`). Reuse one child spec N times via `!import { path, params }`.
  Wired into `run.rs` (`run_portfolio`) and `optimize.rs`.
- **Reset** reseeds every ledger and clears bar-to-bar state. No substrate to
  rebuild — the driver hands a fresh account each run.
- **Superseded — do not reintroduce:** (a) N per-child `PaperWallet`s each with a
  cash slice — that's N *separate accounts*, can't run live. (b) The portfolio
  owning its own `substrate` behind a `SubstrateFactory` + `PortfolioWallet` view +
  mis-pairing guard.
- **Not shipped (yet):** inverse-vol / performance-weighted `WeightPolicy`
  built-ins.

## Wallet (`src/wallet.rs`)

**`Wallet<Sym>`** (`&mut dyn`) — the single **seam** to downstream execution.
Priced **from outside**: `update(symbol, candle) -> Vec<Order>` feeds a bar per tick
(`close` marks, `[low, high]` bounds fills), returns fills booked. Query:
`funds()`/`position(&Sym)`/`price(&Sym)`/`equity()`.

- **Submitting ≠ filling.** Market moves (`set_position`, `set` (Side + Size,
  opposite reverses), `close`) return `Ack::Filled(Order)` or `Ack::Working(OrderId)`.
  Resting **exits**: `set_stop(sym, trigger, size)` / `set_take_profit(sym, trigger,
  size)` (idempotent, latest-wins; the wallet reads side from position). The `size`
  is **reduce-only** — resolved at the fill price and clamped to `|position|`, so a
  leg can flatten but never flip. `Size::position_frac(1.0)` is the whole-position
  exit every single-asset strategy passes; an explicit `Size::units(n)` is a
  **partial** exit, which is what lets several owners rest their own share on one
  shared account. `cancel_protective(&sym)`. Resting **entry**: `set_limit(sym,
  side, size, limit)` + `cancel_limit(&sym)` — same latest-wins convention; fills at
  the limit **or better** (a gap through it hands you the better `open`). `Size`
  resolves at the *fill* price. Both default to `UnsupportedOperation` / `Ok(())` on
  the trait; **`PaperWallet` and `OkxWallet` both implement them**.
- **🚧 WIP — strategy-layer limit entries.** No strategy shape uses `set_limit`
  yet: the four shapes still enter at market, so limits are reachable only from a
  hand-written `Strategy` or directly from Python via `PaperWallet.set_limit`.
  Wiring them into the signal slots is a **design question** (what an entry signal
  firing *means* when the limit may never fill) — don't add a `(signal, action)`
  table without being asked.
- **`PaperWallet` timing.** Queues market moves, fills at *next* bar's `open`;
  protective fill when the bar trades through the trigger (at level, or `open` on
  gap). A backtest never fills on the signal's own bar. Market moves queue one per
  symbol (latest wins); resting stops register one bracket; resting limits one per
  symbol. `update` marks the bar, flushes queued at `open`, matches protective
  against `[low, high]` (stop precedence; fill flattens + OCO-cancels bracket),
  **then** limits — so a protective exit books before a limit entry on the same bar.
  A triggered-but-unaffordable limit is a rejection and is *consumed*. Resting fill
  price provably in `[low, high]`.
- **Errors.** `WalletError` (`UnknownPrice`, `InvalidPrice`, `PriceOutOfRange`,
  `InsufficientFunds`, `UnsupportedOperation`).
- **Optional capability methods** (defaulted, opt-in per impl): `adjust_funds` /
  `set_limit` / `cancel_limit` / `cancel` / `poll_fills` / `take_rejections`, plus
  **`positions() -> Vec<Units<Sym>>`** (default empty — "can't enumerate", *not*
  "holds nothing") and **`set_costs_for(sym, costs)`** (default
  `UnsupportedOperation` — a live venue owns its own fees). Both moved onto the trait
  for the portfolio's erased sub-wallets; no inherent twin remains.
- **`take_rejections() -> Vec<Rejection<Sym>>`** — the **failure stream**, twin of
  `update`'s fill stream. `Rejection { symbol, id, error, kind }`. Default empty;
  **any impl that can drop an order must override**. `PaperWallet` books all three
  refusal paths (submit-time pre-flight via `reject_submission`, the queued market
  order in `update`, a triggered protective leg in `match_protective`). Destructive
  drain via a `rejections_drained` cursor, undisturbing the non-destructive
  `rejections()` history accessor. For a portfolio, the driver drains the passed
  wallet's `take_rejections()` and routes each to `Portfolio::on_reject`.
- **No explicit-price primitive, no `trade(delta)`** — scale-in is
  `set_position(position + delta)`.
- **Unit-tagged amounts.** `Reference(Real)` (quote/funds), `Units<Sym> { symbol,
  amount }`. `Order<Sym>` = `{ symbol, side, units, price, kind, id }`; `OrderKind`
  = `Market`/`Stop`/`TakeProfit`/`Limit`. `Order::from_delta(...)` returns `None`
  within `DEFAULT_EPSILON`. **A `Limit` fill is passive** — crosses no spread
  (`wallet::half_spread_for`), takes a `0.0` slippage multiplier
  (`costs::kind_multiplier`); commission still applies (no maker/taker distinction).
- **`Ack<Sym>`** (`Filled(Order) | Working(OrderId)`), **`OrderId(u64)`**
  wallet-minted. Execution **synchronous**; live fills between bars reach the
  strategy via `on_fill`.
- **`Size`**: `Units(n)`, `FundsFraction(f)` (`f·funds/price`), `ValueFraction(f)`
  (`f·equity/price`; `1.0` flips cleanly on reversal), `PositionFraction(f)`
  (`f·|position|`, adjust-only). Direction from `Side`.
- No `Market` trait: the wallet holds prices. Python binds `PaperWallet`, the live
  `OkxWallet` (constructors `demo` / `mainnet` + `refresh_account` / `errors`; same
  order-flow surface) and `CoinbaseWallet` (`mainnet` only; spot, so a `position` is
  a base balance and `set_position` diffs it, market-ordering the difference — a
  short target sells to flat and reports the un-shortable remainder), `Order`, and
  `Size` (sides `"buy"`/`"sell"`; `WalletError` → `ValueError`). The live wallets
  differ only in signing: `OkxWallet` HMAC-SHA256, `CoinbaseWallet` an ES256
  (ECDSA P-256) JWT per request.

## Run driver (`src/backtest.rs`)

**`fugazi::backtest::run(&mut strategy, &mut wallet, snapshots) -> RunReport<Sym>`**
walks a `Strategy` over a snapshot stream through any `impl Wallet<Sym>` (live too).

**Per-bar.** For each tagged entry in `Snapshot<Sym>` (`(symbol, freq, atom)` where
`symbol: Some`): `wallet.update(symbol, atom.candle)`, route fills to `on_fill`,
append bar-tagged to the blotter. Untagged entries are skipped for wallet pricing
but visible to the strategy. Then drain `wallet.take_rejections()` → `on_reject`
(before `update`) → `strategy.update(snap)` → `strategy.trade(wallet)` iff
`is_ready()` (then drain again, for a synchronously-rejecting live wallet) → push
`wallet.equity().0` to the curve.

`run<Sym, S, W, I, A>` where `A: Into<Snapshot<Sym>>`. `Vec<Atom>` / `Vec<Candle>`
produce untagged size-1 snapshots; single-series callers use `Snapshot::single(sym,
atom)`. `RunReport<Sym> { equity_curve, fills, rejections, initial_equity }` —
`fills` are `Fill<Sym> { bar, order }`, `rejections` are `Rejected<Sym> { bar,
rejection }` (non-empty ⇒ the curve/metrics describe a different strategy than the
one written; `report_slice` rebases them like fills; CLI `run` prints a post-run
`warn` banner grouped by reason+kind). `Fill`/`Rejected`/`RunReport` re-exported at
crate root; `run` namespaced.

## Run resuming — full-state serialization (`fugazi::spec::RunState`, `src/runtime.rs`, `fugazi-derive/`)

Persist a run's **entire runtime state** to JSON and continue it later over new
bars with **bit-identical** behavior. **Full-state serialization, not replay** — a
spec-built strategy is a tree that *interleaves* concrete indicator structs with
type-erased trait-object boxes (`!ema { source: !close }` → `Adapter<Ema<As<Real>>>`),
so plain `#[derive(Serialize)]` can't traverse it and `typetag` can't (the generic
instantiations are open-ended). The *structure* is rebuilt from the spec; only the
*values* are replayed in, keyed positionally by tree shape.

**The mechanism.** `save_state(&self) -> serde_json::Value` / `load_state(&mut self,
&Value) -> Result<(), String>` on:
- **`Indicator`** (default no-op — stateless leaves need nothing) and
  **`DynIndicator`** (no default; the four runtime carriers —
  `Adapter`/`As`/`Chain`/`UnstableWrap` — each supply it, threading the recursion
  across the `Indicator`/`DynIndicator` boundary; `Chain` drops its cached `value`,
  recomputed on next `update`).
- **`Strategy`** (default no-op) — so a strategy *embedded inside an indicator* (the
  `!sharpe`/… trailing metrics drive one over a private wallet) is reachable through
  the `Strategy` handle the metric holds.
- **`#[derive(SaveState)]`** (`fugazi-derive`) generates the per-indicator bodies:
  default = plain serde state, `#[state(source)]` = a child indicator (recurse via
  `crate::Indicator::save_state`), `#[state(skip)]` = `PhantomData` / config /
  `Arc<Mutex>` shared handles. **Default-is-state is deliberate:** forgetting
  `#[state(source)]` on a new box field is a **compile error** (a box isn't
  `Serialize`), not silent loss.

**Shared/path-dependent state** is serialized once at the strategy level, never
per-indicator: `Position::snapshot`/`restore`, `Book::snapshot_state`/`restore_state`,
`PaperWallet::snapshot_state`/`restore_state` (positions/cash/pending/resting/blotter
— **not** costs, which are re-primed), `PortfolioInner`/`Ledger`.
**`PositionField`/`BookField` keep the no-op default** — they hold a clone of the
shared handle, and serializing per-accessor would double-count.

**Driving.** `RunnableStrategy::{save_state, restore_state, drive_resumable}` +
`RunState { format_version, kind, last_bar, bars_seen, strategy, wallet }`.
`drive_resumable(snaps, cash, costs, resume: Option<&RunState>, flatten: bool)`
restores before the run (rejecting a `format_version` / `kind` mismatch), optionally
flattens open positions at the end, and surfaces the final `RunState`. `drive` is
the thin `(…, None, false).0` wrapper. `backtest::run_iteration_resumable` is the
CLI/spec-shape entry.

**Per-shape fidelity.** single / pairs = exact. basket / multi = exact via **lazy
per-symbol restore** (a `pending_restore: Option<HashMap<Sym, Value>>` stash applied
inside `update` right after a symbol's chains are first built). portfolio = ledgers
+ aggregate `Book` restore exactly; children (erased `Box<dyn Strategy>`) re-warm
their chains. **Trailing metrics** (`Sharpe<S>` et al.) serialize the embedded
strategy + private wallet + `prev_equity` + the rolling window — full fidelity; their
`Indicator` impls therefore require `Sym: Serialize + DeserializeOwned`.

**Flatten toggle.** `backtest::flatten_open_positions` books a closing fill for every
still-open position at the last bar so `reconstruct_trades`/metrics count the realized
P&L (equity curve untouched). Terminal, mutually exclusive with saving state.

**Surfaces.** CLI `fugazi run … --save-state <file>` / `--resume <file>` /
`--flatten`. Python `spec.run_resumable(wallet, snapshots, resume=None, flatten=False)
-> (report, state_json)` (PaperWallet-only like `.run`).

**Adding a stateful indicator ⇒ add `#[derive(SaveState)]` + field annotations + the
two `impl Indicator` forwarding lines**, or its state is silently lost on resume.
tests/resume.rs is the acceptance gate.

**Bounded, documented limitations:** the generic-typed prior of `Change` and the held
value of `Latch` are skipped (their `S::Output` is unbounded) → a one-bar re-warm at
the resume boundary; `Shared`/`SharedComponent` are no-op (the spec layer never uses
`.shared()`). There is **no per-field config-drift guard** — resuming a same-shape
spec with changed params silently loads the saved config (only `kind` +
`format_version` are checked).

## Metrics — one function per metric (`src/metrics.rs`)

See [doc/METRICS.md](METRICS.md) for the user-facing catalogue and caveats.
Internals:

**No aggregate `compute`.** Every metric is its own `pub fn`. Three **intermediate
builders**:

- **`per_bar_returns(equity, initial_equity) -> Vec<Real>`** — for return-moment /
  risk-adjusted metrics.
- **`reconstruct_trades<Sym>(fills) -> Vec<Trade>`** — walks the blotter with signed
  position and volume-weighted entry; one closed leg = one `Trade { entry_bar,
  exit_bar, side, units, entry_price, exit_price, pnl, return_ratio }`.
- **`drawdown_segments(equity) -> Vec<DrawdownSegment>`** — one peak→trough→recovery
  per drop; `{ peak_bar, trough_bar, depth_ratio, duration_bars, underwater_bars }`.

**Catalogue** (each a `pub fn`): return moments, risk-adjusted (`sharpe`, `sortino`,
`calmar`, `omega`, `ulcer_index`, …), Sharpe corrections (`probabilistic_sharpe` —
Bailey/LdP 2012; `deflated_sharpe` — 2014; `*_from_stats` variants take pre-aggregated
stats so `optimize` computes DSR per row without re-scanning), drawdown, trade-level.

**Values in natural units** (`0.15` = +15%). Vanishing-denom ratios return
`Option<Real>`; always-defined ones return `Real` (`0.0` on empty). PSR/DSR use
`statrs`. **Library core stays lean** — no `Metrics` struct in the library, no
plotting. CLI emits data files only: `fills.csv`, `trades.csv`, `returns.csv`,
`metrics.yml`; under `-w LEN` also `metrics.csv` + `rolling.csv`.

**CLI `Metrics` document** (`src/spec/metrics.rs`) carries serde derives + YAML names
(`sharpe`, `max_pct`, `annualized_mean_pct`). Populated by `metrics::from_report<Sym>(
&RunReport<Sym>, bars_per_year, risk_free_rate) -> Metrics`. Downstream:

- **`MetricKey`** — validated-once dotted-path handle; `from_name(name, sample)` +
  `.resolve(&Metrics)`.
- **`report_slice`** — sub-run over a bar range; the shared measurement primitive.
- **`windowed_from_report`** / **`rolling_from_report`** — twin reductions
  (non-overlapping vs rolling stride-1). Under `-w LEN`, `run` emits both.
- **`optimize -w`** uses only non-overlapping: each `-m` becomes `_mean`/`_std`
  columns; `--best-by` ranks by mean shifted by `-k/--risk-aversion` stddevs,
  direction-aware.
- **`optimize --walkforward IS,OS[,Embargo]`** — rolling WFO, mutex with `-w`. Fixed
  IS, stride OS, last fold's OOS absorbs trailing bars. One backtest/row,
  `report_slice`/fold → IS/OOS metrics; `--best-by` picks the winner; composite OOS =
  stitched winners' OOS slices, running-total scaled. Emits `wf.csv` + sibling
  `.composite_oos_equity.csv` + `.composite_oos_metrics.yml`. Embargo drops OOS-metric
  bars only. Pairs/basket rejected.
- **`selection.deflated_sharpe` on `optimize`** — per-row DSR against the grid-wide
  null (`N` = trials, `Var[SR]` = sample variance of the grid's annualized Sharpes).
  Omitted if <2 rows have defined Sharpe or trial variance is zero.

`Trade`/`DrawdownSegment` re-exported at crate root.

## Monte Carlo significance (`src/montecarlo.rs`, `src/spec/montecarlo.rs`)

Behind the **`montecarlo`** feature (off by default; `cli` turns it on). Opt-in at
runtime via `run --montecarlo` — expensive and analysis-only, so it never runs
unasked. Two layers:

- **`src/montecarlo.rs`** (`rand` only) — the pure, seeded resampling core.
  `ResampleScheme { Iid | MovingBlock { block } | Stationary { mean_block } }`;
  `resample_indices(n, scheme, rng)` is the one primitive, `resample_slice` maps it
  over a slice. RNG is `rand_chacha::ChaCha8Rng` seeded from a `u64` (`rng_from_seed`)
  — a **portable, reproducible** stream. Plus `percentile` (R type-7) and `std_dev`.
  **The scheme is orthogonal to the estimator** — block resampling preserves the
  short-range serial dependence an IID bootstrap destroys; **stationary is the
  default**.
- **`src/spec/montecarlo.rs`** (`montecarlo` + `spec`; `spec ⇒ parallel`, so rayon)
  — the two estimators over a completed `RunReport`. `McConfig { permutations,
  scheme, seed, ci_level, rerun_null, metrics }`; `run_montecarlo(spec,
  snapshots, ctx, observed, config) -> McOutcome { section, samples }`:
  1. **Bootstrap CIs** (shape-agnostic) — resample the run's realized per-bar
     returns, rebuild the equity path, reduce via `EvalContext::reduce`, take a
     percentile CI + bootstrap std error per metric. **Trade-level metrics come back
     `None`** (no fills on a resampled return path).
  2. **Re-run null p-values** (all shapes) — block-resample the input **price paths**
     (candle reconstruction preserving intrabar OHLC geometry; joint bar-index order
     keeps the cross-section aligned) and re-drive the strategy via
     `measured_report_any` once per permutation, **in parallel** (index sequences
     drawn up front, sequentially, so the parallel map stays deterministic).

  A single seed drives every estimator (drawn CI → rerun). p-values are
  one-sided `(1 + #extreme) / (1 + N)` with a per-metric direction
  (`metric_is_maximize`). Superseded, do not reintroduce: a "cheap" null that froze
  the run's realized per-bar exposure and re-paired it with resampled *market*
  returns without re-trading — single-asset only, and it answered a narrower
  question (was the realized position sequence well-timed) than the strategy-level
  claim ("would this edge survive a shuffle of the underlying order of events")
  that `--montecarlo` is meant to answer.

- **Where it runs — the backtest layer, not the CLI.** The config rides on
  **`EvalContext::mc: Option<McConfig>`**. `McConfig`/`ResampleScheme`/`McSamples`
  compile *unconditionally* (plain-data); only the `rand`-backed compute is
  feature-gated. `run_iteration_resumable` calls `attach_montecarlo` after the run,
  folding the summary onto `metrics.montecarlo` + raw samples onto
  `IterationResult::mc_samples`. So **every driver that goes through
  `run_iteration_*` gets the block**. `optimize` sets `mc: None`. CLI's
  `run::emit_montecarlo` is IO-only (writes `montecarlo.csv`, prints the console
  block).
- **Output.** `McSection` (plain data) is an `Option<McSection>` field on the
  `Metrics` document. CLI flags: `--montecarlo`, `--mc-permutations N` (1000),
  `--mc-scheme iid|moving-block|stationary`, `--mc-block L` (10), `--mc-seed S` (0),
  `--mc-null none|rerun` (rerun), `--mc-ci LEVEL` (0.95), `--mc-metrics`.
  **Not on `optimize`**. **Python:** `StrategySpec.evaluate(montecarlo=ta.MonteCarloConfig(...))`.
  `tests/montecarlo.rs` + `python/tests/test_specs.py` are the gates.

## Generic transform ops (`src/indicators/ops.rs`)

Source-wrapping carriers driven by operator types (new op = trait impl, not new
type):

- **`Combine<L, R, Op>`** (binary, `BinaryOp`): one carrier for all binary ops, op
  **by value**. Serves arithmetic `Add`/`Sub`/`Mul`/`Div` (`Div` → `None` on /0),
  comparisons (op carries epsilon), boolean logic. Needs `Op: Default`; comparisons
  get `with_epsilon`. Feeds the *same* input to both sides, requires `Input: Clone`;
  use `lhs`/`rhs` naming.
- **`Lookback<I, Op>`** (unary, `LookbackOp`, zero-sized markers): `Lag`, `Diff`,
  `Ratio`, `Roc`.
- **`Extreme<S, Op>`** (rolling, `ExtremeOp`): `RollingMax`/`RollingMin`.

**`IfElse<Cond, T, F>`** — three-source ternary. `Cond: Indicator<Output=bool>`
picks: `Some(true)` → `if_true`, `Some(false)` → `if_false`, `None` propagates. All
three advanced every bar (never short-circuited). `warm_up_period()`/`stable_period()`
report the max across three (safe worst case), but **first `Some` can arrive
earlier** — cond + the selected branch settled is enough. `IfElse` is excluded from
`tests/warm_up.rs`. Fluent `.if_else(t, f)` on `BoolIndicatorExt`. YAML: `!if_else {
cond, if_true, if_false }`.

## Shared cores (`pub(crate)`)

Bare `Real -> Real` math, **no source, no `Indicator` impl**, shared:

- `smoothing.rs`: `EmaState`, `WilderState` (mean-seed). `Ema`/`Macd` use `EmaState`;
  `Rma` uses `WilderState`; `Rsi` uses two; `Atr` = `TrueRange` + `WilderState`; `Adx`
  uses four. Internal smoothing uses these cores directly — `Rma<S>`/`Ema<S>` wrap a
  *source* and can't smooth inline values.
- `stats.rs`: `WindowStats` (sum + sum-of-squares → mean/variance/stddev) backs
  `Sma`/`StdDev`/`Bollinger`; `WindowExtreme<Op>` (monotonic-deque) backs
  `Extreme`/`RollingMax`/`RollingMin`/`Stochastic`; `WindowQuantile` (sorted `Vec` +
  arrival `VecDeque`; O(period)/bar, O(log period) query) backs `Percentile` /
  `PercentileRank`; `quantile_of_sorted(sorted, p)` is the crate's **one** quantile
  convention (R type-7, interpolated) — `metrics::percentile` and the rolling
  `!percentile` tag both delegate; don't add a second.

## Remote providers — one `SeriesSource` trait (`src/sources/`)

Behind the `sources` feature. `SeriesSource: Send + Sync`, RPITIT (`impl Future`),
takes **objects/enums, not strings**, shares one `SourceError` and `Interval`. **One
trait**, because the candle-vs-overlay split is a property of the *data*, not the
provider: `Atom::candle` is `Option`, so a fetch yields `Vec<Atom>` whether or not it
carries prices.

- **`atoms(symbol, interval, since, until) -> Vec<Atom>`** — ascending by `time`,
  every atom `time: Some(_)`. A price provider fills each atom's candle; an overlay
  provider leaves it `None` and carries per-bar side-channel values behind a `Schema`.
  Every atom in one fetch shares one `Arc<Schema>` — pick with `schema_of`.
- **`schema() -> Option<Arc<Schema>>`** (default `None`) — the provider's fixed
  overlay schema when known *before* the fetch.
- **`tickers() -> Vec<String>`** (default `SourceError::Unsupported`) — enumerate the
  provider's symbols where such an endpoint exists.

Impls: `Binance` (live spot klines; `binance_schema()`), `Yahoo` (`yahoo_schema(adjusted)`:
candles split/dividend-**adjusted by default** — `close` is adjusted, raw print rides as
a `raw_close` overlay; `Yahoo::with_adjusted(false)` flips), `CoinGecko`
(`coingecko_schema()`: the one genuinely **price-less** provider — every atom is
`candle: None` plus `price`/`market_cap`/`total_volume`/`circulating_supply`), and
`BinanceVision` — the `data.binance.vision` archive, one provider covering two markets
by `Market`, each **carrying candles**: `Spot` = spot klines + four `kline_extras`,
`UsdMFutures` = the perp's *own* klines + a derivative side channel (`funding_rate`,
`premium_index`, `open_interest`/`_value`, four positioning ratios). The old two-trait
split (`CandleSource`/`OverlaySource`, `BinanceFunding`) is gone.

Providers whose side-channel samples arrive off-cadence bucket them onto the requested
interval via `sources::floor_to_bucket`, but **level vs accrual** columns aggregate
differently: a *level* keeps one representative sample per bucket (CoinGecko keeps the
**first**; BinanceVision's `premium_index`/`open_interest`/ratios keep the **last**,
`Aggregation::Last`), while `funding_rate` is an *accrual* so its samples are
**summed** — `[1d]` is the day's total carry (3 × 8h). No baked-in moving average —
that's `get -x` / `!sma { source: !get { key: funding_rate } }`.

**How overlay data reaches a strategy.** Fetch overlay to its own CSV; `--series`
joins by `(symbol, time)`. Read with `!get { key: market_cap }`. Overlay series carry
their own symbol and are **stacked** into the run rather than joined onto a price row —
reach them with `!pick` + `!get`. Cross-sectional `BasketStrategy` is the natural
consumer.

```text
fugazi get binance:BTCUSDT[1d]                       -o prices.csv
fugazi get cg:BTCUSDT=bitcoin[1d]                    -o caps.csv
fugazi run @strategy.yml -s @prices.csv -s @caps.csv -o out/
```

**CoinGecko specifics.** `market_chart/range` picks granularity from window length
(~5-min ≤1d, hourly ≤90d, daily beyond). The client rejects sub-hourly, paginates
hourly in 80-day windows, buckets keeping the **first** sample per bucket. Weekly
floors to Monday, monthly to the 1st via calendar (epoch day 0 = Thursday would
silently break Monday joins). `User-Agent` **mandatory**. Public tier serves the
**last 365 days** only. `COINGECKO_API_KEY` = demo key.

## Spec / backtest / optimize kernel (`src/spec/`) + CLI (`src/cli/`)

Two related module trees. `src/spec/` is a **library** module (gated behind the
`spec` feature — required by both `cli` and the Python bindings) hosting the YAML
spec surface, load-time passes, backtest evaluators, metrics document, and optimize /
walkforward kernel — everything a downstream caller needs minus I/O and console
styling. `src/cli/` is the binary crate on top, adding clap arg parsing, `--series`
CSV loading, `fugazi get`, progress banners, and CSV/YAML output writers.

**In `src/spec/`** (library, `spec` feature): `mod.rs`, `expr.rs`, `strategy.rs`,
`pairs.rs`, `basket.rs`, `multi_asset.rs`, `portfolio.rs`, `preset.rs`, `trailing.rs`,
`template.rs`, `imports.rs`, `params.rs`, `undefined.rs`, `args.rs`, `convert.rs`,
`input.rs`, `dyn_indicator.rs`, `calendar.rs`, `costs/`, `backtest.rs`, `metrics.rs`,
`optimize.rs`, `pool.rs`, `runnable.rs`, `overlay.rs`, `montecarlo.rs`, `typecheck.rs`.

**In `src/cli/`** (binary): `main.rs`, `run.rs`, `optimize.rs` (thin UI wrapper on the
kernel), `get.rs`, `overlay.rs`, `data.rs`, `csv_source.rs`, `list.rs`,
`completions.rs`, `style.rs`, `glob.rs`.

### CLI layout by concern

- **`main.rs`** — clap defs, subcommand dispatch. Uses `pub(crate) use
  fugazi::spec::*;` to keep the rest of `src/cli/` referencing `crate::spec::foo`.
- **`run.rs`, `optimize.rs`, `backtest.rs`** — drivers on pure `backtest` from
  `src/spec/backtest.rs`, which is **shape-agnostic**: `run_iteration_any` /
  `evaluate_any` / `evaluate_windowed_any` / `measured_report_any`, each taking a
  **`StrategySpec`** (the sum over the five shapes) and one **`EvalContext`** (the
  seven-field bundle: cash / bars_per_year / risk_free_rate / cost_config /
  effective_freq / windowed / seconds_per_bar). No per-shape `evaluate_pairs` etc.
  (see *One handle per shape* in CLAUDE.md). `optimize.rs`'s kernel
  (`spec::optimize::{optimize, walkforward, ...}`) lives in the library; the CLI
  wrapper owns CSV output + progress banners.
- **`get.rs`** — `fugazi get`. Grammar: `<provider>:<symbol>[<freq>,...]`. The
  provider is split off at the **first** colon and no provider name contains one, so
  the symbol is everything after it, verbatim — no escaping. **Freqs have no remap**.
  **One pipeline**: `run_candles` handles every provider, because `Atom::candle` is
  optional and the writer omits the OHLCV block when no row has a bar. `get --params`
  resolves `!param` inside `-x/--overlay`.

### `src/spec/` — YAML mirror of the composition API

- `expr.rs` — **`NodeSpec`, the one composable expression enum** (there is no separate
  `SignalSpec` — a "signal" is a `NodeSpec` whose `output_type()` is `Bool`). Every
  tag lives here: numeric sources, boolean predicates (`!gt`/`!and`/`!crosses_above`/
  `!changed`/`!every`/`!is_weekday`/…), string comparisons (`!str_eq`/`!str_ne`).
  Polymorphic over `DynType` for `!current`/`!pick`/`!time`/`!get`/`!if_else`/`!value`.
  `!changed` dispatches Bool-toggle vs Real-change on the child's `output_type()` at
  build; `!unstable { source }` is output-agnostic; `!eq`/`!ne` dispatch Real/Str on
  the lhs. Carries `default_source`/`default_high`/`default_low`/`default_bar_source`,
  the cadence sugar (`!daily` → `!changed { source: !day }`), `StrOperand`, and
  **`ValueLit`** — `!value` payload (number → `Value`/`Real`; bool → `ValueBool`/`Bool`
  — subsumes `!never`; string → `ValueStr`/`Str`; or a per-child weight list). Uses the
  `serde_norway::Value` bridge. **`RealNode` / `BoolNode`** are thin newtypes that
  parse a `NodeSpec` then assert its `output_type()`, skipping the undecidable. They
  sit **only at the eager strategy slot boundaries** (`SideSpec.enter`/`exit` →
  `BoolNode`, `stop_loss`/`take_profit`/`sizing` → `RealNode`, `rebalance_on` →
  `BoolNode`), so a decidably-wrong `enter: !sma { … }` is rejected at *parse*.
  **Internal `NodeSpec` fields are never newtypes**. The deferred `SpecTemplate<NodeSpec>`
  slots stay plain `NodeSpec`.
- `template.rs` — `SpecTemplate<T>`: captures raw `serde_json::Value`; `.build(&args)`
  runs `!arg` then typed-parses. Two-pass: `!param` at load, `!arg` each `.build()`.
- `strategy.rs` — `SideSpec`, `SingleStrategySpec`, `DynSingleStrategy`.
- `preset.rs` — `StrategyPreset` (`!buy_and_hold`/`!ma_crossover`/`!rsi_reversal`/
  `!donchian_breakout`/`!keltner_breakout`) and `StrategyRef`. `optimize` =
  `SingleStrategySpec`-only.
- `trailing.rs` — `!sharpe`/`!sortino`/`!volatility`/`!max_drawdown`/`!calmar`. Wraps
  non-`Clone` `Sharpe<S>` etc. in `RebuildIndicator`. `strategy:` is `AnyStrategyRef`
  (`Single | Pairs | Basket`).
- `pairs.rs` — `PairsStrategySpec` (`long_spread` / `short_spread` `Option<SideSpec>`
  + legacy flat long-side keys), `DynPairsStrategy`.
- `basket.rs` — `BasketStrategySpec` + `SelectionRuleSpec` (`!top_bottom`/`!threshold`/
  `!quantile`/`!everything`; **composable** — each ranked rule carries an optional `of:`
  inner defaulting to `!everything`; `SelectionRuleSpec::build` walks bottom-up,
  wrapping each inner in `DynSelection`) + `UniverseSpec` (`!all_of`/`!any_of`). Fields:
  `selection`, `score`, `sizing`, optional `universe`. `!equal_weight <N>` is a sizing
  sugar rewritten to `!value <1/N>` via `rewrite_sugar_tags`.
- `multi_asset.rs` — `MultiAssetStrategySpec` + `MultiSideSpec`, `sizing`, optional
  `universe`. No `symbol:` field.
- `portfolio.rs` — `PortfolioSpec` + `PortfolioChildSpec` + `PortfolioChildStrategy`
  (`Single | Pairs | Basket | Multi`, routed by distinctive top-level key). `weights:`
  is `Option<SpecTemplate<NodeSpec>>` with a `deserialize_with` running
  `rewrite_weights_sugar` (`!fixed [...]` → `!value [...]`, `!equal_weight` → `!value
  1.0`). `.build(cash, schema, costs)` splits cash via `resolve_allocations`, builds
  each typed child at its share, captures each child's stable/warm-up periods **before**
  boxing (the only chance). Per-child weight-share instantiation runs
  `rewrite_value_list_by_index(tree, i)`.
- `mod.rs` — shared `load_value(text, params, base)` (`parse → !import → !param →
  typed parse`).

### CLI auxiliary modules

- **`costs/`** — `--costs`. `spec.rs`: CLI-arg parsing into `CostSpec`; `config.rs`:
  `CostConfig`, `LegConfig<T>`, `ScopedEntry<T>`, typed `CommissionSpec`/`SpreadSpec`/
  `SlippageSpec` (**externally tagged** — `!percentage { rate: 0.001 }`, never `kind:
  percentage`). Dotted `--costs` setter is a *literal* address. See [doc/COSTS.md](COSTS.md).
- **`dyn_indicator.rs`** — facade re-exporting **`fugazi::runtime`** (`DynIndicator` +
  `DynValue` (`Real | Bool | Atom | Candle | Str | Time | Snapshot<String>`) + `DynType`
  + `Adapter` blanket + `AsReal`/`AsBool`/`AsCandle`/`AsAtom`/`AsStr` + `chain`/
  `unstable_wrap`). **New YAML-visible indicators plug in via `dyn_indicator::wrap(...)`.**
- **`csv_source.rs`** — local CSV candle source for `fugazi get csv:PATH`.
- **`data.rs`** — `--series` data frame (`@file.csv` + inline, full-joined on `symbol`
  +`time`).
- **`overlay.rs`** — `--overlay` parsing for `fugazi get`.
- **`calendar.rs`** — `Frequency`, `AssetClass`, `Scope`, `ScopedFrequency`,
  `parse_scope`/`parse_scope_parts`, **`WindowSpec`** (`-w`), **`parse_time_to_millis`**,
  **`detect_frequency_from_atoms`**.
- **`metrics.rs`** — CLI `Metrics` doc + `MetricKey` + `resolve_metric`.
- **`input.rs`** — `@file`-or-inline `Source`; **`base_dir()`**.
- **`glob.rs`** — shell glob (`b*`/`*b*`/`?`/`[a-z]`/`[!abc]`/`\*`), case-insensitive,
  whole-string. Hand-rolled to avoid regex deps.
- **`imports.rs`** — `!import` pass, runs **before `!param`**. Paths relative to the
  importing doc. Cycles = hard error. Object form `!import { path, params: {...} }`
  resolves the imported subtree's `!param` against the inline table first (via
  `params::substitute_partial`).
- **`typecheck.rs`** — static input/output type checking of `NodeSpec` trees, run from
  `NodeSpec::try_from` **only in check mode**. `output_type(&NodeSpec) -> Option<DynType>`
  (`None` = undecidable ⇒ *skip*, never *invalid*) and `children()`. Both matches
  **exhaustive with no wildcard**, so a new `NodeSpec` variant fails to compile until
  classified. Pinned bidirectionally against the engine by two tests
  (`declared_output_type_matches_what_build_produces`,
  `declared_child_expectations_match_what_build_demands`).
- **`undefined.rs`** — type-directed *undefined-value* deserialization for `fugazi
  check strategy`. Three sentinel kinds share one `UndefinedDeserializer`:
  `UNSET_PARAM_KEY`, `UNSET_ARG_KEY`, `UNDEFINED_KEY`. Records per-`!param` the type its
  position demanded. `check` validates a spec's **shape**, never building or driving it.
  Gated by a thread-local `check_mode()` RAII guard.
- **`params.rs`, `args.rs`, `convert.rs`, `list.rs`, `completions.rs`, `pool.rs`,
  `style.rs`** — auxiliary. `params::substitute` and `args::substitute` share a walker,
  differ only in sentinel key.

## Python bindings (`python/src/lib.rs`)

**Type-erased mirror** of the Rust library (pyo3 cdylib, `fugazi-python` → `fugazi`).
See [doc/PYTHON.md](PYTHON.md) for the user-facing API. Python can't carry source
generics across FFI, so everything is erase-then-dispatch via **`fugazi::runtime`**
(`DynIndicator`+`DynValue`, plus `DynIndicatorSync` subtrait adding `Send + Sync` and
deep clone via `runtime::wrap_sync`). Output-typed carriers = `TypedSource<In, Out>`
newtypes: `Source<I>`, `StrSource<I>`, `AtomBox<I>`, `SignalBox<I>` (flattens warm-up
`None` to `Some(false)`). Multi-output stays local as `DynMulti<I>`/`MultiBox<I>`.
`AnySource`/`AnySignal`/`AnyMulti` record the input domain; `map_source!`/
`combine_sources!`/`source_to_signal!`/`sources_to_signal!`/`map_signal!`/
`combine_signals!`/`map_multi!`/`combine_multi!` macros dispatch. **Rule: mirror
constructors use those macros; never name concrete `Ema<Sma<Current, …>, …>`.**

### Parity discipline

**When a Rust API is added/extended/renamed, mirror it in `python/src/lib.rs` in the
same PR.** Two tests catch the common cases: `python/tests/test_parity.py` (every spec
tag is bound, or listed with a reason) and
`list::tests::the_catalogue_documents_every_spec_tag` (every tag appears in `fugazi
list indicators`). Both derive the expected set from **`spec::typecheck::known_node_tags`
/ `known_selection_tags`** (serde's own variant list). Everything below is still on you:

**The grammar descriptor (`spec::grammar`, `fugazi.spec_grammar()`).** `#[derive(SpecGrammar)]`
(the `fugazi-derive` crate) reflects `NodeSpec` / `SelectionRuleSpec` / `UniverseSpec` into one
JSON-serializable record per tag — names, shape, fields (types, required-ness, defaults),
prose, plus a per-variant `kind` / `output` / `since`. It is the single authority for the
spec's *presentation* metadata, the way `known_*_tags` is for its *names*: `spec_tags()` is
now a projection of it, `test_parity.py` pins each Python constructor's defaults against it,
and external tooling (docs, editor LSP, the web grammar table) generates from it rather than
re-encoding by hand. Canonical numeric defaults (MACD's 12/26/9, …) live once as
const-backed `#[serde(default)]` fns in `spec::expr` — feeding YAML deserialization, the
descriptor, and (test-pinned) the pyo3 signatures. **Multi-output indicators are modelled as
separate scalar tags** (`macd_line`, `bb_upper`, …), so `output` is `scalar` and
`projections` is empty for them — there are no `struct`-output node tags today.

Records carry a **`group`**: `node` / `selection` / `universe` (all reflected off serde) are
the expression + basket-selection + universe-declaration vocabularies; `weighting`
(`!fixed`/`!equal_weight`) and `document` (`!import`/`!param`/`!arg`/`!undefined`) are
**hand-authored** in `grammar::document_grammar_tags`, because these load-time tags are
`Value` rewrites that never reach the typed parse (there's no variant for the derive to read).
Their *name set* is still pinned — to `typecheck::REWRITTEN_TAGS` (∪ `fixed`) by
`document_level_groups_are_pinned` — so a new load-time tag can't ship without a row.
`output` is `none` for all five document-level tags (they resolve to another node, or nothing,
at load). **Adding a group / kind / output / field-type value does *not* bump `SCHEMA_VERSION`**
— the record *shape* is unchanged; a consumer with an exhaustive `group` switch treats unknown
values as inert. Deliberately **out of scope**: the nested config sub-documents (`costs:`,
a portfolio child's embedded strategy), documented as such in the `spec_grammar()` docstring.

Records also carry a **`category`** (v3, 0.51) — the fine conceptual sub-group (`moving
averages`, `oscillators`, `bands`, `trend / directional`, …), one rung finer than `kind`, for
consumers that present the vocabulary in curated sections. It's editorial (not reflectable off
the type), so `grammar::spec_grammar` *stamps* it from the `pub CATEGORIES` taxonomy table —
the single authority for both the classification and its curated order — after reflection; a
test pins the table to cover every tag exactly once, so nothing ships uncategorised. The CLI
`fugazi list indicators` catalogue is now a **pure projection** of the descriptor (signatures
derived from `shape`/`fields`/`payload`, prose from the stamped records, grouping + order from
`CATEGORIES`) rather than a hand-maintained parallel table — the drift-prone duplication the
descriptor exists to kill. Prose is stripped of rustdoc link markup (`` [`Type`] ``,
`[text](url)`) in the derive's `doc_string` so it reads as clean presentation text for every
consumer. `category` is a new *field*, so it **did** bump `SCHEMA_VERSION` to 3.

`spec_json_schema()` (`fugazi.spec_json_schema()`) is a second projection of the same
descriptor: a JSON Schema (draft 2020-12) for the expression grammar's **JSON bridge form**
(single-key `{tag: body}` objects + bare-literal shorthands + authored load-time
placeholders), for structural validation by consumers without the Rust build path.
`spec_document_json_schema()` extends it to the whole document — the five strategy shapes as
a `oneOf`, each `$ref`-ing the same node/selection grammar for every expression slot. Both are
**complementary to `fugazi check`**, not a replacement — `check` (the typed parse) remains the
authority, validating the type discipline and build-time semantics the schema can't express.
See [proposals/spec-json-schema.md](proposals/spec-json-schema.md).

- **New indicator/signal/operator** → `#[pyfunction]`, register in `#[pymodule] fn
  fugazi`, smoke test in `python/tests/test_fugazi.py`. Single-output real-source use
  `src_period!`; bar-only `bar_period!`/`bar_noarg!`; multi-output `bar_period_multi!`
  or hand-written. New fluent method → `#[pymethods]` on `PyIndicator`/`PySignal`.
- **New metric fn** → `#[pyfunction]`, name to `register_metrics_module`. `Option<Real>`
  stays; `Real` → `f64`.
- **New field on `Trade`/`DrawdownSegment`/`Order`** → `#[getter]` on `Py*` + update
  `__repr__`.
- **New remote provider** → `Py*` client + register + `fetch(provider=…)` branch. Every
  provider exposes one `.fetch(...)` routed through `fetch_frame` → `build_series_frame`.
- **Changes to `Candle`/`Atom`/`OverlayInfo`/`Schema`/`SchemaBuilder`** → update `Py*`
  field-for-field.

**Bound — all five strategy shapes + `run`.** `PyStrategy` mirrors
`SingleAssetStrategy`; `PyPairsStrategy` / `PyMultiAssetStrategy` / `PyBasketStrategy`
mirror their Rust siblings and drive over a sequence of snapshots. Multi / basket
factory slots are per-symbol **Python callables** converted via
`signal_factory_from_callable` / `source_factory_from_callable` (the closure captures
the `Py<PyAny>` callable and calls it once per symbol under the GIL). `PyPortfolio`
mirrors `Portfolio::builder()`; children are the other four Py builders, each
**materialized** at its share of the seed via the `materialize(...)` seam. **All five
shapes share one run seam** (`run_over_wallet!` / `run_prepared!` in
`python/src/strategy.rs`): `.run(wallet, …)` accepts a `PaperWallet` **or** the live
`OkxWallet`. The seam handles **external positions automatically** via the core
`SleeveWallet`. `test_specs.py::test_portfolio_builder_matches_the_equivalent_yaml_document`
pins the builder against the equivalent `portfolio:` document. (The `load_spec(...).run(wallet)`
path is still `PaperWallet`-only.)

**Not bound** (don't add without asking): position-anchored protective levels,
`BasketStrategy::selection(closure)` escape hatch, per-child weight-share indicators
(`weights:` as an expression — the YAML path has it).

**Bound — YAML spec loading + run/evaluate/optimize/walkforward.** `ta.load_spec(text,
params={}, base_dir=".", kind="auto")` parses through the same `spec::load_value`
pipeline and auto-detects shape. Returns a `StrategySpec` pyclass with the same
`.run` / `.evaluate` shape as the manual builders. `ta.optimize(text, snapshots, ...)`
wraps `spec::optimize::optimize`; `walkforward=(is, oos[, embargo])` switches to the
walk-forward kernel. `windowed=N` and `walkforward=` are mutually exclusive.
`ta.TradingCostsConfig({...})` wraps `CostConfig`. **Run resuming:**
`spec.run_resumable(wallet, snapshots, resume=None, flatten=False) -> (report,
state_json)` — PaperWallet-only.

**Bound — overlay calculation.** `ta.compute_overlays(series, overlays, params=None) ->
(schema, augmented)` computes derived overlay columns from indicator specs. `overlays`
is a YAML doc (`name: !expr { ... }`) or a dict. Core in `src/spec/overlay.rs`, shared
with the CLI's `-x`. Output schema = existing columns + new appended; every augmented
atom binds to the **one returned schema `Arc`** (`get.rs`'s `Arc::ptr_eq` guard requires
it — **use the returned schema downstream**). A computed column reads `None` while
warming up (why `OverlayInfo` slots are `Option<OverlayValue>`). Snapshot mode computes
per (symbol, freq) series driven by size-1 snapshots.

**Wallet-method parity is hand-maintained.** `Wallet` is a *trait*, which Python can't
reflect into, so `python/tests/test_parity.py::test_wallet_surface_matches_the_ledger`
carries an explicit `WALLET_BOUND` / `WALLET_NOT_BOUND` list. It exists because the gap
let a real regression through — `set_stop` grew a `size` and the binding kept passing a
hardcoded whole-position size. **Change a `Wallet` method → update that list in the same
PR.** Currently unbound with reasons: `take_rejections`, `set_costs_for`.

**Intentionally not bound**: `Strategy` trait as subclassable, the CLI binary, `Wallet`
as a trait to *implement* in Python (only concrete `PaperWallet` / live `OkxWallet` are
bound), Rust-internal types (`Position`, `PositionField`, `Ack`, `OrderId`, `Reference`,
`Units`). **Monte Carlo** *is* bound (`ta.MonteCarloConfig`,
`StrategySpec.evaluate(montecarlo=cfg)`). Trailing risk indicators *are* bound
(`sharpe_of` / `sortino_of` / `volatility_of` / `max_drawdown_of` / `calmar_of`). The
per-tag ledger lives in `python/tests/test_parity.py`.

**Spec-loading discipline**: a new YAML tag under `src/spec/` usually needs no Python
change — the `StrategySpec` wrapper calls the typed-parse machinery. But: (a) a new
top-level strategy shape needs a `detect_kind` arm (in `python/src/lib.rs`) and a
`LoadedSpec` variant; (b) new per-kind `.run()`/`.evaluate()` plumbing must be threaded.
`optimize` shares the same dispatch as `.run()`; keep them in sync.

### Python layout — one module per concern

| File | Holds |
|---|---|
| `carriers.rs` | type-erasing `TypedSource` + `Source`/`SignalBox`/`StrSource`/`AtomBox`/`MultiBox`, the `AnySource`/`AnySignal`/`AnyMulti` domain enums |
| `macros.rs` | the 8 domain-preserving dispatch macros. `#[macro_use]`d first in `lib.rs` |
| `classes.rs` | `PyCandle`/`PySchema`/`PySchemaBuilder`/`PyOverlayInfo`/`PyAtom`/`PyFrequency`/`PySelector`/`PySnapshot`/`PyAtomSource`/`PyIndicator`/`PySignal`/`PyMulti` |
| `strategy.rs` | `PyWallet`/`PyOrder`/`PySize`, the four strategy builders, `PyRunReport`, `AtomLift`, per-symbol factory helpers, catalogue constructors, trailing risk indicators |
| `constructors.rs` | leaf sources, `src_period!`/`bar_period!`/… invocations, hand-written `macd`/`bollinger`/`keltner`/`donchian`/`stoch_rsi`, `resample`/`latch`, `unstable`, `get`, `compute_overlays` |
| `sources.rs` | `PyBinance`/`PyYahoo`/`PyCoinGecko`/`PyBinanceVision` + `fetch` |
| `metrics.rs` | `PyFill`/`PyTrade`/`PyDrawdownSegment` + one `#[pyfunction]` per metric; injected into `sys.modules["fugazi.metrics"]` |
| `spec.rs` | `PyCostConfig`/`PyStrategySpec`/`PySweep`/`PySweepRow`/`PyWalkForward*` + `load_spec` / `optimize` / `spec_tags` |
| `prelude.rs` | the shared `use` block every module glob-imports |
| `lib.rs` | module wiring + `#[pymodule] fn fugazi` |

Two wiring rules: modules glob-import their siblings (`use crate::classes::*;`) so
cross-module references stay path-free — **not** through a `pub(crate) use` at the crate
root, which creates a resolution cycle. And `lib.rs` names every registered function
**explicitly** rather than by glob, because `wrap_pyfunction!` resolves a hidden item
pyo3 generates beside each `#[pyfunction]` that a glob import doesn't carry.

Cargo: `python/Cargo.toml` depends on `fugazi_core = { package = "fugazi", …
default-features = false, features = ["sources", "runtime", "spec"] }`. `pyo3 = "0.29"`
with `abi3-py39`. Test: `maturin develop` then `pytest python/tests/`.
