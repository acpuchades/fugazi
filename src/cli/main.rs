//! `fugazi` — a command-line backtester for the fugazi library.
//!
//! Load a strategy from a `strategy.yml`, feed it candle (and arbitrary extra)
//! data assembled from one or more `--series`, and run it through a paper wallet,
//! writing `trades.csv` and `returns.csv`:
//!
//! ```text
//! fugazi run --strategy strategy.yml \
//!            --series @candles.csv \
//!            --output-dir out/
//! ```

mod backtest;
mod data;
mod dynd;
mod params;
mod spec;

use std::path::PathBuf;

use anyhow::{Context, Result};
use clap::{Args, Parser, Subcommand};

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
    /// Path to the strategy YAML file.
    #[arg(long)]
    strategy: PathBuf,

    /// A data series: `,`-separated `key=value` literals and `@file.csv` loaders
    /// (repeatable; series full-join on `symbol` + `time`). Each file's column
    /// delimiter is autodetected.
    #[arg(long = "series", required = true)]
    series: Vec<String>,

    /// Directory to write `trades.csv` and `returns.csv` into.
    #[arg(long = "output-dir")]
    output_dir: PathBuf,

    /// Initial cash for the paper wallet.
    #[arg(long, default_value_t = 10_000.0)]
    cash: f64,

    /// Override a `!param { key: NAME }` placeholder (repeatable): NAME=value.
    #[arg(long = "param", value_name = "NAME=VALUE")]
    param: Vec<String>,

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
    let yaml = std::fs::read_to_string(&args.strategy)
        .with_context(|| format!("reading strategy `{}`", args.strategy.display()))?;
    let params = params::parse(&args.param)?;
    let spec = spec::StrategySpec::from_yaml_with_params(&yaml, &params)
        .with_context(|| format!("parsing strategy `{}`", args.strategy.display()))?;

    let frame = data::DataFrame::from_series(&args.series)?;

    let opts = backtest::RunOptions {
        cash: args.cash,
        out_dir: &args.output_dir,
        strategy_path: &args.strategy,
        params: &args.param,
        seed: args.seed,
        quiet: args.quiet,
    };
    backtest::run(&spec, &frame, &opts)?;
    Ok(())
}
