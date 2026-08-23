//! Degenerate bars must produce `None` or a finite number — never a `NaN`.
//!
//! `tests/warm_up.rs` walks the catalogue over *well-formed* data and pins when
//! the first reading arrives. This walks the same catalogue over the shapes real
//! feeds actually contain and pins what a reading may **be**: a flat session, a
//! zero-volume bar, a zero-range bar (a limit-locked or thinly-quoted print), an
//! all-zero row from a broken loader, and a single discontinuous gap.
//!
//! The assertion is deliberately weak on *value* and strict on *kind*. What a
//! zero-range window's Williams %R should read is a modelling question; that it
//! must not be a `NaN` is not. A `NaN` is the one output that cannot be caught
//! downstream: it compares `false` against every threshold, so a signal built on
//! it reads "no" forever, and `Combine` has no `is_finite` guard to turn it back
//! into the `None` the rest of the crate uses for "no reading". The crate's own
//! rule (`ops::UnaryOp`, `DivOp`, `Moments::correlation`) is that an undefined
//! answer is `None` or a documented degradation — this is that rule, swept.

use fugazi::indicators::{
    Abs, Ad, Adx, Aroon, Atr, BarsSinceHigh, BarsSinceLow, Beta, Bollinger, Cci, Correlation,
    Covariance, CumMax, CumMin, CumSum, Current, Donchian, Ema, Exp, GarmanKlass, Hma, Keltner,
    Kurtosis, Latch, LinReg, Log, Macd, Mfi, Obv, Parkinson, Percentile, PercentileRank, Resample,
    Rma, RogersSatchell, Rsi, Sar, Sigmoid, Sign, Skewness, Sma, Sqrt, StdDev, Stochastic, Tanh,
    TrueRange, Value, VarianceRatio, Vwap, WilliamsR, Wma, ZScore,
};
use fugazi::prelude::*;
use fugazi::types::{Atom, Candle, Real};

const N: usize = 80;

/// Every bar identical — a halted or untraded session repeated.
fn flat() -> Vec<Candle> {
    vec![Candle::new(100.0, 100.0, 100.0, 100.0, 1000.0); N]
}

/// Well-formed prices, no volume at all. Every volume-weighted indicator
/// divides by this.
fn zero_volume() -> Vec<Candle> {
    (0..N)
        .map(|i| {
            let base = 100.0 + (i as Real * 0.7).sin() * 5.0;
            Candle::new(base, base + 1.0, base - 1.0, base + 0.2, 0.0)
        })
        .collect()
}

/// Zero-range bars that still move between bars — a limit-locked or
/// single-print series. `high - low` is zero, which is the denominator of
/// Stochastic, Williams %R and every range-based volatility estimator.
fn zero_range() -> Vec<Candle> {
    (0..N)
        .map(|i| {
            let p = 100.0 + (i as Real * 0.7).sin() * 5.0;
            Candle::new(p, p, p, p, 1000.0 + i as Real)
        })
        .collect()
}

/// An all-zero row, the shape a broken loader or a missing column produces.
fn all_zero() -> Vec<Candle> {
    vec![Candle::new(0.0, 0.0, 0.0, 0.0, 0.0); N]
}

/// One discontinuous gap in an otherwise ordinary series — the shape a stock
/// split or a bad tick leaves behind.
fn gapped() -> Vec<Candle> {
    (0..N)
        .map(|i| {
            let base = if i < N / 2 { 100.0 } else { 1e6 };
            let w = (i as Real * 0.3).sin();
            Candle::new(
                base + w,
                base + 1.0 + w,
                base - 1.0 + w,
                base + 0.5 * w,
                1000.0,
            )
        })
        .collect()
}

fn series() -> Vec<(&'static str, Vec<Candle>)> {
    vec![
        ("flat", flat()),
        ("zero volume", zero_volume()),
        ("zero range", zero_range()),
        ("all zero", all_zero()),
        ("gapped", gapped()),
    ]
}

/// Drive `build()`'s indicator over each degenerate series and assert every
/// emitted reading is finite.
fn sweep<I, B>(name: &str, build: B)
where
    I: Indicator<Input = Atom, Output = Real>,
    B: Fn() -> I,
{
    for (shape, bars) in series() {
        let mut ind = build();
        for (i, c) in bars.into_iter().enumerate() {
            if let Some(v) = ind.update(Atom::from(c)) {
                assert!(
                    v.is_finite(),
                    "{name} on a {shape} series produced {v} at bar {i} — an \
                     undefined reading must be `None`, not a NaN or an infinity"
                );
            }
        }
    }
}

#[test]
fn no_indicator_emits_a_non_finite_reading_on_degenerate_bars() {
    sweep("close", Current::close);
    sweep("log", || Log::natural(Current::close()));
    sweep("exp", || Exp::natural(Current::close()));
    sweep("abs", || Abs::new(Current::close()));
    sweep("sign", || Sign::new(Current::close()));
    sweep("sqrt", || Sqrt::new(Current::close()));
    sweep("tanh", || Tanh::new(Current::close()));
    sweep("sigmoid", || Sigmoid::new(Current::close()));
    sweep("true_range", || TrueRange::new(Current::candle()));
    sweep("obv", || Obv::new(Current::candle()));
    sweep("ad", || Ad::new(Current::candle()));
    sweep("cum_sum", || CumSum::new(Current::close()));
    sweep("cum_max", || CumMax::new(Current::close()));
    sweep("cum_min", || CumMin::new(Current::close()));
    sweep("vwap", || Vwap::new(Current::candle(), 20));
    sweep("sar", || Sar::with_defaults(Current::candle()));
    sweep("atr", || Atr::new(Current::candle(), 14));
    sweep("parkinson", || Parkinson::new(Current::candle(), 20));
    sweep("garman_klass", || GarmanKlass::new(Current::candle(), 20));
    sweep("rogers_satchell", || {
        RogersSatchell::new(Current::candle(), 20)
    });
    sweep("mfi", || Mfi::new(Current::candle(), 14));
    sweep("adx", || Adx::new(Current::candle(), 14).adx());
    sweep("williams_r", || WilliamsR::new(Current::candle(), 14));
    sweep("sma", || Sma::new(Current::close(), 20));
    sweep("ema", || Ema::new(Current::close(), 20));
    sweep("rma", || Rma::new(Current::close(), 14));
    sweep("wma", || Wma::new(Current::close(), 20));
    sweep("hma", || Hma::new(Current::close(), 16));
    sweep("rsi", || Rsi::new(Current::close(), 14));
    sweep("stddev", || StdDev::new(Current::close(), 20));
    sweep("skewness", || Skewness::new(Current::close(), 20));
    sweep("kurtosis", || Kurtosis::new(Current::close(), 20));
    sweep("zscore", || ZScore::new(Current::close(), 20));
    sweep("percentile", || Percentile::new(Current::close(), 20, 0.8));
    sweep("percentile_rank", || {
        PercentileRank::new(Current::close(), 20)
    });
    sweep("bars_since_high", || {
        BarsSinceHigh::new(Current::close(), 20)
    });
    sweep("bars_since_low", || BarsSinceLow::new(Current::close(), 20));
    sweep("correlation", || {
        Correlation::new(Current::close(), Current::open(), 20)
    });
    sweep("covariance", || {
        Covariance::new(Current::close(), Current::open(), 20)
    });
    sweep("beta", || Beta::new(Current::close(), Current::open(), 20));
    sweep("variance_ratio", || {
        VarianceRatio::new(Current::close(), 20, 2)
    });
    sweep("cci", || Cci::new(Current::typical(), 20));
    sweep("stochastic", || Stochastic::new(Current::close(), 14));
    sweep("latched_resample", || {
        Latch::new(Resample::new(Current::candle(), 4).close())
    });
    // Composite / multi-output: every component read separately.
    sweep("macd.line", || {
        Macd::new(Current::close(), 12, 26, 9).line()
    });
    sweep("macd.signal", || {
        Macd::new(Current::close(), 12, 26, 9).signal()
    });
    sweep("macd.histogram", || {
        Macd::new(Current::close(), 12, 26, 9).histogram()
    });
    sweep("adx.plus_di", || Adx::new(Current::candle(), 14).plus_di());
    sweep("adx.minus_di", || {
        Adx::new(Current::candle(), 14).minus_di()
    });
    sweep("aroon.up", || Aroon::new(Current::candle(), 25).up());
    sweep("aroon.down", || Aroon::new(Current::candle(), 25).down());
    sweep("aroon.oscillator", || {
        Aroon::new(Current::candle(), 25).oscillator()
    });
    sweep("bollinger.upper", || {
        Bollinger::new(Current::close(), 20, 2.0).upper()
    });
    sweep("bollinger.lower", || {
        Bollinger::new(Current::close(), 20, 2.0).lower()
    });
    sweep("donchian.middle", || {
        Donchian::new(Current::high(), Current::low(), 20).middle()
    });
    sweep("keltner.upper", || {
        Keltner::new(Current::close(), Current::candle(), 20, 10, 2.0).upper()
    });
    sweep("linreg.slope", || LinReg::new(Current::close(), 20).slope());
    sweep("linreg.intercept", || {
        LinReg::new(Current::close(), 20).intercept()
    });
    sweep("linreg.r2", || LinReg::new(Current::close(), 20).r2());
    // Operator layer over the same degenerate bars.
    sweep("ratio", || Current::close().ratio(3));
    sweep("roc", || Current::close().roc(5));
    sweep("diff", || Current::close().diff(3));
    sweep("pow", || Current::close().pow(Value::new(2.0)));
    sweep("div_by_close", || Current::high().div(Current::close()));
}

/// `period: 1` is the smallest legal window, and the one where every `n - 1`
/// in a formula becomes zero: a Bessel correction's divisor, a linear
/// regression's spread, a quantile's interpolation base. Constructors assert
/// `period > 0` rather than `> 1`, so the grammar admits it and a sweep must
/// too.
///
/// Same contract as above — a reading may be absent, it may not be a `NaN`.
#[test]
fn a_period_of_one_is_degenerate_but_not_undefined() {
    sweep("sma@1", || Sma::new(Current::close(), 1));
    sweep("ema@1", || Ema::new(Current::close(), 1));
    sweep("rma@1", || Rma::new(Current::close(), 1));
    sweep("wma@1", || Wma::new(Current::close(), 1));
    sweep("stddev@1", || StdDev::new(Current::close(), 1));
    sweep("zscore@1", || ZScore::new(Current::close(), 1));
    sweep("skewness@1", || Skewness::new(Current::close(), 1));
    sweep("kurtosis@1", || Kurtosis::new(Current::close(), 1));
    sweep("percentile@1", || Percentile::new(Current::close(), 1, 0.5));
    sweep("percentile_rank@1", || {
        PercentileRank::new(Current::close(), 1)
    });
    sweep("stochastic@1", || Stochastic::new(Current::close(), 1));
    sweep("williams_r@1", || WilliamsR::new(Current::candle(), 1));
    sweep("atr@1", || Atr::new(Current::candle(), 1));
    sweep("adx@1", || Adx::new(Current::candle(), 1).adx());
    sweep("aroon@1", || Aroon::new(Current::candle(), 1).oscillator());
    sweep("cci@1", || Cci::new(Current::typical(), 1));
    sweep("mfi@1", || Mfi::new(Current::candle(), 1));
    sweep("vwap@1", || Vwap::new(Current::candle(), 1));
    sweep("parkinson@1", || Parkinson::new(Current::candle(), 1));
    sweep("garman_klass@1", || GarmanKlass::new(Current::candle(), 1));
    sweep("rogers_satchell@1", || {
        RogersSatchell::new(Current::candle(), 1)
    });
    sweep("bollinger@1", || {
        Bollinger::new(Current::close(), 1, 2.0).upper()
    });
    sweep("correlation@1", || {
        Correlation::new(Current::close(), Current::open(), 1)
    });
    sweep("bars_since_high@1", || {
        BarsSinceHigh::new(Current::close(), 1)
    });
    sweep("rolling_max@1", || Current::close().rolling_max(1));
    sweep("lag@1", || Current::close().lag(1));
    sweep("roc@1", || Current::close().roc(1));
    // `LinReg` needs two points to have a slope at all, and says so; `period:
    // 2` is its floor and is the degenerate case here.
    sweep("linreg@2", || LinReg::new(Current::close(), 2).slope());
    sweep("linreg.r2@2", || LinReg::new(Current::close(), 2).r2());
}

/// `reset()` must return an indicator to its *freshly constructed* condition —
/// the contract on `Indicator::reset`. A field left behind is invisible until
/// something reuses an instance across runs, which `optimize` and the walk-
/// forward probes do; the saved state is the only complete view of it, so this
/// compares that rather than the public `value()`.
#[test]
fn reset_returns_every_indicator_to_its_constructed_state() {
    fn check<I, B>(name: &str, build: B)
    where
        I: Indicator<Input = Atom>,
        B: Fn() -> I,
    {
        let fresh = build().save_state();
        let mut used = build();
        for c in gapped() {
            used.update(Atom::from(c));
        }
        used.reset();
        assert_eq!(
            used.save_state(),
            fresh,
            "{name}: reset did not return the indicator to its constructed state"
        );
        assert!(
            used.value().is_none() || build().value().is_some(),
            "{name}: reset left a value behind"
        );
    }

    check("sma", || Sma::new(Current::close(), 20));
    check("ema", || Ema::new(Current::close(), 20));
    check("rma", || Rma::new(Current::close(), 14));
    check("wma", || Wma::new(Current::close(), 20));
    check("hma", || Hma::new(Current::close(), 16));
    check("rsi", || Rsi::new(Current::close(), 14));
    check("atr", || Atr::new(Current::candle(), 14));
    check("adx", || Adx::new(Current::candle(), 14));
    check("aroon", || Aroon::new(Current::candle(), 25));
    check("macd", || Macd::new(Current::close(), 12, 26, 9));
    check("bollinger", || Bollinger::new(Current::close(), 20, 2.0));
    check("keltner", || {
        Keltner::new(Current::close(), Current::candle(), 20, 10, 2.0)
    });
    check("donchian", || {
        Donchian::new(Current::high(), Current::low(), 20)
    });
    check("stochastic", || Stochastic::new(Current::close(), 14));
    check("williams_r", || WilliamsR::new(Current::candle(), 14));
    check("cci", || Cci::new(Current::typical(), 20));
    check("mfi", || Mfi::new(Current::candle(), 14));
    check("obv", || Obv::new(Current::candle()));
    check("ad", || Ad::new(Current::candle()));
    check("vwap", || Vwap::new(Current::candle(), 20));
    check("sar", || Sar::with_defaults(Current::candle()));
    check("true_range", || TrueRange::new(Current::candle()));
    check("stddev", || StdDev::new(Current::close(), 20));
    check("skewness", || Skewness::new(Current::close(), 20));
    check("kurtosis", || Kurtosis::new(Current::close(), 20));
    check("zscore", || ZScore::new(Current::close(), 20));
    check("percentile", || Percentile::new(Current::close(), 20, 0.8));
    check("percentile_rank", || {
        PercentileRank::new(Current::close(), 20)
    });
    check("linreg", || LinReg::new(Current::close(), 20));
    check("correlation", || {
        Correlation::new(Current::close(), Current::open(), 20)
    });
    check("variance_ratio", || {
        VarianceRatio::new(Current::close(), 20, 2)
    });
    check("parkinson", || Parkinson::new(Current::candle(), 20));
    check("garman_klass", || GarmanKlass::new(Current::candle(), 20));
    check("rogers_satchell", || {
        RogersSatchell::new(Current::candle(), 20)
    });
    check("bars_since_high", || {
        BarsSinceHigh::new(Current::close(), 20)
    });
    check("cum_sum", || CumSum::new(Current::close()));
    check("cum_max", || CumMax::new(Current::close()));
    check("lag", || Current::close().lag(3));
    check("rolling_max", || Current::close().rolling_max(10));
    check("crosses_above", || {
        Current::close().crosses_above(Sma::new(Current::close(), 10))
    });
    check("resample", || Resample::new(Current::candle(), 4));
}

/// Save mid-stream, rebuild from the same construction, restore, and continue:
/// every remaining reading must be **bit-identical** to a twin that never
/// paused. `tests/resume.rs` proves this for whole strategies; this proves it
/// per indicator, which is where a missing `#[state(...)]` annotation actually
/// lives.
#[test]
fn every_indicator_resumes_bit_identically_from_a_mid_stream_save() {
    fn check<I, B>(name: &str, build: B)
    where
        I: Indicator<Input = Atom, Output = Real>,
        B: Fn() -> I,
    {
        let bars = gapped();
        let cut = bars.len() / 2;
        let (mut paused, mut twin) = (build(), build());
        for c in &bars[..cut] {
            paused.update(Atom::from(*c));
            twin.update(Atom::from(*c));
        }
        let saved = paused.save_state();
        let mut resumed = build();
        resumed
            .load_state(&saved)
            .unwrap_or_else(|e| panic!("{name}: load_state rejected its own save: {e}"));
        for (i, c) in bars[cut..].iter().enumerate() {
            let a = resumed.update(Atom::from(*c));
            let b = twin.update(Atom::from(*c));
            match (a, b) {
                (Some(a), Some(b)) => assert_eq!(
                    a.to_bits(),
                    b.to_bits(),
                    "{name}: diverged {a} vs {b} at bar {} after resume",
                    cut + i
                ),
                (a, b) => assert_eq!(
                    a.is_some(),
                    b.is_some(),
                    "{name}: readiness diverged at bar {} after resume",
                    cut + i
                ),
            }
        }
    }

    check("sma", || Sma::new(Current::close(), 20));
    check("ema", || Ema::new(Current::close(), 20));
    check("rma", || Rma::new(Current::close(), 14));
    check("wma", || Wma::new(Current::close(), 20));
    check("hma", || Hma::new(Current::close(), 16));
    check("rsi", || Rsi::new(Current::close(), 14));
    check("atr", || Atr::new(Current::candle(), 14));
    check("adx", || Adx::new(Current::candle(), 14).adx());
    check("aroon", || Aroon::new(Current::candle(), 25).oscillator());
    check("macd", || {
        Macd::new(Current::close(), 12, 26, 9).histogram()
    });
    check("bollinger", || {
        Bollinger::new(Current::close(), 20, 2.0).upper()
    });
    check("keltner", || {
        Keltner::new(Current::close(), Current::candle(), 20, 10, 2.0).upper()
    });
    check("donchian", || {
        Donchian::new(Current::high(), Current::low(), 20).middle()
    });
    check("stochastic", || Stochastic::new(Current::close(), 14));
    check("williams_r", || WilliamsR::new(Current::candle(), 14));
    check("cci", || Cci::new(Current::typical(), 20));
    check("mfi", || Mfi::new(Current::candle(), 14));
    check("obv", || Obv::new(Current::candle()));
    check("ad", || Ad::new(Current::candle()));
    check("vwap", || Vwap::new(Current::candle(), 20));
    check("sar", || Sar::with_defaults(Current::candle()));
    check("true_range", || TrueRange::new(Current::candle()));
    check("stddev", || StdDev::new(Current::close(), 20));
    check("skewness", || Skewness::new(Current::close(), 20));
    check("kurtosis", || Kurtosis::new(Current::close(), 20));
    check("zscore", || ZScore::new(Current::close(), 20));
    check("percentile", || Percentile::new(Current::close(), 20, 0.8));
    check("percentile_rank", || {
        PercentileRank::new(Current::close(), 20)
    });
    check("linreg", || LinReg::new(Current::close(), 20).slope());
    check("correlation", || {
        Correlation::new(Current::close(), Current::open(), 20)
    });
    check("variance_ratio", || {
        VarianceRatio::new(Current::close(), 20, 2)
    });
    check("parkinson", || Parkinson::new(Current::candle(), 20));
    check("garman_klass", || GarmanKlass::new(Current::candle(), 20));
    check("rogers_satchell", || {
        RogersSatchell::new(Current::candle(), 20)
    });
    check("bars_since_high", || {
        BarsSinceHigh::new(Current::close(), 20)
    });
    check("cum_sum", || CumSum::new(Current::close()));
    check("cum_max", || CumMax::new(Current::close()));
    check("lag", || Current::close().lag(3));
    check("rolling_max", || Current::close().rolling_max(10));
    // `Latch` is deliberately **absent**. Its held value is `S::Output`, an
    // unbounded associated type, so it is `#[state(skip)]` and a resumed
    // `Latch` re-warms until the inner source next emits — the one bounded,
    // documented fidelity gap in the resume path (see `docs/ARCHITECTURE.md`,
    // *Run resuming*, and the field comment on `Latch::value`). It shares that
    // gap with the generic `Change` toggle detector. Pinned as behaviour by
    // `a_latch_re_warms_across_a_resume_rather_than_re_emitting` below rather
    // than asserted away here.
}

/// The documented `Latch` gap, pinned as *behaviour* so it stays bounded.
///
/// A resumed `Latch` holds nothing until its inner source next emits `Some`.
/// Over a `Resample`, that is up to `every - 1` bars of `None` where the
/// never-paused twin re-emits the held bucket — and then the two agree forever.
/// This test exists to fail if that window ever grows, or if the two stop
/// converging at the next boundary.
#[test]
fn a_latch_re_warms_across_a_resume_rather_than_re_emitting() {
    let bars = gapped();
    let every = 4;
    let build = || Latch::new(Resample::new(Current::candle(), every).close());
    // Cut one bar *after* a bucket boundary, so the held value is the thing
    // being lost.
    let cut = 41;
    assert_eq!(cut % every, 1, "the cut must land mid-bucket");

    let (mut paused, mut twin) = (build(), build());
    for c in &bars[..cut] {
        paused.update(Atom::from(*c));
        twin.update(Atom::from(*c));
    }
    assert!(twin.value().is_some(), "precondition: a value is held");

    let mut resumed = build();
    resumed
        .load_state(&paused.save_state())
        .expect("round trip");
    assert!(
        resumed.value().is_none(),
        "precondition: the held value is not restored"
    );

    let mut disagreed = 0usize;
    for (i, c) in bars[cut..].iter().enumerate() {
        let a = resumed.update(Atom::from(*c));
        let b = twin.update(Atom::from(*c));
        if a != b {
            disagreed += 1;
            assert!(
                a.is_none() && b.is_some(),
                "the gap must only ever be a missing reading, never a wrong one"
            );
        } else {
            assert!(
                a.is_none() || i + 1 >= every - (cut % every),
                "the two must agree from the next boundary onward"
            );
        }
    }
    assert!(
        disagreed < every,
        "the re-warm must be bounded by one bucket, saw {disagreed} bars"
    );
}
