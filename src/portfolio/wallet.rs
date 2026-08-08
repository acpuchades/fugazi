//! The composite [`PortfolioWallet`] a [`Portfolio`](super::Portfolio) exposes
//! to `backtest::run`.
//!
//! On a shared account the aggregate view is nearly trivial: cash, positions
//! and equity *are* the account's, so every read delegates straight to the
//! substrate. The one method that does real work is [`update`](Wallet::update),
//! which feeds the substrate the bar and then hands the resulting fills to the
//! netting layer to be attributed back to the child ledgers that caused them.
//!
//! The mutating methods **panic**. The outer wallet is a reporting view, not a
//! trading interface: all order flow reaches the account through
//! [`Portfolio::trade`](super::Portfolio)'s netting step, and a caller reaching
//! around that is working against the design.

use std::hash::Hash;
use std::sync::{Arc, Mutex};

use crate::types::{Candle, Real};
use crate::wallet::{Ack, Order, Reference, Rejection, Side, Size, Units, Wallet, WalletError};

use super::netting::PortfolioInner;

/// The aggregate [`Wallet`] view over a portfolio's single account.
///
/// Hand it to [`backtest::run`](crate::backtest::run) alongside the portfolio —
/// or, better, use [`Portfolio::run`](super::Portfolio::run) and never hold one.
pub struct PortfolioWallet<Sym> {
    inner: Arc<Mutex<PortfolioInner<Sym>>>,
}

impl<Sym> Clone for PortfolioWallet<Sym> {
    fn clone(&self) -> Self {
        Self {
            inner: Arc::clone(&self.inner),
        }
    }
}

impl<Sym> PortfolioWallet<Sym> {
    pub(super) fn from_inner(inner: Arc<Mutex<PortfolioInner<Sym>>>) -> Self {
        Self { inner }
    }
}

impl<Sym: Clone + Eq + Hash> PortfolioWallet<Sym> {
    /// Child `idx`'s notional equity — its ledger cash plus its share of the
    /// account's positions, marked to market. Ordered by `.add(...)` index.
    ///
    /// # Panics
    /// Panics if `idx` is out of range.
    pub fn sub_equity(&self, idx: usize) -> Reference {
        Reference(
            self.inner
                .lock()
                .expect("portfolio lock poisoned")
                .child_equity(idx),
        )
    }

    /// Assert that the child ledgers still sum to the account — the identity
    /// the netting design rests on. Intended for tests and debugging.
    ///
    /// # Panics
    /// Panics if the ledgers have drifted from the account.
    pub fn assert_books_balance(&self) {
        self.inner
            .lock()
            .expect("portfolio lock poisoned")
            .check_invariants();
    }
}

macro_rules! reporting_view_only {
    ($name:literal) => {
        panic!(concat!(
            "PortfolioWallet::",
            $name,
            ": the aggregate wallet is a reporting view. Child strategies trade through \
             their ledgers, and the portfolio nets their intents onto the account inside \
             Portfolio::trade.",
        ))
    };
}

impl<Sym: Clone + Eq + Hash> Wallet<Sym> for PortfolioWallet<Sym> {
    fn funds(&self) -> Reference {
        self.inner
            .lock()
            .expect("portfolio lock poisoned")
            .substrate
            .funds()
    }

    fn position(&self, symbol: &Sym) -> Units<Sym> {
        self.inner
            .lock()
            .expect("portfolio lock poisoned")
            .substrate
            .position(symbol)
    }

    fn price(&self, symbol: &Sym) -> Option<Reference> {
        self.inner
            .lock()
            .expect("portfolio lock poisoned")
            .substrate
            .price(symbol)
    }

    fn equity(&self) -> Reference {
        self.inner
            .lock()
            .expect("portfolio lock poisoned")
            .substrate
            .equity()
    }

    /// Feed the account the bar, then attribute what filled back to the child
    /// ledgers. Returns one synthetic [`Order`] per child share rather than the
    /// single netted fill, so `on_fill` routing and the run blotter read as
    /// they did when every child had its own wallet.
    fn update(&mut self, symbol: Sym, candle: Candle) -> Vec<Order<Sym>> {
        self.inner
            .lock()
            .expect("portfolio lock poisoned")
            .settle(symbol, candle)
    }

    /// Fills the venue reported out of band, attributed like any other.
    ///
    /// A no-op while the account is a [`PaperWallet`](crate::PaperWallet), and
    /// load-bearing once it isn't: a live venue reports a fill on a symbol that
    /// didn't tick this bar through here and nowhere else.
    fn poll_fills(&mut self) -> Vec<Order<Sym>> {
        self.inner.lock().expect("portfolio lock poisoned").poll()
    }

    fn take_rejections(&mut self) -> Vec<Rejection<Sym>> {
        let mut inner = self.inner.lock().expect("portfolio lock poisoned");
        // Pick up anything the account booked since the last settle, then hand
        // over the translated list. The account's own stream is *not* passed
        // through raw — a refusal there is about a netted order, so it belongs
        // to the children that contributed to it.
        inner.drain_substrate_rejections();
        inner.take_rejections()
    }

    fn set_position(&mut self, _target: Units<Sym>) -> Result<Ack<Sym>, WalletError> {
        reporting_view_only!("set_position")
    }
    fn set(&mut self, _symbol: Sym, _side: Side, _size: Size) -> Result<Ack<Sym>, WalletError> {
        reporting_view_only!("set")
    }
    fn close(&mut self, _symbol: Sym) -> Result<Ack<Sym>, WalletError> {
        reporting_view_only!("close")
    }
    fn set_stop(
        &mut self,
        _symbol: Sym,
        _trigger: Reference,
        _size: Size,
    ) -> Result<Ack<Sym>, WalletError> {
        reporting_view_only!("set_stop")
    }
    fn set_take_profit(
        &mut self,
        _symbol: Sym,
        _trigger: Reference,
        _size: Size,
    ) -> Result<Ack<Sym>, WalletError> {
        reporting_view_only!("set_take_profit")
    }
    fn cancel_protective(&mut self, _symbol: &Sym) -> Result<(), WalletError> {
        reporting_view_only!("cancel_protective")
    }

    fn positions(&self) -> Vec<Units<Sym>> {
        self.inner
            .lock()
            .expect("portfolio lock poisoned")
            .substrate
            .positions()
    }

    fn adjust_funds(&mut self, delta: Real) -> Result<(), WalletError> {
        // Adjusting the account as a whole would leave the ledgers stale and
        // break the sum-to-account identity. Rebalancing moves notional cash
        // between ledgers instead, which needs nothing from the account.
        let _ = delta;
        Err(WalletError::UnsupportedOperation)
    }
}
