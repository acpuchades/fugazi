//! The execution vocabulary: quantity epsilons, sizes, orders, and the
//! error/refusal types every [`Wallet`](crate::Wallet)(super::Wallet) impl speaks.
//!
//! Split out of the old single-file `wallet.rs` so a downstream crate
//! implementing `Wallet` for a real broker gets the vocabulary without
//! compiling [`PaperWallet`](crate::PaperWallet)(super::PaperWallet)'s fill-matching engine — the
//! same argument that moved `Strategy` out of this module earlier.

use std::fmt;

use serde::{Deserialize, Serialize};

use crate::types::Real;

/// **empty**, in the instrument's own units.
///
/// Absolute on purpose: it is a quantity, not a float-noise guard, and the unit
/// is fixed by the instrument rather than by whatever an expression produced.
/// (Contrast [`DEFAULT_TOLERANCE`](crate::indicators::DEFAULT_TOLERANCE), which
/// compares expression outputs of unbounded scale and is therefore relative.)
pub const POSITION_EPSILON: Real = 1e-8;

/// Slack when checking a computed fill price against the bar's `[low, high]`,
/// in price units — absorbs the rounding in a spread/slippage adjustment
/// without letting a genuinely out-of-range fill through.
pub const PRICE_EPSILON: Real = 1e-8;

/// Cash amount below which a balance counts as zero, and the relative term used
/// when the amount is large — see the crate-internal `cash_tolerance` helper.
pub const CASH_EPSILON: Real = 1e-8;

/// The tolerance for a cash comparison at `scale`: `CASH_EPSILON` near zero,
/// growing with the balance beyond that. Money spans many orders of magnitude
/// across accounts, so an all-in `value_frac(1.0)` on a large balance rounds by
/// more than a fixed `1e-8` and would otherwise read as insufficient funds.
pub(crate) fn cash_tolerance(scale: Real) -> Real {
    CASH_EPSILON * scale.abs().max(1.0)
}

/// Which way an [`Order`] trades, and the direction a [`set`](crate::Wallet::set)
/// targets.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
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
/// same units as [`funds`](crate::Wallet::funds) and [`equity`](crate::Wallet::equity), and
/// the worth of one unit of a symbol ([`price`](crate::Wallet::price)).
///
/// A distinct type from [`Units`] so a reference amount and a count of some
/// instrument's units can never be silently mixed.
#[derive(Debug, Clone, Copy, PartialEq, PartialOrd, Serialize, Deserialize)]
pub struct Reference(pub Real);

/// A signed quantity of one instrument's units (positive long, negative short),
/// tagged with the `symbol` it counts.
///
/// Returned by [`position`](crate::Wallet::position) and taken by
/// [`set_position`](crate::Wallet::set_position); distinct from a [`Reference`] amount
/// so instrument units and quote currency never silently mix.
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub struct Units<Sym> {
    /// The instrument these units count.
    pub symbol: Sym,
    /// The signed number of units (positive long, negative short).
    pub amount: Real,
}

/// How a [`set`](crate::Wallet::set) sizes the position it targets, resolved to a
/// magnitude in instrument units.
///
/// Absolute sizing is a plain unit count; relative sizing is a fraction of the
/// available **funds**, the total **equity** (funds plus all positions marked to
/// market), or the symbol's current **position** — the first two converted to
/// units at the current price. Fractions and unit counts are taken as magnitudes
/// (the sign comes from the trade's [`Side`]).
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
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
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
pub struct OrderId(pub u64);

/// What kind of order produced a fill: a plain **market** order, or one of the
/// two resting protective legs — a **stop**-loss or a **take-profit** — that the
/// wallet triggered against a bar's range.
///
/// Recorded on every [`Order`] so a backtest's blotter can tell an ordinary
/// next-open market fill apart from a stop/take-profit trigger fill.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum OrderKind {
    /// A market order (filled at the market — the next bar's `open` on a
    /// [`PaperWallet`](crate::PaperWallet)).
    Market,
    /// A resting stop-loss, triggered when the bar traded through its level.
    Stop,
    /// A resting take-profit, triggered when the bar traded through its level.
    TakeProfit,
    /// A resting limit order, filled when the bar trades through its price —
    /// at that price or better. Unlike the other three this is an *entry*
    /// instrument: it drives the position toward a target rather than
    /// flattening one.
    ///
    /// A limit fill is **passive**: it provides liquidity rather than taking
    /// it, so it crosses no spread and suffers no slippage (see
    /// `PaperWallet::fill_at` and `costs::kind_multiplier`). Anything else
    /// would let the cost pipeline fill it worse than the price the caller
    /// named, which is the one thing a limit order guarantees.
    Limit,
}

/// A single filled order: a `symbol`, a [`Side`], a strictly-positive number of
/// instrument units, the `price` it filled at, the [`OrderKind`] that produced
/// it, the [`OrderId`] of the submission it fills, and the per-fill
/// `commission` paid on top of the notional (in reference currency; `0.0`
/// unless a cost model set it).
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
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
    /// built with [`PaperWallet::new`](crate::PaperWallet::new)(crate::PaperWallet::new); populated on a
    /// wallet built with [`PaperWallet::with_costs`](crate::PaperWallet::with_costs)(crate::PaperWallet::with_costs)
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
    /// (within [`POSITION_EPSILON`]). Commission defaults to `0.0`.
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
        if delta.abs() <= POSITION_EPSILON {
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
/// order and works it, filling later (and a [`PaperWallet`](crate::PaperWallet) queues a market order
/// to the next bar's `open`). So a submission returns either the fill, if one
/// happened synchronously, or a handle to the working order whose fill will
/// arrive later — as an [`Order`] in the wallet's fill stream (see
/// [`Wallet::update`](crate::Wallet::update)), carrying the same [`OrderId`].
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum Ack<Sym> {
    /// The order filled immediately; here is the resulting [`Order`].
    Filled(Order<Sym>),
    /// The order was accepted and is working; its fill (if any) arrives later,
    /// tagged with this [`OrderId`].
    Working(OrderId),
}

/// Why a [`Wallet`](crate::Wallet) movement could not be carried out.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum WalletError {
    /// No price has been fed for the symbol (see [`Wallet::update`](crate::Wallet::update)), so the
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
    /// The operation is not supported by this wallet implementation. Returned
    /// by the default [`Wallet::adjust_funds`](crate::Wallet::adjust_funds) impl, which live-broker impls
    /// selectively override when their venue exposes a deposit / withdrawal /
    /// sub-account transfer API. Callers (e.g. [`Portfolio`](crate::portfolio::Portfolio)'s
    /// cash-phase rebalance) should treat this as "the transfer didn't
    /// happen" and fall back to trait-friendly alternatives (position resize).
    UnsupportedOperation,
    /// A live venue could not carry out the operation — the request failed at
    /// the broker (network error, HTTP error, an exchange rejection, or an
    /// unparseable response). The [`WalletError`] stays a small `Copy` enum, so
    /// this variant is a **category**, not the detail: a live wallet records
    /// the full error (endpoint, status, body) on an internal, queryable log
    /// and returns this to say "the venue leg failed". The [`PaperWallet`](crate::PaperWallet)
    /// never returns it.
    Venue,
}

impl fmt::Display for WalletError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            WalletError::UnknownPrice => f.write_str("no price has been fed for this symbol"),
            WalletError::InvalidPrice => f.write_str("the fed price is not strictly positive"),
            WalletError::UnsupportedOperation => {
                f.write_str("the operation is not supported by this wallet implementation")
            }
            WalletError::Venue => f.write_str("the live venue could not carry out the operation"),
            WalletError::PriceOutOfRange => {
                f.write_str("the fill price is outside the current candle's range")
            }
            WalletError::InsufficientFunds => f.write_str("insufficient funds for this buy"),
        }
    }
}

impl std::error::Error for WalletError {}

/// A queued order that [`PaperWallet`](crate::PaperWallet)'s [`update`](crate::Wallet::update) tried and failed to fill on a
/// given bar, along with the [`WalletError`] that blocked it and the
/// [`OrderId`] the submission returned in its [`Ack::Working`].
///
/// The wallet stashes one of these on every silent drop so a driver can
/// inspect why a bar produced no fill (typically `InsufficientFunds` for a
/// `Size::Units` buy larger than cash on hand, after the shrink helper
/// exempts fractional sizings). Query with
/// [`PaperWallet::rejections`](crate::PaperWallet::rejections)(crate::PaperWallet::rejections).
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub struct Rejection<Sym> {
    pub symbol: Sym,
    pub id: OrderId,
    pub error: WalletError,
    /// Whether the refused order was a plain market order or one of the resting
    /// protective legs. A protective leg fails at *trigger* time rather than at
    /// submission, and the distinction matters: a refused entry leaves the
    /// strategy flat when it wanted a position, while a refused stop leaves it
    /// holding one it wanted out of.
    pub kind: OrderKind,
}
