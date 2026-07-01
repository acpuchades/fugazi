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
mod calendar;
mod chart;
mod convert;
mod data;
mod dynd;
mod input;
mod metrics;
mod optimize;
mod params;
mod spec;
mod style;

use std::collections::HashMap;
use std::path::PathBuf;

use anyhow::{Context, Result};
use clap::{Args, Parser, Subcommand};

use input::Source;

/// Incremental technical-analysis backtester.
#[derive(Parser)]
#[command(name = "fugazi", version, about)]
struct Cli {
    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand)]
enum Command {
    /// Run a `strategy.yml` backtest over CSV series.
    Run(RunArgs),
    /// Parse a `strategy.yml` and report whether it is syntactically valid.
    Check(CheckArgs),
    /// Sweep a strategy over a parameter grid and rank the combinations.
    Optimize(OptimizeArgs),
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

    /// RNG seed, recorded for reproducibility and echoed in the run block. The
    /// backtest is deterministic today, so this only matters once a stochastic
    /// step (slippage, sampling, …) consumes it.
    #[arg(long, default_value_t = 1234)]
    seed: u64,

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
    #[arg(short, long, value_name = "CODE")]
    frequency: Option<calendar::Frequency>,

    /// Explicit `bars_per_year` for the annualization step in `metrics.yml`
    /// (Sharpe/Sortino/CAGR/annualized volatility). Overrides the value
    /// derived from `--stocks`/`--forex`/`--crypto` + `--frequency`; defaults
    /// to 252 (US-equity daily) when nothing else is set.
    #[arg(long, value_name = "N")]
    bars_per_year: Option<f64>,

    /// Annualized risk-free rate as a fraction (e.g. `0.045` = 4.5% p.a.).
    /// Subtracted from the annualized mean return before Sharpe/Sortino/UPI,
    /// and used as the per-bar threshold for Omega. Default 0 — the
    /// pre-adjusted excess-return semantics of the original release.
    #[arg(long, value_name = "RATE", default_value_t = 0.0)]
    risk_free_rate: f64,

    /// Suppress all console output (the result files are still written).
    #[arg(short, long)]
    quiet: bool,
}

#[derive(Args)]
struct CheckArgs {
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

    /// RNG seed, mirrored from `run` for reproducibility.
    #[arg(long, default_value_t = 1234)]
    seed: u64,

    /// US-equity trading calendar. Same semantics as `run --stocks`.
    #[arg(long, group = "asset_class")]
    stocks: bool,

    /// Forex trading calendar. Same semantics as `run --forex`.
    #[arg(long, group = "asset_class")]
    forex: bool,

    /// 24/7 trading calendar (crypto). Same semantics as `run --crypto`.
    #[arg(long, group = "asset_class")]
    crypto: bool,

    /// Bar cadence, e.g. `1d` / `4h`. Same semantics as `run --frequency`.
    #[arg(short, long, value_name = "CODE")]
    frequency: Option<calendar::Frequency>,

    /// Explicit `bars_per_year`. Same semantics as `run --bars-per-year`.
    #[arg(long, value_name = "N")]
    bars_per_year: Option<f64>,

    /// Annualized risk-free rate. Same semantics as `run --risk-free-rate`.
    #[arg(long, value_name = "RATE", default_value_t = 0.0)]
    risk_free_rate: f64,

    /// Suppress console output. The CSV is still written.
    #[arg(short, long)]
    quiet: bool,
}

fn main() -> Result<()> {
    let cli = Cli::parse();
    match cli.command {
        Command::Run(args) => run(args),
        Command::Check(args) => check(args),
        Command::Optimize(args) => optimize(args),
    }
}

fn check(args: CheckArgs) -> Result<()> {
    let param_table = params::table(&args.params)?;

    let text = args.strategy.read().context("reading strategy")?;
    let spec = spec::StrategySpec::from_text_with_params(&text, &param_table)
        .with_context(|| parse_error_context(&args.strategy))?;

    if !args.quiet {
        println!("{}: ok (symbol {})", args.strategy.label(), spec.symbol);
    }
    Ok(())
}

fn run(args: RunArgs) -> Result<()> {
    let param_table = params::table(&args.params)?;

    let text = args.strategy.read().context("reading strategy")?;
    let spec = spec::StrategySpec::from_text_with_params(&text, &param_table)
        .with_context(|| parse_error_context(&args.strategy))?;

    let frame = data::DataFrame::from_series(&args.series)?;

    let strat_label = args.strategy.label();
    let params_label = params_label(&param_table);
    let class = asset_class(args.stocks, args.forex, args.crypto);
    let bars_per_year = calendar::resolve(args.bars_per_year, class, args.frequency);
    let opts = backtest::RunOptions {
        cash: args.cash,
        out_dir: &args.output_dir,
        strategy_label: &strat_label,
        params: &params_label,
        seed: args.seed,
        bars_per_year,
        risk_free_rate: args.risk_free_rate,
        quiet: args.quiet,
    };
    backtest::run(&spec, &frame, &opts)?;
    Ok(())
}

fn optimize(args: OptimizeArgs) -> Result<()> {
    let param_table = params::table(&args.params)?;
    let text = args.strategy.read().context("reading strategy")?;
    let frame = data::DataFrame::from_series(&args.series)?;

    let strat_label = args.strategy.label();
    let class = asset_class(args.stocks, args.forex, args.crypto);
    let bars_per_year = calendar::resolve(args.bars_per_year, class, args.frequency);

    let opts = optimize::OptimizeOptions {
        cash: args.cash,
        strategy_text: &text,
        strategy_label: &strat_label,
        params_table: param_table,
        metrics: args.metrics,
        best_by: args.best_by,
        output: &args.output,
        bars_per_year,
        risk_free_rate: args.risk_free_rate,
        jobs: args.jobs,
        seed: args.seed,
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
