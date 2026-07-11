//! The core [`Strategy`] trait — the decision layer above indicators and
//! signals. The [`Wallet`](crate::Wallet) it trades into and the surrounding
//! vocabulary ([`Side`](crate::Side), [`Size`](crate::Size),
//! [`Order`](crate::Order), the unit-tagged [`Reference`](crate::Reference) /
//! [`Units`](crate::Units) amounts, [`WalletError`](crate::WalletError), and the
//! built-in in-memory [`PaperWallet`](crate::PaperWallet)) live in
//! [`crate::wallet`].

use crate::wallet::{Order, Wallet};

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
    /// for a concrete implementation gated on the `stable_period()` of every
    /// entry signal and protective level. Users who explicitly accept the
    /// unstable output on a particular subtree wrap it in
    /// [`Unstable`](crate::indicators::Unstable) — the safe default is to wait,
    /// and opting out is an explicit act.
    fn is_ready(&self) -> bool {
        true
    }

    /// Clear the strategy's own state (its signals/indicators), returning it to
    /// its freshly-constructed condition. Does not touch any wallet.
    fn reset(&mut self);
}
