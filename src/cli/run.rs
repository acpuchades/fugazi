//! The `run` subcommand's IO driver.
//!
//! Owns everything user-facing: file writes (`trades.csv`, `returns.csv`,
//! `metrics.yml`, and the optional `metrics.csv`/`rolling.csv` under `-w N`),
//! the tiered console banners (**inputs** / **trades** / **result** /
//! **metrics**), and the wall-clock timing. Evaluation is delegated to
//! [`crate::backtest::run_iteration`] — this module never touches the
//! per-bar loop or the metrics reduction itself; it just wraps the pure
//! payload with IO.
//!
//! ## Output shape
//!
//! Per bar: feed the wallet the candle (in [`run_iteration`]); the priced
//! blotter comes back sorted by fill index. Every order is written to
//! `trades.csv` with its bar's `time` and its own fill price. The running
//! equity is emitted to `returns.csv`. Both files are `;`-delimited for
//! Excel. After the loop the equity curve + blotter reduce to `metrics.yml`
//! (whole-run summary — see [`crate::metrics`]) and, under `-w N`, to
//! `metrics.csv` (non-overlapping N-bar windows) and `rolling.csv` (rolling
//! stride-1 windows).
//!
//! Metrics cover the whole run — the strategy layer is opinion-free about
//! stability. A strategy that wants entries held off until every source it
//! consults has settled composes the check at the entry with `!stable`, i.e.
//! `!all [<entry>, !stable { signal: <entry> }]`.

use std::num::NonZeroUsize;
use std::path::Path;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use anyhow::{Context, Result};
use fugazi::prelude::*;

use crate::backtest::{self, IterationInputs, IterationResult};
use crate::calendar::{self, AssetClass, BarsPerYearSpec, FrequencySpec};
use crate::costs::CostConfig;
use crate::data::DataFrame;
use crate::metrics;
use crate::spec::StrategySpec;
use crate::style;

/// Console-logging knobs plus the run's inputs, threaded in from the CLI args.
/// Held by the `run` subcommand's driver; never enters [`crate::backtest`],
/// which stays IO-free.
pub struct RunOptions<'a> {
    /// Initial cash for the paper wallet.
    pub cash: Real,
    /// Directory to write `trades.csv` / `returns.csv` into.
    pub out_dir: &'a Path,
    /// A short label for the strategy source (file path or `(inline)`), echoed
    /// in the run block.
    pub strategy_label: &'a str,
    /// A one-line view of the effective params (`NAME=value, …`), echoed in
    /// the run block.
    pub params: &'a str,
    /// `--bars-per-year` entries: each is a plain `N` or a `SYMBOL[FREQ]:N`
    /// override. Resolved per iteration via
    /// [`crate::calendar::pick_bars_per_year`].
    pub bars_per_year: &'a [BarsPerYearSpec],
    /// Trading-calendar shortcut (`--stocks`/`--forex`/`--crypto`). `None`
    /// falls back to [`AssetClass::Stocks`].
    pub asset_class: Option<AssetClass>,
    /// Annualized risk-free rate as a fraction (e.g. `0.045` = 4.5% p.a.).
    pub risk_free_rate: Real,
    /// When set, also emit windowed reductions at this window length: one row
    /// per non-overlapping window in `metrics.csv`, one row per rolling
    /// (stride-1) window in `rolling.csv`. `metrics.yml` (whole-run) is
    /// always written; `None` skips the CSVs.
    pub windowed: Option<NonZeroUsize>,
    /// Configured cost models, resolved into a live [`TradingCosts`] per
    /// (symbol, frequency) at run time. See [`crate::costs`].
    pub cost_config: &'a CostConfig,
    /// `-f/--frequency` entries: plain `CODE` or `SYMBOL:CODE`. Resolved per
    /// iteration via [`crate::calendar::pick_frequency`]; falls through to
    /// detection when no entry matches.
    pub frequency: &'a [FrequencySpec],
    /// Whether the user passed at least one `--costs` flag (even `--costs
    /// none`). Governs the "no cost model set" warning banner.
    pub costs_supplied: bool,
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

/// Run `spec` over `frame` per `opts` — resolve inputs, delegate the pure
/// work to [`backtest::run_iteration`], and write the result files +
/// narrate the tiered run/trade/result/metrics logs.
pub fn run(spec: &StrategySpec, frame: &DataFrame, opts: &RunOptions) -> Result<Summary> {
    let started = SystemTime::now();
    let symbol = spec.symbol.clone();
    let candles = frame.candles(&symbol)?;

    std::fs::create_dir_all(opts.out_dir)
        .with_context(|| format!("creating output dir `{}`", opts.out_dir.display()))?;

    let start = candles.first().map_or("", |(t, _)| t.as_str());
    let end = candles.last().map_or("", |(t, _)| t.as_str());
    // The effective bar cadence for both annualization and cost-scope
    // matching: a symbol-matching `-f/--frequency` entry wins, else we
    // auto-detect from the strategy's dominant `(symbol, freq)` series in
    // the frame.
    let effective_freq = calendar::pick_frequency(opts.frequency, &symbol).or_else(|| {
        frame
            .dominant_series_times(&symbol)
            .and_then(calendar::detect_frequency)
    });
    // Resolve `bars_per_year`: a scope-matching `--bars-per-year` entry wins,
    // else fall through to the class × cadence calendar.
    let bars_per_year = calendar::pick_bars_per_year(opts.bars_per_year, &symbol, effective_freq)
        .unwrap_or_else(|| calendar::resolve(None, opts.asset_class, effective_freq));
    let no_cost_warning = !opts.costs_supplied;
    let inputs = IterationInputs {
        cash: opts.cash,
        bars_per_year,
        risk_free_rate: opts.risk_free_rate,
        cost_config: opts.cost_config,
        effective_freq,
        windowed: opts.windowed,
    };
    // Print the inputs block up front so a long-running run still shows the
    // user what they asked for while it's working.
    if !opts.quiet {
        let costs_active = !opts.cost_config.resolve(&symbol, effective_freq).is_none();
        style::print_header("run", "backtest a strategy over CSV series");
        print_inputs_block(opts, start, end, candles.len(), costs_active);
        if no_cost_warning {
            print_no_cost_warning();
        }
    }

    let iter = backtest::run_iteration(spec, &candles, &inputs);

    // Emit `trades.csv` and echo each fill in the same order the wallet booked
    // them. The console stream matches the CSV row-for-row.
    write_trades_csv(&iter, &opts.out_dir.join("trades.csv"))?;
    if !opts.quiet {
        println!("\n{}", style::bold("trades"));
        stream_trades(&iter);
    }

    write_returns_csv(&iter, &opts.out_dir.join("returns.csv"))?;

    metrics::write_yaml(&iter.metrics, &opts.out_dir.join("metrics.yml"))?;

    if let Some(ws) = iter.windowed.as_deref() {
        write_windowed_csv(ws, &iter.bars, &opts.out_dir.join("metrics.csv"))?;
    }
    if let Some(rs) = iter.rolling.as_deref() {
        write_windowed_csv(rs, &iter.bars, &opts.out_dir.join("rolling.csv"))?;
    }

    let summary = Summary {
        final_equity: iter.summary.final_equity,
        return_pct: if opts.cash != 0.0 {
            (iter.summary.final_equity - opts.cash) / opts.cash * 100.0
        } else {
            0.0
        },
        trades: iter.summary.trades,
        bars: iter.summary.bars,
    };

    let finished = SystemTime::now();
    if !opts.quiet {
        print_result_block(opts, &summary, started, finished);
        print_metrics_block(&iter.metrics, None, iter.gross_metrics.as_ref());
    }
    Ok(summary)
}

// ---------------------------------------------------------------------------
// CSV writers
// ---------------------------------------------------------------------------

/// Write `trades.csv` from an [`IterationResult`]. `commission` is only
/// present when the iteration's costs were active.
fn write_trades_csv(iter: &IterationResult, path: &Path) -> Result<()> {
    let mut w = writer(path)?;
    let header: &[&str] = if iter.costs_active {
        &["time", "symbol", "side", "units", "price", "kind", "commission"]
    } else {
        &["time", "symbol", "side", "units", "price", "kind"]
    };
    w.write_record(header)?;
    for fill in &iter.report.fills {
        let order = &fill.order;
        let time = &iter.bars[fill.bar];
        let side = match order.side {
            Side::Buy => "buy",
            Side::Sell => "sell",
        };
        let kind = match order.kind {
            OrderKind::Market => "market",
            OrderKind::Stop => "stop",
            OrderKind::TakeProfit => "take_profit",
        };
        let mut row: Vec<String> = vec![
            time.clone(),
            order.symbol.clone(),
            side.to_string(),
            order.units.to_string(),
            order.price.to_string(),
            kind.to_string(),
        ];
        if iter.costs_active {
            row.push(order.commission.to_string());
        }
        w.write_record(&row)?;
    }
    w.flush()?;
    Ok(())
}

/// Write `returns.csv` from an [`IterationResult`].
fn write_returns_csv(iter: &IterationResult, path: &Path) -> Result<()> {
    let mut w = writer(path)?;
    w.write_record(["time", "equity", "return"])?;
    let per_bar =
        fugazi::metrics::per_bar_returns(&iter.report.equity_curve, iter.report.initial_equity);
    for (i, time) in iter.bars.iter().enumerate() {
        let equity = iter.report.equity_curve[i];
        let ret = per_bar[i];
        w.write_record([time.as_str(), &equity.to_string(), &ret.to_string()])?;
    }
    w.flush()?;
    Ok(())
}

/// Echo each fill of `iter` to the console — one line per row, matching
/// the CSV order.
fn stream_trades(iter: &IterationResult) {
    for fill in &iter.report.fills {
        let order = &fill.order;
        let time = &iter.bars[fill.bar];
        let side = match order.side {
            Side::Buy => "buy",
            Side::Sell => "sell",
        };
        let kind = match order.kind {
            OrderKind::Market => "market",
            OrderKind::Stop => "stop",
            OrderKind::TakeProfit => "take_profit",
        };
        let side_padded = format!("{side:<4}");
        let side_colored = match order.side {
            Side::Buy => style::green(&side_padded),
            Side::Sell => style::red(&side_padded),
        };
        if iter.costs_active {
            println!(
                "  {}  {:<6}  {side_colored} {:.4} @ {:.2}  {} · fee {:.4}",
                style::dim(time),
                order.symbol,
                order.units,
                order.price,
                style::dim(kind),
                order.commission,
            );
        } else {
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
}

/// Emit a windowed-metrics CSV to `path`: one row per window —
/// `window_start` / `window_end` (the times of the window's first and last
/// bars) followed by the full metric catalogue, one column per dotted
/// `metrics.yml` name. A metric that is degenerate in a window (no trades,
/// zero variance, …) is an empty cell there. Shared between the
/// non-overlapping (`metrics.csv`) and rolling (`rolling.csv`) writes.
fn write_windowed_csv(
    windows: &[metrics::WindowMetrics],
    bars: &[String],
    path: &Path,
) -> Result<()> {
    let mut out = writer(path)?;
    let names = windows
        .first()
        .map(|w| metrics::flatten(&w.metrics))
        .unwrap_or_default();
    let header = ["window_start", "window_end"]
        .into_iter()
        .chain(names.iter().map(|(name, _)| *name));
    out.write_record(header)?;
    for window in windows {
        let mut record = vec![bars[window.start_bar].clone(), bars[window.end_bar].clone()];
        record.extend(
            metrics::flatten(&window.metrics)
                .into_iter()
                .map(|(_, value)| value.map(|v| v.to_string()).unwrap_or_default()),
        );
        out.write_record(&record)?;
    }
    out.flush()?;
    Ok(())
}

/// A `;`-delimited CSV writer at `path`.
fn writer(path: &Path) -> Result<csv::Writer<std::fs::File>> {
    csv::WriterBuilder::new()
        .delimiter(b';')
        .from_path(path)
        .with_context(|| format!("creating `{}`", path.display()))
}

// ---------------------------------------------------------------------------
// Console blocks (single-run mode)
// ---------------------------------------------------------------------------

/// The "inputs" block: what this run was given. Timing (start/finish) lives
/// in the result block, since it's not an input.
fn print_inputs_block(opts: &RunOptions, start: &str, end: &str, bars: usize, costs_active: bool) {
    println!("{}", style::bold("inputs"));
    print_field("strategy", opts.strategy_label);
    print_field("params", opts.params);
    print_field("period", &format!("{start} → {end} ({bars} bars)"));
    print_field("capital", &format!("{:.2}", opts.cash));
    if costs_active {
        print_field("costs", "active (commission/spread/slippage applied)");
    } else if opts.costs_supplied {
        print_field("costs", "none (explicit)");
    }
    print_field("output", &opts.out_dir.display().to_string());
}

/// The default-cost warning banner: nothing was set, so every fill was
/// frictionless.
fn print_no_cost_warning() {
    let msg = "no cost model set — commission, spread, and slippage are zero; results are frictionless";
    println!("  {} {msg}", style::yellow("warn"));
}

/// The "result" block: the run's outputs, then its wall-clock timing.
fn print_result_block(opts: &RunOptions, s: &Summary, started: SystemTime, finished: SystemTime) {
    println!("\n{}", style::bold("result"));
    print_field("bars", &s.bars.to_string());
    print_field("trades", &s.trades.to_string());
    let delta = s.final_equity - opts.cash;
    let change = format!("{delta:+.2}, {:+.2}%", s.return_pct);
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

/// The "metrics" block: a compact summary of `metrics.yml`'s headline
/// figures. When `gross` is set (a costed run), decision-relevant rows also
/// print their gross twin so the cost drag is one line away.
fn print_metrics_block(
    m: &metrics::Metrics,
    measured: Option<&str>,
    gross: Option<&metrics::Metrics>,
) {
    println!("\n{}", style::bold("metrics"));
    if let Some(measured) = measured {
        print_field("measured", measured);
    }
    if let Some(g) = gross {
        let net = m.returns.cagr_pct.map_or("—".to_string(), |v| format!("{v:+.2}%"));
        let gross = g.returns.cagr_pct.map_or("—".to_string(), |v| format!("{v:+.2}%"));
        print_field("cagr", &format!("net {net} · gross {gross}"));
    }
    print_field(
        "return",
        &format!(
            "{:+.2}% ann · vol {:.2}%",
            m.returns.annualized_mean_pct, m.returns.annualized_volatility_pct
        ),
    );
    if let Some(g) = gross {
        let net = format_ratio(m.risk_adjusted.sharpe);
        let gross = format_ratio(g.risk_adjusted.sharpe);
        print_field("sharpe", &format!("net {net} · gross {gross}"));
    } else {
        print_field("sharpe", &format_ratio(m.risk_adjusted.sharpe));
    }
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

fn format_ratio(v: Option<Real>) -> String {
    v.map_or_else(|| "—".to_string(), |r| format!("{r:.2}"))
}

fn format_pct(v: Option<Real>) -> String {
    v.map_or_else(|| "—".to_string(), |r| format!("{r:.1}%"))
}

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

fn print_field(label: &str, value: &str) {
    println!("  {}{value}", style::dim(&format!("{label:<9}")));
}

/// Format a [`SystemTime`] as `YYYY-MM-DD HH:MM:SS UTC`, without pulling in
/// a date library (Howard Hinnant's civil-from-days algorithm).
fn format_utc(t: SystemTime) -> String {
    let secs = t.duration_since(UNIX_EPOCH).map_or(0, |d| d.as_secs());
    let (days, rem) = (secs / 86_400, secs % 86_400);
    let (hour, min, sec) = (rem / 3_600, (rem % 3_600) / 60, rem % 60);

    let z = days as i64 + 719_468;
    let era = if z >= 0 { z } else { z - 146_096 } / 146_097;
    let doe = z - era * 146_097;
    let yoe = (doe - doe / 1_460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let day = doy - (153 * mp + 2) / 5 + 1;
    let month = if mp < 10 { mp + 3 } else { mp - 9 };
    let year = yoe + era * 400 + i64::from(month <= 2);

    format!("{year:04}-{month:02}-{day:02} {hour:02}:{min:02}:{sec:02} UTC")
}
