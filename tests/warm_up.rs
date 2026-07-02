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
    Adx, Aroon, Atr, Bollinger, Cci, Current, Dmi, Donchian, Ema, Hma, Identity, Keltner, Macd,
    Mfi, Obv, Rma, Rsi, Sar, Sma, StdDev, Stochastic, TrueRange, Value, Vwap, WilliamsR, Wma,
};
use fugazi::prelude::*;
use fugazi::types::{Candle, Real};

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

fn candle_case(ind: impl Indicator<Input = Candle>, name: &str) {
    let n = ind.warm_up_period() + 5;
    assert_exact_warm_up(ind, bars(n), name);
}

fn real_case(ind: impl Indicator<Input = Real>, name: &str) {
    let n = ind.warm_up_period() + 5;
    let series = bars(n).iter().map(|b| b.close).collect();
    assert_exact_warm_up(ind, series, name);
}

#[test]
fn warm_up_is_exact_for_the_catalogue() {
    candle_case(Current::close(), "close");
    candle_case(TrueRange::new(), "true_range");
    candle_case(Obv::new(), "obv");
    candle_case(fugazi::indicators::Ad::new(), "ad");
    candle_case(Vwap::new(), "vwap");
    candle_case(Sar::default(), "sar");
    candle_case(Atr::new(14), "atr");
    candle_case(Mfi::new(14), "mfi");
    candle_case(Dmi::new(14), "dmi");
    candle_case(Adx::new(14), "adx");
    candle_case(Aroon::new(25), "aroon");
    candle_case(WilliamsR::new(14), "williams_r");
    candle_case(Sma::new(Current::close(), 20), "sma");
    candle_case(Ema::new(Current::close(), 20), "ema");
    candle_case(Rma::new(Current::close(), 14), "rma");
    candle_case(Wma::new(Current::close(), 20), "wma");
    candle_case(Hma::new(Current::close(), 16), "hma");
    candle_case(Rsi::new(Current::close(), 14), "rsi");
    candle_case(Macd::new(Current::close(), 12, 26, 9), "macd");
    candle_case(StdDev::new(Current::close(), 20), "stddev");
    candle_case(Cci::new(Current::typical(), 20), "cci");
    candle_case(Bollinger::new(Current::close(), 20, 2.0), "bollinger");
    candle_case(Stochastic::new(Current::close(), 14), "stochastic");
    candle_case(
        Stochastic::new(Rsi::new(Current::close(), 14), 14),
        "stoch_rsi",
    );
    candle_case(Keltner::new(Current::close(), 20, 10, 2.0), "keltner");
    candle_case(
        Donchian::new(Current::high(), Current::low(), 20),
        "donchian",
    );
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

#[test]
fn windowed_indicators_are_stable_once_ready() {
    assert_eq!(Sma::new(Current::close(), 20).unstable_period(), 0);
    assert_eq!(Wma::new(Current::close(), 20).unstable_period(), 0);
    assert_eq!(Bollinger::new(Current::close(), 20, 2.0).unstable_period(), 0);
    assert_eq!(Stochastic::new(Current::close(), 14).unstable_period(), 0);
    assert_eq!(Aroon::new(25).unstable_period(), 0);
    assert_eq!(WilliamsR::new(14).unstable_period(), 0);
    assert_eq!(Mfi::new(14).unstable_period(), 0);
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
    assert_eq!(Atr::new(14).unstable_period(), 94);
    // ADX stacks a second Wilder pass on the DI lines.
    assert_eq!(Adx::new(14).unstable_period(), 188);
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
        I: Indicator<Input = Candle, Output = Real>,
    {
        let tail = history.len() - tail_only.stable_period();
        for (i, bar) in history.iter().enumerate() {
            full.update(*bar);
            if i >= tail {
                tail_only.update(*bar);
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
