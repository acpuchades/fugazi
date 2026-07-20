//! Warm-up / unstable-period introspection.
//!
//! `warm_up_period` is exact: for every indicator, `update` yields `None` for
//! the first `warm_up_period() - 1` samples and `Some` from sample
//! `warm_up_period()` onwards (given non-degenerate data). `unstable_period`
//! is `0` for windowed indicators and, for the recursive ones, counts the
//! samples until the seed's residual weight decays below 0.1% — checked here
//! by replaying only the last `stable_period()` samples and comparing against
//! an instance that saw the full history.

use fugazi::indicators::{
    Adx, Aroon, Atr, Bollinger, Cci, Correlation, CurrentTime, Current, Day, DayOfWeek, DayOfYear,
    Dmi, Donchian, Ema, GarmanKlass, Hma, Hour, Identity, IsWeekday, IsWeekend, Keltner, Kurtosis,
    Latch, Log, Macd, Mfi, Minute, Month, Obv, Parkinson, Quarter, Resample, Rma, RogersSatchell,
    Rsi, Sar, Second, Skewness, Sma, StdDev, Stochastic, TrueRange, UnixMillis, UnixSeconds, Value,
    VarianceRatio, Vwap, WeekOfYear, WilliamsR, Wma, Year, ZScore,
};
use fugazi::prelude::*;
use fugazi::types::{Atom, Candle, Real, Timestamp};

/// Synthetic wiggly-but-trending bars: well-formed OHLC, positive volume, no
/// degenerate (flat / zero-volume) stretches.
fn bars(n: usize) -> Vec<Candle> {
    (0..n)
        .map(|i| {
            let t = i as Real;
            let base = 100.0 + (t * 0.7).sin() * 5.0 + t * 0.1;
            let open = base - 0.3;
            let close = base + 0.4 * (t * 0.9).sin();
            let high = base + 1.0 + (t * 1.3).sin().abs();
            let low = base - 1.0 - (t * 1.7).cos().abs();
            Candle::new(open, high, low, close, 1000.0 + t)
        })
        .collect()
}

/// Drive `ind` over `inputs` and assert the first `Some` lands exactly on
/// sample `warm_up_period()`.
fn assert_exact_warm_up<I: Indicator>(mut ind: I, inputs: Vec<I::Input>, name: &str) {
    let w = ind.warm_up_period();
    if w == 0 {
        assert!(ind.is_ready(), "{name}: warm-up 0 should be ready untouched");
        return;
    }
    assert!(
        inputs.len() >= w + 3,
        "{name}: test needs more samples than the warm-up ({w})"
    );
    for (i, input) in inputs.into_iter().enumerate() {
        let sample = i + 1;
        let ready = ind.update(input).is_some();
        assert_eq!(
            ready,
            sample >= w,
            "{name}: readiness at sample {sample} contradicts warm_up_period() = {w}"
        );
    }
}

fn candle_case(ind: impl Indicator<Input = Atom>, name: &str) {
    let n = ind.warm_up_period() + 5;
    let atoms: Vec<Atom> = bars(n).into_iter().map(Atom::from).collect();
    assert_exact_warm_up(ind, atoms, name);
}

/// Feed timed atoms (one-minute cadence starting 2024-01-01 UTC) so calendar
/// indicators, which return `None` on `atom.time == None`, get a bar-open
/// timestamp to decompose.
fn timed_candle_case(ind: impl Indicator<Input = Atom>, name: &str) {
    let base = 1_704_067_200_000i64; // 2024-01-01 00:00:00 UTC in ms
    let n = ind.warm_up_period() + 5;
    let atoms: Vec<Atom> = bars(n)
        .into_iter()
        .enumerate()
        .map(|(i, c)| Atom::with_time(c, Timestamp(base + (i as i64) * 60_000)))
        .collect();
    assert_exact_warm_up(ind, atoms, name);
}

fn real_case(ind: impl Indicator<Input = Real>, name: &str) {
    let n = ind.warm_up_period() + 5;
    let series = bars(n).iter().map(|b| b.close).collect();
    assert_exact_warm_up(ind, series, name);
}

#[test]
fn warm_up_is_exact_for_the_catalogue() {
    candle_case(Current::close(), "close");
    candle_case(Log::natural(Current::close()), "log");
    candle_case(TrueRange::new(Current::candle()), "true_range");
    candle_case(Obv::new(Current::candle()), "obv");
    candle_case(fugazi::indicators::Ad::new(Current::candle()), "ad");
    candle_case(Vwap::new(Current::candle(), 20), "vwap");
    candle_case(Sar::with_defaults(Current::candle()), "sar");
    candle_case(Atr::new(Current::candle(), 14), "atr");
    candle_case(Parkinson::new(Current::candle(), 20), "parkinson");
    candle_case(GarmanKlass::new(Current::candle(), 20), "garman_klass");
    candle_case(RogersSatchell::new(Current::candle(), 20), "rogers_satchell");
    candle_case(Mfi::new(Current::candle(), 14), "mfi");
    candle_case(Dmi::new(Current::candle(), 14), "dmi");
    candle_case(Adx::new(Current::candle(), 14), "adx");
    candle_case(Aroon::new(Current::candle(), 25), "aroon");
    candle_case(WilliamsR::new(Current::candle(), 14), "williams_r");
    candle_case(Sma::new(Current::close(), 20), "sma");
    candle_case(Ema::new(Current::close(), 20), "ema");
    candle_case(Rma::new(Current::close(), 14), "rma");
    candle_case(Wma::new(Current::close(), 20), "wma");
    candle_case(Hma::new(Current::close(), 16), "hma");
    candle_case(Rsi::new(Current::close(), 14), "rsi");
    candle_case(Macd::new(Current::close(), 12, 26, 9), "macd");
    candle_case(StdDev::new(Current::close(), 20), "stddev");
    candle_case(Skewness::new(Current::close(), 20), "skewness");
    candle_case(Kurtosis::new(Current::close(), 20), "kurtosis");
    candle_case(ZScore::new(Current::close(), 20), "zscore");
    candle_case(
        Correlation::new(Current::close(), Current::open(), 20),
        "correlation",
    );
    candle_case(
        VarianceRatio::new(Current::close(), 20, 2),
        "variance_ratio",
    );
    candle_case(Cci::new(Current::typical(), 20), "cci");
    candle_case(Bollinger::new(Current::close(), 20, 2.0), "bollinger");
    candle_case(Stochastic::new(Current::close(), 14), "stochastic");
    candle_case(
        Stochastic::new(Rsi::new(Current::close(), 14), 14),
        "stoch_rsi",
    );
    candle_case(
        Keltner::new(Current::close(), Current::candle(), 20, 10, 2.0),
        "keltner",
    );
    candle_case(
        Donchian::new(Current::high(), Current::low(), 20),
        "donchian",
    );
    // Cross-timeframe: `Resample` alone only emits on boundary ticks, so it
    // doesn't fit the always-ready-after-warm-up shape this battery asserts;
    // the [`Latch`] wrap converts it into a continuous-output source (holds
    // the last emitted value between boundaries) that does.
    candle_case(
        Latch::new(Resample::new(Current::candle(), 4).close()),
        "latched_resample_close",
    );

    // Calendar accessors: warm-up 1, emit only when `atom.time` is `Some`.
    // The `timed_candle_case` helper stamps each bar with a synthetic time so
    // these decompose deterministically.
    timed_candle_case(Year::new(), "year");
    timed_candle_case(Month::new(), "month");
    timed_candle_case(Day::new(), "day");
    timed_candle_case(Hour::new(), "hour");
    timed_candle_case(Minute::new(), "minute");
    timed_candle_case(Second::new(), "second");
    timed_candle_case(DayOfWeek::new(), "day_of_week");
    timed_candle_case(DayOfYear::new(), "day_of_year");
    timed_candle_case(WeekOfYear::new(), "week_of_year");
    timed_candle_case(Quarter::new(), "quarter");
    timed_candle_case(UnixSeconds::new(), "unix_seconds");
    timed_candle_case(UnixMillis::new(), "unix_millis");
    timed_candle_case(CurrentTime::new(), "current_time");
    timed_candle_case(IsWeekday::new(), "is_weekday");
    timed_candle_case(IsWeekend::new(), "is_weekend");
}

#[test]
fn warm_up_is_exact_for_composition() {
    // Chaining adds up: the EMA seeds on the SMA's first output.
    candle_case(
        Ema::new(Sma::new(Current::close(), 10), 20),
        "ema_of_sma",
    );
    // Components report the whole multi-output indicator's warm-up.
    candle_case(Macd::new(Current::close(), 12, 26, 9).line(), "macd_line");
    // Operators take the max of their operands; lookbacks add their period.
    candle_case(
        Sma::new(Current::close(), 5).sub(Ema::new(Current::close(), 20)),
        "sma_minus_ema",
    );
    candle_case(Current::close().lag(3), "lag");
    candle_case(Current::close().roc(5), "roc");
    candle_case(Current::close().rolling_max(10), "rolling_max");
    // NB: `IfElse` is deliberately not part of the exact-warm-up battery.
    // Its `warm_up_period()` reports the max of the three sources (safe
    // upper bound for downstream stability gates), but the actual first
    // `Some` can arrive earlier — as soon as the condition and the
    // *selected* branch are both settled. That's the natural semantics
    // (see the type-level doc) but breaks the "first Some at exactly
    // sample N" contract this battery asserts. `IfElse` is covered by
    // the unit tests in `src/indicators/if_else.rs`.
    //
    // The trailing risk indicators (`Sharpe` / `Sortino` / `Volatility` /
    // `MaxDrawdown` / `Calmar`) are excluded on the same footing: they own an
    // embedded strategy whose equity is flat (zero-variance → `None` for the
    // ratio metrics) until its own readiness gate elapses, so `warm_up_period()`
    // (= `period`) is a lower bound on the first `Some`, not exact — and they
    // need a `Strategy`, not the plain atom/ramp inputs this battery feeds.
    // Covered by the unit tests in `src/indicators/trailing.rs`.
    // Boolean layer: comparisons, edges and connectives.
    candle_case(Current::close().above(100.0), "above");
    candle_case(Current::close().above(100.0).changed(), "changed");
    candle_case(
        Current::close().crosses_above(Sma::new(Current::close(), 10)),
        "crosses_above",
    );
    candle_case(
        Current::close()
            .above(100.0)
            .and(Rsi::new(Current::close(), 14).below(70.0)),
        "and",
    );
    // Real-rooted chains behave identically.
    real_case(Identity::new(), "identity");
    real_case(Rsi::new(Identity::new(), 14), "rsi_of_identity");
    real_case(Identity::new().diff(4), "diff");
    assert_exact_warm_up(Value::<Real>::new(42.0), vec![1.0, 2.0, 3.0], "value");
}

/// The downstream `.max(1)` guard in every source-wrapping formula: a
/// `warm_up = 0` leaf (`Value`) fed into a windowed / recursive / lookback
/// indicator still requires the full number of `update` calls, not one fewer.
///
/// Previously the formulas read `source.warm_up + period - 1` verbatim, so
/// `Sma::new(Value(1.0), N)` reported `N − 1` instead of the true `N`. The
/// battery below pins the corrected formulas at N.
#[test]
fn warm_up_from_a_warm_up_zero_source_is_exact() {
    // Sanity: Value itself is ready without input.
    assert_eq!(Value::<Real>::new(1.0).warm_up_period(), 0);

    // Windowed / lookback: source.warm_up.max(1) + period − 1 (or + period).
    assert_eq!(Sma::new(Value::<Real>::new(1.0), 3).warm_up_period(), 3);
    assert_eq!(Wma::new(Value::<Real>::new(1.0), 3).warm_up_period(), 3);
    assert_eq!(StdDev::new(Value::<Real>::new(1.0), 3).warm_up_period(), 3);
    assert_eq!(Skewness::new(Value::<Real>::new(1.0), 3).warm_up_period(), 3);
    assert_eq!(Kurtosis::new(Value::<Real>::new(1.0), 3).warm_up_period(), 3);
    assert_eq!(ZScore::new(Value::<Real>::new(1.0), 3).warm_up_period(), 3);
    assert_eq!(
        Correlation::new(Value::<Real>::new(1.0), Value::<Real>::new(2.0), 3).warm_up_period(),
        3
    );
    assert_eq!(
        VarianceRatio::new(Value::<Real>::new(1.0), 4, 2).warm_up_period(),
        4
    );
    assert_eq!(
        Bollinger::new(Value::<Real>::new(1.0), 3, 2.0).warm_up_period(),
        3
    );
    assert_eq!(
        Stochastic::new(Value::<Real>::new(1.0), 3).warm_up_period(),
        3
    );
    // Cci sources a real-valued indicator; project the typical price via a
    // constant to exercise the same path.
    assert_eq!(Cci::new(Value::<Real>::new(1.0), 3).warm_up_period(), 3);
    assert_eq!(Value::<Real>::new(1.0).rolling_max(3).warm_up_period(), 3);
    assert_eq!(Value::<Real>::new(1.0).rolling_min(3).warm_up_period(), 3);
    assert_eq!(Value::<Real>::new(1.0).lag(3).warm_up_period(), 4);
    assert_eq!(Value::<Real>::new(1.0).diff(3).warm_up_period(), 4);

    // Recursive: source.warm_up.max(1) [+ …]. Ema/Macd need one update to
    // seed; Rma needs a full period; Rsi one seed + a full period of deltas.
    assert_eq!(Ema::new(Value::<Real>::new(1.0), 3).warm_up_period(), 1);
    assert_eq!(Rma::new(Value::<Real>::new(1.0), 3).warm_up_period(), 3);
    assert_eq!(Rsi::new(Value::<Real>::new(1.0), 3).warm_up_period(), 4);
}

#[test]
fn windowed_indicators_are_stable_once_ready() {
    assert_eq!(Sma::new(Current::close(), 20).unstable_period(), 0);
    assert_eq!(Wma::new(Current::close(), 20).unstable_period(), 0);
    assert_eq!(Skewness::new(Current::close(), 20).unstable_period(), 0);
    assert_eq!(Kurtosis::new(Current::close(), 20).unstable_period(), 0);
    assert_eq!(ZScore::new(Current::close(), 20).unstable_period(), 0);
    assert_eq!(
        Correlation::new(Current::close(), Current::open(), 20).unstable_period(),
        0
    );
    assert_eq!(
        VarianceRatio::new(Current::close(), 20, 2).unstable_period(),
        0
    );
    assert_eq!(Bollinger::new(Current::close(), 20, 2.0).unstable_period(), 0);
    assert_eq!(Stochastic::new(Current::close(), 14).unstable_period(), 0);
    assert_eq!(Aroon::new(Current::candle(), 25).unstable_period(), 0);
    assert_eq!(WilliamsR::new(Current::candle(), 14).unstable_period(), 0);
    assert_eq!(Mfi::new(Current::candle(), 14).unstable_period(), 0);
    assert_eq!(Parkinson::new(Current::candle(), 20).unstable_period(), 0);
    assert_eq!(GarmanKlass::new(Current::candle(), 20).unstable_period(), 0);
    assert_eq!(
        RogersSatchell::new(Current::candle(), 20).unstable_period(),
        0
    );
    assert_eq!(Current::close().rolling_max(10).unstable_period(), 0);
    assert_eq!(
        Donchian::new(Current::high(), Current::low(), 20).unstable_period(),
        0
    );
}

#[test]
fn recursive_indicators_report_their_settling() {
    // EMA period 3 has alpha 0.5: 0.5^10 is the first power below 0.1%.
    assert_eq!(Ema::new(Identity::new(), 3).unstable_period(), 10);
    // Wilder period 14 decays by 13/14 per sample: settles at 94.
    assert_eq!(Rma::new(Identity::new(), 14).unstable_period(), 94);
    assert_eq!(Rsi::new(Identity::new(), 14).unstable_period(), 94);
    assert_eq!(Atr::new(Current::candle(), 14).unstable_period(), 94);
    // ADX stacks a second Wilder pass on the DI lines.
    assert_eq!(Adx::new(Current::candle(), 14).unstable_period(), 188);
    // Instability propagates through composition and operators.
    let sig = Current::close().crosses_above(Ema::new(Current::close(), 20));
    assert_eq!(
        sig.unstable_period(),
        Ema::new(Current::close(), 20).unstable_period()
    );
    assert_eq!(
        Sma::new(Current::close(), 5).unstable_period(),
        0,
        "windowed stays exact"
    );
    let stacked = Sma::new(Ema::new(Current::close(), 20), 5);
    assert_eq!(
        stacked.unstable_period(),
        Ema::new(Current::close(), 20).unstable_period()
    );
}

/// Replaying only the last `stable_period()` samples reproduces the
/// full-history output to within the documented 0.1% seed weight.
#[test]
fn stable_period_bounds_the_seeding_error() {
    let history = bars(500);

    fn converged_output<I>(mut full: I, mut tail_only: I, history: &[Candle]) -> (Real, Real)
    where
        I: Indicator<Input = Atom, Output = Real>,
    {
        let tail = history.len() - tail_only.stable_period();
        for (i, bar) in history.iter().enumerate() {
            full.update((*bar).into());
            if i >= tail {
                tail_only.update((*bar).into());
            }
        }
        (full.value().unwrap(), tail_only.value().unwrap())
    }

    let (a, b) = converged_output(
        Ema::new(Current::close(), 20),
        Ema::new(Current::close(), 20),
        &history,
    );
    assert!(
        (a - b).abs() <= 1e-3 * a.abs(),
        "EMA seeding residual too large: {a} vs {b}"
    );

    let (a, b) = converged_output(
        Rsi::new(Current::close(), 14),
        Rsi::new(Current::close(), 14),
        &history,
    );
    assert!(
        (a - b).abs() <= 0.5,
        "RSI seeding residual too large: {a} vs {b}"
    );
}
