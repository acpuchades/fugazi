//! `fugazi` — a command-line backtester for the fugazi library.
//!
//! Load a strategy from a `strategy.yml`, feed it candle (and arbitrary extra)
//! data assembled from one or more `--series`, and run it through a paper wallet,
//! writing `trades.csv` and `returns.csv`:
//!
//! ```text
//! fugazi run --strategy strategy.yml \
//!            --series symbol=BTC,@candles.csv \
//!            --output-dir out/
//! ```

mod backtest;
mod data;
mod dynd;
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
    let spec = spec::StrategySpec::from_yaml(&yaml)
        .with_context(|| format!("parsing strategy `{}`", args.strategy.display()))?;

    let frame = data::DataFrame::from_series(&args.series)?;

    let summary = backtest::run(&spec, &frame, args.cash, &args.output_dir)?;

    println!("symbol:       {}", spec.symbol);
    println!("bars:         {}", summary.bars);
    println!("final equity: {:.2}", summary.final_equity);
    println!("return:       {:+.2}%", summary.return_pct);
    println!("trades:       {}", summary.trades);
    println!("output:       {}", args.output_dir.display());
    Ok(())
}
