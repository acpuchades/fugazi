//! The core [`Strategy`] trait, the [`Wallet`] it trades into, and the order
//! vocabulary in between: [`Side`], [`Size`], [`Action`], [`Order`], and the
//! [`Market`] pricing view.

use std::collections::HashMap;
use std::hash::Hash;

use crate::signals::DEFAULT_EPSILON;
use crate::types::{Candle, Real};

/// An incremental trading strategy — the *decision* layer above indicators and
/// signals.
///
/// Like an [`Indicator`](crate::Indicator) and a [`Signal`](crate::Signal), a
/// strategy is advanced one bar at a time. But where those layers are pure
/// value-producers, a strategy *acts*: each bar it reads the input and opens,
/// modifies, or closes positions on a [`Wallet`] handed to it. The wallet — not
/// the strategy — owns the portfolio state (funds and positions), so one wallet
/// can be inspected directly, shared across strategies, or swapped out, and the
/// strategy itself stays a thin holder of its signals.
///
/// Because [`Wallet`] is a trait taken as `&mut dyn`, the same strategy runs
/// against a [`PaperWallet`] in a backtest or a live broker wallet unchanged.
/// A strategy emits nothing on a quiet bar simply by not touching the wallet.
pub trait Strategy {
    /// The per-bar input — commonly a [`Candle`], or a multi-asset snapshot.
    type Input;

    /// The symbol type identifying instruments in the [`Wallet`].
    type Symbol;

    /// Read the next bar and act on `wallet` — opening, modifying, or closing
    /// positions as the strategy's logic dictates.
    fn evaluate(&mut self, input: Self::Input, wallet: &mut dyn Wallet<Self::Symbol>);

    /// Clear the strategy's own state (its signals/rules), returning it to its
    /// freshly-constructed condition. Does not touch any wallet.
    fn reset(&mut self);
}

/// Which way an [`Order`] trades, and the direction an [`Action`] targets.
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

/// How an [`Action`] sizes the position it targets, resolved to a magnitude in
/// instrument units.
///
/// Absolute sizing is a plain unit count; relative sizing is a fraction of
/// either the available **funds** (converted to units at the current price) or
/// the symbol's current **position**. Fractions and unit counts are taken as
/// magnitudes (the sign comes from the trade's [`Side`]).
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum Size {
    /// An absolute number of units.
    Units(Real),
    /// A fraction of available funds, converted to units at the current price:
    /// `fraction * funds / price`.
    FundsFraction(Real),
    /// A fraction of the symbol's current position magnitude.
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
    /// Sugar for [`Size::PositionFraction`].
    pub fn position_frac(fraction: Real) -> Self {
        Size::PositionFraction(fraction)
    }

    /// Resolve to a non-negative unit magnitude given the current `price`, the
    /// symbol's current `position`, and the strategy's available `funds`.
    pub fn resolve(&self, price: Real, position: Real, funds: Real) -> Real {
        match self {
            Size::Units(units) => units.abs(),
            Size::FundsFraction(fraction) => {
                if price > 0.0 {
                    (fraction.abs() * funds) / price
                } else {
                    0.0
                }
            }
            Size::PositionFraction(fraction) => fraction.abs() * position.abs(),
        }
    }
}

/// A single order: a `symbol`, a [`Side`], and a strictly-positive quantity in
/// instrument units.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Order<Sym> {
    pub symbol: Sym,
    pub side: Side,
    pub quantity: Real,
}

impl<Sym> Order<Sym> {
    /// A `side` order for `quantity` units of `symbol`.
    pub fn new(symbol: Sym, side: Side, quantity: Real) -> Self {
        Self {
            symbol,
            side,
            quantity,
        }
    }

    /// The order that moves `symbol`'s position by `delta` units — [`Buy`] for a
    /// positive delta, [`Sell`] for a negative one — or `None` when the delta is
    /// negligible (within [`DEFAULT_EPSILON`]).
    ///
    /// [`Buy`]: Side::Buy
    /// [`Sell`]: Side::Sell
    pub fn from_delta(symbol: Sym, delta: Real) -> Option<Self> {
        if delta.abs() <= DEFAULT_EPSILON {
            None
        } else if delta > 0.0 {
            Some(Order::new(symbol, Side::Buy, delta))
        } else {
            Some(Order::new(symbol, Side::Sell, -delta))
        }
    }

    /// The signed quantity this order trades: `+quantity` for a buy,
    /// `-quantity` for a sell.
    pub fn signed_quantity(&self) -> Real {
        match self.side {
            Side::Buy => self.quantity,
            Side::Sell => -self.quantity,
        }
    }
}

/// A per-bar pricing view: the price at which a strategy values and transacts
/// each `symbol` on the current bar.
///
/// A strategy's input must provide this so it can convert funds-relative
/// [`Size`]s to units and mark assumed fills against its funds. A single
/// instrument is the trivial case — a [`Candle`] prices any symbol at its close
/// — while a multi-asset snapshot returns a different price per symbol.
pub trait Market<Sym> {
    /// The price for `symbol` on this bar.
    fn price(&self, symbol: &Sym) -> Real;
}

/// A single instrument: every symbol is priced at this bar's close.
impl<Sym> Market<Sym> for Candle {
    fn price(&self, _symbol: &Sym) -> Real {
        self.close
    }
}

/// The portfolio interface a [`Strategy`] trades into: query `funds` and
/// positions, and execute trades.
///
/// `Wallet` is a trait so it is the single **seam** between pure arcana and a
/// downstream execution system. arcana ships only the pure, in-memory
/// [`PaperWallet`] (for backtests and dry runs); a downstream crate that imports
/// arcana can implement `Wallet` with a type whose [`trade`](Wallet::trade)
/// publishes a message onto an event bus / routes to a broker instead of booking
/// in memory. That way all market-specific, side-effecting code stays out of
/// arcana, behind this interface.
///
/// Implementors provide just three primitives — [`funds`](Wallet::funds),
/// [`position`](Wallet::position), and [`trade`](Wallet::trade) (execute a signed
/// delta at a price). The ergonomic [`open`](Wallet::open) (additive),
/// [`set`](Wallet::set) (absolute target), and [`close`](Wallet::close) are
/// default methods built on those, so the additive/absolute/relative-[`Size`]
/// logic lives here once and every implementation inherits it.
pub trait Wallet<Sym> {
    /// The available cash balance.
    fn funds(&self) -> Real;

    /// The current position in `symbol` (signed: positive long, negative short,
    /// zero flat).
    fn position(&self, symbol: &Sym) -> Real;

    /// The execution primitive: trade `delta` units of `symbol` (positive buys,
    /// negative sells), assumed to fill at `price`. Implementors carry out the
    /// trade — update a paper book, or publish to a bus / broker — and return the
    /// resulting [`Order`], or `None` if `delta` is negligible.
    fn trade(&mut self, symbol: Sym, delta: Real, price: Real) -> Option<Order<Sym>>;

    /// **Additively** trade `side · size` of `symbol` at `price`, adding to
    /// whatever is already held — open or scale a position. Re-firing
    /// accumulates.
    fn open(&mut self, symbol: Sym, side: Side, size: Size, price: Real) -> Option<Order<Sym>> {
        let current = self.position(&symbol);
        let magnitude = size.resolve(price, current, self.funds());
        self.trade(symbol, side.sign() * magnitude, price)
    }

    /// **Set** the target position in `symbol` to `side · size` at `price`,
    /// trading the difference. Re-firing the opposite side reverses; the same
    /// side is idempotent. ([`close`](Wallet::close) is this with a flat target.)
    fn set(&mut self, symbol: Sym, side: Side, size: Size, price: Real) -> Option<Order<Sym>> {
        let current = self.position(&symbol);
        let magnitude = size.resolve(price, current, self.funds());
        self.trade(symbol, side.sign() * magnitude - current, price)
    }

    /// Flatten `symbol` at `price`.
    fn close(&mut self, symbol: Sym, price: Real) -> Option<Order<Sym>> {
        let current = self.position(&symbol);
        self.trade(symbol, -current, price)
    }
}

/// The built-in **pure**, in-memory [`Wallet`]: a paper book of `funds`,
/// per-symbol positions, and a blotter of executed [`Order`]s, with no IO.
///
/// [`trade`](Wallet::trade) assumes the fill at the passed price and books it —
/// debiting a buy, crediting a sell. Use it for backtests and dry runs; a
/// downstream `Wallet` impl handles live execution / bus publishing.
#[derive(Debug, Clone)]
pub struct PaperWallet<Sym> {
    positions: HashMap<Sym, Real>,
    funds: Real,
    blotter: Vec<Order<Sym>>,
}

impl<Sym> PaperWallet<Sym> {
    /// A wallet seeded with `funds` of cash and no positions.
    pub fn new(funds: Real) -> Self {
        Self {
            positions: HashMap::new(),
            funds,
            blotter: Vec::new(),
        }
    }

    /// Whether no positions are currently held.
    pub fn is_flat(&self) -> bool {
        self.positions.is_empty()
    }

    /// Every order executed so far, in order (the trade blotter).
    pub fn orders(&self) -> &[Order<Sym>] {
        &self.blotter
    }

    /// Drop the blotter history (positions and funds are untouched).
    pub fn clear_blotter(&mut self) {
        self.blotter.clear();
    }
}

impl<Sym: Clone + Eq + Hash> PaperWallet<Sym> {
    /// Iterate the held (symbol, signed-quantity) positions.
    pub fn positions(&self) -> impl Iterator<Item = (&Sym, Real)> {
        self.positions.iter().map(|(s, &q)| (s, q))
    }

    /// Mark-to-market equity: funds plus every position valued at `market`.
    pub fn equity<M: Market<Sym>>(&self, market: &M) -> Real {
        self.funds
            + self
                .positions
                .iter()
                .map(|(symbol, &qty)| qty * market.price(symbol))
                .sum::<Real>()
    }
}

impl<Sym: Clone + Eq + Hash> Wallet<Sym> for PaperWallet<Sym> {
    fn funds(&self) -> Real {
        self.funds
    }

    fn position(&self, symbol: &Sym) -> Real {
        self.positions.get(symbol).copied().unwrap_or(0.0)
    }

    fn trade(&mut self, symbol: Sym, delta: Real, price: Real) -> Option<Order<Sym>> {
        let order = Order::from_delta(symbol.clone(), delta)?;
        // Pay for a buy, receive for a sell.
        self.funds -= order.signed_quantity() * price;
        let new_position = self.position(&symbol) + delta;
        if new_position.abs() <= DEFAULT_EPSILON {
            self.positions.remove(&symbol);
        } else {
            self.positions.insert(symbol, new_position);
        }
        self.blotter.push(order.clone());
        Some(order)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::indicators::{Current, Sma};
    use crate::signal::Signal;
    use crate::signals::IndicatorExt;

    fn bar(close: Real) -> Candle {
        Candle::new(close, close, close, close, 0.0)
    }

    #[test]
    fn open_is_additive_and_books_funds() {
        let mut w: PaperWallet<&str> = PaperWallet::new(1_000.0);
        assert_eq!(w.open("X", Side::Buy, Size::units(3.0), 100.0), Some(Order::new("X", Side::Buy, 3.0)));
        // Re-firing accumulates.
        w.open("X", Side::Buy, Size::units(2.0), 100.0);
        assert_eq!(w.position(&"X"), 5.0);
        assert_eq!(w.funds(), 1_000.0 - 5.0 * 100.0);
    }

    #[test]
    fn set_is_absolute_and_reverses() {
        let mut w: PaperWallet<&str> = PaperWallet::new(10_000.0);
        w.set("X", Side::Buy, Size::units(4.0), 50.0);
        assert!(w.set("X", Side::Buy, Size::units(4.0), 50.0).is_none()); // idempotent
        // Opposite side reverses: +4 -> -4 is a sell of 8.
        assert_eq!(w.set("X", Side::Sell, Size::units(4.0), 50.0), Some(Order::new("X", Side::Sell, 8.0)));
        assert_eq!(w.position(&"X"), -4.0);
    }

    #[test]
    fn close_flattens() {
        let mut w: PaperWallet<&str> = PaperWallet::new(1_000.0);
        w.open("X", Side::Buy, Size::units(10.0), 100.0);
        assert_eq!(w.close("X", 110.0), Some(Order::new("X", Side::Sell, 10.0)));
        assert!(w.is_flat());
        assert_eq!(w.funds(), 1_100.0);
    }

    #[test]
    fn relative_sizing_resolves_against_funds_and_position() {
        let mut w: PaperWallet<&str> = PaperWallet::new(1_000.0);
        // 10% of 1000 = 100 / price 25 = 4 units.
        assert_eq!(w.open("X", Side::Buy, Size::funds_frac(0.1), 25.0), Some(Order::new("X", Side::Buy, 4.0)));
        // Set to 50% of the 4-unit position -> sell 2.
        assert_eq!(w.set("X", Side::Buy, Size::position_frac(0.5), 25.0), Some(Order::new("X", Side::Sell, 2.0)));
        assert_eq!(w.position(&"X"), 2.0);
    }

    #[test]
    fn equity_marks_positions_to_market() {
        let mut w: PaperWallet<&str> = PaperWallet::new(1_000.0);
        w.open("X", Side::Buy, Size::units(4.0), 100.0); // funds 600, +4 units
        assert_eq!(w.equity(&bar(120.0)), 600.0 + 4.0 * 120.0);
    }

    /// A self-contained strategy type: long the golden cross, flat the death
    /// cross, on a configurable symbol. It owns only the symbol and its signals;
    /// the wallet owns the portfolio.
    struct GoldenCross {
        symbol: &'static str,
        enter: Box<dyn Signal<Input = Candle>>,
        exit: Box<dyn Signal<Input = Candle>>,
    }
    impl GoldenCross {
        fn new(symbol: &'static str, fast: usize, slow: usize) -> Self {
            Self {
                symbol,
                enter: Box::new(
                    Sma::new(Current::close(), fast).crosses_above(Sma::new(Current::close(), slow)),
                ),
                exit: Box::new(
                    Sma::new(Current::close(), fast).crosses_below(Sma::new(Current::close(), slow)),
                ),
            }
        }
    }
    impl Strategy for GoldenCross {
        type Input = Candle;
        type Symbol = &'static str;
        fn evaluate(&mut self, candle: Candle, wallet: &mut dyn Wallet<&'static str>) {
            // Advance both signals every bar before branching.
            let enter = self.enter.update(candle);
            let exit = self.exit.update(candle);
            if enter {
                wallet.open(self.symbol, Side::Buy, Size::funds_frac(1.0), candle.close);
            } else if exit {
                wallet.close(self.symbol, candle.close);
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
        // Rising then falling closes to trigger a golden then death cross.
        for px in [10.0, 11.0, 12.0, 13.0, 14.0, 13.0, 11.0, 9.0, 8.0] {
            strat.evaluate(bar(px), &mut w);
        }
        // It entered and later exited at least once; ends flat with funds back.
        assert!(!w.orders().is_empty());
        assert!(w.is_flat());
        assert!(w.funds() > 0.0);
    }

    /// A two-symbol snapshot, priced per symbol — the multi-asset case.
    #[derive(Clone, Copy)]
    struct Pair {
        a: Real,
        b: Real,
    }
    impl Market<&'static str> for Pair {
        fn price(&self, symbol: &&'static str) -> Real {
            match *symbol {
                "A" => self.a,
                "B" => self.b,
                _ => 0.0,
            }
        }
    }

    /// A market-neutral pairs leg-in: on the first bar, go long A and short B.
    struct PairsTrade {
        legged_in: bool,
    }
    impl Strategy for PairsTrade {
        type Input = Pair;
        type Symbol = &'static str;
        fn evaluate(&mut self, snap: Pair, wallet: &mut dyn Wallet<&'static str>) {
            if !self.legged_in {
                wallet.open("A", Side::Buy, Size::units(3.0), snap.price(&"A"));
                wallet.open("B", Side::Sell, Size::units(2.0), snap.price(&"B"));
                self.legged_in = true;
            }
        }
        fn reset(&mut self) {
            self.legged_in = false;
        }
    }

    #[test]
    fn multi_asset_strategy_acts_on_several_symbols_per_bar() {
        let mut strat = PairsTrade { legged_in: false };
        let mut w: PaperWallet<&'static str> = PaperWallet::new(100_000.0);
        strat.evaluate(Pair { a: 10.0, b: 20.0 }, &mut w);
        assert_eq!(
            w.orders(),
            &[
                Order::new("A", Side::Buy, 3.0),
                Order::new("B", Side::Sell, 2.0),
            ]
        );
        assert_eq!(w.position(&"A"), 3.0);
        assert_eq!(w.position(&"B"), -2.0);
        // Bought 3@10 (-30), shorted 2@20 (+40): net +10 vs start.
        assert_eq!(w.funds(), 100_000.0 + 10.0);
    }
}
