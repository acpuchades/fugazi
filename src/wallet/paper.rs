//! The in-memory paper broker: order queueing, fill simulation against the
//! next bar, protective-leg matching, and mark-to-market accounting.
//!
//! This is the bulk of the old `wallet.rs`. Nothing outside it needs
//! `Pending` / `Leg` / `Protective` / `FillPricing` / `RestingLimit`, which is
//! why they are private here rather than in [`super::types`].

use std::collections::HashMap;
use std::hash::Hash;

use serde::de::DeserializeOwned;
use serde::{Deserialize, Serialize};

use crate::costs::TradingCosts;
use crate::types::{Candle, Real};

use super::types::{
    Ack, POSITION_EPSILON, PRICE_EPSILON, Order, OrderId, OrderKind, Reference,
    Rejection, Side, Size, Units, WalletError, cash_tolerance,
};
use super::Wallet;

/// A market order queued on a [`PaperWallet`] to fill at the next bar's `open`.
///
/// The two market entry points differ only in *when* the target is known: an
/// absolute unit target is fixed at queue time, while a [`Side`] + [`Size`] is
/// resolved against the fill (`open`) price — so an all-in
/// [`value_frac(1.0)`](Size::value_frac) stays affordable even when the bar gaps.
/// Each carries the [`OrderId`] minted when it was submitted.
#[derive(Debug, Clone, Copy, Serialize, Deserialize)]
enum Pending {
    /// Drive to an absolute signed-unit target (from [`set_position`](Wallet::set_position)).
    Target(Real, OrderId),
    /// A side + size, resolved against the fill bar's `open` (from [`set`](Wallet::set)).
    Sized(Side, Size, OrderId),
}

/// One resting protective leg: the `trigger` level, **how much of the position
/// it takes off**, and the [`OrderId`] it fills under.
///
/// `size` resolves at the fill price and is clamped to the position's
/// magnitude, so a protective leg is always *reduce-only* — it can flatten a
/// position but never flip it. [`Size::position_frac(1.0)`](Size::position_frac)
/// is the whole-position exit every single-asset strategy wants; an explicit
/// [`Size::Units`] is what lets one account carry several owners' exits on the
/// same symbol.
#[derive(Debug, Clone, Copy, Serialize, Deserialize)]
struct Leg {
    trigger: Real,
    size: Size,
    id: OrderId,
}

/// The resting protective bracket for a symbol — a stop-loss and/or take-profit
/// leg. Holding both together makes them one-cancels-the-other: a fill on either
/// (or a market exit/reversal) drops the whole record.
#[derive(Debug, Clone, Copy, Default, Serialize, Deserialize)]
struct Protective {
    stop: Option<Leg>,
    take_profit: Option<Leg>,
}

/// Where and how a fill prices: the bar it lands on, the pre-cost price, and
/// the [`OrderKind`]. Travels together through the cost pipeline, so it is one
/// argument rather than three.
#[derive(Debug, Clone, Copy)]
struct FillPricing<'a> {
    bar: &'a Candle,
    price: Real,
    kind: OrderKind,
}

/// The half-spread a fill of `kind` crosses.
///
/// Zero for a [`Limit`](OrderKind::Limit): a resting limit provides liquidity
/// instead of taking it, so it crosses no spread. This is wallet policy rather
/// than a per-model decision — every spread model gives the same answer for a
/// passive fill — which is why [`SpreadModel::half_spread`] doesn't take the
/// kind. Together with the zero slippage multiplier `costs` gives `Limit`, it
/// is what keeps a limit fill from ever pricing worse than the caller's limit.
///
/// Shared by [`PaperWallet::fill_at`] and
/// [`shrink_buy_to_fit`](PaperWallet::shrink_buy_to_fit) so the price a buy is
/// sized against and the price it books at can't disagree.
fn half_spread_for(costs: &TradingCosts, kind: OrderKind, price: Real, bar: &Candle) -> Real {
    match kind {
        OrderKind::Limit => 0.0,
        _ => costs.spread.half_spread(price, bar),
    }
}

/// A resting limit order: a target `side · size` to be reached once the bar
/// trades through `limit`.
///
/// The [`Size`] is stored unresolved and resolved at the fill price, so an
/// all-in sizes against the equity at the moment the limit is actually hit
/// rather than at submission — the same reason [`Pending::Sized`] defers.
#[derive(Debug, Clone, Copy, Serialize, Deserialize)]
struct RestingLimit {
    side: Side,
    size: Size,
    limit: Real,
    id: OrderId,
}


/// The built-in **pure**, in-memory [`Wallet`]: a paper book of `funds`,
/// per-symbol positions, the prices fed to it, a queue of market orders awaiting
/// their next-open fill, the resting protective brackets, and a blotter of
/// executed [`Order`]s, with no IO.
///
/// The **market** movements ([`set_position`](Wallet::set_position) /
/// [`set`](Wallet::set) / [`close`](Wallet::close)) *queue*: they record the
/// intended move, return [`Ack::Working`], and the next [`update`](Wallet::update)
/// fills it at that bar's `open` (one queued move per symbol, latest wins) — which
/// keeps a backtest from filling on the same bar whose `close` triggered the
/// signal. The **resting** movements ([`set_stop`](Wallet::set_stop) /
/// [`set_take_profit`](Wallet::set_take_profit)) register a trigger level (one
/// bracket per symbol, latest wins); [`update`](Wallet::update) triggers and
/// prices them itself, filling at the level or — when the bar gaps past it — at
/// the `open`. Use it for backtests and dry runs; a downstream `Wallet` impl
/// handles live execution / bus publishing.
#[derive(Debug)]
pub struct PaperWallet<Sym> {
    positions: HashMap<Sym, Real>,
    bars: HashMap<Sym, Candle>,
    pending: HashMap<Sym, Pending>,
    protective: HashMap<Sym, Protective>,
    /// One resting limit order per symbol, latest-wins — the same convention
    /// `pending` and `protective` use.
    limits: HashMap<Sym, RestingLimit>,
    funds: Real,
    initial_funds: Real,
    blotter: Vec<Order<Sym>>,
    rejections: Vec<Rejection<Sym>>,
    /// How many of `rejections` have already been yielded by
    /// [`take_rejections`](Wallet::take_rejections). The vec is never truncated,
    /// so [`rejections`](Self::rejections) keeps reporting the full run history
    /// while the drain still yields each entry exactly once.
    rejections_drained: usize,
    next_id: u64,
    costs: TradingCosts,
    per_symbol_costs: HashMap<Sym, TradingCosts>,
}

impl<Sym> PaperWallet<Sym> {
    /// A wallet seeded with `funds` of cash, no positions and no prices, and
    /// **no trading costs** — every fill books at the theoretical price with
    /// zero commission, matching the pre-costs release. Byte-identical to the
    /// pre-costs behavior on any driver.
    pub fn new(funds: Real) -> Self {
        Self::with_costs(funds, TradingCosts::none())
    }

    /// A wallet seeded with `funds` of cash and the given `costs` model — every
    /// fill goes through the spread → slippage → commission pipeline
    /// documented on [`crate::costs`]. Pass [`TradingCosts::none`] for a
    /// zero-cost wallet (equivalent to [`new`](Self::new)).
    pub fn with_costs(funds: Real, costs: TradingCosts) -> Self {
        Self {
            positions: HashMap::new(),
            bars: HashMap::new(),
            pending: HashMap::new(),
            protective: HashMap::new(),
            limits: HashMap::new(),
            funds,
            initial_funds: funds,
            blotter: Vec::new(),
            rejections: Vec::new(),
            rejections_drained: 0,
            next_id: 0,
            costs,
            per_symbol_costs: HashMap::new(),
        }
    }

    /// Every order executed so far, in order (the trade blotter).
    pub fn orders(&self) -> &[Order<Sym>] {
        &self.blotter
    }

    /// Every queued order [`update`](Wallet::update) tried and failed to fill,
    /// in submission order. Populated by any [`WalletError`] the flush hit —
    /// typically `InsufficientFunds` on a [`Size::Units`] buy larger than
    /// cash on hand (fractional sizings shrink to fit and never end up here),
    /// or `InvalidPrice` on a zero-opening bar. Lets a driver report why a
    /// bar produced no fill instead of the silent drop the pre-fix wallet
    /// left callers with.
    pub fn rejections(&self) -> &[Rejection<Sym>] {
        &self.rejections
    }

    /// Restore the wallet to its freshly-constructed state — the seed `funds`
    /// it was built with, no positions, no fed prices, no pending or resting
    /// orders, and an empty blotter. Lets one wallet drive successive runs.
    pub fn reset(&mut self) {
        self.positions.clear();
        self.bars.clear();
        self.pending.clear();
        self.protective.clear();
        self.limits.clear();
        self.blotter.clear();
        self.rejections.clear();
        self.rejections_drained = 0;
        self.funds = self.initial_funds;
        self.next_id = 0;
    }

    /// Mint the next unique [`OrderId`].
    fn mint(&mut self) -> OrderId {
        let id = OrderId(self.next_id);
        self.next_id += 1;
        id
    }
}

/// The serializable slice of a [`PaperWallet`]'s state, for run resuming.
///
/// Everything the wallet needs to continue a run except the cost models, which
/// are configuration re-primed from the caller (the CLI's `--costs`, the
/// strategy spec) rather than persisted — a venue owns its own fees.
#[derive(Serialize, Deserialize)]
#[serde(bound(
    serialize = "Sym: Serialize + Eq + Hash",
    deserialize = "Sym: Deserialize<'de> + Eq + Hash"
))]
struct WalletSnapshot<Sym> {
    positions: HashMap<Sym, Real>,
    bars: HashMap<Sym, Candle>,
    pending: HashMap<Sym, Pending>,
    protective: HashMap<Sym, Protective>,
    limits: HashMap<Sym, RestingLimit>,
    funds: Real,
    initial_funds: Real,
    blotter: Vec<Order<Sym>>,
    rejections: Vec<Rejection<Sym>>,
    rejections_drained: usize,
    next_id: u64,
}

impl<Sym: Clone + Eq + Hash + Serialize + DeserializeOwned> PaperWallet<Sym> {
    /// Serialize the wallet's resumable state — cash, positions, fed prices,
    /// queued and resting orders, the blotter, and the id counter. The cost
    /// models are deliberately excluded (see `WalletSnapshot`); a resumed run
    /// re-primes them from the caller.
    pub fn snapshot_state(&self) -> serde_json::Value {
        let snapshot = WalletSnapshot {
            positions: self.positions.clone(),
            bars: self.bars.clone(),
            pending: self.pending.clone(),
            protective: self.protective.clone(),
            limits: self.limits.clone(),
            funds: self.funds,
            initial_funds: self.initial_funds,
            blotter: self.blotter.clone(),
            rejections: self.rejections.clone(),
            rejections_drained: self.rejections_drained,
            next_id: self.next_id,
        };
        serde_json::to_value(&snapshot).expect("WalletSnapshot is serializable")
    }

    /// Restore state produced by [`snapshot_state`](Self::snapshot_state). Leaves
    /// the cost models untouched — they were set by the freshly-constructed
    /// wallet (via `--costs` / the spec) before this call.
    pub fn restore_state(&mut self, state: &serde_json::Value) -> Result<(), String> {
        let snapshot: WalletSnapshot<Sym> =
            serde_json::from_value(state.clone()).map_err(|e| format!("wallet: {e}"))?;
        self.positions = snapshot.positions;
        self.bars = snapshot.bars;
        self.pending = snapshot.pending;
        self.protective = snapshot.protective;
        self.limits = snapshot.limits;
        self.funds = snapshot.funds;
        self.initial_funds = snapshot.initial_funds;
        self.blotter = snapshot.blotter;
        self.rejections = snapshot.rejections;
        self.rejections_drained = snapshot.rejections_drained;
        self.next_id = snapshot.next_id;
        Ok(())
    }
}

impl<Sym: Clone + Eq + Hash> PaperWallet<Sym> {
    /// Book a fill: drive `symbol` to `target` signed units, using
    /// `theoretical_price` as the pre-cost trigger price (bar `open` for a
    /// market order, the trigger level — or the `open` on a gap — for a stop /
    /// take-profit). The wallet's [`TradingCosts`] pipeline then applies
    /// **spread → slippage → commission**, and the final price is what lands on
    /// the [`Order`]. `kind`/`id` tag the resulting fill.
    ///
    /// The engine behind every fill — the queued market flush and the
    /// resting-order triggers both route here. Returns the [`Order`] (also
    /// pushed to the blotter), `Ok(None)` if already at `target`, or a
    /// [`WalletError`] (`UnknownPrice`, `InvalidPrice`, `PriceOutOfRange`,
    /// `InsufficientFunds`).
    fn fill_at(
        &mut self,
        symbol: Sym,
        target: Real,
        theoretical_price: Real,
        kind: OrderKind,
        id: OrderId,
    ) -> Result<Option<Order<Sym>>, WalletError> {
        let current = self.positions.get(&symbol).copied().unwrap_or(0.0);
        let delta = target - current;
        if delta.abs() <= POSITION_EPSILON {
            return Ok(None);
        }
        let bar = *self.bars.get(&symbol).ok_or(WalletError::UnknownPrice)?;
        if theoretical_price <= 0.0 {
            return Err(WalletError::InvalidPrice);
        }
        // The pre-cost price must be one the bar actually traded at; cost
        // adjustments (spread, slippage) may push the *final* fill price
        // outside the bar's range and that is fine — a real market fill can
        // execute above the tape.
        if theoretical_price < bar.low - PRICE_EPSILON
            || theoretical_price > bar.high + PRICE_EPSILON
        {
            return Err(WalletError::PriceOutOfRange);
        }

        // Apply the costs pipeline: spread → slippage → commission. Direction
        // is derived from `delta`'s sign (buys pay the ask, sells receive the
        // bid), and the fill kind threads through so a stop can slip further
        // than a plain market fill. Per-symbol overrides installed via
        // [`set_costs_for`](Self::set_costs_for) win over the default bundle.
        let side = if delta > 0.0 { Side::Buy } else { Side::Sell };
        let units = delta.abs();
        let costs = self
            .per_symbol_costs
            .get(&symbol)
            .unwrap_or(&self.costs);
        let half_spread = half_spread_for(costs, kind, theoretical_price, &bar);
        let post_spread = match side {
            Side::Buy => theoretical_price + half_spread,
            Side::Sell => theoretical_price - half_spread,
        };
        let final_price = costs.slippage.adjust(side, post_spread, units, &bar, kind);
        // A pathological cost config could drive the fill non-positive; refuse
        // rather than book a negative-value trade.
        if final_price <= 0.0 {
            return Err(WalletError::InvalidPrice);
        }
        let notional = final_price * units;
        let commission = costs.commission.commission(notional, units).max(0.0);

        // No margin: a net buy plus its commission can't drive cash below zero
        // (tolerant of the epsilon rounding in an all-in `value_frac(1.0)`,
        // whose cost equals funds when zero-cost).
        if delta > 0.0 {
            let cost = delta * final_price + commission;
            let tolerance = cash_tolerance(self.funds);
            if cost - self.funds > tolerance {
                return Err(WalletError::InsufficientFunds);
            }
        }
        let order = Order::from_delta(symbol.clone(), delta, final_price, kind, id)
            .expect("delta exceeds POSITION_EPSILON, so the order is non-empty")
            .with_commission(commission);
        // Pay for a buy, receive for a sell — and pay commission out of cash
        // on both sides.
        self.funds -= order.signed_units() * final_price + commission;
        let new_position = current + delta;
        if new_position.abs() <= POSITION_EPSILON {
            self.positions.remove(&symbol);
        } else {
            self.positions.insert(symbol.clone(), new_position);
        }
        // A fill that flattens or flips the sign voids any resting bracket (so a
        // bare market exit / reversal drops a now-stale stop even without an
        // explicit cancel).
        if new_position.abs() <= POSITION_EPSILON || current * new_position < 0.0 {
            self.protective.remove(&symbol);
        }
        self.blotter.push(order.clone());
        Ok(Some(order))
    }

    /// Shrink a resolved [`Size`] magnitude so a net buy fits within available
    /// cash *after* spread, slippage and commission. Only fractional sizings
    /// (`ValueFraction` / `FundsFraction`) hit the funds ceiling this covers —
    /// [`Size::Units`] is a caller-explicit unit count that should fail loudly
    /// if it doesn't fit rather than silently truncate, and a sell always
    /// credits cash. Returns the input magnitude unchanged on any of those.
    ///
    /// The cost pipeline is opaque behind [`CommissionModel`] / [`SpreadModel`]
    /// / [`SlippageModel`] so the shrink is a fixed-point iteration rather
    /// than a closed-form invert: probe the cost at the current magnitude,
    /// scale down by the deficit ratio, repeat. Converges in one step for
    /// linear cost shapes (`PercentageCommission`, `FixedBpsSpread`), quickly
    /// for the others; an 8-iteration cap keeps a pathological composite
    /// bounded.
    /// Shrink a fractional buy so spread + slippage + commission fit available
    /// cash, at `price` for a fill of `kind`.
    ///
    /// `price` and `kind` are parameters rather than `candle.open` /
    /// `OrderKind::Market` because a resting limit fills at its own price: sized
    /// against the open, an all-in limit buy below the market would shrink to
    /// the units the *open* could afford rather than the (larger) number its
    /// own cheaper fill can.
    fn shrink_buy_to_fit(
        &self,
        symbol: &Sym,
        side: Side,
        current: Real,
        magnitude: Real,
        pricing: FillPricing<'_>,
    ) -> Real {
        let FillPricing { bar: candle, price, kind } = pricing;
        if magnitude <= 0.0 {
            return magnitude;
        }
        // A sell (delta < 0) credits cash and always fits.
        let target = side.sign() * magnitude;
        if target - current <= 0.0 {
            return magnitude;
        }
        let costs = self.per_symbol_costs.get(symbol).unwrap_or(&self.costs);
        let tolerance = cash_tolerance(self.funds);
        let mut m = magnitude;
        for _ in 0..8 {
            let delta = side.sign() * m - current;
            if delta <= 0.0 {
                return m.max(0.0);
            }
            let half_spread = half_spread_for(costs, kind, price, candle);
            let post_spread = price + half_spread; // net buy
            let final_price =
                costs.slippage.adjust(Side::Buy, post_spread, delta, candle, kind);
            if final_price <= 0.0 {
                return 0.0;
            }
            let notional = final_price * delta;
            let commission = costs.commission.commission(notional, delta).max(0.0);
            let cost = notional + commission;
            if cost - self.funds <= tolerance {
                return m;
            }
            // Scale toward feasibility. For a linear cost model this converges
            // in one step; for a non-linear one it monotonically decreases.
            let scale = (self.funds / cost).clamp(0.0, 1.0);
            let next = m * scale;
            if (m - next).abs() <= cash_tolerance(m) {
                return next.max(0.0);
            }
            m = next;
        }
        m.max(0.0)
    }

    /// Pre-flight a market submission against the last close: reject
    /// synchronously when the symbol has never been priced, its close is
    /// non-positive, or a net-buy `delta` clearly can't be paid for out of
    /// cash on hand at that price.
    ///
    /// Used by [`Wallet::set_position`](Wallet::set_position) — the
    /// unit-explicit market path — and mirrors what a live venue does with an
    /// unfillable order. The affordability check is *approximate* (uses
    /// last-close as a proxy for the actual fill price at next open, and
    /// ignores costs), so a submission that just clears here can still be
    /// dropped into the rejections log at fill time if the open gaps
    /// meaningfully higher than the close.
    fn preflight_market(&self, symbol: &Sym, delta: Real) -> Result<(), WalletError> {
        let close = self.price(symbol).ok_or(WalletError::UnknownPrice)?.0;
        if close <= 0.0 {
            return Err(WalletError::InvalidPrice);
        }
        self.check_buy_affordability(delta, close)
    }

    /// Reject a net-buy `delta` whose notional at `price` clearly exceeds
    /// cash on hand. Sells and no-ops always clear. Uses the same
    /// funds-plus-tolerance comparison [`fill_at`](Self::fill_at) does at
    /// fill time so a submission passing this is virtually certain to fill
    /// (barring a big gap in the open).
    /// Book a **submission-time** refusal and hand the error back.
    ///
    /// The pre-flight paths on [`set`](Wallet::set) /
    /// [`set_position`](Wallet::set_position) return `Err` synchronously, which
    /// a [`Strategy`](crate::Strategy) has no way to report — its `trade`
    /// returns `()`, so the caller's only option is `let _ = ...`. Recording the
    /// refusal here puts it on the same failure stream as a fill-time drop, so
    /// the driver surfaces both through
    /// [`take_rejections`](Wallet::take_rejections) regardless of *when* the
    /// order died. An id is minted for it so it correlates like any other
    /// submission, even though no order was ever queued.
    fn reject_submission(&mut self, symbol: &Sym, error: WalletError) -> WalletError {
        let id = self.mint();
        self.rejections.push(Rejection {
            symbol: symbol.clone(),
            id,
            error,
            kind: OrderKind::Market,
        });
        error
    }

    fn check_buy_affordability(&self, delta: Real, price: Real) -> Result<(), WalletError> {
        if delta <= 0.0 {
            return Ok(());
        }
        let cost = delta * price;
        let tolerance = cash_tolerance(self.funds);
        if cost - self.funds > tolerance {
            return Err(WalletError::InsufficientFunds);
        }
        Ok(())
    }

    /// Trigger and fill a resting protective leg on `symbol` against `candle`, if
    /// one is crossed. Stop-loss takes precedence over take-profit, and at most one
    /// leg fills per bar (the fill flattens, which drops the whole bracket).
    /// Fill any resting limit on `symbol` that this bar traded through.
    ///
    /// A buy triggers when `low` reaches the limit and fills at
    /// `min(limit, open)`; a sell triggers on `high` and fills at
    /// `max(limit, open)`. Both spellings say the same thing: **at the limit
    /// or better, never worse** — a bar that gapped past the limit hands the
    /// caller the better `open` rather than their stale price.
    ///
    /// The [`Size`] resolves here, at the fill price, so an all-in sizes
    /// against the equity when the limit is hit. Equity marks this symbol at
    /// the fill price rather than the bar's `close` for the same reason
    /// [`Pending::Sized`] marks at the `open`: sizing must not see information
    /// from later in the bar than the fill.
    ///
    /// The order is consumed whether or not it books — a limit that triggers
    /// but can't be afforded is a rejection, not something to silently retry
    /// next bar at a price the market has already left behind.
    fn match_limit(&mut self, symbol: &Sym, candle: &Candle) -> Option<Order<Sym>> {
        let resting = *self.limits.get(symbol)?;
        let fill = match resting.side {
            Side::Buy if candle.low <= resting.limit + POSITION_EPSILON => {
                resting.limit.min(candle.open)
            }
            Side::Sell if candle.high >= resting.limit - POSITION_EPSILON => {
                resting.limit.max(candle.open)
            }
            _ => return None,
        };
        self.limits.remove(symbol);

        let position = self.positions.get(symbol).copied().unwrap_or(0.0);
        let equity_at_fill = self.funds
            + self
                .positions
                .iter()
                .map(|(s, &a)| {
                    let mark = if s == symbol {
                        fill
                    } else {
                        self.bars.get(s).map_or(0.0, |c| c.close)
                    };
                    a * mark
                })
                .sum::<Real>();
        let magnitude = resting
            .size
            .resolve(fill, position, self.funds, equity_at_fill);
        // Same rule as the queued-market path: a fractional sizing means "as
        // much as fits", so shrink it to leave room for commission; an explicit
        // unit count is a specific intent and is left alone.
        let magnitude = match resting.size {
            Size::ValueFraction(_) | Size::FundsFraction(_) => self.shrink_buy_to_fit(
                symbol,
                resting.side,
                position,
                magnitude,
                FillPricing {
                    bar: candle,
                    price: fill,
                    kind: OrderKind::Limit,
                },
            ),
            Size::Units(_) | Size::PositionFraction(_) => magnitude,
        };
        let target = resting.side.sign() * magnitude;

        match self.fill_at(symbol.clone(), target, fill, OrderKind::Limit, resting.id) {
            Ok(order) => order,
            Err(error) => {
                self.rejections.push(Rejection {
                    symbol: symbol.clone(),
                    id: resting.id,
                    error,
                    kind: OrderKind::Limit,
                });
                None
            }
        }
    }

    fn match_protective(&mut self, symbol: &Sym, candle: &Candle) -> Option<Order<Sym>> {
        let pos = self.positions.get(symbol).copied().unwrap_or(0.0);
        let prot = *self.protective.get(symbol)?;
        // Downside exits (long stop, short target) fill at the level, or lower at
        // the open on a gap: `min(level, open)`. Upside exits are `max`. Either way
        // the fill stays within the bar's range.
        let (leg, fill, kind) = if pos > POSITION_EPSILON {
            if let Some(leg) = prot.stop
                && candle.low <= leg.trigger + POSITION_EPSILON
            {
                (leg, leg.trigger.min(candle.open), OrderKind::Stop)
            } else if let Some(leg) = prot.take_profit
                && candle.high >= leg.trigger - POSITION_EPSILON
            {
                (leg, leg.trigger.max(candle.open), OrderKind::TakeProfit)
            } else {
                return None;
            }
        } else if pos < -POSITION_EPSILON {
            if let Some(leg) = prot.stop
                && candle.high >= leg.trigger - POSITION_EPSILON
            {
                (leg, leg.trigger.max(candle.open), OrderKind::Stop)
            } else if let Some(leg) = prot.take_profit
                && candle.low <= leg.trigger + POSITION_EPSILON
            {
                (leg, leg.trigger.min(candle.open), OrderKind::TakeProfit)
            } else {
                return None;
            }
        } else {
            return None;
        };
        // A protective leg that triggers but cannot be booked is the worst
        // silent failure in the wallet: the strategy believes its stop is
        // protecting it, and the bracket stays resting (`fill_at` only clears it
        // on success) so it retries next bar — but without this nobody is ever
        // told the exit did not happen.
        // Reduce-only: resolve the leg's size at the fill price, clamp it to the
        // position's magnitude, and step *toward* zero. `position_frac(1.0)` —
        // what every whole-position exit passes — resolves to `|pos|` and so
        // flattens, exactly as an unsized leg used to.
        let magnitude = leg
            .size
            .resolve(fill, pos, self.funds, self.equity().0)
            .min(pos.abs());
        let target = pos - pos.signum() * magnitude;
        match self.fill_at(symbol.clone(), target, fill, kind, leg.id) {
            Ok(order) => order,
            Err(error) => {
                self.rejections.push(Rejection {
                    symbol: symbol.clone(),
                    id: leg.id,
                    error,
                    kind,
                });
                None
            }
        }
    }
}

impl<Sym: Clone + Eq + Hash> Wallet<Sym> for PaperWallet<Sym> {
    fn funds(&self) -> Reference {
        Reference(self.funds)
    }

    fn position(&self, symbol: &Sym) -> Units<Sym> {
        Units {
            symbol: symbol.clone(),
            amount: self.positions.get(symbol).copied().unwrap_or(0.0),
        }
    }

    fn positions(&self) -> Vec<Units<Sym>> {
        self.positions
            .iter()
            .map(|(symbol, &amount)| Units {
                symbol: symbol.clone(),
                amount,
            })
            .collect()
    }

    fn set_costs_for(&mut self, symbol: Sym, costs: TradingCosts) -> Result<(), WalletError> {
        self.per_symbol_costs.insert(symbol, costs);
        Ok(())
    }

    /// `true` — the paper account has no spot restriction: a sell credits cash,
    /// so a position may go as negative as the strategy asks. Stated explicitly
    /// (rather than left to the trait default) because this is the reference
    /// answer every backtest reads.
    fn can_short(&self) -> bool {
        true
    }

    fn price(&self, symbol: &Sym) -> Option<Reference> {
        self.bars.get(symbol).map(|c| Reference(c.close))
    }

    fn equity(&self) -> Reference {
        let positions_value: Real = self
            .positions
            .iter()
            .map(|(symbol, &amount)| amount * self.bars.get(symbol).map_or(0.0, |c| c.close))
            .sum();
        Reference(self.funds + positions_value)
    }

    fn update(&mut self, symbol: Sym, candle: Candle) -> Vec<Order<Sym>> {
        // Mark the new bar first so a queued fill validates against *this* bar's
        // range (its `open` is trivially within it), then flush any queued market
        // order at the open, then test the resting protective legs.
        self.bars.insert(symbol.clone(), candle);
        let mut fills = Vec::new();
        if let Some(pending) = self.pending.remove(&symbol) {
            let (target, id) = match pending {
                Pending::Target(amount, id) => (amount, id),
                // Resolve the size at the fill price, so an all-in stays exact.
                // Equity marks the fill symbol at `open` (the actual fill price),
                // not the just-inserted `close` — otherwise a reversal sizes off
                // information from later in this bar.
                Pending::Sized(side, size, id) => {
                    let position = self.positions.get(&symbol).copied().unwrap_or(0.0);
                    let equity_at_open = self.funds
                        + self
                            .positions
                            .iter()
                            .map(|(s, &a)| {
                                let mark = if *s == symbol {
                                    candle.open
                                } else {
                                    self.bars.get(s).map_or(0.0, |c| c.close)
                                };
                                a * mark
                            })
                            .sum::<Real>();
                    let magnitude = size.resolve(candle.open, position, self.funds, equity_at_open);
                    // For a fractional sizing ("as much of my equity/funds as
                    // fits"), shrink a net buy so spread + slippage +
                    // commission fit available cash. Without this, an all-in
                    // `value_frac(1.0)` under any positive cost model would
                    // size the notional to the entire equity, and paying
                    // commission on top would fail the affordability check in
                    // `fill_at` and silently drop the fill. An explicit
                    // `Size::Units(n)` or `Size::PositionFraction(f)` carries
                    // a specific unit intent and is left alone — an infeasible
                    // request is a caller error, not a sizing target.
                    let magnitude = match size {
                        Size::ValueFraction(_) | Size::FundsFraction(_) => self
                            .shrink_buy_to_fit(
                                &symbol,
                                side,
                                position,
                                magnitude,
                                FillPricing {
                                    bar: &candle,
                                    price: candle.open,
                                    kind: OrderKind::Market,
                                },
                            ),
                        Size::Units(_) | Size::PositionFraction(_) => magnitude,
                    };
                    (side.sign() * magnitude, id)
                }
            };
            match self.fill_at(symbol.clone(), target, candle.open, OrderKind::Market, id) {
                Ok(Some(order)) => fills.push(order),
                Ok(None) => {}
                Err(error) => self.rejections.push(Rejection {
                    symbol: symbol.clone(),
                    id,
                    error,
                    kind: OrderKind::Market,
                }),
            }
        }
        if let Some(order) = self.match_protective(&symbol, &candle) {
            fills.push(order);
        }
        // Limits come last: a protective leg guards a position that already
        // exists, so letting a fresh entry fill ahead of the exit it was meant
        // to trigger would leave the strategy holding something it had asked to
        // be out of.
        if let Some(order) = self.match_limit(&symbol, &candle) {
            fills.push(order);
        }
        fills
    }

    fn set_position(&mut self, target: Units<Sym>) -> Result<Ack<Sym>, WalletError> {
        // Pre-flight against last close so an infeasible submission errors
        // synchronously (mirroring a live venue's rejection) rather than
        // queuing an order that fill_at will drop into the rejections log.
        let current = self.positions.get(&target.symbol).copied().unwrap_or(0.0);
        if let Err(e) = self.preflight_market(&target.symbol, target.amount - current) {
            return Err(self.reject_submission(&target.symbol, e));
        }
        let id = self.mint();
        self.pending
            .insert(target.symbol, Pending::Target(target.amount, id));
        Ok(Ack::Working(id))
    }

    fn set(&mut self, symbol: Sym, side: Side, size: Size) -> Result<Ack<Sym>, WalletError> {
        // Pre-flight what we can at submission: price validity always, and the
        // affordability check for an explicit Size::Units target. Fractional
        // sizings (ValueFraction / FundsFraction) always shrink to fit at
        // fill time, so they never fail a submission-time affordability
        // check and only need the price-validity guards here.
        let close = match self.price(&symbol) {
            Some(p) => p.0,
            None => return Err(self.reject_submission(&symbol, WalletError::UnknownPrice)),
        };
        if close <= 0.0 {
            return Err(self.reject_submission(&symbol, WalletError::InvalidPrice));
        }
        if let Size::Units(units) = size {
            let current = self.positions.get(&symbol).copied().unwrap_or(0.0);
            let target = side.sign() * units.abs();
            if let Err(e) = self.check_buy_affordability(target - current, close) {
                return Err(self.reject_submission(&symbol, e));
            }
        }
        let id = self.mint();
        self.pending.insert(symbol, Pending::Sized(side, size, id));
        Ok(Ack::Working(id))
    }

    fn set_stop(
        &mut self,
        symbol: Sym,
        trigger: Reference,
        size: Size,
    ) -> Result<Ack<Sym>, WalletError> {
        let id = self.mint();
        self.protective.entry(symbol).or_default().stop = Some(Leg {
            trigger: trigger.0,
            size,
            id,
        });
        Ok(Ack::Working(id))
    }

    fn set_take_profit(
        &mut self,
        symbol: Sym,
        trigger: Reference,
        size: Size,
    ) -> Result<Ack<Sym>, WalletError> {
        let id = self.mint();
        self.protective.entry(symbol).or_default().take_profit = Some(Leg {
            trigger: trigger.0,
            size,
            id,
        });
        Ok(Ack::Working(id))
    }

    fn cancel_protective(&mut self, symbol: &Sym) -> Result<(), WalletError> {
        self.protective.remove(symbol);
        Ok(())
    }

    fn set_limit(
        &mut self,
        symbol: Sym,
        side: Side,
        size: Size,
        limit: Reference,
    ) -> Result<Ack<Sym>, WalletError> {
        if limit.0 <= 0.0 {
            return Err(WalletError::InvalidPrice);
        }
        let id = self.mint();
        self.limits.insert(
            symbol,
            RestingLimit {
                side,
                size,
                limit: limit.0,
                id,
            },
        );
        Ok(Ack::Working(id))
    }

    fn cancel_limit(&mut self, symbol: &Sym) -> Result<(), WalletError> {
        self.limits.remove(symbol);
        Ok(())
    }

    fn take_rejections(&mut self) -> Vec<Rejection<Sym>> {
        // Yield the not-yet-drained tail and advance the cursor rather than
        // truncating — `rejections()` still reports the full run history.
        let fresh = self.rejections[self.rejections_drained..].to_vec();
        self.rejections_drained = self.rejections.len();
        fresh
    }

    /// Directly credit / debit the cash balance — the paper impl of the
    /// [`Wallet::adjust_funds`] hook. Returns
    /// [`WalletError::InsufficientFunds`] if the resulting balance would
    /// be negative, otherwise applies the delta atomically. Booked
    /// outside the blotter (no `Order`, no `on_fill`).
    fn adjust_funds(&mut self, delta: Real) -> Result<(), WalletError> {
        let new_funds = self.funds + delta;
        if new_funds < 0.0 {
            return Err(WalletError::InsufficientFunds);
        }
        self.funds = new_funds;
        Ok(())
    }

    /// Drop a working order by id — the paper impl of [`Wallet::cancel`]. A
    /// queued market order (one per symbol) is removed by matching its
    /// [`OrderId`]; a resting protective leg is cleared (and its bracket
    /// dropped when both legs are gone). An id the wallet no longer holds is a
    /// no-op `Ok(())`, per the trait contract.
    fn cancel(&mut self, id: OrderId) -> Result<(), WalletError> {
        // A queued market order carries its id in either `Pending` variant.
        let queued = self.pending.iter().find_map(|(sym, pending)| {
            let pid = match pending {
                Pending::Target(_, pid) | Pending::Sized(_, _, pid) => *pid,
            };
            (pid == id).then(|| sym.clone())
        });
        if let Some(sym) = queued {
            self.pending.remove(&sym);
            return Ok(());
        }
        // Otherwise clear a matching resting protective leg, then discard any
        // now-empty bracket.
        for prot in self.protective.values_mut() {
            if prot.stop.is_some_and(|l| l.id == id) {
                prot.stop = None;
            }
            if prot.take_profit.is_some_and(|l| l.id == id) {
                prot.take_profit = None;
            }
        }
        self.protective
            .retain(|_, prot| prot.stop.is_some() || prot.take_profit.is_some());
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use std::collections::HashMap;

    use super::*;
    use crate::wallet::SleeveWallet;
    use crate::indicators::{BoolIndicatorExt, IndicatorExt, Sma};
    use crate::signal::Signal;
    use crate::strategy::Strategy;
    use crate::types::Candle;

    fn bar(close: Real) -> Candle {
        Candle::new(close, close, close, close, 0.0)
    }

    /// Assert an order's fields, ignoring its (wallet-minted) id.
    fn assert_fill(o: &Order<&str>, side: Side, units: Real, price: Real, kind: OrderKind) {
        assert_eq!(o.side, side, "side");
        assert!((o.units - units).abs() < 1e-9, "units {} != {}", o.units, units);
        assert!((o.price - price).abs() < 1e-9, "price {} != {}", o.price, price);
        assert_eq!(o.kind, kind, "kind");
    }

    /// An OHLC bar, for the limit tests — the flat `bar()` helper can't
    /// express "traded down to X and back".
    fn ohlc(open: Real, high: Real, low: Real, close: Real) -> Candle {
        Candle::new(open, high, low, close, 1_000.0)
    }

    #[test]
    fn a_buy_limit_rests_until_the_bar_trades_down_to_it() {
        let mut w: PaperWallet<&str> = PaperWallet::new(1_000.0);
        w.update("X", ohlc(100.0, 101.0, 99.0, 100.0));
        assert!(matches!(
            w.set_limit("X", Side::Buy, Size::units(5.0), Reference(98.0)),
            Ok(Ack::Working(_))
        ));

        // Bar never reaches 98 — nothing fills, the order still rests.
        let fills = w.update("X", ohlc(100.0, 102.0, 98.5, 101.0));
        assert!(fills.is_empty(), "limit must not fill above its price");
        assert_eq!(w.position(&"X").amount, 0.0);

        // Bar trades through it.
        let fills = w.update("X", ohlc(100.0, 101.0, 97.0, 99.0));
        assert_eq!(fills.len(), 1);
        assert_fill(&fills[0], Side::Buy, 5.0, 98.0, OrderKind::Limit);
        assert_eq!(w.position(&"X").amount, 5.0);
    }

    #[test]
    fn a_sell_limit_mirrors_on_the_high() {
        let mut w: PaperWallet<&str> = PaperWallet::new(1_000.0);
        w.update("X", ohlc(100.0, 101.0, 99.0, 100.0));
        w.set_limit("X", Side::Sell, Size::units(3.0), Reference(105.0))
            .unwrap();

        assert!(w.update("X", ohlc(100.0, 104.0, 99.0, 103.0)).is_empty());
        let fills = w.update("X", ohlc(104.0, 106.0, 103.0, 105.0));
        assert_eq!(fills.len(), 1);
        assert_fill(&fills[0], Side::Sell, 3.0, 105.0, OrderKind::Limit);
        assert_eq!(w.position(&"X").amount, -3.0);
    }

    #[test]
    fn a_gap_through_the_limit_fills_at_the_better_open() {
        let mut w: PaperWallet<&str> = PaperWallet::new(10_000.0);
        w.update("X", ohlc(100.0, 101.0, 99.0, 100.0));
        w.set_limit("X", Side::Buy, Size::units(5.0), Reference(98.0))
            .unwrap();
        // Opens at 95 — below the limit. The caller asked for "98 or better"
        // and the market opened better, so they get 95, not 98.
        let fills = w.update("X", ohlc(95.0, 96.0, 94.0, 95.5));
        assert_fill(&fills[0], Side::Buy, 5.0, 95.0, OrderKind::Limit);
    }

    #[test]
    fn a_limit_never_fills_worse_than_its_price_under_costs() {
        // The invariant that makes a limit a limit. An aggressive spread +
        // slippage bundle would push a market buy well above 98; a passive fill
        // crosses no spread and takes no impact, so it prices exactly at the
        // limit.
        use crate::costs::{FixedBpsSlippage, FixedBpsSpread, NoCommission};
        let costs = TradingCosts::new(
            Box::new(NoCommission),
            Box::new(FixedBpsSpread::new(50.0)),
            Box::new(FixedBpsSlippage::new(100.0)),
        );
        let mut w: PaperWallet<&str> = PaperWallet::with_costs(10_000.0, costs);
        w.update("X", ohlc(100.0, 101.0, 99.0, 100.0));
        w.set_limit("X", Side::Buy, Size::units(5.0), Reference(98.0))
            .unwrap();
        let fills = w.update("X", ohlc(100.0, 101.0, 97.0, 99.0));
        assert_eq!(fills.len(), 1);
        assert!(
            fills[0].price <= 98.0 + 1e-9,
            "limit buy filled at {}, worse than its 98.0 limit",
            fills[0].price
        );
        assert_fill(&fills[0], Side::Buy, 5.0, 98.0, OrderKind::Limit);
    }

    #[test]
    fn a_limit_is_latest_wins_and_cancellable() {
        let mut w: PaperWallet<&str> = PaperWallet::new(10_000.0);
        w.update("X", ohlc(100.0, 101.0, 99.0, 100.0));
        w.set_limit("X", Side::Buy, Size::units(5.0), Reference(98.0))
            .unwrap();
        // Re-submitting replaces rather than stacking — the convention the
        // protective legs use, so a strategy can walk the price each bar.
        w.set_limit("X", Side::Buy, Size::units(2.0), Reference(96.0))
            .unwrap();
        let fills = w.update("X", ohlc(100.0, 101.0, 95.0, 97.0));
        assert_eq!(fills.len(), 1, "only the latest order rests");
        assert_fill(&fills[0], Side::Buy, 2.0, 96.0, OrderKind::Limit);

        w.set_limit("X", Side::Buy, Size::units(1.0), Reference(90.0))
            .unwrap();
        w.cancel_limit(&"X").unwrap();
        assert!(w.update("X", ohlc(95.0, 96.0, 85.0, 90.0)).is_empty());
    }

    #[test]
    fn a_limit_sizes_against_equity_at_the_fill_not_at_submission() {
        let mut w: PaperWallet<&str> = PaperWallet::new(1_000.0);
        w.update("X", ohlc(100.0, 101.0, 99.0, 100.0));
        w.set_limit("X", Side::Buy, Size::value_frac(1.0), Reference(50.0))
            .unwrap();
        let fills = w.update("X", ohlc(100.0, 101.0, 40.0, 45.0));
        // All-in at 50 with 1000 of equity — 20 units, not the 10 that sizing
        // at the submission price of 100 would have produced.
        assert_fill(&fills[0], Side::Buy, 20.0, 50.0, OrderKind::Limit);
    }

    #[test]
    fn a_protective_exit_fills_before_a_limit_entry_on_the_same_bar() {
        // Both trigger on one bar. The stop guards a position that already
        // exists; filling the entry first would leave the strategy holding
        // something it had asked to be out of.
        let mut w: PaperWallet<&str> = PaperWallet::new(10_000.0);
        w.update("X", ohlc(100.0, 101.0, 99.0, 100.0));
        w.set_position(Units {
            symbol: "X",
            amount: 10.0,
        })
        .unwrap();
        w.update("X", ohlc(100.0, 101.0, 99.0, 100.0));
        w.set_stop("X", Reference(95.0), Size::position_frac(1.0)).unwrap();
        w.set_limit("X", Side::Buy, Size::units(4.0), Reference(94.0))
            .unwrap();

        let fills = w.update("X", ohlc(99.0, 99.5, 93.0, 94.0));
        assert_eq!(fills.len(), 2);
        assert_eq!(fills[0].kind, OrderKind::Stop, "stop books first");
        assert_eq!(fills[1].kind, OrderKind::Limit);
        // Flattened by the stop, then re-entered 4 long by the limit.
        assert_eq!(w.position(&"X").amount, 4.0);
    }

    #[test]
    fn an_unaffordable_limit_is_rejected_not_silently_retried() {
        let mut w: PaperWallet<&str> = PaperWallet::new(100.0);
        w.update("X", ohlc(100.0, 101.0, 99.0, 100.0));
        w.set_limit("X", Side::Buy, Size::units(1_000.0), Reference(98.0))
            .unwrap();
        let fills = w.update("X", ohlc(100.0, 101.0, 97.0, 99.0));
        assert!(fills.is_empty());
        let rejections = w.take_rejections();
        assert_eq!(rejections.len(), 1);
        assert_eq!(rejections[0].error, WalletError::InsufficientFunds);
        assert_eq!(rejections[0].kind, OrderKind::Limit);
        // Consumed, not left resting at a price the market has moved past.
        assert!(w.update("X", ohlc(97.0, 98.0, 96.0, 97.0)).is_empty());
    }

    #[test]
    fn set_limit_defaults_to_unsupported_for_a_wallet_that_opts_out() {
        // The trait default, so a downstream wallet whose venue has no resting
        // limit keeps compiling and says so rather than silently doing nothing.
        struct Minimal;
        impl Wallet<&'static str> for Minimal {
            fn funds(&self) -> Reference {
                Reference(0.0)
            }
            fn position(&self, _s: &&'static str) -> Units<&'static str> {
                Units {
                    symbol: "X",
                    amount: 0.0,
                }
            }
            fn price(&self, _s: &&'static str) -> Option<Reference> {
                None
            }
            fn equity(&self) -> Reference {
                Reference(0.0)
            }
            fn update(&mut self, _s: &'static str, _c: Candle) -> Vec<Order<&'static str>> {
                Vec::new()
            }
            fn set_position(
                &mut self,
                _t: Units<&'static str>,
            ) -> Result<Ack<&'static str>, WalletError> {
                Err(WalletError::UnsupportedOperation)
            }
            fn set_stop(
        &mut self,
        _s: &'static str,
        _t: Reference,
        _size: Size,
    ) -> Result<Ack<&'static str>, WalletError> {
                Err(WalletError::UnsupportedOperation)
            }
            fn set_take_profit(
        &mut self,
        _s: &'static str,
        _t: Reference,
        _size: Size,
    ) -> Result<Ack<&'static str>, WalletError> {
                Err(WalletError::UnsupportedOperation)
            }
            fn cancel_protective(&mut self, _s: &&'static str) -> Result<(), WalletError> {
                Ok(())
            }
        }
        let mut w = Minimal;
        assert_eq!(
            w.set_limit("X", Side::Buy, Size::units(1.0), Reference(10.0)),
            Err(WalletError::UnsupportedOperation)
        );
        assert!(w.cancel_limit(&"X").is_ok());
        // Same default-shape question for the capability read: signed positions
        // are the trait's model, so an impl that says nothing can short.
        assert!(w.can_short());
    }

    #[test]
    fn can_short_is_introspected_and_a_sleeve_delegates_it() {
        // The paper account is the permissive reference: a sell credits cash.
        let paper: PaperWallet<&str> = PaperWallet::new(1_000.0);
        assert!(paper.can_short());

        // A spot-shaped wallet — the shape `CoinbaseWallet` has — reports the
        // limit up front instead of leaving it to be discovered one clamped
        // order at a time.
        struct SpotOnly(PaperWallet<&'static str>);
        impl Wallet<&'static str> for SpotOnly {
            fn can_short(&self) -> bool {
                false
            }
            fn funds(&self) -> Reference {
                self.0.funds()
            }
            fn position(&self, s: &&'static str) -> Units<&'static str> {
                self.0.position(s)
            }
            fn positions(&self) -> Vec<Units<&'static str>> {
                self.0.positions()
            }
            fn price(&self, s: &&'static str) -> Option<Reference> {
                self.0.price(s)
            }
            fn equity(&self) -> Reference {
                self.0.equity()
            }
            fn update(&mut self, s: &'static str, c: Candle) -> Vec<Order<&'static str>> {
                self.0.update(s, c)
            }
            fn set_position(
                &mut self,
                t: Units<&'static str>,
            ) -> Result<Ack<&'static str>, WalletError> {
                // Spot: clamp a short to flat, as a real spot venue must.
                self.0.set_position(Units {
                    symbol: t.symbol,
                    amount: t.amount.max(0.0),
                })
            }
            fn set_stop(
                &mut self,
                s: &'static str,
                t: Reference,
                size: Size,
            ) -> Result<Ack<&'static str>, WalletError> {
                self.0.set_stop(s, t, size)
            }
            fn set_take_profit(
                &mut self,
                s: &'static str,
                t: Reference,
                size: Size,
            ) -> Result<Ack<&'static str>, WalletError> {
                self.0.set_take_profit(s, t, size)
            }
            fn cancel_protective(&mut self, s: &&'static str) -> Result<(), WalletError> {
                self.0.cancel_protective(s)
            }
        }

        let spot = SpotOnly(PaperWallet::new(1_000.0));
        assert!(!spot.can_short());

        // A sleeve is a view, not an account: it answers for what it wraps,
        // either way.
        let over_spot = SleeveWallet::new(spot, HashMap::new());
        assert!(!over_spot.can_short());
        let over_paper: SleeveWallet<&str, PaperWallet<&str>> =
            SleeveWallet::new(PaperWallet::new(1_000.0), HashMap::new());
        assert!(over_paper.can_short());
    }

    #[test]
    fn set_position_queues_and_fills_at_next_open() {
        let mut w: PaperWallet<&str> = PaperWallet::new(1_000.0);
        w.update("X", bar(100.0));
        // A market order only queues (Ack::Working) — nothing is booked yet.
        assert!(matches!(
            w.set_position(Units {
                symbol: "X",
                amount: 3.0
            }),
            Ok(Ack::Working(_))
        ));
        assert_eq!(w.position(&"X").amount, 0.0);
        assert!(w.orders().is_empty());
        // The next bar fills it at that bar's open, returning it in the fill stream.
        let fills = w.update("X", bar(100.0));
        assert_eq!(w.position(&"X").amount, 3.0);
        assert_eq!(w.funds().0, 1_000.0 - 3.0 * 100.0);
        assert_fill(&fills[0], Side::Buy, 3.0, 100.0, OrderKind::Market);
        assert_fill(w.orders().last().unwrap(), Side::Buy, 3.0, 100.0, OrderKind::Market);
        // Setting a larger target buys the difference (scale in), again next open.
        w.set_position(Units {
            symbol: "X",
            amount: 5.0,
        })
        .unwrap();
        w.update("X", bar(100.0));
        assert_eq!(w.position(&"X").amount, 5.0);
        assert_eq!(w.funds().0, 1_000.0 - 5.0 * 100.0);
    }

    #[test]
    fn set_targets_absolute_and_reverses() {
        let mut w: PaperWallet<&str> = PaperWallet::new(10_000.0);
        w.update("X", bar(50.0));
        w.set("X", Side::Buy, Size::units(4.0)).unwrap();
        w.update("X", bar(50.0)); // fills the +4 at the open
        assert_eq!(w.position(&"X").amount, 4.0);
        // Re-targeting the same side is idempotent: the queued fill is a no-op.
        let before = w.orders().len();
        w.set("X", Side::Buy, Size::units(4.0)).unwrap();
        w.update("X", bar(50.0));
        assert_eq!(w.orders().len(), before);
        // Opposite side reverses: +4 -> -4 is a sell of 8.
        w.set("X", Side::Sell, Size::units(4.0)).unwrap();
        w.update("X", bar(50.0));
        assert_fill(w.orders().last().unwrap(), Side::Sell, 8.0, 50.0, OrderKind::Market);
        assert_eq!(w.position(&"X").amount, -4.0);
    }

    #[test]
    fn close_flattens() {
        let mut w: PaperWallet<&str> = PaperWallet::new(1_000.0);
        w.update("X", bar(100.0));
        w.set("X", Side::Buy, Size::units(10.0)).unwrap();
        w.update("X", bar(100.0)); // fill the buy at 100
        w.update("X", bar(110.0)); // mark to 110
        assert!(matches!(w.close("X"), Ok(Ack::Working(_)))); // queued
        w.update("X", bar(110.0)); // fills the close at the open 110
        assert_fill(w.orders().last().unwrap(), Side::Sell, 10.0, 110.0, OrderKind::Market);
        assert!(w.positions().is_empty());
        assert_eq!(w.funds().0, 1_100.0);
    }

    #[test]
    fn relative_sizing_resolves_against_funds_and_position() {
        let mut w: PaperWallet<&str> = PaperWallet::new(1_000.0);
        w.update("X", bar(25.0));
        // 10% of 1000 = 100 / price 25 = 4 units, resolved at the fill (open 25).
        w.set("X", Side::Buy, Size::funds_frac(0.1)).unwrap();
        w.update("X", bar(25.0));
        assert_fill(w.orders().last().unwrap(), Side::Buy, 4.0, 25.0, OrderKind::Market);
        // Set to 50% of the 4-unit position -> sell 2.
        w.set("X", Side::Buy, Size::position_frac(0.5)).unwrap();
        w.update("X", bar(25.0));
        assert_fill(w.orders().last().unwrap(), Side::Sell, 2.0, 25.0, OrderKind::Market);
        assert_eq!(w.position(&"X").amount, 2.0);
    }

    #[test]
    fn value_fraction_sizes_against_equity_and_flips_all_in() {
        let mut w: PaperWallet<&str> = PaperWallet::new(1_000.0);
        w.update("X", bar(100.0));
        // All-in long: 100% of equity (== funds when flat) / 100 = 10 units.
        w.set("X", Side::Buy, Size::value_frac(1.0)).unwrap();
        w.update("X", bar(100.0));
        assert_eq!(w.position(&"X").amount, 10.0);
        assert!(w.funds().0.abs() <= 1e-6);
        // Equity is still 1000; flip all-in short -> -10 units (a sell of 20).
        w.set("X", Side::Sell, Size::value_frac(1.0)).unwrap();
        w.update("X", bar(100.0));
        assert_fill(w.orders().last().unwrap(), Side::Sell, 20.0, 100.0, OrderKind::Market);
        assert_eq!(w.position(&"X").amount, -10.0);
    }

    #[test]
    fn value_fraction_reversal_sizes_against_open_not_close() {
        // Regression: on a reversal the sizing must mark the existing position at
        // the fill (open) price, not this bar's close — otherwise a bar whose
        // open ≠ close leaks close information into the size.
        let mut w: PaperWallet<&str> = PaperWallet::new(1_000.0);
        w.update("X", bar(100.0));
        w.set("X", Side::Buy, Size::value_frac(1.0)).unwrap();
        w.update("X", bar(100.0)); // long 10 @ 100; funds 0
        // Reverse all-in on a bar with open 95 (fill price) and close 105.
        // Equity-at-open = 0 + 10*95 = 950, magnitude = 950/95 = 10 -> target -10,
        // delta = -20. Using close (105) would give ~21.05 units sold — the bug.
        w.set("X", Side::Sell, Size::value_frac(1.0)).unwrap();
        w.update("X", Candle::new(95.0, 106.0, 94.0, 105.0, 0.0));
        assert_fill(w.orders().last().unwrap(), Side::Sell, 20.0, 95.0, OrderKind::Market);
        assert_eq!(w.position(&"X").amount, -10.0);
    }

    #[test]
    fn fill_time_rejection_is_recorded_on_a_gap_up() {
        // A submission that clears the last-close pre-flight can still be
        // dropped at fill time if the next open gaps meaningfully higher.
        // The wallet stashes the drop in `rejections()` so a driver can
        // report why a bar produced no fill.
        let mut w: PaperWallet<&str> = PaperWallet::new(100.0);
        w.update("X", bar(50.0));
        // 1 unit @ close 50 = 50, comfortably within 100 funds — pre-flight passes.
        let ack = w.set("X", Side::Buy, Size::units(1.0)).unwrap();
        let id = match ack {
            Ack::Working(id) => id,
            Ack::Filled(_) => panic!("market order should queue, not fill"),
        };
        // But the bar gaps up: open 200 > 100 funds. fill_at rejects.
        let fills = w.update("X", Candle::new(200.0, 210.0, 195.0, 205.0, 0.0));
        assert!(fills.is_empty(), "expected no fill");
        assert!(w.positions().is_empty());
        assert_eq!(w.rejections().len(), 1);
        assert_eq!(w.rejections()[0].symbol, "X");
        assert_eq!(w.rejections()[0].id, id);
        assert_eq!(w.rejections()[0].error, WalletError::InsufficientFunds);
    }

    #[test]
    fn drain_yields_each_rejection_once_without_disturbing_the_accessor() {
        let mut w: PaperWallet<&str> = PaperWallet::new(100.0);
        w.update("X", bar(50.0));
        w.set("X", Side::Buy, Size::units(1.0)).unwrap();
        w.update("X", Candle::new(200.0, 210.0, 195.0, 205.0, 0.0));

        // The drain is the driver-facing stream: each entry exactly once.
        let drained = w.take_rejections();
        assert_eq!(drained.len(), 1);
        assert_eq!(drained[0].error, WalletError::InsufficientFunds);
        assert_eq!(drained[0].kind, OrderKind::Market);
        assert!(w.take_rejections().is_empty(), "already yielded");

        // ...but the non-destructive accessor still reports the full history.
        assert_eq!(w.rejections().len(), 1, "drain must not truncate history");
    }

    #[test]
    fn a_triggered_stop_that_cannot_be_booked_is_reported() {
        // The protective-leg drop site: a stop triggers, but booking the exit
        // fails. Before this was reported, the strategy was left holding a
        // position it believed was protected, with nothing said.
        let mut w: PaperWallet<&str> = PaperWallet::new(10_000.0);
        w.update("X", bar(100.0));
        w.set("X", Side::Sell, Size::units(50.0)).unwrap();
        w.update("X", bar(100.0));
        assert_eq!(w.position(&"X").amount, -50.0, "short 50");

        // Rest a stop above, then gap through it to a price that makes buying
        // back the short unaffordable.
        w.set_stop("X", Reference(120.0), Size::position_frac(1.0)).unwrap();
        let fills = w.update("X", Candle::new(900.0, 950.0, 890.0, 940.0, 0.0));

        assert!(fills.is_empty(), "the stop could not be booked");
        assert_eq!(w.position(&"X").amount, -50.0, "still exposed");
        let drained = w.take_rejections();
        assert_eq!(drained.len(), 1, "the refusal must be surfaced");
        assert_eq!(drained[0].kind, OrderKind::Stop, "reported as a stop");
        assert_eq!(drained[0].error, WalletError::InsufficientFunds);
    }

    #[test]
    fn value_fraction_all_in_shrinks_to_fit_under_costs() {
        // Regression: `value_frac(1.0)` under any positive cost model used to
        // silently produce zero fills — the resolved size was `equity/open`,
        // but paying commission on top drove `cost > funds` and the fill was
        // rejected. The wallet now shrinks the resolved magnitude so the fill
        // clears the affordability check.
        use crate::costs::{FixedBpsSpread, NoSlippage, PercentageCommission};
        let costs = TradingCosts::new(
            Box::new(PercentageCommission::new(0.001)), // 10 bps
            Box::new(FixedBpsSpread::new(10.0)),        // 10 bps round-trip
            Box::new(NoSlippage),
        );
        let mut w: PaperWallet<&str> = PaperWallet::with_costs(1_000.0, costs);
        w.update("X", bar(100.0));
        w.set("X", Side::Buy, Size::value_frac(1.0)).unwrap();
        let fills = w.update("X", bar(100.0));
        // A fill happens (was zero before the fix) and cash never goes negative.
        assert_eq!(fills.len(), 1, "expected one fill, got {}", fills.len());
        assert!(w.position(&"X").amount > 0.0);
        assert!(
            w.funds().0 >= -1e-6,
            "funds went negative: {}",
            w.funds().0
        );
        // The resulting notional is just under equity (deducted spread +
        // commission), not equal to it.
        let fill = &fills[0];
        assert!(fill.units < 10.0, "units {} should be shrunk below 10.0", fill.units);
        assert!(fill.units > 9.9, "units {} shrunk too aggressively", fill.units);
    }

    #[test]
    fn equity_marks_positions_to_fed_prices() {
        let mut w: PaperWallet<&str> = PaperWallet::new(1_000.0);
        w.update("X", bar(100.0));
        w.set("X", Side::Buy, Size::units(4.0)).unwrap();
        w.update("X", bar(100.0)); // fill: funds 600, +4 units
        w.update("X", bar(120.0));
        assert_eq!(w.equity().0, 600.0 + 4.0 * 120.0);
    }

    #[test]
    fn unknown_price_is_flagged_at_submission_and_at_fill() {
        let mut w: PaperWallet<&str> = PaperWallet::new(1_000.0);
        // "X" was never fed a bar. fill_at flags it directly...
        assert_eq!(
            w.fill_at("X", 1.0, 50.0, OrderKind::Market, OrderId(0)),
            Err(WalletError::UnknownPrice)
        );
        // ...and the submission-time pre-flight refuses to queue an order
        // that can never be priced, so the caller learns synchronously
        // instead of via the rejections log.
        assert_eq!(
            w.set_position(Units { symbol: "X", amount: 1.0 }),
            Err(WalletError::UnknownPrice)
        );
        assert_eq!(
            w.set("X", Side::Buy, Size::units(1.0)),
            Err(WalletError::UnknownPrice)
        );
    }

    #[test]
    fn insufficient_funds_is_flagged_at_submission_but_shorts_are_free() {
        let mut w: PaperWallet<&str> = PaperWallet::new(100.0);
        w.update("X", bar(50.0));
        // 3 units cost 150 > 100 funds, and there is no margin. fill_at
        // flags it directly...
        assert_eq!(
            w.fill_at("X", 3.0, 50.0, OrderKind::Market, OrderId(0)),
            Err(WalletError::InsufficientFunds)
        );
        // ...and set/set_position pre-flight against last close so a caller
        // learns synchronously instead of waiting for the fill-time rejection.
        assert_eq!(
            w.set("X", Side::Buy, Size::units(3.0)),
            Err(WalletError::InsufficientFunds)
        );
        assert_eq!(
            w.set_position(Units { symbol: "X", amount: 3.0 }),
            Err(WalletError::InsufficientFunds)
        );
        // A short sale credits cash, so selling is always feasible.
        w.set("X", Side::Sell, Size::units(3.0)).unwrap();
        w.update("X", bar(50.0));
        assert_eq!(w.position(&"X").amount, -3.0);
    }

    #[test]
    fn non_positive_price_is_flagged_at_submission_and_at_fill() {
        let mut w: PaperWallet<&str> = PaperWallet::new(1_000.0);
        w.update("X", bar(0.0));
        // fill_at flags a non-positive theoretical price directly...
        assert_eq!(
            w.fill_at("X", 1.0, 0.0, OrderKind::Market, OrderId(0)),
            Err(WalletError::InvalidPrice)
        );
        // ...and submissions against a symbol whose last close is
        // non-positive refuse to queue at all.
        assert_eq!(
            w.set("X", Side::Buy, Size::value_frac(1.0)),
            Err(WalletError::InvalidPrice)
        );
        assert_eq!(
            w.set_position(Units { symbol: "X", amount: 1.0 }),
            Err(WalletError::InvalidPrice)
        );
    }

    #[test]
    fn fill_outside_candle_range_is_rejected() {
        let mut w: PaperWallet<&str> = PaperWallet::new(10_000.0);
        w.update("X", Candle::new(100.0, 110.0, 90.0, 105.0, 0.0));
        // 120 is above the bar's high — it never traded there this bar.
        assert_eq!(
            w.fill_at("X", 1.0, 120.0, OrderKind::Stop, OrderId(0)),
            Err(WalletError::PriceOutOfRange)
        );
    }

    #[test]
    fn resting_stop_fills_at_the_level() {
        let mut w: PaperWallet<&str> = PaperWallet::new(10_000.0);
        w.update("X", bar(100.0));
        w.set("X", Side::Buy, Size::units(1.0)).unwrap();
        w.update("X", bar(100.0)); // long 1 @ 100
        w.set_stop("X", Reference(90.0), Size::position_frac(1.0)).unwrap();
        // The bar trades down through 90 (low 88) but opens above it.
        let fills = w.update("X", Candle::new(95.0, 96.0, 88.0, 89.0, 0.0));
        assert_eq!(fills.len(), 1);
        assert_fill(&fills[0], Side::Sell, 1.0, 90.0, OrderKind::Stop);
        assert!(w.positions().is_empty());
    }

    #[test]
    fn resting_stop_gaps_to_the_open() {
        let mut w: PaperWallet<&str> = PaperWallet::new(10_000.0);
        w.update("X", bar(100.0));
        w.set("X", Side::Buy, Size::units(1.0)).unwrap();
        w.update("X", bar(100.0));
        w.set_stop("X", Reference(90.0), Size::position_frac(1.0)).unwrap();
        // Gaps down opening at 85, already below the stop -> fills at the open.
        let fills = w.update("X", Candle::new(85.0, 86.0, 84.0, 84.0, 0.0));
        assert_fill(&fills[0], Side::Sell, 1.0, 85.0, OrderKind::Stop);
        assert!(w.positions().is_empty());
    }

    #[test]
    fn resting_take_profit_on_a_short_fills_at_the_level() {
        let mut w: PaperWallet<&str> = PaperWallet::new(10_000.0);
        w.update("X", bar(100.0));
        w.set("X", Side::Sell, Size::units(1.0)).unwrap();
        w.update("X", bar(100.0)); // short 1 @ 100
        // A short take-profit sits below entry; the bar trades down to it.
        w.set_take_profit("X", Reference(90.0), Size::position_frac(1.0)).unwrap();
        let fills = w.update("X", Candle::new(95.0, 96.0, 88.0, 92.0, 0.0));
        assert_fill(&fills[0], Side::Buy, 1.0, 90.0, OrderKind::TakeProfit);
        assert!(w.positions().is_empty());
    }

    #[test]
    fn oco_stop_takes_precedence_and_cancels_the_target() {
        let mut w: PaperWallet<&str> = PaperWallet::new(10_000.0);
        w.update("X", bar(100.0));
        w.set("X", Side::Buy, Size::units(1.0)).unwrap();
        w.update("X", bar(100.0));
        w.set_stop("X", Reference(90.0), Size::position_frac(1.0)).unwrap();
        w.set_take_profit("X", Reference(110.0), Size::position_frac(1.0)).unwrap();
        // A wide bar crosses both legs; the stop wins, and the fill flattens and
        // drops the whole bracket.
        let fills = w.update("X", Candle::new(100.0, 111.0, 89.0, 105.0, 0.0));
        assert_eq!(fills.len(), 1);
        assert_eq!(fills[0].kind, OrderKind::Stop);
        assert!(w.positions().is_empty());
        // No leftover leg: a later bar does nothing.
        let more = w.update("X", Candle::new(105.0, 112.0, 88.0, 100.0, 0.0));
        assert!(more.is_empty());
    }

    #[test]
    fn market_exit_auto_cancels_the_bracket() {
        let mut w: PaperWallet<&str> = PaperWallet::new(10_000.0);
        w.update("X", bar(100.0));
        w.set("X", Side::Buy, Size::units(1.0)).unwrap();
        w.update("X", bar(100.0));
        w.set_stop("X", Reference(90.0), Size::position_frac(1.0)).unwrap();
        // Flatten with a market close; the fill drops the resting stop.
        w.close("X").unwrap();
        w.update("X", bar(100.0));
        assert!(w.positions().is_empty());
        // The old stop no longer fires even if price revisits 90.
        let fills = w.update("X", Candle::new(95.0, 96.0, 88.0, 89.0, 0.0));
        assert!(fills.is_empty());
    }

    #[test]
    fn cancel_protective_removes_both_legs() {
        let mut w: PaperWallet<&str> = PaperWallet::new(10_000.0);
        w.update("X", bar(100.0));
        w.set("X", Side::Buy, Size::units(1.0)).unwrap();
        w.update("X", bar(100.0));
        w.set_stop("X", Reference(90.0), Size::position_frac(1.0)).unwrap();
        w.cancel_protective(&"X").unwrap();
        let fills = w.update("X", Candle::new(95.0, 96.0, 88.0, 89.0, 0.0));
        assert!(fills.is_empty());
        assert!(!w.positions().is_empty());
    }

    #[test]
    fn a_sized_stop_takes_off_only_its_share() {
        // The capability a shared account needs: several owners resting exits
        // on one position, each reducing its own share rather than flattening
        // everyone. Previously inexpressible — a stop always targeted 0.
        let mut w: PaperWallet<&str> = PaperWallet::new(10_000.0);
        w.update("X", bar(100.0));
        w.set("X", Side::Buy, Size::units(10.0)).unwrap();
        w.update("X", bar(100.0));

        w.set_stop("X", Reference(90.0), Size::units(4.0)).unwrap();
        let fills = w.update("X", Candle::new(95.0, 96.0, 88.0, 89.0, 0.0));

        assert_eq!(fills.len(), 1);
        assert_eq!(fills[0].kind, OrderKind::Stop);
        assert!((fills[0].units - 4.0).abs() < 1e-9, "units {}", fills[0].units);
        assert!(
            (w.position(&"X").amount - 6.0).abs() < 1e-9,
            "6 units should survive the partial stop, got {}",
            w.position(&"X").amount,
        );
    }

    #[test]
    fn a_whole_position_stop_still_flattens() {
        // `position_frac(1.0)` is what every existing caller passes, and it has
        // to behave exactly as the old unsized leg did.
        let mut w: PaperWallet<&str> = PaperWallet::new(10_000.0);
        w.update("X", bar(100.0));
        w.set("X", Side::Buy, Size::units(10.0)).unwrap();
        w.update("X", bar(100.0));

        w.set_stop("X", Reference(90.0), Size::position_frac(1.0)).unwrap();
        let fills = w.update("X", Candle::new(95.0, 96.0, 88.0, 89.0, 0.0));

        assert_eq!(fills.len(), 1);
        assert!((fills[0].units - 10.0).abs() < 1e-9);
        assert!(w.positions().is_empty() || w.position(&"X").amount.abs() < 1e-9);
    }

    #[test]
    fn a_sized_stop_is_reduce_only_and_never_flips() {
        // An oversized share is clamped to the position rather than reversing
        // it — a protective leg must never open an opposite position.
        let mut w: PaperWallet<&str> = PaperWallet::new(10_000.0);
        w.update("X", bar(100.0));
        w.set("X", Side::Buy, Size::units(3.0)).unwrap();
        w.update("X", bar(100.0));

        w.set_stop("X", Reference(90.0), Size::units(50.0)).unwrap();
        w.update("X", Candle::new(95.0, 96.0, 88.0, 89.0, 0.0));

        assert!(
            w.position(&"X").amount.abs() < 1e-9,
            "expected flat, got {}",
            w.position(&"X").amount,
        );
    }

    #[test]
    fn a_sized_stop_on_a_short_covers_only_its_share() {
        // Mirror of the long case: "reduce" is toward zero from below.
        let mut w: PaperWallet<&str> = PaperWallet::new(10_000.0);
        w.update("X", bar(100.0));
        w.set("X", Side::Sell, Size::units(10.0)).unwrap();
        w.update("X", bar(100.0));

        w.set_stop("X", Reference(110.0), Size::units(4.0)).unwrap();
        let fills = w.update("X", Candle::new(105.0, 112.0, 104.0, 111.0, 0.0));

        assert_eq!(fills.len(), 1);
        assert!((fills[0].units - 4.0).abs() < 1e-9);
        assert!(
            (w.position(&"X").amount + 6.0).abs() < 1e-9,
            "6 short units should survive, got {}",
            w.position(&"X").amount,
        );
    }

    /// A self-contained strategy type: long the golden cross, flat the death
    /// cross, on a configurable symbol. It owns only the symbol and its signals;
    /// the wallet owns the portfolio.
    struct GoldenCross {
        symbol: &'static str,
        enter: Box<dyn Signal<crate::types::Snapshot<&'static str>>>,
        exit: Box<dyn Signal<crate::types::Snapshot<&'static str>>>,
    }
    impl GoldenCross {
        fn new(symbol: &'static str, fast: usize, slow: usize) -> Self {
            use crate::indicators::{Close, Pick};
            let close = || Close::of(Pick::<&'static str>::new());
            Self {
                symbol,
                enter: Box::new(
                    Sma::new(close(), fast).crosses_above(Sma::new(close(), slow)),
                ),
                exit: Box::new(
                    Sma::new(close(), fast).crosses_below(Sma::new(close(), slow)),
                ),
            }
        }
    }
    impl Strategy for GoldenCross {
        type Input = crate::types::Snapshot<&'static str>;
        type Symbol = &'static str;
        fn update(&mut self, snap: crate::types::Snapshot<&'static str>) {
            // Advance both signals every bar.
            self.enter.update(snap.clone());
            self.exit.update(snap);
        }
        fn trade(&self, wallet: &mut dyn Wallet<&'static str>) {
            let flat = wallet.position(&self.symbol).amount.abs() <= POSITION_EPSILON;
            if self.enter.is_true() && flat {
                let _ = wallet.set(self.symbol, Side::Buy, Size::value_frac(1.0));
            } else if self.exit.is_true() && !flat {
                let _ = wallet.close(self.symbol);
            }
        }
        fn reset(&mut self) {
            self.enter.reset();
            self.exit.reset();
        }
    }

    #[test]
    fn custom_strategy_trades_into_its_wallet() {
        let mut strat = GoldenCross::new("X", 2, 4);
        let mut w: PaperWallet<&'static str> = PaperWallet::new(1_000.0);
        // Decline first so the fast/slow MAs warm up with fast *below* slow, then
        // rally (a genuine golden cross) and fall again (a death cross). The
        // initial decline matters: a comparison reads `None` until warmed, so an
        // edge only registers once both MAs are ready — the cross must happen
        // after warm-up, not coincide with it.
        for px in [
            14.0, 13.0, 12.0, 11.0, 10.0, 11.0, 13.0, 15.0, 17.0, 15.0, 12.0, 9.0, 7.0,
        ] {
            w.update("X", bar(px));
            strat.update(crate::types::Snapshot::<&'static str>::of_atom(bar(px).into()));
            strat.trade(&mut w);
        }
        // Market orders fill a bar late, so settle any order the last bar queued.
        w.update("X", bar(7.0));
        // It entered and later exited at least once; ends flat with funds back.
        assert!(!w.orders().is_empty());
        assert!(w.positions().is_empty());
        assert!(w.funds().0 > 0.0);
    }

    /// A two-symbol snapshot — the multi-asset case.
    #[derive(Clone, Copy)]
    struct Pair {
        a: Real,
        b: Real,
    }

    /// A market-neutral pairs leg-in: while flat, go long A and short B. "Am I
    /// in?" is read from the wallet, not stored on the strategy.
    struct PairsTrade;
    impl Strategy for PairsTrade {
        type Input = Pair;
        type Symbol = &'static str;
        fn update(&mut self, _snap: Pair) {}
        fn trade(&self, wallet: &mut dyn Wallet<&'static str>) {
            if wallet.position(&"A").amount == 0.0 && wallet.position(&"B").amount == 0.0 {
                let _ = wallet.set_position(Units {
                    symbol: "A",
                    amount: 3.0,
                });
                let _ = wallet.set_position(Units {
                    symbol: "B",
                    amount: -2.0,
                });
            }
        }
        fn reset(&mut self) {}
    }

    #[test]
    fn multi_asset_strategy_acts_on_several_symbols_per_bar() {
        let mut strat = PairsTrade;
        let mut w: PaperWallet<&'static str> = PaperWallet::new(100_000.0);
        let snap = Pair { a: 10.0, b: 20.0 };
        w.update("A", bar(snap.a));
        w.update("B", bar(snap.b));
        strat.update(snap);
        strat.trade(&mut w); // queues both legs
        // The legs fill on each symbol's next bar, at its open.
        w.update("A", bar(snap.a));
        w.update("B", bar(snap.b));
        assert_eq!(w.orders().len(), 2);
        assert_fill(&w.orders()[0], Side::Buy, 3.0, 10.0, OrderKind::Market);
        assert_fill(&w.orders()[1], Side::Sell, 2.0, 20.0, OrderKind::Market);
        assert_eq!(w.position(&"A").amount, 3.0);
        assert_eq!(w.position(&"B").amount, -2.0);
        // Bought 3@10 (-30), shorted 2@20 (+40): net +10 vs start.
        assert_eq!(w.funds().0, 100_000.0 + 10.0);
    }

    /// A basket-shaped setup: no wallet default, every traded symbol enters
    /// via [`PaperWallet::set_costs_for`], and each pays its own commission.
    /// The shape a future N-symbol `BasketStrategy` would use.
    #[test]
    fn per_symbol_costs_scale_to_many_symbols() {
        use crate::costs::{FixedCommission, NoSlippage, NoSpread};
        let mut w: PaperWallet<&'static str> = PaperWallet::new(100_000.0);
        // Universe of five symbols, each on its own commission model.
        let universe = [("A", 1.0), ("B", 2.0), ("C", 3.0), ("D", 4.0), ("E", 5.0)];
        for &(sym, fee) in &universe {
            let _ = w.set_costs_for(
                sym,
                TradingCosts::new(
                    Box::new(FixedCommission::new(fee)),
                    Box::new(NoSpread),
                    Box::new(NoSlippage),
                ),
            );
        }
        // Prime every symbol, then queue and fill one buy per symbol.
        for &(sym, _) in &universe {
            w.update(sym, bar(10.0));
        }
        for &(sym, _) in &universe {
            w.set_position(Units { symbol: sym, amount: 1.0 }).unwrap();
        }
        for &(sym, _) in &universe {
            w.update(sym, bar(10.0));
        }
        for &(sym, expected) in &universe {
            let fill = w
                .orders()
                .iter()
                .find(|o| o.symbol == sym)
                .expect("fill");
            assert!(
                (fill.commission - expected).abs() < 1e-9,
                "{sym}: expected {expected}, got {}",
                fill.commission
            );
        }
    }

    /// A symbol with no per-symbol installation falls back to the wallet's
    /// default bundle — the safe zero-cost default when the wallet is built
    /// via [`PaperWallet::new`].
    #[test]
    fn fill_on_unconfigured_symbol_uses_default_costs() {
        use crate::costs::{FixedCommission, NoSlippage, NoSpread};
        let mut w: PaperWallet<&'static str> = PaperWallet::new(10_000.0);
        // Only "A" gets a custom model; "B" trades on the (zero-cost) fallback.
        let _ = w.set_costs_for(
            "A",
            TradingCosts::new(
                Box::new(FixedCommission::new(7.0)),
                Box::new(NoSpread),
                Box::new(NoSlippage),
            ),
        );
        w.update("A", bar(10.0));
        w.update("B", bar(20.0));
        w.set_position(Units { symbol: "A", amount: 1.0 }).unwrap();
        w.set_position(Units { symbol: "B", amount: 1.0 }).unwrap();
        w.update("A", bar(10.0));
        w.update("B", bar(20.0));
        let a = w.orders().iter().find(|o| o.symbol == "A").unwrap();
        let b = w.orders().iter().find(|o| o.symbol == "B").unwrap();
        assert!((a.commission - 7.0).abs() < 1e-9, "A: {}", a.commission);
        assert!(b.commission.abs() < 1e-9, "B (default): {}", b.commission);
    }

    #[test]
    fn per_symbol_costs_override_the_default_bundle() {
        use crate::costs::{FixedCommission, NoSlippage, NoSpread};
        // Default: $1 per fill. A leg gets its own override: $5 per fill.
        let default = TradingCosts::new(
            Box::new(FixedCommission::new(1.0)),
            Box::new(NoSpread),
            Box::new(NoSlippage),
        );
        let leg_override = TradingCosts::new(
            Box::new(FixedCommission::new(5.0)),
            Box::new(NoSpread),
            Box::new(NoSlippage),
        );
        let mut w: PaperWallet<&'static str> = PaperWallet::with_costs(100_000.0, default);
        let _ = w.set_costs_for("B", leg_override);
        // Prime both symbols and queue a buy on each.
        w.update("A", bar(10.0));
        w.update("B", bar(20.0));
        w.set_position(Units { symbol: "A", amount: 3.0 }).unwrap();
        w.set_position(Units { symbol: "B", amount: 2.0 }).unwrap();
        // Fill both at the next open.
        w.update("A", bar(10.0));
        w.update("B", bar(20.0));
        // A uses the default: $1 commission. B uses the override: $5.
        let a_fill = w.orders().iter().find(|o| o.symbol == "A").unwrap();
        let b_fill = w.orders().iter().find(|o| o.symbol == "B").unwrap();
        assert!((a_fill.commission - 1.0).abs() < 1e-9, "A: got {}", a_fill.commission);
        assert!((b_fill.commission - 5.0).abs() < 1e-9, "B: got {}", b_fill.commission);
        // Cash out: 100000 − (3·10 + 1) − (2·20 + 5) = 100000 − 31 − 45 = 99924.
        assert!((w.funds().0 - 99_924.0).abs() < 1e-6, "funds: {}", w.funds().0);
    }

    #[test]
    fn adjust_funds_credits_debits_and_rejects_overdraft() {
        let mut w: PaperWallet<&'static str> = PaperWallet::new(1_000.0);
        // Deposit / credit: funds go up, no blotter entry.
        assert!(w.adjust_funds(500.0).is_ok());
        assert_eq!(w.funds().0, 1_500.0);
        assert!(w.orders().is_empty());
        // Withdrawal within available: fine.
        assert!(w.adjust_funds(-1_000.0).is_ok());
        assert_eq!(w.funds().0, 500.0);
        // Overdraft: refused, funds unchanged.
        assert_eq!(w.adjust_funds(-1_000.0), Err(WalletError::InsufficientFunds));
        assert_eq!(w.funds().0, 500.0);
    }

    #[test]
    fn trait_default_adjust_funds_returns_unsupported() {
        // A minimal Wallet impl that only fills the required methods and
        // relies on the trait defaults — exercising the default
        // `adjust_funds` path a live-broker wallet without account-transfer
        // support would inherit unchanged.
        struct NoTransferWallet;
        impl Wallet<&'static str> for NoTransferWallet {
            fn funds(&self) -> Reference {
                Reference(0.0)
            }
            fn position(&self, symbol: &&'static str) -> Units<&'static str> {
                Units {
                    symbol: *symbol,
                    amount: 0.0,
                }
            }
            fn price(&self, _symbol: &&'static str) -> Option<Reference> {
                None
            }
            fn equity(&self) -> Reference {
                Reference(0.0)
            }
            fn update(&mut self, _symbol: &'static str, _candle: Candle) -> Vec<Order<&'static str>> {
                Vec::new()
            }
            fn set_position(
                &mut self,
                _target: Units<&'static str>,
            ) -> Result<Ack<&'static str>, WalletError> {
                Err(WalletError::UnsupportedOperation)
            }
            fn set_stop(
        &mut self,
        _symbol: &'static str,
        _trigger: Reference,
        _size: Size,
    ) -> Result<Ack<&'static str>, WalletError> {
                Err(WalletError::UnsupportedOperation)
            }
            fn set_take_profit(
        &mut self,
        _symbol: &'static str,
        _trigger: Reference,
        _size: Size,
    ) -> Result<Ack<&'static str>, WalletError> {
                Err(WalletError::UnsupportedOperation)
            }
            fn cancel_protective(&mut self, _symbol: &&'static str) -> Result<(), WalletError> {
                Ok(())
            }
        }
        let mut w = NoTransferWallet;
        assert_eq!(w.adjust_funds(100.0), Err(WalletError::UnsupportedOperation));
        assert_eq!(w.adjust_funds(-50.0), Err(WalletError::UnsupportedOperation));
        // The two Tier-B additions also fall through to their trait defaults:
        // no out-of-band fills, and cancel is unsupported for a bare impl.
        assert!(w.poll_fills().is_empty());
        assert_eq!(w.cancel(OrderId(7)), Err(WalletError::UnsupportedOperation));
    }

    #[test]
    fn paper_wallet_poll_fills_is_empty() {
        // A backtest never has out-of-band fills, so the paper impl keeps the
        // empty default — the driver draining it must be a no-op.
        let mut w: PaperWallet<&str> = PaperWallet::new(1_000.0);
        w.update("X", bar(100.0));
        w.set_position(Units { symbol: "X", amount: 3.0 }).unwrap();
        w.update("X", bar(100.0));
        assert!(w.poll_fills().is_empty());
    }

    #[test]
    fn cancel_drops_a_queued_market_order() {
        let mut w: PaperWallet<&str> = PaperWallet::new(1_000.0);
        w.update("X", bar(100.0));
        let id = match w.set_position(Units { symbol: "X", amount: 3.0 }).unwrap() {
            Ack::Working(id) => id,
            Ack::Filled(_) => panic!("market order should queue, not fill"),
        };
        // Cancel before the next bar flushes the queue -> no fill happens.
        assert_eq!(w.cancel(id), Ok(()));
        let fills = w.update("X", bar(100.0));
        assert!(fills.is_empty(), "cancelled order should not fill");
        assert_eq!(w.position(&"X").amount, 0.0);
        // Cancelling an unknown / already-gone id is a no-op, not an error.
        assert_eq!(w.cancel(id), Ok(()));
        assert_eq!(w.cancel(OrderId(999)), Ok(()));
    }

    #[test]
    fn cancel_clears_a_resting_protective_leg() {
        let mut w: PaperWallet<&str> = PaperWallet::new(10_000.0);
        w.update("X", bar(100.0));
        w.set("X", Side::Buy, Size::units(10.0)).unwrap();
        w.update("X", bar(100.0)); // long 10 @ 100
        let stop = match w.set_stop("X", Reference(90.0), Size::position_frac(1.0)).unwrap() {
            Ack::Working(id) => id,
            Ack::Filled(_) => panic!("resting order returns Working"),
        };
        w.set_take_profit("X", Reference(120.0), Size::position_frac(1.0)).unwrap();
        // Cancel only the stop; the take-profit leg survives and still fires.
        assert_eq!(w.cancel(stop), Ok(()));
        let through_stop = w.update("X", Candle::new(95.0, 96.0, 85.0, 88.0, 0.0));
        assert!(through_stop.is_empty(), "cancelled stop must not fill");
        assert_eq!(w.position(&"X").amount, 10.0);
        let through_tp = w.update("X", Candle::new(115.0, 125.0, 114.0, 121.0, 0.0));
        assert_eq!(through_tp.len(), 1, "take-profit leg should still fire");
        assert_fill(&through_tp[0], Side::Sell, 10.0, 120.0, OrderKind::TakeProfit);
    }
}
