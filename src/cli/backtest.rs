//! The backtest driver: walk one symbol's candles through a strategy and a
//! [`PaperWallet`], writing the result files and narrating the run.
//!
//! Each bar: feed the wallet the candle (it marks to `close`, bounds fills to the
//! bar's range, and fills any order queued on the previous bar at this `open`),
//! `update` the strategy, then `trade` it (queuing this bar's market orders and
//! booking any immediate stop). Every order appended to the blotter this bar —
//! the previous bar's market fill and this bar's stops alike — is emitted to
//! `trades.csv` stamped with this bar's `time` and the order's own fill price, and
//! the running equity is emitted to `returns.csv`. Both result files are written
//! `;`-delimited for Excel. After the loop, the recorded equity curve and fill
//! blotter are reduced into `metrics.yml` — see [`crate::metrics`] for the
//! catalogue (return moments, Sharpe/Sortino/Calmar, drawdown, round-trip
//! trade statistics).
//!
//! Console output (silenced by [`RunOptions::quiet`]) is a two-line banner (the
//! constant tool identity, then the active command), then three blocks: **inputs**
//! (strategy, params, period, capital, output), **trades** (each fill, with
//! its symbol, streamed as it happens), and **result** (bars, trades, capital
//! change, then start/finish times with elapsed runtime). A symbol is per-trade,
//! never a run-level field.

use std::path::Path;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use anyhow::{Context, Result};
use fugazi::prelude::*;

use crate::chart;
use crate::data::DataFrame;
use crate::metrics;
use crate::spec::StrategySpec;
use crate::style;

/// Pure metrics-only evaluation: drive `spec` over `candles` through a paper
/// wallet with `cash` starting funds and reduce the run to a [`metrics::Metrics`]
/// document. No filesystem, no printing — the shape `optimize` calls per grid
/// combination.
pub fn evaluate(
    spec: &StrategySpec,
    candles: &[(String, Candle)],
    cash: Real,
    bars_per_year: Real,
    risk_free_rate: Real,
) -> metrics::Metrics {
    let symbol = spec.symbol.clone();
    let mut strategy = spec.build();
    let mut wallet = PaperWallet::new(cash);
    let report = fugazi::backtest::run(
        &mut strategy,
        &mut wallet,
        symbol,
        candles.iter().map(|(_, c)| *c),
    );
    metrics::from_report(&report, bars_per_year, risk_free_rate)
}

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
    /// Bars per year used to annualize per-bar return moments in `metrics.yml`.
    pub bars_per_year: Real,
    /// Annualized risk-free rate as a fraction (e.g. `0.045` = 4.5% p.a.).
    /// Subtracted from annualized returns before Sharpe/Sortino/UPI and used
    /// as the per-bar threshold for Omega.
    pub risk_free_rate: Real,
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

    let start = candles.first().map_or("", |(t, _)| t.as_str());
    let end = candles.last().map_or("", |(t, _)| t.as_str());
    if !opts.quiet {
        style::print_header("run", "backtest a strategy over CSV series");
        print_inputs_block(opts, start, end, candles.len());
    }

    // Delegate the per-bar loop to the library primitive. Fills and the equity
    // curve come back as two parallel vectors, indexed by bar.
    let mut wallet = PaperWallet::new(opts.cash);
    let report = fugazi::backtest::run(
        &mut strategy,
        &mut wallet,
        symbol,
        candles.iter().map(|(_, c)| *c),
    );

    // Emit `trades.csv` and echo each fill in the same order the wallet booked
    // them; the CSV is `;`-delimited for Excel.
    let mut trades = writer(&opts.out_dir.join("trades.csv"))?;
    trades.write_record(["time", "symbol", "side", "units", "price", "kind"])?;
    if !opts.quiet {
        println!("\n{}", style::bold("trades"));
    }
    for fill in &report.fills {
        let order = &fill.order;
        let time = &candles[fill.bar].0;
        let side = match order.side {
            Side::Buy => "buy",
            Side::Sell => "sell",
        };
        // Which order booked the fill — a market order, or a resting stop /
        // take-profit the wallet triggered.
        let kind = match order.kind {
            OrderKind::Market => "market",
            OrderKind::Stop => "stop",
            OrderKind::TakeProfit => "take_profit",
        };
        trades.write_record([
            time,
            &order.symbol,
            side,
            &order.units.to_string(),
            &order.price.to_string(),
            kind,
        ])?;
        if !opts.quiet {
            // Columns mirror trades.csv: time, symbol, side, units, price, kind.
            // Each trade carries its own symbol, so this stays correct for a
            // future multi-symbol strategy. Pad the side to width before
            // coloring it (escape codes would otherwise break the alignment).
            let side_padded = format!("{side:<4}");
            let side_colored = match order.side {
                Side::Buy => style::green(&side_padded),
                Side::Sell => style::red(&side_padded),
            };
            println!(
                "  {}  {:<6}  {side_colored} {:.4} @ {:.2}  {}",
                style::dim(time),
                order.symbol,
                order.units,
                order.price,
                style::dim(kind),
            );
        }
    }
    trades.flush()?;

    // Emit `returns.csv` from the equity curve — fractional bar-to-bar return
    // (0.05 = +5%), not percent, matching the metric math's convention.
    let per_bar = fugazi::metrics::per_bar_returns(&report.equity_curve, report.initial_equity);
    let mut returns = writer(&opts.out_dir.join("returns.csv"))?;
    returns.write_record(["time", "equity", "return"])?;
    for (i, (time, _)) in candles.iter().enumerate() {
        let equity = report.equity_curve[i];
        let ret = per_bar[i];
        returns.write_record([time, &equity.to_string(), &ret.to_string()])?;
    }
    returns.flush()?;

    let final_equity = report.equity_curve.last().copied().unwrap_or(opts.cash);
    let summary = Summary {
        final_equity,
        return_pct: if opts.cash != 0.0 {
            (final_equity - opts.cash) / opts.cash * 100.0
        } else {
            0.0
        },
        trades: report.fills.len(),
        bars: report.equity_curve.len(),
    };

    // Reduce the recorded blotter + equity curve to a metrics document and
    // persist it alongside the CSVs, then echo the top-line ratios in a
    // dedicated console block.
    let m = metrics::from_report(&report, opts.bars_per_year, opts.risk_free_rate);
    metrics::write_yaml(&m, &opts.out_dir.join("metrics.yml"))?;

    let times: Vec<String> = candles.iter().map(|(t, _)| t.clone()).collect();
    chart::write_equity_curve(
        &report.equity_curve,
        &times,
        opts.cash,
        &opts.out_dir.join("equity.png"),
    )?;

    let finished = SystemTime::now();
    if !opts.quiet {
        print_result_block(opts, &summary, started, finished);
        print_metrics_block(&m);
    }
    Ok(summary)
}

/// The "inputs" block: what this run was given. Timing (start/finish) lives in
/// the result block, since it's not an input.
///
/// No `symbol` line: a symbol is a property of each trade, not of the run (see
/// the trade stream and `trades.csv`), so this stays correct for a future
/// multi-symbol strategy.
fn print_inputs_block(opts: &RunOptions, start: &str, end: &str, bars: usize) {
    println!("{}", style::bold("inputs"));
    print_field("strategy", opts.strategy_label);
    print_field("params", opts.params);
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
    println!("\n{}", style::bold("result"));
    print_field("bars", &s.bars.to_string());
    print_field("trades", &s.trades.to_string());
    let delta = s.final_equity - opts.cash;
    let change = format!("{delta:+.2}, {:+.2}%", s.return_pct);
    // Green for a gain, red for a loss — the run's bottom line at a glance.
    let change = if delta >= 0.0 {
        style::green(&change)
    } else {
        style::red(&change)
    };
    print_field(
        "capital",
        &format!("{:.2} → {:.2}  ({change})", opts.cash, s.final_equity),
    );
    let elapsed = finished.duration_since(started).unwrap_or_default();
    print_field("started", &format_utc(started));
    print_field(
        "finished",
        &format!("{} ({})", format_utc(finished), format_elapsed(elapsed)),
    );
}

/// The "metrics" block: a compact summary of `metrics.yml`'s headline figures.
/// Only the most-referenced ones are surfaced here (annualized return + vol,
/// Sharpe/Sortino/Omega, max drawdown, exposure, trade count + win rate +
/// profit factor); the file itself carries the full set.
fn print_metrics_block(m: &metrics::Metrics) {
    println!("\n{}", style::bold("metrics"));
    print_field(
        "return",
        &format!(
            "{:+.2}% ann · vol {:.2}%",
            m.returns.annualized_mean_pct, m.returns.annualized_volatility_pct
        ),
    );
    print_field("sharpe", &format_ratio(m.risk_adjusted.sharpe));
    print_field("sortino", &format_ratio(m.risk_adjusted.sortino));
    print_field("omega", &format_ratio(m.risk_adjusted.omega));
    print_field(
        "max_dd",
        &format!(
            "{:.2}% ({} bars)",
            m.drawdown.max_pct, m.drawdown.max_duration_bars
        ),
    );
    print_field("exposure", &format!("{:.1}%", m.trades.exposure_pct));
    print_field(
        "trades",
        &format!(
            "{} · win {} · pf {}",
            m.trades.total,
            format_pct(m.trades.win_rate_pct),
            format_ratio(m.trades.profit_factor),
        ),
    );
}

/// A ratio to two decimals, or `—` when its denominator was degenerate and the
/// value is `None` (see the `skip_serializing_if` fields on the metrics types).
fn format_ratio(v: Option<Real>) -> String {
    v.map_or_else(|| "—".to_string(), |r| format!("{r:.2}"))
}

fn format_pct(v: Option<Real>) -> String {
    v.map_or_else(|| "—".to_string(), |r| format!("{r:.1}%"))
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
    // Pad to the common width first, then dim — the escape codes are invisible
    // bytes that would otherwise throw off the `{:<9}` alignment.
    println!("  {}{value}", style::dim(&format!("{label:<9}")));
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
