//! Cross-validation of fugazi's indicators against TA-Lib reference values.
//!
//! This test consumes two committed CSVs from `tests/data/`:
//!   * `aapl_monthly.csv`   — offline OHLCV input (see tests/data/README.md).
//!   * `talib_expected.csv` — TA-Lib outputs for that input, produced by
//!     `tools/gen_talib_fixtures.py` (run once, needs the TA-Lib library).
//!
//! If the expected file is absent the test **skips** (prints how to generate it)
//! so `cargo test` stays green without TA-Lib installed. When present, fugazi is
//! run over the identical input and compared cell-by-cell.
//!
//! Parameters must match `tools/gen_talib_fixtures.py`.

use std::path::PathBuf;

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
/// Looser tolerance for EMA/ATR, whose seeding differs from TA-Lib (see below);
/// only checked over the tail of the series, where the seed difference has
/// decayed away.
const CONVERGED_TOL: Real = 2e-2;

fn data_path(name: &str) -> PathBuf {
    [env!("CARGO_MANIFEST_DIR"), "tests", "data", name]
        .iter()
        .collect()
}

/// Minimal CSV reader: returns (headers, rows-of-cells). No quoting/escaping —
/// our fixtures are plain numeric CSV.
fn read_csv(path: &PathBuf) -> Option<(Vec<String>, Vec<Vec<String>>)> {
    let text = std::fs::read_to_string(path).ok()?;
    let mut lines = text.lines();
    let headers: Vec<String> = lines.next()?.split(',').map(|s| s.trim().to_string()).collect();
    let rows = lines
        .filter(|l| !l.trim().is_empty())
        .map(|l| l.split(',').map(|s| s.trim().to_string()).collect())
        .collect();
    Some((headers, rows))
}

fn float_col(headers: &[String], rows: &[Vec<String>], name: &str) -> Vec<Real> {
    let idx = headers.iter().position(|h| h == name).expect("missing column");
    rows.iter().map(|r| r[idx].parse().expect("numeric")).collect()
}

/// Column of expected values, `None` for empty (warm-up / NaN) cells.
fn opt_col(headers: &[String], rows: &[Vec<String>], name: &str) -> Vec<Option<Real>> {
    let idx = headers.iter().position(|h| h == name).expect("missing column");
    rows.iter()
        .map(|r| {
            let c = &r[idx];
            (!c.is_empty()).then(|| c.parse().expect("numeric"))
        })
        .collect()
}

fn rel_close(a: Real, b: Real, tol: Real) -> bool {
    (a - b).abs() <= tol * a.abs().max(b.abs()).max(1.0)
}

/// Compare fugazi output against the expected column at `tol`, only over indices
/// `>= start`. Returns the number of cells actually compared.
fn compare(
    label: &str,
    fugazi: &[Option<Real>],
    expected: &[Option<Real>],
    tol: Real,
    start: usize,
) -> usize {
    let mut compared = 0;
    for i in start..expected.len() {
        let (Some(exp), Some(got)) = (expected[i], fugazi[i]) else {
            // For exact-convention indicators, warm-up must align: where TA-Lib
            // has a value, fugazi must too.
            if expected[i].is_some() && tol == EXACT_TOL {
                panic!("{label}[{i}]: TA-Lib has {:?} but fugazi is None", expected[i]);
            }
            continue;
        };
        assert!(
            rel_close(got, exp, tol),
            "{label}[{i}]: fugazi {got} vs TA-Lib {exp} (tol {tol})"
        );
        compared += 1;
    }
    compared
}

#[test]
fn matches_talib_reference() {
    let expected_path = data_path("talib_expected.csv");
    let Some((headers, rows)) = read_csv(&expected_path) else {
        eprintln!(
            "SKIP talib_validation: {} not found.\n\
             Generate it with TA-Lib installed:\n  \
             pip install TA-Lib numpy && python3 tools/gen_talib_fixtures.py\n  \
             cargo test --test talib_validation",
            expected_path.display()
        );
        return;
    };

    // Guard against a stale fixture: if the committed CSV predates a new
    // indicator column, skip with the same regenerate hint rather than panicking
    // on a missing column mid-run.
    const REQUIRED: &[&str] = &[
        "sma10", "ema10", "rsi14", "atr14", "stddev10", "bb_upper", "bb_mid", "bb_lower",
        "max10_high", "min10_low", "macd", "macd_signal", "macd_hist", "adx14", "plus_di14",
        "minus_di14", "trange", "stochf_k14", "obv", "ad", "mfi14", "wma10", "hma16", "roc10",
        "willr14", "cci20", "aroon_up14", "aroon_dn14", "aroon_osc14", "kc_upper", "kc_mid",
        "kc_lower", "sar",
    ];
    if let Some(missing) = REQUIRED.iter().find(|c| !headers.iter().any(|h| h == *c)) {
        eprintln!(
            "SKIP talib_validation: {} is missing column `{missing}` (stale fixture).\n\
             Regenerate it with TA-Lib installed:\n  \
             python3 tools/gen_talib_fixtures.py\n  \
             cargo test --test talib_validation",
            expected_path.display()
        );
        return;
    }

    let (ih, ir) = read_csv(&data_path("aapl_monthly.csv")).expect("input fixture present");
    let high = float_col(&ih, &ir, "high");
    let low = float_col(&ih, &ir, "low");
    let close = float_col(&ih, &ir, "close");
    let volume = float_col(&ih, &ir, "volume");
    let n = close.len();
    assert_eq!(rows.len(), n, "fixture row counts differ");

    // Run each fugazi indicator over the identical input.
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

    let mut sma_o = Vec::with_capacity(n);
    let mut ema_o = Vec::with_capacity(n);
    let mut rsi_o = Vec::with_capacity(n);
    let mut atr_o = Vec::with_capacity(n);
    let mut sd_o = Vec::with_capacity(n);
    let mut bb_u = Vec::with_capacity(n);
    let mut bb_m = Vec::with_capacity(n);
    let mut bb_l = Vec::with_capacity(n);
    let mut max_o = Vec::with_capacity(n);
    let mut min_o = Vec::with_capacity(n);
    let mut macd_o = Vec::with_capacity(n);
    let mut macd_sig_o = Vec::with_capacity(n);
    let mut macd_hist_o = Vec::with_capacity(n);
    let mut adx_o = Vec::with_capacity(n);
    let mut plus_di_o = Vec::with_capacity(n);
    let mut minus_di_o = Vec::with_capacity(n);
    let mut tr_o = Vec::with_capacity(n);
    let mut stoch_o = Vec::with_capacity(n);
    let mut obv_o = Vec::with_capacity(n);
    let mut ad_o = Vec::with_capacity(n);
    let mut mfi_o = Vec::with_capacity(n);
    let mut wma_o = Vec::with_capacity(n);
    let mut hma_o = Vec::with_capacity(n);
    let mut roc_o = Vec::with_capacity(n);
    let mut willr_o = Vec::with_capacity(n);
    let mut cci_o = Vec::with_capacity(n);
    let mut aroon_up_o = Vec::with_capacity(n);
    let mut aroon_dn_o = Vec::with_capacity(n);
    let mut aroon_osc_o = Vec::with_capacity(n);
    let mut dmi_plus_o = Vec::with_capacity(n);
    let mut dmi_minus_o = Vec::with_capacity(n);
    let mut kc_u = Vec::with_capacity(n);
    let mut kc_m = Vec::with_capacity(n);
    let mut kc_l = Vec::with_capacity(n);
    let mut sar_o = Vec::with_capacity(n);

    for i in 0..n {
        let candle = Candle::new(close[i], high[i], low[i], close[i], volume[i]);
        let atom: Atom = candle.into();
        sma_o.push(sma.update(close[i]));
        ema_o.push(ema.update(close[i]));
        rsi_o.push(rsi.update(close[i]));
        atr_o.push(atr.update(atom.clone()));
        sd_o.push(sd.update(close[i]));
        let b = bb.update(close[i]);
        bb_u.push(b.map(|v| v.upper));
        bb_m.push(b.map(|v| v.middle));
        bb_l.push(b.map(|v| v.lower));
        max_o.push(rmax.update(high[i]));
        min_o.push(rmin.update(low[i]));
        let m = macd.update(close[i]);
        macd_o.push(m.map(|v| v.macd));
        macd_sig_o.push(m.map(|v| v.signal));
        macd_hist_o.push(m.map(|v| v.histogram));
        // +DI/-DI populate (and TA-Lib emits them) `period` bars before `adx`
        // is ready, so read the public fields directly rather than the combined
        // `AdxValue`, which only surfaces once `adx` itself exists.
        adx.update(atom.clone());
        adx_o.push(adx.adx);
        plus_di_o.push(adx.plus_di);
        minus_di_o.push(adx.minus_di);
        tr_o.push(tr.update(atom.clone()));
        // fugazi yields the stochastic in [0, 1]; TA-Lib's %K is in [0, 100].
        stoch_o.push(stoch.update(close[i]).map(|v| v * 100.0));
        obv_o.push(obv.update(atom.clone()));
        ad_o.push(ad.update(atom.clone()));
        mfi_o.push(mfi.update(atom.clone()));
        wma_o.push(wma.update(close[i]));
        hma_o.push(hma.update(close[i]));
        roc_o.push(roc.update(close[i]));
        willr_o.push(willr.update(atom.clone()));
        cci_o.push(cci.update(atom.clone()));
        let ar = aroon.update(atom.clone());
        aroon_up_o.push(ar.map(|v| v.up));
        aroon_dn_o.push(ar.map(|v| v.down));
        aroon_osc_o.push(ar.map(|v| v.oscillator));
        dmi.update(atom.clone());
        dmi_plus_o.push(dmi.plus_di);
        dmi_minus_o.push(dmi.minus_di);
        let k = kc.update(atom.clone());
        kc_u.push(k.map(|v| v.upper));
        kc_m.push(k.map(|v| v.middle));
        kc_l.push(k.map(|v| v.lower));
        sar_o.push(sar.update(atom));
    }

    // Exact-convention indicators: must match to EXACT_TOL across all warmed bars.
    let mut total = 0;
    total += compare("sma10", &sma_o, &opt_col(&headers, &rows, "sma10"), EXACT_TOL, 0);
    total += compare("rsi14", &rsi_o, &opt_col(&headers, &rows, "rsi14"), EXACT_TOL, 0);
    total += compare("stddev10", &sd_o, &opt_col(&headers, &rows, "stddev10"), EXACT_TOL, 0);
    total += compare("bb_upper", &bb_u, &opt_col(&headers, &rows, "bb_upper"), EXACT_TOL, 0);
    total += compare("bb_mid", &bb_m, &opt_col(&headers, &rows, "bb_mid"), EXACT_TOL, 0);
    total += compare("bb_lower", &bb_l, &opt_col(&headers, &rows, "bb_lower"), EXACT_TOL, 0);
    total += compare("max10_high", &max_o, &opt_col(&headers, &rows, "max10_high"), EXACT_TOL, 0);
    total += compare("min10_low", &min_o, &opt_col(&headers, &rows, "min10_low"), EXACT_TOL, 0);
    total += compare("trange", &tr_o, &opt_col(&headers, &rows, "trange"), EXACT_TOL, 0);
    total += compare("stochf_k14", &stoch_o, &opt_col(&headers, &rows, "stochf_k14"), EXACT_TOL, 0);
    // Volume indicators: cumulative (OBV/AD) or windowed (MFI) sums, no recursive
    // seed, so they match TA-Lib exactly. (VWAP has no TA-Lib counterpart.)
    total += compare("obv", &obv_o, &opt_col(&headers, &rows, "obv"), EXACT_TOL, 0);
    total += compare("ad", &ad_o, &opt_col(&headers, &rows, "ad"), EXACT_TOL, 0);
    total += compare("mfi14", &mfi_o, &opt_col(&headers, &rows, "mfi14"), EXACT_TOL, 0);
    // WMA/ROC/Williams %R/CCI/Aroon are non-recursive windowed math, and HMA is
    // pure WMA composition, so all match TA-Lib's conventions exactly. SAR is
    // recursive but fully deterministic (no smoothed seed), so it matches too.
    total += compare("wma10", &wma_o, &opt_col(&headers, &rows, "wma10"), EXACT_TOL, 0);
    total += compare("hma16", &hma_o, &opt_col(&headers, &rows, "hma16"), EXACT_TOL, 0);
    total += compare("roc10", &roc_o, &opt_col(&headers, &rows, "roc10"), EXACT_TOL, 0);
    total += compare("willr14", &willr_o, &opt_col(&headers, &rows, "willr14"), EXACT_TOL, 0);
    total += compare("cci20", &cci_o, &opt_col(&headers, &rows, "cci20"), EXACT_TOL, 0);
    total += compare("aroon_up14", &aroon_up_o, &opt_col(&headers, &rows, "aroon_up14"), EXACT_TOL, 0);
    total += compare("aroon_dn14", &aroon_dn_o, &opt_col(&headers, &rows, "aroon_dn14"), EXACT_TOL, 0);
    total += compare("aroon_osc14", &aroon_osc_o, &opt_col(&headers, &rows, "aroon_osc14"), EXACT_TOL, 0);
    total += compare("sar", &sar_o, &opt_col(&headers, &rows, "sar"), EXACT_TOL, 0);
    assert!(total > 0, "no cells were compared — check fixtures");

    // EMA/ATR: fugazi seeds the recurrence with the first value, whereas TA-Lib
    // seeds with an SMA of the first `period` samples. That difference decays
    // geometrically, so we only check the tail of the series (looser tolerance).
    //
    // The same applies to every other Wilder/EMA-seeded indicator:
    //   * MACD — fast/slow/signal are all EMAs.
    //   * ADX, +DI, -DI — TA-Lib seeds its Wilder sums differently; the gap
    //     decays geometrically, so fugazi and TA-Lib agree to ~5 figures by the
    //     tail even though the first warmed bars differ by ~1%.
    let tail = n * 3 / 4;
    compare("ema10", &ema_o, &opt_col(&headers, &rows, "ema10"), CONVERGED_TOL, tail);
    compare("atr14", &atr_o, &opt_col(&headers, &rows, "atr14"), CONVERGED_TOL, tail);
    compare("macd", &macd_o, &opt_col(&headers, &rows, "macd"), CONVERGED_TOL, tail);
    compare("macd_signal", &macd_sig_o, &opt_col(&headers, &rows, "macd_signal"), CONVERGED_TOL, tail);
    compare("macd_hist", &macd_hist_o, &opt_col(&headers, &rows, "macd_hist"), CONVERGED_TOL, tail);
    compare("adx14", &adx_o, &opt_col(&headers, &rows, "adx14"), CONVERGED_TOL, tail);
    compare("plus_di14", &plus_di_o, &opt_col(&headers, &rows, "plus_di14"), CONVERGED_TOL, tail);
    compare("minus_di14", &minus_di_o, &opt_col(&headers, &rows, "minus_di14"), CONVERGED_TOL, tail);
    // Dmi is the standalone +DI/-DI core Adx embeds; same Wilder seeding, so it
    // tracks TA-Lib's PLUS_DI/MINUS_DI over the converged tail.
    compare("dmi_plus", &dmi_plus_o, &opt_col(&headers, &rows, "plus_di14"), CONVERGED_TOL, tail);
    compare("dmi_minus", &dmi_minus_o, &opt_col(&headers, &rows, "minus_di14"), CONVERGED_TOL, tail);
    // Keltner bands TA-Lib's EMA with its ATR; both seed recursively, so (like
    // EMA/ATR) fugazi and TA-Lib agree once the seed difference has decayed.
    compare("kc_upper", &kc_u, &opt_col(&headers, &rows, "kc_upper"), CONVERGED_TOL, tail);
    compare("kc_mid", &kc_m, &opt_col(&headers, &rows, "kc_mid"), CONVERGED_TOL, tail);
    compare("kc_lower", &kc_l, &opt_col(&headers, &rows, "kc_lower"), CONVERGED_TOL, tail);
}
