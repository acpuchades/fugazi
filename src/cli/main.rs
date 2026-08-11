//! `fugazi` — a command-line backtester for the fugazi library.
//!
//! Load a strategy from a `strategy.yml`, feed it candle (and arbitrary extra)
//! data assembled from one or more `--series`, and run it through a paper wallet,
//! writing `fills.csv`, `trades.csv`, `returns.csv` and `metrics.yml`:
//!
//! ```text
//! fugazi run @strategy.yml \
//!            --series @candles.csv \
//!            --output-dir out/
//! ```
//!
//! The strategy (a positional) takes `@file` to load a file, or inline YAML for
//! anything else — the same `@` convention `--series`/`--params` use.

mod completions;
mod csv_source;
mod data;
mod get;
mod glob;
mod list;
mod optimize;
mod overlay;
mod run;
mod style;

// Re-export spec vocabulary into the binary crate's namespace so the existing
// `crate::foo::bar` references in the remaining CLI files continue to resolve.
// The moved modules now live under `fugazi::spec::*` on the library; these
// re-exports let this binary keep its historical short paths.
pub(crate) use fugazi::spec as spec;
pub(crate) use fugazi::spec::calendar;
pub(crate) use fugazi::spec::costs;
pub(crate) use fugazi::spec::dyn_indicator;
pub(crate) use fugazi::spec::imports;
pub(crate) use fugazi::spec::input;
pub(crate) use fugazi::spec::metrics;
pub(crate) use fugazi::spec::params;
pub(crate) use fugazi::spec::backtest;

use std::collections::HashMap;
use std::path::PathBuf;

use anyhow::{Context, Result};
use clap::{Args, Parser, Subcommand};
use clap_complete::Shell;

use input::{Source, StrategyKind, StrategySource};

/// Incremental technical-analysis backtester.
#[derive(Parser)]
#[command(name = "fugazi", version, about)]
pub(crate) struct Cli {
    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand)]
enum Command {
    /// Run a `strategy.yml` backtest over CSV series.
    Run(RunArgs),
    /// Parse a spec and report whether it is syntactically valid.
    ///
    /// `fugazi check strategy <STRATEGY>` validates a strategy spec's shape
    /// (an unset required `--params` placeholder is held as a typed hole, since
    /// `check` never builds or drives the strategy); `fugazi check overlay
    /// <SPEC>...` validates one or more `get --overlay` specs; `fugazi check
    /// costs <SPEC>...` validates one or more `run --costs` specs.
    #[command(subcommand_required = true, arg_required_else_help = true)]
    Check {
        #[command(subcommand)]
        cmd: CheckCmd,
    },
    /// Sweep a strategy over a parameter grid and rank the combinations.
    Optimize(OptimizeArgs),
    /// Fetch OHLCV candles from remote providers into a `run`-ready CSV.
    ///
    /// Spec grammar: `<provider>:[OUT=]<symbol>[<freq>,<freq>...](,[OUT=]<symbol>[<freq>...])*`;
    /// several specs may be given and all series download in parallel. A `=`
    /// inside a symbol escapes as `\=`.
    /// Example: `fugazi get binance:BTCUSDT[1d,1h],ETHUSDT[1d] yfinance:AAPL[1d] --since 2020-01-01 --until today -o candles.csv`.
    Get(get::GetArgs),
    /// Print a shell-completion script for the given shell to stdout.
    ///
    /// Install into zsh with e.g.:
    /// `fugazi completions zsh > "${fpath[1]}/_fugazi"` (then restart the shell).
    /// The zsh output teaches the shell about the `@file` convention so
    /// `fugazi run @cand<TAB>` completes to `candles.csv`; the other shells
    /// currently get subcommand/flag completion only.
    Completions {
        /// Target shell (`bash`, `zsh`, `fish`, `elvish`, `powershell`).
        shell: Shell,
    },
    /// Print a printed catalogue of what the CLI knows about.
    ///
    /// `fugazi list indicators` enumerates the strategy-YAML tag vocabulary
    /// (real-valued sources, boolean signals, the `!param` placeholder);
    /// `fugazi list sources` enumerates the remote candle providers the `get`
    /// subcommand can fetch from; `fugazi list tickers <provider> [PATTERN]`
    /// fetches and prints every symbol the given provider offers, optionally
    /// filtered by a shell-style glob — `fugazi list tickers binance 'b*'`
    /// (starts with `b`), `'*b*'` (contains `b`). Quote the pattern so the
    /// shell doesn't expand it against your files first.
    List {
        #[command(subcommand)]
        cmd: list::ListCmd,
    },
}

#[derive(Args)]
struct RunArgs {
    /// The strategy: `@file.yml` loads a file, anything else is inline YAML.
    /// May carry a leading shape prefix: `single:` (or none) for a
    /// `SingleAssetStrategy`, `pairs:` for a two-leg `PairsStrategy`.
    #[arg(value_name = "STRATEGY")]
    strategy: StrategySource,

    /// A data series: `,`-separated `key=value` literals and `@file.csv` loaders
    /// (repeatable; series full-join on `symbol` + `time`). Each file's column
    /// delimiter is autodetected.
    #[arg(short, long = "series", required = true)]
    series: Vec<data::SeriesSpec>,

    /// Directory to write `fills.csv`, `trades.csv`, `returns.csv`, and
    /// `metrics.yml` into.
    #[arg(short, long = "output-dir")]
    output_dir: PathBuf,

    /// Initial cash for the paper wallet.
    #[arg(short, long, default_value_t = 10_000.0)]
    cash: f64,

    /// Resolve the strategy's `param` placeholders. Like `--series`: a
    /// `,`-separated list of `NAME=value` settings and `@file.yml` mapping loaders
    /// (repeatable; later terms win), e.g. `@base.yml,FAST=3`.
    #[arg(short, long = "params", value_name = "SPEC")]
    params: Vec<params::ParamSpec>,

    /// US-equity trading calendar (252 trading days a year, 6.5-hour day).
    /// Combines with `--frequency` to derive `bars_per_year`; `--bars-per-year`
    /// overrides. Mutually exclusive with `--forex`/`--crypto`.
    #[arg(long, group = "asset_class")]
    stocks: bool,

    /// Forex trading calendar (~260 weekdays a year, 24-hour day). Combines
    /// with `--frequency`; `--bars-per-year` overrides.
    #[arg(long, group = "asset_class")]
    forex: bool,

    /// 24/7 trading calendar (365 days a year, 24-hour day; crypto). Combines
    /// with `--frequency`; `--bars-per-year` overrides.
    #[arg(long, group = "asset_class")]
    crypto: bool,

    /// Bar cadence as `N<unit>` (e.g. `5m`, `4h`, `1d`, `1w`, `1M`). Unit is
    /// one of `m` minute, `h` hour, `d` day, `w` week, `M` month; `N` is a
    /// positive integer multiplier. Combined with `--stocks`/`--forex`/
    /// `--crypto` to derive `bars_per_year`; `--bars-per-year` overrides.
    ///
    /// Repeatable, and each entry may carry a `SYMBOL:` scope prefix —
    /// `-f 1d -f BTC:4h` — so a preset can pre-declare per-symbol cadences.
    /// At run time the symbol-scoped entry matching the strategy's symbol
    /// wins; the unscoped default applies otherwise. Omit entirely and the
    /// CLI auto-detects the cadence from the input series' `time` column
    /// (median gap snapped to a named cadence). The effective cadence —
    /// scope match, plain override, or detected — is used for both
    /// annualization *and* freq-scoped `--costs` matching.
    #[arg(short, long, value_name = "[SYM:]CODE")]
    frequency: Vec<calendar::ScopedFrequency>,

    /// Explicit `bars_per_year` for the annualization step in `metrics.yml`
    /// (Sharpe/Sortino/CAGR/annualized volatility). Overrides the value
    /// derived from `--stocks`/`--forex`/`--crypto` + `--frequency`.
    ///
    /// Repeatable, and each entry may carry a `SYMBOL[FREQ]:` scope prefix —
    /// `--bars-per-year 252 --bars-per-year BTC[1h]:8760` — so a preset can
    /// pre-declare per-series overrides. At run time the entry with the
    /// highest scope specificity matching the strategy's (symbol, effective
    /// freq) wins (`SYM[FREQ]` > `SYM` > `[FREQ]` > default, ties break to
    /// the last-declared). When no entry matches, the CLI auto-detects the
    /// bar cadence from the median gap in the input `time` column and pairs
    /// it with the calendar (default `--stocks`, 252 trading days a year).
    #[arg(long, value_name = "[SYM[FREQ]:]N")]
    bars_per_year: Vec<calendar::BarsPerYearSpec>,

    /// Annualized risk-free rate as a fraction (e.g. `0.045` = 4.5% p.a.).
    /// Subtracted from the annualized mean return before Sharpe/Sortino/UPI,
    /// and used as the per-bar threshold for Omega. Default 0 — the
    /// pre-adjusted excess-return semantics of the original release.
    #[arg(long, value_name = "RATE", default_value_t = 0.0)]
    risk_free_rate: f64,

    /// Also reduce the run in windows for post-hoc analysis. `metrics.yml`
    /// (whole-run) is always written; adding `-w LEN` writes two extra CSVs
    /// at window length `LEN` — `metrics.csv` (non-overlapping windows, one
    /// row each) and `rolling.csv` (rolling stride-1 windows, one row each).
    /// Both share the same columns as `metrics.yml` under their dotted names,
    /// with the window's start/end times in the first two columns. Plot from
    /// R/Python; no charts are produced.
    ///
    /// `LEN` is either a plain bar count (`10`, `252`) or a duration in the
    /// `-f/--frequency` alphabet (`1d`, `1w`, `1M`, `4h`) that resolves to a
    /// bar count against the trading calendar — `-w 1w` picks 5 bars on daily
    /// equities, 7 on continuous crypto; `-w 1d` picks 7 bars on hourly
    /// equities (one 6.5-hour trading day) and 24 on hourly crypto. The
    /// duration form requires `--stocks`/`--forex`/`--crypto` and a
    /// resolvable bar cadence (`-f/--frequency`, or a `time` column so the
    /// cadence can be auto-detected).
    #[arg(short = 'w', long = "windowed", value_name = "LEN")]
    windowed: Option<calendar::WindowSpec>,

    /// Configure trading costs (commission, spread, slippage). Same shape as
    /// `--params`: `,`-separated terms `[SCOPE:]key=value` and `@file.yml`
    /// preset loaders (repeatable; later terms win, more-specific scopes win
    /// over less-specific). `--costs none` acknowledges the frictionless
    /// default and silences the "no cost model set" warning. Omit for a
    /// zero-cost backtest (matches the pre-costs release byte-for-byte).
    #[arg(long = "costs", value_name = "SPEC")]
    costs: Vec<costs::CostSpec>,

    /// Suppress all console output (the result files are still written).
    #[arg(short, long)]
    quiet: bool,

    /// After the run, write the strategy + wallet state to this JSON file so a
    /// later `--resume` continues where this run left off. Open positions are
    /// kept (not realized). Mutually exclusive with `--realize-open`.
    #[arg(long = "save-state", value_name = "FILE", conflicts_with = "realize_open")]
    save_state: Option<PathBuf>,

    /// Restore strategy + wallet state from a JSON file written by a previous
    /// `--save-state`, then continue the run over this invocation's series. The
    /// document must be the same strategy shape the state was captured from.
    #[arg(long = "resume", value_name = "FILE")]
    resume: Option<PathBuf>,

    /// Mark every position still open at the end of the run to close at the
    /// final bar, booking it into `trades.csv` / the trade metrics (default:
    /// open positions are carried, unrealized). Mutually exclusive with
    /// `--save-state`.
    #[arg(long = "realize-open")]
    realize_open: bool,
}

/// What kind of spec `fugazi check` is checking. Nested subcommand so each
/// kind can carry its own positional shape without the top-level `check` args
/// having to caveat "only applies when `kind = ...`".
#[derive(Subcommand)]
enum CheckCmd {
    /// Validate a strategy spec's shape (an unset required `--params`
    /// placeholder is held as a typed hole rather than failing the check).
    Strategy(CheckStrategyArgs),
    /// Parse `get --overlay` specs — validates spec structure, the
    /// `SYMBOL[FREQ]:` scope prefix, column names, and reserved-name
    /// collisions.
    ///
    /// Deliberately parse-only: overlay specs are built with an empty schema
    /// (they're output-side, so no overlay side channel is bound), so a
    /// build-time check would panic on any `!get { key }` reference. Fully-
    /// typed validation (`!get` key resolution, typed-position mismatches, …)
    /// is a `fugazi get` concern where the atom stream's schema exists.
    Overlay(CheckOverlayArgs),
    /// Parse `run --costs` specs and build each configured leg's model.
    ///
    /// Surfaces unknown `kind:` values, malformed scope prefixes, and other
    /// tree-build errors that a plain `run` would only hit at startup.
    Costs(CheckCostsArgs),
}

#[derive(Args)]
struct CheckStrategyArgs {
    /// The strategy: `@file.yml` loads a file, anything else is inline YAML.
    /// May carry a leading shape prefix: `single:` (or none) for a
    /// `SingleAssetStrategy`, `pairs:` for a two-leg `PairsStrategy`.
    #[arg(value_name = "STRATEGY")]
    strategy: StrategySource,

    /// Resolve the strategy's `param` placeholders. Same shape as `run --params`:
    /// a `,`-separated list of `NAME=value` settings and `@file.yml` mapping
    /// loaders (repeatable; later terms win). Unlike `run`, omitting a required
    /// placeholder is *not* a check failure — `check` validates shape only, so
    /// an unset placeholder is held as a typed hole (the field's expected type
    /// decides the stand-in) and the count is reported alongside the params.
    #[arg(short, long = "params", value_name = "SPEC")]
    params: Vec<params::ParamSpec>,

    /// Suppress the "ok" message on success. Errors still print, and the exit
    /// code (0 ok, non-zero on failure) is unchanged.
    #[arg(short, long)]
    quiet: bool,
}

#[derive(Args)]
struct CheckOverlayArgs {
    /// One or more overlay specs — same shape as `get --overlay`:
    /// `[SCOPE:]col=expr[,col=expr,...]` inline or `[SCOPE:]@file.yml`, where
    /// `SCOPE` is an optional `SYMBOL[FREQ]:`, `SYMBOL:`, or `[FREQ]:` prefix.
    #[arg(value_name = "SPEC", required = true, num_args = 1..)]
    overlays: Vec<Source>,

    /// Resolve `!param` placeholders inside the overlay expressions, same as
    /// `get --params`: `,`-separated `NAME=value` terms and `@file.yml`.
    #[arg(short, long = "params", value_name = "SPEC")]
    params: Vec<params::ParamSpec>,

    /// Suppress the "ok" message on success. Errors still print, and the exit
    /// code (0 ok, non-zero on failure) is unchanged.
    #[arg(short, long)]
    quiet: bool,
}

#[derive(Args)]
struct CheckCostsArgs {
    /// One or more `--costs` specs — same shape as `run --costs`:
    /// `[SCOPE:]key=value[,key=value,...]` inline or `@file.yml`. `SCOPE` is
    /// an optional `SYMBOL[FREQ]:`, `SYMBOL:`, or `[FREQ]:` prefix; `none` is
    /// accepted as an explicit no-costs sentinel.
    #[arg(value_name = "SPEC", required = true, num_args = 1..)]
    specs: Vec<costs::CostSpec>,

    /// Suppress the "ok" message on success. Errors still print, and the exit
    /// code (0 ok, non-zero on failure) is unchanged.
    #[arg(short, long)]
    quiet: bool,
}

#[derive(Args)]
struct OptimizeArgs {
    /// The strategy: `@file.yml` loads a file, anything else is inline YAML.
    /// May carry a leading shape prefix: `single:` (or none) for a
    /// `SingleAssetStrategy`, `pairs:` for a two-leg `PairsStrategy`.
    #[arg(value_name = "STRATEGY")]
    strategy: StrategySource,

    /// A data series — same shape as `run --series` (repeatable; series
    /// full-join on `symbol` + `time`).
    #[arg(short, long = "series", required = true)]
    series: Vec<data::SeriesSpec>,

    /// Resolve the strategy's `param` placeholders — same syntax and semantics
    /// as `run --params`. Values that look like sweep axes (a JSON list
    /// `[v1,v2,v3]` or a range `start..end[:step]`) are rejected here — use
    /// `--grid` for sweep axes. The scalars set by `--params` form the shared
    /// baseline applied under every `--grid` subgrid.
    #[arg(short, long = "params", value_name = "SPEC")]
    params: Vec<params::ParamSpec>,

    /// Declare one sweep subgrid. Same term grammar as `--params` — comma-
    /// separated `NAME=value` settings and `@file.yml` mapping loaders — with
    /// two extra value forms only allowed here: `NAME=[v1,v2,v3]` (a discrete
    /// list) and `NAME=start..end[:step]` (an inclusive numeric range). Every
    /// axis' cartesian product within one `--grid` flag is that subgrid's
    /// point set; scalars stay fixed across the subgrid. Repeat the flag to
    /// stack subgrids (a *union* of Cartesian products): e.g.
    /// `--grid X=A,Y=1..10 --grid X=B,Z=10..100:10`, useful when a parameter
    /// only makes sense conditionally on another. Each subgrid layers over
    /// `--params`; total grid = sum of subgrid point counts.
    #[arg(short = 'g', long = "grid", value_name = "SPEC", required = true)]
    grid: Vec<params::ParamSpec>,

    /// The metrics to record for each grid point, as one CSV column each.
    /// Names are short leaf keys when unambiguous (`sharpe`, `max_pct`,
    /// `cagr_pct`) or dotted paths (`risk_adjusted.sharpe`,
    /// `drawdown.max_pct`) — see `metrics.yml` for the full catalogue. Column
    /// headers are always the canonical dotted path. Omit to emit every metric
    /// in the catalogue as its own column. `,`-separated, repeatable.
    #[arg(short = 'm', long = "metrics", value_delimiter = ',')]
    metrics: Vec<String>,

    /// Sort the output CSV (and print the winner) by this metric. Direction is
    /// hardcoded per metric — higher is better for `sharpe`/`sortino`/`cagr_pct`
    /// etc, lower is better for `max_pct`/`ulcer_index`/`annualized_volatility_pct`
    /// etc. Omit to emit rows in cartesian order.
    #[arg(long = "best-by", value_name = "METRIC")]
    best_by: Option<String>,

    /// Output CSV path. One row per grid point: axis columns then metric columns,
    /// `,`-delimited. Parent directories are created if missing.
    #[arg(short = 'o', long = "output", value_name = "FILE")]
    output: PathBuf,

    /// Rayon worker count for the grid. Defaults to one worker per logical CPU.
    #[arg(short = 'j', long = "jobs", value_name = "N")]
    jobs: Option<usize>,

    /// Initial cash for each backtest (per grid point).
    #[arg(short, long, default_value_t = 10_000.0)]
    cash: f64,

    /// US-equity trading calendar. Same semantics as `run --stocks`.
    #[arg(long, group = "asset_class")]
    stocks: bool,

    /// Forex trading calendar. Same semantics as `run --forex`.
    #[arg(long, group = "asset_class")]
    forex: bool,

    /// 24/7 trading calendar (crypto). Same semantics as `run --crypto`.
    #[arg(long, group = "asset_class")]
    crypto: bool,

    /// Bar cadence, e.g. `1d` / `4h`. Same semantics as `run --frequency`,
    /// including repeatable `SYMBOL:CODE` overrides.
    #[arg(short, long, value_name = "[SYM:]CODE")]
    frequency: Vec<calendar::ScopedFrequency>,

    /// Explicit `bars_per_year`. Same semantics as `run --bars-per-year`,
    /// including repeatable `SYMBOL[FREQ]:N` overrides.
    #[arg(long, value_name = "[SYM[FREQ]:]N")]
    bars_per_year: Vec<calendar::BarsPerYearSpec>,

    /// Annualized risk-free rate. Same semantics as `run --risk-free-rate`.
    #[arg(long, value_name = "RATE", default_value_t = 0.0)]
    risk_free_rate: f64,

    /// Configure trading costs — same shape as `run --costs`. Applied
    /// uniformly to every grid point.
    #[arg(long = "costs", value_name = "SPEC")]
    costs: Vec<costs::CostSpec>,

    /// Evaluate each grid point in non-overlapping windows (the same windowing
    /// as `run -w`). Every `-m` metric becomes two CSV columns — `<name>_mean`
    /// and `<name>_std`, its cross-window mean and standard deviation over the
    /// windows where it is defined — and `--best-by` ranks by the windowed
    /// mean, rewarding consistency across regimes rather than one lucky
    /// stretch.
    ///
    /// `LEN` is either a plain bar count (`10`, `252`) or a duration in the
    /// `-f/--frequency` alphabet (`1d`, `1w`, `1M`, `4h`) — see `run -w` for
    /// the resolution rules.
    #[arg(short = 'w', long = "windowed", value_name = "LEN", group = "sweep_shape")]
    windowed: Option<calendar::WindowSpec>,

    /// Rolling walk-forward optimization. `IS,OS[,Embargo]` — each component is
    /// a `-w`-style bar count or duration. For each fold the grid is scored on
    /// the in-sample window, the winner (by `--best-by`) is applied on the
    /// out-of-sample window, and results are emitted per fold plus a composite
    /// OOS artifact.
    ///
    /// Skips grid-wide `max(stable_period)` at the head of the series before
    /// laying out folds; pass `--keep-unstable` to skip only `max(warm_up)`
    /// (letting the IIR settling tail bleed into the first IS window). Embargo
    /// defaults to 0 bars — it removes the first N bars of each fold's OOS
    /// from the metric evaluation only (state still rolls through).
    ///
    /// Mutually exclusive with `-w/--windowed`.
    #[arg(long = "walkforward", value_name = "IS,OS[,E]", group = "sweep_shape")]
    walkforward: Option<calendar::WalkForwardSpec>,

    /// Under `--walkforward`, skip only `max(warm_up)` at the head of the
    /// series (not `max(stable_period)`), including the IIR settling tail in
    /// the first IS window. Opt-out for the safe default. No-op without
    /// `--walkforward`.
    #[arg(long = "keep-unstable", requires = "walkforward")]
    keep_unstable: bool,

    /// Rank `--best-by` conservatively (needs `-w` and `--best-by`): shift each
    /// grid point's cross-window mean *against* it by K standard deviations
    /// before sorting — higher-is-better metrics rank by `mean − K·std`,
    /// lower-is-better ones by `mean + K·std`. `K=0` is the plain windowed
    /// mean (the default). A metric defined in only one window has std 0 and
    /// ranks on its raw mean — check its `_std` CSV column.
    #[arg(
        short = 'k',
        long = "risk-aversion",
        value_name = "K",
        requires = "windowed",
        requires = "best_by"
    )]
    risk_aversion: Option<f64>,

    /// Suppress console output. The CSV is still written.
    #[arg(short, long)]
    quiet: bool,
}

fn main() -> std::process::ExitCode {
    let cli = Cli::parse();
    let outcome = match cli.command {
        Command::Run(args) => run(args),
        Command::Check { cmd } => match cmd {
            CheckCmd::Strategy(args) => check_strategy(args),
            CheckCmd::Overlay(args) => check_overlay(args),
            CheckCmd::Costs(args) => check_costs(args),
        },
        Command::Optimize(args) => optimize(args),
        Command::Get(args) => get::run(args),
        Command::Completions { shell } => completions::run(shell),
        Command::List { cmd } => list::run(cmd),
    };
    match outcome {
        Ok(()) => std::process::ExitCode::SUCCESS,
        Err(e) => {
            print_error(&e);
            std::process::ExitCode::FAILURE
        }
    }
}

/// Render a failure as a short block instead of anyhow's default chain.
///
/// The audience is as often a tool as a person — an agent iterating on a
/// strategy document reads this output and edits the file — so the shape is:
/// one line saying *what* is wrong, then the location on its own line, then
/// the context chain. Spec-parse errors arrive with a ` > `-separated tag path
/// glued to the front (see [`spec::diagnostics`]); splitting it out keeps the
/// first line to the actual problem rather than burying it behind a run of
/// `!above > !add > !mul >` that has to be read past every time.
fn print_error(err: &anyhow::Error) {
    // The innermost cause carries the real message; the outer ones are the
    // `with_context` breadcrumbs ("building strategy from …").
    let root = err.chain().last().map(|c| c.to_string()).unwrap_or_default();
    let (trail, message) = spec::diagnostics::split_trail(&root);

    eprintln!("{} {message}", style::red("error:"));
    if !trail.is_empty() {
        eprintln!("  {} {}", style::dim("at: "), trail.join(" › "));
    }
    // Context frames, outermost first, minus the root we already printed.
    let context: Vec<String> = err.chain().map(|c| c.to_string()).collect();
    for frame in context.iter().take(context.len().saturating_sub(1)) {
        eprintln!("  {} {frame}", style::dim("in: "));
    }
}

fn check_strategy(args: CheckStrategyArgs) -> Result<()> {
    let param_table = params::table(&args.params)?;

    let text = args.strategy.read().context("reading strategy")?;
    let label = args.strategy.label();
    let base = args.strategy.base_dir();

    // `check` validates shape, not values — it never builds or drives the
    // strategy (unlike `run`/`optimize`), so a required `!param` with no
    // `--params` value and no `default` doesn't need the user's real value.
    // Splice imports, then substitute in check mode: an unresolved required
    // placeholder becomes a *hole* rather than an error, and the typed parse
    // below fills each hole with a value of whatever type the field expects
    // (see `spec::undefined`).
    let value = spec::load_value_pre_params(&text, &base, &label)
        .with_context(|| parse_error_hint(&args.strategy))?;
    // The site count is discarded: the report below counts distinct placeholder
    // *names* instead, which is what the user has to supply values for.
    let (value, _n_hole_sites) = params::substitute_for_check(value, &param_table)
        .with_context(|| parse_error_hint(&args.strategy))?;
    let params_base = params_label(&param_table);

    // Deserialize under the hole-aware guard. `from_json_value` moves the tree
    // into the `serde_norway::Value` shape the bridges buffer through.
    let _guard = spec::undefined::check_mode();
    let parse_err = || parse_error_hint(&args.strategy);

    // Each arm parses its shape and reports back `(description, detail)`; the
    // placeholder-type checks and the printing are common, and must run *after*
    // the parse, since that is what populates the observations.
    let (description, detail) = match args.strategy.kind {
        StrategyKind::Single => {
            let strategy: spec::StrategyRef =
                spec::undefined::from_json_value(value).map_err(anyhow::Error::new).with_context(parse_err)?;
            (
                "parse and validate a strategy spec",
                format!("symbol {}", strategy.symbol()),
            )
        }
        StrategyKind::Pairs => {
            let spec: spec::PairsStrategySpec =
                spec::undefined::from_json_value(value).map_err(anyhow::Error::new).with_context(parse_err)?;
            (
                "parse and validate a pairs strategy spec",
                format!("pair {} / {}", spec.left, spec.right),
            )
        }
        StrategyKind::Basket => {
            // Basket parses eagerly: the top-level enum + templates. Under the
            // check-mode guard each template *body* typed-parses too (with its
            // `!arg`s held as holes), so an unknown tag or misspelled field
            // inside `score:` / `sizing:` is caught here rather than at the
            // first run that reaches a symbol.
            let spec: spec::BasketStrategySpec =
                spec::undefined::from_json_value(value).map_err(anyhow::Error::new).with_context(parse_err)?;
            (
                "parse and validate a basket strategy spec",
                format!("selection {:?}", spec.selection),
            )
        }
        StrategyKind::Multi => {
            // Multi-asset parses eagerly like basket, template bodies included.
            let spec: spec::MultiAssetStrategySpec =
                spec::undefined::from_json_value(value).map_err(anyhow::Error::new).with_context(parse_err)?;
            let sides: Vec<&str> = [
                spec.long.as_ref().map(|_| "long"),
                spec.short.as_ref().map(|_| "short"),
            ]
            .into_iter()
            .flatten()
            .collect();
            let sides = if sides.is_empty() {
                "no sides wired".to_string()
            } else {
                sides.join(" + ")
            };
            ("parse and validate a multi-asset strategy spec", sides)
        }
        StrategyKind::Portfolio => {
            // Portfolio parses eagerly at the top level (children, weights);
            // each child's own spec typed-parses too, and every template body
            // under a child validates the same way it would standalone.
            let spec: spec::PortfolioSpec =
                spec::undefined::from_json_value(value).map_err(anyhow::Error::new).with_context(parse_err)?;
            let n = spec.children.len();
            (
                "parse and validate a portfolio strategy spec",
                format!("{n} child strateg{}", if n == 1 { "y" } else { "ies" }),
            )
        }
    };

    // The typed parse above resolved every unset `!param` through a hole, and
    // each hole recorded the type its position demanded. A name required to be
    // two different types can never be satisfied by any `--params` value, so
    // that is a hard error; the rest is reported so the user knows what each
    // placeholder has to look like.
    let observations = spec::undefined::take_observations();
    reject_contradictory_params(&observations).with_context(parse_err)?;

    if !args.quiet {
        // Count distinct placeholder *names*, not substitution sites: one name
        // used in three positions is one value the user has to supply, and the
        // type line below lists it once.
        let n_undefined = observations
            .iter()
            .filter(|(o, _, _)| *o == spec::undefined::UndefinedOrigin::Undefined)
            .count();
        let params_label =
            params_label_with_holes(&params_base, observations.len() - n_undefined, n_undefined);
        let params_label = match param_types_label(&observations) {
            Some(types) => format!("{params_label}\n  {types}"),
            None => params_label,
        };
        print_check_report(description, &label, &params_label, &detail);
    }
    Ok(())
}

/// One-shape `check` output: the standard header, an `inputs` block with the
/// spec label and any resolved params, then a `result` block with `ok` and
/// the per-kind summary detail (`symbol BTC`, `pair … / …`, `N child …`).
/// Mirrors the section shape of `run` / `optimize`.
fn print_check_report(description: &str, input_label: &str, params: &str, detail: &str) {
    style::print_header("check", description);
    style::print_section("inputs");
    style::print_field("spec", input_label, 8);
    style::print_field("params", params, 8);
    println!();
    style::print_section("result");
    let ok = style::green("ok");
    style::print_field("status", &format!("{ok} · {detail}"), 8);
}

fn check_costs(args: CheckCostsArgs) -> Result<()> {
    // Fold the specs and build every leg's model (through resolve on a probe
    // symbol/freq) so an unknown `kind:`, a missing required field, or a
    // malformed scope prefix all surface here rather than at run start.
    let cfg = costs::config(&args.specs)?;
    // Force materialization of each configured leg — resolve for a nonsense
    // symbol+freq (won't match any scoped entry) so we hit the default; also
    // resolve for each configured scope so `by_symbol`/`by_interval`/`scoped`
    // entries build.
    let _ = cfg.resolve("__probe__", None);
    if !args.quiet {
        style::print_header("check", "parse and validate a cost spec");
        let n_scoped = cfg.scoped_count();
        let default_note = if cfg.has_any_default() {
            "with defaults"
        } else if cfg.is_none() {
            "no-op"
        } else {
            "no default (scoped-only)"
        };
        let scope_note = if n_scoped == 0 {
            "no scoped overrides".to_string()
        } else {
            format!("{n_scoped} scoped override(s)")
        };
        let n_specs = args.specs.len();
        style::print_section("inputs");
        style::print_field(
            "specs",
            &format!("{n_specs} spec{}", if n_specs == 1 { "" } else { "s" }),
            8,
        );
        println!();
        style::print_section("result");
        let ok = style::green("ok");
        style::print_field(
            "status",
            &format!("{ok} · {default_note}; {scope_note}"),
            8,
        );
    }
    Ok(())
}

fn check_overlay(args: CheckOverlayArgs) -> Result<()> {
    // Parse-only: the spec structure, scope prefix, column names, and reserved-
    // name collisions all surface here. We deliberately *don't* call
    // `Overlay::build` — that would panic on any `!get { key }` because
    // `Overlay::build` uses an empty schema (overlays are output-side; the
    // schema doesn't exist yet). Fully-typed validation (unknown `!get` keys,
    // typed-position mismatches, `period: 0` in a constructor's `assert!`, …)
    // is a `fugazi get` / `fugazi run` concern, where the atom stream's real
    // schema is available.
    let param_table = params::table(&args.params)?;
    let overlays = overlay::parse_specs(&args.overlays, &param_table)?;
    let columns = overlay::column_names(&overlays);

    if !args.quiet {
        style::print_header("check", "parse and validate an overlay spec");
        let labels: Vec<String> = args.overlays.iter().map(|s| s.label()).collect();
        let n_cols = columns.len();
        style::print_section("inputs");
        style::print_field("specs", &labels.join(", "), 8);
        style::print_field(
            "params",
            &params_label(&params::table(&args.params).unwrap_or_default()),
            8,
        );
        println!();
        style::print_section("result");
        let ok = style::green("ok");
        style::print_field(
            "status",
            &format!(
                "{ok} · {} overlay{} · {} column{}: {}",
                overlays.len(),
                if overlays.len() == 1 { "" } else { "s" },
                n_cols,
                if n_cols == 1 { "" } else { "s" },
                columns.join(", "),
            ),
            8,
        );
    }
    Ok(())
}

fn run(args: RunArgs) -> Result<()> {
    let text = args.strategy.read().context("reading strategy")?;
    let frame = data::DataFrame::from_series(&args.series)?;

    let strat_label = args.strategy.label();
    let class = asset_class(args.stocks, args.forex, args.crypto);
    let cost_config = costs::config(&args.costs)?;
    let costs_were_supplied = !args.costs.is_empty();

    let param_table = params::table(&args.params)?;
    let params_label = params_label(&param_table);
    // Load a `--resume` state file up front so it outlives the RunOptions borrow.
    let resume_state = match &args.resume {
        Some(path) => {
            let text = std::fs::read_to_string(path)
                .with_context(|| format!("reading resume state {}", path.display()))?;
            let state: spec::RunState = serde_json::from_str(&text)
                .with_context(|| format!("parsing resume state {}", path.display()))?;
            Some(state)
        }
        None => None,
    };
    let opts = run::RunOptions {
        cash: args.cash,
        out_dir: &args.output_dir,
        strategy_label: &strat_label,
        params: &params_label,
        bars_per_year: &args.bars_per_year,
        asset_class: class,
        risk_free_rate: args.risk_free_rate,
        windowed: args.windowed,
        cost_config: &cost_config,
        frequency: &args.frequency,
        costs_supplied: costs_were_supplied,
        quiet: args.quiet,
        resume: resume_state.as_ref(),
        save_state: args.save_state.as_deref(),
        realize_open: args.realize_open,
    };
    let base = args.strategy.base_dir();
    match args.strategy.kind {
        StrategyKind::Single => {
            let strategy = spec::StrategyRef::from_text_with_params_in(&text, &param_table, &base, &strat_label)
                .with_context(|| parse_error_hint(&args.strategy))?;
            run::run(&strategy, &frame, &opts)?;
        }
        StrategyKind::Pairs => {
            let spec = spec::PairsStrategySpec::from_text_with_params_in(&text, &param_table, &base, &strat_label)
                .with_context(|| parse_error_hint(&args.strategy))?;
            run::run_pairs(&spec, &frame, &opts)?;
        }
        StrategyKind::Basket => {
            let spec = spec::BasketStrategySpec::from_text_with_params_in(&text, &param_table, &base, &strat_label)
                .with_context(|| parse_error_hint(&args.strategy))?;
            run::run_basket(&spec, &frame, &opts)?;
        }
        StrategyKind::Multi => {
            let spec = spec::MultiAssetStrategySpec::from_text_with_params_in(&text, &param_table, &base, &strat_label)
                .with_context(|| parse_error_hint(&args.strategy))?;
            run::run_multi(&spec, &frame, &opts)?;
        }
        StrategyKind::Portfolio => {
            let spec = spec::PortfolioSpec::from_text_with_params_in(&text, &param_table, &base, &strat_label)
                .with_context(|| parse_error_hint(&args.strategy))?;
            run::run_portfolio(&spec, &frame, &opts)?;
        }
    }
    Ok(())
}

fn optimize(args: OptimizeArgs) -> Result<()> {
    let param_table = params::table(&args.params)?;
    optimize::reject_axes_in_params(&param_table)?;
    let grid_tables: Vec<HashMap<String, serde_json::Value>> = args
        .grid
        .iter()
        .map(|spec| params::table(std::slice::from_ref(spec)))
        .collect::<Result<_>>()?;
    let text = args.strategy.read().context("reading strategy")?;
    let frame = data::DataFrame::from_series(&args.series)?;

    let strat_label = args.strategy.label();
    let class = asset_class(args.stocks, args.forex, args.crypto);
    let cost_config = costs::config(&args.costs)?;
    let costs_were_supplied = !args.costs.is_empty();

    let opts = optimize::OptimizeOptions {
        cash: args.cash,
        strategy_kind: args.strategy.kind,
        strategy_text: &text,
        strategy_dir: &args.strategy.base_dir(),
        strategy_label: &strat_label,
        params_table: param_table,
        grid_tables,
        metrics: args.metrics,
        best_by: args.best_by,
        output: &args.output,
        bars_per_year: &args.bars_per_year,
        asset_class: class,
        risk_free_rate: args.risk_free_rate,
        windowed: args.windowed,
        walkforward: args.walkforward,
        keep_unstable: args.keep_unstable,
        risk_aversion: args.risk_aversion.unwrap_or(0.0),
        cost_config: &cost_config,
        frequency: &args.frequency,
        costs_supplied: costs_were_supplied,
        jobs: args.jobs,
        quiet: args.quiet,
    };
    optimize::run(&frame, opts).with_context(|| parse_error_hint(&args.strategy))?;
    Ok(())
}

/// A one-line `NAME=value, …` view of the effective params for the run block.
fn params_label(table: &HashMap<String, serde_json::Value>) -> String {
    if table.is_empty() {
        return "(defaults)".to_string();
    }
    let mut entries: Vec<String> = table
        .iter()
        .map(|(k, v)| match v {
            serde_json::Value::String(s) => format!("{k}={s}"),
            other => format!("{k}={other}"),
        })
        .collect();
    entries.sort();
    entries.join(", ")
}

/// `params_label` with a note of how many required placeholders `check` had to
/// fill with a hole (unset, no `default` — see `spec::undefined`). `0` is the common
/// case and adds nothing.
fn params_label_with_holes(base: &str, n_params: usize, n_undefined: usize) -> String {
    let mut parts = Vec::new();
    if n_params > 0 {
        parts.push(format!(
            "{n_params} unset placeholder{}",
            if n_params == 1 { "" } else { "s" }
        ));
    }
    if n_undefined > 0 {
        parts.push(format!("{n_undefined} !undefined"));
    }
    if parts.is_empty() {
        base.to_string()
    } else {
        format!("{base} ({})", parts.join(", "))
    }
}

/// The inferred type of each unresolved `!param`, e.g. `PERIOD: number`.
///
/// `check` cannot know a placeholder's *value*, but the typed parse reveals its
/// *type*: serde asks for a `usize` at `period:`, a `String` at `symbol:`, and
/// the hole records which. Reporting that turns "3 unset placeholders" into
/// something a user can act on — it says exactly what each `--params` value has
/// to look like.
fn param_types_label(
    observations: &[(spec::undefined::UndefinedOrigin, String, Vec<spec::undefined::RequiredType>)],
) -> Option<String> {
    use spec::undefined::UndefinedOrigin;
    if observations.is_empty() {
        return None;
    }
    Some(
        observations
            .iter()
            .map(|(origin, name, types)| {
                let types: Vec<&str> = types.iter().map(|t| t.label()).collect();
                let types = types.join("|");
                match origin {
                    // Spelled as the flag the user would actually type, so the
                    // type is unambiguous: `<number>` is the shape of the value
                    // that goes there, not a category being asserted about it.
                    UndefinedOrigin::Param => format!("needs --params {name}=<{types}>"),
                    // An `!undefined` has no name, so say what it needs and where.
                    UndefinedOrigin::Undefined => format!("needs <{types}> at {name}"),
                }
            })
            .collect::<Vec<_>>()
            .join("\n  "),
    )
}

/// Reject a `!param` required to be two different types in two places.
///
/// This is decidable without any data and is always a real defect: no single
/// `--params NAME=…` value can satisfy both positions, so the document cannot
/// run whatever the user supplies. Catching it here is the whole point of
/// inferring hole types rather than just counting them.
fn reject_contradictory_params(
    observations: &[(spec::undefined::UndefinedOrigin, String, Vec<spec::undefined::RequiredType>)],
) -> anyhow::Result<()> {
    use spec::undefined::UndefinedOrigin;
    let bad: Vec<String> = observations
        .iter()
        // Only named placeholders can contradict: an `!undefined` is keyed by
        // its own document path, so it is one position and cannot be two types.
        .filter(|(origin, _, types)| *origin == UndefinedOrigin::Param && types.len() > 1)
        .map(|(_, name, types)| {
            let types: Vec<&str> = types.iter().map(|t| t.label()).collect();
            format!("`{name}` is used as {}", types.join(" and as "))
        })
        .collect();
    if bad.is_empty() {
        return Ok(());
    }
    anyhow::bail!(
        "contradictory placeholder types: {}. No single `--params` value can satisfy \
         both positions — use a separate placeholder name for each.",
        bad.join("; ")
    )
}

/// Collapse the three mutually-exclusive asset-class booleans (clap enforces
/// the "at most one" rule via the `asset_class` arg group) into the enum a
/// downstream `Calendar` consumes. `None` means "unset — use the default".
fn asset_class(stocks: bool, forex: bool, crypto: bool) -> Option<calendar::AssetClass> {
    if stocks {
        Some(calendar::AssetClass::Stocks)
    } else if forex {
        Some(calendar::AssetClass::Forex)
    } else if crypto {
        Some(calendar::AssetClass::Crypto)
    } else {
        None
    }
}

/// Extra context on a strategy parse failure — the label is already baked in
/// by the loaders (via [`spec::load_value`] and the `from_text_with_params_in`
/// typed-parse wrap), so this only surfaces the `@file` hint when the caller
/// passed an inline value that looks like a bare file path.
fn parse_error_hint(strategy: &StrategySource) -> String {
    match strategy.misused_path() {
        Some(path) => format!("strategy `{path}` looks like a file path — did you mean `@{path}`?"),
        None => format!("loading strategy {}", strategy.label()),
    }
}
