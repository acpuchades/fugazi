//! The netting layer: N notional `Ledger`s over one account, and the
//! arithmetic that keeps them in step.
//!
//! `PortfolioInner` is the interior a [`Portfolio`](super::Portfolio) shares
//! with its children (via `LedgerWallet`). It owns one ledger per child and a
//! per-symbol `marks` cache, but **not** the account: the account is the wallet
//! [`Portfolio::trade`](super::Portfolio) is handed by the driver, exactly like
//! every other strategy shape.
//!
//! # The identity everything rests on
//!
//! For every symbol, the sum of the children's ledger positions equals the
//! account's position; the sum of their ledger cash equals the account's cash.
//! Ledgers are never moved by intent, only by real fills, which is what keeps
//! that true — and `check_invariants`
//! asserts it in tests rather than trusting it.
//!
//! # One bar
//!
//! 1. The driver feeds the account the bar; the resulting fills reach
//!    [`Portfolio::on_fill`](super::Portfolio), which calls
//!    `attribute_fill` to move the ledgers
//!    that caused them.
//! 2. [`Portfolio::update`](super::Portfolio) refreshes `marks` from the
//!    snapshot and books any fully-crossed flow at that bar's open.
//! 3. Children trade into `LedgerWallet`s, recording intent, then
//!    `net_and_submit` turns every child's
//!    intent into one order per symbol on the passed wallet, and rests the most
//!    urgent protective leg.
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

use serde::Serialize;
use serde::de::DeserializeOwned;

use crate::types::Real;
use crate::wallet::{
    Ack, Order, OrderId, OrderKind, Reference, Rejection, Side, Size, Units, Wallet, WalletError,
};
use crate::wallet::{POSITION_EPSILON, cash_tolerance};

use super::ledger::{Intent, Ledger, ProtectiveIntent, rejection};

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

/// The interior a [`Portfolio`](super::Portfolio) shares with its children.
pub(super) struct PortfolioInner<Sym> {
    /// One notional book per child, in `add(...)` order.
    pub(super) ledgers: Vec<Ledger<Sym>>,
    /// Last-seen close per symbol, refreshed each bar from the snapshot. The
    /// account is priced by the driver; this is the price children size against
    /// and the marks the aggregate book uses, in place of the old substrate.
    pub(super) marks: HashMap<Sym, Real>,

    /// This bar's recorded intents, per child, cleared by `net_and_submit`.
    intents: Vec<HashMap<Sym, Intent>>,
    /// Each child's resting protective levels. Persist across bars — a child
    /// re-submits every bar, and only the most urgent reaches the account.
    protective: Vec<HashMap<Sym, ProtectiveIntent>>,
    /// Which child owns the leg currently rested on the account, per symbol,
    /// so a protective fill routes to the right ledger.
    protective_owner: HashMap<Sym, usize>,
    /// Submitted flow awaiting settlement, keyed by symbol.
    pending: HashMap<Sym, PendingFlow<Sym>>,

    /// Portfolio-wide id → owning child, for routing fills and refusals.
    pub(super) owners: HashMap<OrderId, usize>,
    next_pf_id: u64,
    /// Child hard-cap refusals booked here during `trade`, drained to the owning
    /// child by [`Portfolio::update`](super::Portfolio) on the next bar. They do
    /// not reach the run report (the account never saw them).
    rejections: Vec<Rejection<Sym>>,

    /// Per-child seed cash, kept for `reset` (rebuild ledgers).
    seeds: Vec<Real>,

    /// The account wallet's [`can_short`](Wallet::can_short), cached each bar by
    /// [`Portfolio::trade`](super::Portfolio) before the children run — a child
    /// only ever holds a `LedgerWallet`, which has no handle on the account, so
    /// the capability has to be carried across. `true` until the first bar
    /// (matching the trait default), so a child asking before any account has
    /// been seen gets the permissive answer rather than a spurious `false`.
    pub(super) account_can_short: bool,
}

// `snapshot`/`restore` are consumed only by `Portfolio::{save_state,restore_state}`,
// which the `spec`-gated `DynPortfolio` wrapper drives — unreachable when `spec`
// is off.
#[cfg_attr(not(feature = "spec"), allow(dead_code))]
impl<Sym: Clone + Eq + Hash + Serialize + DeserializeOwned> PortfolioInner<Sym> {
    /// Serialize the persistent, cross-bar portfolio state for run resuming: the
    /// per-child notional ledgers (cash + positions — the "Σ ledgers == account"
    /// invariant), the marks cache, and the id counter. The per-bar transient
    /// state (this bar's intents, the resting protective levels re-submitted
    /// every bar, and any last-bar flow still awaiting settlement) is not
    /// persisted — a portfolio should be resumed at a settled bar boundary.
    pub(super) fn snapshot(&self) -> serde_json::Value {
        serde_json::json!({
            "ledgers": self.ledgers,
            "marks": self.marks,
            "next_pf_id": self.next_pf_id,
        })
    }

    /// Restore state produced by [`snapshot`](Self::snapshot).
    pub(super) fn restore(&mut self, state: &serde_json::Value) -> Result<(), String> {
        let obj = state
            .as_object()
            .ok_or_else(|| format!("portfolio inner: expected a state object, got {state}"))?;
        if let Some(v) = obj.get("ledgers") {
            self.ledgers =
                serde_json::from_value(v.clone()).map_err(|e| format!("ledgers: {e}"))?;
        }
        if let Some(v) = obj.get("marks") {
            self.marks = serde_json::from_value(v.clone()).map_err(|e| format!("marks: {e}"))?;
        }
        if let Some(v) = obj.get("next_pf_id") {
            self.next_pf_id =
                serde_json::from_value(v.clone()).map_err(|e| format!("next_pf_id: {e}"))?;
        }
        Ok(())
    }
}

impl<Sym: Clone + Eq + Hash> PortfolioInner<Sym> {
    pub(super) fn new(seeds: Vec<Real>) -> Self {
        let n = seeds.len();
        Self {
            ledgers: seeds.iter().map(|&c| Ledger::new(c)).collect(),
            marks: HashMap::new(),
            intents: vec![HashMap::new(); n],
            protective: vec![HashMap::new(); n],
            protective_owner: HashMap::new(),
            pending: HashMap::new(),
            owners: HashMap::new(),
            next_pf_id: 0,
            rejections: Vec::new(),
            seeds,
            account_can_short: true,
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
        self.marks.get(symbol).copied()
    }

    /// Child `idx`'s mark-to-market equity.
    pub(super) fn child_equity(&self, idx: usize) -> Real {
        self.ledgers[idx].equity(|s| self.price_of(s))
    }

    /// Drain the child hard-cap refusals booked during `trade`, paired with the
    /// child that owns each. Dispatched to children by `Portfolio::update`; not
    /// surfaced to the run report.
    pub(super) fn take_child_rejections(&mut self) -> Vec<(usize, Rejection<Sym>)> {
        std::mem::take(&mut self.rejections)
            .into_iter()
            .map(|r| (self.owners.get(&r.id).copied().unwrap_or(0), r))
            .collect()
    }

    /// Reset every ledger to its seed and clear all bar-to-bar state. There is
    /// no substrate to rebuild — the driver hands a fresh account each run.
    pub(super) fn reset(&mut self) {
        self.ledgers = self.seeds.iter().map(|&c| Ledger::new(c)).collect();
        self.marks.clear();
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
        // Back to the permissive default: the next run's account re-caches it on
        // its first `trade`.
        self.account_can_short = true;
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
            let tolerance = cash_tolerance(cash);
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

    /// Turn every child's recorded intent into at most one order per symbol on
    /// the passed `wallet`, then rest the most urgent protective leg per symbol.
    pub(super) fn net_and_submit(&mut self, wallet: &mut dyn Wallet<Sym>) {
        let mut symbols: Vec<Sym> = Vec::new();
        for per_child in &self.intents {
            for symbol in per_child.keys() {
                if !symbols.contains(symbol) {
                    symbols.push(symbol.clone());
                }
            }
        }
        for symbol in symbols {
            self.submit_symbol(&symbol, wallet);
        }
        for m in &mut self.intents {
            m.clear();
        }
        self.rest_protective(wallet);
    }

    fn submit_symbol(&mut self, symbol: &Sym, wallet: &mut dyn Wallet<Sym>) {
        let mut legs: Vec<Leg> = Vec::new();
        for idx in 0..self.ledgers.len() {
            let Some(intent) = self.intents[idx].get(symbol).copied() else {
                continue;
            };
            let delta = intent.target - self.ledgers[idx].position(symbol);
            if delta.abs() > POSITION_EPSILON {
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

        if market_delta.abs() > POSITION_EPSILON {
            // One order for the imbalance. The rest crosses internally.
            let current = wallet.position(symbol).amount;
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
            let equity = wallet.equity().0;
            let price = self.price_of(symbol).unwrap_or(0.0);
            let submitted = if market_delta > 0.0 && amount > 0.0 && equity > 0.0 && price > 0.0 {
                wallet.set(symbol.clone(), Side::Buy, Size::value_frac(amount * price / equity))
            } else {
                wallet.set_position(Units {
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
    fn rest_protective(&mut self, wallet: &mut dyn Wallet<Sym>) {
        let mut symbols: Vec<Sym> = Vec::new();
        for per_child in &self.protective {
            for symbol in per_child.keys() {
                if !symbols.contains(symbol) {
                    symbols.push(symbol.clone());
                }
            }
        }
        for symbol in symbols {
            let net = wallet.position(&symbol).amount;
            if net.abs() <= POSITION_EPSILON {
                self.protective_owner.remove(&symbol);
                let _ = wallet.cancel_protective(&symbol);
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
                if own.abs() <= POSITION_EPSILON || (own > 0.0) != long {
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
                    let _ = wallet.cancel_protective(&symbol);
                    let _ = match kind {
                        OrderKind::TakeProfit => wallet.set_take_profit(
                            symbol.clone(),
                            Reference(level),
                            Size::units(units),
                        ),
                        _ => wallet.set_stop(symbol.clone(), Reference(level), Size::units(units)),
                    };
                }
                None => {
                    self.protective_owner.remove(&symbol);
                    let _ = wallet.cancel_protective(&symbol);
                }
            }
        }
    }

    // ---- settlement (attribution) ---------------------------------------

    /// Attribute one RAW account fill (from the driver's `wallet.update` /
    /// `poll_fills`) to the ledgers that caused it, returning one synthetic
    /// [`Order`] per child share for [`Portfolio::on_fill`](super::Portfolio) to
    /// dispatch to each owning child.
    ///
    /// A market fill splits pro-rata across its pending flow; a protective fill
    /// belongs wholly to the child whose leg was rested. The crossed portion of
    /// a partially-crossed flow books at the fill price here (≈ the bar open for
    /// a market fill) rather than the raw open the old substrate settle used —
    /// identical for a zero-cost paper fill, a negligible slippage-only
    /// difference otherwise.
    pub(super) fn attribute_fill(&mut self, fill: &Order<Sym>) -> Vec<Order<Sym>> {
        match fill.kind {
            OrderKind::Stop | OrderKind::TakeProfit => self
                .attribute_protective(&fill.symbol.clone(), fill)
                .into_iter()
                .collect(),
            _ => {
                let symbol = fill.symbol.clone();
                self.attribute_market(&symbol, fill, fill.price)
            }
        }
    }

    /// Book any flow that crossed entirely — net 0, so no order was submitted and
    /// no fill will ever arrive — at each symbol's `open`, returning the synthetic
    /// per-child orders. Called once per bar from
    /// [`Portfolio::update`](super::Portfolio), after the driver's fills have
    /// removed (via [`attribute_fill`](Self::attribute_fill)) every symbol that
    /// did reach the market. A flow that submitted an order and hasn't filled
    /// (nonzero `market_delta`) stays pending — still working.
    pub(super) fn book_crosses(&mut self, opens: &HashMap<Sym, Real>) -> Vec<Order<Sym>> {
        let crossed: Vec<Sym> = self
            .pending
            .iter()
            .filter(|(_, flow)| flow.market_delta.abs() <= POSITION_EPSILON)
            .map(|(sym, _)| sym.clone())
            .collect();
        let mut out = Vec::new();
        for symbol in crossed {
            let Some(flow) = self.pending.remove(&symbol) else {
                continue;
            };
            let open = opens
                .get(&symbol)
                .copied()
                .or_else(|| self.price_of(&symbol))
                .unwrap_or(0.0);
            out.extend(self.book(&flow, 1.0, open, open, 0.0));
        }
        out
    }

    /// Split one account refusal into per-child refusals for
    /// [`Portfolio::on_reject`](super::Portfolio) to dispatch. The account
    /// refuses a *netted* order, which belongs to whichever children contributed
    /// to it (its pending legs); a refused protective leg goes to the child whose
    /// leg was rested. An unattributable refusal yields nothing (the account-level
    /// entry is already in the run report).
    pub(super) fn attribute_rejection(
        &mut self,
        refusal: Rejection<Sym>,
    ) -> Vec<(usize, Rejection<Sym>)> {
        if let Some(flow) = self.pending.remove(&refusal.symbol) {
            flow.legs
                .iter()
                .map(|leg| {
                    (
                        self.owners.get(&leg.id).copied().unwrap_or(0),
                        Rejection {
                            symbol: refusal.symbol.clone(),
                            id: leg.id,
                            error: refusal.error,
                            kind: refusal.kind,
                        },
                    )
                })
                .collect()
        } else if let Some(&idx) = self.protective_owner.get(&refusal.symbol) {
            vec![(idx, refusal)]
        } else {
            Vec::new()
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
        let fraction = if flow.market_delta.abs() > POSITION_EPSILON {
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
        let market_share = if gross_majority > POSITION_EPSILON {
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
            if delta.abs() <= POSITION_EPSILON {
                continue;
            }
            let on_majority = (leg.delta > 0.0) == majority_is_buy;
            // The minority side is entirely crossed; the majority side splits.
            let market_part = if on_majority { delta * market_share } else { 0.0 };
            let crossed_part = delta - market_part;
            let comm = if market_units > POSITION_EPSILON {
                commission * (market_part.abs() * fraction) / (market_units * fraction).max(POSITION_EPSILON)
            } else {
                0.0
            };
            let cash_out = crossed_part * crossed_price + market_part * market_price;
            let ledger = &mut self.ledgers[leg.idx];
            let entry = ledger.positions.entry(flow.symbol.clone()).or_insert(0.0);
            *entry += delta;
            if entry.abs() <= POSITION_EPSILON {
                ledger.positions.remove(&flow.symbol);
            }
            ledger.cash -= cash_out + comm;

            // The price this child actually experienced, blended across its
            // crossed and market parts — what its own book should record.
            let effective = if delta.abs() > POSITION_EPSILON {
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
        if demand > POSITION_EPSILON {
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
    pub(super) fn check_invariants(&self, wallet: &dyn Wallet<Sym>) {
        let ledger_cash: Real = self.ledgers.iter().map(|l| l.cash).sum();
        let account_cash = wallet.funds().0;
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
            let account_units = wallet.position(&symbol).amount;
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
