//! The core [`Strategy`] trait — the decision layer above indicators and
//! signals. The [`Wallet`] it trades into and the surrounding
//! vocabulary ([`Side`](crate::Side), [`Size`](crate::Size),
//! [`Order`], the unit-tagged [`Reference`](crate::Reference) /
//! [`Units`](crate::Units) amounts, [`WalletError`](crate::WalletError), and the
//! built-in in-memory [`PaperWallet`](crate::PaperWallet)) live in
//! [`crate::wallet`].

use crate::attribution::Attribution;
use crate::wallet::{Order, Rejection, Wallet};

/// An incremental trading strategy — the *decision* layer above indicators and
/// signals.
///
/// Like an [`Indicator`](crate::Indicator) and a [`Signal`](crate::Signal), a
/// strategy is advanced one bar at a time, but where those layers are pure
/// value-producers a strategy *acts*. The work is split in two so the expensive,
/// independent part is separated from the part that touches shared state:
///
/// * [`update`](Strategy::update) advances the strategy's own indicators and
///   signals. It borrows only `&mut self`, so the updates of many strategies are
///   independent and can run in parallel.
/// * [`trade`](Strategy::trade) reads that freshly-advanced state (`&self`) and
///   opens, adjusts, or closes positions on the [`Wallet`] handed to it. It is
///   *price-free*: the wallet is priced from outside (see [`Wallet::update`]).
///   Trades against a shared wallet must run serially and in order, since
///   funds/value sizing resolves against the wallet's running state.
///
/// A typical driver does, each bar: feed the wallet its prices, `update` every
/// strategy, then `trade` each one. Because [`Wallet`] is taken as `&mut dyn`,
/// the same strategy runs against a [`PaperWallet`](crate::PaperWallet) backtest
/// or a live broker wallet unchanged.
///
/// **One shape does not honour that literally.**
/// [`Portfolio`](crate::portfolio::Portfolio) is a composite that needs one
/// sub-wallet *per child*, and `trade` offers it one wallet — so it ignores the
/// argument and trades its own interior instead. That makes the
/// portfolio/wallet pairing a caller obligation rather than a type-level one,
/// which is why `Portfolio` both checks it at runtime (an unpriced interior
/// panics rather than producing a silently flat equity curve) and offers
/// [`Portfolio::run`](crate::portfolio::Portfolio::run), where there is nothing
/// to pair. Every other shape routes all order flow through the wallet it is
/// handed.
pub trait Strategy {
    /// The per-bar input — commonly a [`Candle`](crate::Candle), or a
    /// multi-asset snapshot.
    type Input;

    /// The symbol type identifying instruments in the [`Wallet`].
    type Symbol;

    /// Advance the strategy's indicators/signals on the next bar. No trading
    /// happens here, so this can run independently of every other strategy.
    fn update(&mut self, input: Self::Input);

    /// Act on `wallet` using the state from the most recent
    /// [`update`](Strategy::update) — opening, adjusting, or closing positions.
    fn trade(&self, wallet: &mut dyn Wallet<Self::Symbol>);

    /// Notify the strategy of an [`Order`] that filled on its wallet — the wallet's
    /// fill stream (see [`Wallet::update`]). The driver calls this for each fill,
    /// before the next [`update`](Strategy::update)/[`trade`](Strategy::trade), so a
    /// strategy can track its own position from fills rather than polling the
    /// wallet. Defaults to a no-op for strategies that don't need it.
    fn on_fill(&mut self, order: &Order<Self::Symbol>) {
        let _ = order;
    }

    /// Hook called for each order the wallet **refused** — the failure-side twin
    /// of [`on_fill`](Self::on_fill).
    ///
    /// A strategy that tracks its own position from the fill stream is, by
    /// default, silently wrong when a submission is refused: it staged an entry,
    /// no fill ever arrives, and nothing tells it so. Override this to notice.
    /// The common shapes are to stand down (clear whatever intent was staged) or
    /// to escalate — a repeated
    /// [`InsufficientFunds`](crate::WalletError::InsufficientFunds) usually means
    /// the sizing indicator disagrees with the wallet about available capital,
    /// which is not a condition to trade through. A refused
    /// [`Stop`](crate::OrderKind::Stop) is graver still: the position is still
    /// open and its protection did not fire.
    ///
    /// Called by the driver before [`update`](Self::update) for refusals booked
    /// while the wallet was being priced, and after [`trade`](Self::trade) for
    /// those the strategy's own submissions caused this bar. Default: no-op.
    fn on_reject(&mut self, rejection: &Rejection<Self::Symbol>) {
        let _ = rejection;
    }

    /// Whether the strategy has seen enough history that its
    /// [`trade`](Strategy::trade) decisions are safe to act on. A driver skips
    /// [`trade`](Strategy::trade) while this returns `false` — but still calls
    /// [`update`](Strategy::update) and [`on_fill`](Strategy::on_fill), so the
    /// warm-up runs to completion.
    ///
    /// Defaults to `true` — a strategy with no warm-up (or one that doesn't
    /// care to gate on it) is ready from the first bar. A strategy built from
    /// sources with unstable tails (EMA, RSI, ATR, …) should override it to
    /// hold entries until those tails have settled; see
    /// [`SingleAssetStrategy::is_ready`](crate::strategies::SingleAssetStrategy)
    /// for a concrete implementation gated on the `stable_bars()` of every
    /// entry signal and protective level. Users who explicitly accept the
    /// unstable output on a particular subtree wrap it in
    /// [`Unstable`](crate::indicators::Unstable) — the safe default is to wait,
    /// and opting out is an explicit act.
    fn is_ready(&self) -> bool {
        true
    }

    /// **Take** this strategy's per-child run decomposition, leaving nothing
    /// behind — `None` for every strategy that is not a composite.
    ///
    /// A composite ([`Portfolio`](crate::portfolio::Portfolio)) nets its
    /// children's intents before anything reaches the account, so its fill
    /// stream alone cannot say which child asked for which unit. It therefore
    /// retains the split it already computes to move each child's ledger, and
    /// hands it over here; the driver folds the result into
    /// [`RunReport::attribution`](crate::RunReport::attribution). See
    /// [`Attribution`] for what reconciles and what a `ChildFill` is not.
    ///
    /// **Draining is the point.** The buffers grow with the run, exactly as the
    /// report's own `fills` and `equity_curve` do, and are scoped to it the same
    /// way. A caller driving one long-lived composite over repeated
    /// [`backtest::run`](crate::backtest::run) calls without taking the result
    /// would accumulate across all of them; the driver takes it on every run, so
    /// this only bites a caller reaching past the driver.
    ///
    /// Default: `None`, taking nothing.
    fn take_attribution(&mut self) -> Option<Attribution<Self::Symbol>> {
        None
    }

    /// Arm or clear a **one-shot override of this strategy's rebalance gate**,
    /// consumed by the very next [`trade`](Strategy::trade) call.
    ///
    /// `Some(hold)` makes the next `trade` behave as though the gate fired,
    /// leaving the symbols named in `hold` alone; `None` clears the latch. The
    /// driver arms it immediately before one `trade` and clears it immediately
    /// after, so the override never outlives the bar it was issued for — which
    /// is why it is deliberately **absent from
    /// [`save_state`](Strategy::save_state)**. A resumed run rebuilds with the
    /// latch clear, and cannot re-fire an instruction its operator issued for a
    /// bar that has already gone by.
    ///
    /// Two halves, and only the first is common to every shape. *Arming* is
    /// generic — it is the same act the gate performs on a fire bar, so it means
    /// exactly what `rebalance_on:` means for the shape it is called on:
    /// resizing a held position on `single:` / `multi:` / `pairs:`, re-running
    /// *selection* on `basket:`, and one full cash-then-position cycle on
    /// `portfolio:`. *Holding back* is per-symbol and applies to whatever
    /// order flow the shape would otherwise have produced for that symbol.
    ///
    /// Orders go out through the shape's ordinary path — [`Wallet::set`], not
    /// [`Wallet::settle_position`] — so a
    /// [`PaperWallet`](crate::PaperWallet) queues the move and fills it at the
    /// next bar's `open` like any other rebalance, and a live venue routes it to
    /// the broker now. Nothing fills on the bar that caused it, here as
    /// everywhere else.
    ///
    /// Default: a no-op, for the many strategies that have no gate to force.
    fn force_rebalance(&mut self, hold: Option<&[Self::Symbol]>) {
        let _ = hold;
    }

    /// Clear the strategy's own state (its signals/indicators), returning it to
    /// its freshly-constructed condition. Does not touch any wallet.
    fn reset(&mut self);

    /// Serialize this strategy's runtime state for run resuming — the object-safe
    /// twin of [`RunnableStrategy::save_state`](crate::spec::RunnableStrategy).
    ///
    /// Default [`Null`](serde_json::Value::Null). It exists on the base
    /// [`Strategy`] trait (not only `RunnableStrategy`) so a strategy *embedded
    /// inside an indicator* — the [`Sharpe`](crate::indicators::Sharpe) /
    /// [`Sortino`](crate::indicators::Sortino) / … trailing metrics drive one
    /// over a private wallet — can have its state captured through the `Strategy`
    /// handle the metric holds. The concrete spec-built wrappers override it.
    fn save_state(&self) -> serde_json::Value {
        serde_json::Value::Null
    }

    /// Restore state produced by [`save_state`](Strategy::save_state). Default:
    /// accept and ignore.
    fn load_state(&mut self, state: &serde_json::Value) -> Result<(), String> {
        let _ = state;
        Ok(())
    }
}
