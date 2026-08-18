//! Hand-derived reference values for the indicator catalogue.
//!
//! # Why this exists
//!
//! `tests/talib_validation.rs` is the crate's declared indicator drift guard,
//! but it consumes a *generated* fixture that isn't committed, so on a checkout
//! without TA-Lib it skips — and a skip is indistinguishable from a pass. The
//! numbers were therefore unguarded in CI.
//!
//! This battery holds that line unconditionally. Every expected value below is
//! **derived by hand from the indicator's own definition** and shown in the
//! comment above it, over inputs short enough to check by eye. It is not a
//! snapshot of what fugazi currently prints: a golden-master recorded from the
//! implementation would agree with any bug the implementation already has.
//!
//! # What belongs here, and what doesn't
//!
//! In: closed-form math whose value on a five-bar input is a fact about the
//! *definition* — moving averages, dispersion, ranges, cumulative flows.
//!
//! Out: anything whose reference is only defensible against another
//! implementation (TA-Lib's exact ADX seeding, empyrical's Sortino convention).
//! Those stay in the cross-validation suites, where the reference is named.
//! Also out: warm-up boundaries, which `tests/warm_up.rs` already asserts
//! exactly for the whole catalogue — this file pins *values*, and states a
//! warm-up only as the `None` prefix of the series it is checking.

use fugazi::indicators::{
    Ad, Aroon, Bollinger, Cci, Current, Ema, Exp, Identity, Log, Mfi, Obv, Percentile, RollingMax,
    RollingMin, Rma, Rsi, Sma, StdDev, Stochastic, TrueRange, Value, WilliamsR, Wma,
};
use fugazi::prelude::*;

/// Tolerance for closed-form arithmetic: two evaluations of the same formula in
/// f64 differ only in the last couple of ulps.
const TOL: Real = 1e-12;

/// Drive `ind` over `inputs` and collect one output per sample.
fn run<I>(mut ind: I, inputs: Vec<I::Input>) -> Vec<Option<Real>>
where
    I: Indicator<Output = Real>,
{
    inputs.into_iter().map(|x| ind.update(x)).collect()
}

/// Assert an output series matches `want` element for element, `None` included.
#[track_caller]
fn assert_series(got: &[Option<Real>], want: &[Option<Real>], name: &str) {
    assert_eq!(
        got.len(),
        want.len(),
        "{name}: length {} vs expected {}",
        got.len(),
        want.len()
    );
    for (i, (g, w)) in got.iter().zip(want).enumerate() {
        match (g, w) {
            (None, None) => {}
            (Some(g), Some(w)) => assert!(
                (g - w).abs() <= TOL * g.abs().max(w.abs()).max(1.0),
                "{name}[{i}]: got {g}, want {w}"
            ),
            _ => panic!("{name}[{i}]: got {g:?}, want {w:?}"),
        }
    }
}

/// `Some(x)` for every element — the common all-warm tail.
fn some(xs: &[Real]) -> Vec<Option<Real>> {
    xs.iter().copied().map(Some).collect()
}

/// `n` leading `None`s then `xs`.
fn warm(n: usize, xs: &[Real]) -> Vec<Option<Real>> {
    let mut out = vec![None; n];
    out.extend(some(xs));
    out
}

/// A flat-OHLC bar, so a `Real`-source indicator and a candle-source one see
/// the same number.
fn flat(px: Real) -> Atom {
    Candle::new(px, px, px, px, 1.0).into()
}

const RAMP: [Real; 5] = [1.0, 2.0, 3.0, 4.0, 5.0];

// ---------------------------------------------------------------------------
// Moving averages
// ---------------------------------------------------------------------------

/// SMA(3) of `1..5` is the mean of each trailing triple: `(1+2+3)/3 = 2`,
/// `(2+3+4)/3 = 3`, `(3+4+5)/3 = 4`.
#[test]
fn sma_is_the_mean_of_its_window() {
    let got = run(Sma::new(Identity::new(), 3), RAMP.to_vec());
    assert_series(&got, &warm(2, &[2.0, 3.0, 4.0]), "sma3");
}

/// WMA(3) weights the window `1, 2, 3` oldest→newest over a divisor of
/// `3·4/2 = 6`:
///   bar 3 → `(1·1 + 2·2 + 3·3)/6 = 14/6`
///   bar 4 → `(1·2 + 2·3 + 3·4)/6 = 20/6`
///   bar 5 → `(1·3 + 2·4 + 3·5)/6 = 26/6`
#[test]
fn wma_weights_the_window_linearly() {
    let got = run(Wma::new(Identity::new(), 3), RAMP.to_vec());
    assert_series(
        &got,
        &warm(2, &[14.0 / 6.0, 20.0 / 6.0, 26.0 / 6.0]),
        "wma3",
    );
}

/// EMA(3) has `α = 2/(3+1) = 0.5` and **seeds on the first sample** (fugazi's
/// convention; TA-Lib seeds with an SMA, which is why the cross-check only
/// compares the converged tail). So `e₁ = 1`, and `eₙ = 0.5·xₙ + 0.5·eₙ₋₁`:
///   `1, 1.5, 2.25, 3.125, 4.0625`.
#[test]
fn ema_seeds_on_the_first_sample_and_halves_the_residual() {
    let got = run(Ema::new(Identity::new(), 3), RAMP.to_vec());
    assert_series(&got, &some(&[1.0, 1.5, 2.25, 3.125, 4.0625]), "ema3");
}

/// Wilder's RMA(3) seeds with the **mean of the first three samples**, then
/// `rₙ = (rₙ₋₁·2 + xₙ)/3`:
///   bar 3 → `(1+2+3)/3 = 2`
///   bar 4 → `(2·2 + 4)/3 = 8/3`
///   bar 5 → `(8/3·2 + 5)/3 = 31/9`
#[test]
fn rma_seeds_on_the_window_mean_then_smooths_by_wilder() {
    let got = run(Rma::new(Identity::new(), 3), RAMP.to_vec());
    assert_series(&got, &warm(2, &[2.0, 8.0 / 3.0, 31.0 / 9.0]), "rma3");
}

// ---------------------------------------------------------------------------
// Dispersion
// ---------------------------------------------------------------------------

/// `[2, 4, 4, 6]` has mean `4` and **population** variance
/// `(4 + 0 + 0 + 4)/4 = 2`, so the standard deviation is `√2`. (Population, not
/// sample — `StdDev`/`Bollinger` use the `n` divisor; the sample form is
/// reserved for the metrics layer.)
#[test]
fn stddev_is_the_population_deviation_of_its_window() {
    let got = run(StdDev::new(Identity::new(), 4), vec![2.0, 4.0, 4.0, 6.0]);
    assert_series(&got, &warm(3, &[2.0_f64.sqrt()]), "stddev4");
}

/// Bollinger(4, k=2) over the same window: middle `4`, bands `4 ± 2√2`.
#[test]
fn bollinger_bands_sit_k_deviations_off_the_middle() {
    let mut bb = Bollinger::new(Identity::new(), 4, 2.0);
    let mut last = None;
    for x in [2.0, 4.0, 4.0, 6.0] {
        last = bb.update(x);
    }
    let v = last.expect("full window");
    let sd = 2.0_f64.sqrt();
    assert_series(&[Some(v.middle)], &some(&[4.0]), "bb_middle");
    assert_series(&[Some(v.upper)], &some(&[4.0 + 2.0 * sd]), "bb_upper");
    assert_series(&[Some(v.lower)], &some(&[4.0 - 2.0 * sd]), "bb_lower");
}

/// CCI(3) is `(x − mean) / (0.015 · mean_abs_dev)`. Over `[1, 2, 6]` the mean is
/// `3` and the MAD is `(2 + 1 + 3)/3 = 2`, so the last bar reads
/// `(6 − 3)/(0.015·2) = 100`.
#[test]
fn cci_scales_the_deviation_by_the_mean_absolute_deviation() {
    let got = run(Cci::new(Identity::new(), 3), vec![1.0, 2.0, 6.0]);
    assert_series(&got, &warm(2, &[100.0]), "cci3");
}

// ---------------------------------------------------------------------------
// Ranges and order statistics
// ---------------------------------------------------------------------------

const SPIKY: [Real; 8] = [3.0, 1.0, 4.0, 1.0, 5.0, 9.0, 2.0, 6.0];

/// The rolling extrema of `[3,1,4,1,5,9,2,6]` over a 3-window, read off the
/// trailing triples: `(3,1,4) (1,4,1) (4,1,5) (1,5,9) (5,9,2) (9,2,6)`.
#[test]
fn rolling_extrema_track_their_window() {
    assert_series(
        &run(RollingMax::new(Identity::new(), 3), SPIKY.to_vec()),
        &warm(2, &[4.0, 4.0, 5.0, 9.0, 9.0, 9.0]),
        "rolling_max3",
    );
    assert_series(
        &run(RollingMin::new(Identity::new(), 3), SPIKY.to_vec()),
        &warm(2, &[1.0, 1.0, 1.0, 1.0, 2.0, 2.0]),
        "rolling_min3",
    );
}

/// `!exp` is `base^x` sample by sample, and the inverse of `!log`. Base 2 over
/// the ramp `[1,2,3,4,5]` is `[2,4,8,16,32]` — every value a power of two, so
/// f64 represents each exactly. Round-tripping the same ramp through
/// `exp(ln(x))` returns it unchanged.
#[test]
fn exp_raises_its_base_to_the_sample() {
    let got = run(Exp::new(Identity::new(), 2.0), RAMP.to_vec());
    assert_series(&got, &some(&[2.0, 4.0, 8.0, 16.0, 32.0]), "exp2");

    let round_trip = run(Exp::natural(Log::natural(Identity::new())), RAMP.to_vec());
    assert_series(&round_trip, &some(&RAMP), "exp_of_log");
}

/// The result of `base^x` can leave the finite range where the input cannot:
/// `e^1000` overflows, and an unrepresentable answer is reported as no answer
/// rather than as `inf`. Underflow is not the same case — `e^-1000` is `0.0`,
/// which is representable, so it is a value.
#[test]
fn exp_reports_an_unrepresentable_result_as_none() {
    let got = run(Exp::natural(Identity::new()), vec![1000.0, -1000.0, 0.0]);
    assert_series(&got, &[None, Some(0.0), Some(1.0)], "exp_overflow");
}

/// The stochastic places the newest sample inside its window's range,
/// `(x − min)/(max − min)`, in `[0, 1]`. Over `[3,1,4,1,5]` with a 3-window:
///   bar 3 → window `(3,1,4)`, x=4 → `(4−1)/3 = 1`
///   bar 4 → window `(1,4,1)`, x=1 → `(1−1)/3 = 0`
///   bar 5 → window `(4,1,5)`, x=5 → `(5−1)/4 = 1`
#[test]
fn stochastic_positions_the_sample_within_its_range() {
    let got = run(
        Stochastic::new(Identity::new(), 3),
        vec![3.0, 1.0, 4.0, 1.0, 5.0],
    );
    assert_series(&got, &warm(2, &[1.0, 0.0, 1.0]), "stochastic3");
}

/// A flat window has no range, and the documented degenerate answer is `0.0`
/// rather than a division by zero.
#[test]
fn stochastic_of_a_flat_window_is_zero_not_nan() {
    let got = run(Stochastic::new(Identity::new(), 3), vec![7.0; 4]);
    assert_series(&got, &warm(2, &[0.0, 0.0]), "stochastic_flat");
}

/// The crate's single quantile convention is R type-7 (numpy's default):
/// `idx = p·(n−1)`, linearly interpolated between the bracketing order
/// statistics. Over the window `[1,2,3,4]`:
///   `p = 0.25` → `idx = 0.75` → `1·0.25 + 2·0.75 = 1.75`
///   `p = 0.50` → `idx = 1.5`  → `2·0.5  + 3·0.5  = 2.5`
///   `p = 1.00` → the maximum, `4`
///
/// Pinning it here matters because `metrics`' VaR/CVaR/tail-ratio and the
/// rolling `!percentile` tag share this one function — a change of convention
/// would silently move both.
#[test]
fn percentile_follows_the_r_type_7_convention() {
    let window = vec![4.0, 1.0, 3.0, 2.0]; // arrival order; the quantile sorts
    for (p, want) in [(0.25, 1.75), (0.5, 2.5), (1.0, 4.0)] {
        let got = run(Percentile::new(Identity::new(), 4, p), window.clone());
        assert_series(&got, &warm(3, &[want]), &format!("percentile(p={p})"));
    }
}

// ---------------------------------------------------------------------------
// Bar indicators
// ---------------------------------------------------------------------------

/// True range is `max(h−l, |h−prevClose|, |l−prevClose|)`, and on the very
/// first bar — where there is no previous close — just `h−l`:
///   bar 1 `(h12 l8)`                → `12−8 = 4`
///   bar 2 `(h15 l13, prevClose 11)` → `max(2, 4, 2) = 4`
///   bar 3 `(h14 l6,  prevClose 14)` → `max(8, 0, 8) = 8`
#[test]
fn true_range_takes_the_widest_of_the_three_spans() {
    let bars = vec![
        Candle::new(10.0, 12.0, 8.0, 11.0, 1.0).into(),
        Candle::new(11.0, 15.0, 13.0, 14.0, 1.0).into(),
        Candle::new(14.0, 14.0, 6.0, 7.0, 1.0).into(),
    ];
    let got = run(TrueRange::new(Current::candle()), bars);
    assert_series(&got, &some(&[4.0, 4.0, 8.0]), "true_range");
}

/// Williams %R mirrors the stochastic onto `[-100, 0]`:
/// `−100·(highestHigh − close)/(highestHigh − lowestLow)`. Over three bars with
/// highs `(10,12,11)` and lows `(5,6,7)`, the last close of `8` reads
/// `−100·(12−8)/(12−5) = −400/7`.
#[test]
fn williams_r_mirrors_the_stochastic_onto_a_negative_scale() {
    let bars = vec![
        Candle::new(9.0, 10.0, 5.0, 9.0, 1.0).into(),
        Candle::new(9.0, 12.0, 6.0, 11.0, 1.0).into(),
        Candle::new(11.0, 11.0, 7.0, 8.0, 1.0).into(),
    ];
    let got = run(WilliamsR::new(Current::candle(), 3), bars);
    assert_series(&got, &warm(2, &[-400.0 / 7.0]), "williams_r3");
}

/// OBV seeds at the first bar's own volume, then adds on an up-close,
/// subtracts on a down-close, and holds on a flat one. Closes
/// `10 → 11 → 11 → 9` with volumes `100, 200, 300, 400` give
/// `100, 300, 300, −100`.
#[test]
fn obv_accumulates_volume_by_the_sign_of_the_close() {
    let bars: Vec<Atom> = [(10.0, 100.0), (11.0, 200.0), (11.0, 300.0), (9.0, 400.0)]
        .into_iter()
        .map(|(c, v)| Candle::new(c, c, c, c, v).into())
        .collect();
    let got = run(Obv::new(Current::candle()), bars);
    assert_series(&got, &some(&[100.0, 300.0, 300.0, -100.0]), "obv");
}

/// The A/D line accumulates `volume · closeLocationValue`, where CLV is
/// `((c−l) − (h−c))/(h−l)` in `[-1, 1]`. Two bars spanning `[0, 10]` with
/// volume `100`, closing at `8` then `2`:
///   CLV `(8−2)/10 = +0.6` → `+60`
///   CLV `(2−8)/10 = −0.6` → back to `0`
/// A third bar with no range (`h == l`) contributes nothing.
#[test]
fn ad_accumulates_volume_by_close_location_and_skips_rangeless_bars() {
    let bars = vec![
        Candle::new(5.0, 10.0, 0.0, 8.0, 100.0).into(),
        Candle::new(5.0, 10.0, 0.0, 2.0, 100.0).into(),
        Candle::new(4.0, 4.0, 4.0, 4.0, 100.0).into(),
    ];
    let got = run(Ad::new(Current::candle()), bars);
    assert_series(&got, &some(&[60.0, 0.0, 0.0]), "ad");
}

/// MFI(2) is a volume-weighted RSI over the typical price. With flat bars at
/// `10 → 11 → 10`, all volume `100`:
///   bar 1 seeds the previous typical price (no flow yet)
///   bar 2 typical rises → positive flow `11·100 = 1100`
///   bar 3 typical falls → negative flow `10·100 = 1000`
/// The 2-window is now full, so
/// `MFI = 100 − 100/(1 + 1100/1000) = 100 − 100/2.1`.
#[test]
fn mfi_is_a_volume_weighted_rsi_of_the_typical_price() {
    let bars: Vec<Atom> = [10.0, 11.0, 10.0]
        .into_iter()
        .map(|c| Candle::new(c, c, c, c, 100.0).into())
        .collect();
    let got = run(Mfi::new(Current::candle(), 2), bars);
    assert_series(&got, &warm(2, &[100.0 - 100.0 / 2.1]), "mfi2");
}

/// RSI(3) Wilder-seeds both averages on the first three deltas. Over
/// `10 → 11 → 10 → 11` the deltas are `+1, −1, +1`, so
/// `avgGain = 2/3`, `avgLoss = 1/3`, `RS = 2`, and
/// `RSI = 100 − 100/(1 + 2) = 200/3`.
#[test]
fn rsi_seeds_both_wilder_averages_on_the_first_deltas() {
    let got = run(
        Rsi::new(Identity::new(), 3),
        vec![10.0, 11.0, 10.0, 11.0],
    );
    assert_series(&got, &warm(3, &[200.0 / 3.0]), "rsi3");
}

/// A source that only ever rises has no losing delta, so RSI pins at `100`
/// rather than dividing by a zero average loss.
#[test]
fn rsi_of_a_monotonic_rise_pins_at_one_hundred() {
    let got = run(Rsi::new(Identity::new(), 3), RAMP.to_vec());
    assert_series(&got, &warm(3, &[100.0, 100.0]), "rsi_monotonic");
}

/// Aroon(3) reports how recently the window's extreme occurred, as
/// `100·(period − barsSince)/period`. Its lookback spans `period + 1` bars —
/// the current one plus the `period` before it — so it needs four bars, and a
/// brand-new extreme reads `barsSince == 0`.
///
/// Highs `(3, 5, 4, 2)` put the highest high 2 bars back → `up = 100·(3−2)/3`;
/// lows `(2, 3, 2, 1)` put the lowest low on the current bar →
/// `down = 100·(3−0)/3 = 100`. The oscillator is their difference.
#[test]
fn aroon_measures_how_recently_the_extreme_occurred() {
    let bars = vec![
        Candle::new(2.5, 3.0, 2.0, 2.5, 1.0).into(),
        Candle::new(4.0, 5.0, 3.0, 4.0, 1.0).into(),
        Candle::new(3.0, 4.0, 2.0, 3.0, 1.0).into(),
        Candle::new(1.5, 2.0, 1.0, 1.5, 1.0).into(),
    ];
    let mut aroon = Aroon::new(Current::candle(), 3);
    let mut last = None;
    for b in bars {
        last = aroon.update(b);
    }
    let v = last.expect("full window");
    assert_series(&[Some(v.up)], &some(&[100.0 / 3.0]), "aroon_up");
    assert_series(&[Some(v.down)], &some(&[100.0]), "aroon_down");
    assert_series(
        &[Some(v.oscillator)],
        &some(&[100.0 / 3.0 - 100.0]),
        "aroon_oscillator",
    );
}

// ---------------------------------------------------------------------------
// Operators
// ---------------------------------------------------------------------------

/// `roc(n)` is a **percentage** rate of change, `100·(x − x₋ₙ)/x₋ₙ` — the
/// TA-Lib convention, not a natural-units ratio. Over `1..5` with `n = 2`:
///   bar 3 → `100·(3−1)/1 = 200`
///   bar 4 → `100·(4−2)/2 = 100`
///   bar 5 → `100·(5−3)/3 = 200/3`
#[test]
fn roc_is_a_percentage_not_a_ratio() {
    let got = run(Identity::new().roc(2), RAMP.to_vec());
    assert_series(&got, &warm(2, &[200.0, 100.0, 200.0 / 3.0]), "roc2");
}

/// `lag(n)` reproduces the value `n` samples back, and `diff(n)` their
/// difference. Both need `n + 1` samples, so `1..5` with `n = 2` yields three
/// readings.
#[test]
fn lag_and_diff_reach_exactly_n_samples_back() {
    assert_series(
        &run(Identity::new().lag(2), RAMP.to_vec()),
        &warm(2, &[1.0, 2.0, 3.0]),
        "lag2",
    );
    assert_series(
        &run(Identity::new().diff(2), RAMP.to_vec()),
        &warm(2, &[2.0, 2.0, 2.0]),
        "diff2",
    );
}

/// Division by zero yields `None` rather than an infinity that would poison
/// every downstream comparison.
#[test]
fn division_by_zero_is_none_not_infinity() {
    let mut div = Identity::new().div(Value::new(0.0));
    assert_eq!(div.update(1.0), None);
}

// ---------------------------------------------------------------------------
// Composition
// ---------------------------------------------------------------------------

/// Composition is construction, and the numbers have to compose too: an SMA(3)
/// of an EMA(3) must equal the SMA(3) of the EMA series computed above
/// (`1, 1.5, 2.25, 3.125, 4.0625`) — first triple mean `(1+1.5+2.25)/3`, then
/// `(1.5+2.25+3.125)/3`, then `(2.25+3.125+4.0625)/3`.
#[test]
fn a_nested_chain_composes_its_operands_numerically() {
    let got = run(Sma::new(Ema::new(Identity::new(), 3), 3), RAMP.to_vec());
    assert_series(
        &got,
        &warm(
            2,
            &[
                (1.0 + 1.5 + 2.25) / 3.0,
                (1.5 + 2.25 + 3.125) / 3.0,
                (2.25 + 3.125 + 4.0625) / 3.0,
            ],
        ),
        "sma3_of_ema3",
    );
}

/// A candle-rooted chain and a `Real`-rooted one must agree when the bars are
/// flat, since `Current::close()` then hands the same stream `Identity` sees.
/// Guards the leaf plumbing rather than the arithmetic.
#[test]
fn candle_rooted_and_real_rooted_chains_agree_on_flat_bars() {
    let by_close = run(
        Sma::new(Current::close(), 3),
        RAMP.iter().copied().map(flat).collect(),
    );
    let by_identity = run(Sma::new(Identity::new(), 3), RAMP.to_vec());
    assert_series(&by_close, &by_identity, "sma3 close vs identity");
}

/// `reset()` must return an indicator to its constructed state — not merely
/// clear the output — so a replay reproduces the original series exactly.
#[test]
fn reset_replays_every_stateful_shape_identically() {
    fn replays<I: Indicator<Output = Real> + Clone>(ind: I, inputs: Vec<I::Input>, name: &str)
    where
        I::Input: Clone,
    {
        let first = run(ind.clone(), inputs.clone());
        let mut warmed = ind;
        for x in inputs.clone() {
            warmed.update(x);
        }
        warmed.reset();
        let second = run(warmed, inputs);
        assert_series(&second, &first, name);
    }

    // One per state shape: window (Sma), IIR seed (Ema), Wilder seed (Rma),
    // monotonic deque (RollingMax), sorted view (Percentile), two Wilder
    // states (Rsi).
    replays(Sma::new(Identity::new(), 3), SPIKY.to_vec(), "sma");
    replays(Ema::new(Identity::new(), 3), SPIKY.to_vec(), "ema");
    replays(Rma::new(Identity::new(), 3), SPIKY.to_vec(), "rma");
    replays(RollingMax::new(Identity::new(), 3), SPIKY.to_vec(), "rolling_max");
    replays(Percentile::new(Identity::new(), 3, 0.5), SPIKY.to_vec(), "percentile");
    replays(Rsi::new(Identity::new(), 3), SPIKY.to_vec(), "rsi");
    replays(
        Obv::new(Current::candle()),
        SPIKY.iter().copied().map(flat).collect(),
        "obv",
    );
}
