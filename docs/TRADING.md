# The trading path — from a tick to a closed trade

How one bar becomes an order, an order becomes a fill, and a fill becomes a
closed trade in the metrics. This is the *execution* story; for the indicator and
strategy layers see [ARCHITECTURE.md](ARCHITECTURE.md), for the cost models
[COSTS.md](COSTS.md), and for what the numbers at the end mean
[METRICS.md](METRICS.md).

Read it when you are debugging a fill that didn't happen, a price you didn't
expect, or a position two layers of bookkeeping disagree about.

---

## The one-paragraph version

A bar arrives. The wallet is priced with it, which settles whatever was queued on
the *previous* bar and triggers any resting protective leg the bar's range ran
through; those fills are routed to the strategy so its own position and book stay
in step. Only then does the strategy see the bar. If it is warmed up, it decides,
and its decisions become *submissions* — which queue rather than execute. The
bar's equity is recorded and the loop moves on. Nothing fills on the bar that
caused it.

---

## 1. The driver loop — `backtest::run`

`src/backtest.rs`. Every surface funnels through this one function: the CLI, the
spec layer, `Portfolio`, the Python bindings. It is generic over the wallet, so
the same loop drives a `PaperWallet` in a backtest and an `OkxWallet` against a
live venue.

Per bar, in this exact order:

1. **Price the wallet** — collect every tagged, priceable entry in the snapshot
   and hand the whole bar over in one call, `wallet.advance(&bars)`. This is
   where fills are born (§2). Each returned `Order` goes to
   `strategy.on_fill(&order)` and into the report's blotter.
2. **Drain rejections** — `wallet.take_rejections()` → `strategy.on_reject`.
   Routed *before* `update`, alongside the fills they occurred with.
3. **Drain out-of-band fills** — `wallet.poll_fills()`. A live venue fills on its
   own schedule and reports asynchronously, possibly for a symbol that didn't
   tick this bar. A `PaperWallet` keeps the empty default, so this is a no-op in
   a backtest and the equity curve is byte-identical whether or not it runs.
4. **`strategy.update(snap)`** — always, so warm-up progresses even while the
   strategy is unready to act.
5. **`strategy.trade(wallet)`** — *only* if `strategy.is_ready()`. Then drain
   rejections again, for submissions this bar refused synchronously (a live
   venue; `PaperWallet` accepts at submit time and fails at fill time instead).
6. **Record equity** — one `wallet.equity()` reading per bar, post
   mark-to-market.

**Why the whole bar goes over at once.** `advance` takes every `(symbol,
candle)` together rather than being called per symbol, because two things in the
wallet are shared across a bar and neither is expressible one symbol at a time:

- **The mark that values equity.** A `value_frac` fill resolves against equity,
  and equity marks every *other* position at its last fed price. Feeding symbols
  one at a time makes "last fed" mean *this* bar's close for the symbols already
  fed and the *previous* bar's close for the rest — so a fill trading at the
  open gets sized off a co-held asset's close, which is information from later
  in the same bar.
- **The single cash balance.** A rotation that sells one holding to fund another
  is only affordable once the sale has settled. Priced first, the buy is
  silently scaled down to whatever residual cash happened to be lying around
  (see the shrink in §3) — no rejection, just a small fill.

Both made the run depend on the order the snapshot's rows happened to be in,
which is an artefact of how the snapshot was assembled (`--series` order in the
CLI, dict insertion order in the Python bindings) and carries no meaning. The
contract is now explicit: **the booked fills must be identical under any
permutation of a bar's symbols**, and `tests/bar_phasing.rs` asserts it.
`Wallet::update` remains as the single-symbol special case, and `advance`'s
trait default is the per-symbol loop — correct for every live venue, where the
venue owns fills and `update` only marks a price.

**Why fills come before `update`.** A queued order settles at the *next* bar's
open, so by the time the strategy sees bar N it must already know about the fill
that bar N's open produced. Inverting this would have the strategy decide on bar
N while believing it still holds what it asked to sell.

> **Caveat — this ordering is load-bearing for resuming.** Because `on_fill`
> precedes `update`, the first bar of a resumed run delivers a fill *before* the
> strategy has seen a single snapshot. Shapes that build per-symbol state lazily
> (basket, multi) therefore restore **eagerly** — every symbol in the state blob
> is built inside `restore_state`, not on next sighting — or that fill lands on
> the shared `Book` with no `Position` built to receive it, and the two disagree
> for the rest of the run. `tests/driver_contract.rs` pins the ordering;
> `tests/resume.rs` pins the consequence.

`backtest::warm_up` is the same loop with step 5 gated off (`DriveMode::WarmUpOnly`
— one branch, not a second loop). It exists for a *pause gap*: bars that elapsed
while a live deployment was stopped must warm the indicators without booking
trades at prices nobody could have traded at. Steps 1–3 still run, so a resting
order left from before the pause still fills and still reaches the strategy.

## 2. Submission — decisions become intent, not execution

`src/strategies/mod.rs::trade_leg` is the shared decision body behind the
single-asset, basket and multi-asset shapes. Given this bar's signal readings it
does at most one of:

| Condition | Call |
| --- | --- |
| `enter_long` and not already long | `wallet.set(sym, Buy, value_frac(size))` |
| `enter_short` and not already short | `wallet.set(sym, Sell, value_frac(size))` |
| `close_long` while long (or the short twin) | `wallet.close(sym)` |
| the rebalance gate fired, position open | `wallet.set(...)` at the current target |

then re-rests the active side's protective legs (`set_stop` / `set_take_profit`)
every bar, which is what lets a trailing stop follow the position — the calls are
idempotent and latest-wins per symbol.

The five market movements — `set_position`, `set`, `close`, and the two
protective resters — all return an `Ack`, never a fill. On a `PaperWallet`:

- **`set_position` / `set` / `close` queue.** One pending move per symbol, latest
  wins, filled at the *next* bar's open. This is the single most important rule
  in the engine: **a signal computed from bar N's close cannot execute at bar N's
  close.** Anything else is lookahead, and it is the most common way a backtest
  flatters itself.
- **`set_stop` / `set_take_profit` rest.** One bracket per symbol, latest wins.
  The wallet triggers and prices them itself against each bar's range (§3).
  When **both legs sit inside one bar** the range says only that the price
  visited each level, not in which order — so the **stop wins**, on both a long
  and a short. That is the pessimistic reading of an ambiguity the data cannot
  settle, and the same choice on either side of the book. A bracket set while
  flat guards the entry from the bar the entry fills on, since market fills
  resolve before protective legs; a fill that **reverses through zero** drops
  the bracket with the position it left, rather than reinterpreting a long's
  stop as a short's.
- **`set_limit` rests** as an *entry* instrument: it drives the position toward a
  target once the market trades through the limit price, at that price or better.

`set_position` pre-flights against the last close, so an infeasible submission
errors *synchronously* — mirroring a live venue's rejection — rather than queuing
something that will be silently dropped at fill time.

> **Caveat — strategies are not rule engines.** `trade_leg` is a fixed body, not
> a `(signal, action)` table. Adding one — including wiring `set_limit` into the
> signal slots — is a design change, not an extension.

## 3. Fill — `PaperWallet::advance`, in phases

`src/wallet/paper.rs`. Given every `(symbol, candle)` the bar carries, in six
phases. Each phase completes across *all* symbols before the next begins — that
is what makes the result independent of the order they arrive in.

1. **Mark every symbol**, before anything is priced, so a queued fill validates
   against *this* bar's range (its `open` is trivially inside it) and every
   sizing sees the same book.
2. **Resolve the queued market orders** against **one** equity, built from this
   bar's *opens*.
   - `Pending::Target(units)` is already an absolute target.
   - `Pending::Sized(side, size)` resolves the `Size` **at the fill price**, so
     an all-in stays exact. Equity marks every symbol in this bar at its own
     `open` — the price fills actually trade at — and any other position the
     wallet holds at the last close it was fed. Reading a close for a symbol
     that *is* in this bar would size the fill off information from later in the
     same bar.
   - The shrink is deliberately **not** applied here: it reads live cash, so it
     has to happen at the moment each fill is booked, once this bar's credits
     have landed.
3. **Book the market fills that credit cash** — sales and position reductions.
4. **Book the market fills that consume cash.** A *fractional* sizing
   (`value_frac` / `funds_frac`) is shrunk here to fit available cash after
   spread, slippage and commission. Without this an all-in `value_frac(1.0)`
   under any positive cost model would size the notional to the entire equity,
   fail the affordability check, and silently drop the fill. An explicit
   `Size::Units(n)` or `position_frac(f)` carries a specific intent and is
   **left alone** — an infeasible request there is a caller error, not a sizing
   target to be quietly truncated.
5. **Test the resting protective brackets** (§4) — every symbol's trigger
   evaluated before any is booked, then crediting exits (a long stopping out)
   settled before debiting ones (a short being covered).
6. **Test the resting limits** — last, deliberately. A protective leg guards a
   position that already exists; letting a fresh entry fill ahead of the exit it
   was meant to trigger would leave the strategy holding something it had asked
   to be out of.

**Ties break by `OrderId`, never by symbol or by position in the bar.** Two buys
that both want cash and neither of which funds the other have no
funding-derived ordering, so the tie falls to submission order — what the
strategy expressed, and what a venue would honour first-come-first-served.

> **A stop does not fund a market entry on the same bar**, and that is
> chronology rather than an oversight: the market order fills at the `open`, the
> stop triggers only when the bar later trades through its level. Phases 3–4
> therefore precede phase 5. Pinned by a test, because it is easy to "fix" into
> lookahead.

Every one of those paths goes through the same private engine, `fill_at`.

### `fill_at` — the one place a fill is made

1. Compute `delta = target - current`; a move smaller than `POSITION_EPSILON` is
   a no-op, not a zero-unit order.
2. Reject if the pre-cost price is outside the bar's range. **Cost adjustments
   may push the *final* price outside it, and that is fine** — a real fill can
   execute above the tape. It is the *theoretical* price that has to be one the
   bar traded at.
3. **The cost pipeline: spread → slippage → commission.** Direction comes from
   `delta`'s sign (buys pay the ask, sells receive the bid) and the `OrderKind`
   threads through, so a stop can slip further than a plain market order and a
   *limit* fill crosses no spread and suffers no slippage at all — it provides
   liquidity rather than taking it, and anything else would let the pipeline fill
   it worse than the price a limit order exists to guarantee. Per-symbol cost
   overrides win over the wallet's default bundle.
4. **Solvency, in two rules** — preceded by a finiteness guard, because a `NaN`
   quantity passes both of them (and rule 2 above) by reading false against
   every comparison, and would book a `NaN` position: `InvalidQuantity`.
   *Cash*: on an unlevered wallet a net buy plus its
   commission cannot drive cash below zero, otherwise `InsufficientFunds`.
   *Leverage*: no fill may leave gross notional (`Σ |position| × price`) above
   `max_gross × equity`, otherwise `ExceedsMaxGross`.

   For a **long-only** book at the default `max_gross = 1.0` these are the same
   inequality — `gross <= equity` *is* `funds >= 0` when nothing is short — so
   the second changes nothing there. What it adds is the same bound on the
   **short** side, where a sale credits cash and the first can never fire.
   Without it `sizing: 3.0` meant 1x long and 3x short under one spec value.
   Raising `max_gross` above `1.0` lifts the cash rule too: the account borrows,
   which is what leverage is.

   **The cap bounds the result; the deployment multiple is what moves it.**
   `PaperWallet::with_leverage` (CLI `--leverage`) multiplies a *fractional*
   sizing on the way out of `Size::resolve_at_leverage`, and defaults
   `max_gross` to itself — so an unedited `sizing: 1.0` document trades 3x on a
   `with_leverage(3.0)` account without the request being fitted straight back
   down. It scales neither `Size::Units` nor `Size::PositionFraction`: a named
   unit count is a specific intent, the same reason it is never fitted. Every
   impl that resolves a `Size` itself reads it through `Wallet::deployment` —
   the trait default, `SleeveWallet`, and `portfolio::LedgerWallet`, which
   caches the account's answer beside `account_leverage` because a child has no
   handle on the account to ask.

   **A ceiling still never re-denominates the request.** A
   `Size::ValueFraction` is a multiple of *equity* on every account —
   `value_frac(1.0)` is 1x equity at `max_gross = 1` and 1x equity at
   `max_gross = 10` — so raising the cap cannot enlarge a request that already
   fits, only stop truncating one that does not. Sizing is what the rule wants;
   leverage is what the account will carry. Re-basing the fraction on
   `max_gross × equity` so that `1.0` always meant "fully deployed" is inert at
   the default and therefore looks free, but it would multiply every
   *risk-denominated* sizing rule by the account's leverage: `!vol_target` means
   "hold this much realized vol", and scaling that by `max_gross` does not scale
   a risk target, it removes one. Measured on a levered wallet, the re-base took
   a 20%-target document to 35.5% realized vol and a 55% max drawdown (from
   15.8% and 25%) — while *still* clipping 38 of 139 fills, because a multiplier
   of 3.8 overshoots a 3x cap either way.

   **Only a fill that raises the position's magnitude is bound.** An exit, a
   protective leg and `flatten` are exempt outright, so an account carried over
   its limit by a mark can always trade its way back.
5. Move cash and the position, push to the blotter, return the `Order` — carrying
   `requested_units` beside `units`, so a magnitude that was fitted to either
   rule is visible rather than reported as if it were what was asked for.
   `Order::is_materially_fitted` applies the crate-wide `MATERIALLY_FITTED`
   threshold (99%, above the sliver a commission always costs an all-in), and
   `RunReport::materially_fitted` reduces a whole run to `(count, worst ratio)`.
   That sits beside `rejections` and `carry_coverage` for the same reason both
   do: a fitted fill is **not** a refusal — the trade happened — so it reaches
   `rejections` never, and a `sizing:` the account could not carry would
   otherwise be indistinguishable from a signal that sized smaller.

   **A fit that collapses to *no trade* is booked as a rejection instead.** There
   is no order to hang `requested_units` on, so without that the leg simply
   vanished — no fill, no refusal, nothing in the blotter — and a strategy the
   account could not afford looked like one that chose not to trade. The same
   situation reached through an explicit `Size::Units` is a loud
   `InsufficientFunds` at submission, and a fractional sizing is what the spec
   layer builds for *every* `sizing:`, so the silent spelling was the default
   one. An unlevered basket whose earlier legs have used the whole gross budget
   reaches it on the last leg routinely.
6. **A fill that flattens or flips the sign voids the resting bracket**, so a
   bare market exit or reversal drops a now-stale stop without an explicit
   cancel.

### Carry — what the gap between fills costs

Before any of this bar's orders resolve, `advance` charges the **cost of
holding**: each symbol's `CarryModel` on the position carried *into* the bar
(funding, a borrow fee), plus account-level interest on a negative cash balance
(`--margin-rate`). Marked at the bar's `open`, on what was held through the
interval that just ended — so a position opened this bar first pays next bar, and
one closed this bar has already paid for the time it was held.

It runs *before* the fills so this bar's sizing sees the account the charge left
behind, rather than one still holding cash it has already spent. Only symbols
that ticked this bar are charged: a position with no mark this bar has no price
to value the charge at, and carrying the last close forward would bill against a
price the bar never saw.

At `--max-gross 1` with no carry configured — the default — this phase does
nothing and the bar is byte-identical to one without it.

### The margin call

Last in the bar, after every fill: if `--maintenance-margin` is set and equity
has fallen below `ratio × gross notional`, the whole book is force-closed as
`OrderKind::Liquidation`. Off unless asked for, because the ratio is a venue
assumption fugazi will not guess.

The **trigger** marks each position where the bar hurt it most — the `low` for a
long, the `high` for a short — because a wick is what liquidates a levered
account, and a close-only test would report a strategy that survived an event it
did not. The **fill** books at the `close`, which is a simplification: the price
at which the breach occurred is not recoverable from a single bar once more than
one symbol is involved. Nothing stops the strategy re-entering next bar; the
`liquidation` rows in the blotter are what say it happened.

## 4. Protective legs — stops and take-profits

`protective_trigger` + `execute_protective`. A long's stop triggers when
`low <= trigger`; its
take-profit when `high >= trigger`; a short is the mirror. The **fill price**
is the level, or the open when the bar gapped past it —
`min(level, open)` for a downside exit, `max(level, open)` for an upside one — so
the fill always stays inside the bar's range and a gap is priced honestly rather
than at a level the market skipped.

Legs are **reduce-only**: the size resolves at the fill price, clamps to the
position's magnitude, and steps *toward* zero. `position_frac(1.0)` — what every
whole-position exit passes — resolves to `|pos|` and flattens.

> **Caveat — a triggered leg that cannot be booked is reported, not swallowed.**
> It is the worst silent failure available here: the strategy believes it is
> protected, and the bracket stays resting (it clears only on success) so it
> retries next bar. Without a rejection nobody is ever told the exit did not
> happen. Check `RunReport.rejections` before trusting a run's metrics — a
> non-empty list means the equity curve reflects trades that did not go the way
> the strategy intended.

> **Caveat — intrabar order is unknowable.** Within one bar the engine cannot
> know whether the low or the high came first, so a bar that spans both a stop
> and a take-profit resolves by the fixed precedence above rather than by what
> actually happened. Bars coarse enough for both legs to be in range are bars too
> coarse to trust for protective-exit fidelity.

## 5. Bookkeeping — three views of the same position, on purpose

A fill updates as many as three records, and they answer different questions:

| Record | Question it answers | Updated by |
| --- | --- | --- |
| `Wallet` positions + funds | what the **account** holds | `fill_at` |
| `Position` | what **this leg** is doing — entry, peak, trough since entry | `Strategy::on_fill` |
| `Book` | what the **strategy** has done — cash, per-leg units, equity, trade P&L | `Strategy::on_fill` |

`Position` exists because protective levels are written against it
(`!entry`, `!peak`), and it publishes the extreme over **completed** bars,
folding the in-progress bar in afterwards. That is what keeps a trailing stop
fixed at the bar's open and reacting on the bar *after* a new extreme — never
intra-bar, which would be lookahead.

`Book` aggregates across legs: a "trade" in its sense is one open-to-flat cycle
across the *whole* strategy, so a pair or basket banks realized P&L when every
leg is flat, not per symbol. Its equity is summed in a canonical order rather
than `HashMap` order — two books holding the same legs would otherwise land a ULP
apart, and equity feeds `!drawdown` and the book-anchored sizing recipes, where a
ULP either side of a threshold is a different trade.

> **Caveat — the three can legitimately differ.** The wallet holds everything the
> account holds, including positions the strategy never opened. Python's run seam
> makes that explicit: whatever the wallet holds at run start is snapshotted as an
> external baseline and left untouched, with the strategy sizing against its own
> capital through a `SleeveWallet`. A flat wallet collapses to the fast path.

## 6. Portfolios — N children, one account

`src/portfolio/`. A portfolio is an ordinary `Strategy` that trades the wallet it
is handed; there is no composite wallet. Each child trades a `LedgerWallet` view
that **records intent** rather than executing, and once all children have decided:

1. **Net.** Per symbol, sum the children's deltas. Only the imbalance reaches the
   market. The offsetting part **crosses internally**: both ledgers move as if
   they traded, at the bar's open, carrying no commission — it never touched the
   market.
2. **Submit** one order for the imbalance on the real account.
3. **Rest** the most urgent protective leg (the account holds one bracket per
   symbol, so competing children's stops resolve by urgency).
4. **Attribute** the resulting fill back across the ledgers that caused it,
   pro rata, splitting each child's share into its crossed part (free, at the
   open) and its market part (at the fill price, carrying a pro-rata slice of the
   commission).

The identity everything rests on: **for every symbol, Σ ledger positions == the
account's position, and Σ ledger cash == the account's cash.** Ledgers move only
on real fills, never on intent, which is what keeps it true;
`Portfolio::assert_books_balance` checks it directly.

> **Caveat — netting understates costs slightly.** A portfolio whose children
> frequently trade against each other books lower costs than it would live. That
> is the documented price of netting rather than grossing up.

> **Caveat — the account must be the portfolio's alone.** The ledgers only
> balance against it if nothing else writes to it.

## 7. Closing out at the end

`backtest::apply_closeout` applies a `backtest::Closeout` once the last
bar is driven — `Carry` (leave it, the default), `Flatten` (the CLI's
`--flatten`, via `Wallet::flatten`), or `Hold(map)` (drive named symbols to
signed unit targets, via `Wallet::settle_position`, `0.0` being a close).

`Closeout::Rebalance { hold }` is the fourth arm and the one that does **not** come
through here. It forces the document's own rebalance gate on the final bar — "re-size
to what your `sizing:` now wants" — which is an ordinary `Wallet::set`, so it queues
and fills at the next chunk's `open` like every other rebalance. Settling it at the
last bar's close would manufacture a fill at a price the market never offered, and a
rebalance has no claim to the exemption the three terminal arms take below: the run
continues, so there *is* a next bar. Its `hold` map is the only part `apply_closeout`
sees, read exactly as `Hold`'s. See ARCHITECTURE *Rebalance on demand*.

`close` alone cannot finish a run: it *queues*, and a queued move settles at the
next bar's open — of which, at the end, there is none. So `PaperWallet`
overrides both trait defaults with a synchronous form that goes straight to
`fill_at`. Costs, commission and the blotter all apply exactly as they would to
a strategy-issued exit, and each leg mints a real `OrderId` so trade
reconstruction pairs it like any other close. `flatten` *is* `settle_position`
at `0.0` for every open symbol — one body, so the two cannot price a close
differently.

A settle that only shrinks a position is classified **reducing**, which exempts
it from the leverage cap exactly as an exit is exempt everywhere else in this
document (§5): a cap that could refuse a way out is a cap that traps a position.
One that grows a position, flips its side, or opens from flat is an ordinary
sized fill and meets the account's solvency rules in full, so an unaffordable
target is refused rather than booked. So is a symbol the wallet has never been
priced for — refused into the rejection log rather than skipped, because a
close somebody asked for and did not get has to be visible.

The final equity point is **overwritten, not appended**: each leg closes at the
same mark that point was computed from, so only the realized cost drag changes,
and `equity_curve.len() == snapshots.len()` is an invariant every downstream
consumer relies on. The zero-cost gross twin used for cost attribution takes the
same closeout as the priced run, or it would have no counterpart fills to
pair against.

## 8. Trades — how fills become the numbers

`metrics::reconstruct_trades` folds the blotter into closed trades, **one signed
position per symbol**: a fill with no open leg in that symbol opens one; a
same-side fill scales in with a volume-weighted entry; an opposite-side fill in
the *same* symbol reduces, closes, or reverses, banking P&L on the closed
portion. A fill never touches another instrument's leg. Everything in `metrics.trades.*` derives from that fold, which is
why a run with open positions at the end under-reports trade count and win rate
unless you `--flatten`.

> **Caveat — metrics assume a closed system.** Deposits and withdrawals mid-run
> are read as returns. See [METRICS.md](METRICS.md).

---

## Where each step lives

| Step | Code |
| --- | --- |
| The per-bar loop, and its warm-up-only twin | `src/backtest.rs` (`run`, `warm_up`, `drive`) |
| Decision → submission | `src/strategies/mod.rs::trade_leg`, each shape's `trade` |
| Queue / rest / trigger / fill | `src/wallet/paper.rs` (`update`, `fill_at`, `protective_trigger`/`execute_protective`, `limit_trigger`/`execute_limit`) |
| The `Wallet` contract every venue implements | `src/wallet/mod.rs` |
| Cost pipeline | `src/costs/`, and see [COSTS.md](COSTS.md) |
| Per-leg and per-strategy bookkeeping | `src/indicators/position.rs`, `src/indicators/book.rs` |
| Netting, crossing, attribution | `src/portfolio/netting.rs`, `src/portfolio/ledger.rs` |
| Live venues | `src/live/okx.rs`, `src/live/coinbase.rs`, `src/live/kraken.rs`, and the shared flow in `src/live/venue/` |
| Fills → trades → metrics | `src/metrics.rs`, and see [METRICS.md](METRICS.md) |
| The ordering contract, asserted | `tests/driver_contract.rs` |
