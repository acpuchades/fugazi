//! Per-child notional books, and the [`Wallet`] handle a child trades through.
//!
//! A [`Portfolio`](super::Portfolio) running on a single account cannot give
//! each child its own wallet — the venue holds one balance and one position per
//! symbol. What it gives each child instead is a `Ledger`: pure bookkeeping
//! saying "this child notionally owns 3 of the account's 10 BTC and $500 of its
//! $2,000". The ledgers always sum to the account; that identity is the
//! composite's core invariant, asserted in tests.
//!
//! `LedgerWallet` is what makes that invisible to the child. It implements
//! `Wallet` like any other, but its reads come from the child's ledger rather
//! than the account — so `value_frac(1.0)` still means "all of *my* equity",
//! per-child equity stays meaningful, and no strategy code changes. Its writes
//! don't reach the venue: they record what the child wants its position to be,
//! and [`Portfolio::trade`](super::Portfolio) nets every child's intent into one
//! order per symbol afterwards.
//!
//! # Sizes resolve here, not at the fill
//!
//! This is the one deliberate behavioural difference from a per-child
//! `PaperWallet`. A paper wallet defers `Size` resolution to the fill, pricing
//! `value_frac(1.0)` at the next bar's open. Netting cannot: "all of A's equity"
//! and "3 units" are not addable until both are numbers, and the netted order
//! has to be submitted at decision time so it reaches the venue as soon as the
//! decision is made. So a `LedgerWallet` resolves against the child's ledger at
//! the current price, one bar earlier than a paper wallet would.
//!
//! # Hard cap
//!
//! A child may not spend past its ledger cash, even when the account has idle
//! cash belonging to a sibling. The refusal is booked as a [`Rejection`] exactly
//! as a standalone `PaperWallet` would book it, so a child sees no difference.
//! The alternative — letting a ledger go negative against the real balance —
//! would buy capital efficiency at the cost of making per-child equity (and
//! therefore every `value_frac` sizing decision) a fiction.

use std::collections::HashMap;
use std::hash::Hash;
use std::sync::{Arc, Mutex};

use serde::{Deserialize, Serialize};

use crate::types::{Candle, Real};
use crate::wallet::{
    Ack, Order, OrderId, Reference, Rejection, Side, Size, Units, Wallet, WalletError,
};
use crate::wallet::{CASH_EPSILON, POSITION_EPSILON};

use super::netting::PortfolioInner;

/// One child's notional book: its share of the account's cash and positions.
///
/// Carries no execution machinery — no pending queue, no resting orders, no
/// blotter. Those live on the substrate wallet, which is the only thing that
/// actually trades. A ledger is moved only by
/// [`attribute`](super::netting::PortfolioInner::attribute), from real fills.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(bound(
    serialize = "Sym: Serialize + Eq + Hash",
    deserialize = "Sym: Deserialize<'de> + Eq + Hash"
))]
pub(super) struct Ledger<Sym> {
    /// This child's share of the account's cash.
    pub(super) cash: Real,
    /// This child's signed notional units per symbol. Symbols it doesn't hold
    /// are absent rather than zero.
    pub(super) positions: HashMap<Sym, Real>,
}

impl<Sym: Clone + Eq + Hash> Ledger<Sym> {
    pub(super) fn new(cash: Real) -> Self {
        Self {
            cash,
            positions: HashMap::new(),
        }
    }

    pub(super) fn position(&self, symbol: &Sym) -> Real {
        self.positions.get(symbol).copied().unwrap_or(0.0)
    }

    /// Apply a signed unit change at `price`, moving cash the other way.
    /// `commission` is this child's share of the real fill's commission.
    pub(super) fn apply(&mut self, symbol: &Sym, delta: Real, price: Real, commission: Real) {
        let entry = self.positions.entry(symbol.clone()).or_insert(0.0);
        *entry += delta;
        if entry.abs() <= POSITION_EPSILON {
            self.positions.remove(symbol);
        }
        self.cash -= delta * price + commission;
    }

    /// Mark-to-market equity: cash plus every held position at `price_of`.
    /// A symbol with no price contributes nothing, matching `PaperWallet`.
    pub(super) fn equity(&self, price_of: impl Fn(&Sym) -> Option<Real>) -> Real {
        let held: Real = self
            .positions
            .iter()
            .map(|(symbol, &amount)| amount * price_of(symbol).unwrap_or(0.0))
            .sum();
        self.cash + held
    }
}

/// What a child wants its ledger position in one symbol to be, recorded by
/// [`LedgerWallet`] and consumed by the netting step.
#[derive(Debug, Clone, Copy)]
pub(super) struct Intent {
    /// Target ledger position in signed units, already resolved.
    pub(super) target: Real,
    /// The portfolio-wide id the child was handed for this submission. The
    /// synthetic fill the child eventually receives carries it back.
    pub(super) id: OrderId,
}

/// A child's resting protective level for one symbol.
///
/// The account holds only one bracket per symbol, so when several children want
/// stops on the same symbol the portfolio rests whichever is nearest to
/// triggering — see [`PortfolioInner::rest_protective`](super::netting::PortfolioInner).
#[derive(Debug, Clone, Copy, Default, Serialize, Deserialize)]
pub(super) struct ProtectiveIntent {
    pub(super) stop: Option<Real>,
    pub(super) take_profit: Option<Real>,
}

/// The per-child [`Wallet`] handed to a child inside
/// [`Portfolio::trade`](super::Portfolio).
///
/// Reads answer from child `idx`'s [`Ledger`]; the one exception is
/// [`price`](Wallet::price), which comes from the substrate because a price is a
/// fact about the market, not about a book. Writes record intent rather than
/// executing.
pub(super) struct LedgerWallet<Sym> {
    inner: Arc<Mutex<PortfolioInner<Sym>>>,
    idx: usize,
}

impl<Sym> LedgerWallet<Sym> {
    pub(super) fn new(inner: Arc<Mutex<PortfolioInner<Sym>>>, idx: usize) -> Self {
        Self { inner, idx }
    }
}

impl<Sym: Clone + Eq + Hash> Wallet<Sym> for LedgerWallet<Sym> {
    fn funds(&self) -> Reference {
        let inner = self.inner.lock().expect("portfolio lock poisoned");
        Reference(inner.ledgers[self.idx].cash)
    }

    fn position(&self, symbol: &Sym) -> Units<Sym> {
        let inner = self.inner.lock().expect("portfolio lock poisoned");
        Units {
            symbol: symbol.clone(),
            amount: inner.ledgers[self.idx].position(symbol),
        }
    }

    fn price(&self, symbol: &Sym) -> Option<Reference> {
        // From the marks cache: a price belongs to the market, not to a book.
        // The portfolio refreshes it from the snapshot each bar, in place of the
        // old substrate the account's `price()` used to serve.
        self.inner
            .lock()
            .expect("portfolio lock poisoned")
            .price_of(symbol)
            .map(Reference)
    }

    fn equity(&self) -> Reference {
        let inner = self.inner.lock().expect("portfolio lock poisoned");
        Reference(inner.ledgers[self.idx].equity(|s| inner.price_of(s)))
    }

    /// The **account's** answer, cached by `Portfolio::trade` before the
    /// children run: a ledger records notional intent, but that intent is netted
    /// onto the one real account, so what can be shorted is decided there.
    fn can_short(&self) -> bool {
        self.inner
            .lock()
            .expect("portfolio lock poisoned")
            .account_can_short
    }

    /// Carry is charged once, on the one real account the ledgers net onto, so
    /// every child would otherwise report the whole portfolio's coverage as its
    /// own. `None` — "this view does not say" — is the honest answer; ask the
    /// account.
    fn carry_coverage(&self) -> Option<(usize, usize)> {
        None
    }

    /// Takes the trait default (`None`) deliberately, unlike
    /// [`can_short`](Wallet::can_short) right above.
    ///
    /// Every read here goes through the portfolio mutex, and `Option<&str>`
    /// cannot outlive the guard it would be borrowed from — so delegating would
    /// mean either an owned `String` on the trait (an allocation on every call,
    /// on every wallet, for one caller) or caching a clone on each handle. The
    /// account's denomination is worth neither: `can_short` earned its cached
    /// copy because `Portfolio::trade` needs it per-bar to gate child intent,
    /// and nothing reads a child ledger's currency at all. Ask the account
    /// wallet, which is where the fact lives.
    fn quote_ccy(&self) -> Option<&str> {
        None
    }

    /// The **account's** answer, cached alongside
    /// [`can_short`](Wallet::can_short) — and delegated, unlike
    /// [`quote_ccy`](Wallet::quote_ccy) directly above.
    ///
    /// The lifetime argument that stops `quote_ccy` doesn't apply: a
    /// `&'static [&'static str]` borrows nothing from the guard it is read
    /// through, so carrying it across costs a pointer copy and no allocation.
    /// A child asking which venue's bars its notional slice is priced off gets
    /// the venue the intent actually nets onto, rather than a silent empty
    /// answer from a handle that trades no venue of its own.
    fn data_sources(&self) -> &'static [&'static str] {
        self.inner
            .lock()
            .expect("portfolio lock poisoned")
            .account_data_sources
    }

    /// The **account's** answer, cached per symbol alongside
    /// [`can_short`](Wallet::can_short) — and delegated, for
    /// [`data_sources`](Wallet::data_sources)'s reason rather than refused for
    /// [`quote_ccy`](Wallet::quote_ccy)'s: an `Option<Real>` is a copy, so it
    /// crosses the guard without borrowing from it.
    ///
    /// It is the account's leverage because that is the only leverage there is.
    /// A ledger is notional attribution — it holds no margin of its own, and
    /// its intent is netted against its siblings onto one real balance, which is
    /// where the borrowing happens. A child asking what its exposure is
    /// ultimately bounded by should get that number, not a `None` from a handle
    /// that margins nothing.
    fn leverage(&self, symbol: &Sym) -> Option<Real> {
        self.inner
            .lock()
            .expect("portfolio lock poisoned")
            .account_leverage
            .get(symbol)
            .copied()
            .flatten()
    }

    fn update(&mut self, _symbol: Sym, _candle: Candle) -> Vec<Order<Sym>> {
        // The driver feeds the account wallet the portfolio trades, not a child
        // handle. A handle receiving update() means the caller wired the driver
        // against a child's view rather than the portfolio's account.
        panic!("LedgerWallet::update: the driver should update the account wallet, not a handle.");
    }

    fn set_position(&mut self, target: Units<Sym>) -> Result<Ack<Sym>, WalletError> {
        let mut inner = self.inner.lock().expect("portfolio lock poisoned");
        inner.record_intent(self.idx, target.symbol, target.amount)
    }

    fn set(&mut self, symbol: Sym, side: Side, size: Size) -> Result<Ack<Sym>, WalletError> {
        // Resolve here rather than at the fill — see the module docs. The
        // reads feeding `Size::resolve` are the child's own, so a fraction
        // means a fraction *of this child*.
        let mut inner = self.inner.lock().expect("portfolio lock poisoned");
        let price = inner.price_of(&symbol).ok_or(WalletError::UnknownPrice)?;
        if price <= 0.0 {
            return Err(WalletError::InvalidPrice);
        }
        let ledger = &inner.ledgers[self.idx];
        let position = ledger.position(&symbol);
        let equity = ledger.equity(|s| inner.price_of(s));
        let magnitude = size.resolve(price, position, ledger.cash, equity);
        inner.record_intent(self.idx, symbol, side.sign() * magnitude)
    }

    fn close(&mut self, symbol: Sym) -> Result<Ack<Sym>, WalletError> {
        let mut inner = self.inner.lock().expect("portfolio lock poisoned");
        inner.record_intent(self.idx, symbol, 0.0)
    }

    fn set_stop(
        &mut self,
        symbol: Sym,
        trigger: Reference,
        _size: Size,
    ) -> Result<Ack<Sym>, WalletError> {
        // The size is implicit: a child's protective leg covers that child's
        // own position, which the portfolio reads off its ledger when it rests
        // the leg. Accepting the argument keeps the trait shape.
        let mut inner = self.inner.lock().expect("portfolio lock poisoned");
        Ok(inner.record_protective(self.idx, symbol, Some(trigger.0), None))
    }

    fn set_take_profit(
        &mut self,
        symbol: Sym,
        trigger: Reference,
        _size: Size,
    ) -> Result<Ack<Sym>, WalletError> {
        let mut inner = self.inner.lock().expect("portfolio lock poisoned");
        Ok(inner.record_protective(self.idx, symbol, None, Some(trigger.0)))
    }

    fn cancel_protective(&mut self, symbol: &Sym) -> Result<(), WalletError> {
        let mut inner = self.inner.lock().expect("portfolio lock poisoned");
        inner.clear_protective(self.idx, symbol);
        Ok(())
    }

    fn adjust_funds(&mut self, delta: Real) -> Result<(), WalletError> {
        // Moves this child's slice of the account's cash. The account balance
        // is untouched — only the notional split changes — which is exactly
        // what a portfolio rebalance wants and why its cash phase costs
        // nothing here.
        let mut inner = self.inner.lock().expect("portfolio lock poisoned");
        let ledger = &mut inner.ledgers[self.idx];
        if ledger.cash + delta < -CASH_EPSILON {
            return Err(WalletError::InsufficientFunds);
        }
        ledger.cash += delta;
        Ok(())
    }

    fn positions(&self) -> Vec<Units<Sym>> {
        let inner = self.inner.lock().expect("portfolio lock poisoned");
        inner.ledgers[self.idx]
            .positions
            .iter()
            .map(|(symbol, &amount)| Units {
                symbol: symbol.clone(),
                amount,
            })
            .collect()
    }

    // `take_rejections` and `poll_fills` stay at their trait defaults on
    // purpose. The driver drains the composite wallet; a child draining its own
    // handle would consume entries before that, deleting them from the run
    // report and from its own `on_reject`.

    // `set_limit` / `cancel_limit` / `cancel` are also left at their defaults:
    // a resting entry has no netting story yet (whose intent is it while it
    // rests?), so refusing is honest where guessing would not be. See the
    // module docs on strategy-layer limit entries.
}

/// A child's refusal, booked by the netting layer and drained through the
/// composite wallet.
pub(super) fn rejection<Sym>(symbol: Sym, id: OrderId, error: WalletError) -> Rejection<Sym> {
    Rejection {
        symbol,
        id,
        error,
        kind: crate::wallet::OrderKind::Market,
    }
}
