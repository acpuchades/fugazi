#!/usr/bin/env python3
"""Generate TA-Lib reference values for arcana's cross-validation test.

Reads the offline price fixture and writes one column of expected output per
indicator, aligned row-for-row with the input. Empty cells mark warm-up / NaN.

Usage (conda, recommended — bundles the TA-Lib C library):
    conda env create -f tools/environment.yml
    conda activate arcana-talib
    python3 tools/gen_talib_fixtures.py

Usage (pip — needs the TA-Lib C library already installed, e.g.
`brew install ta-lib` on macOS):
    pip install TA-Lib numpy
    python3 tools/gen_talib_fixtures.py

Then run the Rust side:
    cargo test --test talib_validation

Both sides consume the SAME CSV, so the comparison is valid regardless of how
representative the prices are. Parameters here must match those in
`tests/talib_validation.rs`.
"""

import csv
import os

import numpy as np
import talib

ROOT = os.path.dirname(os.path.dirname(os.path.abspath(__file__)))
IN_CSV = os.path.join(ROOT, "tests", "data", "aapl_monthly.csv")
OUT_CSV = os.path.join(ROOT, "tests", "data", "talib_expected.csv")

# Parameters — keep in sync with tests/talib_validation.rs.
SMA_P = 10
EMA_P = 10
RSI_P = 14
ATR_P = 14
STDDEV_P = 10
BB_P, BB_K = 20, 2.0
DONCHIAN_P = 10
MACD_FAST, MACD_SLOW, MACD_SIGNAL = 12, 26, 9
ADX_P = 14
STOCH_P = 14
MFI_P = 14


def main() -> None:
    rows = []
    with open(IN_CSV, newline="") as f:
        for r in csv.DictReader(f):
            rows.append(r)

    high = np.array([float(r["high"]) for r in rows])
    low = np.array([float(r["low"]) for r in rows])
    close = np.array([float(r["close"]) for r in rows])
    volume = np.array([float(r["volume"]) for r in rows])

    bb_up, bb_mid, bb_lo = talib.BBANDS(
        close, timeperiod=BB_P, nbdevup=BB_K, nbdevdn=BB_K, matype=0
    )
    macd, macd_signal, macd_hist = talib.MACD(
        close, fastperiod=MACD_FAST, slowperiod=MACD_SLOW, signalperiod=MACD_SIGNAL
    )
    # arcana's Stochastic over a single source positions that source within its
    # own rolling [min, max]. Feeding close as high/low/close makes TA-Lib's
    # fast %K compute the same thing (scaled to [0, 100]); we take %K only.
    stoch_k, _stoch_d = talib.STOCHF(
        close, close, close, fastk_period=STOCH_P, fastd_period=3, fastd_matype=0
    )

    cols = {
        "sma10": talib.SMA(close, SMA_P),
        "ema10": talib.EMA(close, EMA_P),
        "rsi14": talib.RSI(close, RSI_P),
        "atr14": talib.ATR(high, low, close, ATR_P),
        "stddev10": talib.STDDEV(close, STDDEV_P, nbdev=1.0),
        "bb_upper": bb_up,
        "bb_mid": bb_mid,
        "bb_lower": bb_lo,
        "max10_high": talib.MAX(high, DONCHIAN_P),
        "min10_low": talib.MIN(low, DONCHIAN_P),
        "macd": macd,
        "macd_signal": macd_signal,
        "macd_hist": macd_hist,
        "adx14": talib.ADX(high, low, close, ADX_P),
        "plus_di14": talib.PLUS_DI(high, low, close, ADX_P),
        "minus_di14": talib.MINUS_DI(high, low, close, ADX_P),
        "trange": talib.TRANGE(high, low, close),
        "stochf_k14": stoch_k,
        "obv": talib.OBV(close, volume),
        "ad": talib.AD(high, low, close, volume),
        "mfi14": talib.MFI(high, low, close, volume, MFI_P),
    }

    names = list(cols)
    with open(OUT_CSV, "w", newline="") as f:
        w = csv.writer(f)
        w.writerow(["index", *names])
        for i in range(len(close)):
            cells = []
            for n in names:
                v = cols[n][i]
                cells.append("" if (v is None or np.isnan(v)) else repr(float(v)))
            w.writerow([i, *cells])

    print(f"wrote {OUT_CSV} ({len(close)} rows, {len(names)} indicators)")


if __name__ == "__main__":
    main()
