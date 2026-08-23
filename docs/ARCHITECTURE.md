# Architecture (deep reference)

Detailed internals for `fugazi`. [CLAUDE.md](../CLAUDE.md) is the load-bearing
summary — the invariants, conventions, and the "grep before writing" table. This
file is the depth behind it: read the relevant section before touching a
subsystem. [docs/CONTRIBUTING.md](CONTRIBUTING.md) is the change procedure.

Three composable layers: **indicators** (numeric sources), **signals**
(`Indicator<Output = bool>`), **strategies** (decision layer trading into a
wallet).

## Indicators — numeric sources (`src/indicator.rs`, `src/indicators/`)

`Indicator` has `Input`/`Output`, `update(&mut self, Input) -> Option<Output>`,
`value()`, `is_ready()`, `reset()`, `save_state()`/`load_state()` (default no-op;
see *Run resuming*), plus:

- **`warm_up_bars()`** — *exact* samples before first `Some`. Wrappers add on
  top; binary carriers take max. `tests/warm_up.rs` asserts exactness — add new
  indicators to that battery.
- **`unstable_bars()`** (default `0`) — extra samples IIR smoothers need for
  the seed's residual to decay below `SETTLE_TOLERANCE = 1e-3`. Wrappers sum into
  the source's.
- **`stable_bars()`** = warm-up + unstable.

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
  `FromIterator` + the **sole-atom trio**, three spellings of one decision that
  differ only in how a 2+ priceable snapshot is answered (all three read `None`
  on empty and `Some` on exactly one): `sole_atom_or_panic` (**panics** — the
  unrooted guard, where nothing named an asset), `sole_atom_or_none` (`None` —
  backs `Pick::rooted`'s and `extract_self_atom`'s fallback, where a 2+ snapshot
  means "the blessed leg is absent this bar", not "mis-wired"), and
  `sole_atom_or_err` (`Err(count)` — the FFI boundary, which turns it into a
  `ValueError` rather than an unwinding `PanicException`).
  `impl Snapshot<Selector>` adds `find(query)`.
- **`Pick<S = Identity<Snapshot<Selector>>>`** projects one asset: `Output =
  Atom`. Three modes: `Pick::new()` (empty selector → sole-atom, **panics** on
  2+); `Pick::matching(selector)` (strict structural match → `None` when absent —
  the explicit cross-asset form); **`Pick::rooted(selector)`** (match, else fall
  back to `sole_atom_or_none` — the *blessed-series* root a context installs for
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
autocorrelation via `Correlation::new(x.clone(), x.lag(n), period)`). Off
`WindowStats`/`WindowCovariance`, which update in O(1) and read dispersion in one
centred O(period) pass — the `E[X²] − E[X]²` shortcut cancels away `(mean/σ)²`
significant digits and was unusable at market scale (see that module's docs). **`VarianceRatio` is the deliberate exception** —
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

Passthrough forwarding everything to `S` *except* `unstable_bars() = 0`. Fluent
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
  GtOp>` etc. The op carries a **`Tolerance { abs, rel }`**, whose band is
  `max(abs, rel · max(|lhs|, |rhs|))`; the default `DEFAULT_TOLERANCE` is
  `(1e-12, 1e-9)`. Relative on purpose: an absolute `1e-8` was `1e-13` relative at
  a five-figure price — below f64 resolution there, so it gave no noise protection
  at all — and `1e-4` relative on a per-bar return. `Gt::with_epsilon(a, b, eps)`
  overrides with an **absolute** band (a deadband the caller means literally);
  `Gt::with_tolerance(a, b, t)` takes both terms. YAML `epsilon:` is the absolute
  form. The execution-side quantity epsilons are separate and live in
  `src/wallet.rs`: `POSITION_EPSILON` (units), `PRICE_EPSILON` (price),
  `CASH_EPSILON` / `cash_tolerance(scale)` (money).
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

- **Readiness gate.** `is_ready()` = `bars_seen >= max(stable_bars())` across
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
- **Not a rule engine.** `(signal, action)` tables are a deliberate non-goal.

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
- **Side balancing.** `.balance_sides(bool)` (YAML `balance_sides:`), **on by
  default**, scales per-symbol sizes at each rebalance so `Σ long_sizes ==
  Σ short_sizes`; the smaller-side sum is the target gross-per-side (never levers
  up). This is dollar neutrality in the classic sense — named for the mechanism,
  since fugazi does no FX and takes no view on the numeraire. Default-on because
  an unbalanced cross-sectional basket carries net exposure its ranking never
  asked for. A one-sided selection **passes through unscaled**: there is no
  counter-side to balance against, and a long-only basket (`top_bottom(n, 0)`, or
  a `threshold` that admits one side this bar) is an ordinary shape. It must not
  short-circuit `trade()` — the same loop's `None` arm is what closes de-selected
  symbols, so returning early would hold a stale one-sided book rather than sit.
  Balancing equalizes *intent* at rebalance, not realized notional: sizes are read
  on transition only, so the balance holds as legs open and then drifts with price
  like every other basket size.
- **Per-leg protective.** `.long_stop_loss(|sym, &Position| level)` /
  `.long_take_profit(...)` / `.short_stop_loss(...)` / `.short_take_profit(...)`
  per-symbol factories, plus YAML `long: { stop_loss: ..., take_profit: ... }` /
  `short: { ... }` using `BasketSideSpec` templates with `!arg SYM` and `!entry` /
  `!peak` / `!trough` anchored to *that* symbol's Position.
- **Python**: `ta.BasketStrategy().scored_by(fn).sized_by(fn).top_bottom(l, s)`
  (or `.threshold` / `.quantile`), `.balance_sides(bool)`, `.rebalance_on(sig)`,
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
  blocks until every listed leg is past its own `stable_bars`.
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
  the one size the wallet will `fit_to_account`). Sells and short targets use
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
  weights(&self, n) -> Vec<Real>; warm_up_bars; reset }`. Two built-ins:
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
- **Readiness.** `is_ready() = bars_seen >= policy.warm_up_bars() && bars_seen >=
  rebalance.stable_bars() && every child is ready`.
- **YAML.** `portfolio:` prefix + `PortfolioSpec` (`src/spec/portfolio.rs`).
  `children:` is an ordered list of `{ name, strategy }` slots; `strategy:` accepts
  any of the four shapes routed by distinctive top-level key (`left`+`right` →
  pairs, `selection` → basket, `symbol` / preset tag → single, else multi).
  `rebalance_on:` optional (any boolean `NodeSpec`, default `!never`) **except
  when `weights:` is non-constant, where omitting it is a build error** — weight
  shares are read only inside a rebalance cycle, so an ungated dynamic
  expression would be updated every bar and consulted on none, leaving the
  portfolio on its equal-split seed with the weighting rule inert
  (`weights_are_constant` is the exemption test: a top-level `!value` scalar or
  list, both of which the build-time seed already captures; `!never` is the
  named opt-out). The signal-anchor and `Book` handed to `NodeSpec::build` at
  the portfolio level are dummies, so `!entry`/`!drawdown` read empty — use
  snapshot / calendar / cadence signals. `rebalance_policy:` optional (`!proportional` | `!largest_first`, default
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
  firing *means* when the limit may never fill), not missing plumbing; a
  `(signal, action)` table remains a deliberate non-goal.
- **`PaperWallet` timing.** Queues market moves, fills at *next* bar's `open`;
  protective fill when the bar trades through the trigger (at level, or `open` on
  gap). A backtest never fills on the signal's own bar. Market moves queue one per
  symbol (latest wins); resting stops register one bracket; resting limits one per
  symbol. `update` marks the bar, flushes queued at `open`, matches protective
  against `[low, high]` (stop precedence; fill flattens + OCO-cancels bracket),
  **then** limits — so a protective exit books before a limit entry on the same bar.
  A triggered-but-unaffordable limit is a rejection and is *consumed*. Resting fill
  price provably in `[low, high]`.
- **Errors.** `WalletError` (`UnknownPrice`, `InvalidPrice`, `InvalidQuantity`,
  `PriceOutOfRange`, `InsufficientFunds`, `ExceedsMaxGross`,
  `UnsupportedOperation`, `Venue`). `InvalidQuantity` is the account's guard
  against a non-finite request: a `NaN` reads false against **every** `>` and
  `<`, so it clears both solvency rules and the range check, books a `NaN`
  position, and takes cash and equity with it for the rest of the run.
- **Optional capability methods** (defaulted, opt-in per impl): `adjust_funds` /
  `set_limit` / `cancel_limit` / `cancel` / `poll_fills` / `take_rejections`, plus
  **`positions() -> Vec<Units<Sym>>`** (default empty — "can't enumerate", *not*
  "holds nothing") and **`set_costs_for(sym, costs)`** (default
  `UnsupportedOperation` — a live venue owns its own fees). Both moved onto the trait
  for the portfolio's erased sub-wallets; no inherent twin remains.
- **`can_short() -> bool`** — capability **introspection**, default `true` (a
  position is signed `Units`, so shorting is the baseline; `PaperWallet` states it
  explicitly, `OkxWallet` says `true` for net-mode swaps). A spot venue overrides to
  `false` — `CoinbaseWallet` does, and still clamps a negative target to flat and
  books a `Rejection` for the remainder: **the flag informs, it does not enforce**.
  Wrappers delegate to what they wrap (`SleeveWallet` → inner; `LedgerWallet` → the
  account's answer, cached into `PortfolioInner::account_can_short` by
  `Portfolio::trade` before children run, since a child holds no handle on the
  account). Lets a caller degrade to long-only *before* trading instead of learning
  the limit one rejection at a time.
- **`data_sources() -> &'static [&'static str]`** — the third introspection read,
  and the one that crosses a subsystem boundary: which market-data providers quote
  what this account trades, named as the `sources` layer names them (the `provider:`
  token a `fugazi get` spec takes). Default `&[]` = **"does not say"**, the reading
  `quote_ccy`'s `None` asks for; `PaperWallet` takes it (simulated money has no venue
  whose prices are the *right* ones), `OkxWallet` answers `["okx"]`, `CoinbaseWallet`
  `["coinbase"]`. **Introspection, not fetching** — the wallet still has no view of
  the market and is fed prices through `update`; this lets a caller preflight the
  pairing it was going to make anyway (a live runner warning it is about to drive an
  OKX account off Yahoo bars). **Venue granularity is all a provider name can carry**:
  `OkxWallet` trades `instType=SWAP`, so its bars are `okx:` fetched for the *swap*
  instrument id, not the spot pair the same provider serves — pairing the instrument
  stays the caller's job. Wrappers delegate; `LedgerWallet` delegates *here* where it
  declines to for `quote_ccy`, because a `&'static [&'static str]` borrows nothing
  from the portfolio guard it is read through (cached as
  `PortfolioInner::account_data_sources` beside `account_can_short`).
  **Deliberately not a typed handle.** Returning `impl SeriesSource`es would point
  unconditional core at the default-on `sources` feature and hand every downstream
  `Wallet` implementor a registry it cannot extend; a name is the widest thing both
  halves already agree on.
- **`leverage(&sym) -> Option<Real>`** — the fourth introspection read, and the
  only **per-symbol** one, because venues configure it that way (OKX carries a
  `(instId, mgnMode)` setting, not an account-wide one). Default `None` = "does
  not say", never `1x`. `CoinbaseWallet` answers `None` *structurally* — spot has
  nothing borrowed to parameterise, the same fact `can_short() == false` reports
  the other way. `OkxWallet` answers from a cache filled for free off the
  positions payload's `lever` field and, for a symbol the account is flat in, by
  an explicit `refresh_leverage(sym)`; the accessor takes `&self` and every
  account read here answers from cache rather than blocking on REST.
  `PaperWallet` answers `Some(max_gross)` — unlike a currency label this one it
  *knows*, because it is a rule it applies to every fill.
  **Reporting, not control**: nothing in fugazi sets a venue's leverage. A live
  account's is configured out of band and can change under a running strategy, so
  the point is that a deployment can *record* what its fills executed at and
  reconcile it against the `max_gross` the backtest it tracks was run under —
  without which a live equity curve is uninterpretable against that backtest.
  Wrappers delegate; `LedgerWallet` delegates *here* like `data_sources` and
  unlike `quote_ccy`, since an `Option<Real>` is a copy and borrows nothing from
  the portfolio guard (cached per symbol as `PortfolioInner::account_leverage`,
  over the universe the portfolio has seen priced).
- **`observe(&sym, &atom)`** — the **data** seam, default ignore. The driver
  hands over each bar's whole atom (candle *plus* overlay columns) before
  anything is priced, and the wallet takes what its own models asked for. Exists
  for one thing: a `CarryModel` whose rate is a published series rather than a
  constant — a perpetual's funding changes every settlement and flips sign, so it
  has to reach a cost model the way a price does. The inversion matters: nothing
  above the wallet learns which column a cost bundle was configured with, or that
  carry exists. Live wallets ignore it (the venue charges its own funding and
  reports the result in the balance, so simulating it would double-count).
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
  `Size` (sides `"buy"`/`"sell"`; `WalletError` → `ValueError`).

### Live venues (`src/live/`, feature `live`)

Two backends, and **one order flow between them**. `src/live/venue/` holds the
shared half:

| Module | Holds |
|---|---|
| `venue/mod.rs` | the exchange-precision arithmetic every venue quantises with |
| `venue/rest.rs` | `HttpCore` — the `reqwest` client, the private `tokio` runtime, the base URL |
| `venue/state.rs` | `InstrumentGrid`, `OrderRegistry`, `RestingOrder`, `Bracket`, `LiveLog`, and the `LiveCore` that owns them |
| `venue/fills.rs` | `VenueFill` and `FillCursor` — one fill shape, two dedupe models |
| `venue/backend.rs` | **`VenueBackend`** — the ten hooks a venue supplies |
| `venue/flow.rs` | the shared `Wallet` bodies, over those hooks |

`VenueBackend` is `pub(in crate::live)` and **must stay that way**. Capability on
`Wallet` is expressed by overriding-or-defaulting on *one* trait; this is an
internal implementation-sharing trait, and it changes nothing a downstream
implementor of `Wallet` sees. Every hook is a venue *fact* — an endpoint, an
envelope, a request body — and returns `LiveError`, never `WalletError`, because
"log it" versus "log it and buffer a `Rejection`" is the call site's decision and
every call site is in `flow`. **A hook whose body wants to be `true` / `false` /
`unreachable!()` / empty on one venue is the signal that the method belongs back
in that venue's `Wallet` impl.**

`flow`'s bodies are **free generic functions**, not provided trait methods: a
provided `update` would collide with the `Wallet::update` it implements, and a
free function can't be silently overridden — a venue needing different behaviour
has to add a visible hook. They may read only five `Wallet` methods (`funds`,
`equity`, `price`, `position`, `can_short`), the five neither wallet delegates
back; anything else recurses. A trait can't have fields, so state is reached
through `core()` / `core_mut()`, and every body is a straight-line sequence that
re-borrows — a `&mut` into the core cannot be held across a hook call.

What stays per-venue is what differs in *kind*: the credentials, the signing
(`OkxWallet` base64-HMAC-SHA256 over `timestamp+method+path+body`,
`CoinbaseWallet` an ES256 / ECDSA P-256 JWT per request), `refresh_account`, the
envelope parsers, the request bodies inside the `place_*` hooks, and the six
reads that *are* the account shape. `OkxWallet` holds one signed swap position in
base units (`InstrumentGrid::contract_multiplier` converts, so nothing above the
wallet sees a contract); `CoinbaseWallet` holds a table of currency balances,
cannot short, and values its own book through `wallet::marked_sum` so the sum is
canonical across processes.

Both logs are bounded at `DEFAULT_RETENTION`, and so is the fill-dedupe set —
these run for months, and an unbounded reporting artifact in that deployment is a
leak rather than a convenience.

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

**Ruin is a run outcome, decided here.** At that last step, if equity is `<= 0`
and the run hasn't already been ruined, `run` records the bar in
`RunReport::ruin_bar`, liquidates through `Wallet::flatten` (fills and refusals
booked at that bar, like any other), and pushes `0.0` instead. Every later bar
still `update`s the wallet and the strategy — the one-entry-per-snapshot
invariant holds and a resumed run's state stays correct — but `trade` is gated
off and the curve stays pinned at `0.0`. `warm_up` (`DriveMode::WarmUpOnly`)
never triggers it: it submits nothing, so it must not close anything either.

The reason it lives at the driver rather than in `metrics`: per-bar returns are
`(e - prev) / prev`, which **inverts sign** once `prev < 0`, so a run allowed to
continue past zero reported further losses as positive returns and gave whole
regions of an `optimize` grid a genuinely positive Sharpe. Bounding the curve
where it is produced makes `drawdown.max_pct <= 100` true *by construction*,
makes CAGR `-100%` instead of silently `None`, and leaves `--best-by`,
`--smooth`, the walk-forward composite and the Monte Carlo bootstrap correct
with no code of their own. `flatten_open_positions` (`--flatten`) returns early
on a ruined report for the same reason — the book is already closed, and
overwriting the final point would un-pin the curve. **Only the account's own
equity counts**: a portfolio is an ordinary strategy trading the wallet it was
handed, so total ruin is caught at this one site with no per-shape branch, while
a single child *ledger* going negative is notional attribution netted against
its siblings on one real balance, not insolvency.

Ruin is the *floor*, not the margin model. Liquidation before zero is
`PaperWallet::with_maintenance_margin(ratio)` — **opt-in**, because the ratio is
a venue assumption fugazi will not guess — which force-closes the book as
`OrderKind::Liquidation` when equity falls below `ratio × gross`, triggered on
each bar's adverse extreme. `with_max_gross` is a different bound: it limits what
a strategy may *ask for* at fill time and never re-checks a book that drifts over
the line on a mark. What is still absent is the rest of a real margin engine —
partial liquidation, tiered ratios, continuous marking, and a fill price better
than the bar's close. See TODO *Liquidation is modelled; a full margin model
still is not*.

`run<Sym, S, W, I, A>` where `A: Into<Snapshot<Sym>>`. `Vec<Atom>` / `Vec<Candle>`
produce untagged size-1 snapshots; single-series callers use `Snapshot::single(sym,
atom)`. `RunReport<Sym> { equity_curve, fills, rejections, initial_equity, ruin_bar }` —
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
  `crate::Indicator::save_state`), `#[state(skip)]` = `PhantomData` /
  `Arc<Mutex>` shared handles, `#[state(window)]` = a fixed-capacity
  `indicators::stats::Ring<T>`, `#[state(config)]` = a value the *spec* fixes
  (a period, a band multiplier, a tolerance), `#[state(core)]` = an embedded
  stateful core that carries its own configuration (`WindowStats`, `EmaState`,
  …). **Default-is-state is deliberate:** forgetting
  `#[state(source)]` on a new box field is a **compile error** (a box isn't
  `Serialize`), not silent loss.
  - **Why `window` is its own role.** A `Ring` serializes as a bare
    oldest-first array — the shape the `VecDeque` it replaced produced, so old
    run-state files still load — and that array does **not** record the capacity.
    A window saved mid-warm-up is shorter than its period, so a plain
    `Deserialize` would restore it at the array's length and silently shrink the
    window for the rest of the run. `#[state(window)]` routes the load through
    `stats::LoadWindow`, which takes the capacity from the **destination**;
    that is sound precisely because the contract above is that the structure is
    rebuilt from the spec first and only values are replayed in. This is why
    `Ring` has no `Deserialize` impl and must not grow one.
  - **Why `config` and `core` are their own roles.** Both are saved like plain
    state; both are **checked** rather than replayed on load. Nothing stops
    `--resume` being pointed at a state file written by a *different* document,
    and replaying config in place made that silently wrong in the worst way — a
    `Diff` of period 4 restored from a period-2 blob took the blob's `period`
    field and kept the destination's four-slot buffer, so it reported a warm-up
    of 3 while differencing over 4 bars; a `Percentile` built for the 90th
    became the 10th; an `Sma(5)` became a self-consistent `Sma(3)` that
    contradicted the document it came from. `config` compares the field against
    the rebuilt destination; `core` routes through `stats::LoadCore`, which does
    the same for the period (or smoothing factor) the core records inside its
    own blob. Either mismatch is an `Err` with the `field > ` breadcrumb, so the
    operator sees which knob moved. **A missing key is accepted** — a state
    written before a field existed cannot disagree with anything.

**Shared/path-dependent state** is serialized once at the strategy level, never
per-indicator: `Position::snapshot`/`restore`, `Book::snapshot_state`/`restore_state`,
`Wallet::snapshot_state`/`restore_state`, `PortfolioInner`/`Ledger`.
**`PositionField`/`BookField` keep the no-op default** — they hold a clone of the
shared handle, and serializing per-accessor would double-count.

`Wallet::snapshot_state`/`restore_state` default to `Null` / accept-and-ignore, which
is the right answer for a **live venue**: the broker owns the positions and the cash,
so a local snapshot can only go stale and replaying one would overwrite reality with a
guess. `PaperWallet` overrides both (positions/cash/pending/resting — **not**
costs, which are re-primed by the caller), because it *is* the book. Both carry
`where Self: Sized` so `dyn Wallet<Sym>` stays object-safe for `Strategy::trade`.

**The blotter and the rejection log are excluded, because history is not state.**
Nothing in the fill, pricing or restore path reads either — `orders()`/`rejections()`
are observability accessors, and `RunReport::fills` is built from `Wallet::update`'s
return value, not from the blotter. Persisting them made the state grow linearly in
bars forever: on a 1500-bar 8-symbol basket they were **98%** of the file (253 KB of
258 KB), and dropping them took it to 10.6 KB and made it flat in run length. So a
resumed wallet's `orders()` covers the resumed chunk — which is what the per-chunk
`RunReport` always did. The keys were simply dropped from `WalletSnapshot`; serde
ignores unknown fields, so an older state still resumes and `RUN_STATE_FORMAT_VERSION`
stays at 2. The same reasoning bounds them *in memory*: they retain
`wallet::DEFAULT_RETENTION` entries, with `PaperWallet::with_retention(None)` as the
named opt-out, so a strategy driven live for years doesn't accumulate every fill it
ever booked.

**Driving.** `RunnableStrategy::{save_state, restore_state, drive_resumable}` +
`RunState { format_version, kind, last_bar, bars_seen, strategy, wallet }`.
`drive_resumable(snaps, cash, costs, resume: Option<&RunState>, flatten: bool)`
restores before the run (rejecting a `format_version` / `kind` mismatch), optionally
flattens open positions at the end, and surfaces the final `RunState`. `drive` is
the thin `(…, None, false).0` wrapper. `backtest::run_iteration_resumable` is the
CLI/spec-shape entry.

`RunnableStrategy` is object-safe (`try_build` hands back a `Box<dyn …>`), so its
driving methods build a `PaperWallet` internally. To supply the account — a primed
paper wallet, or a live venue — use **`RunnableStrategyExt::drive_resumable_with(snaps,
wallet, resume, flatten)`**, blanket-impl'd over every `RunnableStrategy` including
`?Sized` ones. Both spellings share one body, `runnable::drive_over`. Against a live
wallet `RunState.wallet` is `Null` and the venue is re-read on resume.

**`format_version` is 2.** v1 blobs are rejected outright with no migration: a v1
portfolio blob does not *contain* its children's state, so a migration could only
fabricate it. Re-run the history (resuming optimizes that, it doesn't replace it) or
finish on the build that wrote the state.

**Per-shape fidelity: all five are exact.** `tests/resume.rs` holds every shape to
bit-identical equity *and* fills across a three-way chunked run, plus a separate
assertion that the state blob itself round-trips.

- single / pairs — a flat field set, restored eagerly, every field error propagated.
- basket / multi — the same, plus the shared `Book` and the `rebalance` gate (a
  cadence like `!every 7` carries a bar counter, so its *phase* is state). Per-symbol
  chains are built **eagerly inside `restore_state`**, not lazily on next sighting:
  `backtest::run` routes fills through `on_fill` *before* `update`, so a resumed run's
  first-bar fill — the previous chunk's queued order settling — would otherwise reach
  the `Book` with no `Position` built to receive it; and `save_state` can only
  serialize symbols it holds state for, so a symbol that doesn't quote during a chunk
  would be dropped at that chunk's save rather than carried.
- portfolio — ledgers, aggregate `Book`, `bars_seen` (it gates `is_ready`), the
  rebalance gate, the per-child weight-share chains, and **every child's own state**
  through `Strategy::save_state`. Children are keyed positionally with the name
  carried as a check; a shape change between save and resume is an error, not a
  partial apply. `PortfolioInner` also persists in-flight netting state
  (`pending`/`owners`/`protective`): a `PaperWallet` fills at the *next* bar's open,
  so a portfolio that traded on a chunk's last bar has flow across the seam by
  construction, and dropping it would break `Σ ledgers == account` permanently.
- **Trailing metrics** (`Sharpe<S>` et al.) serialize the embedded strategy + private
  wallet + `prev_equity` + the rolling window; their `Indicator` impls therefore
  require `Sym: Serialize + DeserializeOwned`.

**Flatten toggle.** `backtest::flatten_open_positions` delegates to `Wallet::flatten`,
which closes every open position **in the account** at the last bar — through the
normal execution path, so costs and commission apply and a real `OrderId` is minted
per leg — then routes the fills to `on_fill` and the report. `PaperWallet` overrides
the trait default because its queued moves settle at the *next* bar's open and there
isn't one; it goes straight to `fill_at`, the engine every other fill uses. The final
equity point is **overwritten, not appended** (each leg closes at the mark that point
was computed from, so only the cost drag changes, and
`equity_curve.len() == snapshots.len()` is an invariant every consumer relies on).
The zero-cost gross twin is flattened alongside the priced run, or `costs_section`
would pair net fills against gross fills it doesn't have. The captured `RunState` then
holds a genuinely flat book, so resuming from a flattened run continues from flat.

**Warming without trading.** `backtest::warm_up` is `run` with the `trade` step gated
(`DriveMode::WarmUpOnly` — one loop, one branch), surfaced as
`RunnableStrategyExt::warm_up_over` and Python `spec.warm_up(...) -> state_json`. It
closes a *pause gap*: bars that elapsed while a deployment was stopped warm the
indicators without booking trades at prices nobody could have traded at, so a
long-period indicator keeps its warm-up across a pause. Fills that arrive anyway (a
resting order left from before) still route to `on_fill`, or the strategy's position
would drift from the account's.

**Surfaces.** CLI `fugazi run … --save-state <file>` / `--resume <file>` /
`--flatten`. Python `spec.run_resumable(wallet, snapshots, resume=None, flatten=False)
-> (report, state_json)` and `spec.warm_up(wallet, snapshots, resume=None)
-> state_json`, both taking a `PaperWallet`, an `OkxWallet` or a `CoinbaseWallet`.

**Adding a stateful indicator ⇒ add `#[derive(SaveState)]` + field annotations + the
two `impl Indicator` forwarding lines**, or its state is silently lost on resume.
tests/resume.rs is the acceptance gate.

**Bounded, documented limitations:** the generic-typed prior of `Change` and the held
value of `Latch` are skipped (their `S::Output` is unbounded) → a one-bar re-warm at
the resume boundary; `Shared`/`SharedComponent` are no-op (the spec layer never uses
`.shared()`).

Config drift *is* guarded, per field, by `#[state(config)]` / `#[state(core)]`
(above) on top of the `kind` + `format_version` checks — but the guard is only as
complete as the annotations, and it can only catch a knob that is **written into
the state at all**. A parameter that no `SaveState` struct carries (a strategy's
`bars_per_year`, a cost model's rates, a portfolio weight literal) still resumes
without complaint, because there is nothing in the blob to disagree with. The
whole-document fingerprint that would close that gap is not written: it would
have to be stable across an equivalent re-spelling of the same document to avoid
refusing legitimate resumes, and nothing in the loader produces such a
normalisation today.

## Metrics — one function per metric (`src/metrics.rs`)

See [docs/METRICS.md](METRICS.md) for the user-facing catalogue and caveats.
Internals:

**No aggregate `compute`.** Every metric is its own `pub fn`. Three **intermediate
builders**:

- **`per_bar_returns(equity, initial_equity) -> Vec<Real>`** — for return-moment /
  risk-adjusted metrics.
- **`reconstruct_trades<Sym: PartialEq>(fills) -> Vec<Trade>`** — walks the blotter
  **per symbol**, each with its own signed position and volume-weighted entry; one
  closed leg = one `Trade { entry_bar, exit_bar, side, units, entry_price,
  exit_price, pnl, return_ratio }`. Open legs live in an insertion-ordered list
  keyed by borrowed symbol, so grouping needs only `PartialEq` and is deterministic
  by construction; trades are emitted as they close, hence in non-decreasing
  `exit_bar` order. Before 0.63.2 this walked every symbol with one shared
  position, so a multi-symbol blotter fabricated cross-instrument trades — the
  bound exists to make that class of misuse unrepresentable.
- **`drawdown_segments(equity) -> Vec<DrawdownSegment>`** — one peak→trough→recovery
  per drop; `{ peak_bar, trough_bar, depth_ratio, duration_bars, underwater_bars }`.

**Catalogue** (each a `pub fn`): return moments, risk-adjusted (`sharpe`, `sortino`,
`calmar`, `omega`, `ulcer_index`, …), Sharpe corrections (`probabilistic_sharpe` —
Bailey/LdP 2012; `deflated_sharpe` — 2014; `*_from_stats` variants take pre-aggregated
stats so `optimize` computes DSR per row without re-scanning; `expected_max_sharpe`
exposes the selection benchmark DSR tests against, which needs only `(n_trials,
Var[SR])` — no returns vector, no higher moments), drawdown, trade-level.

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
- **`optimize --smooth[=KERNEL]`** — neighbourhood-kernel ranking, the parameter-
  space counterpart to `-k`'s time-space penalty. `spec::optimize::smooth_keys` is
  the one neighbour walk, shared by the grid sweep and the per-fold walk-forward
  selection; it reads each subgrid's `combos` as a mixed-radix lattice (last axis
  fastest, `Subgrid::{axis_lens, strides, digits}`), smooths only numeric axes of
  length ≥ 2 (categorical *and* one-value axes partition, and neither counts
  toward `support`), renormalizes at edges and reports the realized `support`.
  Direction-agnostic: it averages the already-directed `ranking_value`, so `-k`
  composes for free. Runs between the row rejoin and the sort, while
  `rows[i] ↔ plan[i]` still holds. Adds `<best-by>_smoothed`/`_support` columns.
  **Distance is in value space, normalized per axis** (`axis_geometry`): the
  parameter gap over that axis' median gap, on a `linear` / `log` / `index`
  `AxisScale` chosen per axis (log when it makes the axis' gaps clearly more
  uniform and every value is positive) and overridable via `--smooth-scale` /
  `SmoothScales`. Because the kernels are separable the axes never need a common
  scale — only one internal to each, which the axis' own spacing supplies. A
  *regular* axis takes an exact integer-rank fast path (no float division at
  all), so an ascending `start..end:step` range or evenly spaced list reproduces
  the pre-0.65 index-space numbers bit for bit; only irregular lists move.
  Neighbourhoods are always summed in **ascending parameter order**, never
  declared order — that is what makes the result independent of how the list was
  typed, exactly rather than to the last ULP, and it is why a *descending*
  regular declaration no longer matches pre-0.65's last bit. `support`'s
  denominator stays kernel-only (`Π_j Σ_{d=−R..R} w(d)`), unclamped, so `1.0` is
  grid-independent and reachable and a denser-than-median pocket reads above it.
  `plateau_size` connects `±1` in *sorted* position, not within the bandwidth —
  the console prints it as a cell count.
- **`selection.deflated_sharpe` on `optimize`** — per-row DSR against the grid-wide
  null (`N` = trials, `Var[SR]` = sample variance of their annualized Sharpes). The
  trial population is the rows a `--best-by` could have returned, so a **ruined row
  is not in it** (`optimize::trial_sharpe`, `ranking_lookup`'s rule carried to the
  one selection number derived grid-wide) — though it still gets a DSR *cell*.
  Omitted if <2 candidate rows have defined Sharpe or trial variance is zero.

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
  comparisons (op carries a `Tolerance`), boolean logic. Needs `Op: Default`;
  comparisons get `with_epsilon` / `with_tolerance`. Feeds the *same* input to both sides, requires `Input: Clone`;
  use `lhs`/`rhs` naming.
  Arithmetic also covers `Pow` (`None` where the result is not a finite real) and
  the **pairwise** `Max`/`Min` — two sources compared on one bar, as against
  `Extreme`'s one source over a window. `.clamp(lo, hi)` is `Min` of `Max`, and
  the YAML `!clamp` builds exactly that nesting rather than a fourth type.
- **`Unary<S, Op>`** (pointwise, `UnaryOp`, zero-sized markers): `Abs`, `Sign`,
  `Sqrt`, `Tanh`, `Sigmoid`. Stateless — the answer depends on this sample alone,
  so warm-up and unstable period pass straight through, and the op returns
  `Option` so a domain it has no answer for (`√x` of a negative) reads `None`
  rather than a NaN. `Log`/`Exp` predate it and stay standalone; they carry a
  `base`, where every op here is zero-sized.
- **`Lookback<I, Op>`** (unary, `LookbackOp`, zero-sized markers): `Lag`, `Diff`,
  `Ratio`, `Roc`.
- **`Extreme<S, Op>`** (rolling, `ExtremeOp`): `RollingMax`/`RollingMin`.
- **`Cumulative<S, Op>`** (unbounded fold, `CumulativeOp`): `CumSum`, `CumMax`,
  `CumMin`. No window, so the state is one `Real`. The fold takes
  `acc: Option<Real>`, which is how an op with no identity element seeds from
  its first sample. Anchored, not unstable — where the total starts is part of
  its meaning, exactly as `Obv` is (and `Obv`/`Ad` are hard-wired `CumSum`s).
  `x / x.cum_max() - 1` is the drawdown of any series, which is what generalises
  the book-anchored `!drawdown`.

**Three markers wear more than one hat**, because the operation is one idea and
only the carrier differs: `AddOp` is binary `+` *and* the fold behind `CumSum`;
`MaxOp`/`MinOp` serve the pairwise, rolling and cumulative extremes alike.

**`IfElse<Cond, T, F>`** — three-source ternary. `Cond: Indicator<Output=bool>`
picks: `Some(true)` → `if_true`, `Some(false)` → `if_false`, `None` propagates. All
three advanced every bar (never short-circuited). `warm_up_bars()`/`stable_bars()`
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
  `Sma`/`StdDev`/`Bollinger`; `WindowCovariance` (paired window; one centred
  lane-reduced pass → `Moments { mean_x, mean_y, var_x, var_y, cov }`) backs the
  whole `PairStat<L, R, Op>` family — `Correlation`, `Covariance`, `Beta`, which
  differ only in which field of one `moments()` call they read — and `LinReg`,
  which feeds it the bar index as its `x` leg; `WindowExtreme<Op>` (monotonic-deque) backs
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
  **Deferred value, eager shape**: `Deserialize` (and `SpecTemplate::checked`, for the
  callers that preprocess a tree first) typed-parses a *probe* copy at load with every
  `!arg` held as an `undefined` hole, so a typo in a deferred body is a parse error like
  any other, for every consumer of the loader. A probe error naming a hole sentinel is
  *skipped*, not reported — `!value`'s hand-rolled `TryFrom` has no type demand to answer
  a hole with, and refusing to load `!value !arg CHILD_GROUP` would be a false verdict
  (`undefined::parse_probe`).
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
  `universe`. No `root:` field.
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
  percentage`). Dotted `--costs` setter is a *literal* address. See [docs/COSTS.md](COSTS.md).
- **`dyn_indicator.rs`** — facade re-exporting **`fugazi::runtime`** (`DynIndicator` +
  `DynValue` (`Real | Bool | Atom | Candle | Str | Time | Snapshot<String>`) + `DynType`
  + `Adapter` blanket + `AsReal`/`AsBool`/`AsCandle`/`AsAtom`/`AsStr` + `chain`/
  `unstable_wrap`). **New YAML-visible indicators plug in via `dyn_indicator::wrap(...)`.**
- **`csv_source.rs`** — the CSV reader behind `fugazi get file:PATH`. The provider
  is named for its transport (`file:`), not its format; a second format would be a
  sibling reader here, dispatched off the path's extension.
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

## Python bindings (`python/src/`)

**Type-erased mirror** of the Rust library (pyo3 cdylib, `fugazi-python` → `fugazi`).
See [docs/PYTHON.md](PYTHON.md) for the user-facing API. Python can't carry source
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

**When a Rust API is added/extended/renamed, mirror it in `python/src/` in the
same PR.** Two tests catch the common cases: `python/tests/test_parity.py` (every spec
tag is bound, or listed with a reason) and
`cli::list::tests::the_output_renders_every_category_and_tag` (every tag appears in
`fugazi list indicators`). Both derive the expected set from **`spec::typecheck::known_node_tags`
/ `known_selection_tags`** (serde's own variant list). Everything below is still on you:

**The grammar descriptor (`spec::grammar`, `fugazi.spec_grammar()`).** `#[derive(SpecGrammar)]`
(the `fugazi-derive` crate) reflects `NodeSpec` / `SelectionRuleSpec` / `UniverseSpec` into one
JSON-serializable record per tag — names, `forms` (each with a shape, fields with types /
required-ness / tagged defaults, or a payload), prose, plus a per-variant `kind` / `output` / `since`. It is the single authority for the
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
derived from the canonical form's `shape`/`fields`/`payload`, prose from the stamped records, grouping + order from
`CATEGORIES`) rather than a hand-maintained parallel table — the drift-prone duplication the
descriptor exists to kill. Prose is stripped of rustdoc link markup (`` [`Type`] ``,
`[text](url)`) in the derive's `doc_string` so it reads as clean presentation text for every
consumer. `category` is a new *field*, so it **did** bump `SCHEMA_VERSION` to 3.

**A tag is a set of spellings, not one — `forms` (v5, 0.67).** `shape` / `fields` / `payload` /
`payload_output` sit on a `GrammarForm`, and a record carries a list of them, canonical first.
The single `shape` the descriptor used to report was silently wrong for eight tags: `!param` /
`!arg` (`NAME` or `{ key, default }` — only the second can carry a default), `!import` (a path
or `{ path, params }`), `!equal_weight` (bare in a portfolio `weights:`, `<N>` as sizing —
different meanings), and the four unary wrappers `!changed` / `!became_true` / `!became_false` /
`!unstable`, which take their inner bare *or* under a `source:` key. The last four are in the
**reflected** group, and that is the point: their alternate spelling lives in
`NodeSpec::parse_unchecked`'s normalisation pass, not in the variant, so the derive cannot see
it. It is therefore *declared* — `#[grammar(alt = "unary_source")]`, joining `kind`/`output`/
`since` as the things serde doesn't know — and the derive synthesises whichever of the two the
variant isn't. **The claim is settled against the parser, both ways**: `every_declared_form_parses`
runs a probe in every declared form, and `no_unary_wrapper_hides_an_undeclared_mirror` probes the
mirror spelling of *undeclared* unary-shaped tags and fails if one parses. So a form can neither
be claimed without existing nor exist without being claimed. `forms[0]` is canonical (what a
generator emits, what `GrammarTag::canonical()` returns); a consumer that *accepts* documents
must iterate all of them. `spec_json_schema()` emits a multi-form tag as an `anyOf` over its
forms, which is what makes `{"unstable": "close"}` and `{"changed": {"source": …}}` validate —
both had always parsed. Building this found and fixed one real asymmetry: `!unstable`'s
bare-inner branch matched only a tagged or bare-word payload, so the **JSON bridge** form
`{"unstable": {"sma": …}}` was rejected while `{"changed": {"sma": …}}` was accepted; all four
now share `expr::UNARY_WRAPPERS`.

**`scope` says where a form is legal, because `group` doesn't.** `document` is a *provenance*
label — resolved by a `Value` pass before the typed parse — and reading it as a position claim
is wrong for half of that group. `!param` and `!import` genuinely go anywhere a value goes (an
expression slot, `period:`, `root:`, a list element). `!arg` is `scope: "template"`: it is
substituted only inside a deferred `SpecTemplate` body (a basket's `score:`/`sizing:`, a
multi-asset side's `enter:`, a portfolio's `weights:`), and one written elsewhere is a hard
parse error — `check` included, since no pass touches it. `!undefined` is `scope: "internal"`.
On the `weighting` side, `!fixed` and bare `!equal_weight` are `scope: "portfolio_weights"`,
while `!equal_weight <N>` is unscoped. Absent `scope` means unrestricted, which is every
expression tag.

**`host_affecting` (v6) is a fact, not a policy.** `true` on exactly one tag — `import`,
because resolving it is a filesystem read; `false` on every other tag, reflected or
hand-authored. Fugazi itself has no deployment-policy layer to attach to this — it exists so an
embedder that hosts user-authored documents (see [`spec::imports`](../src/spec/imports.rs) for
why that's a real threat, not a hypothetical one) can derive its own allow/deny table and editor
completions from the descriptor instead of hand-maintaining a table pinned to fugazi's tag list.
Present on every record (not omitted when `false`), so this is a shape change — the field-count
change, not a new group or legend value, is what earns the bump.

**A tagged `default` (v7) — a default that is a node, not a literal.** A field's `default` was
a bare JSON value doing double duty: a literal for the 34 scalar keys that have one, `null`
for everything else. But "everything else" was two different answers, and a consumer could not
tell them apart — 69 slots default to an *expression*, and the only trace of it was English:
37 `source:` keys saying "defaults to the bar's `!close` when omitted", 23 "the current bar",
six `!high`/`!low`, three `!everything`. That prose is the same failure mode `node_output`
fixed one field over — a downstream table derived by regexing doc strings, which nothing pins
and which silently stops matching the day a `///` is reworded.

`default` is now a tagged `GrammarDefault`: `{"literal": 12}`, `{"expr": "!close"}`, or
`null` for no default at all. `!ema`'s `source` is `!close`, `!atr`'s `!current`, a selection
rule's `of` `!everything`. The fragment parses in the slot it describes, so an editor can both
render it (`!macd_line · source=!close, fast=12`) and insert it, where before it could only
write `source?`. Tagged rather than "bare value means literal": `{literal} | {expr}` is a
discriminable union in every consumer language, where `JsonValue | {expr}` is only
distinguishable by a runtime key probe that stops being sound the day a field defaults to an
object. That is the original bug in miniature, and worth ten characters a record to avoid.
Retagging also surfaced one: `!pick`'s `symbol` / `freq` are `Option<String>` with a serde
default, so the derive was reporting `Some(Value::Null)` for them — a "default" that
serialised to exactly the `null` a real absence did.

It is **reflected off the default's own value**, not off prose: `grammar::default_expr_of`
reads the `Debug` of what the `#[serde(default = "…")]` fn returns, the same trick
`typecheck::tag_name` uses to name a variant without a parallel table — change `default_source`
and the fragment changes with it. And the equivalence is *settled against the parser*:
`a_default_expr_is_equivalent_to_omitting_the_field` parses each tag twice, with the key
omitted and with the key set to the fragment, and requires the same tree.

**The fragment is a root floor.** Every one is a bare leaf, and a bare leaf reads the blessed
series its enclosing document confers — so a fragment never nests (`!close`, not
`!close { source: … }`) and is always one token. That is also what lets the *other* 33 slots
stay honest rather than being flattened into the same arm: a candle leaf's own `source:`
defaults to "the strategy's own series", which no tag spells, so it reports `null` — which now
unambiguously means **no default**. Nothing is lost by stopping a rung above it: the floor
already says it. The derive's rule is structural, not editorial: a non-`Option` field with a
serde default gets its value spelled, as a literal or a fragment by type; an `Option` field's
default is Rust `None`, which names neither.

`spec_json_schema()` carries the same fact in its own encoding — a literal goes in as JSON
Schema's `default` verbatim, a fragment is normalised through the loader's YAML → JSON-bridge
pass first (`!close` → `"close"`, the canonical bare spelling rather than the `{"close": null}`
a tagged empty scalar parses to), which is the form that schema validates. The Python suite
validates every advertised default against the slot it sits on, so the two projections cannot
disagree. Doing that found a second asymmetry of the v5 kind: the `map` arm of `form_schema`
admitted the bare string and a real object body but **not** an explicit null, so the schema
rejected `{"close": null}` — which is exactly what a YAML `!close` normalises to and what the
parser has always taken. The `unit` arm always had that branch; the `map` arm now does too,
pinned from both sides (`all_optional_map_tags_parse_from_a_null_body` for the parser,
`test_all_optional_map_tags_accept_an_explicit_null_body` for the schema).

Records carry one datum that is **not** reflected off serde: **`node_output`** (v4, 0.61), on
every field whose `type` is `node` / `node_list` / `match_cases`, plus **`payload_output`** for
a newtype/seq tag's positional payload. `type: "node"` says a slot holds a nested expression;
`node_output` says *which* expressions belong there, spelled in the same vocabulary as
`output` so a consumer matches by string equality — `!and`'s `lhs` is `["bool"]`, `!sma`'s
`source` `["scalar"]`, `!changed`'s payload `["bool", "scalar"]`. Three states, mirroring
`typecheck::slot_demand`: **absent** = not a free-expression slot (a scalar field, or a *book
selector* like `!drawdown`'s `source`, which takes only `!strategy_book` / `!portfolio_book`);
`[]` = a passthrough that demands nothing (`!unstable`'s `source`, `!resample`'s `inner`);
otherwise the admitted set. Both are omitted when absent. They are stamped on **every** form,
not just the canonical one — an alternate spelling holds the same slots under different syntax,
and a consumer completing inside `!changed { source: ` needs the demand there too.

The demand lives in **`spec::typecheck`**, whose `children()` table is keyed on a *node*, not a
tag — so `spec_grammar` can't read it directly. Rather than hand-write a second table (which
would drift, and lose the exhaustive-match guard that makes the first trustworthy),
`typecheck::slot_demand(tag, slot)` synthesises one **prototype** node per tag from that tag's
own grammar record (its canonical form — an alternate holds the same slots, so probing it would
report each demand twice) — filling every expression slot with a `!get`, whose `output_type()` is
`None` and therefore satisfies any demand — and runs `children()` on it. One authority, no
duplication. `demand_table_covers_every_node_slot` pins the coverage, since a tag whose
prototype failed to build would silently report *no* demands. Building it caught two slots
`build` constrained via `into_bool()` but `children()` never listed — `!bars_since`'s `source`
and `!if_else`'s `cond` — which now fail at parse rather than mid-build.

`spec_json_schema()` (`fugazi.spec_json_schema()`) is a second projection of the same
descriptor: a JSON Schema (draft 2020-12) for the expression grammar's **JSON bridge form**
(single-key `{tag: body}` objects + bare-literal shorthands + authored load-time
placeholders), for structural validation by consumers without the Rust build path.
`spec_document_json_schema()` extends it to the whole document — the five strategy shapes as
a `oneOf`, each `$ref`-ing the same node/selection grammar for every expression slot. Both are
**complementary to `fugazi check`**, not a replacement — `check` (the typed parse) remains the
authority, validating the type discipline and build-time semantics the schema can't express.

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
shapes share one run seam** (`over_any_wallet!` / `over_prepared_wallet!` in
`python/src/strategy.rs`): `.run(wallet, …)` accepts a `PaperWallet`, an `OkxWallet`
or a `CoinbaseWallet`. The seam handles **external positions automatically** via the
core `SleeveWallet`. `test_specs.py::test_portfolio_builder_matches_the_equivalent_yaml_document`
pins the builder against the equivalent `portfolio:` document. The **spec** surface
(`load_spec(...).run` / `.run_resumable` / `.warm_up`) goes through the same seam and
so takes the same three wallets — that is what makes a *portfolio* spec runnable
against a venue. `run_spec` / `run_spec_resumable` are thin adapters over the library's
`drive_over` rather than a second implementation of the driver, so the Python and CLI
paths cannot drift.

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
top-level strategy shape needs a `detect_kind` arm (in `python/src/spec.rs`) and a
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
with `abi3-py311` (the buffer protocol needs it — see `docs/PERFORMANCE.md`).
Test: `maturin develop` then `pytest python/tests/`.
