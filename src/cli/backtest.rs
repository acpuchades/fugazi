//! The backtest driver: walk one symbol's candles through a strategy and a
//! [`PaperWallet`], writing the result files and narrating the run.
//!
//! Each bar: price the wallet at the candle's `close`, `update` the strategy,
//! then `trade` it; any orders the trade appended to the blotter are emitted to
//! `trades.csv` stamped with this bar's `time` and fill price (the close), and
//! the running equity is emitted to `returns.csv`. Both result files are written
//! `;`-delimited for Excel.
//!
//! Console output (silenced by [`RunOptions::quiet`]) is a two-line banner (the
//! constant tool identity, then the active command), then three blocks: **inputs**
//! (strategy, params, seed, period, capital, output), **trades** (each fill, with
//! its symbol, streamed as it happens), and **result** (bars, trades, capital
//! change, then start/finish times with elapsed runtime). A symbol is per-trade,
//! never a run-level field.

use std::path::Path;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use anyhow::{Context, Result};
use fugazi::prelude::*;

use crate::data::DataFrame;
use crate::spec::StrategySpec;

/// Console-logging knobs plus the run's inputs, threaded in from the CLI args.
pub struct RunOptions<'a> {
    /// Initial cash for the paper wallet.
    pub cash: Real,
    /// Directory to write `trades.csv` / `returns.csv` into.
    pub out_dir: &'a Path,
    /// A short label for the strategy source (file path or `(inline)`), echoed in
    /// the run block.
    pub strategy_label: &'a str,
    /// A one-line view of the effective params (`NAME=value, …`), echoed in the
    /// run block.
    pub params: &'a str,
    /// The RNG seed, recorded for reproducibility. The backtest is currently
    /// deterministic so it has no functional effect yet; it is echoed in the run
    /// block so a run can be replayed (and will seed any future stochastic step —
    /// slippage, sampling, …).
    pub seed: u64,
    /// Suppress all console output (the result files are still written).
    pub quiet: bool,
}

/// Headline numbers returned from a run.
pub struct Summary {
    pub final_equity: Real,
    pub return_pct: Real,
    pub trades: usize,
    pub bars: usize,
}

/// Run `spec` over the dataframe per `opts`, writing `trades.csv` and
/// `returns.csv` and printing the tiered run/trade/result logs.
pub fn run(spec: &StrategySpec, frame: &DataFrame, opts: &RunOptions) -> Result<Summary> {
    let started = SystemTime::now();
    let symbol = spec.symbol.clone();
    let mut strategy = spec.build();
    let candles = frame.candles(&symbol)?;

    std::fs::create_dir_all(opts.out_dir)
        .with_context(|| format!("creating output dir `{}`", opts.out_dir.display()))?;
    let mut trades = writer(&opts.out_dir.join("trades.csv"))?;
    trades.write_record(["time", "symbol", "side", "quantity", "price"])?;
    let mut returns = writer(&opts.out_dir.join("returns.csv"))?;
    returns.write_record(["time", "equity", "return"])?;

    let start = candles.first().map_or("", |(t, _)| t.as_str());
    let end = candles.last().map_or("", |(t, _)| t.as_str());
    if !opts.quiet {
        print_header();
        print_inputs_block(opts, start, end, candles.len());
        println!("\ntrades");
    }

    let mut wallet = PaperWallet::new(opts.cash);
    let mut prev_equity = opts.cash;

    for (time, candle) in &candles {
        wallet.update(symbol.clone(), Reference(candle.close));
        strategy.update(*candle);

        let before = wallet.orders().len();
        strategy.trade(&mut wallet);
        for order in &wallet.orders()[before..] {
            let side = match order.side {
                Side::Buy => "buy",
                Side::Sell => "sell",
            };
            trades.write_record([
                time,
                &order.symbol,
                side,
                &order.quantity.to_string(),
                &candle.close.to_string(),
            ])?;
            if !opts.quiet {
                // Columns mirror trades.csv: time, symbol, side, quantity, price.
                // Each trade carries its own symbol, so this stays correct for a
                // future multi-symbol strategy.
                println!(
                    "  {time}  {:<6}  {side:<4} {:.4} @ {:.2}",
                    order.symbol, order.quantity, candle.close
                );
            }
        }

        let equity = wallet.equity().0;
        let ret = if prev_equity != 0.0 {
            (equity - prev_equity) / prev_equity * 100.0
        } else {
            0.0
        };
        returns.write_record([time, &equity.to_string(), &ret.to_string()])?;
        prev_equity = equity;
    }

    trades.flush()?;
    returns.flush()?;

    let final_equity = wallet.equity().0;
    let summary = Summary {
        final_equity,
        return_pct: if opts.cash != 0.0 {
            (final_equity - opts.cash) / opts.cash * 100.0
        } else {
            0.0
        },
        trades: wallet.orders().len(),
        bars: candles.len(),
    };

    let finished = SystemTime::now();
    if !opts.quiet {
        print_result_block(opts, &summary, started, finished);
    }
    Ok(summary)
}

/// The banner. Line 1 is the constant tool identity (the same for any
/// subcommand); line 2 names the active command and what it does.
fn print_header() {
    println!(
        "{} {} · {}",
        env!("CARGO_PKG_NAME"),
        env!("CARGO_PKG_VERSION"),
        env!("CARGO_PKG_REPOSITORY")
    );
    println!("run · backtest a strategy over CSV series");
    println!();
}

/// The "inputs" block: what this run was given. Timing (start/finish) lives in
/// the result block, since it's not an input.
///
/// No `symbol` line: a symbol is a property of each trade, not of the run (see
/// the trade stream and `trades.csv`), so this stays correct for a future
/// multi-symbol strategy.
fn print_inputs_block(opts: &RunOptions, start: &str, end: &str, bars: usize) {
    println!("inputs");
    print_field("strategy", opts.strategy_label);
    print_field("params", opts.params);
    print_field("seed", &opts.seed.to_string());
    print_field("period", &format!("{start} → {end} ({bars} bars)"));
    print_field("capital", &format!("{:.2}", opts.cash));
    print_field("output", &opts.out_dir.display().to_string());
}

/// The always-on "result" block: the run's outputs, then its wall-clock timing
/// (start, finish, and elapsed runtime).
///
/// No `symbol` line: a symbol is a property of each trade (see the trade stream
/// and `trades.csv`), not of the run as a whole — so this stays correct for a
/// future multi-symbol strategy.
fn print_result_block(opts: &RunOptions, s: &Summary, started: SystemTime, finished: SystemTime) {
    println!("\nresult");
    print_field("bars", &s.bars.to_string());
    print_field("trades", &s.trades.to_string());
    print_field(
        "capital",
        &format!(
            "{:.2} → {:.2}  ({:+.2}, {:+.2}%)",
            opts.cash,
            s.final_equity,
            s.final_equity - opts.cash,
            s.return_pct
        ),
    );
    let elapsed = finished.duration_since(started).unwrap_or_default();
    print_field("started", &format_utc(started));
    print_field(
        "finished",
        &format!("{} ({})", format_utc(finished), format_elapsed(elapsed)),
    );
}

/// A short human runtime: `12 ms`, `3.40 s`, or `1m 04s`.
fn format_elapsed(d: Duration) -> String {
    let secs = d.as_secs_f64();
    if secs < 1.0 {
        format!("{} ms", d.as_millis())
    } else if secs < 60.0 {
        format!("{secs:.2} s")
    } else {
        format!("{}m {:02}s", d.as_secs() / 60, d.as_secs() % 60)
    }
}

/// Print one `  label   value` line with the label padded to a common width.
fn print_field(label: &str, value: &str) {
    println!("  {label:<9}{value}");
}

/// Format a [`SystemTime`] as `YYYY-MM-DD HH:MM:SS UTC`, without pulling in a
/// date library (the civil-from-days algorithm by Howard Hinnant).
fn format_utc(t: SystemTime) -> String {
    let secs = t.duration_since(UNIX_EPOCH).map_or(0, |d| d.as_secs());
    let (days, rem) = (secs / 86_400, secs % 86_400);
    let (hour, min, sec) = (rem / 3_600, (rem % 3_600) / 60, rem % 60);

    let z = days as i64 + 719_468;
    let era = if z >= 0 { z } else { z - 146_096 } / 146_097;
    let doe = z - era * 146_097; // day-of-era, [0, 146096]
    let yoe = (doe - doe / 1_460 + doe / 36_524 - doe / 146_096) / 365; // [0, 399]
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100); // day-of-year, [0, 365]
    let mp = (5 * doy + 2) / 153; // month-pivot, [0, 11] (Mar=0)
    let day = doy - (153 * mp + 2) / 5 + 1; // [1, 31]
    let month = if mp < 10 { mp + 3 } else { mp - 9 }; // [1, 12]
    let year = yoe + era * 400 + i64::from(month <= 2);

    format!("{year:04}-{month:02}-{day:02} {hour:02}:{min:02}:{sec:02} UTC")
}

/// A `;`-delimited CSV writer at `path`.
fn writer(path: &Path) -> Result<csv::Writer<std::fs::File>> {
    csv::WriterBuilder::new()
        .delimiter(b';')
        .from_path(path)
        .with_context(|| format!("creating `{}`", path.display()))
}
