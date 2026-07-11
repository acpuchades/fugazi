//! [`Book`]: a strategy's own view of its equity curve and closed-trade
//! history, and the book-anchored sources built from it.
//!
//! Twin of [`Position`](crate::indicators::Position). Where `Position`
//! tracks *the current open trade* (side, size, entry, extremes), `Book`
//! tracks *strategy-lifetime state* — cash, per-leg position units,
//! marked-to-market total equity, running peak, per-bar returns, and a
//! stream of closed-trade summaries. Position-dependent sizing recipes
//! read this state through ordinary indicator accessors, so a drawdown
//! throttle is `book.drawdown()` composed with a clamp, a realized-vol
//! target is `Value::new(t).div(StdDev::new(book.return_per_bar(), N).mul(...))`,
//! and fractional Kelly is `k * mean/variance` of `book.trade_return()`
//! — no new abstraction, just more accessors.
//!
//! ## Equity accounting
//!
//! `Book::new(initial_equity)` seeds `cash = initial_equity`, no legs
//! registered, and equity at `initial_equity`. Each fill routed via
//! [`apply_fill`](Book::apply_fill) updates cash (`Buy` costs, `Sell`
//! earns) and the fill's leg's signed position count (registering the
//! leg on its first fill). Each bar routed via [`update`](Book::update)
//! marks all registered legs to market:
//!
//! ```text
//! equity = cash + sum over legs (position_units * close)
//! ```
//!
//! The running peak, per-bar return, and closed-trade metadata all fall
//! out of that. `Book` is generic over the symbol type `Sym` (default
//! [`String`]), so a single-asset strategy holds `Book<Sym>` with one
//! leg and a two-leg pair holds `Book<Sym>` with two legs — the same
//! `BookField` accessors work in both.
//!
//! **Match the wallet's initial capital.** The `Book`'s `initial_equity`
//! must line up with the wallet's seed; a mismatch produces meaningless
//! equity levels (position units are in wallet-scale but equity would
//! start at a different scale). The
//! [`SingleAssetStrategy::with_initial_equity`](crate::strategies::SingleAssetStrategy::with_initial_equity)
//! and
//! [`PairsStrategy::with_initial_equity`](crate::strategies::PairsStrategy::with_initial_equity)
//! constructors take it explicitly for this reason; `new(sym)` /
//! `new(left, right)` default to `1.0`.
//!
//! ## Trade lifecycle
//!
//! A "trade" opens when the strategy moves from *all* legs flat to *any*
//! leg non-flat (an entry), and closes when all legs return to flat
//! again (a flatten). Between the open and close all leg-level fills are
//! considered part of the same trade — scaling in/out, or the second leg
//! of a pair opening after the first. A single-leg cross-zero
//! (single-asset reversal) closes the current trade at the fill price and
//! opens a new one immediately (matches the pre-refactor single-leg
//! behaviour). Multi-leg strategies whose pattern is "both legs open
//! together, both close together" — like [`PairsStrategy`] — see one
//! aggregate trade per pair cycle.
//!
//! On close, a [`TradeClose`] is *staged* on the pending slot; the next
//! [`update`](Book::update) call moves it to the active slot so that
//! this bar's indicator reads see it as `Some`. The following bar's
//! `update` drains the active slot back to `None`, so a per-close
//! indicator (say `Sma::new(book.trade_return(), 30)`) only advances on
//! the close bar itself.
//!
//! [`PairsStrategy`]: crate::strategies::PairsStrategy

use std::cell::RefCell;
use std::collections::HashMap;
use std::hash::Hash;
use std::marker::PhantomData;
use std::rc::Rc;

use crate::indicator::Indicator;
use crate::indicators::DEFAULT_EPSILON;
use crate::wallet::Side;
use crate::types::{Atom, Candle, Real};

/// Per-leg tracked state.
#[derive(Debug, Clone, Copy)]
struct LegState {
    /// Signed position units for this leg (positive long, negative short,
    /// zero flat).
    units: Real,
    /// Last seen close for this leg — used to mark to market on each bar,
    /// and to compute the trade-close realized P&L per leg. `None` before
    /// the first update that includes this leg.
    prev_close: Option<Real>,
    /// Per-leg entry price of the current trade (set when this leg
    /// transitions from flat to non-flat as part of an open trade, `None`
    /// while this leg is flat or between trades). Scaling in on the same
    /// side keeps the anchor unchanged.
    entry_price: Option<Real>,
}

impl LegState {
    fn flat() -> Self {
        Self {
            units: 0.0,
            prev_close: None,
            entry_price: None,
        }
    }

    fn is_flat(&self) -> bool {
        self.units.abs() <= DEFAULT_EPSILON
    }
}

/// The realized outcome of a closed trade.
#[derive(Debug, Clone, Copy)]
struct TradeClose {
    /// Realized P&L in reference-currency terms (same units as the
    /// [`Book`]'s `initial_equity`), summed across every leg.
    pnl: Real,
    /// P&L as a fraction of the equity at trade open.
    return_ratio: Real,
}

/// The shared running state a [`Book`] carries.
#[derive(Debug)]
struct BookState<Sym> {
    initial_equity: Real,
    cash: Real,
    legs: HashMap<Sym, LegState>,
    equity: Real,
    equity_peak: Real,
    /// Equity at the moment the currently-open trade opened, `None` while
    /// all legs are flat. The denominator for [`TradeClose::return_ratio`].
    trade_open_equity: Option<Real>,
    active_return: Option<Real>,
    first_update: bool,
    /// Realized P&L banked from legs that already closed during the
    /// currently-open aggregate trade. Reset to `0` on aggregate open;
    /// added to on each closing / cross-zero fill; totalled on aggregate
    /// close and reset again.
    trade_pnl_accum: Real,
    pending_trade_close: Option<TradeClose>,
    active_trade_close: Option<TradeClose>,
}

impl<Sym: Hash + Eq> BookState<Sym> {
    fn seed(initial_equity: Real) -> Self {
        Self {
            initial_equity,
            cash: initial_equity,
            legs: HashMap::new(),
            equity: initial_equity,
            equity_peak: initial_equity,
            trade_open_equity: None,
            active_return: None,
            first_update: true,
            trade_pnl_accum: 0.0,
            pending_trade_close: None,
            active_trade_close: None,
        }
    }

    /// Whether every registered leg is flat. An empty leg map (no fills
    /// yet) is also considered flat.
    fn is_all_flat(&self) -> bool {
        self.legs.values().all(|l| l.is_flat())
    }

    /// Stage a [`TradeClose`] from the accumulated `trade_pnl_accum` and
    /// reset all per-trade counters. Called on aggregate close (all legs
    /// flat) or on a leg-level reversal.
    fn stage_trade_close(&mut self) {
        let total_pnl = self.trade_pnl_accum;
        let equity_at_open = self.trade_open_equity.take().unwrap_or(self.equity);
        let return_ratio = if equity_at_open.abs() > DEFAULT_EPSILON {
            total_pnl / equity_at_open
        } else {
            0.0
        };
        self.pending_trade_close = Some(TradeClose {
            pnl: total_pnl,
            return_ratio,
        });
        self.trade_pnl_accum = 0.0;
    }
}

/// A strategy's own view of its equity curve and closed-trade history.
///
/// Backed by an `Rc<RefCell<…>>`, so cloning shares one state — every
/// [`BookField`] accessor holds a clone and reads the same facts the
/// strategy writes via [`apply_fill`](Book::apply_fill) /
/// [`update`](Book::update).
///
/// Generic over the symbol type `Sym` — defaults to [`String`] so callers
/// that don't care about the sym type can write `Book` and let the
/// default apply. Requires `Sym: Hash + Eq + Clone` so it can key an
/// internal `HashMap<Sym, LegState>` for per-leg accounting.
pub struct Book<Sym = String>(Rc<RefCell<BookState<Sym>>>);

impl<Sym> std::fmt::Debug for Book<Sym>
where
    Sym: std::fmt::Debug,
{
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_tuple("Book").field(&self.0).finish()
    }
}

impl<Sym> Clone for Book<Sym> {
    fn clone(&self) -> Self {
        Self(Rc::clone(&self.0))
    }
}

impl<Sym: Hash + Eq + Clone> Book<Sym> {
    /// A fresh `Book` seeded at `initial_equity` — the strategy's assumed
    /// starting capital. See the module doc: match the wallet's seed for
    /// meaningful equity/drawdown numbers.
    ///
    /// # Panics
    /// Panics if `initial_equity` is not strictly positive.
    pub fn new(initial_equity: Real) -> Self {
        assert!(
            initial_equity > 0.0,
            "initial_equity must be strictly positive"
        );
        Self(Rc::new(RefCell::new(BookState::seed(initial_equity))))
    }

    /// Reset every counter back to the freshly-constructed state, keeping
    /// the original `initial_equity` seed.
    pub fn reset(&self) {
        let seed = self.0.borrow().initial_equity;
        *self.0.borrow_mut() = BookState::seed(seed);
    }

    /// Apply a fill of `units` at `price` on `side`, tagged with the leg's
    /// `sym` — the book's link from the wallet's fill stream to its own
    /// cash / position / trade view.
    ///
    /// Opening from all-legs-flat → any-leg-non-flat *opens* an aggregate
    /// trade (records `trade_open_equity` from the current equity).
    /// Returning to all-legs-flat *closes* the aggregate trade and stages
    /// a [`TradeClose`] (visible on the next [`Book::update`] and drained
    /// the bar after). A single-leg cross-zero (single-asset reversal)
    /// closes the current trade at `price` and opens a new one at the
    /// same fill — matching the pre-refactor single-leg semantics.
    ///
    /// Scaling in on the same side keeps the leg's entry price unchanged
    /// (matches [`Position::apply`]).
    ///
    /// [`Position::apply`]: crate::indicators::Position::apply
    pub fn apply_fill(&self, sym: &Sym, side: Side, units: Real, price: Real) {
        let mut s = self.0.borrow_mut();
        let sign = side.sign();
        let signed_units = sign * units;

        // Cash impact — a Buy pays out, a Sell earns.
        s.cash -= sign * units * price;

        let was_all_flat = s.is_all_flat();
        let leg = s.legs.entry(sym.clone()).or_insert_with(LegState::flat);
        let prev_leg_units = leg.units;
        let new_leg_units = prev_leg_units + signed_units;
        leg.units = new_leg_units;
        let leg_entry_before = leg.entry_price;

        let leg_was_flat = prev_leg_units.abs() <= DEFAULT_EPSILON;
        let leg_now_flat = new_leg_units.abs() <= DEFAULT_EPSILON;
        let leg_crossed_zero = prev_leg_units * new_leg_units < -DEFAULT_EPSILON;

        // ---- Per-leg P&L banking + entry-price bookkeeping ----
        if leg_was_flat && !leg_now_flat {
            // Fresh open on this leg.
            leg.entry_price = Some(price);
        } else if leg_crossed_zero {
            // Bank the closing side's P&L, then re-anchor entry for the
            // new side at the same fill price.
            if let Some(entry) = leg_entry_before {
                s.trade_pnl_accum += prev_leg_units * (price - entry);
            }
            let leg = s.legs.get_mut(sym).unwrap();
            leg.entry_price = Some(price);
        } else if leg_now_flat && !leg_was_flat {
            // Full close of this leg.
            if let Some(entry) = leg_entry_before {
                s.trade_pnl_accum += prev_leg_units * (price - entry);
            }
            let leg = s.legs.get_mut(sym).unwrap();
            leg.entry_price = None;
        }
        // Same-side scaling keeps the existing entry (do nothing).

        // ---- Aggregate-level transitions (open / close / reversal) ----
        let is_all_flat_now = s.is_all_flat();
        if leg_crossed_zero {
            // Single-leg reversal — always close+open the aggregate trade,
            // even on multi-leg (semantically: this leg flipped, so its
            // era ended; the strategy is now in a new trade regime).
            if s.trade_open_equity.is_some() {
                s.stage_trade_close();
            }
            s.trade_open_equity = Some(s.equity);
        } else if was_all_flat && !is_all_flat_now {
            // Aggregate trade opens.
            s.trade_open_equity = Some(s.equity);
        } else if !was_all_flat && is_all_flat_now && s.trade_open_equity.is_some() {
            // Aggregate trade closes.
            s.stage_trade_close();
        }
    }

    /// Mark every leg to market from the given per-leg closes — updates
    /// each leg's `prev_close`, recomputes total equity, updates the
    /// running peak, computes the per-bar return, and promotes any
    /// pending trade-close into the accessor-visible slot.
    ///
    /// The strategy calls this once per bar after
    /// [`apply_fill`](Book::apply_fill) has routed the bar's fills. Legs
    /// not present in `marks` retain their previous `prev_close`.
    pub fn update<I>(&self, marks: I)
    where
        I: IntoIterator<Item = (Sym, Candle)>,
    {
        let mut s = self.0.borrow_mut();

        // Promote pending-close → active so `book.trade_pnl` /
        // `book.trade_return` read Some this bar.
        s.active_trade_close = s.pending_trade_close.take();

        // Apply the per-leg marks.
        for (sym, candle) in marks {
            let leg = s.legs.entry(sym).or_insert_with(LegState::flat);
            leg.prev_close = Some(candle.close);
        }

        // Mark-to-market total equity.
        let prev_equity = s.equity;
        let mut equity = s.cash;
        for leg in s.legs.values() {
            if let Some(close) = leg.prev_close {
                equity += leg.units * close;
            }
        }
        s.equity = equity;
        if s.equity > s.equity_peak {
            s.equity_peak = s.equity;
        }

        // Per-bar return: None on the very first bar.
        if s.first_update {
            s.first_update = false;
            s.active_return = None;
        } else if prev_equity.abs() > DEFAULT_EPSILON {
            s.active_return = Some((s.equity - prev_equity) / prev_equity);
        } else {
            s.active_return = None;
        }
    }

    /// The seed value the book started with.
    pub fn initial_equity(&self) -> Real {
        self.0.borrow().initial_equity
    }

    /// The marked-to-market equity as of the most recent
    /// [`Book::update`] call.
    pub fn equity_value(&self) -> Real {
        self.0.borrow().equity
    }

    /// The running peak of [`equity_value`](Self::equity_value) since
    /// construction (or [`reset`](Self::reset)).
    pub fn equity_peak_value(&self) -> Real {
        self.0.borrow().equity_peak
    }

    /// The book's [equity level](Self::equity_value) as a real-valued
    /// [`Indicator`] — a leaf a drawdown-throttle expression or an
    /// equity-vol target composes against. Always [`Some`] (seeded at
    /// `initial_equity`).
    pub fn equity<In>(&self) -> BookField<Sym, In> {
        BookField::new(self.clone(), 0, |s| Some(s.equity))
    }

    /// The book's [running-peak equity](Self::equity_peak_value) as a
    /// real-valued [`Indicator`]. Always [`Some`].
    pub fn equity_peak<In>(&self) -> BookField<Sym, In> {
        BookField::new(self.clone(), 0, |s| Some(s.equity_peak))
    }

    /// The book's current drawdown — `(equity - peak) / peak`, always
    /// `<= 0` (and `0` at a new peak).
    pub fn drawdown<In>(&self) -> BookField<Sym, In> {
        BookField::new(self.clone(), 0, |s| {
            if s.equity_peak.abs() > DEFAULT_EPSILON {
                Some((s.equity - s.equity_peak) / s.equity_peak)
            } else {
                None
            }
        })
    }

    /// The just-completed bar's equity return —
    /// `(equity - prev_equity) / prev_equity`. [`None`] on the first bar
    /// (`warm_up_period() = 2`).
    pub fn return_per_bar<In>(&self) -> BookField<Sym, In> {
        BookField::new(self.clone(), 2, |s| s.active_return)
    }

    /// Realized P&L of the just-closed aggregate trade in
    /// reference-currency terms (same units as `initial_equity`),
    /// summed across every leg. [`Some`] only on the bar whose fill
    /// closed the trade.
    pub fn trade_pnl<In>(&self) -> BookField<Sym, In> {
        BookField::new(self.clone(), 0, |s| s.active_trade_close.map(|t| t.pnl))
    }

    /// The just-closed trade's return as a fraction of the equity at
    /// trade open. [`Some`] only on the close bar.
    pub fn trade_return<In>(&self) -> BookField<Sym, In> {
        BookField::new(self.clone(), 0, |s| s.active_trade_close.map(|t| t.return_ratio))
    }
}

/// One field of a shared [`Book`], projected into an
/// `Indicator<Input = In, Output = Real>` so a book-anchored source
/// composes like any other. Returned by every accessor on [`Book`].
pub struct BookField<Sym, In = Atom> {
    book: Book<Sym>,
    warm_up: usize,
    select: fn(&BookState<Sym>) -> Option<Real>,
    _phantom: PhantomData<fn(In)>,
}

impl<Sym: std::fmt::Debug, In> std::fmt::Debug for BookField<Sym, In> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("BookField")
            .field("book", &self.book)
            .field("warm_up", &self.warm_up)
            .finish()
    }
}

impl<Sym, In> Clone for BookField<Sym, In> {
    fn clone(&self) -> Self {
        Self {
            book: self.book.clone(),
            warm_up: self.warm_up,
            select: self.select,
            _phantom: PhantomData,
        }
    }
}

impl<Sym, In> BookField<Sym, In> {
    fn new(book: Book<Sym>, warm_up: usize, select: fn(&BookState<Sym>) -> Option<Real>) -> Self {
        Self {
            book,
            warm_up,
            select,
            _phantom: PhantomData,
        }
    }
}

impl<Sym, In> Indicator for BookField<Sym, In> {
    type Input = In;
    type Output = Real;

    fn update(&mut self, _input: In) -> Option<Real> {
        self.value()
    }

    fn value(&self) -> Option<Real> {
        (self.select)(&self.book.0.borrow())
    }

    fn warm_up_period(&self) -> usize {
        self.warm_up
    }

    fn reset(&mut self) {}
}

#[cfg(test)]
mod tests {
    use super::*;

    fn bar(close: Real) -> Candle {
        Candle::new(close, close, close, close, 0.0)
    }

    #[test]
    #[should_panic]
    fn new_rejects_non_positive_initial_equity() {
        let _: Book<&str> = Book::new(0.0);
    }

    #[test]
    fn initial_state_reads_seed_and_no_return() {
        let book: Book<&str> = Book::new(1_000.0);
        assert_eq!(book.equity_value(), 1_000.0);
        assert_eq!(book.equity_peak_value(), 1_000.0);
        let eq = book.equity::<Atom>();
        let dd = book.drawdown::<Atom>();
        let ret = book.return_per_bar::<Atom>();
        assert_eq!(eq.value(), Some(1_000.0));
        assert_eq!(dd.value(), Some(0.0));
        assert_eq!(ret.value(), None);
    }

    #[test]
    fn buy_and_hold_single_leg() {
        let book: Book<&str> = Book::new(1_000.0);
        book.apply_fill(&"X", Side::Buy, 10.0, 100.0);
        book.update([("X", bar(100.0))]);
        assert_eq!(book.equity_value(), 1_000.0);
        assert_eq!(book.return_per_bar::<Atom>().value(), None);
        book.update([("X", bar(110.0))]);
        assert_eq!(book.equity_value(), 1_100.0);
        assert!(
            (book.return_per_bar::<Atom>().value().unwrap() - 0.1).abs() < 1e-12
        );
        book.update([("X", bar(120.0))]);
        assert_eq!(book.equity_value(), 1_200.0);
        assert_eq!(book.equity_peak_value(), 1_200.0);
        assert_eq!(book.drawdown::<Atom>().value(), Some(0.0));
    }

    #[test]
    fn drawdown_survives_a_dip() {
        let book: Book<&str> = Book::new(1_000.0);
        book.apply_fill(&"X", Side::Buy, 10.0, 100.0);
        book.update([("X", bar(100.0))]);
        book.update([("X", bar(120.0))]);
        book.update([("X", bar(108.0))]);
        assert_eq!(book.equity_peak_value(), 1_200.0);
        assert_eq!(book.equity_value(), 1_080.0);
        assert!((book.drawdown::<Atom>().value().unwrap() - (-0.10)).abs() < 1e-12);
    }

    #[test]
    fn single_leg_trade_close_reports_pnl_on_the_close_bar() {
        let book: Book<&str> = Book::new(1_000.0);
        book.apply_fill(&"X", Side::Buy, 10.0, 100.0);
        book.update([("X", bar(100.0))]);
        book.update([("X", bar(110.0))]);
        book.apply_fill(&"X", Side::Sell, 10.0, 120.0);
        book.update([("X", bar(120.0))]);
        let pnl = book.trade_pnl::<Atom>().value();
        let ret = book.trade_return::<Atom>().value();
        assert!(pnl.is_some() && ret.is_some());
        assert!((pnl.unwrap() - 200.0).abs() < 1e-12);
        assert!((ret.unwrap() - 0.20).abs() < 1e-12);
        book.update([("X", bar(120.0))]);
        assert_eq!(book.trade_pnl::<Atom>().value(), None);
        assert_eq!(book.trade_return::<Atom>().value(), None);
    }

    #[test]
    fn short_leg_close_reports_negative_pnl() {
        let book: Book<&str> = Book::new(1_000.0);
        book.apply_fill(&"X", Side::Sell, 10.0, 100.0);
        book.update([("X", bar(100.0))]);
        book.update([("X", bar(105.0))]);
        book.apply_fill(&"X", Side::Buy, 10.0, 105.0);
        book.update([("X", bar(105.0))]);
        assert!((book.trade_pnl::<Atom>().value().unwrap() - (-50.0)).abs() < 1e-12);
        assert!((book.trade_return::<Atom>().value().unwrap() - (-0.05)).abs() < 1e-12);
    }

    #[test]
    fn single_leg_reversal_closes_and_reopens() {
        let book: Book<&str> = Book::new(1_000.0);
        book.apply_fill(&"X", Side::Buy, 5.0, 100.0);
        book.update([("X", bar(100.0))]);
        book.update([("X", bar(110.0))]);
        book.apply_fill(&"X", Side::Sell, 10.0, 120.0);
        book.update([("X", bar(120.0))]);
        let pnl = book.trade_pnl::<Atom>().value().unwrap();
        let ret = book.trade_return::<Atom>().value().unwrap();
        assert!((pnl - 100.0).abs() < 1e-12); // 5 * (120 - 100)
        assert!((ret - 0.10).abs() < 1e-12);
        // Drain and close the residual short at 100.
        book.update([("X", bar(120.0))]);
        assert_eq!(book.trade_return::<Atom>().value(), None);
        book.apply_fill(&"X", Side::Buy, 5.0, 100.0);
        book.update([("X", bar(100.0))]);
        assert!((book.trade_pnl::<Atom>().value().unwrap() - 100.0).abs() < 1e-12);
    }

    #[test]
    fn scale_in_same_side_keeps_original_entry() {
        let book: Book<&str> = Book::new(1_000.0);
        book.apply_fill(&"X", Side::Buy, 5.0, 100.0);
        book.update([("X", bar(100.0))]);
        book.apply_fill(&"X", Side::Buy, 5.0, 110.0);
        book.update([("X", bar(120.0))]);
        book.apply_fill(&"X", Side::Sell, 10.0, 120.0);
        book.update([("X", bar(120.0))]);
        // Uses original entry (100) across the whole 10 units.
        assert!((book.trade_pnl::<Atom>().value().unwrap() - 200.0).abs() < 1e-12);
    }

    #[test]
    fn two_leg_pair_open_and_close() {
        let book: Book<&str> = Book::new(1_000.0);
        // Open: Long L @ 100 (5u) + Short R @ 50 (5u).
        book.apply_fill(&"L", Side::Buy, 5.0, 100.0);
        book.apply_fill(&"R", Side::Sell, 5.0, 50.0);
        book.update([("L", bar(100.0)), ("R", bar(50.0))]);
        // Equity = cash + 5*100 + (-5)*50 = (1000 - 500 + 250) + 500 - 250 = 1000.
        assert!((book.equity_value() - 1_000.0).abs() < 1e-9);
        assert_eq!(book.trade_pnl::<Atom>().value(), None); // no close yet

        // Second bar — pair moves: L → 110 (winning), R → 55 (adverse).
        book.update([("L", bar(110.0)), ("R", bar(55.0))]);
        // Equity = 750 + 5*110 + (-5)*55 = 750 + 550 - 275 = 1025.
        assert!((book.equity_value() - 1_025.0).abs() < 1e-9);

        // Close: Sell L @ 110, Buy R @ 55.
        book.apply_fill(&"L", Side::Sell, 5.0, 110.0);
        book.apply_fill(&"R", Side::Buy, 5.0, 55.0);
        book.update([("L", bar(110.0)), ("R", bar(55.0))]);
        // Aggregate P&L: L = 5*(110-100) = 50; R = -5*(55-50) = -25. Total 25.
        let pnl = book.trade_pnl::<Atom>().value();
        assert!(pnl.is_some(), "expected close event, got {pnl:?}");
        assert!((pnl.unwrap() - 25.0).abs() < 1e-9);
        assert!((book.trade_return::<Atom>().value().unwrap() - 0.025).abs() < 1e-9);
    }

    #[test]
    fn reset_restores_freshly_constructed_state() {
        let book: Book<&str> = Book::new(1_000.0);
        book.apply_fill(&"X", Side::Buy, 10.0, 100.0);
        book.update([("X", bar(100.0))]);
        book.update([("X", bar(120.0))]);
        book.reset();
        assert_eq!(book.equity_value(), 1_000.0);
        assert_eq!(book.equity_peak_value(), 1_000.0);
        assert_eq!(book.return_per_bar::<Atom>().value(), None);
        assert_eq!(book.trade_pnl::<Atom>().value(), None);
        assert_eq!(book.initial_equity(), 1_000.0);
    }

    #[test]
    fn accessors_share_state_via_clone() {
        let book: Book<&str> = Book::new(1_000.0);
        let eq1 = book.equity::<Atom>();
        let eq2 = book.equity::<Atom>();
        book.apply_fill(&"X", Side::Buy, 10.0, 100.0);
        book.update([("X", bar(120.0))]);
        assert_eq!(eq1.value(), eq2.value());
        assert!((eq1.value().unwrap() - 1_200.0).abs() < 1e-12);
    }
}
