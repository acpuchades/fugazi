# Test fixtures — TA-Lib cross-validation

`tests/talib_validation.rs` checks fugazi's indicators against
[TA-Lib](https://ta-lib.org/) reference values, fully offline.

## Files

- `aapl_monthly.csv` — committed OHLCV input (`date,open,high,low,close,volume`).
  A fixed monthly snapshot used purely as a shared input; the cross-check is
  valid because fugazi **and** TA-Lib consume this exact file. Replace it with
  your own export (same columns) for authoritative AAPL data if you like.
- `talib_expected.csv` — **generated**, not committed. One column of TA-Lib
  output per indicator, aligned row-for-row with the input.

## Regenerating the reference values

TA-Lib isn't a Rust dependency; it's an external tool used once to produce the
fixtures. The `tools/environment.yml` env pulls the TA-Lib C library and Python
wrapper from conda-forge — use conda, mamba, or micromamba:

```sh
mamba env create -f tools/environment.yml   # or: conda env create -f ...
mamba run -n fugazi-talib python3 tools/gen_talib_fixtures.py
cargo test --test talib_validation          # now actually compares
```

`mamba run -n <env> …` runs the generator with the env's Python without needing
to `activate` it first (equivalently: `conda activate fugazi-talib` then run
`python3 tools/gen_talib_fixtures.py`). If your shell wrapper can't run
`mamba run`, call the env's interpreter directly:
`"$(mamba env list | awk '/fugazi-talib/{print $NF}')"/bin/python3
tools/gen_talib_fixtures.py`.

(Or with pip, if the TA-Lib C library is already installed: `pip install TA-Lib
numpy`, then `python3 tools/gen_talib_fixtures.py`.)

The test **skips** (printing these steps) when `talib_expected.csv` is absent
*or* stale — i.e. missing a column for a newly added indicator — so `cargo test`
stays green for contributors who don't have TA-Lib. Regenerate the fixture after
adding any indicator to the generator.

## What is compared, and tolerances

Indicators whose conventions match TA-Lib exactly are checked to `1e-6` over
every warmed-up bar: **SMA, RSI, STDDEV, BBANDS (upper/mid/lower), MAX, MIN**
(MAX/MIN of high/low are the Donchian channel), **TRANGE**, the fast stochastic
**%K** (`STOCHF`, fed `close` as high/low/close so it positions the close within
its own rolling range, matching fugazi's `Stochastic` scaled to `[0, 100]`), and
the volume indicators **OBV, AD** (Chaikin A/D line) and **MFI** — cumulative or
windowed sums with no recursive seed. (**VWAP** has no TA-Lib function, so it is
covered by unit tests only, not this cross-check.)

**EMA**, **ATR**, **MACD** (line/signal/histogram) and **ADX** (with +DI/-DI)
differ only in *seeding*: fugazi seeds each recurrence from the first sample(s),
while TA-Lib uses a different seed (an SMA for EMA-family, summed Wilder state
for ADX-family). That difference decays geometrically, so these are checked with
a looser tolerance over the tail of the series only.

Indicator parameters live in both `tools/gen_talib_fixtures.py` and
`tests/talib_validation.rs` — keep them in sync.
