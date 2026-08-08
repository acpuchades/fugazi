//! The netting layer: one real wallet, N notional [`Ledger`]s, and the
//! arithmetic that keeps them in step.
//!
//! [`PortfolioInner`] is the interior a [`Portfolio`](super::Portfolio) and its
//! [`PortfolioWallet`] share. It owns exactly one **substrate** wallet — a
//! `PaperWallet` in a backtest, a broker live — plus one ledger per child, and
//! it is the only thing in the crate that talks to the account.
//!
//! # The identity everything rests on
//!
//! For every symbol, the sum of the children's ledger positions equals the
//! substrate's position; the sum of their ledger cash equals the substrate's
//! cash. Ledgers are never moved by intent, only by real fills, which is what
//! keeps that true — and [`check_invariants`](PortfolioInner::check_invariants)
//! asserts it in tests rather than trusting it.
//!
//! # One bar
//!
//! 1. [`PortfolioWallet::update`] feeds the substrate the bar, then
//!    [`settle`](PortfolioInner::settle) attributes the resulting fills back to
//!    the ledgers that caused them.
//! 2. Children trade into [`LedgerWallet`]s, recording intent.
//! 3. [`net_and_submit`](PortfolioInner::net_and_submit) turns every child's
//!    intent into one order per symbol, and rests the most urgent protective
//!    leg.
//!
//! # Crossing
//!
//! When two children take opposite sides of a symbol on one bar, only the
//! imbalance reaches the market. The offsetting part is **crossed internally**:
//! both ledgers move as if they traded, but at the bar's open rather than the
//! fill price, and it carries no commission — it never touched the market. A
//! portfolio whose children frequently trade against each other therefore books
//! slightly lower costs than it would live, which is the documented price of
//! netting rather than grossing up.

use std::collections::HashMap;
use std::hash::Hash;

use crate::costs::TradingCosts;
use crate::types::{Candle, Real};
use crate::wallet::{
    Ack, Order, OrderId, OrderKind, PaperWallet, Reference, Rejection, Side, Size, Units,
    Wallet, WalletError,
};
use crate::indicators::DEFAULT_EPSILON;

use super::ledger::{Intent, Ledger, ProtectiveIntent, rejection};

/// Builds the account a [`Portfolio`](super::Portfolio) trades — the one real
/// wallet every child's flow is netted onto.
///
/// `funds` is the portfolio's total cash budget. A paper substrate is seeded
/// with it; a live substrate should ignore it, since the venue holds the real
/// balance.
///
/// # The account must be exclusively the portfolio's
///
/// The portfolio drives this wallet to the sum of its children's intents, so
/// anything else trading the same account — another portfolio, a manual order —
/// appears to the netting layer as an unexplained position and will be traded
/// back out.
pub type SubstrateFactory<Sym> = std::sync::Arc<dyn Fn(Real) -> Box<dyn Wallet<Sym> + Send> + Send + Sync>;

/// The default [`SubstrateFactory`]: an in-memory [`PaperWallet`], optionally
/// carrying a [`TradingCosts`] bundle.
pub(super) fn paper_substrate<Sym>(costs: Option<TradingCosts>) -> SubstrateFactory<Sym>
where
    Sym: Clone + Eq + Hash + Send + 'static,
{
    std::sync::Arc::new(move |funds| match &costs {
        Some(c) => Box::new(PaperWallet::with_costs(funds, c.clone())),
        None => Box::new(PaperWallet::new(funds)),
    })
}

/// One child's contribution to a symbol's flow this bar.
#[derive(Debug, Clone, Copy)]
struct Leg {
    idx: usize,
    /// Signed units this child wants to add to its ledger position.
    delta: Real,
    /// The portfolio-wide id the child was acked with.
    id: OrderId,
}

/// A symbol's netted flow, submitted and awaiting its fill.
#[derive(Debug, Clone)]
struct PendingFlow<Sym> {
    symbol: Sym,
    legs: Vec<Leg>,
    /// Net signed units sent to the market. Zero means the flow crossed
    /// entirely and no order was submitted, so it settles at the bar's open.
    market_delta: Real,
}

/// The interior shared by a [`Portfolio`](super::Portfolio) and its
/// [`PortfolioWallet`](super::PortfolioWallet).
pub(super) struct PortfolioInner<Sym> {
    /// The one real wallet. Everything else here is bookkeeping over it.
    pub(super) substrate: Box<dyn Wallet<Sym> + Send>,
    /// One notional book per child, in `add(...)` order.
    pub(super) ledgers: Vec<Ledger<Sym>>,

    /// This bar's recorded intents, per child, cleared by `net_and_submit`.
    intents: Vec<HashMap<Sym, Intent>>,
    /// Each child's resting protective levels. Persist across bars — a child
    /// re-submits every bar, and only the most urgent reaches the account.
    protective: Vec<HashMap<Sym, ProtectiveIntent>>,
    /// Which child owns the leg currently rested on the substrate, per symbol,
    /// so a protective fill routes to the right ledger.
    protective_owner: HashMap<Sym, usize>,
    /// Submitted flow awaiting settlement, keyed by symbol.
    pending: HashMap<Sym, PendingFlow<Sym>>,

    /// Portfolio-wide id → owning child, for routing fills and refusals.
    pub(super) owners: HashMap<OrderId, usize>,
    next_pf_id: u64,
    /// Refusals booked here, drained through the composite wallet.
    rejections: Vec<Rejection<Sym>>,

    /// How to rebuild the substrate on `reset`, and with what.
    factory: SubstrateFactory<Sym>,
    seeds: Vec<Real>,
    scoped_costs: Vec<(Sym, TradingCosts)>,

    /// Whether the composite wallet has ever been fed a bar — the mis-pairing
    /// guard read by [`Portfolio::trade`](super::Portfolio).
    pub(super) priced: bool,
}

impl<Sym: Clone + Eq + Hash> PortfolioInner<Sym> {
    pub(super) fn new(seeds: Vec<Real>, factory: SubstrateFactory<Sym>) -> Self {
        let total: Real = seeds.iter().sum();
        let n = seeds.len();
        Self {
            substrate: factory(total),
            ledgers: seeds.iter().map(|&c| Ledger::new(c)).collect(),
            intents: vec![HashMap::new(); n],
            protective: vec![HashMap::new(); n],
            protective_owner: HashMap::new(),
            pending: HashMap::new(),
            owners: HashMap::new(),
            next_pf_id: 0,
            rejections: Vec::new(),
            factory,
            seeds,
            scoped_costs: Vec::new(),
            priced: false,
        }
    }

    fn mint(&mut self) -> OrderId {
        let id = OrderId(self.next_pf_id);
        self.next_pf_id += 1;
        id
    }

    pub(super) fn child_count(&self) -> usize {
        self.ledgers.len()
    }

    pub(super) fn price_of(&self, symbol: &Sym) -> Option<Real> {
        self.substrate.price(symbol).map(|p| p.0)
    }

    /// Child `idx`'s mark-to-market equity.
    pub(super) fn child_equity(&self, idx: usize) -> Real {
        self.ledgers[idx].equity(|s| self.price_of(s))
    }

    pub(super) fn record_scoped_costs(&mut self, symbol: Sym, costs: TradingCosts) {
        if let Some(slot) = self.scoped_costs.iter_mut().find(|(s, _)| *s == symbol) {
            slot.1 = costs;
        } else {
            self.scoped_costs.push((symbol, costs));
        }
    }

    pub(super) fn take_rejections(&mut self) -> Vec<Rejection<Sym>> {
        std::mem::take(&mut self.rejections)
    }

    /// Rebuild the substrate at the portfolio's total seed and reset every
    /// ledger, then replay post-build per-symbol costs.
    pub(super) fn reset(&mut self) {
        let total: Real = self.seeds.iter().sum();
        self.substrate = (self.factory)(total);
        for (symbol, costs) in &self.scoped_costs {
            let _ = self.substrate.set_costs_for(symbol.clone(), costs.clone());
        }
        self.ledgers = self.seeds.iter().map(|&c| Ledger::new(c)).collect();
        for m in &mut self.intents {
            m.clear();
        }
        for m in &mut self.protective {
            m.clear();
        }
        self.protective_owner.clear();
        self.pending.clear();
        self.owners.clear();
        self.rejections.clear();
        self.next_pf_id = 0;
        self.priced = false;
    }

    // ---- intent recording (called from LedgerWallet) --------------------

    /// Record child `idx`'s desired ledger position in `symbol`.
    ///
    /// **Hard cap**: a net buy the child's ledger cash can't fund is refused
    /// here, exactly as a standalone `PaperWallet` would refuse it at
    /// submission, even when the account has idle cash belonging to a sibling.
    pub(super) fn record_intent(
        &mut self,
        idx: usize,
        symbol: Sym,
        target: Real,
    ) -> Result<Ack<Sym>, WalletError> {
        let Some(price) = self.price_of(&symbol) else {
            return Err(self.refuse(idx, symbol, WalletError::UnknownPrice));
        };
        if price <= 0.0 {
            return Err(self.refuse(idx, symbol, WalletError::InvalidPrice));
        }
        let delta = target - self.ledgers[idx].position(&symbol);
        let cash = self.ledgers[idx].cash;
        if delta > 0.0 {
            let cost = delta * price;
            let tolerance = DEFAULT_EPSILON * cash.abs().max(1.0);
            if cost - cash > tolerance {
                return Err(self.refuse(idx, symbol, WalletError::InsufficientFunds));
            }
        }
        let id = self.mint();
        self.owners.insert(id, idx);
        self.intents[idx].insert(symbol, Intent { target, id });
        Ok(Ack::Working(id))
    }

    /// Book a refusal against child `idx` and hand the error back.
    fn refuse(&mut self, idx: usize, symbol: Sym, error: WalletError) -> WalletError {
        let id = self.mint();
        self.owners.insert(id, idx);
        self.rejections.push(rejection(symbol, id, error));
        error
    }

    pub(super) fn record_protective(
        &mut self,
        idx: usize,
        symbol: Sym,
        stop: Option<Real>,
        take_profit: Option<Real>,
    ) -> Ack<Sym> {
        let id = self.mint();
        self.owners.insert(id, idx);
        let entry = self.protective[idx].entry(symbol).or_default();
        if stop.is_some() {
            entry.stop = stop;
        }
        if take_profit.is_some() {
            entry.take_profit = take_profit;
        }
        Ack::Working(id)
    }

    pub(super) fn clear_protective(&mut self, idx: usize, symbol: &Sym) {
        self.protective[idx].remove(symbol);
    }

    // ---- netting ---------------------------------------------------------

    /// Turn every child's recorded intent into at most one order per symbol,
    /// then rest the most urgent protective leg per symbol.
    pub(super) fn net_and_submit(&mut self) {
        let mut symbols: Vec<Sym> = Vec::new();
        for per_child in &self.intents {
            for symbol in per_child.keys() {
                if !symbols.contains(symbol) {
                    symbols.push(symbol.clone());
                }
            }
        }
        for symbol in symbols {
            self.submit_symbol(&symbol);
        }
        for m in &mut self.intents {
            m.clear();
        }
        self.rest_protective();
    }

    fn submit_symbol(&mut self, symbol: &Sym) {
        let mut legs: Vec<Leg> = Vec::new();
        for idx in 0..self.ledgers.len() {
            let Some(intent) = self.intents[idx].get(symbol).copied() else {
                continue;
            };
            let delta = intent.target - self.ledgers[idx].position(symbol);
            if delta.abs() > DEFAULT_EPSILON {
                legs.push(Leg {
                    idx,
                    delta,
                    id: intent.id,
                });
            }
        }
        if legs.is_empty() {
            return;
        }
        let market_delta: Real = legs.iter().map(|l| l.delta).sum();

        if market_delta.abs() > DEFAULT_EPSILON {
            // One order for the imbalance. The rest crosses internally.
            let current = self.substrate.position(symbol).amount;
            let amount = current + market_delta;
            // A net buy goes out as a *fraction of equity* rather than a unit
            // count, calibrated to hit `amount` at the current price. Two
            // things fall out of that, both of which a per-child wallet used
            // to give for free:
            //
            //   - It preserves the child's actual intent through a price move.
            //     `value_frac(1.0)` means "spend my cash", not "buy N units";
            //     resolving it a bar early would otherwise turn it into a unit
            //     count that costs more than the child has if price rises.
            //   - A fractional size is the one the wallet will `shrink_buy_to_fit`,
            //     so an order the account can no longer fully fund fills as far
            //     as the cash goes instead of being refused outright.
            //
            // Equity rather than funds is the basis on purpose: several
            // symbols are submitted in one bar and fill one after another, and
            // each fill spends cash. A funds-relative size calibrated before
            // the first fill would under-buy every symbol after it. Equity is
            // unchanged by a fill — cash simply becomes position — so every
            // symbol in the bar resolves against the same number.
            //
            // Sells and short targets need none of this, and an explicit unit
            // target is more predictable, so they take `set_position`.
            let equity = self.substrate.equity().0;
            let price = self.price_of(symbol).unwrap_or(0.0);
            let submitted = if market_delta > 0.0 && amount > 0.0 && equity > 0.0 && price > 0.0 {
                self.substrate
                    .set(symbol.clone(), Side::Buy, Size::value_frac(amount * price / equity))
            } else {
                self.substrate.set_position(Units {
                    symbol: symbol.clone(),
                    amount,
                })
            };
            match submitted {
                Err(error) => {
                    // The account refused the netted order, so nothing moves
                    // and every contributing child hears about it.
                    for leg in &legs {
                        self.rejections
                            .push(rejection(symbol.clone(), leg.id, error));
                    }
                    return;
                }
                Ok(Ack::Filled(order)) => {
                    // A venue that fills synchronously — `PaperWallet` never
                    // does, but the trait allows it. Attribute now, or the
                    // fill would never reach a ledger: there is no later
                    // update-stream entry for it.
                    let flow = PendingFlow {
                        symbol: symbol.clone(),
                        legs,
                        market_delta,
                    };
                    let filled = order.side.sign() * order.units;
                    let fraction = (filled / market_delta).clamp(0.0, 1.0);
                    self.book(&flow, fraction, order.price, order.price, order.commission);
                    return;
                }
                Ok(Ack::Working(_)) => {}
            }
        }
        self.pending.insert(
            symbol.clone(),
            PendingFlow {
                symbol: symbol.clone(),
                legs,
                market_delta,
            },
        );
    }

    /// Rest, per symbol, the child protective leg nearest to triggering, sized
    /// to that child's own position.
    ///
    /// The account holds one bracket per symbol, so when several children want
    /// stops on one symbol only the most urgent can be live. That is the one
    /// that would fire first anyway; if two would be hit on the same bar, the
    /// second fires a bar later. Choosing the *nearest* rather than adding
    /// multi-leg brackets to the wallet keeps the seam simple at the cost of
    /// that narrow case.
    fn rest_protective(&mut self) {
        let mut symbols: Vec<Sym> = Vec::new();
        for per_child in &self.protective {
            for symbol in per_child.keys() {
                if !symbols.contains(symbol) {
                    symbols.push(symbol.clone());
                }
            }
        }
        for symbol in symbols {
            let net = self.substrate.position(&symbol).amount;
            if net.abs() <= DEFAULT_EPSILON {
                self.protective_owner.remove(&symbol);
                let _ = self.substrate.cancel_protective(&symbol);
                continue;
            }
            let long = net > 0.0;
            // "Nearest" is the highest stop under a long position and the
            // lowest over a short one — in both cases the level the market
            // reaches first.
            let mut best: Option<(usize, Real, OrderKind, Real)> = None;
            for idx in 0..self.ledgers.len() {
                let own = self.ledgers[idx].position(&symbol);
                // A child on the opposite side of the net position can't be
                // protected by a reduce-only leg on it.
                if own.abs() <= DEFAULT_EPSILON || (own > 0.0) != long {
                    continue;
                }
                let Some(levels) = self.protective[idx].get(&symbol) else {
                    continue;
                };
                for (level, kind) in [
                    (levels.stop, OrderKind::Stop),
                    (levels.take_profit, OrderKind::TakeProfit),
                ] {
                    let Some(level) = level else { continue };
                    let better = match &best {
                        None => true,
                        Some((_, current, _, _)) => {
                            // Under a long, a stop is below and a target above;
                            // urgency is distance from the mark either way.
                            let mark = self.price_of(&symbol).unwrap_or(level);
                            (level - mark).abs() < (*current - mark).abs()
                        }
                    };
                    if better {
                        best = Some((idx, level, kind, own.abs()));
                    }
                }
            }
            match best {
                Some((idx, level, kind, units)) => {
                    self.protective_owner.insert(symbol.clone(), idx);
                    let _ = self.substrate.cancel_protective(&symbol);
                    let _ = match kind {
                        OrderKind::TakeProfit => self.substrate.set_take_profit(
                            symbol.clone(),
                            Reference(level),
                            Size::units(units),
                        ),
                        _ => self.substrate.set_stop(
                            symbol.clone(),
                            Reference(level),
                            Size::units(units),
                        ),
                    };
                }
                None => {
                    self.protective_owner.remove(&symbol);
                    let _ = self.substrate.cancel_protective(&symbol);
                }
            }
        }
    }

    // ---- settlement ------------------------------------------------------

    /// Feed the substrate a bar and attribute what filled back to the ledgers
    /// that caused it, returning one synthetic [`Order`] per child share.
    ///
    /// The driver sees per-child fills rather than the single netted one, which
    /// is what keeps `on_fill` routing and the run blotter reading as they did
    /// when every child had its own wallet.
    pub(super) fn settle(&mut self, symbol: Sym, candle: Candle) -> Vec<Order<Sym>> {
        self.priced = true;
        let fills = self.substrate.update(symbol.clone(), candle);
        // Before anything else: a refusal booked during that update kills its
        // flow, so translating first keeps a dead flow from being mistaken for
        // one that is still working.
        self.drain_substrate_rejections();
        let mut out = Vec::new();
        let mut market_filled = false;

        for fill in fills {
            match fill.kind {
                OrderKind::Stop | OrderKind::TakeProfit => {
                    if let Some(order) = self.attribute_protective(&symbol, &fill) {
                        out.push(order);
                    }
                }
                _ => {
                    market_filled = true;
                    out.extend(self.attribute_market(&symbol, &fill, candle.open));
                }
            }
        }

        // A flow that crossed entirely submitted no order, so no fill will
        // ever arrive to settle it — book it at the bar's open, which is the
        // price the market portion would have got.
        //
        // A flow that *did* submit an order and hasn't filled is simply still
        // working, and stays pending. `PaperWallet` fills or refuses on this
        // same update so it never lingers, but a live venue fills
        // asynchronously — dropping it here (or guessing it was refused) would
        // leave the eventual fill with nothing to attribute against.
        if !market_filled
            && let Some(flow) = self.pending.get(&symbol).cloned()
            && flow.market_delta.abs() <= DEFAULT_EPSILON
        {
            self.pending.remove(&symbol);
            out.extend(self.book(&flow, 1.0, candle.open, candle.open, 0.0));
        }
        out
    }

    /// Drain fills the venue reported **out of band** — booked between bars
    /// rather than on a specific `update` — and attribute them like any other.
    ///
    /// A no-op while the account is a [`PaperWallet`] (which fills only through
    /// `update`), and the difference between working and broken once it isn't:
    /// a live venue reports a fill on a symbol that didn't tick this bar
    /// through here and nowhere else.
    pub(super) fn poll(&mut self) -> Vec<Order<Sym>> {
        let fills = self.substrate.poll_fills();
        let mut out = Vec::new();
        for fill in fills {
            match fill.kind {
                OrderKind::Stop | OrderKind::TakeProfit => {
                    if let Some(order) = self.attribute_protective(&fill.symbol.clone(), &fill) {
                        out.push(order);
                    }
                }
                _ => {
                    // No candle here, so no bar open to settle a cross at —
                    // which is fine: a flow that crossed entirely never
                    // submitted an order, so it cannot appear in this stream.
                    let symbol = fill.symbol.clone();
                    out.extend(self.attribute_market(&symbol, &fill, fill.price));
                }
            }
        }
        self.drain_substrate_rejections();
        out
    }

    /// Translate refusals the account booked into per-child ones.
    ///
    /// The account refuses a *netted* order, which belongs to whichever
    /// children contributed to it — so the refusal is split back over that
    /// symbol's pending legs, carrying the venue's real error rather than a
    /// guess. A refused protective leg goes to the child whose leg was rested.
    /// Anything unattributable is passed through so it still reaches the run
    /// report.
    pub(super) fn drain_substrate_rejections(&mut self) {
        for refusal in self.substrate.take_rejections() {
            if let Some(flow) = self.pending.remove(&refusal.symbol) {
                for leg in &flow.legs {
                    self.rejections.push(Rejection {
                        symbol: refusal.symbol.clone(),
                        id: leg.id,
                        error: refusal.error,
                        kind: refusal.kind,
                    });
                }
            } else if let Some(&idx) = self.protective_owner.get(&refusal.symbol) {
                let id = self.mint();
                self.owners.insert(id, idx);
                self.rejections.push(Rejection { id, ..refusal });
            } else {
                self.rejections.push(refusal);
            }
        }
    }

    /// Split a netted market fill across the children whose flow produced it.
    fn attribute_market(&mut self, symbol: &Sym, fill: &Order<Sym>, open: Real) -> Vec<Order<Sym>> {
        let Some(flow) = self.pending.get(symbol).cloned() else {
            return Vec::new();
        };
        let signed = fill.side.sign() * fill.units;
        // Partial fills scale the whole bar's flow proportionally, which keeps
        // `Σ ledger delta == substrate delta` exactly.
        let fraction = if flow.market_delta.abs() > DEFAULT_EPSILON {
            (signed / flow.market_delta).clamp(0.0, 1.0)
        } else {
            1.0
        };
        // Whatever filled is the whole story for this bar: a market order does
        // not rest, so an under-fill (the account could only afford part of it)
        // is settled proportionally and the remainder simply expires. Children
        // re-decide next bar, which is what they would do standalone.
        self.pending.remove(symbol);
        self.book(&flow, fraction, open, fill.price, fill.commission)
    }

    /// Move ledgers for `fraction` of `flow`, splitting each child's share into
    /// the part that crossed internally (settled at `crossed_price`, free) and
    /// the part that reached the market (settled at `market_price`, carrying a
    /// pro-rata slice of `commission`).
    fn book(
        &mut self,
        flow: &PendingFlow<Sym>,
        fraction: Real,
        crossed_price: Real,
        market_price: Real,
        commission: Real,
    ) -> Vec<Order<Sym>> {
        let gross_buy: Real = flow.legs.iter().filter(|l| l.delta > 0.0).map(|l| l.delta).sum();
        let gross_sell: Real = flow
            .legs
            .iter()
            .filter(|l| l.delta < 0.0)
            .map(|l| -l.delta)
            .sum();
        let crossed = gross_buy.min(gross_sell);
        let majority_is_buy = gross_buy >= gross_sell;
        let gross_majority = if majority_is_buy { gross_buy } else { gross_sell };
        // Of the majority side's flow, this share reached the market.
        let market_share = if gross_majority > DEFAULT_EPSILON {
            (gross_majority - crossed) / gross_majority
        } else {
            0.0
        };
        let market_units: Real = flow
            .legs
            .iter()
            .filter(|l| (l.delta > 0.0) == majority_is_buy)
            .map(|l| l.delta.abs() * market_share)
            .sum();

        let mut out = Vec::new();
        for leg in &flow.legs {
            let delta = leg.delta * fraction;
            if delta.abs() <= DEFAULT_EPSILON {
                continue;
            }
            let on_majority = (leg.delta > 0.0) == majority_is_buy;
            // The minority side is entirely crossed; the majority side splits.
            let market_part = if on_majority { delta * market_share } else { 0.0 };
            let crossed_part = delta - market_part;
            let comm = if market_units > DEFAULT_EPSILON {
                commission * (market_part.abs() * fraction) / (market_units * fraction).max(DEFAULT_EPSILON)
            } else {
                0.0
            };
            let cash_out = crossed_part * crossed_price + market_part * market_price;
            let ledger = &mut self.ledgers[leg.idx];
            let entry = ledger.positions.entry(flow.symbol.clone()).or_insert(0.0);
            *entry += delta;
            if entry.abs() <= DEFAULT_EPSILON {
                ledger.positions.remove(&flow.symbol);
            }
            ledger.cash -= cash_out + comm;

            // The price this child actually experienced, blended across its
            // crossed and market parts — what its own book should record.
            let effective = if delta.abs() > DEFAULT_EPSILON {
                cash_out / delta
            } else {
                market_price
            };
            out.push(
                Order::new(
                    flow.symbol.clone(),
                    if delta > 0.0 { Side::Buy } else { Side::Sell },
                    delta.abs(),
                    effective,
                    OrderKind::Market,
                    leg.id,
                )
                .with_commission(comm),
            );
        }
        out
    }

    /// A protective fill belongs entirely to the child whose leg was rested.
    fn attribute_protective(&mut self, symbol: &Sym, fill: &Order<Sym>) -> Option<Order<Sym>> {
        let idx = self.protective_owner.remove(symbol)?;
        let delta = fill.side.sign() * fill.units;
        self.ledgers[idx].apply(symbol, delta, fill.price, fill.commission);
        // The leg is spent; the child re-submits next bar if it still wants one.
        self.protective[idx].remove(symbol);
        let id = self.mint();
        self.owners.insert(id, idx);
        Some(
            Order::new(
                symbol.clone(),
                fill.side,
                fill.units,
                fill.price,
                fill.kind,
                id,
            )
            .with_commission(fill.commission),
        )
    }

    // ---- rebalance -------------------------------------------------------

    /// Move notional cash between ledgers toward `target_equities`, returning
    /// each child's residual shortfall for the position phase.
    ///
    /// Free, and it cannot fail: the account balance never moves, only the
    /// notional split of it. That is the whole cash phase on a shared account —
    /// no `adjust_funds` on the substrate, no orders, no fills.
    pub(super) fn rebalance_ledgers_to(&mut self, target_equities: &[Real]) -> Vec<Real> {
        assert_eq!(
            target_equities.len(),
            self.ledgers.len(),
            "rebalance_ledgers_to: {} targets for {} children",
            target_equities.len(),
            self.ledgers.len(),
        );
        let n = self.ledgers.len();
        let equities: Vec<Real> = (0..n).map(|i| self.child_equity(i)).collect();
        let deltas: Vec<Real> = (0..n).map(|i| target_equities[i] - equities[i]).collect();

        let mut shortfalls = vec![0.0; n];
        let mut pot = 0.0;
        for (i, &delta) in deltas.iter().enumerate() {
            if delta < 0.0 {
                let need = -delta;
                let donation = need.min(self.ledgers[i].cash.max(0.0));
                self.ledgers[i].cash -= donation;
                pot += donation;
                shortfalls[i] = need - donation;
            }
        }
        let demand: Real = deltas.iter().filter(|&&d| d > 0.0).sum();
        if demand > DEFAULT_EPSILON {
            let scale = pot / demand;
            for (i, &delta) in deltas.iter().enumerate() {
                if delta > 0.0 {
                    self.ledgers[i].cash += delta * scale;
                }
            }
        }
        shortfalls
    }

    /// Debug/test hook: the identity the whole design rests on.
    ///
    /// # Panics
    /// Panics if the ledgers have drifted from the account.
    pub(super) fn check_invariants(&self) {
        let ledger_cash: Real = self.ledgers.iter().map(|l| l.cash).sum();
        let account_cash = self.substrate.funds().0;
        assert!(
            (ledger_cash - account_cash).abs() < 1e-6,
            "ledger cash {ledger_cash} != account cash {account_cash}",
        );
        let mut symbols: Vec<Sym> = Vec::new();
        for ledger in &self.ledgers {
            for symbol in ledger.positions.keys() {
                if !symbols.contains(symbol) {
                    symbols.push(symbol.clone());
                }
            }
        }
        for symbol in symbols {
            let ledger_units: Real = self.ledgers.iter().map(|l| l.position(&symbol)).sum();
            let account_units = self.substrate.position(&symbol).amount;
            assert!(
                (ledger_units - account_units).abs() < 1e-6,
                "ledger units {ledger_units} != account units {account_units}",
            );
        }
    }
}

/// Split `total_funds` into `n` allocations by `weights` (normalized to sum to
/// `1.0`). Used at build to seed each child's ledger.
pub(super) fn allocate_funds(total_funds: Real, weights: &[Real]) -> Vec<Real> {
    let sum: Real = weights.iter().sum();
    if sum <= 0.0 {
        let mut out = vec![0.0; weights.len()];
        if let Some(first) = out.first_mut() {
            *first = total_funds;
        }
        return out;
    }
    weights.iter().map(|w| total_funds * w / sum).collect()
}
