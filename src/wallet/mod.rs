//! The [`Wallet`] trait a [`Strategy`](crate::Strategy) trades into, the pure
//! in-memory [`PaperWallet`] impl the crate ships, and the vocabulary in
//! between: [`Side`], [`Size`], [`Order`], the unit-tagged [`Reference`] /
//! [`Units`] amounts, and [`WalletError`].
//!
//! The module is split by audience: `types` is the vocabulary every impl
//! speaks, `paper` is the simulated broker, `sleeve` the position-hiding
//! decorator, and the [`Wallet`] trait itself lives here. Everything is
//! re-exported, so `fugazi::wallet::X` and `fugazi::X` paths are unchanged.

mod paper;
mod sleeve;
mod types;

pub use paper::{DEFAULT_RETENTION, PaperWallet};
pub use sleeve::{SleeveWallet, external_baseline, own_equity};
pub use types::{
    Ack, CASH_EPSILON, Order, OrderId, OrderKind, POSITION_EPSILON, PRICE_EPSILON, Reference,
    Rejection, Side, Size, Units, WalletError,
};
// `pub(crate)` in `types`, so it can only be re-exported crate-wide.
pub(crate) use types::cash_tolerance;

use crate::costs::TradingCosts;
use crate::types::{Candle, Real};

/// The portfolio interface a [`Strategy`](crate::Strategy) trades into: query
/// funds, positions and prices, feed prices in, submit market orders, and rest
/// protective orders.
///
/// `Wallet` is a trait so it is the single **seam** between pure fugazi and a
/// downstream execution system. fugazi ships only the pure, in-memory
/// [`PaperWallet`] (for backtests and dry runs); a downstream crate that imports
/// fugazi can implement `Wallet` with a type whose
/// [`set_position`](Wallet::set_position) publishes a message onto an event bus /
/// routes to a broker instead of booking in memory. All market-specific,
/// side-effecting code stays out of fugazi, behind this interface.
///
/// The wallet carries no view of the market on its own: it must be fed each
/// symbol's worth every tick through [`update`](Wallet::update) (fugazi is
/// agnostic to where those prices come from). With prices in hand it can value
/// equity and size relative orders.
///
/// **Submitting is not filling.** Every order-submitting method
/// ([`set_position`](Wallet::set_position), [`set`](Wallet::set),
/// [`close`](Wallet::close), and the resting [`set_stop`](Wallet::set_stop) /
/// [`set_take_profit`](Wallet::set_take_profit)) returns an [`Ack`]
/// synchronously, *not* a fill: [`Ack::Filled`] if a fill happened on the spot,
/// otherwise [`Ack::Working`] with the [`OrderId`] whose fill arrives later.
/// Fills are delivered as [`Order`]s out of [`update`](Wallet::update) — the
/// wallet's fill stream — so a live fill arriving between bars and a paper fill at
/// the next bar's `open` reach the strategy the same way (a driver hands each to
/// [`Strategy::on_fill`](crate::Strategy::on_fill)). The [`PaperWallet`] queues
/// market orders and fills them at the next bar's `open`, so a backtest never
/// fills on the bar whose `close` produced the signal; a live impl fills on the
/// venue's schedule.
///
/// Protective exits are **resting orders the wallet owns**: a strategy rests a
/// stop / take-profit *level* with [`set_stop`](Wallet::set_stop) /
/// [`set_take_profit`](Wallet::set_take_profit) (idempotent, latest-wins per
/// symbol — re-submit to trail), and the wallet triggers and prices them itself,
/// filling when a bar trades through the level (or at the `open` on a gap). This
/// keeps the strategy free of fill-pricing and a live impl free to relay the
/// resting order to a broker.
pub trait Wallet<Sym> {
    /// The available cash balance, in reference currency.
    fn funds(&self) -> Reference;

    /// The current signed position in `symbol`.
    fn position(&self, symbol: &Sym) -> Units<Sym>;

    /// The last price fed for `symbol`, or `None` if it has never been fed.
    fn price(&self, symbol: &Sym) -> Option<Reference>;

    /// Total equity: funds plus every position marked to its fed price.
    fn equity(&self) -> Reference;

    /// Feed `symbol`'s current bar and return the [`Order`]s that filled on it —
    /// the wallet's fill stream. Call this — for every symbol to be traded or
    /// held — each tick, before trading or reading [`equity`](Wallet::equity).
    /// The bar's `close` marks the position to market; its `[low, high]` range
    /// bounds the prices a fill can occur at this tick.
    ///
    /// This is where deferred work resolves: an implementor that queues market
    /// orders (as [`PaperWallet`] does) fills them here at this bar's `open`, and
    /// any resting stop / take-profit this bar triggers fills here too. Each
    /// returned fill should be handed to
    /// [`Strategy::on_fill`](crate::Strategy::on_fill) by the driver.
    fn update(&mut self, symbol: Sym, candle: Candle) -> Vec<Order<Sym>>;

    /// Drive `target.symbol` to `target.amount` signed units as a **market
    /// order**, returning an [`Ack`]. [`PaperWallet`] queues the move and fills at
    /// the next bar's `open` ([`Ack::Working`]); a live impl routes it to the
    /// broker. This is the one required movement — [`set`](Wallet::set) and
    /// [`close`](Wallet::close) build on it.
    fn set_position(&mut self, target: Units<Sym>) -> Result<Ack<Sym>, WalletError>;

    /// Target `side · size` of `symbol` (absolute), as a **market order**. An
    /// opposite-side target reverses the position; the same side adjusts toward
    /// it. ([`close`](Wallet::close) is this with a flat target.) The default
    /// resolves the [`Size`] against the last-fed `close` and forwards to
    /// [`set_position`](Wallet::set_position); [`PaperWallet`] overrides it to
    /// resolve the size at the fill `open` instead.
    fn set(&mut self, symbol: Sym, side: Side, size: Size) -> Result<Ack<Sym>, WalletError> {
        let price = self.price(&symbol).ok_or(WalletError::UnknownPrice)?.0;
        if price <= 0.0 {
            return Err(WalletError::InvalidPrice);
        }
        let position = self.position(&symbol).amount;
        let funds = self.funds().0;
        let equity = self.equity().0;
        let magnitude = size.resolve(price, position, funds, equity);
        self.set_position(Units {
            symbol,
            amount: side.sign() * magnitude,
        })
    }

    /// Flatten `symbol` as a **market order**.
    fn close(&mut self, symbol: Sym) -> Result<Ack<Sym>, WalletError> {
        self.set_position(Units {
            symbol,
            amount: 0.0,
        })
    }

    /// Close **every** open position immediately, returning the fills booked.
    ///
    /// The terminal twin of [`close`](Wallet::close), and the difference is the
    /// whole reason it exists: `close` *queues*, and a queued-fill wallet like
    /// [`PaperWallet`] settles at the next bar's `open`.
    /// At the end of a run there is no next bar, so `close` alone would leave
    /// the position open forever. This finalizes it against the last known
    /// price instead.
    ///
    /// Implementors must route through the same execution path as any other
    /// fill — costs, commission and blotter included — so a flattened run's
    /// numbers are comparable with the rest of it. The default body suits a
    /// venue that fills synchronously or reports asynchronously: cancel the
    /// resting legs, submit a close per position, then drain
    /// [`poll_fills`](Wallet::poll_fills). A wallet that cannot enumerate its
    /// positions (the [`positions`](Wallet::positions) default) flattens
    /// nothing.
    fn flatten(&mut self) -> Vec<Order<Sym>> {
        for units in self.positions() {
            if units.amount.abs() <= POSITION_EPSILON {
                continue;
            }
            let _ = self.cancel_protective(&units.symbol);
            let _ = self.cancel_limit(&units.symbol);
            let _ = self.close(units.symbol);
        }
        self.poll_fills()
    }

    /// Rest a **stop-loss** on `symbol` at `trigger`: an adverse level the wallet
    /// fills when a bar trades through it (a long fills when the bar trades down to
    /// `trigger`, a short when it trades up). The side is read from the current
    /// position. Idempotent and latest-wins per symbol — re-submit each bar to
    /// trail. Returns the [`OrderId`] of the resting order in an [`Ack::Working`].
    fn set_stop(
        &mut self,
        symbol: Sym,
        trigger: Reference,
        size: Size,
    ) -> Result<Ack<Sym>, WalletError>;

    /// Rest a **take-profit** on `symbol` at `trigger` — the favourable twin of
    /// [`set_stop`](Wallet::set_stop). Idempotent and latest-wins per symbol.
    fn set_take_profit(
        &mut self,
        symbol: Sym,
        trigger: Reference,
        size: Size,
    ) -> Result<Ack<Sym>, WalletError>;

    /// Cancel both resting protective legs (stop and take-profit) on `symbol`.
    fn cancel_protective(&mut self, symbol: &Sym) -> Result<(), WalletError>;

    /// Rest a **limit order** on `symbol`: drive the position to `side · size`
    /// once the market trades through `limit`, filling at that price **or
    /// better** and never worse.
    ///
    /// The entry counterpart to [`set_stop`](Wallet::set_stop) — where the
    /// protective legs flatten an open position, this one opens or adjusts one.
    /// A buy fills when a bar's `low` reaches `limit` (at `limit`, or at the
    /// `open` when the bar gapped below it — the better price); a sell mirrors
    /// on `high`. Idempotent and latest-wins per symbol, like the protective
    /// legs, so re-submitting each bar walks the price. Returns the resting
    /// order's [`OrderId`] in an [`Ack::Working`].
    ///
    /// The [`Size`] resolves at the **fill** price, not at submission — an
    /// all-in `value_frac(1.0)` sizes against what the equity actually is when
    /// the limit is hit.
    ///
    /// Defaults to [`UnsupportedOperation`](WalletError::UnsupportedOperation)
    /// so a downstream wallet whose venue has no resting limit (or which hasn't
    /// wired one yet) keeps compiling; [`PaperWallet`] implements it.
    fn set_limit(
        &mut self,
        symbol: Sym,
        side: Side,
        size: Size,
        limit: Reference,
    ) -> Result<Ack<Sym>, WalletError> {
        let _ = (symbol, side, size, limit);
        Err(WalletError::UnsupportedOperation)
    }

    /// Cancel any resting limit order on `symbol`. A no-op when none rests, so
    /// a strategy can call it unconditionally. Defaults to `Ok(())` for wallets
    /// that never accept one.
    fn cancel_limit(&mut self, symbol: &Sym) -> Result<(), WalletError> {
        let _ = symbol;
        Ok(())
    }

    /// Credit the cash balance by `delta` (positive = deposit / credit,
    /// negative = withdrawal / debit) with no order flow. Used by
    /// [`Portfolio`](crate::portfolio::Portfolio)'s cash-phase rebalance to
    /// shift free cash between sub-wallets without generating fills, and
    /// available to any caller that wants to represent an external funding
    /// event (initial deposit, broker margin adjustment).
    ///
    /// **A credit that is not an internal transfer breaks the metrics.** The
    /// [`metrics`](crate::metrics) module assumes a closed equity curve, in
    /// which a deposit is indistinguishable from a gain and a withdrawal from a
    /// loss. A caller representing a genuine external flow must neutralize it
    /// before reducing the curve — see the *Closed system* note on that module.
    /// The portfolio rebalance is exempt: it moves cash *between* sub-wallets,
    /// so the aggregate curve stays closed.
    ///
    /// **Support is optional.** The default impl returns
    /// [`WalletError::UnsupportedOperation`] — an in-memory paper wallet
    /// overrides it directly, while a live-broker impl selectively wires
    /// it up to the venue's deposit / withdrawal / sub-account transfer
    /// API only when such a facility exists. Callers should treat an
    /// error as "the transfer didn't happen" and fall back to
    /// trait-friendly alternatives (position resize via
    /// [`set_position`](Self::set_position)) when they need to move value
    /// through a wallet that doesn't support programmatic cash adjustment.
    ///
    /// An impl that supports the operation should return
    /// [`WalletError::InsufficientFunds`] if `delta < 0` and the resulting
    /// balance would go negative — matching the "no margin" convention of
    /// the market movements. Positive-delta credits are always feasible.
    fn adjust_funds(&mut self, delta: Real) -> Result<(), WalletError> {
        let _ = delta;
        Err(WalletError::UnsupportedOperation)
    }

    /// Every position this wallet currently holds, as signed unit-tagged
    /// amounts. Symbols with no position are omitted; order is unspecified.
    ///
    /// Returns an owned `Vec` rather than an iterator so the method stays
    /// object-safe — this is called through `&dyn Wallet` by
    /// [`Portfolio`](crate::portfolio::Portfolio)'s position-phase rebalance,
    /// which needs to know what a sub-wallet holds before deciding what to
    /// scale down. It allocates only on rebalance-fire bars.
    ///
    /// **Support is optional**, same shape as [`adjust_funds`](Self::adjust_funds):
    /// the default returns empty, which a caller must read as "this wallet
    /// cannot enumerate its positions", not as "it holds nothing". The
    /// consequence for a portfolio is benign and already-documented — a
    /// contributor whose positions can't be enumerated simply gets no
    /// position-phase downsizing, and its shortfall carries to the next fire,
    /// exactly as for a wallet that refuses `adjust_funds`.
    fn positions(&self) -> Vec<Units<Sym>> {
        Vec::new()
    }

    /// Whether this wallet can carry a **short** (negative) position.
    ///
    /// A position is signed [`Units`] throughout the trait, so shorting is the
    /// baseline and the default is `true` — what [`PaperWallet`] takes (a sell
    /// there credits cash, so it is always feasible; see
    /// [`InsufficientFunds`](WalletError::InsufficientFunds)). A wallet whose
    /// venue cannot hold a negative position — a **spot** account, where a
    /// position is an owned base-asset balance — overrides it to `false`.
    ///
    /// This is **introspection, not enforcement**. Answering `false` does not
    /// by itself refuse a negative target: an impl that can't short must still
    /// clamp or reject one itself (`CoinbaseWallet` clamps to flat and books a
    /// [`Rejection`] for the un-shortable remainder). The point is that a
    /// caller can ask *before* trading — a long/short strategy, a CLI preflight,
    /// or a driver picking between wallets can degrade to long-only, or warn,
    /// instead of discovering the limit one rejection at a time.
    ///
    /// A wrapper delegates to what it wraps: capability is a fact about the
    /// account underneath, not about the view onto it.
    fn can_short(&self) -> bool {
        true
    }

    /// The currency this wallet's [`funds`](Wallet::funds) and quote leg are
    /// denominated in — `"USDT"` for a linear USDⓈ-M swap account, `"USD"` or
    /// `"EUR"` for a spot one.
    ///
    /// **`None` means "this wallet does not say", never "no currency".** Same
    /// shape as [`positions`](Self::positions)'s empty default: every amount in
    /// this trait is a bare [`Real`] in *some* unit, and a caller must read the
    /// default as an absent label rather than as an absent currency. A
    /// [`PaperWallet`] answers `None` unless it was told
    /// ([`with_quote_ccy`](PaperWallet::with_quote_ccy)) — simulated money has
    /// no venue to ask.
    ///
    /// This is **introspection, not conversion**, the same line
    /// [`can_short`](Self::can_short) draws. fugazi does no FX anywhere: a run
    /// is sound only if every price fed to it shares one numeraire, and this
    /// method reports what that numeraire *is* so a caller can label a balance,
    /// refuse a mixed-currency universe, or reconcile against a venue — never so
    /// that anything here can convert between two. Answering does not make
    /// mixing safe.
    ///
    /// A wrapper delegates to what it wraps, for `can_short`'s reason: the
    /// denomination is a fact about the account underneath, not about the view
    /// onto it.
    fn quote_ccy(&self) -> Option<&str> {
        None
    }

    /// Install a per-symbol [`TradingCosts`] override — every fill on
    /// `symbol` thereafter books through this bundle instead of the wallet's
    /// default. Latest-wins per symbol.
    ///
    /// Scales to any number of symbols — a multi-asset driver just calls this
    /// once per traded symbol (the pairs CLI does it for `left`/`right`; the
    /// portfolio runner loops over its universe). An impl's fallback default
    /// doubles as the "unscoped" model for symbols the caller doesn't
    /// explicitly configure; [`PaperWallet::new`] (zero-cost default) plus
    /// per-symbol installs gives a fully symmetric, N-way cost model where
    /// every priced leg is a per-symbol entry.
    ///
    /// **Support is optional.** The default returns
    /// [`WalletError::UnsupportedOperation`]: costs are a *modelling* concept,
    /// and a live wallet's fees are set by the venue, not by us. A live impl
    /// may still override it where the venue exposes a fee tier. Callers
    /// installing costs across a heterogeneous set of wallets should treat
    /// `Err` as "this wallet prices its own fills" and carry on.
    fn set_costs_for(&mut self, symbol: Sym, costs: TradingCosts) -> Result<(), WalletError> {
        let _ = (symbol, costs);
        Err(WalletError::UnsupportedOperation)
    }

    /// Drain the [`Rejection`]s booked since the last call — orders this wallet
    /// refused, in the order it refused them.
    ///
    /// This is the wallet's **failure stream**, the twin of the fill stream
    /// [`update`](Wallet::update) returns, and it exists because the `Result` on
    /// a submission is not enough on its own. A strategy has no way to report an
    /// `Err` — its [`trade`](crate::Strategy::trade) returns `()` — and an order
    /// accepted as [`Ack::Working`] can still fail later at fill time, when
    /// nobody holds a `Result` to check. A driver drains this each bar and routes
    /// the entries to [`Strategy::on_reject`](crate::Strategy::on_reject).
    ///
    /// Draining is destructive: each rejection is yielded exactly once. It is
    /// deliberately distinct from
    /// [`PaperWallet::rejections`](PaperWallet::rejections), which is a
    /// non-destructive view of the *whole* run history for tests and debugging —
    /// draining does not disturb it.
    ///
    /// The default returns nothing, so an implementor that never refuses an
    /// order — or that surfaces failures entirely out-of-band — needs no
    /// override. **Any implementor that can drop an order should override it**;
    /// a refusal that reaches no one leaves the strategy's `Position` and `Book`
    /// describing a position it does not hold.
    fn take_rejections(&mut self) -> Vec<Rejection<Sym>> {
        Vec::new()
    }

    /// Drain fills that arrived **out of band** — booked by the venue between
    /// bars rather than on a specific [`update`](Wallet::update) call — and
    /// return them, clearing the buffer.
    ///
    /// [`update`](Wallet::update) is the fill stream for order flow tied to a
    /// bar (a queued market order filling at the next `open`, a resting stop
    /// triggering against a candle's range). A live venue, though, fills on
    /// its own schedule and reports fills asynchronously; a live [`Wallet`]
    /// buffers those and hands them over here, so the fill latency isn't
    /// pinned to the bar-feeding cadence and a fill on a symbol that didn't
    /// tick this bar still reaches the strategy. A driver should call this
    /// once per bar (the [`run`](crate::backtest::run) loop does) and route
    /// each returned [`Order`] through
    /// [`Strategy::on_fill`](crate::Strategy::on_fill), exactly as it does the
    /// [`update`](Wallet::update) fills.
    ///
    /// The default returns an empty vector — the [`PaperWallet`] and every
    /// synchronous impl have no out-of-band fills, so this is a no-op for
    /// backtests (the equity curve is byte-identical whether or not the driver
    /// drains it).
    fn poll_fills(&mut self) -> Vec<Order<Sym>> {
        Vec::new()
    }

    /// Cancel a working order by the [`OrderId`] its submission returned in an
    /// [`Ack`], returning `Ok(())` once it is (or already was) gone.
    ///
    /// Complements [`cancel_protective`](Wallet::cancel_protective) (which
    /// drops the whole resting bracket) with a single-order cancel — the shape
    /// a live venue exposes for a working entry order. The default returns
    /// [`WalletError::UnsupportedOperation`]; a paper impl overrides it to drop
    /// a matching queued market order or resting protective leg, and a live
    /// impl relays a cancel to the broker. Cancelling an id the wallet no
    /// longer knows (already filled, already cancelled, never minted here) is
    /// **not** an error — the post-condition "that order is not working" holds
    /// either way — so an impl should return `Ok(())` for an unknown id.
    fn cancel(&mut self, id: OrderId) -> Result<(), WalletError> {
        let _ = id;
        Err(WalletError::UnsupportedOperation)
    }

    /// Serialize this account's resumable state, or
    /// [`Null`](serde_json::Value::Null) for an account that doesn't have any of
    /// its own.
    ///
    /// The default — `Null`, paired with a
    /// [`restore_state`](Wallet::restore_state) that accepts anything — is the
    /// right answer for a **live venue**: the broker holds the positions and the
    /// cash, so a local snapshot could only go stale, and replaying one over a
    /// resumed session would overwrite reality with a guess. A live wallet
    /// re-reads its account instead (`refresh_account`) and lets the run's
    /// strategy state be the only thing the state file carries.
    /// [`PaperWallet`] overrides both: it *is* the book, so its state must
    /// round-trip.
    ///
    /// `Self: Sized` keeps `dyn Wallet<Sym>` object-safe — [`Strategy::trade`]
    /// hands out `&mut dyn Wallet<Sym>`, so these two must stay out of the
    /// vtable. Nothing is lost: the resume driver is generic over a concrete
    /// wallet type.
    ///
    /// [`Strategy::trade`]: crate::Strategy::trade
    fn snapshot_state(&self) -> serde_json::Value
    where
        Self: Sized,
        Sym: serde::Serialize + serde::de::DeserializeOwned,
    {
        serde_json::Value::Null
    }

    /// Restore state produced by [`snapshot_state`](Wallet::snapshot_state).
    /// The default accepts and ignores — see there for why that is correct for
    /// a live venue.
    fn restore_state(&mut self, state: &serde_json::Value) -> Result<(), String>
    where
        Self: Sized,
        Sym: serde::Serialize + serde::de::DeserializeOwned,
    {
        let _ = state;
        Ok(())
    }
}
