# fugazi CLI

`fugazi` is the crate's command-line binary. It takes a strategy declared in
YAML, one or more CSV data series, and drives them through the same
[`PaperWallet`](README.md#strategies) the library exposes to Rust — for a
single backtest, for a spec-validation pass, or for a parameter-grid sweep.

Three subcommands:

- [`run`](#run) — backtest one strategy over one dataset. Writes `trades.csv`,
  `returns.csv`, and `metrics.yml` (or, with
  [`-w/--windowed`](#windowed-metrics), `metrics.csv`).
- [`check`](#check) — parse and validate a `strategy.yml` without running it.
- [`optimize`](#optimize) — sweep a strategy over a parameter grid in
  parallel; write one CSV row per combination and rank by a metric.

The subcommands share their input vocabulary — the `<STRATEGY>` positional,
`--series`, `--params`, the calendar shortcuts (`--stocks`/`--forex`/`--crypto`,
`--frequency`, `--bars-per-year`), `--risk-free-rate`, and the measurement
knobs (`-w/--windowed`, `--keep-unstable`). Everything that follows those
flags is documented once, in [Common flags](#common-flags).

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
cargo run --bin fugazi -- check @examples/strategy.yml

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

```
fugazi run <STRATEGY> --series <SPEC> [--series <SPEC> …] --output-dir <DIR>
          [--params <SPEC> …] [--cash <N>]
          [--stocks | --forex | --crypto] [-f <CODE>] [--bars-per-year <N>]
          [--risk-free-rate <RATE>] [-w <N>] [--keep-unstable] [-q]
```

| Flag | Description |
| --- | --- |
| `<STRATEGY>` | Positional. `@file.yml` loads a file; anything else is inline YAML. |
| `-s`, `--series <SPEC>` | Data series. Repeatable. See [--series](#--series). |
| `-o`, `--output-dir <DIR>` | Directory to write `trades.csv`, `returns.csv`, and `metrics.yml` into. Created if missing. |
| `-p`, `--params <SPEC>` | Placeholder substitution. Repeatable. See [--params](#--params). |
| `-c`, `--cash <N>` | Initial funds for the paper wallet. Default `10000`. |
| `--stocks` / `--forex` / `--crypto` | Trading-calendar shortcut. See [Calendar](#calendar-and-annualization). Mutually exclusive. |
| `-f`, `--frequency <CODE>` | Bar cadence (`1m`, `5m`, `1h`, `4h`, `1d`, `1w`, `1M`, …). Combines with the calendar to derive `bars_per_year`. |
| `--bars-per-year <N>` | Explicit override for the annualization denominator. Wins over the calendar/frequency pair. |
| `--risk-free-rate <RATE>` | Annualized risk-free rate as a fraction (`0.045` = 4.5% p.a.). Default `0`. See [Risk-free rate](#risk-free-rate). |
| `-w`, `--windowed <N>` | Compute the metrics in non-overlapping windows of `N` bars, writing `metrics.csv` (one row per window) instead of `metrics.yml`. See [Windowed metrics](#windowed-metrics). |
| `--keep-unstable` | Disable the default stability gating of entry signals and its metric anchor. See [Stability gating](#stability-gating). |
| `-q`, `--quiet` | Silence the console output. Files still get written. |

**Outputs.** Three files in `--output-dir`, all documented in
[Output files](#output-files):

- `trades.csv` — one row per fill.
- `returns.csv` — one row per bar (equity, per-bar return).
- `metrics.yml` — the reduced backtest report (with `-w/--windowed`,
  `metrics.csv` — one row per window — replaces it).

**Console output** (unless `-q`): a two-line banner, then blocks for
**inputs** (strategy, params, period, capital, output), **trades**
(each fill listed after the run completes), **result** (bars, trades, capital
before → after, start/finish timestamps + elapsed), and **metrics**
(the headline lines of `metrics.yml` — cross-window mean ± stddev under
`-w/--windowed` — prefixed by a `measured` line whenever the
[stability gate](#stability-gating) skipped leading bars).

### `check`

Parse a strategy spec (with `--params` substitution) and report whether it
is syntactically valid. No dataset, no wallet — a lint pass.

```
fugazi check <STRATEGY> [--params <SPEC> …] [-q]
```

| Flag | Description |
| --- | --- |
| `<STRATEGY>` | Positional. `@file.yml` or inline YAML — same shape as `run`. |
| `-p`, `--params <SPEC>` | Placeholder substitution. Repeatable. See [--params](#--params). Omitting a required placeholder is a check failure. |
| `-q`, `--quiet` | Suppress the "ok" message on success. Errors still print; exit code is unchanged (`0` ok, non-zero on failure). |

Exit code is what a CI job cares about: `0` = the strategy parsed and
built cleanly against the given params, non-zero = something's off.

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
               [-w <N> [-k <K>]] [--keep-unstable]
               [--cash <N>]
               [--stocks | --forex | --crypto] [-f <CODE>] [--bars-per-year <N>]
               [--risk-free-rate <RATE>] [-q]
```

| Flag | Description |
| --- | --- |
| `<STRATEGY>` | Positional. `@file.yml` or inline YAML. Same shape as `run`. |
| `-s`, `--series <SPEC>` | Data series. Repeatable. See [--series](#--series). |
| `-p`, `--params <SPEC>` | Baseline params **and** sweep-axis declarations. See [Sweep axes](#sweep-axes). Repeatable. |
| `-m`, `--metrics <NAMES>` | Metric columns to record. Comma-separated, repeatable. Short leaf names (`sharpe`, `max_pct`) or dotted paths (`risk_adjusted.sharpe`) — see the [Metrics catalogue](#metrics-catalogue). |
| `-o`, `--output <FILE>` | Output CSV path. Parent directories are created if missing. |
| `--best-by <METRIC>` | Sort rows by this metric (direction hardcoded per metric — see [Best-by directions](#best-by-directions)). Omit to keep cartesian order and skip the "best" console block. |
| `-w`, `--windowed <N>` | Evaluate each grid point in non-overlapping windows of `N` bars: every `-m` metric becomes two CSV columns (`<name>_mean` / `<name>_std`) and `--best-by` ranks by the windowed mean. See [Windowed metrics](#windowed-metrics). |
| `-k`, `--risk-aversion <K>` | Rank `--best-by` conservatively: shift each grid point's cross-window mean *against* it by `K` standard deviations before sorting. Requires `-w` and `--best-by`; `K >= 0`. See [Best-by directions](#best-by-directions). |
| `--keep-unstable` | Disable the default stability gating and its metric anchor for every grid point. See [Stability gating](#stability-gating). |
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

## Common flags

The flags below have the same shape across every subcommand that accepts
them (`run`, `optimize`, and — for `--params` and the strategy positional
— `check`).

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

`--params` resolves `!param` placeholders in the strategy YAML, and — on
`optimize` — also declares the sweep axes.

It's a `,`-separated list of terms, itself repeatable:

- `NAME=value` — set one placeholder. The value parses as a JSON scalar
  (`FAST=3` → number, `TRUE=true` → bool, `SYM=BTC` → string). On
  `optimize`, `NAME=[v1,v2,…]` or `NAME=start..end[:step]` declares a
  [sweep axis](#sweep-axes).
- `@file.yml` — load a whole `NAME: value` mapping. See
  [`examples/params.yml`](examples/params.yml).

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
seed for a while after (their *unstable period*). By default `run` and
`optimize` therefore **stability-gate** every entry signal: it is wrapped in
the library's `Stable` combinator, so no entry can fire until its whole
indicator chain has settled, and the metrics are **measured from the first
bar an entry could possibly fire on**. The gated prefix is provably flat
(nothing was at risk), so skipping it removes warm-up dilution from the
return moments without discarding any P&L. `trades.csv`, `returns.csv`, and
`equity.png` still cover the full run, and the console prints a `measured`
line showing the skip:

```
metrics
  measured 2024-01-10 → 2024-01-30 (21 of 30 bars; 9 stability-gated bars skipped)
```

`--keep-unstable` (on `run` and `optimize`) disables both the gate and the
skip, restoring the exact pre-gate behavior.

Notes:

- For purely windowed (FIR) strategies — SMA crossovers and the like — the
  gate coincides with ordinary warm-up: trades are identical either way and
  only the dead prefix leaves the metrics.
- Exits and protective levels are not gated (no position can exist before the
  first gated entry).
- A signal already wrapped in [`!stable`](#signals) is unaffected —
  double-gating is harmless.

### Windowed metrics

`-w/--windowed <N>` computes the metrics in **non-overlapping windows of `N`
bars** over the measured range instead of one whole-run reduction. Each
window is evaluated as a run of its own: its initial equity is the equity
marked on the bar before it, and only the fills booked inside it count — a
position carried across a window boundary shows up in the entering window as
an unmatched fill, the usual windowed-analysis convention. Keep `N` well
above the strategy's typical holding time if the trade statistics matter.

- On `run`: writes [`metrics.csv`](#output-files) instead of `metrics.yml`,
  and the console metrics block reports each figure's cross-window mean ±
  population standard deviation (a metric degenerate in some windows averages
  over the windows where it is defined; `—` when defined nowhere).
- On `optimize`: every `-m` metric becomes two CSV columns
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
`typical`, `median`, `open`, `low`). An omitted `source` defaults to
`close`. Unknown fields on a side are a hard error (catches typos like
`take_profitt`).

### Sources

Real-valued indicators, one YAML tag per fugazi constructor:

- **Candle leaves** (bare words): `close`, `high`, `low`, `open`, `volume`,
  `typical`, `median`.
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
- **Stability gate**: `!stable { source }` — `None` until the source's whole
  chain has settled (its `stable_period()`), then a pass-through. Converts
  the soft unstable period into hard warm-up.

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
- **Stability gate**: `!stable <signal>` — masks the signal (read as `false`)
  until its whole chain has settled, so nothing downstream acts on a
  seed-contaminated value. Entry signals get this automatically — see
  [Stability gating](#stability-gating).
- **Constants**: `!value <bool>`.

See [`examples/strategy.yml`](examples/strategy.yml) for a full SMA
crossover and [`examples/strategy.params.yml`](examples/strategy.params.yml)
for the parameterized version.

## Output files

All CSV files are `;`-delimited for Excel.

### `trades.csv` (from `run`)

One row per fill booked by the wallet — market entries, market exits, and
resting protective triggers alike.

| Column | Meaning |
| --- | --- |
| `time` | Bar timestamp the fill was booked on. |
| `symbol` | Instrument, per-fill (multi-symbol strategies stay correct). |
| `side` | `buy` or `sell`. |
| `units` | Fill size, in instrument units. |
| `price` | Fill price. Market orders fill at the next bar's `open`; protective legs fill at their trigger level (or the bar's `open` on a gap). |
| `kind` | `market`, `stop`, or `take_profit`. |

### `returns.csv` (from `run`)

One row per bar.

| Column | Meaning |
| --- | --- |
| `time` | Bar timestamp. |
| `equity` | Wallet equity marked to this bar's close. |
| `return` | Fractional return since the previous bar's equity (`0.05` = +5%). |

### `metrics.yml` (from `run`)

Reduced backtest report, grouped by theme. See the [Metrics
catalogue](#metrics-catalogue) for every field. Measured from the
[stability gate](#stability-gating)'s anchor onward.

### `metrics.csv` (from `run -w`)

One row per non-overlapping window of the measured range — replaces
`metrics.yml` under [`-w/--windowed`](#windowed-metrics).

| Column | Meaning |
| --- | --- |
| `window_start` / `window_end` | Times of the window's first and last bars (the last window may be shorter than `N`). |
| *(the full catalogue)* | One column per metric, named by its dotted `metrics.yml` path (`run.bars`, `returns.total_pct`, `risk_adjusted.sharpe`, …). A metric degenerate in a window is an empty cell, so every row shares the same fixed column set. |

### Optimize CSV (from `optimize`)

One row per grid point:

- **Axis columns** (sorted by axis name, in the order declared).
- **Metric columns** (in `-m` declaration order — the header uses the
  user-typed name, not the resolved dotted path). Under
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
| `average_bars` / `min_bars` / `max_bars` | Per-trade duration. |

## Examples

The `examples/` directory ships runnable strategy specs paired with the
data files that drive them:

- [`examples/strategy.yml`](examples/strategy.yml) — a complete
  SMA-crossover strategy, always-in-market long/short.
- [`examples/strategy.params.yml`](examples/strategy.params.yml) — the
  same strategy parameterized on `FAST`/`SLOW`/`SYM`.
- [`examples/params.yml`](examples/params.yml) — a `NAME: value` mapping
  loadable with `--params @examples/params.yml`.
- [`examples/candles.csv`](examples/candles.csv) — sample BTC candles
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

# Windowed run: per-window metrics.csv, console mean ± std across windows.
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
