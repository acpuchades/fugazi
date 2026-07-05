//! `fugazi` — a command-line backtester for the fugazi library.
//!
//! Load a strategy from a `strategy.yml`, feed it candle (and arbitrary extra)
//! data assembled from one or more `--series`, and run it through a paper wallet,
//! writing `trades.csv`, `returns.csv` and `metrics.yml`:
//!
//! ```text
//! fugazi run @strategy.yml \
//!            --series @candles.csv \
//!            --output-dir out/
//! ```
//!
//! The strategy (a positional) takes `@file` to load a file, or inline YAML for
//! anything else — the same `@` convention `--series`/`--params` use.

mod backtest;
mod batch;
mod calendar;
mod completions;
mod convert;
mod costs;
mod data;
mod dyn_;
mod file;
mod get;
mod input;
mod list;
mod metrics;
mod optimize;
mod overlay;
mod params;
mod pool;
mod run;
mod sigils;
mod spec;
mod style;

use std::collections::HashMap;
use std::num::NonZeroUsize;
use std::path::PathBuf;

use anyhow::{Context, Result};
use clap::{Args, Parser, Subcommand};
use clap_complete::Shell;

use input::Source;

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
    /// `fugazi check strategy <STRATEGY>` validates a strategy spec (with
    /// `--params` substitution); `fugazi check overlay <SPEC>...` validates
    /// one or more `get --overlay` specs; `fugazi check costs <SPEC>...`
    /// validates one or more `run --costs` specs.
    #[command(subcommand_required = true, arg_required_else_help = true)]
    Check {
        #[command(subcommand)]
        cmd: CheckCmd,
    },
    /// Sweep a strategy over a parameter grid and rank the combinations.
    Optimize(OptimizeArgs),
    /// Fetch OHLCV candles from remote providers into a `run`-ready CSV.
    ///
    /// Spec grammar: `<provider>:<symbol>[<freq>,<freq>...](,<symbol>[<freq>...])*`;
    /// several specs may be given and all series download in parallel.
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
    /// subcommand can fetch from; `fugazi list tickers <provider>` fetches and
    /// prints every symbol the given provider offers.
    List {
        #[command(subcommand)]
        cmd: list::ListCmd,
    },
}

#[derive(Args)]
struct RunArgs {
    /// The strategy: `@file.yml` loads a file, anything else is inline YAML.
    #[arg(value_name = "STRATEGY")]
    strategy: Source,

    /// A data series: `,`-separated `key=value` literals and `@file.csv` loaders
    /// (repeatable; series full-join on `symbol` + `time`). Each file's column
    /// delimiter is autodetected.
    #[arg(short, long = "series", required = true)]
    series: Vec<data::SeriesSpec>,

    /// Directory to write `trades.csv` and `returns.csv` into.
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
    frequency: Vec<calendar::FrequencySpec>,

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
    /// bar cadence from the median gap in the input `time` column (per
    /// `(symbol, freq)` series — see `--series`) and pairs it with the
    /// calendar (default `--stocks`, 252 trading days a year).
    #[arg(long, value_name = "[SYM[FREQ]:]N")]
    bars_per_year: Vec<calendar::BarsPerYearSpec>,

    /// Annualized risk-free rate as a fraction (e.g. `0.045` = 4.5% p.a.).
    /// Subtracted from the annualized mean return before Sharpe/Sortino/UPI,
    /// and used as the per-bar threshold for Omega. Default 0 — the
    /// pre-adjusted excess-return semantics of the original release.
    #[arg(long, value_name = "RATE", default_value_t = 0.0)]
    risk_free_rate: f64,

    /// Also reduce the run in N-bar windows for post-hoc analysis. `metrics.yml`
    /// (whole-run) is always written; adding `-w N` writes two extra CSVs at
    /// window length N — `metrics.csv` (non-overlapping windows, one row each)
    /// and `rolling.csv` (rolling stride-1 windows, one row each). Both share
    /// the same columns as `metrics.yml` under their dotted names, with the
    /// window's start/end times in the first two columns. Plot from R/Python;
    /// no charts are produced.
    #[arg(short = 'w', long = "windowed", value_name = "N")]
    windowed: Option<NonZeroUsize>,

    /// Configure trading costs (commission, spread, slippage). Same shape as
    /// `--params`: `,`-separated terms `[SCOPE:]key=value` and `@file.yml`
    /// preset loaders (repeatable; later terms win, more-specific scopes win
    /// over less-specific). `--costs none` acknowledges the frictionless
    /// default and silences the "no cost model set" warning. Omit for a
    /// zero-cost backtest (matches the pre-costs release byte-for-byte).
    #[arg(long = "costs", value_name = "SPEC")]
    costs: Vec<costs::CostSpec>,

    /// Use `SingleAssetStrategy` (the default). When the frame carries more
    /// than one `(symbol, freq)` series, the strategy is iterated over each
    /// group in parallel — output files aggregate across iterations under
    /// `--output-dir`, with `%SYMBOL`/`%FREQ` sigils in that path (or in
    /// `--params` values) filled per iteration. Mutually exclusive with
    /// `--multiple`.
    #[arg(long, group = "single_multiple")]
    single: bool,

    /// Reserved for a future `MultiAssetStrategy` (portfolio / pairs — one
    /// strategy that sees all series at once). Not yet implemented; passing
    /// this errors out. Mutually exclusive with `--single`.
    #[arg(long, group = "single_multiple")]
    multiple: bool,

    /// Rayon worker count for batch iterations (multi-series frames only).
    /// Defaults to one worker per logical CPU. No effect when the frame
    /// has a single `(symbol, freq)` group.
    #[arg(short = 'j', long = "jobs", value_name = "N")]
    jobs: Option<usize>,

    /// Suppress all console output (the result files are still written).
    #[arg(short, long)]
    quiet: bool,
}

/// What kind of spec `fugazi check` is checking. Nested subcommand so each
/// kind can carry its own positional shape without the top-level `check` args
/// having to caveat "only applies when `kind = ...`".
#[derive(Subcommand)]
enum CheckCmd {
    /// Validate a strategy spec (with `--params` substitution).
    Strategy(CheckStrategyArgs),
    /// Parse `get --overlay` specs and build each column's indicator.
    ///
    /// Surfaces bad `!tag`s, missing parameters, and other tree-build errors
    /// that a plain `get` run would only hit at fetch time.
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
    #[arg(value_name = "STRATEGY")]
    strategy: Source,

    /// Resolve the strategy's `param` placeholders. Same shape as `run --params`:
    /// a `,`-separated list of `NAME=value` settings and `@file.yml` mapping
    /// loaders (repeatable; later terms win). Omitting a required placeholder is
    /// a check failure.
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
    #[arg(value_name = "STRATEGY")]
    strategy: Source,

    /// A data series — same shape as `run --series` (repeatable; series
    /// full-join on `symbol` + `time`).
    #[arg(short, long = "series", required = true)]
    series: Vec<data::SeriesSpec>,

    /// Resolve the strategy's `param` placeholders and declare the sweep axes.
    /// Same shape as `run --params` with two new value forms:
    /// `NAME=[v1,v2,v3]` — a discrete list (JSON array) — and
    /// `NAME=start..end[:step]` — an inclusive numeric range. Every axis'
    /// cartesian product is one grid point; scalar values stay fixed across
    /// the sweep.
    #[arg(short, long = "params", value_name = "SPEC")]
    params: Vec<params::ParamSpec>,

    /// The metrics to record for each grid point, as one CSV column each.
    /// Names are short leaf keys when unambiguous (`sharpe`, `max_pct`,
    /// `cagr_pct`) or dotted paths (`risk_adjusted.sharpe`,
    /// `drawdown.max_pct`) — see `metrics.yml` for the full catalogue.
    /// `,`-separated, repeatable.
    #[arg(short = 'm', long = "metrics", value_delimiter = ',', required = true)]
    metrics: Vec<String>,

    /// Sort the output CSV (and print the winner) by this metric. Direction is
    /// hardcoded per metric — higher is better for `sharpe`/`sortino`/`cagr_pct`
    /// etc, lower is better for `max_pct`/`ulcer_index`/`annualized_volatility_pct`
    /// etc. Omit to emit rows in cartesian order.
    #[arg(long = "best-by", value_name = "METRIC")]
    best_by: Option<String>,

    /// Output CSV path. One row per grid point: axis columns then metric columns,
    /// `;`-delimited. Parent directories are created if missing.
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
    frequency: Vec<calendar::FrequencySpec>,

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

    /// Evaluate each grid point in non-overlapping windows of N bars (the same
    /// windowing as `run -w`). Every `-m` metric becomes two CSV columns —
    /// `<name>_mean` and `<name>_std`, its cross-window mean and standard
    /// deviation over the windows where it is defined — and `--best-by` ranks
    /// by the windowed mean, rewarding consistency across regimes rather than
    /// one lucky stretch.
    #[arg(short = 'w', long = "windowed", value_name = "N")]
    windowed: Option<NonZeroUsize>,

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

fn main() -> Result<()> {
    let cli = Cli::parse();
    match cli.command {
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
    }
}

fn check_strategy(args: CheckStrategyArgs) -> Result<()> {
    let param_table = params::table(&args.params)?;

    let text = args.strategy.read().context("reading strategy")?;
    let spec = spec::StrategySpec::from_text_with_params(&text, &param_table)
        .with_context(|| parse_error_context(&args.strategy))?;

    if !args.quiet {
        style::print_header("check", "parse and validate a strategy spec");
        println!("{}: ok (symbol {})", args.strategy.label(), spec.symbol);
    }
    Ok(())
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
        let labels: Vec<String> = args
            .specs
            .iter()
            .map(|_| "(spec)".to_string())
            .collect();
        println!(
            "{}: ok ({default_note}; {scope_note})",
            labels.join(", "),
        );
    }
    Ok(())
}

fn check_overlay(args: CheckOverlayArgs) -> Result<()> {
    // Parses the specs (including the `SYMBOL[FREQ]:` scope prefix) *and*
    // builds one live indicator per column, so an unknown `!tag`, a missing
    // `period`, or an `entry`-in-`get` misuse all surface here.
    let overlays = overlay::parse_specs(&args.overlays)?;
    for o in &overlays {
        let _ = o.build();
    }
    let columns = overlay::column_names(&overlays);

    if !args.quiet {
        style::print_header("check", "parse and validate an overlay spec");
        let labels: Vec<String> = args.overlays.iter().map(|s| s.label()).collect();
        let n_cols = columns.len();
        println!(
            "{}: ok ({} overlay{} across {} column{}: {})",
            labels.join(", "),
            overlays.len(),
            if overlays.len() == 1 { "" } else { "s" },
            n_cols,
            if n_cols == 1 { "" } else { "s" },
            columns.join(", "),
        );
    }
    Ok(())
}

fn run(args: RunArgs) -> Result<()> {
    // `--multiple` is reserved for a future `MultiAssetStrategy` — reject
    // it up front so users don't wait for a run to start before finding out.
    if args.multiple {
        anyhow::bail!(
            "--multiple is reserved for a future MultiAssetStrategy and is not yet implemented; \
             pass --single (the default) to batch-iterate a SingleAssetStrategy over the frame's series"
        );
    }

    let text = args.strategy.read().context("reading strategy")?;
    let frame = data::DataFrame::from_series(&args.series)?;

    let strat_label = args.strategy.label();
    let class = asset_class(args.stocks, args.forex, args.crypto);
    let cost_config = costs::config(&args.costs)?;
    let costs_were_supplied = !args.costs.is_empty();

    // Dispatch: if the frame carries more than one `(symbol, freq)` group,
    // fan out over the rayon pool via `batch::run`; otherwise take the
    // byte-identical single-run path via `run::run`. Enumerating groups
    // is cheap (one BTreeMap pass); we do it here so a single-group frame
    // avoids all batch machinery.
    let groups = frame.groups()?;
    if groups.len() > 1 {
        let params_label_str = params_label_from_specs(&args.params);
        let opts = batch::BatchOptions {
            cash: args.cash,
            out_dir: &args.output_dir,
            strategy_text: &text,
            strategy_label: &strat_label,
            params_label: &params_label_str,
            param_specs: &args.params,
            bars_per_year: &args.bars_per_year,
            asset_class: class,
            risk_free_rate: args.risk_free_rate,
            windowed: args.windowed,
            cost_config: &cost_config,
            frequency: &args.frequency,
            costs_supplied: costs_were_supplied,
            jobs: args.jobs,
            quiet: args.quiet,
        };
        batch::run(&frame, &opts)?;
    } else {
        // Single-group frame: substitute params once and take the legacy
        // write path. Byte-identical to the pre-batch release for a plain
        // one-symbol CSV.
        let param_table = params::table(&args.params)?;
        let spec = spec::StrategySpec::from_text_with_params(&text, &param_table)
            .with_context(|| parse_error_context(&args.strategy))?;
        let params_label = params_label(&param_table);
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
        };
        run::run(&spec, &frame, &opts)?;
    }
    Ok(())
}

/// A one-line `%SIGIL, NAME=…` view of the params for the batch inputs
/// block. Distinct from [`params_label`] because in batch mode the sigils
/// aren't resolved yet — we show the specs verbatim.
fn params_label_from_specs(specs: &[params::ParamSpec]) -> String {
    if specs.is_empty() {
        return "(defaults)".to_string();
    }
    format!("{} spec(s)", specs.len())
}

fn optimize(args: OptimizeArgs) -> Result<()> {
    let param_table = params::table(&args.params)?;
    let text = args.strategy.read().context("reading strategy")?;
    let frame = data::DataFrame::from_series(&args.series)?;

    let strat_label = args.strategy.label();
    let class = asset_class(args.stocks, args.forex, args.crypto);
    let cost_config = costs::config(&args.costs)?;
    let costs_were_supplied = !args.costs.is_empty();

    let opts = optimize::OptimizeOptions {
        cash: args.cash,
        strategy_text: &text,
        strategy_label: &strat_label,
        params_table: param_table,
        metrics: args.metrics,
        best_by: args.best_by,
        output: &args.output,
        bars_per_year: &args.bars_per_year,
        asset_class: class,
        risk_free_rate: args.risk_free_rate,
        windowed: args.windowed,
        risk_aversion: args.risk_aversion.unwrap_or(0.0),
        cost_config: &cost_config,
        frequency: &args.frequency,
        costs_supplied: costs_were_supplied,
        jobs: args.jobs,
        quiet: args.quiet,
    };
    optimize::run(&frame, opts).with_context(|| parse_error_context(&args.strategy))?;
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

/// Error context for a strategy parse failure. For an inline value that looks like
/// a bare file path, add a hint pointing at the `@file` form.
fn parse_error_context(strategy: &Source) -> String {
    let base = format!("parsing strategy {}", strategy.label());
    match strategy.misused_path() {
        Some(path) => format!("{base} (did you mean `@{path}`?)"),
        None => base,
    }
}
