//! The [`Wallet`] trait a [`Strategy`](crate::Strategy) trades into, the pure
//! in-memory [`PaperWallet`] impl the crate ships, and the vocabulary in
//! between: [`Side`], [`Size`], [`Order`], the unit-tagged [`Reference`] /
//! [`Units`] amounts, and [`WalletError`].

use std::collections::HashMap;
use std::fmt;
use std::hash::Hash;

use crate::costs::TradingCosts;
use crate::indicators::DEFAULT_EPSILON;
use crate::types::{Candle, Real};

/// Which way an [`Order`] trades, and the direction a [`set`](Wallet::set)
/// targets.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Side {
    /// Increase the position (target/trade long).
    Buy,
    /// Decrease the position (target/trade short).
    Sell,
}

impl Side {
    /// `+1.0` for [`Buy`](Side::Buy), `-1.0` for [`Sell`](Side::Sell).
    pub fn sign(self) -> Real {
        match self {
            Side::Buy => 1.0,
            Side::Sell => -1.0,
        }
    }
}

/// An amount denominated in the wallet's **reference** (quote) currency — the
/// same units as [`funds`](Wallet::funds) and [`equity`](Wallet::equity), and
/// the worth of one unit of a symbol ([`price`](Wallet::price)).
///
/// A distinct type from [`Units`] so a reference amount and a count of some
/// instrument's units can never be silently mixed.
#[derive(Debug, Clone, Copy, PartialEq, PartialOrd)]
pub struct Reference(pub Real);

/// A signed quantity of one instrument's units (positive long, negative short),
/// tagged with the `symbol` it counts.
///
/// Returned by [`position`](Wallet::position) and taken by
/// [`set_position`](Wallet::set_position); distinct from a [`Reference`] amount
/// so instrument units and quote currency never silently mix.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Units<Sym> {
    /// The instrument these units count.
    pub symbol: Sym,
    /// The signed number of units (positive long, negative short).
    pub amount: Real,
}

/// How a [`set`](Wallet::set) sizes the position it targets, resolved to a
/// magnitude in instrument units.
///
/// Absolute sizing is a plain unit count; relative sizing is a fraction of the
/// available **funds**, the total **equity** (funds plus all positions marked to
/// market), or the symbol's current **position** — the first two converted to
/// units at the current price. Fractions and unit counts are taken as magnitudes
/// (the sign comes from the trade's [`Side`]).
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum Size {
    /// An absolute number of units.
    Units(Real),
    /// A fraction of available funds, converted to units at the current price:
    /// `fraction * funds / price`. Sizes against cash on hand.
    FundsFraction(Real),
    /// A fraction of total equity, converted to units at the current price:
    /// `fraction * equity / price`. `value_frac(1.0)` is "all-in", and resizes
    /// correctly on a reversal because equity (unlike cash) survives the flip.
    ValueFraction(Real),
    /// A fraction of the symbol's current position magnitude (adjust-only: from
    /// a flat position it resolves to zero).
    PositionFraction(Real),
}

impl Size {
    /// Sugar for [`Size::Units`].
    pub fn units(units: Real) -> Self {
        Size::Units(units)
    }
    /// Sugar for [`Size::FundsFraction`].
    pub fn funds_frac(fraction: Real) -> Self {
        Size::FundsFraction(fraction)
    }
    /// Sugar for [`Size::ValueFraction`].
    pub fn value_frac(fraction: Real) -> Self {
        Size::ValueFraction(fraction)
    }
    /// Sugar for [`Size::PositionFraction`].
    pub fn position_frac(fraction: Real) -> Self {
        Size::PositionFraction(fraction)
    }

    /// Resolve to a non-negative unit magnitude from the current `price`, the
    /// symbol's `position`, the wallet's available `funds`, and its total
    /// `equity`.
    pub fn resolve(&self, price: Real, position: Real, funds: Real, equity: Real) -> Real {
        match self {
            Size::Units(units) => units.abs(),
            Size::FundsFraction(fraction) => {
                if price > 0.0 {
                    (fraction.abs() * funds) / price
                } else {
                    0.0
                }
            }
            Size::ValueFraction(fraction) => {
                if price > 0.0 {
                    (fraction.abs() * equity) / price
                } else {
                    0.0
                }
            }
            Size::PositionFraction(fraction) => fraction.abs() * position.abs(),
        }
    }
}

/// A wallet-minted identifier for a submitted order, handed back in an [`Ack`] so
/// a later fill (carried on the resulting [`Order`]) can be correlated to the
/// submission that caused it. Unique within one wallet.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct OrderId(pub u64);

/// What kind of order produced a fill: a plain **market** order, or one of the
/// two resting protective legs — a **stop**-loss or a **take-profit** — that the
/// wallet triggered against a bar's range.
///
/// Recorded on every [`Order`] so a backtest's blotter can tell an ordinary
/// next-open market fill apart from a stop/take-profit trigger fill.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum OrderKind {
    /// A market order (filled at the market — the next bar's `open` on a
    /// [`PaperWallet`]).
    Market,
    /// A resting stop-loss, triggered when the bar traded through its level.
    Stop,
    /// A resting take-profit, triggered when the bar traded through its level.
    TakeProfit,
}

/// A single filled order: a `symbol`, a [`Side`], a strictly-positive number of
/// instrument units, the `price` it filled at, the [`OrderKind`] that produced
/// it, the [`OrderId`] of the submission it fills, and the per-fill
/// `commission` paid on top of the notional (in reference currency; `0.0`
/// unless a cost model set it).
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Order<Sym> {
    pub symbol: Sym,
    pub side: Side,
    pub units: Real,
    /// The per-unit price this order filled at (reference currency), post
    /// spread and slippage.
    pub price: Real,
    /// Whether this fill came from a market order or a resting stop/take-profit.
    pub kind: OrderKind,
    /// The id of the submission this fill belongs to (see [`Ack`]).
    pub id: OrderId,
    /// Commission paid on this fill, in reference currency. Zero on a wallet
    /// built with [`PaperWallet::new`](crate::PaperWallet::new); populated on a
    /// wallet built with [`PaperWallet::with_costs`](crate::PaperWallet::with_costs)
    /// whose [`TradingCosts::commission`](crate::costs::TradingCosts::commission)
    /// leg is non-trivial.
    pub commission: Real,
}

impl<Sym> Order<Sym> {
    /// A `side` order for `units` units of `symbol`, filled at `price` as `kind`,
    /// belonging to submission `id`. `commission` defaults to `0.0`; set it
    /// with [`with_commission`](Self::with_commission).
    pub fn new(
        symbol: Sym,
        side: Side,
        units: Real,
        price: Real,
        kind: OrderKind,
        id: OrderId,
    ) -> Self {
        Self {
            symbol,
            side,
            units,
            price,
            kind,
            id,
            commission: 0.0,
        }
    }

    /// Set this order's `commission` (in reference currency) — the leg the
    /// wallet stamps after applying its [`CommissionModel`](crate::costs::CommissionModel).
    ///
    /// [`CommissionModel`]: crate::costs::CommissionModel
    pub fn with_commission(mut self, commission: Real) -> Self {
        self.commission = commission;
        self
    }

    /// The order that moves `symbol`'s position by `delta` units, filled at
    /// `price` as `kind` for submission `id` — [`Buy`] for a positive delta,
    /// [`Sell`] for a negative one — or `None` when the delta is negligible
    /// (within [`DEFAULT_EPSILON`]). Commission defaults to `0.0`.
    ///
    /// [`Buy`]: Side::Buy
    /// [`Sell`]: Side::Sell
    pub fn from_delta(
        symbol: Sym,
        delta: Real,
        price: Real,
        kind: OrderKind,
        id: OrderId,
    ) -> Option<Self> {
        if delta.abs() <= DEFAULT_EPSILON {
            None
        } else if delta > 0.0 {
            Some(Order::new(symbol, Side::Buy, delta, price, kind, id))
        } else {
            Some(Order::new(symbol, Side::Sell, -delta, price, kind, id))
        }
    }

    /// The signed number of units this order trades: `+units` for a buy,
    /// `-units` for a sell.
    pub fn signed_units(&self) -> Real {
        match self.side {
            Side::Buy => self.units,
            Side::Sell => -self.units,
        }
    }
}

/// The synchronous acknowledgment of a submitted order.
///
/// Submitting an order is *not* the same as filling it: a live venue accepts an
/// order and works it, filling later (and a [`PaperWallet`] queues a market order
/// to the next bar's `open`). So a submission returns either the fill, if one
/// happened synchronously, or a handle to the working order whose fill will
/// arrive later — as an [`Order`] in the wallet's fill stream (see
/// [`Wallet::update`]), carrying the same [`OrderId`].
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum Ack<Sym> {
    /// The order filled immediately; here is the resulting [`Order`].
    Filled(Order<Sym>),
    /// The order was accepted and is working; its fill (if any) arrives later,
    /// tagged with this [`OrderId`].
    Working(OrderId),
}

/// Why a [`Wallet`] movement could not be carried out.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WalletError {
    /// No price has been fed for the symbol (see [`Wallet::update`]), so the
    /// movement can't be valued or booked.
    UnknownPrice,
    /// The fed price is not strictly positive, so it can't value or book a
    /// movement.
    InvalidPrice,
    /// The requested fill price lies outside the symbol's current candle range
    /// `[low, high]`, so it could not have traded on this bar.
    PriceOutOfRange,
    /// A net buy would drive cash below zero, and the wallet allows no margin.
    /// (A short sale credits cash, so selling is always feasible.)
    InsufficientFunds,
}

impl fmt::Display for WalletError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            WalletError::UnknownPrice => f.write_str("no price has been fed for this symbol"),
            WalletError::InvalidPrice => f.write_str("the fed price is not strictly positive"),
            WalletError::PriceOutOfRange => {
                f.write_str("the fill price is outside the current candle's range")
            }
            WalletError::InsufficientFunds => f.write_str("insufficient funds for this buy"),
        }
    }
}

impl std::error::Error for WalletError {}

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

    /// Rest a **stop-loss** on `symbol` at `trigger`: an adverse level the wallet
    /// fills when a bar trades through it (a long fills when the bar trades down to
    /// `trigger`, a short when it trades up). The side is read from the current
    /// position. Idempotent and latest-wins per symbol — re-submit each bar to
    /// trail. Returns the [`OrderId`] of the resting order in an [`Ack::Working`].
    fn set_stop(&mut self, symbol: Sym, trigger: Reference) -> Result<Ack<Sym>, WalletError>;

    /// Rest a **take-profit** on `symbol` at `trigger` — the favourable twin of
    /// [`set_stop`](Wallet::set_stop). Idempotent and latest-wins per symbol.
    fn set_take_profit(&mut self, symbol: Sym, trigger: Reference)
    -> Result<Ack<Sym>, WalletError>;

    /// Cancel both resting protective legs (stop and take-profit) on `symbol`.
    fn cancel_protective(&mut self, symbol: &Sym) -> Result<(), WalletError>;
}

/// A market order queued on a [`PaperWallet`] to fill at the next bar's `open`.
///
/// The two market entry points differ only in *when* the target is known: an
/// absolute unit target is fixed at queue time, while a [`Side`] + [`Size`] is
/// resolved against the fill (`open`) price — so an all-in
/// [`value_frac(1.0)`](Size::value_frac) stays affordable even when the bar gaps.
/// Each carries the [`OrderId`] minted when it was submitted.
#[derive(Debug, Clone, Copy)]
enum Pending {
    /// Drive to an absolute signed-unit target (from [`set_position`](Wallet::set_position)).
    Target(Real, OrderId),
    /// A side + size, resolved against the fill bar's `open` (from [`set`](Wallet::set)).
    Sized(Side, Size, OrderId),
}

/// One resting protective leg: the `trigger` level and the [`OrderId`] it fills
/// under.
#[derive(Debug, Clone, Copy)]
struct Leg {
    trigger: Real,
    id: OrderId,
}

/// The resting protective bracket for a symbol — a stop-loss and/or take-profit
/// leg. Holding both together makes them one-cancels-the-other: a fill on either
/// (or a market exit/reversal) drops the whole record.
#[derive(Debug, Clone, Copy, Default)]
struct Protective {
    stop: Option<Leg>,
    take_profit: Option<Leg>,
}

/// A queued order that [`PaperWallet::update`] tried and failed to fill on a
/// given bar, along with the [`WalletError`] that blocked it and the
/// [`OrderId`] the submission returned in its [`Ack::Working`].
///
/// The wallet stashes one of these on every silent drop so a driver can
/// inspect why a bar produced no fill (typically `InsufficientFunds` for a
/// `Size::Units` buy larger than cash on hand, after the shrink helper
/// exempts fractional sizings). Query with
/// [`PaperWallet::rejections`](PaperWallet::rejections).
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Rejection<Sym> {
    pub symbol: Sym,
    pub id: OrderId,
    pub error: WalletError,
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
    funds: Real,
    initial_funds: Real,
    blotter: Vec<Order<Sym>>,
    rejections: Vec<Rejection<Sym>>,
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
            funds,
            initial_funds: funds,
            blotter: Vec::new(),
            rejections: Vec::new(),
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
        self.blotter.clear();
        self.rejections.clear();
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

impl<Sym: Clone + Eq + Hash> PaperWallet<Sym> {
    /// Install a per-symbol [`TradingCosts`] override. Every fill on `symbol`
    /// thereafter routes through the given bundle instead of the default set
    /// by [`with_costs`](Self::with_costs); fills on other symbols still see
    /// the default. Latest-wins per symbol.
    ///
    /// Scales to any number of symbols — a multi-asset driver just calls this
    /// once per traded symbol (the pairs CLI does it for `left`/`right`; a
    /// future N-symbol basket driver would loop over its universe). The
    /// wallet's fallback default doubles as the "unscoped" model for symbols
    /// the caller doesn't explicitly configure; using [`Self::new`]
    /// (zero-cost default) plus per-symbol installs gives a fully symmetric,
    /// N-way cost model where every priced leg is a per-symbol entry.
    pub fn set_costs_for(&mut self, symbol: Sym, costs: TradingCosts) {
        self.per_symbol_costs.insert(symbol, costs);
    }

    /// Iterate the held positions.
    pub fn positions(&self) -> impl Iterator<Item = Units<Sym>> + '_ {
        self.positions.iter().map(|(symbol, &amount)| Units {
            symbol: symbol.clone(),
            amount,
        })
    }

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
        if delta.abs() <= DEFAULT_EPSILON {
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
        if theoretical_price < bar.low - DEFAULT_EPSILON
            || theoretical_price > bar.high + DEFAULT_EPSILON
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
        let half_spread = costs.spread.half_spread(theoretical_price, &bar);
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
            let tolerance = DEFAULT_EPSILON * self.funds.abs().max(1.0);
            if cost - self.funds > tolerance {
                return Err(WalletError::InsufficientFunds);
            }
        }
        let order = Order::from_delta(symbol.clone(), delta, final_price, kind, id)
            .expect("delta exceeds DEFAULT_EPSILON, so the order is non-empty")
            .with_commission(commission);
        // Pay for a buy, receive for a sell — and pay commission out of cash
        // on both sides.
        self.funds -= order.signed_units() * final_price + commission;
        let new_position = current + delta;
        if new_position.abs() <= DEFAULT_EPSILON {
            self.positions.remove(&symbol);
        } else {
            self.positions.insert(symbol.clone(), new_position);
        }
        // A fill that flattens or flips the sign voids any resting bracket (so a
        // bare market exit / reversal drops a now-stale stop even without an
        // explicit cancel).
        if new_position.abs() <= DEFAULT_EPSILON || current * new_position < 0.0 {
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
    fn shrink_buy_to_fit(
        &self,
        symbol: &Sym,
        side: Side,
        current: Real,
        magnitude: Real,
        candle: &Candle,
    ) -> Real {
        if magnitude <= 0.0 {
            return magnitude;
        }
        // A sell (delta < 0) credits cash and always fits.
        let target = side.sign() * magnitude;
        if target - current <= 0.0 {
            return magnitude;
        }
        let costs = self.per_symbol_costs.get(symbol).unwrap_or(&self.costs);
        let tolerance = DEFAULT_EPSILON * self.funds.abs().max(1.0);
        let mut m = magnitude;
        for _ in 0..8 {
            let delta = side.sign() * m - current;
            if delta <= 0.0 {
                return m.max(0.0);
            }
            let half_spread = costs.spread.half_spread(candle.open, candle);
            let post_spread = candle.open + half_spread; // net buy
            let final_price =
                costs.slippage.adjust(Side::Buy, post_spread, delta, candle, OrderKind::Market);
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
            if (m - next).abs() <= DEFAULT_EPSILON * m.abs().max(1.0) {
                return next.max(0.0);
            }
            m = next;
        }
        m.max(0.0)
    }

    /// Trigger and fill a resting protective leg on `symbol` against `candle`, if
    /// one is crossed. Stop-loss takes precedence over take-profit, and at most one
    /// leg fills per bar (the fill flattens, which drops the whole bracket).
    fn match_protective(&mut self, symbol: &Sym, candle: &Candle) -> Option<Order<Sym>> {
        let pos = self.positions.get(symbol).copied().unwrap_or(0.0);
        let prot = *self.protective.get(symbol)?;
        // Downside exits (long stop, short target) fill at the level, or lower at
        // the open on a gap: `min(level, open)`. Upside exits are `max`. Either way
        // the fill stays within the bar's range.
        let (leg, fill, kind) = if pos > DEFAULT_EPSILON {
            if let Some(leg) = prot.stop
                && candle.low <= leg.trigger + DEFAULT_EPSILON
            {
                (leg, leg.trigger.min(candle.open), OrderKind::Stop)
            } else if let Some(leg) = prot.take_profit
                && candle.high >= leg.trigger - DEFAULT_EPSILON
            {
                (leg, leg.trigger.max(candle.open), OrderKind::TakeProfit)
            } else {
                return None;
            }
        } else if pos < -DEFAULT_EPSILON {
            if let Some(leg) = prot.stop
                && candle.high >= leg.trigger - DEFAULT_EPSILON
            {
                (leg, leg.trigger.max(candle.open), OrderKind::Stop)
            } else if let Some(leg) = prot.take_profit
                && candle.low <= leg.trigger + DEFAULT_EPSILON
            {
                (leg, leg.trigger.min(candle.open), OrderKind::TakeProfit)
            } else {
                return None;
            }
        } else {
            return None;
        };
        self.fill_at(symbol.clone(), 0.0, fill, kind, leg.id)
            .ok()
            .flatten()
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
                            .shrink_buy_to_fit(&symbol, side, position, magnitude, &candle),
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
                }),
            }
        }
        if let Some(order) = self.match_protective(&symbol, &candle) {
            fills.push(order);
        }
        fills
    }

    fn set_position(&mut self, target: Units<Sym>) -> Result<Ack<Sym>, WalletError> {
        // Market order: queue the absolute target to fill at the next bar's open.
        let id = self.mint();
        self.pending
            .insert(target.symbol, Pending::Target(target.amount, id));
        Ok(Ack::Working(id))
    }

    fn set(&mut self, symbol: Sym, side: Side, size: Size) -> Result<Ack<Sym>, WalletError> {
        // Market order: queue the side+size and resolve it at the fill (open).
        let id = self.mint();
        self.pending.insert(symbol, Pending::Sized(side, size, id));
        Ok(Ack::Working(id))
    }

    fn set_stop(&mut self, symbol: Sym, trigger: Reference) -> Result<Ack<Sym>, WalletError> {
        let id = self.mint();
        self.protective.entry(symbol).or_default().stop = Some(Leg {
            trigger: trigger.0,
            id,
        });
        Ok(Ack::Working(id))
    }

    fn set_take_profit(
        &mut self,
        symbol: Sym,
        trigger: Reference,
    ) -> Result<Ack<Sym>, WalletError> {
        let id = self.mint();
        self.protective.entry(symbol).or_default().take_profit = Some(Leg {
            trigger: trigger.0,
            id,
        });
        Ok(Ack::Working(id))
    }

    fn cancel_protective(&mut self, symbol: &Sym) -> Result<(), WalletError> {
        self.protective.remove(symbol);
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
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
        assert!(w.positions().next().is_none());
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
    fn infeasible_units_buy_is_recorded_as_a_rejection() {
        // A caller-explicit Size::Units target that costs more than funds does
        // not shrink (it carries a specific unit intent). Instead of the
        // pre-fix silent drop, the wallet now records a Rejection queryable
        // via `rejections()`.
        let mut w: PaperWallet<&str> = PaperWallet::new(100.0);
        w.update("X", bar(50.0));
        // 3 units @ 50 = 150 > funds 100.
        let ack = w.set("X", Side::Buy, Size::units(3.0)).unwrap();
        let id = match ack {
            Ack::Working(id) => id,
            Ack::Filled(_) => panic!("market order should queue, not fill"),
        };
        let fills = w.update("X", bar(50.0));
        assert!(fills.is_empty(), "expected no fill");
        assert!(w.positions().next().is_none());
        assert_eq!(w.rejections().len(), 1);
        assert_eq!(w.rejections()[0].symbol, "X");
        assert_eq!(w.rejections()[0].id, id);
        assert_eq!(w.rejections()[0].error, WalletError::InsufficientFunds);
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
    fn unknown_price_is_flagged_on_a_fill() {
        let mut w: PaperWallet<&str> = PaperWallet::new(1_000.0);
        // "X" was never fed a bar, so a fill can't be priced; a market order just
        // queues (it resolves at the fill, which has a price).
        assert_eq!(
            w.fill_at("X", 1.0, 50.0, OrderKind::Market, OrderId(0)),
            Err(WalletError::UnknownPrice)
        );
        assert!(matches!(
            w.set_position(Units {
                symbol: "X",
                amount: 1.0
            }),
            Ok(Ack::Working(_))
        ));
    }

    #[test]
    fn insufficient_funds_is_flagged_but_shorts_are_free() {
        let mut w: PaperWallet<&str> = PaperWallet::new(100.0);
        w.update("X", bar(50.0));
        // 3 units cost 150 > 100 funds, and there is no margin.
        assert_eq!(
            w.fill_at("X", 3.0, 50.0, OrderKind::Market, OrderId(0)),
            Err(WalletError::InsufficientFunds)
        );
        // A queued buy beyond funds simply never fills (the error is swallowed).
        w.set("X", Side::Buy, Size::units(3.0)).unwrap();
        w.update("X", bar(50.0));
        assert!(w.positions().next().is_none());
        // A short sale credits cash, so selling is always feasible.
        w.set("X", Side::Sell, Size::units(3.0)).unwrap();
        w.update("X", bar(50.0));
        assert_eq!(w.position(&"X").amount, -3.0);
    }

    #[test]
    fn non_positive_price_is_flagged() {
        let mut w: PaperWallet<&str> = PaperWallet::new(1_000.0);
        w.update("X", bar(0.0));
        assert_eq!(
            w.fill_at("X", 1.0, 0.0, OrderKind::Market, OrderId(0)),
            Err(WalletError::InvalidPrice)
        );
        // A queued order against a zero open likewise never fills.
        w.set("X", Side::Buy, Size::value_frac(1.0)).unwrap();
        w.update("X", bar(0.0));
        assert!(w.positions().next().is_none());
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
        w.set_stop("X", Reference(90.0)).unwrap();
        // The bar trades down through 90 (low 88) but opens above it.
        let fills = w.update("X", Candle::new(95.0, 96.0, 88.0, 89.0, 0.0));
        assert_eq!(fills.len(), 1);
        assert_fill(&fills[0], Side::Sell, 1.0, 90.0, OrderKind::Stop);
        assert!(w.positions().next().is_none());
    }

    #[test]
    fn resting_stop_gaps_to_the_open() {
        let mut w: PaperWallet<&str> = PaperWallet::new(10_000.0);
        w.update("X", bar(100.0));
        w.set("X", Side::Buy, Size::units(1.0)).unwrap();
        w.update("X", bar(100.0));
        w.set_stop("X", Reference(90.0)).unwrap();
        // Gaps down opening at 85, already below the stop -> fills at the open.
        let fills = w.update("X", Candle::new(85.0, 86.0, 84.0, 84.0, 0.0));
        assert_fill(&fills[0], Side::Sell, 1.0, 85.0, OrderKind::Stop);
        assert!(w.positions().next().is_none());
    }

    #[test]
    fn resting_take_profit_on_a_short_fills_at_the_level() {
        let mut w: PaperWallet<&str> = PaperWallet::new(10_000.0);
        w.update("X", bar(100.0));
        w.set("X", Side::Sell, Size::units(1.0)).unwrap();
        w.update("X", bar(100.0)); // short 1 @ 100
        // A short take-profit sits below entry; the bar trades down to it.
        w.set_take_profit("X", Reference(90.0)).unwrap();
        let fills = w.update("X", Candle::new(95.0, 96.0, 88.0, 92.0, 0.0));
        assert_fill(&fills[0], Side::Buy, 1.0, 90.0, OrderKind::TakeProfit);
        assert!(w.positions().next().is_none());
    }

    #[test]
    fn oco_stop_takes_precedence_and_cancels_the_target() {
        let mut w: PaperWallet<&str> = PaperWallet::new(10_000.0);
        w.update("X", bar(100.0));
        w.set("X", Side::Buy, Size::units(1.0)).unwrap();
        w.update("X", bar(100.0));
        w.set_stop("X", Reference(90.0)).unwrap();
        w.set_take_profit("X", Reference(110.0)).unwrap();
        // A wide bar crosses both legs; the stop wins, and the fill flattens and
        // drops the whole bracket.
        let fills = w.update("X", Candle::new(100.0, 111.0, 89.0, 105.0, 0.0));
        assert_eq!(fills.len(), 1);
        assert_eq!(fills[0].kind, OrderKind::Stop);
        assert!(w.positions().next().is_none());
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
        w.set_stop("X", Reference(90.0)).unwrap();
        // Flatten with a market close; the fill drops the resting stop.
        w.close("X").unwrap();
        w.update("X", bar(100.0));
        assert!(w.positions().next().is_none());
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
        w.set_stop("X", Reference(90.0)).unwrap();
        w.cancel_protective(&"X").unwrap();
        let fills = w.update("X", Candle::new(95.0, 96.0, 88.0, 89.0, 0.0));
        assert!(fills.is_empty());
        assert!(w.positions().next().is_some());
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
            let flat = wallet.position(&self.symbol).amount.abs() <= DEFAULT_EPSILON;
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
        assert!(w.positions().next().is_none());
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
            w.set_costs_for(
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
        w.set_costs_for(
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
        w.set_costs_for("B", leg_override);
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
}
