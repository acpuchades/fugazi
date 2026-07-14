# fugazi CLI

`fugazi` is the crate's command-line binary. It takes a strategy declared in
YAML, one or more CSV data series, and drives them through the same
[`PaperWallet`](README.md#strategies) the library exposes to Rust — for a
single backtest, for a spec-validation pass, or for a parameter-grid sweep.

Six subcommands:

- [`run`](#run) — backtest one strategy over one dataset. Writes `fills.csv`,
  `trades.csv`, `returns.csv`, and `metrics.yml`; adds `metrics.csv` +
  `rolling.csv` under [`-w/--windowed`](#windowed-metrics). No charts — plot
  post-hoc.
- [`check`](#check) — parse and validate a `strategy.yml` or `get --overlay`
  spec without running it. Nested: `check strategy` / `check overlay`.
- [`optimize`](#optimize) — sweep a strategy over a parameter grid in
  parallel; write one CSV row per combination and rank by a metric.
- [`get`](#get) — fetch OHLCV bars from a remote provider (`binance`,
  `yfinance`) or re-process a local CSV (`csv:PATH`) into a `run`-ready
  file, optionally with `-x/--overlay` columns computed on top.
- [`list`](#list) — printed catalogue of what the CLI knows about
  (indicator/signal YAML tags, remote providers, and — via HTTP — a
  provider's ticker vocabulary).
- [`completions`](#shell-completion) — emit a shell-completion script.

The subcommands share their input vocabulary — the `<STRATEGY>` positional,
`--series`, `--params`, the calendar shortcuts (`--stocks`/`--forex`/`--crypto`,
`--frequency`, `--bars-per-year`), `--risk-free-rate`, and the measurement
knob `-w/--windowed`. Everything that follows those flags is documented once,
in [Common flags](#common-flags).

## Install

The binary is a `[[bin]]` target of the library crate — no separate install.

```sh
cargo install fugazi                  # or, in a workspace,
cargo build --release                 # target/release/fugazi
cargo run --bin fugazi -- <subcommand> ...
```

### Shell completion

`fugazi completions <shell>` prints a completion script for `bash`, `zsh`,
`fish`, `elvish` or `powershell` to stdout. The zsh script teaches the shell
about the `@file` convention: `fugazi run @cand<TAB>` completes to
`@candles.csv`, and so does `--series symbol=BTC,@cand<TAB>` (the `key=value,`
and `@` prefixes are peeled before file completion runs). The other shells
currently get subcommand/flag completion only.

```sh
# zsh (drop it on $fpath and restart the shell)
fugazi completions zsh > "${fpath[1]}/_fugazi"

# bash
fugazi completions bash > /etc/bash_completion.d/fugazi   # or source per-session

# fish
fugazi completions fish > ~/.config/fish/completions/fugazi.fish
```

## Quick start

```sh
# Backtest the bundled SMA-crossover strategy on the bundled BTC candles.
cargo run --bin fugazi -- run \
    @examples/strategy.yml \
    --series @examples/candles.csv \
    --output-dir out/ \
    --crypto -f 1d

# Validate a strategy spec (no data needed).
cargo run --bin fugazi -- check strategy @examples/strategy.yml

# Fetch BTC daily bars into a `run`-ready CSV, adding an SMA-20 column.
cargo run --bin fugazi -- get binance:BTCUSDT[1d] \
    --since 2020-01-01 \
    -x 'sma20=!sma { period: 20 }' \
    -o btc.csv

# Sweep FAST / SLOW over a grid, rank by Sharpe.
cargo run --bin fugazi -- optimize \
    @examples/strategy.params.yml \
    --series @examples/candles.csv \
    --params 'FAST=3..15:1,SLOW=[20,50,100],SYM=BTC' \
    --metrics sharpe,sortino,cagr_pct,max_pct \
    --best-by sharpe \
    --crypto -f 1d \
    -o grid.csv
```

## Subcommands

### `run`

Backtest one strategy against one dataset and write the result files.
Every candle in the input series is fed to the strategy in `time` order;
the strategy's `symbol` (from its YAML spec) is what the driver filters
on when the frame carries several symbols.

```
fugazi run <STRATEGY> --series <SPEC> [--series <SPEC> …] --output-dir <DIR>
          [--params <SPEC> …] [--cash <N>] [--costs <SPEC> …]
          [--stocks | --forex | --crypto] [-f <CODE>] [--bars-per-year <N>]
          [--risk-free-rate <RATE>] [-w <LEN>] [-q]
```

| Flag | Description |
| --- | --- |
| `<STRATEGY>` | Positional. `@file.yml` loads a file; anything else is inline YAML. An optional shape prefix picks the strategy shape: `single:` (or no prefix) for a `SingleAssetStrategy`, `pairs:` for a two-leg `PairsStrategy`. See [Strategy shape prefix](#strategy-shape-prefix). |
| `-s`, `--series <SPEC>` | Data series. Repeatable. See [--series](#--series). |
| `-o`, `--output-dir <DIR>` | Directory to write `fills.csv`, `trades.csv`, `returns.csv`, and `metrics.yml` into (plus `metrics.csv` + `rolling.csv` under `-w`). Created if missing. Plain path — no interpolation. |
| `-p`, `--params <SPEC>` | Placeholder substitution. Repeatable. See [--params](#--params). |
| `-c`, `--cash <N>` | Initial funds for the paper wallet. Default `10000`. |
| `--costs <SPEC>` | Trading-cost model — commission, spread, slippage — applied to every fill. Repeatable. See [--costs](#--costs). Omit for a frictionless run (matches the pre-costs release byte-for-byte). |
| `--stocks` / `--forex` / `--crypto` | Trading-calendar shortcut. See [Calendar](#calendar-and-annualization). Mutually exclusive. |
| `-f`, `--frequency <[SYM:]CODE>` | Bar cadence (`1m`, `5m`, `1h`, `4h`, `1d`, `1w`, `1M`, …). Repeatable; may carry a `SYMBOL:` scope prefix. When omitted, the CLI auto-detects the cadence from the `time` column. Combines with the calendar to derive `bars_per_year`. |
| `--bars-per-year <[SYM[FREQ]:]N>` | Explicit override for the annualization denominator. Repeatable; each entry may carry a `SYMBOL[FREQ]:` scope prefix. Wins over the calendar/frequency pair when a scope matches. |
| `--risk-free-rate <RATE>` | Annualized risk-free rate as a fraction (`0.045` = 4.5% p.a.). Default `0`. See [Risk-free rate](#risk-free-rate). |
| `-w`, `--windowed <LEN>` | Also reduce the run in `LEN`-sized windows: one row per non-overlapping window in `metrics.csv`, one row per rolling (stride-1) window in `rolling.csv`. `metrics.yml` (whole-run) is always written; the console prints an extra **windowed metrics** block right after the whole-run one, showing `mean ± std` over the non-overlapping rows for the same headline stats. `LEN` is a plain bar count (`10`, `252`) or a duration in the [`-f`](#-f----frequency) alphabet (`1d`, `1w`, `1M`, `4h`) that resolves to a bar count against the trading calendar. The duration form is strict — it requires an explicit `--stocks`/`--forex`/`--crypto` and a resolvable bar cadence (`-f/--frequency`, or a `time` column so the cadence can be auto-detected). See [Windowed metrics](#windowed-metrics). |
| `-q`, `--quiet` | Silence the console output. Files still get written. |

**Outputs.** Files in `--output-dir`, all documented in
[Output files](#output-files):

- `fills.csv` — one row per booked order (wallet's operation log).
- `trades.csv` — one row per closed round-trip trade (entry → exit, realized P&L, holding period). Empty (header-only) for a run that never closes a position (e.g. buy-and-hold).
- `returns.csv` — one row per bar (equity, per-bar return).
- `metrics.yml` — the whole-run backtest report; always written.
- `metrics.csv` — one row per non-overlapping window (written only under `-w/--windowed <N>`).
- `rolling.csv` — one row per rolling window (written only under `-w/--windowed <N>`).

No charts are produced. Plotting is a post-hoc analysis on the CSVs —
see the README's *Analyzing a run in R* section.

#### Strategy shape prefix

The strategy positional accepts an optional shape prefix:

- `single:` (or no prefix) — a `SingleAssetStrategy` file
  (`single:@strategy.yml` ≡ `@strategy.yml`).
- `pairs:` — a two-symbol `PairsStrategy` file
  (`pairs:@spread.yml`); the document declares `left`/`right` symbols
  and cross-asset signal / level expressions rooted through
  `!pick { symbol, freq }` — see
  [Pairs documents](STRATEGIES.md#pairs-documents).
- `basket:` — an N-symbol cross-sectional `BasketStrategy` file
  (`basket:@basket.yml`); the document declares a `selection` rule plus
  per-symbol `score` / `sizing` templates, and the traded universe is
  whatever symbols the `--series` inputs carry — see
  [Basket documents](STRATEGIES.md#basket-documents). `fugazi optimize`
  doesn't support this shape yet.

Any other prefix is rejected as an unknown shape.

**Console output** (unless `-q`): a two-line banner, then blocks for
**inputs** (strategy, params, period, capital, output), **fills**
(each fill listed after the run completes), **result** (bars, fills, capital
before → after, start/finish timestamps + elapsed), and **metrics**
(the headline lines of `metrics.yml` — note the metrics block's `trades:`
line counts *closed round-trips*, matching `trades.csv`). Under
`-w/--windowed` a further **windowed metrics** block prints right after
**metrics**, showing `mean ± std` over the non-overlapping windows of
`metrics.csv` for the same
headline stats — so the whole-run point estimate and its cross-window
dispersion sit side-by-side. Metrics cover the whole run; the strategy
layer's readiness default (see [Stability gating](#stability-gating)) holds
the first trade until every source it consults is past its unstable tail —
no explicit gate on the entry is needed.

### `check`

Parse a spec and report whether it is syntactically valid — a lint pass, no
dataset, no wallet. Nested subcommand so each kind of spec (strategy YAML vs.
`get --overlay`) carries its own positional shape without leaking `only
applies when …` caveats:

```
fugazi check strategy <STRATEGY> [--params <SPEC> …] [-q]
fugazi check overlay <SPEC>... [-q]
fugazi check costs <SPEC>... [-q]
```

Exit code is what a CI job cares about: `0` = the spec parsed and built
cleanly, non-zero = something's off. In both forms `--quiet` suppresses the
"ok" success message but leaves error output and the exit code unchanged.

#### `check strategy`

| Flag | Description |
| --- | --- |
| `<STRATEGY>` | Positional. `@file.yml` or inline YAML — same shape as `run`. |
| `-p`, `--params <SPEC>` | Placeholder substitution. Repeatable. See [--params](#--params). Omitting a required placeholder is a check failure. |
| `-q`, `--quiet` | Suppress the "ok" message on success. Errors still print; exit code is unchanged (`0` ok, non-zero on failure). |

#### `check overlay`

Parses each `get --overlay` spec and builds a live indicator per column, so
an unknown `!tag`, a missing `period`, or a mis-scoped position leaf all
surface here — not at fetch time.

| Flag | Description |
| --- | --- |
| `<SPEC>...` | One or more `get --overlay` specs. Same shape as [`get --overlay`](#-x----overlay), including the optional `SYMBOL[FREQ]:` scope prefix. |
| `-q`, `--quiet` | Suppress the "ok" message on success. |

#### `check costs`

Parses each `run --costs` spec, folds them into a single [`CostConfig`], and
builds each configured leg's live model — so an unknown `!variant`, a missing
required field, a dotted setter naming the wrong variant, a malformed
`SYMBOL[FREQ]:` scope prefix, or a nested `composite`/`max` with a bad child
all surface here rather than at `run`/`optimize` startup.

| Flag | Description |
| --- | --- |
| `<SPEC>...` | One or more `--costs` specs. Same shape as [`run --costs`](#--costs), including the optional `SYMBOL[FREQ]:` scope prefix and the `none` literal. |
| `-q`, `--quiet` | Suppress the "ok" message on success. |

### `optimize`

Sweep a strategy over a parameter grid, one backtest per combination, in
parallel. Writes one CSV file with axis columns and metric columns. If
`--best-by` is given, rows are sorted and the winning combination is
printed.

```
fugazi optimize <STRATEGY> --series <SPEC> [--series <SPEC> …]
               --params <SPEC> [--params <SPEC> …]
               -m <METRIC>[,<METRIC>…] [-m <METRIC>…]
               -o <FILE> [--best-by <METRIC>] [-j <N>]
               [-w <LEN> [-k <K>]]
               [--cash <N>]
               [--stocks | --forex | --crypto] [-f <CODE>] [--bars-per-year <N>]
               [--risk-free-rate <RATE>] [-q]
```

| Flag | Description |
| --- | --- |
| `<STRATEGY>` | Positional. `@file.yml` or inline YAML. Same shape as `run`. |
| `-s`, `--series <SPEC>` | Data series. Repeatable. See [--series](#--series). |
| `-p`, `--params <SPEC>` | Baseline params **and** sweep-axis declarations. See [Sweep axes](#sweep-axes). Repeatable. |
| `-m`, `--metrics <NAMES>` | Metric columns to record. Comma-separated, repeatable. Short leaf names (`sharpe`, `max_pct`) or dotted paths (`risk_adjusted.sharpe`) — see the [Metrics catalogue](#metrics-catalogue). Column headers are always the canonical dotted path. **Optional** — omit to emit every catalogue metric as its own column. |
| `-o`, `--output <FILE>` | Output CSV path. Parent directories are created if missing. |
| `--best-by <METRIC>` | Sort rows by this metric (direction hardcoded per metric — see [Best-by directions](#best-by-directions)). Omit to keep cartesian order and skip the "best" console block. |
| `-w`, `--windowed <LEN>` | Evaluate each grid point in non-overlapping windows of `LEN`: every `-m` metric becomes two CSV columns (`<name>_mean` / `<name>_std`) and `--best-by` ranks by the windowed mean. Same `LEN` shape as `run -w` — a bar count (`10`, `252`) or a duration (`1d`, `1w`, `1M`); the duration form requires `--stocks`/`--forex`/`--crypto` and a resolvable bar cadence. See [Windowed metrics](#windowed-metrics). |
| `-k`, `--risk-aversion <K>` | Rank `--best-by` conservatively: shift each grid point's cross-window mean *against* it by `K` standard deviations before sorting. Requires `-w` and `--best-by`; `K >= 0`. See [Best-by directions](#best-by-directions). |
| `--costs <SPEC>` | Trading-cost model applied uniformly to every grid point. Repeatable. See [--costs](#--costs). |
| `-j`, `--jobs <N>` | Rayon worker count. Default: one worker per logical CPU. |
| `-c`, `--cash <N>` | Initial funds for each backtest. Default `10000`. |
| `--stocks` / `--forex` / `--crypto` | Trading-calendar shortcut. See [Calendar](#calendar-and-annualization). |
| `-f`, `--frequency <CODE>` | Bar cadence. |
| `--bars-per-year <N>` | Explicit annualization override. |
| `--risk-free-rate <RATE>` | Annualized risk-free rate as a fraction. Default `0`. |
| `-q`, `--quiet` | Silence the console output. CSV still gets written. |

#### Sweep axes

A `--params` term is a sweep axis when its value is a **list** or a **range**;
otherwise it's a fixed scalar shared by every grid point.

| Form | Example | Expands to |
| --- | --- | --- |
| List | `FAST=[3,5,8,13]` | `{3, 5, 8, 13}` |
| Integer range | `FAST=3..20:2` | `{3, 5, 7, …, 19}` (inclusive) |
| Float range | `K=0.5..2.0:0.5` | `{0.5, 1.0, 1.5, 2.0}` |
| Scalar (unchanged) | `SYM=BTC` | `{"BTC"}` — fixed, not an axis |

The range step is optional (`3..7` → step `1`). Ranges are inclusive on both
ends. A range whose step doesn't align with the endpoint stops at the last
value that still fits (`3..10:2` → `3, 5, 7, 9`). Every axis' cartesian
product is one grid point.

Axes are emitted as CSV columns **sorted by axis name** (stable regardless
of `--params` flag order), followed by the requested metric columns.

Passing `optimize --params` with no axis at all is an error — for a single
combination, use `run`.

#### Best-by directions

`--best-by` sorts the CSV (descending for max-oriented metrics, ascending
for min-oriented) and prints the winning row to the console. Directions
are hardcoded to the metric's canonical dotted path so a typo, an
ambiguous metric (`skewness`, trade counts), or one without a clear
direction errors out with a hint:

- **Maximize:** `returns.total`/`total_pct`/`cagr_pct`/`annualized_mean_pct`/
  `mean_bar`/`median_bar`/`best_bar`/`worst_bar`/`positive_bars_pct`/
  `tail_ratio`, `risk_adjusted.sharpe`/`sortino`/`calmar`/`omega`/
  `ulcer_performance_index`, `drawdown.recovery_factor`, `trades.win_rate_pct`/
  `profit_factor`/`payoff_ratio`/`expectancy`/`kelly_fraction`/`average_win`/
  `largest_win`/`average_loss`/`largest_loss`/`average_return_pct`,
  `run.final_equity`.
- **Minimize:** `returns.stddev_bar`/`annualized_volatility_pct`/`var_95`/
  `cvar_95`, `risk_adjusted.ulcer_index`, `drawdown.max`/`max_pct`/
  `max_duration_bars`/`avg`/`avg_pct`/`avg_duration_bars`/
  `time_in_drawdown_pct`.

Metrics that don't appear in the direction table (`returns.skewness`,
`returns.kurtosis`, all trade **counts**, distribution moments, calendar
inputs, …) can still be requested with `-m` — they just can't be passed
to `--best-by` because there's no unambiguous "better".

Under [`-w/--windowed`](#windowed-metrics), `--best-by` ranks by the metric's
**cross-window mean**, and `-k/--risk-aversion <K>` shifts that mean
*against* each grid point by `K` standard deviations before sorting —
`mean − K·std` for a maximize metric (a lower confidence bound),
`mean + K·std` for a minimize one — so a large spread is always penalized,
never rewarded, and `sharpe 2.0 ± 3.0` no longer outranks `1.8 ± 0.2`.
`K = 0` (the default) is the plain mean; negative `K` is rejected. The best
block prints the adjusted score next to the `mean ± std`. Caveat: a metric
defined in only one window has `std = 0` and ranks on its raw mean off a
single observation — check its `_std` column.

#### Overfitting and the train/validate workflow

**`optimize` on its own is a tuning aid, not a strategy validator.** It
picks the grid point that scores best on the data you gave it — a
metric-driven maximum-likelihood fit. Some of the "signal" it finds is
genuine edge; some is the peculiar noise of that window. The winner's
in-sample Sharpe is therefore a biased estimator of its out-of-sample
Sharpe (upwards, always), and the gap grows with the grid size, the
number of metrics you request, and the number of times you re-run the
sweep. Sharpe 2 on the training slice can be Sharpe 0.3 on the next year.
`-w`/`--windowed` + `-k`/`--risk-aversion` reduce the bias (they reward
parameter sets that hold up across regimes rather than in one lucky
stretch) but do not eliminate it — the grid is still ranked on the same
data it was fit on.

The recommended workflow is therefore an **explicit train / validate
split**, with `get` and `csv:` doing the plumbing:

```sh
# 1. Fetch the raw candles once, into a persistent CSV.
fugazi get binance:BTCUSDT[1d] --since 2018-01-01 --until today -o btc.csv

# 2. Split the CSV into a training slice (past) and a validation slice
#    (recent) with `csv:` + --since/--until. Nothing new is fetched.
fugazi get csv:./btc.csv --since 2018-01-01 --until 2023-01-01 -o btc_train.csv
fugazi get csv:./btc.csv --since 2023-01-01 --until today       -o btc_validate.csv

# 3. Optimize on the training slice. Prefer `-w` (+ optional `-k`) so the
#    ranking rewards parameter sets that held up across sub-windows of the
#    training period, not one lucky stretch of it.
fugazi optimize @strategy.params.yml \
    --series @btc_train.csv \
    --params 'FAST=3..20:2,SLOW=[20,50,100]' \
    -m sharpe,cagr_pct,max_pct --best-by sharpe \
    -w 252 -k 1.0 \
    --crypto -f 1d \
    -o grid_train.csv

# 4. Read the winning parameters off `grid_train.csv` (top row) and
#    `run` a single backtest with them on the *validation* slice —
#    also with `-w` so its Sharpe is comparable to the training figure.
fugazi run @strategy.params.yml \
    --series @btc_validate.csv \
    --params FAST=<top>,SLOW=<top> \
    -w 252 \
    --crypto -f 1d \
    --output-dir out/validate/
```

If the validation-slice metrics track the training ones, the edge probably
generalises; if they collapse (or flip sign), the tuning was fitting
noise. Keep the split boundary fixed once you've chosen it — re-sweeping
against different splits until one confirms the result is the same
overfitting under a different name.

### `get`

Fetch OHLCV bars into a `run`-ready `,`-delimited CSV. The header is
`symbol,freq,time,open,high,low,close,volume`, followed by one column per
`-x/--overlay` you request. `time` is ISO 8601 UTC.

```
fugazi get <SPEC> [<SPEC> …] -o <FILE>
          [--since <DATE>] [--until <DATE>]
          [-x <OVERLAY>]... [--params <SPEC> …] [--keep-unstable] [-q]
```

| Flag | Description |
| --- | --- |
| `<SPEC>...` | One or more fetch specs. Repeatable positional. See [Fetch specs](#fetch-specs). |
| `--since <DATE>` | Start date, inclusive. Grammar: `today`, `yesterday`, `Nd/Nw ago`, `YYYY-MM-DD`, `D-M-YYYY`, `3 weeks ago`, `last monday`, `Mar 1, 2020`, … Default `2020-01-01`. When set, extra leading bars are pulled ahead of `--since` so the overlays are already stable at the first output row. |
| `--until <DATE>` | End date, exclusive. Same grammar as `--since`. Default `today`. |
| `-o`, `--output <FILE>` | Output CSV path. Parent directories created if missing. |
| `-x`, `--overlay <SPEC>` | Extra column(s) computed on top of the fetched bars. Repeatable. See [`-x`/`--overlay`](#-x----overlay). |
| `-p`, `--params <SPEC>` | Resolve `!param` placeholders inside the `-x/--overlay` expressions. Repeatable. See [--params](#--params). Applies only to overlays — a fetch with no `-x` has nothing to substitute. |
| `--keep-unstable` | Emit the warm-up rows instead of dropping them. Overlay cells are blank where an applicable overlay has not yet warmed up. |
| `-q`, `--quiet` | Suppress the summary line. Errors still print. |

Every series across all specs downloads in parallel — one series is a
`(provider, symbol, interval)` triple with its own progress bar.

#### Fetch specs

The common shape is `<provider>:<symbol>[<freq>,<freq>...](,<symbol>[<freq>,...])*`
— several symbols and several frequencies per spec are one download. `csv:`
is the one exception; see below.

| Provider | Grammar | Description |
| --- | --- | --- |
| `binance` | `binance:BTCUSDT[1d,1h],ETHUSDT[1d]` | Binance spot klines. Frequencies: `1m`, `5m`, `15m`, `30m`, `1h`, `4h`, `1d`, `1w`, `1M`. |
| `yfinance` | `yfinance:SPY[1d],AAPL[1h]` | Yahoo Finance chart endpoint (stocks/ETFs/indices/FX). Rejects multiples the provider doesn't advertise (e.g. `Day(3)`). |
| `cg` | `cg:BTCUSDT=bitcoin[1d]` | **CoinGecko — overlay-only, no OHLCV.** Market cap / volume / supply columns; symbols are coin ids (`bitcoin`, not `BTC`). Join onto a price series via `--series`. Frequencies: `1h`…`12h`, `1d`, `1w`, `1M`. |
| `cmc` | `cmc:BTCUSDT=BTC[1d]` | **CoinMarketCap — overlay-only, no OHLCV.** Price / `volume_24h` / market cap / circulating & total supply; symbols are tickers (`BTC`) or numeric ids (`1`). **Paid tier required** (set `CMC_PRO_API_KEY`). Frequencies: `1h`/`2h`/`3h`/`4h`/`6h`/`12h`, `1d`, `1w`, `1M`. |
| `csv` | `csv:./candles.csv` | **No `[freq]` bracket.** Reads a local OHLCV CSV (delimiter autodetected: `;`, `,`, `\t`, `|`) — typically a previous `fugazi get` output. Each row's `symbol` + `freq` columns drive the output; `--since` / `--until` filter by `time`; overlays apply the same way. `symbol`, `freq`, `time`, `open`, `high`, `low`, `close` are required, `volume` optional. |

Frequency tokens are case-sensitive: `m` = minute, `M` = month. `fugazi list
sources` prints the same table.

#### `-x`/`--overlay`

An overlay declares extra CSV columns computed by feeding the fetched candles
through a live indicator built from the same YAML source vocabulary the
strategy YAML uses (`close`, `!sma { period: N }`, `!crosses_above { … }`,
…). Repeatable; later definitions override earlier ones per matching group.

Each `-x` argument is `[SCOPE:]BODY`, where:

- **Scope** (optional): `SYMBOL[FREQ]:`, `SYMBOL:`, or `[FREQ]:` — the
  overlay only runs for matching `(symbol, interval)` fetches. A missing
  component is a wildcard; no prefix is a global overlay covering every
  fetch. Cells outside the scope render blank.
- **Body**: either inline `col=expr[,col=expr,...]`
  (`sma20=!sma { period: 20 },ema50=!ema { period: 50 }`) or `@file.yml` — a
  YAML mapping of column name → source expression.

The base OHLCV column names (`open`, `high`, `low`, `close`, `volume`,
`symbol`, `freq`, `time`) are reserved.

An overlay body may carry `!param` placeholders resolved from `--params`,
exactly like a strategy document — so `--params FAST=20 -x 'ma=!sma { period:
!param FAST }'` parameterizes an overlay the same way it parameterizes a
strategy. See [--params](#--params).

Warm-up handling: unless `--keep-unstable` is set, each `(symbol, interval)`
group's leading unready rows are dropped (each overlay reaches its
`stable_period()` before its cell first prints a value); when `--since` is
set, extra leading bars are fetched (or read from the file) instead so the
first row at `--since` already has the overlays stable.
Validate an overlay spec without fetching via
[`check overlay`](#check-overlay).

**Examples**

```sh
# Fetch BTC daily bars and append SMA-20 / EMA-50 columns.
fugazi get binance:BTCUSDT[1d] --since 2020-01-01 \
    -x 'sma20=!sma { period: 20 },ema50=!ema { period: 50 }' \
    -o btc.csv

# Re-process an existing CSV to add an ATR column without re-downloading.
fugazi get csv:./btc.csv -x 'atr14=!atr { period: 14 }' -o btc_atr.csv

# Different overlays per symbol (BTC gets an EMA, ETH gets an RSI).
fugazi get binance:BTCUSDT[1d],ETHUSDT[1d] \
    -x 'BTCUSDT:ema=!ema { period: 20 }' \
    -x 'ETHUSDT:rsi=!rsi { period: 14 }' \
    -o out.csv

# Trailing strategy risk as an overlay column: the rolling 60-bar Sharpe of a
# strategy's own equity curve, computed inline. `!sharpe` embeds a whole
# single-asset strategy (here `!import`ed), drives it against a private paper
# wallet each bar, and reduces the equity curve to the rolling metric — no
# separate run + returns.csv + re-join.
fugazi get binance:BTCUSDT[1d] --since 2020-01-01 \
    -x 'sharpe60=!sharpe { strategy: !import ma_cross.yml, period: 60, bars_per_year: 365 }' \
    -o btc_regime.csv
```

**Trailing risk tags.** `!sharpe` / `!sortino` / `!volatility` / `!max_drawdown`
/ `!calmar` each take `{ strategy: <single-asset strategy document>, period,
bars_per_year }` (plus an optional `risk_free_rate`, default `0`, on Sharpe /
Sortino; `!max_drawdown` needs neither `bars_per_year` nor `risk_free_rate`).
The `strategy:` field is an ordinary single-asset strategy document — inline or
`!import`ed — and the `symbol:` inside it names the instrument the embedded
wallet prices, so it should match the series being fetched. A full-window
`period` reproduces the whole-run [`metrics.yml`](#metricsyml-from-run) number
for Sharpe / Sortino / volatility. These read a live equity curve, so they are
CLI/Rust-only — there is no Python binding (the strategy layer is not bound;
see [python/README.md](../python/README.md)).

### `list`

Printed catalogue, three shapes:

```
fugazi list indicators   # every YAML tag `run --series` and `get --overlay` accept
fugazi list sources      # every provider `get` fetches from (`binance`, `yfinance`, `csv`)
fugazi list tickers <PROVIDER> [PATTERN]   # every symbol the provider exposes (HTTP)
```

`list indicators` groups the vocabulary alphabetically (arithmetic, bands,
bar indicators, boolean logic, comparisons, constants, cross-timeframe
composition — `!resample` + `!latch`, which `check overlay` also validates
(missing `inner`, `every: 0`, and unknown nested tags all fail there) —
crossovers, MACD, moving averages, oscillators, placeholders, position
anchors, rolling extrema, stability gate, trend/directional). `list tickers
binance` calls
`/api/v3/exchangeInfo` and prints its full spot vocabulary — piped into
`grep`/`wc -l`/`sort -u` it's one ticker per line; interactive, it lays out
as a column-major grid sized to the terminal (like `ls`). Yahoo has no such
enumeration endpoint and returns an "unsupported" error; `csv` needs a path
per invocation, so the ticker list is whatever `symbol` values the file
itself contains — enumerate it with `cut -d';' -f1 <path> | sort -u`.

#### Filtering the ticker list

A provider's vocabulary runs to thousands of symbols (Binance ~1.4k, CoinGecko
~17.5k), so `list tickers` takes an optional **shell-style glob** and prints
only the symbols it matches:

```sh
fugazi list tickers binance 'b*'        # starts with b       (96 of 1357)
fugazi list tickers binance '*b*'       # contains b          (247 of 1357)
fugazi list tickers binance 'b*usd*t'   # b… then usd… then …t
fugazi list tickers binance '[a-c]*'    # starts with a, b or c
fugazi list tickers cg 'bitcoin-c*'
```

The alphabet is the familiar one: `*` any run of characters (including none),
`?` exactly one, `[abc]` / `[a-z]` a set or range, `[!abc]` / `[^abc]` its
complement, and `\*` a literal `*`. Matching is **case-insensitive** (the
provider picks the casing — Binance shouts `BTCUSDT`, CoinGecko whispers
`bitcoin` — and `b*` should mean the same to both) and **whole-symbol**, as a
glob does everywhere else: a bare `btc` matches the symbol `BTC` and nothing
else, and "contains" is spelled `*btc*`.

**Quote the pattern.** Unquoted, your shell expands it against your files first
— zsh will refuse outright with `no matches found: b*`.

The filter applies before the output split, so `list tickers binance 'b*' |
wc -l` counts exactly what the grid would have shown. On a terminal, a pattern
that matches nothing says so (and, when the pattern is anchored at both ends,
points at the `*…*` substring form) instead of printing a blank screen.

## Common flags

The flags below have the same shape across every subcommand that accepts
them (`run`, `optimize`, and — for `--params` and the strategy positional
— `check strategy`).

### `<STRATEGY>` (positional)

The strategy is the first positional argument, not a flag. It takes two forms:

- `@path/to/file.yml` — load the file.
- Anything else — inline YAML (block or flow style). Handy for one-offs:
  ```sh
  fugazi check '{ symbol: BTC, long: { enter: !crosses_above { lhs: !sma { period: 3 }, rhs: !sma { period: 8 } } } }'
  ```

The format is YAML. JSON is a subset of YAML, so a JSON-shaped document
still parses — a `!sma { … }` tag is just the singleton map
`{"sma": …}`. See [Strategy YAML reference](#strategy-yaml-reference).

### `--series`

Each `--series` describes one long-format table. It's a `,`-separated list
of terms:

- `key=value` — a **constant column**, broadcast across every loaded row
  (a literal wins a name clash within the same series).
- `@file.csv` — a **CSV file**. Its column delimiter (`;`, `,`, tab, or
  `|`) is autodetected from the header. Rows from several `@files` in one
  `--series` are concatenated.

Across multiple `--series` the tables are **full-outer joined on
`(symbol, time)`** into one long dataframe. `time` is compared as an
opaque, caller-sorted string (dates, epoch timestamps — anything).

Required columns after the join: `symbol`, `time`, `open`, `high`, `low`,
`close`. `volume` is optional (defaults to 0). Extra columns ride along
(you can join fundamentals or a per-symbol regime tag as another series).

**Examples**

```sh
# One file with its own `symbol` column.
--series @examples/candles.csv

# A symbol-less OHLCV file: broadcast the symbol as a literal.
--series 'symbol=BTC,@ohlcv.csv'

# Two files joined on (symbol, time) — candles + a fundamentals series.
--series @candles.csv --series @fundamentals.csv
```

### `--params`

`--params` resolves `!param` placeholders in the strategy YAML (and, on `get`,
in the `-x/--overlay` expressions), and — on `optimize` — also declares the
sweep axes.

It's a `,`-separated list of terms, itself repeatable:

- `NAME=value` — set one placeholder. The value parses as a JSON scalar
  (`FAST=3` → number, `TRUE=true` → bool, `SYM=BTC` → string). On
  `optimize`, `NAME=[v1,v2,…]` or `NAME=start..end[:step]` declares a
  [sweep axis](#sweep-axes).
- `@file.yml` — load a whole `NAME: value` mapping. See
  [`examples/params.yml`](../examples/params.yml).

Terms apply left-to-right, and later `--params` flags win over earlier
ones — so a base file + one override is a clean recipe:

```sh
fugazi run @examples/strategy.params.yml \
    --params @examples/params.yml,FAST=5 \
    --series @examples/candles.csv --output-dir out/
```

**Placeholders in the YAML** (`!param`):

```yaml
symbol: !param { key: SYM, default: BTC }      # optional, default BTC
long:
  enter: !crosses_above
    lhs: !sma { source: close, period: !param { key: FAST } }  # required
    rhs: !sma { source: close, period: !param { key: SLOW, default: 8 } }
```

`!param NAME` is bare-string shorthand for `!param { key: NAME }`. A `default`
makes the param optional; without one, a missing value is an error. Substitution
happens over the untyped YAML/JSON tree before the strategy is typed, so a
param can stand in for a number, a string, or any other field.

### `--costs`

`--costs` configures the trading-cost model applied to every fill: one
**commission**, one **spread** and one **slippage** leg, resolved per
`(symbol, frequency)` at run start. Omit the flag and the wallet is
frictionless — output is byte-identical to the pre-costs release, and the
console prints a one-line warning banner (`no cost model set — …`). Pass
`--costs none` to acknowledge the frictionless default explicitly and
silence the warning.

It's a `,`-separated list of terms, itself repeatable — same grammar
as [`--params`](#--params):

- `[SCOPE:]key=value` — set one leg (or nudge one field of it) inline.
  `key` starts with `commission` / `spread` / `slippage`; `value` is the
  model expression (`!percentage { rate: 0.001 }`, `!bps { bps: 5 }`, …).
  A `key` that reaches *into* a model addresses the spec tree literally, so
  it names the variant too — `commission.percentage.rate=0.00075`. See
  [Nudging one field](#nudging-one-field-of-a-preset).
- `@file.yml` — load a whole venue preset. Two ship in
  [`examples/`](../examples/): `binance.yml` (crypto taker fees) and
  `ibkr.yml` (US-equities Tiered).
- `none` — reset every leg to the no-op default and silence the warning
  banner. Any later term re-establishes a real model.

Terms apply left-to-right, later wins; the `SYMBOL[FREQ]:` scope prefix is
the same as [`get --overlay`](#-x----overlay) — either half is optional
(`BTC:`, `[1d]:`, `BTC[1d]:`).

A leading scope on the first inline term of one `--costs` flag distributes
over every later inline term in the same flag that doesn't carry its own
scope — `--costs 'BTC:commission=…,spread=…'` sets both legs for BTC.
Per-term scopes still override, and `@file` / `none` terms are unaffected.
To leave a term on the default leg while another is scoped, use two flags:
`--costs BTC:commission=… --costs spread=…`.

**Fill pipeline.** For every fill (market, stop, take-profit), the wallet
applies **spread → slippage → commission** on top of the theoretical trigger
price:

1. Start from the theoretical price — bar `open` for a market fill, the
   trigger level (or `open` on a gap) for a stop/take-profit.
2. Apply the half-spread — buys pay it (`+`), sells receive it (`−`).
3. Apply the slippage — always adverse to the *trading side* (buys slip
   up, sells slip down), scaled by the `stop_multiplier` on
   stop/take-profit fills (default `1.5×` for a triggered stop in a
   fast market).
4. The resulting price is what's stamped on the [`Order`](#output-files)
   and recorded as `fills.csv`'s `price` column.
5. Commission is computed from the *final* price × units and written to a
   separate `commission` column — never netted into `price`.

**Model catalogue** — a model is spelled as a `!variant { … }` YAML tag, in
a preset file and on the inline CLI form alike (the same tag vocabulary the
[strategy YAML](#strategy-yaml-reference) uses):

| Leg | Variant | Fields | Notes |
| --- | --- | --- | --- |
| commission | `fixed` | `amount` | Flat per-ticket. |
| commission | `percentage` | `rate` | `rate × notional`. |
| commission | `per_unit` | `rate` | `rate × units`. |
| commission | `composite` | `parts: [Model,…]` | Sum. |
| commission | `max` | `lhs`, `rhs` | `max(a, b)` — e.g. IBKR's per-order minimum. |
| spread | `bps` | `bps` | Basis-point full spread; half applied per side. |
| spread | `absolute` | `amount` | Absolute full spread; half per side. |
| slippage | `bps` | `bps`, `stop_multiplier` (opt.) | Fixed bps adverse. |
| slippage | `volume_participation` | `coefficient`, `exponent`, `stop_multiplier` (opt.) | Almgren-Chriss: `coef × (units/candle.volume)^exp × price`. `exponent` defaults to `0.5` (square-root). Zero-volume bars yield zero impact. |

**Scope precedence.** For each leg, resolution picks the winning model in
this order at run time:

1. An entry in the leg's `scoped` list that matches both `symbol` and
   `freq` (later-declared wins on same specificity).
2. `by_symbol[symbol]` — set via a `SYMBOL:` inline term or a preset
   `by_symbol` map.
3. `by_interval[freq]` — set via a `[FREQ]:` inline term or a preset
   `by_interval` map.
4. `default` — the leg's fallback.
5. Otherwise the no-op default (zero-cost).

**Preset shape** ([`examples/binance.yml`](../examples/binance.yml)):

```yaml
commission: !percentage           # flat form ⇒ commission.default
  rate: 0.001

spread:                           # structured: default + per-symbol overrides
  default: !bps { bps: 2 }
  by_symbol:
    BTCUSDT: !bps { bps: 1 }
    ETHUSDT: !bps { bps: 1.5 }

slippage: !volume_participation
  coefficient: 0.1
  exponent: 0.5
  stop_multiplier: 1.5
```

<a id="nudging-one-field-of-a-preset"></a>**Nudging one field of a preset.**
A dotted `key` is a literal address into that tree, and the model's variant
is a level of it — so nudging one field names the variant:

```sh
--costs @examples/binance.yml,commission.percentage.rate=0.00075
#                             └── leg ──┘└─ variant ─┘└ field ┘
```

You already have to know the variant to nudge it (`rate` exists on
`percentage` and `per_unit`, `amount` on `fixed`, …), so naming it costs
nothing and keeps the path honest. Point the wrong variant at a preset and
it's a hard error naming the model that's actually there, not a silent
half-application:

```console
$ fugazi check costs '@examples/ibkr.yml,commission.percentage.rate=0.001'
Error: cost setter `commission.percentage.rate`: the commission model is
`!max`, not `!percentage` — nudge a field `!max` actually has, or replace the
model outright with `commission=!percentage { … }`
```

**Inline forms**:

```sh
# One-shot: 10 bps taker + 5 bps spread.
fugazi run @strategy.yml -s @candles.csv -o out/ \
    --costs 'commission=!percentage { rate: 0.001 },spread=!bps { bps: 5 }'

# Load a preset, nudge one field.
fugazi run @strategy.yml -s @candles.csv -o out/ \
    --costs @examples/binance.yml,commission.percentage.rate=0.00075   # BNB discount

# Scoped override — a tighter spread for BTC on daily bars.
fugazi run @strategy.yml -s @candles.csv -o out/ \
    --costs 'BTCUSDT[1d]:spread=!bps { bps: 3 }'

# Silence the frictionless-run warning without setting a model.
fugazi run @strategy.yml -s @candles.csv -o out/ --costs none
```

**Reporting.** When a cost model is active, `fills.csv` gains a
`commission` column and `metrics.yml` a [`costs:` section](#costs-catalogue)
with `total_commission`, `total_slippage_cost`, and `cost_drag_pct` (gross
CAGR minus net CAGR). The console `metrics` block prints both the **gross**
(frictionless) and **net** (priced) `sharpe`/`cagr` side by side so the
cost drag is one line away.

**Caveats.**

- Sizing math is theoretical-price based: an all-in `value_frac(1.0)` fill
  under a non-trivial cost model overshoots funds by the cost overhead and
  is silently dropped (matching the wallet's queued-order semantics). Leave
  headroom by sizing under `1.0`.
- `volume_participation` is a **single-bar** approximation — the fill uses
  only its own bar's volume, no participation cap carries across bars.
  Not a full market-impact model. A stochastic / Monte Carlo variant is a
  natural follow-up and not part of this release.
- Bumping `stop_multiplier` above the default `1.5` widens the assumed
  slippage on triggered stops/take-profits; leave it at `1.0` to model
  stops as identical to market orders.

### Calendar and annualization

`metrics.yml` reports annualized figures (Sharpe, Sortino, CAGR, annualized
volatility). Those depend on how many bars a year the market produces, which
depends on the market **and** the bar cadence. Getting the constant wrong
doesn't fail the run — it silently misreports the annualized block. Two
shortcuts compose:

- **Calendar** — `--stocks` (US equities: 252 days × 6.5h), `--forex`
  (~260 weekdays × 24h), `--crypto` (365 days × 24h). Mutually exclusive.
- **Frequency** — `-f`/`--frequency <CODE>`: `N<unit>` where unit is one
  of `m` (minute), `h` (hour), `d` (day), `w` (week), `M` (month) and
  `N` is a positive integer (`1m`, `5m`, `15m`, `30m`, `1h`, `4h`, `1d`,
  `1w`, `1M`).

Explicit `--bars-per-year N` always overrides. Default: `252` (US-equity daily).

**Examples**

- Daily BTC bars: `--crypto -f 1d` → 365 bars/year.
- 1-hour SPY bars: `--stocks -f 1h` → 252 × 6.5 = 1638 bars/year.
- Custom cadence: `--bars-per-year 8760` (24×365).

### Risk-free rate

`--risk-free-rate <RATE>` is the annualized risk-free rate as a fraction —
`0.045` for 4.5% p.a. Default `0`. It is:

- **Subtracted** from the annualized mean return before Sharpe, Sortino, and
  UPI (all excess-return ratios).
- Used as the **per-bar threshold** for Omega (converted to per-bar by
  dividing by `bars_per_year`).

`0` gives the pre-adjusted excess-return semantics of the original release.

### Stability gating

Recursive (IIR-seeded) indicators — EMA, RSI, ATR, and everything built on
them — start emitting values at their warm-up but stay influenced by their
seed for a while after (their *unstable period*).

The default is **safe**: `SingleAssetStrategy::is_ready()` reports `true`
only once the strategy has been fed at least the largest `stable_period()`
across every wired signal (`enter` / `exit` on each side) *and* every
attached protective level, and `fugazi::backtest::run` skips `trade()` until
then. So the plain form —

```yaml
long:
  enter: !crosses_above { lhs: !ema { period: 12 }, rhs: !ema { period: 26 } }
```

— fires its first entry only once both EMAs are past their unstable tail; no
explicit gate on the signal is needed. Purely windowed (FIR) chains — SMA
crossovers and the like — have `unstable_period() = 0`, so the gate elapses
on the last warm-up bar and never lags them.

To **opt out** on a subtree, wrap it in `!unstable`: `!unstable { source: <s> }`
(real source) or `!unstable { signal: <s> }` (boolean signal) is a
passthrough that reports `unstable_period() = 0` for the wrapped subtree
while forwarding the underlying output. The readiness gate then only waits
for the wrapped chain's `warm_up_period()`. Use it when you're comfortable
trading through that subtree's IIR settling tail:

```yaml
long:
  enter: !crosses_above
    lhs: !unstable { source: !ema { period: 12 } }
    rhs: !unstable { source: !ema { period: 26 } }
```

`fugazi get`'s `--keep-unstable` flag is the same pattern one level up — the
overlay CSV trims each column's pre-`stable_period()` cells by default; the
flag opts out. See the library's [safe-defaults][safe-defaults] note.

[safe-defaults]: README.md#safe-defaults-opt-in-overrides

### Windowed metrics

`-w/--windowed <LEN>` reduces the run in **windows of `LEN`** on top of the
whole-run summary. `LEN` is either a plain bar count (`10`, `252`) or a
duration in the [`-f`](#-f----frequency) alphabet (`1d`, `1w`, `1M`, `4h`); a
duration resolves to a bar count against the trading calendar as
`win.trading_seconds / bar_freq.trading_seconds` — so `-w 1w` picks 5 bars
on daily equities and 7 on continuous crypto, `-w 1d` picks 7 bars on hourly
equities (one 6.5-hour trading day, not 24) and 24 on hourly crypto. The
duration form is deliberately strict: it requires an explicit
`--stocks`/`--forex`/`--crypto` (no silent Stocks default — a wrong calendar
here changes the window's bar count in ways that are hard to notice) and a
resolvable bar cadence (`-f/--frequency`, or a `time` column so the cadence
can be auto-detected). A bar cadence larger than the requested duration is
a hard error (would round to zero bars). Each window is evaluated as a run
of its own:
its initial equity is the equity marked on the bar before it, and only the
fills booked inside it count — a position carried across a window boundary
shows up in the entering window as an unmatched fill, the usual
windowed-analysis convention. Keep the resolved bar count well above the
strategy's typical holding time if the trade statistics matter.

- On `run`: `metrics.yml` (whole-run) is still written, and `-w N` adds
  **both** [`metrics.csv`](#output-files) (one row per non-overlapping window)
  and [`rolling.csv`](#output-files) (one row per rolling stride-1 window),
  same `N` for both. The two files share the same columns, so R/Python can
  consume them interchangeably — reach for `rolling.csv` when plotting a
  continuous curve (pyfolio-style rolling Sharpe / drawdown), `metrics.csv`
  when computing cross-window statistics (mean ± stddev, quantiles). Adjacent
  rolling rows share `N-1` bars, so the sample stddev on `rolling.csv` is
  meaningless as an uncertainty estimate — that's what non-overlapping
  windows are for.
- On `optimize`: only the non-overlapping variant is used — the
  `mean − k·std` ranker needs independent samples to interpret the stddev as a
  confidence adjustment. Every `-m` metric becomes two CSV columns
  (`<name>_mean` / `<name>_std`) and `--best-by` ranks by the windowed mean,
  optionally shifted by [`-k/--risk-aversion`](#best-by-directions).

Annualized figures over short windows are noisy (a 10-bar window annualizes a
tiny sample), so prefer raw per-window figures like `total_pct`, or pick `N`
large enough that each window holds a meaningful number of bars.

## Strategy YAML reference

A strategy is a `symbol` plus `long` and/or `short` sides. Each side has an
`enter` signal, an optional `exit` signal, and optional protective levels
`stop_loss` / `take_profit`. A side's `exit` defaults to never-fire — which
is exactly right for an always-in long/short reversal (the opposite side's
`enter` reverses the position). Give an `exit` only for a flat rest
(long/flat, or long/short with a pause).

### Strategy presets

Anywhere a single-asset strategy document is accepted — a top-level
`fugazi run @strat.yml`, or the `strategy:` field of a
[trailing risk tag](#-x----overlay) — you can name a ready-made recipe instead
of spelling out the sides. A preset builds the exact same strategy as its
`fugazi::strategies` Rust twin:

| Tag | Fields | Recipe |
|---|---|---|
| `!buy_and_hold` | `symbol` | all-in long on bar 1, hold |
| `!ma_crossover` | `symbol, fast, slow` | always-in SMA fast/slow crossover |
| `!rsi_reversal` | `symbol, period, oversold, exit` | RSI dip-buy, long/flat |
| `!donchian_breakout` | `symbol, period` | always-in Donchian channel breakout |
| `!keltner_breakout` | `symbol, ema_period, atr_period, multiplier` | always-in Keltner breakout |

```yaml
# strat.yml — a whole document is just the preset tag:
!ma_crossover { symbol: BTC, fast: 3, slow: 8 }
```

```sh
fugazi run @strat.yml -s @btc.csv -o out/ --crypto -f 1d
# or inline as a trailing tag's strategy:
fugazi get binance:BTCUSDT[1d] \
  -x 'sharpe=!sharpe { strategy: !buy_and_hold { symbol: BTCUSDT }, period: 60, bars_per_year: 365 }'
```

Presets are single-asset only, and are not swept by `optimize` (they carry no
`!param` axes — use a full spec for grids).

```yaml
symbol: BTC
long:
  enter: !crosses_above
    lhs: !sma { source: close, period: 3 }
    rhs: !sma { source: close, period: 8 }
  # optional
  exit: !crosses_below
    lhs: !sma { source: close, period: 3 }
    rhs: !sma { source: close, period: 8 }
  # optional protective legs — sources over the same Candle stream
  stop_loss: !sub { lhs: !entry, rhs: !mul { lhs: !atr { period: 14 }, rhs: !value 2.0 } }
  take_profit: !mul { lhs: !entry, rhs: !value 1.1 }
short:
  enter: !crosses_below
    lhs: !sma { source: close, period: 3 }
    rhs: !sma { source: close, period: 8 }
```

Sources and signals are written with YAML **tags** (`!sma { … }` is the SMA
indicator); candle-field leaves are bare words (`close`, `high`, `volume`,
`typical`, `median`, `open`, `low`). An omitted `source` on a
scalar-consuming indicator (`!sma`, `!ema`, `!rsi`, …) defaults to `close`.
Every **bar-consuming** tag (`!atr`, `!obv`, `!vwap`, `!ad`, `!mfi`, `!sar`,
`!adx`, `!plus_di`, `!minus_di`, `!dmi_plus_di`, `!dmi_minus_di`,
`!aroon_up`/`!aroon_down`/`!aroon_oscillator`, `!williams_r`,
`!keltner_upper`/`!keltner_middle`/`!keltner_lower`, `!true_range`,
`!resample`) accepts an optional `source:` field for the underlying candle
stream, defaulting to `!current` (the current bar); `!keltner_*` additionally
accepts an optional `candle_source:` for its ATR leg (also defaults to
`!current`). Unknown fields on a side are a hard error (catches typos like
`take_profitt`).

### Sources

Real-valued indicators, one YAML tag per fugazi constructor:

- **Candle leaves** (bare words): `close`, `high`, `low`, `open`, `volume`,
  `typical`, `median`. The whole current bar is `!current` (the default
  `source:` for every bar-consuming tag; name it explicitly to feed a
  higher-timeframe candle in via `!resample`).
- **Constants**: `!value <n>`.
- **Position anchors** (only inside a strategy — read the live position):
  `!entry`, `!peak`, `!trough`.
- **Moving averages**: `!sma`/`!ema`/`!rma`/`!wma`/`!hma { source, period }`.
- **Oscillators**: `!rsi { source, period }`, `!stddev { source, period }`,
  `!cci { period }`, `!stochastic { source, period }`, `!stoch_rsi
  { source, rsi_period, stoch_period }`, `!williams_r { period }`.
- **MACD** (each component is its own source): `!macd_line`,
  `!macd_signal`, `!macd_histogram { source, fast, slow, signal }`.
- **Bands**: `!bb_upper`/`!bb_middle`/`!bb_lower { source, period, k }`,
  `!keltner_upper`/`!keltner_middle`/`!keltner_lower { source, ema_period,
  atr_period, multiplier }`, `!donchian_upper`/`!donchian_middle`/
  `!donchian_lower { high, low, period }`.
- **Trend / directional**: `!adx`/`!plus_di`/`!minus_di { period }`,
  `!dmi_plus_di`/`!dmi_minus_di { period }`, `!aroon_up`/`!aroon_down`/
  `!aroon_oscillator { period }`, `!sar { step, max }`.
- **Bar indicators**: `!atr { period }`, `!mfi { period }`,
  `!true_range`, `!obv`, `!vwap`, `!ad`.
- **Arithmetic**: `!add`/`!sub`/`!mul`/`!div { lhs, rhs }`.
- **Lookback**: `!lag`/`!diff`/`!ratio`/`!roc { source, periods }`.
- **Rolling extremum**: `!rolling_max`/`!rolling_min { source, period }`.

### Signals

Boolean-valued nodes:

- **Comparisons** (`Real, Real → bool`, tolerance-aware; `epsilon` defaults
  to `1e-8`): `!gt`/`!lt`/`!ge`/`!le`/`!eq`/`!ne { lhs, rhs, epsilon? }`.
- **Level comparisons** (`Real, level → bool`): `!above`/`!below
  { source, level }`.
- **Crossovers**: `!crosses_above`/`!crosses_below { lhs, rhs }` — the
  comparison being true **and** the transition just happening.
- **Logic**: `!and`/`!or`/`!xor { lhs, rhs }`, `!all [signal, …]`,
  `!any [signal, …]`, `!not <signal>`, `!changed <signal>` (fires on any
  transition).
- **Unstable-tail override**: `!unstable { signal: <signal> }` — passthrough
  that reports `unstable_period() = 0` for the wrapped subtree while
  forwarding its output. The explicit opt-out to the safe-by-default
  strategy-readiness gate; see [Stability gating](#stability-gating).
  (`!unstable { source: <source> }` is the source-side twin.)
- **Constants**: `!value <bool>`.

See [`examples/strategy.yml`](../examples/strategy.yml) for a full SMA
crossover and [`examples/strategy.params.yml`](../examples/strategy.params.yml)
for the parameterized version.

### Reusing signals (YAML anchors)

A signal or level that appears in more than one place can be defined once
with a YAML anchor (`&name`) and reused with an alias (`*name`). Anchors
are a native YAML feature — the parser inlines each alias with the anchored
subtree before typed deserialization, so the strategy sees exactly the same
tree it would have without the anchors.

The one YAML rule is that `*name` must appear **after** `&name` in the
document. The natural pattern is to attach the anchor at the first use
site — the earliest field that references the subtree — and alias it from
every later site:

```yaml
symbol: BTC
long:
  enter: &cross_up !crosses_above { lhs: !sma { period: 3 }, rhs: !sma { period: 8 } }
  exit:  &cross_dn !crosses_below { lhs: !sma { period: 3 }, rhs: !sma { period: 8 } }
short:
  enter: *cross_dn
  exit:  *cross_up
```

Anchors compose with `!param`: the parser inlines aliases first, so a
`!param` inside an anchored subtree is substituted at every reuse site
in the same pass.

## Output files

All CSV files are `,`-delimited.

### `fills.csv` (from `run`)

One row per booked order — market entries, market exits, and resting
protective triggers alike. The wallet's raw operation log; each row is one
`Order` the strategy translated into.

| Column | Meaning |
| --- | --- |
| `time` | Bar timestamp the fill was booked on. |
| `symbol` | Instrument, per-fill (multi-symbol strategies stay correct). |
| `side` | `buy` or `sell`. |
| `units` | Fill size, in instrument units. |
| `price` | Fill price. Market orders fill at the next bar's `open`; protective legs fill at their trigger level (or the bar's `open` on a gap). With [`--costs`](#--costs) active, this is the *final* price — post-spread, post-slippage. |
| `kind` | `market`, `stop`, or `take_profit`. |
| `commission` | Commission paid on this fill, in reference currency (from the [`--costs`](#--costs) commission leg). **Only present when `--costs` is active.** Omitted otherwise so a zero-cost `fills.csv` matches the pre-costs schema byte-for-byte. |

### `trades.csv` (from `run`)

One row per **closed round-trip trade** — the same reduction the metrics
document uses ([`fugazi::metrics::reconstruct_trades`](../src/metrics.rs)),
so `trades.csv`'s row count matches `metrics.yml`'s `trades.total`.
Same-side fills roll into the open leg with a volume-weighted entry;
opposite-side fills close it (and, on a cross-zero reversal, re-open the
remainder as a fresh trade).

An open position at end of run has no exit and does not appear here — so a
strategy that never closes (e.g. buy-and-hold) produces a header-only
`trades.csv`. Use `fills.csv` for the full operation log.

| Column | Meaning |
| --- | --- |
| `entry_time` | Bar timestamp of the leg's opening (or last re-averaging) fill. |
| `exit_time` | Bar timestamp of the closing fill. |
| `side` | `long` (opened via a buy) or `short` (opened via a sell). |
| `units` | Leg magnitude, in instrument units. |
| `entry_price` | Volume-weighted average price of the opening leg. |
| `exit_price` | Fill price of the closing leg. |
| `pnl` | Realized P&L in reference (quote) currency. |
| `return` | P&L as a fraction of entry notional (`entry_price × units`); `0` when that notional is degenerate. |
| `bars_held` | `exit_bar − entry_bar` (`0` for a same-bar open+close). |

### `returns.csv` (from `run`)

One row per bar.

| Column | Meaning |
| --- | --- |
| `time` | Bar timestamp. |
| `equity` | Wallet equity marked to this bar's close. |
| `return` | Fractional return since the previous bar's equity (`0.05` = +5%). |

### `metrics.yml` (from `run`)

Reduced backtest report, grouped by theme, over the whole measured range.
Always written. See the [Metrics catalogue](#metrics-catalogue) for every
field.

### `metrics.csv` (from `run -w N`)

One row per **non-overlapping** window of `N` bars — written alongside
`metrics.yml` under [`-w/--windowed`](#windowed-metrics). Reach for this file
when computing cross-window statistics (mean ± stddev, quantiles): the
windows are independent, so the sample stddev is meaningful.

| Column | Meaning |
| --- | --- |
| `window_start` / `window_end` | Times of the window's first and last bars (the last window may be shorter than `N`). |
| *(the full catalogue)* | One column per metric, named by its dotted `metrics.yml` path (`run.bars`, `returns.total_pct`, `risk_adjusted.sharpe`, …). A metric degenerate in a window is an empty cell, so every row shares the same fixed column set. |

### `rolling.csv` (from `run -w N`)

Same shape as `metrics.csv` (identical columns), but one row per **rolling
stride-1** window of `N` bars: window `k` covers bars `[k, k+N)` for
`k ∈ [0, bars − N]`. Reach for this file when plotting a continuous curve
(pyfolio-style rolling Sharpe or drawdown). Adjacent rows share `N−1` bars,
so the rolling series is heavily autocorrelated — its sample stddev is a
plotting artefact, not an uncertainty estimate.

### Optimize CSV (from `optimize`)

One row per grid point:

- **Axis columns** (sorted by axis name, in the order declared).
- **Metric columns** (in `-m` declaration order, or catalogue order when
  `-m` is omitted — the header is always the canonical dotted path, so
  `-m sharpe` lands under `risk_adjusted.sharpe`). Under
  [`-w/--windowed`](#windowed-metrics), each metric becomes two columns —
  `<name>_mean` and `<name>_std`, its cross-window mean and population
  standard deviation over the windows where it is defined.

Missing metric values (`sharpe` on a run with zero variance,
`profit_factor` on a run with no losing trade, …) render as **empty
cells**. When `--best-by` is set, rows are sorted by that metric and empty
cells sink to the bottom regardless of direction.

## Metrics catalogue

`metrics.yml` (and the `optimize` metric columns) draws from four
sections. Ratios and averages whose denominator is degenerate are omitted
rather than emitted as `NaN`/`Infinity`. Any name in the tables below can
appear in `optimize -m`; short leaf names (`sharpe`, `max_pct`) work as
long as they're unambiguous.

### `run` — context

Non-metric inputs echoed at the top of the file.

| Field | Meaning |
| --- | --- |
| `bars` | Bar count the metrics were measured over — the run minus the [stability-gated](#stability-gating) prefix (and, windowed, this window's length). |
| `initial_equity` | Equity at the start of the measured range — the seed cash for a whole run, the prior bar's mark for a window. |
| `final_equity` | Ending equity (marked to the last bar's close). |
| `bars_per_year` | Annualization denominator used. |
| `risk_free_rate` | The annualized risk-free rate as a fraction. |

### `returns` — return distribution

| Field | Meaning |
| --- | --- |
| `total` | Total return as a fraction (`0.15` = +15%). |
| `total_pct` | Same, as a percent. |
| `cagr_pct` | Compound annual growth rate as a percent. Omitted for a non-positive equity path. |
| `mean_bar` / `median_bar` / `stddev_bar` | Per-bar return moments. |
| `best_bar` / `worst_bar` | Max / min per-bar return. |
| `positive_bars_pct` | Percentage of bars with a strictly positive return. |
| `skewness` / `kurtosis` | Distribution shape (biased skew and excess kurtosis; `kurtosis = 0` for a normal). Omitted when stddev is zero. |
| `var_95` / `cvar_95` | Historical 5%-VaR and 5%-CVaR (Expected Shortfall) as positive loss fractions. |
| `tail_ratio` | `|P95(returns)| / |P5(returns)|`. Omitted when P5 magnitude is zero. |
| `annualized_mean_pct` | `mean_bar × bars_per_year × 100`. |
| `annualized_volatility_pct` | `stddev_bar × √bars_per_year × 100`. |

### `risk_adjusted` — headline ratios

| Field | Meaning |
| --- | --- |
| `sharpe` | `(ann_return − risk_free) / ann_vol`. Omitted when vol is zero. |
| `sortino` | `(ann_return − risk_free) / ann_downside_dev`. |
| `calmar` | `CAGR / max_drawdown`. |
| `omega` | Omega ratio at the per-bar risk-free threshold. |
| `ulcer_index` | Peter Martin's Ulcer Index (root-mean-squared drawdown, fractional). |
| `ulcer_performance_index` | `(CAGR − risk_free) / ulcer_index`. |

### `drawdown` — peak-to-trough analytics

| Field | Meaning |
| --- | --- |
| `max` / `max_pct` | Worst peak-to-trough drop (fractional / percent). |
| `max_duration_bars` | Bars from the peak to that trough. |
| `avg` / `avg_pct` | Mean drawdown depth across all segments. |
| `avg_duration_bars` | Mean peak-to-trough bars. |
| `count` | Segment count. |
| `time_in_drawdown_pct` | Percentage of bars the curve was below a prior peak. |
| `recovery_factor` | `total_return / max_drawdown`. |

### `trades` — round-trip statistics

Trades are reconstructed by walking the fill blotter with a signed
position and a volume-weighted entry price — one reversal counts as one
close + one fresh open, matching how `SingleAssetStrategy` reasons about
positions.

| Field | Meaning |
| --- | --- |
| `total` / `wins` / `losses` / `flat` | Closed-trade counts by outcome. |
| `long_trades` / `short_trades` | By initial side. |
| `total_fills` | Blotter length (fills, not trades). |
| `max_consecutive_wins` / `max_consecutive_losses` | Longest streaks. |
| `exposure_pct` | Percentage of bars a non-zero position was held. |
| `win_rate_pct` / `profit_factor` / `payoff_ratio` / `expectancy` / `kelly_fraction` | Round-trip PnL ratios. |
| `average_win` / `average_loss` / `largest_win` / `largest_loss` / `average_return_pct` | Per-trade PnL. |
| `average_bars` / `min_bars` / `max_bars` | Per-trade duration in bar counts. |
| `average_seconds` / `min_seconds` / `max_seconds` | Same in trading seconds — the `_bars` figures scaled by `trading_seconds_per_bar(class, freq)`. Populated only when both an asset-class shortcut (`--stocks`/`--forex`/`--crypto`) and a bar cadence are known; omitted otherwise. Divide by 3600 for hours, 86400 for days. |

### `costs` — cost aggregates

Present **only when [`--costs`](#--costs) was active** on the run — omitted
from the document (and read as an empty CSV cell in `optimize`'s window
columns) otherwise, so a zero-cost `metrics.yml` matches the pre-costs
schema byte-for-byte.

| Field | Meaning |
| --- | --- |
| `total_commission` | Sum of every fill's `commission`, in reference currency. |
| `total_slippage_cost` | Sum of `|final_price − theoretical_price| × units` across every fill — the aggregate spread + slippage the wallet took out of the run. Derived by re-running the same strategy zero-cost and diffing the fill prices. |
| `cost_drag_pct` | Gross CAGR minus net CAGR, in percentage points. Omitted when either endpoint's CAGR is degenerate (non-positive equity). |

## Examples

The `examples/` directory ships runnable strategy specs paired with the
data files that drive them:

- [`examples/strategy.yml`](../examples/strategy.yml) — a complete
  SMA-crossover strategy, always-in-market long/short.
- [`examples/strategy.params.yml`](../examples/strategy.params.yml) — the
  same strategy parameterized on `FAST`/`SLOW`/`SYM`.
- [`examples/params.yml`](../examples/params.yml) — a `NAME: value` mapping
  loadable with `--params @examples/params.yml`.
- [`examples/candles.csv`](../examples/candles.csv) — sample BTC candles
  with a `symbol` column.

**Common recipes**

```sh
# Single-run backtest with the parameterized strategy and a base params file.
fugazi run @examples/strategy.params.yml \
    --series @examples/candles.csv \
    --output-dir out/ \
    --params @examples/params.yml \
    --crypto -f 1d

# Ad-hoc override — later --params terms win.
fugazi run @examples/strategy.params.yml \
    --series @examples/candles.csv \
    --output-dir out/ \
    --params @examples/params.yml,FAST=5,SLOW=15 \
    --crypto -f 1d

# Optimize over one integer and one list axis, rank by Sortino.
fugazi optimize @examples/strategy.params.yml \
    --series @examples/candles.csv \
    --params 'FAST=3..10:1,SLOW=[20,50,100],SYM=BTC' \
    -m sharpe,sortino,cagr_pct,max_pct \
    --best-by sortino \
    --crypto -f 1d \
    -o grid.csv

# Windowed run: keeps metrics.yml and also writes metrics.csv (non-overlapping
# 10-bar windows) + rolling.csv (rolling stride-1 10-bar windows) for R/Python.
fugazi run @examples/strategy.yml \
    --series @examples/candles.csv \
    --output-dir out/ \
    --crypto -f 1d \
    -w 10

# Consistency-aware sweep: rank by windowed Sharpe, one sigma conservative.
fugazi optimize @examples/strategy.params.yml \
    --series @examples/candles.csv \
    --params 'FAST=3..10:1,SLOW=[20,50,100],SYM=BTC' \
    -m sharpe,max_pct \
    --best-by sharpe \
    -w 10 -k 1 \
    --crypto -f 1d \
    -o grid.csv

# Lint a spec in CI.
fugazi check @strategies/my_strategy.yml --params ENV=prod
```
