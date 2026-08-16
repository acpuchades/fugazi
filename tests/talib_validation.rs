//! Cross-validation of fugazi's indicators against TA-Lib reference values.
//!
//! Consumes two CSVs from `tests/data/`:
//!   * `aapl_monthly.csv`   — committed OHLCV input (see `tests/data/README.md`).
//!   * `talib_expected.csv` — TA-Lib outputs for that input, produced by
//!     `tools/gen_talib_fixtures.py` (run once, needs the TA-Lib library).
//!
//! # Why the tests are grouped the way they are
//!
//! The split is by *tolerance rationale*, not by indicator, because that is the
//! only axis on which the comparisons genuinely differ:
//!
//! - **Exact** — non-recursive windowed math and cumulative sums. fugazi and
//!   TA-Lib compute the same closed form over the same bars, so they must agree
//!   to `1e-6` on **every** warmed bar, and where TA-Lib has a value fugazi must
//!   have one too (aligned warm-up).
//! - **Converged** — recursive smoothers. fugazi seeds each recurrence from the
//!   first sample(s); TA-Lib seeds from an SMA (EMA family) or a summed Wilder
//!   state (ADX family). That difference decays geometrically, so these are
//!   compared only over the tail, at a looser tolerance.
//!
//! # When the fixture is absent
//!
//! `talib_expected.csv` is generated, and TA-Lib is not a Cargo dependency, so
//! the suite skips when it isn't there — see `common::fixtures` for the policy
//! and for `FUGAZI_REQUIRE_FIXTURES=1`, which turns the skip into a failure.
//! **A skip means this file compared nothing.** `docs/CONTRIBUTING.md` lists this
//! suite as a drift guard; [`tests/indicator_reference.rs`] is the always-running
//! battery that holds the line when the fixture is missing.
//!
//! Parameters must match `tools/gen_talib_fixtures.py`.

mod common;

use std::collections::BTreeMap;

use common::fixtures::{Csv, skip};
use fugazi::indicators::{
    Ad, Adx, Aroon, Atr, Bollinger, Cci, Current, Dmi, Ema, Hma, Identity, Keltner, Macd, Mfi, Obv,
    RollingMax, RollingMin, Rsi, Sar, Sma, StdDev, Stochastic, TrueRange, WilliamsR, Wma,
};
use fugazi::prelude::*;

const SMA_P: usize = 10;
const EMA_P: usize = 10;
const RSI_P: usize = 14;
const ATR_P: usize = 14;
const STDDEV_P: usize = 10;
const BB_P: usize = 20;
const BB_K: Real = 2.0;
const DONCHIAN_P: usize = 10;
const MACD_FAST: usize = 12;
const MACD_SLOW: usize = 26;
const MACD_SIGNAL: usize = 9;
const ADX_P: usize = 14;
const STOCH_P: usize = 14;
const MFI_P: usize = 14;
const WMA_P: usize = 10;
const HMA_P: usize = 16;
const ROC_P: usize = 10;
const WILLR_P: usize = 14;
const CCI_P: usize = 20;
const AROON_P: usize = 14;
const DMI_P: usize = 14;
const KC_EMA_P: usize = 20;
const KC_ATR_P: usize = 10;
const KC_MULT: Real = 2.0;
const SAR_STEP: Real = 0.02;
const SAR_MAX: Real = 0.2;

/// Tolerance for indicators that share TA-Lib's exact conventions.
const EXACT_TOL: Real = 1e-6;
/// Looser tolerance for the recursively-seeded family, checked over the tail
/// only, where the differing seed has decayed away.
const CONVERGED_TOL: Real = 2e-2;

/// Every column the fixture must carry. A generated CSV predating a newly added
/// indicator is *stale*, which skips (or fails) with the same regenerate hint as
/// an absent one — never a mid-run panic on a missing column.
const REQUIRED: &[&str] = &[
    "sma10", "ema10", "rsi14", "atr14", "stddev10", "bb_upper", "bb_mid", "bb_lower", "max10_high",
    "min10_low", "macd", "macd_signal", "macd_hist", "adx14", "plus_di14", "minus_di14", "trange",
    "stochf_k14", "obv", "ad", "mfi14", "wma10", "hma16", "roc10", "willr14", "cci20", "aroon_up14",
    "aroon_dn14", "aroon_osc14", "kc_upper", "kc_mid", "kc_lower", "sar",
];

const HINT: &str = "  mamba env create -f tools/environment.yml   # or: conda\n  \
                    mamba run -n fugazi-talib python3 tools/gen_talib_fixtures.py\n  \
                    cargo test --test talib_validation";

/// fugazi's output for every cross-checked series, keyed by the fixture's
/// column name, plus the reference CSV itself.
struct Comparison {
    fugazi: BTreeMap<&'static str, Vec<Option<Real>>>,
    expected: Csv,
    bars: usize,
}

/// Load the fixtures and run every indicator over the input, or `None` when the
/// reference CSV is absent or stale (which [`skip`] reports or fails on).
fn load() -> Option<Comparison> {
    let Some(expected) = Csv::load("talib_expected.csv") else {
        skip(
            "talib_validation",
            "tests/data/talib_expected.csv is not present",
            HINT,
        );
        return None;
    };
    if let Some(missing) = expected.missing(REQUIRED) {
        skip(
            "talib_validation",
            &format!("tests/data/talib_expected.csv has no `{missing}` column (stale fixture)"),
            HINT,
        );
        return None;
    }

    let input = Csv::require("aapl_monthly.csv");
    let high = input.floats("high");
    let low = input.floats("low");
    let close = input.floats("close");
    let volume = input.floats("volume");
    let bars = close.len();
    assert_eq!(expected.len(), bars, "fixture row counts differ");

    let mut sma = Sma::new(Identity::new(), SMA_P);
    let mut ema = Ema::new(Identity::new(), EMA_P);
    let mut rsi = Rsi::new(Identity::new(), RSI_P);
    let mut atr = Atr::new(Current::candle(), ATR_P);
    let mut sd = StdDev::new(Identity::new(), STDDEV_P);
    let mut bb = Bollinger::new(Identity::new(), BB_P, BB_K);
    let mut rmax = RollingMax::new(Identity::new(), DONCHIAN_P);
    let mut rmin = RollingMin::new(Identity::new(), DONCHIAN_P);
    let mut macd = Macd::new(Identity::new(), MACD_FAST, MACD_SLOW, MACD_SIGNAL);
    let mut adx = Adx::new(Current::candle(), ADX_P);
    let mut tr = TrueRange::new(Current::candle());
    let mut stoch = Stochastic::new(Identity::new(), STOCH_P);
    let mut obv = Obv::new(Current::candle());
    let mut ad = Ad::new(Current::candle());
    let mut mfi = Mfi::new(Current::candle(), MFI_P);
    let mut wma = Wma::new(Identity::new(), WMA_P);
    let mut hma = Hma::new(Identity::new(), HMA_P);
    let mut roc = Identity::new().roc(ROC_P);
    let mut willr = WilliamsR::new(Current::candle(), WILLR_P);
    let mut cci = Cci::new(Current::typical(), CCI_P);
    let mut aroon = Aroon::new(Current::candle(), AROON_P);
    let mut dmi = Dmi::new(Current::candle(), DMI_P);
    let mut kc = Keltner::new(Current::close(), Current::candle(), KC_EMA_P, KC_ATR_P, KC_MULT);
    let mut sar = Sar::new(Current::candle(), SAR_STEP, SAR_MAX);

    let mut out: BTreeMap<&'static str, Vec<Option<Real>>> = BTreeMap::new();
    let push = |out: &mut BTreeMap<&'static str, Vec<Option<Real>>>, k, v| {
        out.entry(k).or_default().push(v)
    };

    for i in 0..bars {
        // The fixture's `open` is unused: the generator feeds TA-Lib
        // (high, low, close, volume) only, so the input candle uses `close` for
        // `open` to keep the two consumers reading identical numbers.
        let atom: Atom = Candle::new(close[i], high[i], low[i], close[i], volume[i]).into();

        push(&mut out, "sma10", sma.update(close[i]));
        push(&mut out, "ema10", ema.update(close[i]));
        push(&mut out, "rsi14", rsi.update(close[i]));
        push(&mut out, "atr14", atr.update(atom.clone()));
        push(&mut out, "stddev10", sd.update(close[i]));
        let b = bb.update(close[i]);
        push(&mut out, "bb_upper", b.map(|v| v.upper));
        push(&mut out, "bb_mid", b.map(|v| v.middle));
        push(&mut out, "bb_lower", b.map(|v| v.lower));
        push(&mut out, "max10_high", rmax.update(high[i]));
        push(&mut out, "min10_low", rmin.update(low[i]));
        let m = macd.update(close[i]);
        push(&mut out, "macd", m.map(|v| v.macd));
        push(&mut out, "macd_signal", m.map(|v| v.signal));
        push(&mut out, "macd_hist", m.map(|v| v.histogram));
        // +DI/-DI populate (and TA-Lib emits them) `period` bars before `adx`
        // is ready, so read the public fields directly rather than the combined
        // `AdxValue`, which only surfaces once `adx` itself exists.
        adx.update(atom.clone());
        push(&mut out, "adx14", adx.adx);
        push(&mut out, "plus_di14", adx.plus_di);
        push(&mut out, "minus_di14", adx.minus_di);
        push(&mut out, "trange", tr.update(atom.clone()));
        // fugazi yields the stochastic in [0, 1]; TA-Lib's %K is in [0, 100].
        push(&mut out, "stochf_k14", stoch.update(close[i]).map(|v| v * 100.0));
        push(&mut out, "obv", obv.update(atom.clone()));
        push(&mut out, "ad", ad.update(atom.clone()));
        push(&mut out, "mfi14", mfi.update(atom.clone()));
        push(&mut out, "wma10", wma.update(close[i]));
        push(&mut out, "hma16", hma.update(close[i]));
        push(&mut out, "roc10", roc.update(close[i]));
        push(&mut out, "willr14", willr.update(atom.clone()));
        push(&mut out, "cci20", cci.update(atom.clone()));
        let ar = aroon.update(atom.clone());
        push(&mut out, "aroon_up14", ar.map(|v| v.up));
        push(&mut out, "aroon_dn14", ar.map(|v| v.down));
        push(&mut out, "aroon_osc14", ar.map(|v| v.oscillator));
        // `Dmi` is the standalone +DI/-DI core `Adx` embeds, so it is checked
        // against TA-Lib's PLUS_DI/MINUS_DI columns too.
        dmi.update(atom.clone());
        push(&mut out, "dmi_plus", dmi.plus_di);
        push(&mut out, "dmi_minus", dmi.minus_di);
        let k = kc.update(atom.clone());
        push(&mut out, "kc_upper", k.map(|v| v.upper));
        push(&mut out, "kc_mid", k.map(|v| v.middle));
        push(&mut out, "kc_lower", k.map(|v| v.lower));
        push(&mut out, "sar", sar.update(atom));
    }

    Some(Comparison {
        fugazi: out,
        expected,
        bars,
    })
}

impl Comparison {
    /// Compare fugazi's `series` against the reference `column` from bar
    /// `start` on, and return how many cells were actually compared.
    #[track_caller]
    fn compare(&self, series: &str, column: &str, tol: Real, start: usize) -> usize {
        let got = self
            .fugazi
            .get(series)
            .unwrap_or_else(|| panic!("no fugazi series `{series}`"));
        let want = self.expected.optional_floats(column);
        let mut compared = 0;
        for i in start..want.len() {
            let (Some(exp), Some(g)) = (want[i], got[i]) else {
                // For exact-convention indicators the warm-up must align too:
                // where TA-Lib has a value, fugazi must have one.
                if want[i].is_some() && tol == EXACT_TOL {
                    panic!("{series}[{i}]: TA-Lib has {:?} but fugazi is None", want[i]);
                }
                continue;
            };
            let scale = g.abs().max(exp.abs()).max(1.0);
            assert!(
                (g - exp).abs() <= tol * scale,
                "{series}[{i}]: fugazi {g} vs TA-Lib {exp} (tol {tol})"
            );
            compared += 1;
        }
        compared
    }

    /// Run a batch of `(series, column)` pairs, asserting each one actually
    /// compared cells — a silently-empty column is the failure mode this whole
    /// suite is prone to.
    #[track_caller]
    fn compare_all(&self, pairs: &[(&str, &str)], tol: Real, start: usize) {
        for &(series, column) in pairs {
            let n = self.compare(series, column, tol, start);
            assert!(
                n > 0,
                "{series}: zero cells compared against `{column}` — \
                 the fixture column is empty or entirely warm-up"
            );
        }
    }

    /// Where the converged comparisons start: three quarters in, by which point
    /// the seed difference in every recursive family has decayed well below
    /// [`CONVERGED_TOL`].
    fn tail(&self) -> usize {
        self.bars * 3 / 4
    }
}

/// Non-recursive windowed math: fugazi and TA-Lib evaluate the same closed
/// form over the same bars, so every warmed cell must agree to `1e-6`.
#[test]
fn windowed_indicators_match_talib_exactly() {
    let Some(c) = load() else { return };
    c.compare_all(
        &[
            ("sma10", "sma10"),
            ("rsi14", "rsi14"),
            ("stddev10", "stddev10"),
            ("bb_upper", "bb_upper"),
            ("bb_mid", "bb_mid"),
            ("bb_lower", "bb_lower"),
            ("max10_high", "max10_high"),
            ("min10_low", "min10_low"),
            ("trange", "trange"),
            ("stochf_k14", "stochf_k14"),
            ("wma10", "wma10"),
            ("hma16", "hma16"),
            ("roc10", "roc10"),
            ("willr14", "willr14"),
            ("cci20", "cci20"),
            ("aroon_up14", "aroon_up14"),
            ("aroon_dn14", "aroon_dn14"),
            ("aroon_osc14", "aroon_osc14"),
            // Recursive but fully deterministic — no smoothed seed to differ on.
            ("sar", "sar"),
        ],
        EXACT_TOL,
        0,
    );
}

/// Volume indicators: cumulative (OBV/AD) or windowed (MFI) sums with no
/// recursive seed, so they match TA-Lib exactly. (VWAP has no TA-Lib
/// counterpart and is covered by unit tests only.)
#[test]
fn volume_indicators_match_talib_exactly() {
    let Some(c) = load() else { return };
    c.compare_all(
        &[("obv", "obv"), ("ad", "ad"), ("mfi14", "mfi14")],
        EXACT_TOL,
        0,
    );
}

/// The recursively-seeded family. fugazi seeds each recurrence from the first
/// sample(s); TA-Lib uses an SMA (EMA family) or a summed Wilder state (ADX
/// family). The gap decays geometrically, so these agree over the tail even
/// though the first warmed bars differ by ~1%.
#[test]
fn recursively_seeded_indicators_converge_to_talib() {
    let Some(c) = load() else { return };
    let tail = c.tail();
    c.compare_all(
        &[
            ("ema10", "ema10"),
            ("atr14", "atr14"),
            ("macd", "macd"),
            ("macd_signal", "macd_signal"),
            ("macd_hist", "macd_hist"),
            ("adx14", "adx14"),
            ("plus_di14", "plus_di14"),
            ("minus_di14", "minus_di14"),
            ("dmi_plus", "plus_di14"),
            ("dmi_minus", "minus_di14"),
            ("kc_upper", "kc_upper"),
            ("kc_mid", "kc_mid"),
            ("kc_lower", "kc_lower"),
        ],
        CONVERGED_TOL,
        tail,
    );
}

/// Every column this suite claims to check must be one the fixture generator
/// actually writes. Catches a `REQUIRED` entry renamed on one side only —
/// which would otherwise present as a permanent "stale fixture" skip that
/// regenerating never fixes.
#[test]
fn every_required_column_is_produced_by_the_generator() {
    let generator = std::fs::read_to_string(concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/tools/gen_talib_fixtures.py"
    ))
    .expect("tools/gen_talib_fixtures.py is committed");
    for column in REQUIRED {
        assert!(
            generator.contains(&format!("\"{column}\"")) || generator.contains(&format!("'{column}'")),
            "`{column}` is required by the test but never written by \
             tools/gen_talib_fixtures.py"
        );
    }
}
