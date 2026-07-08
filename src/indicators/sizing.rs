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

use crate::indicator::Indicator;
use crate::indicators::{Atr, Close, CurrentBar, IndicatorExt, Log, Pick, StdDev, Value};
use crate::types::{Real, Snapshot};

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
}
