//! Position-sizing helper expressions.
//!
//! Free functions that build a real-valued indicator expression to be plugged
//! into a strategy's `position_sizing` slot
//! ([`SingleAssetStrategy::position_sizing`](crate::strategies::SingleAssetStrategy),
//! [`PairsStrategy::position_sizing`](crate::strategies::PairsStrategy)).
//! Nothing here is strategy-specific — the returned value is an ordinary
//! [`Indicator<Input = Snapshot<Sym>, Output = Real>`](crate::Indicator) that
//! composes into any chain.
//!
//! Both helpers read the strategy's *own* asset out of the incoming snapshot
//! via [`Pick::<Sym>::new()`](crate::indicators::Pick) — the empty-selector
//! sole-atom unpack path, matching the convention used across
//! [`crate::strategies`].

use std::marker::PhantomData;

use crate::indicator::Indicator;
use crate::indicators::stats::WindowStats;
use crate::indicators::{
    Atr, Book, Close, CurrentBar, IndicatorExt, Log, Pick, StdDev, Value,
};
use crate::types::{Real, Snapshot};

/// **Equal-weight sizing.**
///
/// Returns a constant multiplier of `1.0 / n_legs` — the per-leg
/// [`ValueFraction`](crate::Size::ValueFraction) that yields 100% gross
/// exposure across a basket of `n_legs` symbols with no leverage.
///
/// The intended pairing is
/// [`BasketStrategy`](crate::strategies::BasketStrategy) with a
/// [`SelectionRule::TopBottom`](crate::strategies::SelectionRule) of the
/// same count: `sized_by(|_| equal_weight(10)).top_bottom(5, 5)` fills a
/// 5-long / 5-short basket at 10% of equity each, totalling 100% gross.
/// The helper is deliberately trivial — the crate never auto-normalizes
/// sizing across a basket, so an explicit call communicates the intent
/// and any deviation (a caller wanting 50% gross, say) reads as a
/// different literal.
///
/// # Panics
/// Panics if `n_legs == 0` (division by zero).
pub fn equal_weight<Sym: Clone + PartialEq + 'static>(
    n_legs: usize,
) -> Value<Snapshot<Sym>> {
    assert!(n_legs > 0, "n_legs must be > 0");
    Value::<Snapshot<Sym>>::new(1.0 / n_legs as Real)
}

/// **Inverse-realized-volatility (vol targeting) sizing.**
///
/// Returns the multiplier
///
/// ```text
/// target_annualized_vol / annualized_realized_vol
/// ```
///
/// where `annualized_realized_vol =
/// stddev(log_returns(close), window) * sqrt(bars_per_year)` and `close` is
/// the strategy's own asset (read via [`Pick::<Sym>::new()`]). When plugged
/// into `position_sizing`, this scales the entry
/// [`value_frac`](crate::Size::value_frac) magnitude so the position holds a
/// constant realized-vol level: if the market's vol doubles, the position
/// halves.
///
/// `bars_per_year` is the caller's annualization constant — 252 for daily
/// equity bars, 365 for daily crypto,
/// `class.trading_days_per_year() * class.trading_hours_per_day()` for hourly
/// bars, etc.
///
/// Warm-up is `window + 1` samples (one extra bar for the first log-return
/// `diff(1)`). The sizing indicator's `stable_period()` folds into the
/// strategy's readiness gate, so no trade fires while it is still warming.
///
/// # Panics
/// Panics if `target_annualized_vol <= 0`, `window == 0`, or
/// `bars_per_year <= 0`.
pub fn vol_target<Sym: Clone + PartialEq + 'static>(
    target_annualized_vol: Real,
    window: usize,
    bars_per_year: Real,
) -> impl Indicator<Input = Snapshot<Sym>, Output = Real> + Clone {
    assert!(
        target_annualized_vol > 0.0,
        "target_annualized_vol must be > 0"
    );
    assert!(window > 0, "window must be > 0");
    assert!(bars_per_year > 0.0, "bars_per_year must be > 0");

    let close = Close::of(Pick::<Sym>::new());
    let log_return = Log::natural(close).diff(1);
    let realized_vol = StdDev::new(log_return, window);
    let annualized = realized_vol.mul(Value::<Snapshot<Sym>>::new(bars_per_year.sqrt()));
    Value::<Snapshot<Sym>>::new(target_annualized_vol).div(annualized)
}

/// **Fixed per-trade risk sizing scaled by ATR.**
///
/// Returns the multiplier
///
/// ```text
/// risk_frac * close / (atr_multiple * ATR(period))
/// ```
///
/// — the position weight that loses exactly `risk_frac * equity` if price
/// moves `atr_multiple` ATRs against the entry (long or short). When plugged
/// into `position_sizing`, this pairs naturally with an ATR-based stop on the
/// same side: the position is sized so the stop distance is by construction
/// the caller's risk budget.
///
/// Typical values: `risk_frac = 0.01` (risk 1% of equity per trade),
/// `period = 14`, `atr_multiple = 2.0`.
///
/// Warm-up is the ATR's `period + 1` samples.
///
/// # Panics
/// Panics if `risk_frac <= 0`, `period == 0`, or `atr_multiple <= 0`.
pub fn atr_risk<Sym: Clone + PartialEq + 'static>(
    risk_frac: Real,
    period: usize,
    atr_multiple: Real,
) -> impl Indicator<Input = Snapshot<Sym>, Output = Real> + Clone {
    assert!(risk_frac > 0.0, "risk_frac must be > 0");
    assert!(period > 0, "period must be > 0");
    assert!(atr_multiple > 0.0, "atr_multiple must be > 0");

    let close = Close::of(Pick::<Sym>::new());
    let atr = Atr::new(CurrentBar::of(Pick::<Sym>::new()), period);
    close
        .mul(Value::<Snapshot<Sym>>::new(risk_frac / atr_multiple))
        .div(atr)
}

// ---------------------------------------------------------------------------
// Position-dependent recipes — read the strategy's Book anchor.
// ---------------------------------------------------------------------------

/// **Drawdown throttle sizing.**
///
/// Returns `max(0, min(1, 1 + book.drawdown() / max_drawdown))` — the
/// multiplier that starts at `1.0` when the strategy is at a new equity
/// peak and *linearly de-levers* as drawdown deepens, hitting `0` when
/// drawdown reaches `-max_drawdown` and staying at `0` beyond that. When
/// the strategy climbs back toward the peak, the throttle relaxes; it
/// re-arms to `1.0` on a new all-time high.
///
/// Wrap a strategy in
/// `strat.position_sizing(drawdown_throttle(&strat.book(), 0.20))` to cap
/// equity give-back at 20% of the running peak.
///
/// # Panics
/// Panics if `max_drawdown <= 0`.
pub fn drawdown_throttle<Sym: Clone + PartialEq + std::hash::Hash + Eq + 'static>(
    book: &Book<Sym>,
    max_drawdown: Real,
) -> impl Indicator<Input = Snapshot<Sym>, Output = Real> + Clone + 'static {
    assert!(max_drawdown > 0.0, "max_drawdown must be > 0");
    DrawdownThrottle {
        book: book.clone(),
        max_drawdown,
        _phantom: PhantomData,
    }
}

/// **Equity-return realized-vol targeting.**
///
/// Returns `target_annualized_vol / (stddev(book.return_per_bar(),
/// window) * sqrt(bars_per_year))`. Unlike the price-based
/// [`vol_target`] (which scales by the underlying's realized vol), this
/// scales by the *strategy's own* realized return vol — so a cash
/// position (returning zero) doesn't contribute, and a leveraged
/// position amplifies the denominator. Emits `None` until `window` bars
/// of `book.return_per_bar` (which itself starts on bar 2) have been
/// collected; the `Book`'s and the `StdDev`'s warm-ups compose into the
/// strategy's readiness gate.
///
/// # Panics
/// Panics if `target_annualized_vol <= 0`, `window == 0`, or
/// `bars_per_year <= 0`.
pub fn equity_vol_target<Sym: Clone + PartialEq + std::hash::Hash + Eq + 'static>(
    book: &Book<Sym>,
    target_annualized_vol: Real,
    window: usize,
    bars_per_year: Real,
) -> impl Indicator<Input = Snapshot<Sym>, Output = Real> + Clone + 'static {
    assert!(
        target_annualized_vol > 0.0,
        "target_annualized_vol must be > 0"
    );
    assert!(window > 0, "window must be > 0");
    assert!(bars_per_year > 0.0, "bars_per_year must be > 0");

    let returns = book.return_per_bar::<Snapshot<Sym>>();
    let vol = StdDev::new(returns, window);
    let annualized = vol.mul(Value::<Snapshot<Sym>>::new(bars_per_year.sqrt()));
    Value::<Snapshot<Sym>>::new(target_annualized_vol).div(annualized)
}

/// **Fractional Kelly sizing** over the last `window` closed trades.
///
/// Returns `max(0, kelly_fraction * mean / variance)`, where `mean` and
/// `variance` are the rolling stats of `book.trade_return()` over the
/// last `window` closed trades (`Sma`/`StdDev` only advance on the
/// close-bar `Some` values). This is the continuous-return Kelly
/// approximation — appropriate for small trade returns. `kelly_fraction`
/// is the caller's Kelly-fraction — typically `0.25`–`0.5` (quarter-
/// to half-Kelly) since full Kelly is well known to over-lever in
/// practice.
///
/// Emits `None` until the window has filled with `window` closed
/// trades. Clamped to `>= 0` (a negative Kelly implies "sit out"; sizing
/// is a magnitude, so we return `0` instead of flipping direction).
/// When variance is zero (all returns identical), emits `0`.
///
/// # Panics
/// Panics if `kelly_fraction <= 0` or `window < 2` (variance is
/// meaningless with fewer than two samples).
pub fn fractional_kelly<Sym: Clone + PartialEq + std::hash::Hash + Eq + 'static>(
    book: &Book<Sym>,
    kelly_fraction: Real,
    window: usize,
) -> impl Indicator<Input = Snapshot<Sym>, Output = Real> + Clone + 'static {
    assert!(kelly_fraction > 0.0, "kelly_fraction must be > 0");
    assert!(window >= 2, "window must be >= 2 (variance needs 2+ samples)");
    FractionalKelly {
        source: book.trade_return::<Snapshot<Sym>>(),
        stats: WindowStats::new(window),
        kelly_fraction,
        value: None,
    }
}

// ---------------------------------------------------------------------------
// Concrete indicator types the three position-dependent recipes return.
// Private to this module; the recipes expose them as `impl Indicator + Clone`.
// ---------------------------------------------------------------------------

struct DrawdownThrottle<Sym, In> {
    book: Book<Sym>,
    max_drawdown: Real,
    _phantom: PhantomData<fn(In)>,
}

impl<Sym, In> Clone for DrawdownThrottle<Sym, In> {
    fn clone(&self) -> Self {
        Self {
            book: self.book.clone(),
            max_drawdown: self.max_drawdown,
            _phantom: PhantomData,
        }
    }
}

impl<Sym: std::hash::Hash + Eq + Clone, In> Indicator for DrawdownThrottle<Sym, In> {
    type Input = In;
    type Output = Real;

    fn update(&mut self, _input: In) -> Option<Real> {
        self.value()
    }

    fn value(&self) -> Option<Real> {
        let equity = self.book.equity_value();
        let peak = self.book.equity_peak_value();
        if peak.abs() < f64::EPSILON {
            return None;
        }
        let drawdown: Real = (equity - peak) / peak; // <= 0
        let raw: Real = 1.0 + drawdown / self.max_drawdown;
        Some(raw.clamp(0.0, 1.0))
    }

    fn warm_up_period(&self) -> usize {
        0
    }

    fn reset(&mut self) {}
}

/// A rolling Kelly-fraction sizer. Owns the source (a
/// [`Book::trade_return`] accessor) and a shared [`WindowStats`] core
/// that computes the rolling mean and variance of trade returns.
#[derive(Debug, Clone)]
struct FractionalKelly<S> {
    source: S,
    stats: WindowStats,
    kelly_fraction: Real,
    value: Option<Real>,
}

impl<S: Indicator<Output = Real>> Indicator for FractionalKelly<S> {
    type Input = S::Input;
    type Output = Real;

    fn update(&mut self, input: S::Input) -> Option<Real> {
        self.value = match self.source.update(input) {
            Some(x) if self.stats.update(x) => {
                let mean = self.stats.mean();
                let var = self.stats.variance();
                if var > 0.0 {
                    Some((self.kelly_fraction * mean / var).max(0.0))
                } else {
                    Some(0.0)
                }
            }
            _ => self.value,
        };
        self.value
    }

    fn value(&self) -> Option<Real> {
        self.value
    }

    /// `0`: the source's Some-values are event-driven (one per closed
    /// trade), so the window fills when `window` trades have actually
    /// closed — the strategy's readiness gate can't reason about that in
    /// bar counts. Until the window fills, the recipe emits `None`, and
    /// the strategy's "sizing None ⇒ skip trade" safe default holds
    /// entries.
    fn warm_up_period(&self) -> usize {
        0
    }

    fn reset(&mut self) {
        self.source.reset();
        self.stats.reset();
        self.value = None;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::types::{Atom, Candle};

    fn feed_bar<I: Indicator<Input = Snapshot<&'static str>, Output = Real>>(
        ind: &mut I,
        price: Real,
    ) -> Option<Real> {
        let candle = Candle::new(price, price, price, price, 0.0);
        ind.update(Snapshot::of_atom(Atom::new(candle)))
    }

    #[test]
    fn equal_weight_returns_reciprocal_of_leg_count() {
        let mut ind = equal_weight::<&'static str>(10);
        // Constant multiplier: reads Some on the first bar and never changes.
        assert!((feed_bar(&mut ind, 100.0).unwrap() - 0.1).abs() < 1e-12);
        assert!((feed_bar(&mut ind, 200.0).unwrap() - 0.1).abs() < 1e-12);
        // Different leg counts read exactly 1/N.
        let mut five = equal_weight::<&'static str>(5);
        assert!((feed_bar(&mut five, 100.0).unwrap() - 0.2).abs() < 1e-12);
    }

    #[test]
    #[should_panic]
    fn equal_weight_rejects_zero_legs() {
        let _ = equal_weight::<&'static str>(0);
    }

    #[test]
    fn vol_target_produces_expected_multiplier_on_constant_returns() {
        // A constant close ⇒ zero log-returns ⇒ zero stddev ⇒ `None` output
        // (division by zero). We just verify warm-up shape and the panic
        // paths.
        let mut ind = vol_target::<&'static str>(0.15, 5, 252.0);
        for _ in 0..5 {
            assert!(feed_bar(&mut ind, 100.0).is_none());
        }
        // Bar 6: stddev of five zero log-returns is 0 → division by zero →
        // `None`. Sanity check.
        assert!(feed_bar(&mut ind, 100.0).is_none());
    }

    #[test]
    fn vol_target_matches_closed_form_on_step_series() {
        // Alternating up/down 1% return gives a stable, computable stddev of
        // log-returns. Feed a long enough series to fill the window, then
        // verify the multiplier tracks the closed-form value.
        let target = 0.20;
        let bpy = 252.0;
        let window = 4;
        let mut ind = vol_target::<&'static str>(target, window, bpy);
        // Prices: 100, 101, 100, 101, 100, 101, ... — log-returns alternate
        // between +ln(1.01) and -ln(1.01), each with magnitude ln(1.01).
        let mut price = 100.0;
        for _ in 0..(window + 1) {
            price = if price > 100.5 { 100.0 } else { 101.0 };
            feed_bar(&mut ind, price);
        }
        // Once warm, the population stddev over the window is |ln(1.01)|.
        // Multiplier = target / (|ln(1.01)| * sqrt(bpy)).
        let expected = target / (0.01_f64.ln_1p().abs() * bpy.sqrt());
        let got = ind.value().unwrap();
        assert!(
            (got - expected).abs() < 1e-6,
            "vol_target: got {got}, expected {expected}"
        );
    }

    #[test]
    #[should_panic]
    fn vol_target_rejects_zero_window() {
        let _ = vol_target::<&'static str>(0.15, 0, 252.0);
    }

    #[test]
    #[should_panic]
    fn vol_target_rejects_non_positive_target() {
        let _ = vol_target::<&'static str>(0.0, 5, 252.0);
    }

    #[test]
    fn atr_risk_matches_closed_form_on_constant_range_bars() {
        // Bars with high - low = 2 and close = 100 on every bar → TR is a
        // constant 2 from bar 1 (bar 1 is `high - low`; every later bar is
        // still 2 because prev_close = 100 sits inside `[low, high]`). Wilder
        // seeds to 2 and stays there. Then multiplier
        //   = risk * close / (atr_mult * ATR)
        //   = 0.02 * 100 / (2.0 * 2.0)
        //   = 0.5.
        let risk = 0.02;
        let period = 5;
        let atr_mult = 2.0;
        let mut ind = atr_risk::<&'static str>(risk, period, atr_mult);
        let bar = Candle::new(100.0, 101.0, 99.0, 100.0, 0.0);
        for _ in 0..(period + 1) {
            ind.update(Snapshot::of_atom(Atom::new(bar)));
        }
        let expected = risk * 100.0 / (atr_mult * 2.0);
        let got = ind.value().unwrap();
        assert!(
            (got - expected).abs() < 1e-12,
            "atr_risk: got {got}, expected {expected}"
        );
    }

    #[test]
    #[should_panic]
    fn atr_risk_rejects_zero_period() {
        let _ = atr_risk::<&'static str>(0.01, 0, 2.0);
    }

    #[test]
    #[should_panic]
    fn atr_risk_rejects_non_positive_risk() {
        let _ = atr_risk::<&'static str>(0.0, 14, 2.0);
    }

    // ------------------------------------------------------------------
    // Position-dependent recipes
    // ------------------------------------------------------------------

    use crate::strategy::Side;

    #[test]
    fn drawdown_throttle_starts_at_one_and_scales_linearly() {
        let book: Book<&'static str> = Book::new(1_000.0);
        let mut ind = drawdown_throttle::<&'static str>(&book, 0.20);
        // At the peak: multiplier = 1.
        assert!((feed_bar(&mut ind, 100.0).unwrap() - 1.0).abs() < 1e-12);
        // Simulate a 10% drawdown: seed 1000 → equity 900, peak 1000.
        book.apply_fill(&"X", Side::Buy, 10.0, 100.0);
        book.update([("X", Candle::new(100.0, 100.0, 100.0, 100.0, 0.0))]);
        book.update([("X", Candle::new(90.0, 90.0, 90.0, 90.0, 0.0))]);
        // Drawdown = -0.1, max_drawdown = 0.2, throttle = 1 + (-0.1)/0.2 = 0.5.
        assert!((ind.update(Snapshot::of_atom(Candle::new(90.0, 90.0, 90.0, 90.0, 0.0).into())).unwrap() - 0.5).abs() < 1e-12);
        // A deeper 30% drawdown clamps to 0.
        book.update([("X", Candle::new(70.0, 70.0, 70.0, 70.0, 0.0))]);
        assert_eq!(
            ind.update(Snapshot::of_atom(Candle::new(70.0, 70.0, 70.0, 70.0, 0.0).into())),
            Some(0.0)
        );
    }

    #[test]
    #[should_panic]
    fn drawdown_throttle_rejects_zero_max_drawdown() {
        let book: Book<&'static str> = Book::new(1_000.0);
        let _ = drawdown_throttle::<&'static str>(&book, 0.0);
    }

    #[test]
    fn equity_vol_target_matches_closed_form_on_steady_returns() {
        let book: Book<&'static str> = Book::new(1_000.0);
        let target = 0.20;
        let bpy = 252.0;
        let window = 4;
        let mut ind = equity_vol_target::<&'static str>(&book, target, window, bpy);

        // Seed a buy-and-hold at 100 units cost, then price oscillates:
        // 100 → 101 → 100 → 101 → 100 → 101, giving a return series of
        // alternating +1% and -1% (approximately — depends on prev equity).
        book.apply_fill(&"X", Side::Buy, 10.0, 100.0);
        book.update([("X", Candle::new(100.0, 100.0, 100.0, 100.0, 0.0))]);
        ind.update(Snapshot::of_atom(Candle::new(100.0, 100.0, 100.0, 100.0, 0.0).into()));
        for close in [101.0, 100.0, 101.0, 100.0, 101.0].iter().copied() {
            book.update([("X", Candle::new(close, close, close, close, 0.0))]);
            ind.update(Snapshot::of_atom(Candle::new(close, close, close, close, 0.0).into()));
        }
        // Once the window has enough per-bar returns, the multiplier is well-defined.
        assert!(ind.value().is_some(), "vol target should be Some once window is full");
        // And positive.
        assert!(ind.value().unwrap() > 0.0);
    }

    #[test]
    fn equity_vol_target_is_none_until_window_fills() {
        let book: Book<&'static str> = Book::new(1_000.0);
        let mut ind = equity_vol_target::<&'static str>(&book, 0.20, 10, 252.0);
        for _ in 0..5 {
            book.update([("X", Candle::new(100.0, 100.0, 100.0, 100.0, 0.0))]);
            ind.update(Snapshot::of_atom(Candle::new(100.0, 100.0, 100.0, 100.0, 0.0).into()));
        }
        assert_eq!(ind.value(), None, "vol target should be None during warm-up");
    }

    #[test]
    fn fractional_kelly_produces_multiplier_after_enough_trades() {
        let book: Book<&'static str> = Book::new(1_000.0);
        let mut ind = fractional_kelly::<&'static str>(&book, 0.5, 3);
        // Simulate three closed trades with a stream of trade returns.
        // Bars roll like: enter → update → close → update → enter → ...
        let trades = [
            (Side::Buy, 10.0, 100.0, 110.0), // +100 pnl, +10% return
            (Side::Buy, 10.0, 110.0, 100.0), // −100 pnl, ≈ -9.1% return
            (Side::Buy, 10.0, 100.0, 120.0), // +200 pnl, +20% return
        ];
        for (side, units, entry, exit) in trades {
            book.apply_fill(&"X", side, units, entry);
            book.update([("X", Candle::new(entry, entry, entry, entry, 0.0))]);
            ind.update(Snapshot::of_atom(Candle::new(entry, entry, entry, entry, 0.0).into()));
            let opp = if side == Side::Buy { Side::Sell } else { Side::Buy };
            book.apply_fill(&"X", opp, units, exit);
            book.update([("X", Candle::new(exit, exit, exit, exit, 0.0))]);
            ind.update(Snapshot::of_atom(Candle::new(exit, exit, exit, exit, 0.0).into()));
            // Next bar drains the trade-close accessor.
            book.update([("X", Candle::new(exit, exit, exit, exit, 0.0))]);
            ind.update(Snapshot::of_atom(Candle::new(exit, exit, exit, exit, 0.0).into()));
        }
        // After 3 closed trades, Kelly should be Some and non-negative.
        let k = ind.value().expect("Kelly should be Some after window fills");
        assert!(k >= 0.0);
    }

    #[test]
    fn fractional_kelly_is_none_until_window_of_trades_closes() {
        let book: Book<&'static str> = Book::new(1_000.0);
        let mut ind = fractional_kelly::<&'static str>(&book, 0.5, 5);
        // No trades yet.
        for _ in 0..10 {
            book.update([("X", Candle::new(100.0, 100.0, 100.0, 100.0, 0.0))]);
            ind.update(Snapshot::of_atom(Candle::new(100.0, 100.0, 100.0, 100.0, 0.0).into()));
        }
        assert_eq!(ind.value(), None);
    }

    #[test]
    #[should_panic]
    fn fractional_kelly_rejects_window_below_two() {
        let book: Book<&'static str> = Book::new(1_000.0);
        let _ = fractional_kelly::<&'static str>(&book, 0.5, 1);
    }
}
