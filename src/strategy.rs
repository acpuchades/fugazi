//! The core [`Strategy`] trait, the [`Wallet`] it trades into, and the
//! vocabulary in between: [`Side`], [`Size`], [`Order`], the unit-tagged
//! [`Reference`] / [`Quantity`] amounts, and [`WalletError`].

use std::collections::HashMap;
use std::fmt;
use std::hash::Hash;

use crate::indicators::DEFAULT_EPSILON;
use crate::types::Real;

/// An incremental trading strategy — the *decision* layer above indicators and
/// signals.
///
/// Like an [`Indicator`](crate::Indicator) and a [`Signal`](crate::Signal), a
/// strategy is advanced one bar at a time, but where those layers are pure
/// value-producers a strategy *acts*. The work is split in two so the expensive,
/// independent part is separated from the part that touches shared state:
///
/// * [`update`](Strategy::update) advances the strategy's own indicators and
///   signals. It borrows only `&mut self`, so the updates of many strategies are
///   independent and can run in parallel.
/// * [`trade`](Strategy::trade) reads that freshly-advanced state (`&self`) and
///   opens, adjusts, or closes positions on the [`Wallet`] handed to it. It is
///   *price-free*: the wallet is priced from outside (see [`Wallet::update`]).
///   Trades against a shared wallet must run serially and in order, since
///   funds/value sizing resolves against the wallet's running state.
///
/// A typical driver does, each bar: feed the wallet its prices, `update` every
/// strategy, then `trade` each one. Because [`Wallet`] is taken as `&mut dyn`,
/// the same strategy runs against a [`PaperWallet`] backtest or a live broker
/// wallet unchanged.
pub trait Strategy {
    /// The per-bar input — commonly a [`Candle`](crate::Candle), or a
    /// multi-asset snapshot.
    type Input;

    /// The symbol type identifying instruments in the [`Wallet`].
    type Symbol;

    /// Advance the strategy's indicators/signals on the next bar. No trading
    /// happens here, so this can run independently of every other strategy.
    fn update(&mut self, input: Self::Input);

    /// Act on `wallet` using the state from the most recent
    /// [`update`](Strategy::update) — opening, adjusting, or closing positions.
    fn trade(&self, wallet: &mut dyn Wallet<Self::Symbol>);

    /// Clear the strategy's own state (its signals/indicators), returning it to
    /// its freshly-constructed condition. Does not touch any wallet.
    fn reset(&mut self);
}

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
/// A distinct type from [`Quantity`] so a reference amount and a count of some
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
pub struct Quantity<Sym> {
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

/// Why a [`Wallet`] movement could not be carried out.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WalletError {
    /// No price has been fed for the symbol (see [`Wallet::update`]), so the
    /// movement can't be valued or booked.
    UnknownPrice,
    /// The fed price is not strictly positive, so it can't value or book a
    /// movement.
    InvalidPrice,
    /// A net buy would drive cash below zero, and the wallet allows no margin.
    /// (A short sale credits cash, so selling is always feasible.)
    InsufficientFunds,
}

impl fmt::Display for WalletError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            WalletError::UnknownPrice => f.write_str("no price has been fed for this symbol"),
            WalletError::InvalidPrice => f.write_str("the fed price is not strictly positive"),
            WalletError::InsufficientFunds => f.write_str("insufficient funds for this buy"),
        }
    }
}

impl std::error::Error for WalletError {}

/// The portfolio interface a [`Strategy`] trades into: query funds, positions
/// and prices, feed prices in, and move positions.
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
/// equity, size relative orders, and flag infeasible movements. The single
/// execution primitive is [`set_position`](Wallet::set_position) (drive a symbol
/// to an absolute target); [`set`](Wallet::set) (a [`Side`] + [`Size`]) and
/// [`close`](Wallet::close) are default methods over it.
pub trait Wallet<Sym> {
    /// The available cash balance, in reference currency.
    fn funds(&self) -> Reference;

    /// The current signed position in `symbol`.
    fn position(&self, symbol: &Sym) -> Quantity<Sym>;

    /// The last price fed for `symbol`, or `None` if it has never been fed.
    fn price(&self, symbol: &Sym) -> Option<Reference>;

    /// Total equity: funds plus every position marked to its fed price.
    fn equity(&self) -> Reference;

    /// Feed `symbol`'s current worth. Call this — for every symbol to be traded
    /// or held — each tick, before trading or reading
    /// [`equity`](Wallet::equity).
    fn update(&mut self, symbol: Sym, price: Reference);

    /// The single execution primitive: drive `target.symbol` to `target.amount`
    /// signed units at its fed price. Implementors carry out the movement — book
    /// it on a paper wallet, or route it to a broker / bus — and return the
    /// resulting [`Order`], `Ok(None)` if the position is already there, or a
    /// [`WalletError`] if it can't be done.
    fn set_position(&mut self, target: Quantity<Sym>) -> Result<Option<Order<Sym>>, WalletError>;

    /// Target `side · size` of `symbol` (absolute). An opposite-side target
    /// reverses the position; the same side adjusts toward it.
    /// ([`close`](Wallet::close) is this with a flat target.)
    fn set(
        &mut self,
        symbol: Sym,
        side: Side,
        size: Size,
    ) -> Result<Option<Order<Sym>>, WalletError> {
        let price = self.price(&symbol).ok_or(WalletError::UnknownPrice)?.0;
        if price <= 0.0 {
            return Err(WalletError::InvalidPrice);
        }
        let position = self.position(&symbol).amount;
        let funds = self.funds().0;
        let equity = self.equity().0;
        let magnitude = size.resolve(price, position, funds, equity);
        self.set_position(Quantity {
            symbol,
            amount: side.sign() * magnitude,
        })
    }

    /// Flatten `symbol`.
    fn close(&mut self, symbol: Sym) -> Result<Option<Order<Sym>>, WalletError> {
        self.set_position(Quantity {
            symbol,
            amount: 0.0,
        })
    }
}

/// The built-in **pure**, in-memory [`Wallet`]: a paper book of `funds`,
/// per-symbol positions, the prices fed to it, and a blotter of executed
/// [`Order`]s, with no IO.
///
/// [`set_position`](Wallet::set_position) assumes the fill at the symbol's last
/// fed price and books it — debiting a buy, crediting a sell. Use it for
/// backtests and dry runs; a downstream `Wallet` impl handles live execution /
/// bus publishing.
#[derive(Debug, Clone)]
pub struct PaperWallet<Sym> {
    positions: HashMap<Sym, Real>,
    prices: HashMap<Sym, Real>,
    funds: Real,
    blotter: Vec<Order<Sym>>,
}

impl<Sym> PaperWallet<Sym> {
    /// A wallet seeded with `funds` of cash, no positions and no prices.
    pub fn new(funds: Real) -> Self {
        Self {
            positions: HashMap::new(),
            prices: HashMap::new(),
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

    /// Drop the blotter history (positions, prices and funds are untouched).
    pub fn clear_blotter(&mut self) {
        self.blotter.clear();
    }
}

impl<Sym: Clone + Eq + Hash> PaperWallet<Sym> {
    /// Iterate the held positions.
    pub fn positions(&self) -> impl Iterator<Item = Quantity<Sym>> + '_ {
        self.positions.iter().map(|(symbol, &amount)| Quantity {
            symbol: symbol.clone(),
            amount,
        })
    }
}

impl<Sym: Clone + Eq + Hash> Wallet<Sym> for PaperWallet<Sym> {
    fn funds(&self) -> Reference {
        Reference(self.funds)
    }

    fn position(&self, symbol: &Sym) -> Quantity<Sym> {
        Quantity {
            symbol: symbol.clone(),
            amount: self.positions.get(symbol).copied().unwrap_or(0.0),
        }
    }

    fn price(&self, symbol: &Sym) -> Option<Reference> {
        self.prices.get(symbol).copied().map(Reference)
    }

    fn equity(&self) -> Reference {
        let positions_value: Real = self
            .positions
            .iter()
            .map(|(symbol, &amount)| amount * self.prices.get(symbol).copied().unwrap_or(0.0))
            .sum();
        Reference(self.funds + positions_value)
    }

    fn update(&mut self, symbol: Sym, price: Reference) {
        self.prices.insert(symbol, price.0);
    }

    fn set_position(&mut self, target: Quantity<Sym>) -> Result<Option<Order<Sym>>, WalletError> {
        let Quantity {
            symbol,
            amount: target,
        } = target;
        let current = self.positions.get(&symbol).copied().unwrap_or(0.0);
        let delta = target - current;
        if delta.abs() <= DEFAULT_EPSILON {
            return Ok(None);
        }
        let price = self
            .prices
            .get(&symbol)
            .copied()
            .ok_or(WalletError::UnknownPrice)?;
        if price <= 0.0 {
            return Err(WalletError::InvalidPrice);
        }
        // No margin: a net buy can't drive cash below zero (tolerant of the
        // rounding in an all-in `value_frac(1.0)`, whose cost equals funds).
        if delta > 0.0 {
            let cost = delta * price;
            let tolerance = DEFAULT_EPSILON * self.funds.abs().max(1.0);
            if cost - self.funds > tolerance {
                return Err(WalletError::InsufficientFunds);
            }
        }
        let order = Order::from_delta(symbol.clone(), delta)
            .expect("delta exceeds DEFAULT_EPSILON, so the order is non-empty");
        // Pay for a buy, receive for a sell.
        self.funds -= order.signed_quantity() * price;
        let new_position = current + delta;
        if new_position.abs() <= DEFAULT_EPSILON {
            self.positions.remove(&symbol);
        } else {
            self.positions.insert(symbol, new_position);
        }
        self.blotter.push(order.clone());
        Ok(Some(order))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::indicators::{BoolIndicatorExt, Current, IndicatorExt, Sma};
    use crate::signal::Signal;
    use crate::types::Candle;

    fn bar(close: Real) -> Candle {
        Candle::new(close, close, close, close, 0.0)
    }

    #[test]
    fn set_position_is_absolute_and_books_funds() {
        let mut w: PaperWallet<&str> = PaperWallet::new(1_000.0);
        w.update("X", Reference(100.0));
        assert_eq!(
            w.set_position(Quantity {
                symbol: "X",
                amount: 3.0
            }),
            Ok(Some(Order::new("X", Side::Buy, 3.0)))
        );
        // Setting a larger target buys the difference (scale in).
        w.set_position(Quantity {
            symbol: "X",
            amount: 5.0,
        })
        .unwrap();
        assert_eq!(w.position(&"X").amount, 5.0);
        assert_eq!(w.funds().0, 1_000.0 - 5.0 * 100.0);
    }

    #[test]
    fn set_targets_absolute_and_reverses() {
        let mut w: PaperWallet<&str> = PaperWallet::new(10_000.0);
        w.update("X", Reference(50.0));
        w.set("X", Side::Buy, Size::units(4.0)).unwrap();
        assert!(w.set("X", Side::Buy, Size::units(4.0)).unwrap().is_none()); // idempotent
        // Opposite side reverses: +4 -> -4 is a sell of 8.
        assert_eq!(
            w.set("X", Side::Sell, Size::units(4.0)),
            Ok(Some(Order::new("X", Side::Sell, 8.0)))
        );
        assert_eq!(w.position(&"X").amount, -4.0);
    }

    #[test]
    fn close_flattens() {
        let mut w: PaperWallet<&str> = PaperWallet::new(1_000.0);
        w.update("X", Reference(100.0));
        w.set("X", Side::Buy, Size::units(10.0)).unwrap();
        w.update("X", Reference(110.0));
        assert_eq!(w.close("X"), Ok(Some(Order::new("X", Side::Sell, 10.0))));
        assert!(w.is_flat());
        assert_eq!(w.funds().0, 1_100.0);
    }

    #[test]
    fn relative_sizing_resolves_against_funds_and_position() {
        let mut w: PaperWallet<&str> = PaperWallet::new(1_000.0);
        w.update("X", Reference(25.0));
        // 10% of 1000 = 100 / price 25 = 4 units.
        assert_eq!(
            w.set("X", Side::Buy, Size::funds_frac(0.1)),
            Ok(Some(Order::new("X", Side::Buy, 4.0)))
        );
        // Set to 50% of the 4-unit position -> sell 2.
        assert_eq!(
            w.set("X", Side::Buy, Size::position_frac(0.5)),
            Ok(Some(Order::new("X", Side::Sell, 2.0)))
        );
        assert_eq!(w.position(&"X").amount, 2.0);
    }

    #[test]
    fn value_fraction_sizes_against_equity_and_flips_all_in() {
        let mut w: PaperWallet<&str> = PaperWallet::new(1_000.0);
        w.update("X", Reference(100.0));
        // All-in long: 100% of equity (== funds when flat) / 100 = 10 units.
        w.set("X", Side::Buy, Size::value_frac(1.0)).unwrap();
        assert_eq!(w.position(&"X").amount, 10.0);
        assert!(w.funds().0.abs() <= 1e-6);
        // Equity is still 1000; flip all-in short in one call -> -10 units.
        assert_eq!(
            w.set("X", Side::Sell, Size::value_frac(1.0)),
            Ok(Some(Order::new("X", Side::Sell, 20.0)))
        );
        assert_eq!(w.position(&"X").amount, -10.0);
    }

    #[test]
    fn equity_marks_positions_to_fed_prices() {
        let mut w: PaperWallet<&str> = PaperWallet::new(1_000.0);
        w.update("X", Reference(100.0));
        w.set("X", Side::Buy, Size::units(4.0)).unwrap(); // funds 600, +4 units
        w.update("X", Reference(120.0));
        assert_eq!(w.equity().0, 600.0 + 4.0 * 120.0);
    }

    #[test]
    fn unknown_price_is_flagged() {
        let mut w: PaperWallet<&str> = PaperWallet::new(1_000.0);
        // "X" was never fed a price.
        assert_eq!(
            w.set("X", Side::Buy, Size::units(1.0)),
            Err(WalletError::UnknownPrice)
        );
        assert_eq!(
            w.set_position(Quantity {
                symbol: "X",
                amount: 1.0
            }),
            Err(WalletError::UnknownPrice)
        );
    }

    #[test]
    fn insufficient_funds_is_flagged_but_shorts_are_free() {
        let mut w: PaperWallet<&str> = PaperWallet::new(100.0);
        w.update("X", Reference(50.0));
        // 3 units cost 150 > 100 funds, and there is no margin.
        assert_eq!(
            w.set("X", Side::Buy, Size::units(3.0)),
            Err(WalletError::InsufficientFunds)
        );
        // A short sale credits cash, so selling is always feasible.
        assert!(w.set("X", Side::Sell, Size::units(3.0)).is_ok());
    }

    #[test]
    fn non_positive_price_is_flagged() {
        let mut w: PaperWallet<&str> = PaperWallet::new(1_000.0);
        w.update("X", Reference(0.0));
        assert_eq!(
            w.set("X", Side::Buy, Size::value_frac(1.0)),
            Err(WalletError::InvalidPrice)
        );
        assert_eq!(
            w.set_position(Quantity {
                symbol: "X",
                amount: 1.0
            }),
            Err(WalletError::InvalidPrice)
        );
    }

    /// A self-contained strategy type: long the golden cross, flat the death
    /// cross, on a configurable symbol. It owns only the symbol and its signals;
    /// the wallet owns the portfolio.
    struct GoldenCross {
        symbol: &'static str,
        enter: Box<dyn Signal>,
        exit: Box<dyn Signal>,
    }
    impl GoldenCross {
        fn new(symbol: &'static str, fast: usize, slow: usize) -> Self {
            Self {
                symbol,
                enter: Box::new(
                    Sma::new(Current::close(), fast)
                        .crosses_above(Sma::new(Current::close(), slow)),
                ),
                exit: Box::new(
                    Sma::new(Current::close(), fast)
                        .crosses_below(Sma::new(Current::close(), slow)),
                ),
            }
        }
    }
    impl Strategy for GoldenCross {
        type Input = Candle;
        type Symbol = &'static str;
        fn update(&mut self, candle: Candle) {
            // Advance both signals every bar.
            self.enter.update(candle);
            self.exit.update(candle);
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
            w.update("X", Reference(px));
            strat.update(bar(px));
            strat.trade(&mut w);
        }
        // It entered and later exited at least once; ends flat with funds back.
        assert!(!w.orders().is_empty());
        assert!(w.is_flat());
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
                let _ = wallet.set_position(Quantity {
                    symbol: "A",
                    amount: 3.0,
                });
                let _ = wallet.set_position(Quantity {
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
        w.update("A", Reference(snap.a));
        w.update("B", Reference(snap.b));
        strat.update(snap);
        strat.trade(&mut w);
        assert_eq!(
            w.orders(),
            &[
                Order::new("A", Side::Buy, 3.0),
                Order::new("B", Side::Sell, 2.0),
            ]
        );
        assert_eq!(w.position(&"A").amount, 3.0);
        assert_eq!(w.position(&"B").amount, -2.0);
        // Bought 3@10 (-30), shorted 2@20 (+40): net +10 vs start.
        assert_eq!(w.funds().0, 100_000.0 + 10.0);
    }
}
