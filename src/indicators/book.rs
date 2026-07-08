//! [`Book`]: a strategy's own view of its equity curve and closed-trade
//! history, and the book-anchored sources built from it.
//!
//! Twin of [`Position`](crate::indicators::Position). Where `Position` tracks
//! *the current open trade* (side, size, entry, extremes), `Book` tracks
//! *strategy-lifetime state* — running cash, marked-to-market equity, the
//! peak equity since start, per-bar returns, and a stream of closed-trade
//! summaries. Position-dependent sizing recipes read this state through
//! ordinary indicator accessors, so a drawdown throttle is
//! `book.drawdown()` composed with a clamp, a realized-vol target is
//! `Value::new(t).div(StdDev::new(book.return_per_bar(), N).mul(...))`, and
//! fractional Kelly is `k * mean/variance` of `book.trade_return()` — no
//! new abstraction, just more accessors.
//!
//! ## Equity accounting
//!
//! `Book::new(initial_equity)` seeds `cash = initial_equity`, positions at
//! `0`, and equity at `initial_equity`. Each fill routed via
//! [`apply_fill`](Book::apply_fill) updates cash (`Buy` costs, `Sell`
//! earns) and the signed position count. Each bar routed via
//! [`update`](Book::update) marks-to-market at the bar's `close`:
//! `equity = cash + position_units * close`. The running peak, per-bar
//! return, and closed-trade metadata all fall out of that.
//!
//! **Match the wallet's initial capital.** The `Book`'s `initial_equity`
//! must line up with the wallet's seed; a mismatch produces meaningless
//! equity levels (position units are in wallet-scale but equity would
//! start at a different scale). The
//! [`SingleAssetStrategy::with_initial_equity`](crate::strategies::SingleAssetStrategy::with_initial_equity)
//! constructor takes it explicitly for this reason; `new(sym)` defaults to
//! `1.0` (matches a `PaperWallet::new(1.0)` toy setup — anything else
//! needs the explicit seed).
//!
//! ## Trade lifecycle
//!
//! A "trade" opens when the signed position count moves from zero to
//! non-zero (an entry) and closes when it returns to zero (a flatten) or
//! crosses zero (a reversal — the old side closes, a new one opens at the
//! same fill). Scaling in/out on the same side keeps the current trade's
//! anchor unchanged (the original entry price is retained; the same
//! semantics [`Position`] uses).
//!
//! On close, a [`TradeClose`] is *staged* on the pending slot; the next
//! [`update`](Book::update) call moves it to the active slot so that this
//! bar's indicator reads see it as `Some`. The following bar's `update`
//! drains the active slot back to `None`, so a per-close indicator (say
//! `Sma::new(book.trade_return(), 30)`) only advances on the close bar
//! itself. That composes with the rest of the crate: since [`Sma`] /
//! [`StdDev`] only advance on `Some(_)` inputs, "rolling stat over the
//! last N closed trades" comes for free.

use std::cell::RefCell;
use std::marker::PhantomData;
use std::rc::Rc;

use crate::indicator::Indicator;
use crate::indicators::DEFAULT_EPSILON;
use crate::strategy::Side;
use crate::types::{Atom, Candle, Real};

/// A currently-open trade — set when the signed position count moves from
/// zero to non-zero, consumed when it returns to zero (or crosses through
/// it into a reversal, which re-anchors it on the new side).
#[derive(Debug, Clone, Copy)]
struct OpenTrade {
    /// Entry price of the trade (the fill price on the opening fill;
    /// scaled-in fills keep the original entry, matching [`Position`]).
    ///
    /// [`Position`]: crate::indicators::Position
    entry_price: Real,
    /// Equity at the moment this trade opened — the denominator for
    /// [`TradeClose::return_ratio`].
    equity_at_open: Real,
}

/// The realized outcome of a closed trade.
#[derive(Debug, Clone, Copy)]
struct TradeClose {
    /// Realized P&L in reference-currency terms (same units as the
    /// [`Book`]'s `initial_equity`).
    pnl: Real,
    /// P&L as a fraction of the equity at trade open — the composable
    /// "return of this trade" a rolling stat over `book.trade_return()`
    /// aggregates.
    return_ratio: Real,
}

/// The shared running state a [`Book`] carries.
#[derive(Debug)]
struct BookState {
    initial_equity: Real,
    cash: Real,
    position_units: Real,
    /// Marked-to-market equity as of the last [`Book::update`] call. Seeded
    /// at `initial_equity`; updated per bar to `cash + position_units *
    /// close`.
    equity: Real,
    /// Highest [`equity`](Self::equity) ever seen. Seeded at
    /// `initial_equity`; monotone across the run.
    equity_peak: Real,
    /// The just-completed bar's return (`(equity - prev_equity) /
    /// prev_equity`); `None` on the first bar (no prior equity).
    active_return: Option<Real>,
    /// Whether this bar is the first ever fed to `update`. Toggled by the
    /// first call and then never again — separates "no prior bar" from
    /// "return this bar was 0".
    first_update: bool,
    /// Metadata for the trade currently open, `None` while flat.
    open_trade: Option<OpenTrade>,
    /// A close event booked by the most recent [`apply_fill`] but not yet
    /// promoted into the accessor's active slot. Drained on the next
    /// [`update`] into `active_trade_close`.
    ///
    /// [`apply_fill`]: Book::apply_fill
    /// [`update`]: Book::update
    pending_trade_close: Option<TradeClose>,
    /// The close event visible on this bar's accessor reads. Populated
    /// from `pending_trade_close` at the start of each [`Book::update`];
    /// drained again on the *next* `update` so a `Sma` over
    /// [`Book::trade_return`] only advances on the closing bar.
    active_trade_close: Option<TradeClose>,
}

impl BookState {
    fn seed(initial_equity: Real) -> Self {
        Self {
            initial_equity,
            cash: initial_equity,
            position_units: 0.0,
            equity: initial_equity,
            equity_peak: initial_equity,
            active_return: None,
            first_update: true,
            open_trade: None,
            pending_trade_close: None,
            active_trade_close: None,
        }
    }
}

/// A strategy's own view of its equity curve and closed-trade history.
///
/// Backed by an `Rc<RefCell<…>>`, so cloning shares one state — every
/// [`BookField`] accessor holds a clone and reads the same facts the
/// strategy writes via [`apply_fill`](Book::apply_fill) /
/// [`update`](Book::update).
#[derive(Debug, Clone)]
pub struct Book(Rc<RefCell<BookState>>);

impl Book {
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

    /// Apply a fill of `units` at `price` on `side` — the book's link from
    /// the wallet's fill stream to its own cash / position / trade view.
    /// Opening from flat or reversing through zero re-anchors the current
    /// trade at `price`; flattening closes the current trade and stages a
    /// [`TradeClose`] on the pending slot (visible from the *next*
    /// [`Book::update`] onwards, then drained the bar after that).
    ///
    /// Same-side scaling keeps the current trade's entry unchanged
    /// (matches [`Position::apply`]).
    ///
    /// [`Position::apply`]: crate::indicators::Position::apply
    pub fn apply_fill(&self, side: Side, units: Real, price: Real) {
        let mut s = self.0.borrow_mut();
        let sign = side.sign();
        let signed_units = sign * units;
        let prev_units = s.position_units;
        let new_units = prev_units + signed_units;

        // Cash impact — a Buy pays out, a Sell earns.
        s.cash -= sign * units * price;
        s.position_units = new_units;

        let was_flat = prev_units.abs() <= DEFAULT_EPSILON;
        let now_flat = new_units.abs() <= DEFAULT_EPSILON;
        let crossed_zero = prev_units * new_units < -DEFAULT_EPSILON;

        if was_flat && !now_flat {
            // Fresh open from flat.
            s.open_trade = Some(OpenTrade {
                entry_price: price,
                equity_at_open: s.equity,
            });
        } else if now_flat {
            // Full close back to flat.
            if let Some(open) = s.open_trade.take() {
                let pnl = prev_units * (price - open.entry_price);
                let return_ratio = if open.equity_at_open > 0.0 {
                    pnl / open.equity_at_open
                } else {
                    0.0
                };
                s.pending_trade_close = Some(TradeClose { pnl, return_ratio });
            }
        } else if crossed_zero {
            // Reversal: close the prior trade at `price` and open a new
            // trade with the residual position at the same price.
            if let Some(open) = s.open_trade.take() {
                let pnl = prev_units * (price - open.entry_price);
                let return_ratio = if open.equity_at_open > 0.0 {
                    pnl / open.equity_at_open
                } else {
                    0.0
                };
                s.pending_trade_close = Some(TradeClose { pnl, return_ratio });
            }
            s.open_trade = Some(OpenTrade {
                entry_price: price,
                equity_at_open: s.equity,
            });
        }
        // Otherwise: same-side scaling. Keep the open trade's anchor
        // unchanged (matches Position's semantics).
    }

    /// Mark the book to market at the bar's `close` — updates equity, the
    /// running peak, the just-completed bar's return, and promotes any
    /// [`pending_trade_close`] into the accessor-visible slot.
    ///
    /// A no-op-ish call is still safe (equity just recomputes to the same
    /// value); the strategy calls this once per bar after
    /// [`apply_fill`](Book::apply_fill) has routed the bar's fills.
    ///
    /// [`pending_trade_close`]: BookState::pending_trade_close
    pub fn update(&self, candle: Candle) {
        let mut s = self.0.borrow_mut();
        // Promote pending-close → active so `book.trade_pnl` /
        // `book.trade_return` read Some this bar.
        s.active_trade_close = s.pending_trade_close.take();

        let prev_equity = s.equity;
        s.equity = s.cash + s.position_units * candle.close;
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

    /// The seed value the book started with — the assumed initial capital
    /// against which every equity / drawdown / return figure is scaled.
    pub fn initial_equity(&self) -> Real {
        self.0.borrow().initial_equity
    }

    /// The marked-to-market equity as of the most recent
    /// [`Book::update`] call. Read `initial_equity` on a freshly
    /// constructed `Book`.
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
    /// `initial_equity`), so `warm_up_period() = 0`.
    pub fn equity<In>(&self) -> BookField<In> {
        BookField::new(self.clone(), 0, |s| Some(s.equity))
    }

    /// The book's [running-peak equity](Self::equity_peak_value) as a
    /// real-valued [`Indicator`]. Always [`Some`] (seeded at
    /// `initial_equity`); `warm_up_period() = 0`.
    pub fn equity_peak<In>(&self) -> BookField<In> {
        BookField::new(self.clone(), 0, |s| Some(s.equity_peak))
    }

    /// The book's current drawdown — `(equity - peak) / peak`, always
    /// `<= 0` (and `0` at a new peak). The leaf the drawdown-throttle
    /// sizing recipe reads. Always [`Some`] in practice (only reads
    /// [`None`] if the peak has somehow drifted to zero); `warm_up_period()
    /// = 0`.
    pub fn drawdown<In>(&self) -> BookField<In> {
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
    /// (there's no prior equity to compare against), so
    /// `warm_up_period() = 2` — the first `Some` lands on the second
    /// [`Book::update`] call. The leaf a rolling realized-vol target
    /// composes over via
    /// [`StdDev::new(book.return_per_bar(), N)`](crate::indicators::StdDev).
    pub fn return_per_bar<In>(&self) -> BookField<In> {
        BookField::new(self.clone(), 2, |s| s.active_return)
    }

    /// Realized P&L of the just-closed trade in reference-currency
    /// terms (same units as `initial_equity`). [`Some`] only on the bar
    /// whose fill closed the trade, [`None`] otherwise. Warm-up is
    /// reported as `0` because a trade-close event is event-driven, not
    /// bar-count driven — a rolling stat over this source (via `Sma` /
    /// `StdDev`) advances only on the `Some` bars, so the caller's
    /// window fills when N trades have actually closed.
    pub fn trade_pnl<In>(&self) -> BookField<In> {
        BookField::new(self.clone(), 0, |s| s.active_trade_close.map(|t| t.pnl))
    }

    /// The just-closed trade's return as a fraction of the equity at
    /// trade open. [`Some`] only on the close bar, [`None`] otherwise —
    /// so rolling stats (`Sma` / `StdDev`) over this source produce the
    /// "over the last N closed trades" summaries the Kelly sizing recipe
    /// wants. Warm-up reported as `0` (event-driven; see
    /// [`trade_pnl`](Book::trade_pnl)).
    pub fn trade_return<In>(&self) -> BookField<In> {
        BookField::new(self.clone(), 0, |s| s.active_trade_close.map(|t| t.return_ratio))
    }
}

/// One field of a shared [`Book`], projected into an
/// `Indicator<Input = In, Output = Real>` so a book-anchored source
/// composes like any other. Returned by every accessor on [`Book`]; reads
/// live state and ignores its input (the owning [`Book`] is advanced by
/// the strategy). Generic over `In` so a strategy fed
/// [`Snapshot<Sym>`](crate::types::Snapshot)s builds book expressions
/// whose `Input` matches the rest of its chain.
#[derive(Debug, Clone)]
pub struct BookField<In = Atom> {
    book: Book,
    warm_up: usize,
    select: fn(&BookState) -> Option<Real>,
    _phantom: PhantomData<fn(In)>,
}

impl<In> BookField<In> {
    fn new(book: Book, warm_up: usize, select: fn(&BookState) -> Option<Real>) -> Self {
        Self {
            book,
            warm_up,
            select,
            _phantom: PhantomData,
        }
    }
}

impl<In> Indicator for BookField<In> {
    type Input = In;
    type Output = Real;

    fn update(&mut self, _input: In) -> Option<Real> {
        self.value()
    }

    fn value(&self) -> Option<Real> {
        (self.select)(&self.book.0.borrow())
    }

    /// Per-accessor: `0` for equity / equity_peak / drawdown (seeded to
    /// `initial_equity`, always `Some`), `2` for
    /// [`Book::return_per_bar`] (needs a prior equity to compute
    /// `(equity - prev_equity) / prev_equity`), and `0` for
    /// [`Book::trade_pnl`] / [`Book::trade_return`] (event-driven; the
    /// caller's rolling window fills when trades actually close, not on
    /// a fixed bar count).
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
        let _ = Book::new(0.0);
    }

    #[test]
    fn initial_state_reads_seed_and_no_return() {
        let book = Book::new(1_000.0);
        assert_eq!(book.equity_value(), 1_000.0);
        assert_eq!(book.equity_peak_value(), 1_000.0);
        let eq = book.equity::<Atom>();
        let dd = book.drawdown::<Atom>();
        let ret = book.return_per_bar::<Atom>();
        assert_eq!(eq.value(), Some(1_000.0));
        assert_eq!(dd.value(), Some(0.0));
        // No update yet, per-bar return is None.
        assert_eq!(ret.value(), None);
    }

    #[test]
    fn buy_and_hold_grows_equity_marks_and_returns() {
        // Seed 1000, buy 10 @ 100 (cash → 0), hold through 100, 110, 120.
        let book = Book::new(1_000.0);
        book.apply_fill(Side::Buy, 10.0, 100.0);
        book.update(bar(100.0));
        assert_eq!(book.equity_value(), 1_000.0); // marked at 100
        assert_eq!(book.return_per_bar::<Atom>().value(), None); // first update
        book.update(bar(110.0));
        assert_eq!(book.equity_value(), 1_100.0);
        assert!((book.return_per_bar::<Atom>().value().unwrap() - 0.1).abs() < 1e-12);
        book.update(bar(120.0));
        assert_eq!(book.equity_value(), 1_200.0);
        assert!(
            (book.return_per_bar::<Atom>().value().unwrap() - (1_200.0 / 1_100.0 - 1.0)).abs()
                < 1e-12
        );
        // Peak tracks the high.
        assert_eq!(book.equity_peak_value(), 1_200.0);
        // Drawdown is zero at a new peak.
        assert_eq!(book.drawdown::<Atom>().value(), Some(0.0));
    }

    #[test]
    fn drawdown_and_peak_survive_a_dip() {
        let book = Book::new(1_000.0);
        book.apply_fill(Side::Buy, 10.0, 100.0);
        book.update(bar(100.0));
        book.update(bar(120.0)); // peak
        book.update(bar(108.0)); // 10% dip from the peak
        assert_eq!(book.equity_peak_value(), 1_200.0);
        assert_eq!(book.equity_value(), 1_080.0);
        // (1080 − 1200) / 1200 = −0.10
        assert!(
            (book.drawdown::<Atom>().value().unwrap() - (-0.10)).abs() < 1e-12,
            "got {:?}",
            book.drawdown::<Atom>().value()
        );
    }

    #[test]
    fn closing_a_winning_trade_reports_pnl_and_return_on_the_close_bar() {
        let book = Book::new(1_000.0);
        book.apply_fill(Side::Buy, 10.0, 100.0);
        book.update(bar(100.0));
        book.update(bar(110.0));
        // Close at 120: on_fill first, then update — accessors see Some
        // during this bar's reads.
        book.apply_fill(Side::Sell, 10.0, 120.0);
        book.update(bar(120.0));
        let pnl = book.trade_pnl::<Atom>().value();
        let ret = book.trade_return::<Atom>().value();
        assert!(pnl.is_some() && ret.is_some(), "pnl {pnl:?} ret {ret:?}");
        assert!((pnl.unwrap() - 200.0).abs() < 1e-12);
        assert!((ret.unwrap() - 0.20).abs() < 1e-12);
        // Next bar (no new close): active slot drains.
        book.update(bar(120.0));
        assert_eq!(book.trade_pnl::<Atom>().value(), None);
        assert_eq!(book.trade_return::<Atom>().value(), None);
    }

    #[test]
    fn closing_a_losing_short_reports_negative_pnl() {
        let book = Book::new(1_000.0);
        book.apply_fill(Side::Sell, 10.0, 100.0);
        book.update(bar(100.0));
        book.update(bar(105.0)); // adverse move
        book.apply_fill(Side::Buy, 10.0, 105.0);
        book.update(bar(105.0));
        // prev_units was -10; pnl = -10 * (105 - 100) = -50.
        assert!((book.trade_pnl::<Atom>().value().unwrap() - (-50.0)).abs() < 1e-12);
        assert!((book.trade_return::<Atom>().value().unwrap() - (-0.05)).abs() < 1e-12);
    }

    #[test]
    fn reversal_closes_the_old_trade_and_opens_a_new_one() {
        let book = Book::new(1_000.0);
        // Long 5 @ 100, then reversal Sell 10 @ 120 → closes long, opens short 5.
        book.apply_fill(Side::Buy, 5.0, 100.0);
        book.update(bar(100.0));
        book.update(bar(110.0)); // equity now 1_050
        book.apply_fill(Side::Sell, 10.0, 120.0);
        book.update(bar(120.0));
        // Old trade closed: pnl = 5 * (120 − 100) = 100; equity_at_open = 1_000.
        let pnl = book.trade_pnl::<Atom>().value().unwrap();
        let ret = book.trade_return::<Atom>().value().unwrap();
        assert!((pnl - 100.0).abs() < 1e-12);
        assert!((ret - 0.10).abs() < 1e-12);
        // New short trade opened at 120 with the residual −5 units.
        // Drain the close event on the next bar.
        book.update(bar(120.0));
        assert_eq!(book.trade_return::<Atom>().value(), None);
        // The residual short is still open. Close it at 100 — that should
        // yield pnl = −5 * (100 − 120) = 100 (a winning short covering).
        book.apply_fill(Side::Buy, 5.0, 100.0);
        book.update(bar(100.0));
        let pnl2 = book.trade_pnl::<Atom>().value().unwrap();
        assert!((pnl2 - 100.0).abs() < 1e-12);
    }

    #[test]
    fn scale_in_same_side_keeps_the_original_entry() {
        let book = Book::new(1_000.0);
        book.apply_fill(Side::Buy, 5.0, 100.0);
        book.update(bar(100.0));
        book.apply_fill(Side::Buy, 5.0, 110.0); // scale in on same side
        // Closing at 120 — pnl uses the *original* 100 entry across the full 10 units:
        // pnl = 10 * (120 − 100) = 200.
        // (matches Position's semantics of holding the anchor)
        book.update(bar(120.0));
        book.apply_fill(Side::Sell, 10.0, 120.0);
        book.update(bar(120.0));
        assert!((book.trade_pnl::<Atom>().value().unwrap() - 200.0).abs() < 1e-12);
    }

    #[test]
    fn reset_restores_the_freshly_constructed_state() {
        let book = Book::new(1_000.0);
        book.apply_fill(Side::Buy, 10.0, 100.0);
        book.update(bar(100.0));
        book.update(bar(120.0));
        book.reset();
        assert_eq!(book.equity_value(), 1_000.0);
        assert_eq!(book.equity_peak_value(), 1_000.0);
        assert_eq!(book.return_per_bar::<Atom>().value(), None);
        assert_eq!(book.trade_pnl::<Atom>().value(), None);
        assert_eq!(book.initial_equity(), 1_000.0);
    }

    #[test]
    fn accessors_share_state_via_clone() {
        let book = Book::new(1_000.0);
        let eq1 = book.equity::<Atom>();
        let eq2 = book.equity::<Atom>();
        book.apply_fill(Side::Buy, 10.0, 100.0);
        book.update(bar(120.0));
        // Both clones read the same shared state.
        assert_eq!(eq1.value(), eq2.value());
        assert!((eq1.value().unwrap() - 1_200.0).abs() < 1e-12);
    }
}
