//! The composite [`PortfolioWallet`] a [`Portfolio`](super::Portfolio) exposes
//! to `backtest::run`, the internal [`PortfolioInner`] that carries one
//! [`PaperWallet`] per child, and the [`SubWalletHandle`] each child trades
//! into.
//!
//! Every child strategy in a portfolio needs its own accounting — its own
//! cash, its own bracket table, its own equity for `value_frac` sizing — but
//! [`backtest::run`](crate::backtest::run) only sees one wallet. The seam
//! here is a shared [`Rc<RefCell<PortfolioInner>>`]: the outer
//! [`PortfolioWallet`] reports aggregate reads (equity, funds, net position)
//! to the driver, while each child trades through a [`SubWalletHandle`] that
//! delegates to its own [`PaperWallet`] and namespaces its OrderIds into a
//! portfolio-wide space so per-child fill routing survives collisions.
//!
//! The mutating methods on [`PortfolioWallet`] itself (`set`, `close`,
//! `set_stop`, …) **panic** — the outer wallet is a *reporting view*, not a
//! trading interface. All order flow reaches sub-wallets through
//! [`SubWalletHandle`] inside [`Portfolio::trade`](super::Portfolio); a caller
//! that reaches around the Portfolio and mutates the outer wallet directly is
//! working against the design, and the panic is the loudest signal we can
//! give.

use std::cell::RefCell;
use std::collections::HashMap;
use std::hash::Hash;
use std::rc::Rc;

use crate::costs::TradingCosts;
use crate::types::{Candle, Real};
use crate::wallet::{
    Ack, Order, OrderId, PaperWallet, Reference, Side, Size, Units, Wallet, WalletError,
};

/// The interior state a [`PortfolioWallet`] and every
/// [`SubWalletHandle`] share via `Rc<RefCell<_>>`. Carries one
/// [`PaperWallet`] per child plus the id-translation tables needed to route
/// fills back to their owning child.
///
/// Sub-wallets mint their own [`OrderId`]s starting at `0`, so two subs would
/// otherwise collide on the wire. Portfolio mints a global id per
/// submission and keeps `(sub_idx, sub_local_id) → portfolio_id` in
/// [`sub_to_pf`](Self::sub_to_pf), translating on the way out of
/// [`PortfolioWallet::update`]. [`owners`](Self::owners) then maps the
/// portfolio id → child idx for [`Portfolio::on_fill`](super::Portfolio) to
/// route fills to the right child.
pub(super) struct PortfolioInner<Sym> {
    pub(super) subs: Vec<PaperWallet<Sym>>,
    /// Portfolio-wide `OrderId` → owning child index. Populated at
    /// submission via [`register_ack`](Self::register_ack), drained by
    /// [`Portfolio::on_fill`](super::Portfolio).
    pub(super) owners: HashMap<OrderId, usize>,
    /// `(child_idx, sub_local_id)` → portfolio-wide `OrderId`. Translates
    /// the sub-wallet's fill-stream id back to what the outside world saw.
    pub(super) sub_to_pf: HashMap<(usize, OrderId), OrderId>,
    /// Running counter for portfolio-wide id minting.
    next_pf_id: u64,
}

impl<Sym: Clone + Eq + Hash> PortfolioInner<Sym> {
    pub(super) fn new(subs: Vec<PaperWallet<Sym>>) -> Self {
        Self {
            subs,
            owners: HashMap::new(),
            sub_to_pf: HashMap::new(),
            next_pf_id: 0,
        }
    }

    fn mint_pf_id(&mut self) -> OrderId {
        let id = OrderId(self.next_pf_id);
        self.next_pf_id += 1;
        id
    }

    /// Translate a sub-wallet's [`Ack`] into the portfolio-wide id space,
    /// registering the owner mapping so [`Portfolio::on_fill`](super::Portfolio)
    /// can dispatch the eventual fill to the right child.
    fn register_ack(&mut self, idx: usize, sub_ack: Ack<Sym>) -> Ack<Sym> {
        let pf_id = self.mint_pf_id();
        match sub_ack {
            Ack::Working(sub_id) => {
                self.sub_to_pf.insert((idx, sub_id), pf_id);
                self.owners.insert(pf_id, idx);
                Ack::Working(pf_id)
            }
            Ack::Filled(mut order) => {
                // Synchronous fills never come from PaperWallet (it always
                // queues to the next open); a live sub could return one, in
                // which case there's no later update-stream entry to
                // translate, so we only rewrite the id and skip
                // sub_to_pf. Not routed to on_fill either — the driver
                // only fans update()'s return value.
                order.id = pf_id;
                Ack::Filled(order)
            }
        }
    }

    /// Reset every sub-wallet and clear the id-tracking tables — matches
    /// [`Strategy::reset`](crate::Strategy::reset) semantics on the wallet
    /// side.
    pub(super) fn reset(&mut self) {
        for sub in &mut self.subs {
            sub.reset();
        }
        self.owners.clear();
        self.sub_to_pf.clear();
        self.next_pf_id = 0;
    }
}

/// A composite [`Wallet`] that carries one [`PaperWallet`] per child
/// strategy behind an aggregate view.
///
/// This is what a caller hands to [`backtest::run`](crate::backtest::run) when
/// driving a [`Portfolio`](super::Portfolio): the driver sees a normal
/// [`Wallet<Sym>`] and gets a normal [`RunReport<Sym>`] back — aggregate
/// [`equity`](Wallet::equity) is the sum of every child's equity,
/// [`position`](Wallet::position) is net across children, and the fill stream
/// out of [`update`](Wallet::update) carries every child's fills tagged with
/// portfolio-wide [`OrderId`]s.
///
/// **The mutating methods panic.** `set` / `close` / `set_stop` / … are
/// meaningless at the aggregate level — a portfolio can't unambiguously
/// answer "which child sends this order?" — and are never called during a
/// well-formed run: the driver only calls [`update`](Wallet::update) and the
/// reading methods, and children trade through [`SubWalletHandle`] instead.
/// A panic here means the composition invariant broke.
///
/// Build one with [`Portfolio::wallet_view`](super::Portfolio::wallet_view).
/// Multiple views share the same interior, so cloning is a plain [`Rc`]
/// bump.
pub struct PortfolioWallet<Sym> {
    inner: Rc<RefCell<PortfolioInner<Sym>>>,
}

impl<Sym> Clone for PortfolioWallet<Sym> {
    fn clone(&self) -> Self {
        Self {
            inner: Rc::clone(&self.inner),
        }
    }
}

impl<Sym> PortfolioWallet<Sym> {
    pub(super) fn from_inner(inner: Rc<RefCell<PortfolioInner<Sym>>>) -> Self {
        Self { inner }
    }
}

impl<Sym: Clone + Eq + Hash> PortfolioWallet<Sym> {
    /// The equity of the child at index `idx` — funds plus mark-to-market
    /// positions in *that* child's sub-wallet. Ordered by the child's
    /// `.add(...)` index on the builder.
    ///
    /// # Panics
    /// Panics if `idx` is out of range.
    pub fn sub_equity(&self, idx: usize) -> Reference {
        self.inner.borrow().subs[idx].equity()
    }

    /// The cash balance of the child at index `idx`.
    ///
    /// # Panics
    /// Panics if `idx` is out of range.
    pub fn sub_funds(&self, idx: usize) -> Reference {
        self.inner.borrow().subs[idx].funds()
    }

    /// The number of child sub-wallets carried in this portfolio.
    pub fn sub_count(&self) -> usize {
        self.inner.borrow().subs.len()
    }
}

impl<Sym: Clone + Eq + Hash> Wallet<Sym> for PortfolioWallet<Sym> {
    fn funds(&self) -> Reference {
        let inner = self.inner.borrow();
        Reference(inner.subs.iter().map(|w| w.funds().0).sum())
    }

    fn position(&self, symbol: &Sym) -> Units<Sym> {
        let inner = self.inner.borrow();
        let amount: Real = inner.subs.iter().map(|w| w.position(symbol).amount).sum();
        Units {
            symbol: symbol.clone(),
            amount,
        }
    }

    fn price(&self, symbol: &Sym) -> Option<Reference> {
        // Sub-wallets fed from the same driver see the same price; take the
        // first one that has any.
        self.inner
            .borrow()
            .subs
            .iter()
            .find_map(|w| w.price(symbol))
    }

    fn equity(&self) -> Reference {
        let inner = self.inner.borrow();
        Reference(inner.subs.iter().map(|w| w.equity().0).sum())
    }

    fn update(&mut self, symbol: Sym, candle: Candle) -> Vec<Order<Sym>> {
        let mut inner = self.inner.borrow_mut();
        // Feed every sub the same bar so their pending queues flush, their
        // resting brackets trigger, and their mark-to-market updates. Then
        // translate each fill's sub-local id into the portfolio-wide id
        // space we've been reporting on Acks, so the driver can route
        // via `owners` in Portfolio::on_fill.
        let mut all = Vec::new();
        for i in 0..inner.subs.len() {
            let fills = inner.subs[i].update(symbol.clone(), candle);
            for mut fill in fills {
                if let Some(pf_id) = inner.sub_to_pf.remove(&(i, fill.id)) {
                    fill.id = pf_id;
                }
                all.push(fill);
            }
        }
        all
    }

    fn set_position(&mut self, _target: Units<Sym>) -> Result<Ack<Sym>, WalletError> {
        panic!(
            "PortfolioWallet::set_position: the aggregate wallet is a reporting view; \
             child strategies trade through SubWalletHandle inside Portfolio::trade."
        );
    }

    fn set(&mut self, _symbol: Sym, _side: Side, _size: Size) -> Result<Ack<Sym>, WalletError> {
        panic!(
            "PortfolioWallet::set: the aggregate wallet is a reporting view; \
             child strategies trade through SubWalletHandle inside Portfolio::trade."
        );
    }

    fn close(&mut self, _symbol: Sym) -> Result<Ack<Sym>, WalletError> {
        panic!(
            "PortfolioWallet::close: the aggregate wallet is a reporting view; \
             child strategies trade through SubWalletHandle inside Portfolio::trade."
        );
    }

    fn set_stop(&mut self, _symbol: Sym, _trigger: Reference) -> Result<Ack<Sym>, WalletError> {
        panic!(
            "PortfolioWallet::set_stop: the aggregate wallet is a reporting view; \
             child strategies trade through SubWalletHandle inside Portfolio::trade."
        );
    }

    fn set_take_profit(
        &mut self,
        _symbol: Sym,
        _trigger: Reference,
    ) -> Result<Ack<Sym>, WalletError> {
        panic!(
            "PortfolioWallet::set_take_profit: the aggregate wallet is a reporting view; \
             child strategies trade through SubWalletHandle inside Portfolio::trade."
        );
    }

    fn cancel_protective(&mut self, _symbol: &Sym) -> Result<(), WalletError> {
        panic!(
            "PortfolioWallet::cancel_protective: the aggregate wallet is a reporting view; \
             child strategies trade through SubWalletHandle inside Portfolio::trade."
        );
    }
}

/// The per-child [`Wallet`] a [`Portfolio`](super::Portfolio) hands to each
/// child strategy inside [`trade`](super::Portfolio).
///
/// Reads (funds, position, price, equity) come from the child's *own*
/// sub-wallet — so `value_frac(1.0)` sizes against the child's allocated
/// equity, not the aggregate — and mutations forward to that same
/// sub-wallet, registering the returned [`Ack`] in the portfolio-wide id
/// space so [`Portfolio::on_fill`](super::Portfolio) can route the fill
/// back to this child.
///
/// [`update`](Wallet::update) is a no-op / panic path: the driver only calls
/// `update` on the outer [`PortfolioWallet`], never on a handle.
pub(super) struct SubWalletHandle<Sym> {
    inner: Rc<RefCell<PortfolioInner<Sym>>>,
    idx: usize,
}

impl<Sym> SubWalletHandle<Sym> {
    pub(super) fn new(inner: Rc<RefCell<PortfolioInner<Sym>>>, idx: usize) -> Self {
        Self { inner, idx }
    }
}

impl<Sym: Clone + Eq + Hash> Wallet<Sym> for SubWalletHandle<Sym> {
    fn funds(&self) -> Reference {
        self.inner.borrow().subs[self.idx].funds()
    }

    fn position(&self, symbol: &Sym) -> Units<Sym> {
        self.inner.borrow().subs[self.idx].position(symbol)
    }

    fn price(&self, symbol: &Sym) -> Option<Reference> {
        self.inner.borrow().subs[self.idx].price(symbol)
    }

    fn equity(&self) -> Reference {
        self.inner.borrow().subs[self.idx].equity()
    }

    fn update(&mut self, _symbol: Sym, _candle: Candle) -> Vec<Order<Sym>> {
        // Driver never feeds a handle — it feeds the outer PortfolioWallet
        // which fans to every sub. A handle receiving update() means the
        // caller wired the driver against a handle rather than the outer
        // view.
        panic!(
            "SubWalletHandle::update: driver should update PortfolioWallet, not a handle."
        );
    }

    fn set_position(&mut self, target: Units<Sym>) -> Result<Ack<Sym>, WalletError> {
        let mut inner = self.inner.borrow_mut();
        let ack = inner.subs[self.idx].set_position(target)?;
        Ok(inner.register_ack(self.idx, ack))
    }

    fn set(&mut self, symbol: Sym, side: Side, size: Size) -> Result<Ack<Sym>, WalletError> {
        let mut inner = self.inner.borrow_mut();
        let ack = inner.subs[self.idx].set(symbol, side, size)?;
        Ok(inner.register_ack(self.idx, ack))
    }

    fn close(&mut self, symbol: Sym) -> Result<Ack<Sym>, WalletError> {
        let mut inner = self.inner.borrow_mut();
        let ack = inner.subs[self.idx].close(symbol)?;
        Ok(inner.register_ack(self.idx, ack))
    }

    fn set_stop(&mut self, symbol: Sym, trigger: Reference) -> Result<Ack<Sym>, WalletError> {
        let mut inner = self.inner.borrow_mut();
        let ack = inner.subs[self.idx].set_stop(symbol, trigger)?;
        Ok(inner.register_ack(self.idx, ack))
    }

    fn set_take_profit(
        &mut self,
        symbol: Sym,
        trigger: Reference,
    ) -> Result<Ack<Sym>, WalletError> {
        let mut inner = self.inner.borrow_mut();
        let ack = inner.subs[self.idx].set_take_profit(symbol, trigger)?;
        Ok(inner.register_ack(self.idx, ack))
    }

    fn cancel_protective(&mut self, symbol: &Sym) -> Result<(), WalletError> {
        self.inner.borrow_mut().subs[self.idx].cancel_protective(symbol)
    }
}

/// Split `total_funds` into `n` allocations by `weights` (normalized to sum
/// to `1.0`). Used at portfolio build to seed each child's sub-wallet.
pub(super) fn allocate_funds(total_funds: Real, weights: &[Real]) -> Vec<Real> {
    let sum: Real = weights.iter().sum();
    if sum <= 0.0 {
        // Degenerate — hand everything to the first slot so the run can
        // proceed; the panic on empty weights lives at build time.
        let mut out = vec![0.0; weights.len()];
        if !out.is_empty() {
            out[0] = total_funds;
        }
        return out;
    }
    weights.iter().map(|w| total_funds * w / sum).collect()
}

/// Fresh [`PaperWallet`]s seeded from `initial_funds` (one per child),
/// optionally wearing `costs` cloned per-sub. Used by
/// [`PortfolioBuilder::build`](super::PortfolioBuilder).
pub(super) fn seed_subs<Sym>(
    initial_funds: &[Real],
    costs: Option<&TradingCosts>,
) -> Vec<PaperWallet<Sym>>
where
    Sym: Clone + Eq + Hash,
{
    initial_funds
        .iter()
        .map(|&f| match costs {
            Some(c) => PaperWallet::with_costs(f, c.clone()),
            None => PaperWallet::new(f),
        })
        .collect()
}
