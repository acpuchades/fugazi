//! The composite [`PortfolioWallet`] a [`Portfolio`](super::Portfolio) exposes
//! to `backtest::run`, the internal [`PortfolioInner`] that carries one
//! [`PaperWallet`] per child, and the [`SubWalletHandle`] each child trades
//! into.
//!
//! Every child strategy in a portfolio needs its own accounting — its own
//! cash, its own bracket table, its own equity for `value_frac` sizing — but
//! [`backtest::run`](crate::backtest::run) only sees one wallet. The seam
//! here is a shared `Arc<Mutex<PortfolioInner>>`: the outer
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

use std::collections::HashMap;
use std::hash::Hash;

use std::sync::{Arc, Mutex};

use crate::costs::TradingCosts;
use crate::types::{Candle, Real};
use crate::wallet::{
    Ack, Order, OrderId, PaperWallet, Reference, Rejection, Side, Size, Units, Wallet, WalletError,
};

/// Builds the sub-wallet for child `idx`, seeded with `funds` of cash.
///
/// Called once per child at [`Portfolio`](super::Portfolio) build time, and
/// again for each child on [`reset`](PortfolioInner::reset) — resetting a
/// portfolio rebuilds its sub-wallets from this rather than requiring a
/// `reset` method on the [`Wallet`] seam (there is no sensible one for a live
/// venue, and a defaulted no-op would silently corrupt the second run of an
/// `optimize` sweep).
///
/// `Arc` rather than `Box` so [`PortfolioInner`] can hold it and still be
/// rebuilt from; `Send + Sync` so the composite stays `Send`.
pub type SubWalletFactory<Sym> =
    Arc<dyn Fn(usize, Real) -> Box<dyn Wallet<Sym> + Send> + Send + Sync>;

/// The default [`SubWalletFactory`]: an in-memory [`PaperWallet`] per child,
/// optionally carrying a shared [`TradingCosts`] bundle.
pub(super) fn paper_sub_wallets<Sym>(costs: Option<TradingCosts>) -> SubWalletFactory<Sym>
where
    Sym: Clone + Eq + Hash + Send + 'static,
{
    Arc::new(move |_idx, funds| match &costs {
        Some(c) => Box::new(PaperWallet::with_costs(funds, c.clone())),
        None => Box::new(PaperWallet::new(funds)),
    })
}

/// The interior state a [`PortfolioWallet`] and every
/// [`SubWalletHandle`] share via `Arc<Mutex<_>>`. Carries one
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
    /// One wallet per child. **Erased**, not concrete: a portfolio of live
    /// sub-accounts is the whole point of the [`SubWalletFactory`] seam, and
    /// every operation the portfolio performs on a sub is a [`Wallet`] trait
    /// method.
    pub(super) subs: Vec<Box<dyn Wallet<Sym> + Send>>,
    /// How to (re)build a sub-wallet. Retained for
    /// [`reset`](Self::reset).
    factory: SubWalletFactory<Sym>,
    /// Each child's build-time cash allocation, in child order. Retained so
    /// `reset` reseeds exactly as `build` did.
    seeds: Vec<Real>,
    /// Per-symbol cost bundles installed *after* build via
    /// [`Portfolio::install_costs_for`](super::Portfolio::install_costs_for),
    /// in installation order. Replayed onto freshly-built subs by `reset`, so
    /// a reset portfolio books at the same rates the run did.
    scoped_costs: Vec<(Sym, TradingCosts)>,
    /// Portfolio-wide `OrderId` → owning child index. Populated at
    /// submission via [`register_ack`](Self::register_ack), drained by
    /// [`Portfolio::on_fill`](super::Portfolio).
    pub(super) owners: HashMap<OrderId, usize>,
    /// `(child_idx, sub_local_id)` → portfolio-wide `OrderId`. Translates
    /// the sub-wallet's fill-stream id back to what the outside world saw.
    pub(super) sub_to_pf: HashMap<(usize, OrderId), OrderId>,
    /// The reverse of [`sub_to_pf`](Self::sub_to_pf): portfolio-wide
    /// `OrderId` → `(child_idx, sub_local_id)`.
    ///
    /// Needed because [`Wallet::cancel`] takes an id *inward*, unlike every
    /// other method. A child holds portfolio-wide ids (that's what its
    /// [`Ack`]s carried), but every sub-wallet mints from `0` and
    /// [`PaperWallet::cancel`] matches on the raw id — so forwarding a
    /// child's id untranslated would cancel an unrelated order in that sub.
    pub(super) pf_to_sub: HashMap<OrderId, (usize, OrderId)>,
    /// Running counter for portfolio-wide id minting.
    next_pf_id: u64,
    /// Whether this interior has ever been priced through
    /// [`PortfolioWallet::update`] — the mis-pairing guard read by
    /// [`Portfolio::trade`](super::Portfolio). See the guard's rationale
    /// there.
    pub(super) priced: bool,
}

impl<Sym: Clone + Eq + Hash> PortfolioInner<Sym> {
    /// Seed one sub-wallet per entry in `seeds` from `factory`.
    pub(super) fn new(seeds: Vec<Real>, factory: SubWalletFactory<Sym>) -> Self {
        let subs = seeds
            .iter()
            .enumerate()
            .map(|(i, &funds)| factory(i, funds))
            .collect();
        Self {
            subs,
            factory,
            seeds,
            scoped_costs: Vec::new(),
            owners: HashMap::new(),
            sub_to_pf: HashMap::new(),
            pf_to_sub: HashMap::new(),
            next_pf_id: 0,
            priced: false,
        }
    }

    /// Remember a per-symbol bundle installed after build, so
    /// [`reset`](Self::reset) can replay it onto the rebuilt sub-wallets.
    /// Latest-wins per symbol, matching the wallets' own semantics.
    pub(super) fn record_scoped_costs(&mut self, symbol: Sym, costs: TradingCosts) {
        if let Some(slot) = self.scoped_costs.iter_mut().find(|(s, _)| *s == symbol) {
            slot.1 = costs;
        } else {
            self.scoped_costs.push((symbol, costs));
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
                self.pf_to_sub.insert(pf_id, (idx, sub_id));
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
    /// Rebuild every sub-wallet from the factory at its original seed, then
    /// replay any post-build per-symbol cost bundles.
    ///
    /// Rebuilding rather than calling a `reset` method on each sub is what
    /// keeps [`Wallet`] free of one: a live wallet has no meaningful "restore
    /// to freshly-constructed" (the venue holds the real position), and a
    /// defaulted no-op on the seam would silently leave a stale wallet driving
    /// the second run of an `optimize` sweep. Rebuilding says exactly what it
    /// does — for paper subs it is equivalent to the old in-place reset, and
    /// for a live sub it hands back a fresh handle on the same account.
    pub(super) fn reset(&mut self) {
        self.subs = self
            .seeds
            .iter()
            .enumerate()
            .map(|(i, &funds)| (self.factory)(i, funds))
            .collect();
        for (symbol, costs) in &self.scoped_costs {
            for sub in &mut self.subs {
                let _ = sub.set_costs_for(symbol.clone(), costs.clone());
            }
        }
        self.owners.clear();
        self.sub_to_pf.clear();
        self.pf_to_sub.clear();
        self.next_pf_id = 0;
        self.priced = false;
    }

    /// Translate a batch of fills out of sub-wallet `idx` into the
    /// portfolio-wide id space.
    ///
    /// **Consuming** (`remove`): a fill is *terminal* for its order, so the
    /// mapping has no further use and holding it would leak. Contrast
    /// [`translate_rejections`](Self::translate_rejections), which must not
    /// consume.
    ///
    /// A fill with no mapping keeps its sub-local id — that only happens for
    /// an order the portfolio never acked (see `register_ack`'s
    /// [`Ack::Filled`] arm), and there is no better id to give it.
    pub(super) fn translate_fills(&mut self, idx: usize, fills: Vec<Order<Sym>>) -> Vec<Order<Sym>> {
        fills
            .into_iter()
            .map(|mut fill| {
                if let Some(pf_id) = self.sub_to_pf.remove(&(idx, fill.id)) {
                    self.pf_to_sub.remove(&pf_id);
                    fill.id = pf_id;
                }
                fill
            })
            .collect()
    }

    /// Translate a batch of rejections out of sub-wallet `idx` into the
    /// portfolio-wide id space, registering an owner for any that the
    /// portfolio never saw an [`Ack`] for.
    ///
    /// Two things here are deliberate and load-bearing:
    ///
    /// **Non-consuming** (`get`, not `remove`): a rejection is *not*
    /// terminal. `PaperWallet::match_protective` books the refusal and
    /// leaves the bracket resting (`fill_at` only clears it on success), so
    /// the very same leg id can reject on bar N and *fill* on bar N+1.
    /// Consuming the mapping here would break that later fill's translation
    /// and the owning child would never learn its stop executed.
    ///
    /// **Mint-on-miss**: a submit-time refusal never went through
    /// [`register_ack`] at all — `SubWalletHandle::set` and friends use `?`,
    /// so the `Err` path returns before the ack is registered, and
    /// `PaperWallet::reject_submission` mints its own sub-local id. Without
    /// minting a portfolio id and recording the owner here, those rejections
    /// could never be routed to the child that caused them.
    pub(super) fn translate_rejections(
        &mut self,
        idx: usize,
        rejections: Vec<Rejection<Sym>>,
    ) -> Vec<Rejection<Sym>> {
        rejections
            .into_iter()
            .map(|mut rejection| {
                rejection.id = match self.sub_to_pf.get(&(idx, rejection.id)) {
                    Some(&pf_id) => pf_id,
                    None => {
                        let pf_id = self.mint_pf_id();
                        self.owners.insert(pf_id, idx);
                        pf_id
                    }
                };
                rejection
            })
            .collect()
    }

    /// Run the **cash phase** of a rebalance: for each child i, compute the
    /// signed equity delta `delta_i = target_equities[i] - equity_i`; every
    /// contributor (`delta_i < 0`) donates `min(|delta_i|, funds_i)` in cash
    /// via [`Wallet::adjust_funds`], and receivers (`delta_i > 0`) split the
    /// pot in proportion to `|delta_i|`.
    ///
    /// Returns a per-child vector of **residual shortfalls** — the amount of
    /// equity a contributor still holds above its target after donating what
    /// cash it could (`0.0` for receivers and for contributors whose full
    /// donation fit into cash on hand). The position phase reads this vector
    /// to decide which children need forced position downsizes to raise cash
    /// for the *next* rebalance cycle.
    ///
    /// Cash flow routes through the [`Wallet::adjust_funds`] trait method so
    /// this phase works with any wallet impl that supports programmatic cash
    /// adjustment (paper wallets always do; live-broker wallets may, if their
    /// venue exposes a deposit / withdrawal / sub-account transfer API). A
    /// wallet that returns [`WalletError::UnsupportedOperation`] gets its
    /// intended donation added to its shortfall instead — the position phase
    /// then handles the delta through
    /// [`set_position`](Wallet::set_position), which is universally supported.
    /// If a receiver's credit fails on that same error, the corresponding
    /// contributor debits are rolled back symmetrically (their pot re-adds
    /// to the receiver's shortfall) to keep total equity conserved.
    ///
    /// No fills, no blotter entries. Equity math on the receiver side lands
    /// atomically this bar when the underlying wallet supports the credit.
    ///
    /// # Panics
    /// Panics if `target_equities.len() != self.subs.len()`.
    pub(super) fn rebalance_cash_to(&mut self, target_equities: &[Real]) -> Vec<Real> {
        assert_eq!(
            target_equities.len(),
            self.subs.len(),
            "rebalance_cash_to: target_equities has {} entries but portfolio has {} children",
            target_equities.len(),
            self.subs.len(),
        );
        let n = self.subs.len();
        // Snapshot current equities and funds — read once so subsequent
        // `adjust_funds` mutations don't shift the deltas mid-loop.
        let equities: Vec<Real> = self.subs.iter().map(|w| w.equity().0).collect();
        let funds: Vec<Real> = self.subs.iter().map(|w| w.funds().0).collect();

        // Signed deltas: positive = receiver (wants gain), negative = contributor
        // (needs to shed). By conservation Σ target = Σ equity, so Σ delta = 0.
        let deltas: Vec<Real> = (0..n).map(|i| target_equities[i] - equities[i]).collect();

        // Cap each contributor's donation at its available cash. Any excess
        // over `funds` becomes a residual shortfall for the position phase.
        let mut donations = vec![0.0; n];
        let mut shortfalls = vec![0.0; n];
        for i in 0..n {
            if deltas[i] < 0.0 {
                let need = -deltas[i];
                let donation = need.min(funds[i]);
                donations[i] = donation;
                shortfalls[i] = need - donation;
            }
        }

        // Debit contributors first. A wallet that refuses the debit (returns
        // `UnsupportedOperation`) has its intended donation folded into its
        // own shortfall instead of the shared pot — that equity stays where
        // it is until the position phase raises it via `set_position`.
        let mut actual_donations = vec![0.0; n];
        for i in 0..n {
            if donations[i] > 0.0 {
                match self.subs[i].adjust_funds(-donations[i]) {
                    Ok(()) => actual_donations[i] = donations[i],
                    Err(_) => {
                        // Debit refused — the shortfall grows by the amount
                        // that couldn't be donated.
                        shortfalls[i] += donations[i];
                    }
                }
            }
        }
        let pot: Real = actual_donations.iter().sum();

        // Receivers' total demand (positive deltas). By conservation this
        // equals `Σ -delta_contributor`; when all contributors are fully
        // covered by their cash and every debit succeeded, `pot == demand`.
        // Cash-limited contributors OR debit refusals shrink the pot and each
        // receiver gets a proportional share of what was raised.
        let demand: Real = deltas.iter().filter(|&&d| d > 0.0).sum();
        let scale = if demand > 0.0 { pot / demand } else { 0.0 };

        // Credit receivers. If a receiver's wallet refuses the credit, roll
        // back the proportional pot back to contributors symmetrically —
        // total equity must stay conserved even under partial trait
        // support. A refunded contribution re-inflates that contributor's
        // shortfall so the position phase can still act on it.
        for (i, &delta) in deltas.iter().enumerate() {
            if delta > 0.0 && scale > 0.0 {
                let credit = delta * scale;
                if self.subs[i].adjust_funds(credit).is_err() {
                    // Refund pot fraction back to each contributor
                    // proportionally to their actual donation.
                    let total_actual: Real = actual_donations.iter().sum();
                    if total_actual > 0.0 {
                        let refund_scale = credit / total_actual;
                        for (j, &donation) in actual_donations.iter().enumerate() {
                            if donation > 0.0 {
                                let refund = donation * refund_scale;
                                // Best-effort re-credit — if the same
                                // wallet refuses the refund (which would be
                                // surprising for a wallet that just
                                // accepted a debit), the equity is stuck
                                // in limbo; log via shortfall so the
                                // position phase can compensate.
                                if self.subs[j].adjust_funds(refund).is_err() {
                                    shortfalls[j] += refund;
                                }
                            }
                        }
                    }
                }
            }
        }
        shortfalls
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
    /// The equity of the child at index `idx` — funds plus mark-to-market
    /// positions in *that* child's sub-wallet. Ordered by the child's
    /// `.add(...)` index on the builder.
    ///
    /// # Panics
    /// Panics if `idx` is out of range.
    pub fn sub_equity(&self, idx: usize) -> Reference {
        self.inner.lock().expect("portfolio lock poisoned").subs[idx].equity()
    }

}

impl<Sym: Clone + Eq + Hash> Wallet<Sym> for PortfolioWallet<Sym> {
    fn funds(&self) -> Reference {
        let inner = self.inner.lock().expect("portfolio lock poisoned");
        Reference(inner.subs.iter().map(|w| w.funds().0).sum())
    }

    fn position(&self, symbol: &Sym) -> Units<Sym> {
        let inner = self.inner.lock().expect("portfolio lock poisoned");
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
            .lock().expect("portfolio lock poisoned")
            .subs
            .iter()
            .find_map(|w| w.price(symbol))
    }

    fn equity(&self) -> Reference {
        let inner = self.inner.lock().expect("portfolio lock poisoned");
        Reference(inner.subs.iter().map(|w| w.equity().0).sum())
    }

    fn update(&mut self, symbol: Sym, candle: Candle) -> Vec<Order<Sym>> {
        let mut inner = self.inner.lock().expect("portfolio lock poisoned");
        // Record that this interior has been priced through its own
        // aggregate view — `Portfolio::trade` reads this to catch a driver
        // that was handed some *other* wallet.
        inner.priced = true;
        // Feed every sub the same bar so their pending queues flush, their
        // resting brackets trigger, and their mark-to-market updates. Then
        // translate each fill's sub-local id into the portfolio-wide id
        // space we've been reporting on Acks, so the driver can route
        // via `owners` in Portfolio::on_fill.
        let mut all = Vec::new();
        for i in 0..inner.subs.len() {
            // Collect into an owned Vec first: the mutable borrow of
            // `subs[i]` has to end before `translate_fills` touches `inner`.
            let fills = inner.subs[i].update(symbol.clone(), candle);
            all.extend(inner.translate_fills(i, fills));
        }
        all
    }

    /// Drain every sub-wallet's rejections into one portfolio-wide stream.
    ///
    /// Without this the driver's drain (`backtest::run`) always came back
    /// empty for a portfolio run: `RunReport::rejections` was permanently
    /// empty and no child ever saw its own `on_reject`, even though the
    /// sub-wallets were booking refusals the whole time.
    ///
    /// [`PaperWallet::take_rejections`] is a *cursor* drain, so refusals
    /// booked during this bar's [`update`](Wallet::update) are picked up by
    /// this bar's drain while the non-destructive `PaperWallet::rejections()`
    /// history stays intact.
    fn take_rejections(&mut self) -> Vec<Rejection<Sym>> {
        let mut inner = self.inner.lock().expect("portfolio lock poisoned");
        let mut all = Vec::new();
        for i in 0..inner.subs.len() {
            let batch = inner.subs[i].take_rejections();
            all.extend(inner.translate_rejections(i, batch));
        }
        all
    }

    /// Fan the out-of-band fill poll to every sub-wallet, translating ids the
    /// same way [`update`](Wallet::update) does.
    ///
    /// A no-op while the subs are [`PaperWallet`]s (which fill only through
    /// `update`), but a sub that reports fills asynchronously would otherwise
    /// have them silently dropped.
    fn poll_fills(&mut self) -> Vec<Order<Sym>> {
        let mut inner = self.inner.lock().expect("portfolio lock poisoned");
        let mut all = Vec::new();
        for i in 0..inner.subs.len() {
            let fills = inner.subs[i].poll_fills();
            all.extend(inner.translate_fills(i, fills));
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

    fn set_stop(
        &mut self,
        _symbol: Sym,
        _trigger: Reference,
        _size: Size,
    ) -> Result<Ack<Sym>, WalletError> {
        panic!(
            "PortfolioWallet::set_stop: the aggregate wallet is a reporting view; \
             child strategies trade through SubWalletHandle inside Portfolio::trade."
        );
    }

    fn set_take_profit(
        &mut self,
        _symbol: Sym,
        _trigger: Reference,
        _size: Size,
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
    inner: Arc<Mutex<PortfolioInner<Sym>>>,
    idx: usize,
}

impl<Sym> SubWalletHandle<Sym> {
    pub(super) fn new(inner: Arc<Mutex<PortfolioInner<Sym>>>, idx: usize) -> Self {
        Self { inner, idx }
    }
}

impl<Sym: Clone + Eq + Hash> Wallet<Sym> for SubWalletHandle<Sym> {
    fn funds(&self) -> Reference {
        self.inner.lock().expect("portfolio lock poisoned").subs[self.idx].funds()
    }

    fn position(&self, symbol: &Sym) -> Units<Sym> {
        self.inner.lock().expect("portfolio lock poisoned").subs[self.idx].position(symbol)
    }

    fn price(&self, symbol: &Sym) -> Option<Reference> {
        self.inner.lock().expect("portfolio lock poisoned").subs[self.idx].price(symbol)
    }

    fn equity(&self) -> Reference {
        self.inner.lock().expect("portfolio lock poisoned").subs[self.idx].equity()
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
        let mut inner = self.inner.lock().expect("portfolio lock poisoned");
        let ack = inner.subs[self.idx].set_position(target)?;
        Ok(inner.register_ack(self.idx, ack))
    }

    fn set(&mut self, symbol: Sym, side: Side, size: Size) -> Result<Ack<Sym>, WalletError> {
        let mut inner = self.inner.lock().expect("portfolio lock poisoned");
        let ack = inner.subs[self.idx].set(symbol, side, size)?;
        Ok(inner.register_ack(self.idx, ack))
    }

    fn close(&mut self, symbol: Sym) -> Result<Ack<Sym>, WalletError> {
        let mut inner = self.inner.lock().expect("portfolio lock poisoned");
        let ack = inner.subs[self.idx].close(symbol)?;
        Ok(inner.register_ack(self.idx, ack))
    }

    fn set_stop(
        &mut self,
        symbol: Sym,
        trigger: Reference,
        size: Size,
    ) -> Result<Ack<Sym>, WalletError> {
        let mut inner = self.inner.lock().expect("portfolio lock poisoned");
        let ack = inner.subs[self.idx].set_stop(symbol, trigger, size)?;
        Ok(inner.register_ack(self.idx, ack))
    }

    fn set_take_profit(
        &mut self,
        symbol: Sym,
        trigger: Reference,
        size: Size,
    ) -> Result<Ack<Sym>, WalletError> {
        let mut inner = self.inner.lock().expect("portfolio lock poisoned");
        let ack = inner.subs[self.idx].set_take_profit(symbol, trigger, size)?;
        Ok(inner.register_ack(self.idx, ack))
    }

    fn cancel_protective(&mut self, symbol: &Sym) -> Result<(), WalletError> {
        self.inner.lock().expect("portfolio lock poisoned").subs[self.idx].cancel_protective(symbol)
    }

    fn set_limit(
        &mut self,
        symbol: Sym,
        side: Side,
        size: Size,
        limit: Reference,
    ) -> Result<Ack<Sym>, WalletError> {
        let mut inner = self.inner.lock().expect("portfolio lock poisoned");
        let ack = inner.subs[self.idx].set_limit(symbol, side, size, limit)?;
        Ok(inner.register_ack(self.idx, ack))
    }

    fn cancel_limit(&mut self, symbol: &Sym) -> Result<(), WalletError> {
        self.inner.lock().expect("portfolio lock poisoned").subs[self.idx].cancel_limit(symbol)
    }

    fn adjust_funds(&mut self, delta: Real) -> Result<(), WalletError> {
        // Forwarded so a child behaves the same inside a portfolio as it does
        // standalone. It doesn't disturb rebalancing, which reads every
        // child's equity and funds fresh at each fire.
        self.inner.lock().expect("portfolio lock poisoned").subs[self.idx].adjust_funds(delta)
    }

    fn cancel(&mut self, id: OrderId) -> Result<(), WalletError> {
        // `cancel` is the one method that takes an id *inward*. The child
        // holds a portfolio-wide id (that's what its Ack carried), but the
        // sub-wallet mints from 0 and matches on the raw id — so forwarding
        // it untranslated would cancel some unrelated order in this sub.
        let mut inner = self.inner.lock().expect("portfolio lock poisoned");
        match inner.pf_to_sub.get(&id).copied() {
            // Ours: cancel the sub-local order the id really names.
            Some((idx, sub_id)) if idx == self.idx => inner.subs[idx].cancel(sub_id),
            // Unknown, already terminal, or another child's id. The trait's
            // post-condition — "that order is not working" — holds from this
            // child's point of view either way, and we must not touch a
            // sibling's book.
            _ => Ok(()),
        }
    }

    // `take_rejections` and `poll_fills` are deliberately left at their trait
    // defaults (empty). A handle is *not* a drain point: the driver drains
    // the outer `PortfolioWallet`, and `PaperWallet::take_rejections`
    // advances a cursor — so a child that helpfully drained its own handle
    // inside `trade` would consume those entries before the outer drain ran,
    // silently deleting them from `RunReport::rejections` *and* from its own
    // `on_reject`. Don't "complete" this impl.
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::wallet::OrderKind;

    /// A two-sub interior with no cash — enough to exercise the pure
    /// id-translation logic, which never touches the sub-wallets.
    fn inner() -> PortfolioInner<&'static str> {
        PortfolioInner::new(vec![0.0, 0.0], paper_sub_wallets(None))
    }

    fn order(id: u64) -> Order<&'static str> {
        Order::new(
            "A",
            Side::Buy,
            1.0,
            10.0,
            OrderKind::Market,
            OrderId(id),
        )
    }

    fn rejection(id: u64, kind: OrderKind) -> Rejection<&'static str> {
        Rejection {
            symbol: "A",
            id: OrderId(id),
            error: WalletError::InsufficientFunds,
            kind,
        }
    }

    #[test]
    fn pf_to_sub_round_trips_through_register_ack() {
        let mut inner = inner();
        // Both subs mint from 0, so the same sub-local id in two children
        // is exactly the collision the portfolio id space exists to fix.
        let a = inner.register_ack(0, Ack::Working(OrderId(0)));
        let b = inner.register_ack(1, Ack::Working(OrderId(0)));
        let (Ack::Working(a), Ack::Working(b)) = (a, b) else {
            panic!("expected Working acks");
        };
        assert_ne!(a, b, "distinct children must get distinct portfolio ids");
        assert_eq!(inner.pf_to_sub.get(&a), Some(&(0, OrderId(0))));
        assert_eq!(inner.pf_to_sub.get(&b), Some(&(1, OrderId(0))));
        assert_eq!(inner.owners.get(&a), Some(&0));
        assert_eq!(inner.owners.get(&b), Some(&1));
    }

    #[test]
    fn translate_fills_consumes_the_mapping() {
        let mut inner = inner();
        let Ack::Working(pf_id) = inner.register_ack(0, Ack::Working(OrderId(7))) else {
            panic!("expected a Working ack");
        };
        let out = inner.translate_fills(0, vec![order(7)]);
        assert_eq!(out[0].id, pf_id, "fill should carry the portfolio-wide id");
        // A fill is terminal, so both directions of the mapping are gone.
        assert!(inner.sub_to_pf.is_empty());
        assert!(inner.pf_to_sub.is_empty());
    }

    #[test]
    fn translate_rejections_is_non_destructive() {
        // A protective leg that can't be booked stays resting and retries
        // next bar, so the same id must still translate after a rejection.
        let mut inner = inner();
        let Ack::Working(pf_id) = inner.register_ack(0, Ack::Working(OrderId(3))) else {
            panic!("expected a Working ack");
        };
        let rejected = inner.translate_rejections(0, vec![rejection(3, OrderKind::Stop)]);
        assert_eq!(rejected[0].id, pf_id);
        // Mapping survives — the later fill on the same leg still routes.
        let filled = inner.translate_fills(0, vec![order(3)]);
        assert_eq!(filled[0].id, pf_id);
    }

    #[test]
    fn translate_rejections_mints_for_an_unacked_refusal() {
        // A submit-time refusal never reaches `register_ack` (the handle's
        // `?` returns first), so there is no mapping to find. It still has
        // to be attributable to the child that caused it.
        let mut inner = inner();
        let out = inner.translate_rejections(1, vec![rejection(0, OrderKind::Market)]);
        assert_eq!(
            inner.owners.get(&out[0].id),
            Some(&1),
            "a minted rejection id must record its owning child",
        );
    }

    #[test]
    fn translate_rejections_does_not_collide_across_children() {
        // Both subs refuse their own sub-local id 0; the two must not
        // collapse onto one portfolio id, or `on_reject` would misroute.
        let mut inner = inner();
        let first = inner.translate_rejections(0, vec![rejection(0, OrderKind::Market)]);
        let second = inner.translate_rejections(1, vec![rejection(0, OrderKind::Market)]);
        assert_ne!(first[0].id, second[0].id);
        assert_eq!(inner.owners.get(&first[0].id), Some(&0));
        assert_eq!(inner.owners.get(&second[0].id), Some(&1));
    }
}
