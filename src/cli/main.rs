//! `fugazi` — a command-line backtester for the fugazi library.
//!
//! Load a strategy from a `strategy.yml`, feed it candle (and arbitrary extra)
//! data assembled from one or more `--series`, and run it through a paper wallet,
//! writing `trades.csv` and `returns.csv`:
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
mod convert;
mod data;
mod dynd;
mod input;
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

    /// Suppress all console output (the result files are still written).
    #[arg(short, long)]
    quiet: bool,
}

fn main() -> Result<()> {
    let cli = Cli::parse();
    match cli.command {
        Command::Run(args) => run(args),
    }
}

fn run(args: RunArgs) -> Result<()> {
    let param_table = params::table(&args.params)?;

    let text = args.strategy.read().context("reading strategy")?;
    let spec = spec::StrategySpec::from_text_with_params(&text, &param_table)
        .with_context(|| parse_error_context(&args.strategy))?;

    let frame = data::DataFrame::from_series(&args.series)?;

    let strat_label = args.strategy.label();
    let params_label = params_label(&param_table);
    let opts = backtest::RunOptions {
        cash: args.cash,
        out_dir: &args.output_dir,
        strategy_label: &strat_label,
        params: &params_label,
        seed: args.seed,
        quiet: args.quiet,
    };
    backtest::run(&spec, &frame, &opts)?;
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

/// Error context for a strategy parse failure. For an inline value that looks like
/// a bare file path, add a hint pointing at the `@file` form.
fn parse_error_context(strategy: &Source) -> String {
    let base = format!("parsing strategy {}", strategy.label());
    match strategy.misused_path() {
        Some(path) => format!("{base} (did you mean `@{path}`?)"),
        None => base,
    }
}
