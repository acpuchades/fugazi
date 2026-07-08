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
//! equity is emitted to `returns.csv`. Both files are `,`-delimited.
//! After the loop the equity curve + blotter reduce to `metrics.yml`
//! (whole-run summary — see [`crate::metrics`]) and, under `-w N`, to
//! `metrics.csv` (non-overlapping N-bar windows) and `rolling.csv` (rolling
//! stride-1 windows). The console prints the whole-run headline block first;
//! under `-w` a second **windowed metrics** block follows it, showing
//! `mean ± std` across the non-overlapping windows for the same headline
//! stats — so the caller sees both the whole-run point estimate and its
//! cross-window dispersion side-by-side.
//!
//! Metrics cover the whole run — the strategy layer is opinion-free about
//! stability. A strategy that wants entries held off until every source it
//! consults has settled composes the check at the entry with `!stable`, i.e.
//! `!all [<entry>, !stable { signal: <entry> }]`.

use std::path::Path;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use anyhow::{Context, Result};
use fugazi::prelude::*;

use crate::backtest::{self, IterationInputs, IterationResult};
use crate::calendar::{self, AssetClass, BarsPerYearSpec, ScopedFrequency, WindowSpec};
use crate::costs::CostConfig;
use crate::data::DataFrame;
use crate::metrics;
use crate::spec::{PairsStrategySpec, StrategySpec};
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
    /// always written; `None` skips the CSVs. The raw CLI spec — a bar count
    /// or a duration; resolved to a bar count against the trading calendar
    /// inside [`run`]. The duration form requires `asset_class` and a
    /// resolvable bar cadence (`frequency` or auto-detection from
    /// `Atom::time`).
    pub windowed: Option<WindowSpec>,
    /// Configured cost models, resolved into a live [`TradingCosts`] per
    /// (symbol, frequency) at run time. See [`crate::costs`].
    pub cost_config: &'a CostConfig,
    /// `-f/--frequency` entries: plain `CODE` or `SYMBOL:CODE`. Resolved per
    /// iteration via [`crate::calendar::pick_frequency`]; falls through to
    /// detection when no entry matches.
    pub frequency: &'a [ScopedFrequency],
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
    let series = frame.atoms(&symbol)?;
    let atoms = series.atoms;
    let skipped_overlay_columns = series.skipped_columns;

    std::fs::create_dir_all(opts.out_dir)
        .with_context(|| format!("creating output dir `{}`", opts.out_dir.display()))?;

    let start = atoms.first().map_or("", |(t, _)| t.as_str());
    let end = atoms.last().map_or("", |(t, _)| t.as_str());
    // The effective bar cadence for both annualization and cost-scope
    // matching: a symbol-matching `-f/--frequency` entry wins, else we
    // auto-detect from the atoms' `time` field (populated by the loader).
    let effective_freq = calendar::pick_frequency(opts.frequency, &symbol)
        .or_else(|| calendar::detect_frequency_from_atoms(atoms.iter().map(|(_, a)| a)));
    // Resolve `bars_per_year`: a scope-matching `--bars-per-year` entry wins,
    // else fall through to the class × cadence calendar.
    let bars_per_year = calendar::pick_bars_per_year(opts.bars_per_year, &symbol, effective_freq)
        .unwrap_or_else(|| calendar::resolve(None, opts.asset_class, effective_freq));
    let no_cost_warning = !opts.costs_supplied;
    let windowed_bars = opts
        .windowed
        .map(|w| {
            w.resolve(effective_freq, opts.asset_class)
                .map_err(anyhow::Error::msg)
        })
        .transpose()
        .context("resolving `-w/--windowed`")?;
    let seconds_per_bar = opts
        .asset_class
        .zip(effective_freq)
        .map(|(class, freq)| class.trading_seconds_per_bar(freq));
    let inputs = IterationInputs {
        cash: opts.cash,
        bars_per_year,
        risk_free_rate: opts.risk_free_rate,
        cost_config: opts.cost_config,
        effective_freq,
        windowed: windowed_bars,
        seconds_per_bar,
    };
    // Print the inputs block up front so a long-running run still shows the
    // user what they asked for while it's working.
    if !opts.quiet {
        let costs_active = !opts.cost_config.resolve(&symbol, effective_freq).is_none();
        style::print_header("run", "backtest a strategy over CSV series");
        print_skipped_overlay_warning(&skipped_overlay_columns);
        print_inputs_block(opts, start, end, atoms.len(), costs_active);
        if no_cost_warning {
            print_no_cost_warning();
        }
    }

    let iter = backtest::run_iteration(spec, &atoms, &inputs);

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
        let dsr_context = metrics::windows_dsr_context(ws);
        write_windowed_csv(ws, &iter.bars, dsr_context, &opts.out_dir.join("metrics.csv"))?;
    }
    if let Some(rs) = iter.rolling.as_deref() {
        write_windowed_csv(rs, &iter.bars, None, &opts.out_dir.join("rolling.csv"))?;
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
        print_metrics_block(
            &iter.metrics,
            None,
            iter.gross_metrics.as_ref(),
            effective_freq,
        );
        if let Some(windows) = iter.windowed.as_deref() {
            print_windowed_metrics_block(windows);
        }
    }
    Ok(summary)
}

/// The pairs twin of [`run`]: drive a
/// [`PairsStrategy`](fugazi::strategies::PairsStrategy) over the two legs'
/// aligned atom streams. Same output shape (`trades.csv`, `returns.csv`,
/// `metrics.yml`, and the windowed CSVs under `-w`), so the caller's downstream
/// analysis pipeline is unchanged.
///
/// Time-alignment is an **inner join** on the `time` column: only bars where
/// both symbols have data are fed to the strategy. A mismatched pair produces
/// a run over the intersecting bars, with the count echoed in the run's
/// `period` line.
pub fn run_pairs(
    spec: &PairsStrategySpec,
    frame: &DataFrame,
    opts: &RunOptions,
) -> Result<Summary> {
    let started = SystemTime::now();
    let left_series = frame.atoms(&spec.left)?;
    let right_series = frame.atoms(&spec.right)?;
    let (bars, left_atoms, right_atoms) = join_pair_by_time(&left_series.atoms, &right_series.atoms);

    std::fs::create_dir_all(opts.out_dir)
        .with_context(|| format!("creating output dir `{}`", opts.out_dir.display()))?;

    let start = bars.first().map_or("", |t| t.as_str());
    let end = bars.last().map_or("", |t| t.as_str());
    // Pick the effective cadence off the left leg (both legs are expected to
    // share one cadence — the inner-join filters to the shared timeline).
    let effective_freq = calendar::pick_frequency(opts.frequency, &spec.left)
        .or_else(|| calendar::pick_frequency(opts.frequency, &spec.right))
        .or_else(|| {
            calendar::detect_frequency_from_atoms(left_series.atoms.iter().map(|(_, a)| a))
        });
    let bars_per_year =
        calendar::pick_bars_per_year(opts.bars_per_year, &spec.left, effective_freq)
            .or_else(|| calendar::pick_bars_per_year(opts.bars_per_year, &spec.right, effective_freq))
            .unwrap_or_else(|| calendar::resolve(None, opts.asset_class, effective_freq));
    let no_cost_warning = !opts.costs_supplied;
    let windowed_bars = opts
        .windowed
        .map(|w| {
            w.resolve(effective_freq, opts.asset_class)
                .map_err(anyhow::Error::msg)
        })
        .transpose()
        .context("resolving `-w/--windowed`")?;
    let seconds_per_bar = opts
        .asset_class
        .zip(effective_freq)
        .map(|(class, freq)| class.trading_seconds_per_bar(freq));
    let inputs = IterationInputs {
        cash: opts.cash,
        bars_per_year,
        risk_free_rate: opts.risk_free_rate,
        cost_config: opts.cost_config,
        effective_freq,
        windowed: windowed_bars,
        seconds_per_bar,
    };
    if !opts.quiet {
        let costs_active = !opts
            .cost_config
            .resolve(&spec.left, effective_freq)
            .is_none()
            || !opts
                .cost_config
                .resolve(&spec.right, effective_freq)
                .is_none();
        style::print_header("run", "pair-trade a two-leg strategy over CSV series");
        print_pairs_inputs_block(opts, spec, start, end, bars.len(), costs_active);
        if no_cost_warning {
            print_no_cost_warning();
        }
    }

    let iter =
        backtest::run_iteration_pairs(spec, &bars, &left_atoms, &right_atoms, &inputs);

    write_trades_csv(&iter, &opts.out_dir.join("trades.csv"))?;
    if !opts.quiet {
        println!("\n{}", style::bold("trades"));
        stream_trades(&iter);
    }

    write_returns_csv(&iter, &opts.out_dir.join("returns.csv"))?;

    metrics::write_yaml(&iter.metrics, &opts.out_dir.join("metrics.yml"))?;

    if let Some(ws) = iter.windowed.as_deref() {
        let dsr_context = metrics::windows_dsr_context(ws);
        write_windowed_csv(ws, &iter.bars, dsr_context, &opts.out_dir.join("metrics.csv"))?;
    }
    if let Some(rs) = iter.rolling.as_deref() {
        write_windowed_csv(rs, &iter.bars, None, &opts.out_dir.join("rolling.csv"))?;
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
        print_metrics_block(
            &iter.metrics,
            None,
            iter.gross_metrics.as_ref(),
            effective_freq,
        );
        if let Some(windows) = iter.windowed.as_deref() {
            print_windowed_metrics_block(windows);
        }
    }
    Ok(summary)
}

/// Inner-join the two legs' atom streams on their `time` label. Returns
/// `(times, left_atoms, right_atoms)` where index `i` corresponds to a bar
/// present in both legs. Each `atoms(...)` slice is sorted ascending by
/// `time` (by construction — `DataFrame::atoms` walks a `BTreeMap`), so a
/// simple two-cursor merge suffices.
fn join_pair_by_time(
    left: &[(String, Atom)],
    right: &[(String, Atom)],
) -> (Vec<String>, Vec<Atom>, Vec<Atom>) {
    let (mut times, mut ls, mut rs) = (Vec::new(), Vec::new(), Vec::new());
    let (mut i, mut j) = (0, 0);
    while i < left.len() && j < right.len() {
        match left[i].0.cmp(&right[j].0) {
            std::cmp::Ordering::Equal => {
                times.push(left[i].0.clone());
                ls.push(left[i].1.clone());
                rs.push(right[j].1.clone());
                i += 1;
                j += 1;
            }
            std::cmp::Ordering::Less => i += 1,
            std::cmp::Ordering::Greater => j += 1,
        }
    }
    (times, ls, rs)
}

fn print_pairs_inputs_block(
    opts: &RunOptions,
    spec: &PairsStrategySpec,
    start: &str,
    end: &str,
    bars: usize,
    costs_active: bool,
) {
    println!("{}", style::bold("inputs"));
    print_field("strategy", opts.strategy_label);
    print_field("pair", &format!("{} / {}", spec.left, spec.right));
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
///
/// `dsr_context = Some((n_trials, trial_variance))` appends a trailing
/// `selection.deflated_sharpe` column — the per-window DSR against the windows treated
/// as the trial population (see [`metrics::windows_dsr_context`] for the
/// caveats). Wired for `metrics.csv` only; `rolling.csv` passes `None`
/// because its heavy autocorrelation makes the trial-variance model unsound.
fn write_windowed_csv(
    windows: &[metrics::WindowMetrics],
    bars: &[String],
    dsr_context: Option<(usize, Real)>,
    path: &Path,
) -> Result<()> {
    let mut out = writer(path)?;
    let names = windows
        .first()
        .map(|w| metrics::flatten(&w.metrics))
        .unwrap_or_default();
    let mut header: Vec<String> = ["window_start", "window_end"]
        .into_iter()
        .map(String::from)
        .chain(names.iter().map(|(name, _)| (*name).to_string()))
        .collect();
    if dsr_context.is_some() {
        header.push("selection.deflated_sharpe".to_string());
    }
    out.write_record(&header)?;
    for window in windows {
        let mut record = vec![bars[window.start_bar].clone(), bars[window.end_bar].clone()];
        record.extend(
            metrics::flatten(&window.metrics)
                .into_iter()
                .map(|(_, value)| value.map(|v| v.to_string()).unwrap_or_default()),
        );
        if let Some((n_trials, trial_var)) = dsr_context {
            let m = &window.metrics;
            let dsr = fugazi::metrics::deflated_sharpe_from_stats(
                m.risk_adjusted.sharpe,
                m.returns.skewness,
                m.returns.kurtosis,
                m.run.bars,
                m.run.bars_per_year,
                n_trials,
                trial_var,
            );
            record.push(dsr.map(|v| v.to_string()).unwrap_or_default());
        }
        out.write_record(&record)?;
    }
    out.flush()?;
    Ok(())
}

/// A `,`-delimited CSV writer at `path`.
fn writer(path: &Path) -> Result<csv::Writer<std::fs::File>> {
    csv::WriterBuilder::new()
        .delimiter(b',')
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

/// The "skipped overlay columns" banner: non-numeric CSV columns that were
/// dropped from the overlay [`Schema`] because at least one value failed to
/// parse as [`Real`]. Silent when nothing was skipped.
fn print_skipped_overlay_warning(skipped: &[String]) {
    if skipped.is_empty() {
        return;
    }
    let msg = format!(
        "skipped non-numeric overlay column{}: {} \
         — not accessible via `!get`",
        if skipped.len() == 1 { "" } else { "s" },
        skipped.join(", "),
    );
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
/// print their gross twin so the cost drag is one line away. When
/// `bar_freq` is known, the `holding` line prints each bar count with a
/// duration twin in the bar cadence's own unit alphabet (`21d`, `4h`).
fn print_metrics_block(
    m: &metrics::Metrics,
    measured: Option<&str>,
    gross: Option<&metrics::Metrics>,
    bar_freq: Option<Frequency>,
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
    if let Some(text) = format_holding_line(m, bar_freq) {
        print_field("holding", &text);
    }
}

/// Compose the `holding` line: `avg N bars (~Xu) · min N (~Xu) · max N (~Xu)`,
/// the duration twin dropped when `bar_freq` is unknown. `None` when the run
/// booked no trades (all three legs are absent).
fn format_holding_line(m: &metrics::Metrics, bar_freq: Option<Frequency>) -> Option<String> {
    let avg = m.trades.average_bars;
    let min = m.trades.min_bars.map(|n| n as Real);
    let max = m.trades.max_bars.map(|n| n as Real);
    if avg.is_none() && min.is_none() && max.is_none() {
        return None;
    }
    let leg = |label: &str, bars: Option<Real>, precision: usize| -> Option<String> {
        let bars = bars?;
        let dur = bar_freq
            .map(|f| format!(" (~{})", format_bars_as_duration(bars, f)))
            .unwrap_or_default();
        Some(format!("{label} {bars:.*} bars{dur}", precision))
    };
    let parts: Vec<String> = [leg("avg", avg, 1), leg("min", min, 0), leg("max", max, 0)]
        .into_iter()
        .flatten()
        .collect();
    Some(parts.join(" · "))
}

/// Render `bars` bars of `freq` cadence as a duration in the cadence's own
/// unit alphabet (`21d`, `4h`, `26h` — `Frequency::from_str`-compatible for
/// integer counts). Fractional averages carry one decimal.
fn format_bars_as_duration(bars: Real, freq: Frequency) -> String {
    let (mult, letter) = match freq {
        Frequency::Minute(n) => (n, "m"),
        Frequency::Hour(n) => (n, "h"),
        Frequency::Day(n) => (n, "d"),
        Frequency::Week(n) => (n, "w"),
        Frequency::Month(n) => (n, "M"),
    };
    let total = bars * mult as Real;
    if (total - total.round()).abs() < 1e-6 {
        format!("{total:.0}{letter}")
    } else {
        format!("{total:.1}{letter}")
    }
}

fn format_ratio(v: Option<Real>) -> String {
    v.map_or_else(|| "—".to_string(), |r| format!("{r:.2}"))
}

fn format_pct(v: Option<Real>) -> String {
    v.map_or_else(|| "—".to_string(), |r| format!("{r:.1}%"))
}

/// Printed right after [`print_metrics_block`] under `-w`: each headline stat
/// becomes the cross-window `mean ± std` over the non-overlapping N-bar rows
/// in `metrics.csv`, so the caller sees both the whole-run single estimate
/// and the windowed dispersion around it side-by-side. Same field set and
/// layout as the whole-run block. Windows where a ratio is degenerate (no
/// losing trade for a profit factor, zero variance for Sharpe, …) are dropped
/// from that stat's aggregation via the `Option` filter — a stat with fewer
/// than one defined window prints as `—`.
///
/// No net-vs-gross split under `-w`: the pipeline currently only windows the
/// priced run, and printing whole-run gross next to windowed-net numbers would
/// mix aggregation scopes.
fn print_windowed_metrics_block(windows: &[metrics::WindowMetrics]) {
    println!("\n{}", style::bold("windowed metrics"));
    print_field(
        "windows",
        &format!(
            "{} × {} bars (non-overlapping)",
            windows.len(),
            windows.first().map_or(0, |w| w.metrics.run.bars),
        ),
    );
    let ann_mean = mean_std_of(windows, |m| Some(m.returns.annualized_mean_pct));
    let ann_vol = mean_std_of(windows, |m| Some(m.returns.annualized_volatility_pct));
    print_field(
        "return",
        &format!(
            "{} ann · vol {}",
            format_ms_signed_pct(ann_mean),
            format_ms_unsigned_pct(ann_vol),
        ),
    );
    print_field(
        "sharpe",
        &format_ms_ratio(mean_std_of(windows, |m| m.risk_adjusted.sharpe)),
    );
    print_field(
        "sortino",
        &format_ms_ratio(mean_std_of(windows, |m| m.risk_adjusted.sortino)),
    );
    print_field(
        "omega",
        &format_ms_ratio(mean_std_of(windows, |m| m.risk_adjusted.omega)),
    );
    let max_dd = mean_std_of(windows, |m| Some(m.drawdown.max_pct));
    let max_dur = mean_std_of(windows, |m| Some(m.drawdown.max_duration_bars as Real));
    print_field(
        "max_dd",
        &format!(
            "{} ({} bars)",
            format_ms_unsigned_pct(max_dd),
            format_ms_count(max_dur, 0),
        ),
    );
    print_field(
        "exposure",
        &format_ms_unsigned_pct(mean_std_of(windows, |m| Some(m.trades.exposure_pct))),
    );
    let trades = mean_std_of(windows, |m| Some(m.trades.total as Real));
    let win_rate = mean_std_of(windows, |m| m.trades.win_rate_pct);
    let pf = mean_std_of(windows, |m| m.trades.profit_factor);
    print_field(
        "trades",
        &format!(
            "{} · win {} · pf {}",
            format_ms_count(trades, 1),
            format_ms_unsigned_pct(win_rate),
            format_ms_ratio(pf),
        ),
    );
}

/// Project `f` across each window's `Metrics`, drop `None`s, and reduce to
/// `(mean, population_std)` via [`metrics::mean_std`]. `None` when no window
/// defines the stat.
fn mean_std_of<F>(windows: &[metrics::WindowMetrics], f: F) -> Option<(Real, Real)>
where
    F: Fn(&metrics::Metrics) -> Option<Real>,
{
    metrics::mean_std(windows.iter().filter_map(|w| f(&w.metrics)))
}

/// `+M.MM ± S.SS%` — signed mean (returns can be negative), unsigned stddev,
/// unit suffix once at the end.
fn format_ms_signed_pct(pair: Option<(Real, Real)>) -> String {
    pair.map_or_else(
        || "—".to_string(),
        |(m, s)| format!("{m:+.2} ± {s:.2}%"),
    )
}

/// `M.MM ± S.SS%` — unsigned mean (magnitudes, ratios in percent form).
fn format_ms_unsigned_pct(pair: Option<(Real, Real)>) -> String {
    pair.map_or_else(|| "—".to_string(), |(m, s)| format!("{m:.2} ± {s:.2}%"))
}

/// `M.MM ± S.SS` — unitless ratio (Sharpe, Sortino, Omega, profit factor).
fn format_ms_ratio(pair: Option<(Real, Real)>) -> String {
    pair.map_or_else(|| "—".to_string(), |(m, s)| format!("{m:.2} ± {s:.2}"))
}

/// `M ± S` at `precision` decimals — for counts (trades, drawdown duration
/// bars) treated as floats so a fractional mean survives the format.
fn format_ms_count(pair: Option<(Real, Real)>, precision: usize) -> String {
    pair.map_or_else(
        || "—".to_string(),
        |(m, s)| format!("{m:.*} ± {s:.*}", precision, precision),
    )
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
