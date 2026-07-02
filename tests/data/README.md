# Test fixtures — cross-validation of indicators and metrics

Two independent cross-checks, both fully offline, both able to skip when their
reference library isn't installed:

- `tests/talib_validation.rs` — indicators vs [TA-Lib](https://ta-lib.org/)
- `tests/metrics_validation.rs` — evaluation metrics
  (`src/metrics.rs`) vs [empyrical](https://github.com/quantopian/empyrical)

## Files

- `aapl_monthly.csv` — committed OHLCV input for the TA-Lib check
  (`date,open,high,low,close,volume`). A fixed monthly snapshot used purely
  as a shared input; the cross-check is valid because fugazi **and** TA-Lib
  consume this exact file. Replace it with your own export (same columns) for
  authoritative AAPL data if you like.
- `talib_expected.csv` — **generated**, not committed. One column of TA-Lib
  output per indicator, aligned row-for-row with the input.
- `metrics_returns.csv` — committed 252-bar per-bar returns series for the
  metrics check, produced by a fixed-seed numpy draw (see
  `tools/gen_metrics_returns.py`). Rerun only when the fixture needs to be
  replaced; commit the regenerated file alongside its updated
  `metrics_expected.csv`.
- `metrics_expected.csv` — **generated**, not committed. One row per
  `(metric, expected)` reference value computed by empyrical (plus a few
  manual numpy formulas for fields empyrical doesn't cover).

## Regenerating the reference values

Neither TA-Lib nor empyrical is a Rust dependency; they're external tools used
once to produce the fixtures. The `tools/environment.yml` env pulls the
TA-Lib C library + Python wrapper *and* empyrical from conda-forge — use conda,
mamba, or micromamba:

```sh
mamba env create -f tools/environment.yml   # or: conda env create -f ...
mamba run -n fugazi-talib python3 tools/gen_talib_fixtures.py
mamba run -n fugazi-talib python3 tools/gen_metrics_fixtures.py
cargo test --test talib_validation          # now actually compares
cargo test --test metrics_validation
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

### Metrics (`metrics_validation.rs` vs empyrical)

The generator produces 28 reference values covering `run.*`, `returns.*`,
`risk_adjusted.*` and `drawdown.*`. Each is checked to `1e-9` absolute (or
`1e-9` relative for large magnitudes) — an exact float agreement, since
fugazi and empyrical implement the same formulas over the same input. The
alignment relies on fugazi using sample stddev (Bessel-corrected, `ddof=1`)
for `stddev_bar`/annualized volatility and an `n`-divisor downside stddev
with `rf_per_bar` as the MAR threshold — the conventions empyrical uses.
Omega uses arithmetic per-bar rf as its threshold in both codebases (the
generator reproduces this rather than calling `empyrical.omega_ratio`, which
uses a geometric per-bar conversion). Trade-level metrics (payoff, streaks,
exposure, …) aren't part of this cross-check — they're covered by unit tests
inside `src/metrics.rs`.

### Indicators (`talib_validation.rs` vs TA-Lib)

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
`tests/talib_validation.rs` — keep them in sync. Same rule for the metrics
constants (`INITIAL_CASH`, `BARS_PER_YEAR`, `RISK_FREE_RATE`) which live in
both `tools/gen_metrics_fixtures.py` and `tests/metrics_validation.rs`.
