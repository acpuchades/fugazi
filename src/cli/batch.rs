//! The `run --single` batch driver — parallel per-`(symbol, freq)` runs.
//!
//! When the input frame carries several `(symbol, freq)` series (see
//! [`crate::data::DataFrame::groups`]), `--single` iterates the strategy
//! over each group. Each iteration is a full [`crate::backtest::run_iteration`]
//! call on the rayon pool (from [`crate::pool::build_pool`], sized by
//! `-j/--jobs`); results are collected on the main thread and written under
//! the *shape rule*:
//!
//! * Iterations bucket by their fully-expanded output-dir (`--output-dir`
//!   with `%SYMBOL`/`%FREQ` substituted, see [`crate::sigils`]).
//! * Within a bucket, sigils whose values differ across iterations are
//!   *loose* and become leading columns on every CSV (`symbol`, then
//!   `freq`).
//! * A single-iteration bucket keeps the pre-batch shape byte-for-byte —
//!   `metrics.yml` + (under `-w N`) `metrics.csv` + `rolling.csv`. A
//!   multi-iteration bucket drops `metrics.yml` in favour of a tabular
//!   `metrics.csv` (one row per iteration when not windowed, `iterations ×
//!   windows` rows when `-w N`).
//!
//! CSV rows are sorted `(symbol, freq, time)` (or `(symbol, freq)` /
//! `(symbol, freq, window_start)` where there is no `time`), with `freq`
//! ordered by duration via [`crate::calendar::Frequency`]'s [`Ord`] impl.
//!
//! ## Silent-skip semantics
//!
//! If a per-iteration [`crate::spec::StrategySpec`] (built after
//! `%SYMBOL`/`%FREQ` substitution) resolves to a symbol other than the
//! group's own — e.g. a strategy with a hardcoded `symbol: BTC` on a frame
//! that also carries AAPL — the AAPL iteration is silently skipped. The
//! user's decision here (matches their explicit ask) — a hardcoded strategy
//! naturally targets one symbol, so iterating the others would produce
//! meaningless output.

use std::collections::BTreeMap;
use std::num::NonZeroUsize;
use std::path::{Path, PathBuf};
use std::str::FromStr;

use anyhow::{Context, Result};
use fugazi::prelude::*;
use rayon::prelude::*;

use crate::backtest::{self, IterationInputs, IterationResult};
use crate::calendar::{self, AssetClass, BarsPerYearSpec, Frequency, FrequencySpec};
use crate::costs::CostConfig;
use crate::data::{DataFrame, Group};
use crate::metrics;
use crate::params::{self, ParamSpec};
use crate::pool;
use crate::run::{self as run_mod, Summary};
use crate::sigils;
use crate::spec::StrategySpec;
use crate::style;

/// Inputs the batch driver consumes. Shape mirrors [`run_mod::RunOptions`]
/// with two changes: the strategy is passed as *raw text + param specs*
/// (each iteration substitutes its own `%SYMBOL`/`%FREQ` values before
/// building the spec), and a `jobs` field sizes the rayon pool.
pub struct BatchOptions<'a> {
    pub cash: Real,
    /// `--output-dir` — may contain `%SYMBOL`/`%FREQ` sigils that expand
    /// per iteration; iterations whose expanded dir collides get bucketed
    /// together (shape rule below).
    pub out_dir: &'a Path,
    pub strategy_text: &'a str,
    pub strategy_label: &'a str,
    pub params_label: &'a str,
    pub param_specs: &'a [ParamSpec],
    pub bars_per_year: &'a [BarsPerYearSpec],
    pub asset_class: Option<AssetClass>,
    pub risk_free_rate: Real,
    pub windowed: Option<NonZeroUsize>,
    pub cost_config: &'a CostConfig,
    pub frequency: &'a [FrequencySpec],
    pub costs_supplied: bool,
    pub jobs: Option<usize>,
    pub quiet: bool,
}

/// Run the strategy over every group in `frame`'s `(symbol, freq)`
/// enumeration, one [`crate::backtest::run_iteration`] call per group on
/// the rayon pool. Returns an aggregate [`Summary`] whose counters sum
/// across every non-skipped iteration.
pub fn run(frame: &DataFrame, opts: &BatchOptions) -> Result<Summary> {
    let groups = frame.groups()?;
    if !opts.quiet {
        style::print_header("run", "batch backtest over multi-series frame");
        println!("{}", style::bold("inputs"));
        print_field("strategy", opts.strategy_label);
        print_field("params", opts.params_label);
        print_field("series", &format!("{} group(s)", groups.len()));
        print_field("capital", &format!("{:.2}", opts.cash));
        print_field("output", &opts.out_dir.display().to_string());
        if !opts.costs_supplied {
            println!(
                "  {} no cost model set — commission, spread, and slippage are zero; \
                 results are frictionless",
                style::yellow("warn"),
            );
        }
    }

    let pool = pool::build_pool(opts.jobs)?;
    let slots: Vec<Result<Option<IterationSlot>>> = pool.install(|| {
        groups.par_iter().map(|group| iterate_group(group, opts)).collect()
    });

    let mut ready: Vec<IterationSlot> = Vec::new();
    let mut skipped = 0usize;
    for slot in slots {
        match slot? {
            Some(s) => ready.push(s),
            None => skipped += 1,
        }
    }

    // Bucket iterations by their fully-expanded output dir; shape rule and
    // sigil-column widening apply per bucket.
    let mut by_dir: BTreeMap<PathBuf, Vec<IterationSlot>> = BTreeMap::new();
    for slot in ready {
        by_dir.entry(slot.output_dir.clone()).or_default().push(slot);
    }

    let mut agg = Summary {
        final_equity: 0.0,
        return_pct: 0.0,
        trades: 0,
        bars: 0,
    };
    let mut buckets_written = 0usize;
    for (dir, bucket) in &by_dir {
        std::fs::create_dir_all(dir)
            .with_context(|| format!("creating output dir `{}`", dir.display()))?;
        write_bucket(dir, bucket, opts)?;
        buckets_written += 1;
        for slot in bucket {
            agg.bars += slot.iteration.summary.bars;
            agg.trades += slot.iteration.summary.trades;
            agg.final_equity += slot.iteration.summary.final_equity;
        }
    }

    if !opts.quiet {
        println!("\n{}", style::bold("result"));
        print_field("iterations", &format!("{}", by_dir.values().map(Vec::len).sum::<usize>()));
        print_field("skipped", &skipped.to_string());
        print_field("output dirs", &buckets_written.to_string());
        print_field("bars total", &agg.bars.to_string());
        print_field("trades total", &agg.trades.to_string());
    }
    Ok(agg)
}

/// One iteration's payload plus the fully-expanded output dir it belongs
/// to. Grouping by `output_dir` is what determines which sigils are loose.
struct IterationSlot {
    iteration: IterationResult,
    output_dir: PathBuf,
}

/// Run the strategy over one group. Returns `None` (silent skip) when the
/// per-iteration substituted spec pins a symbol other than the group's own.
fn iterate_group(group: &Group, opts: &BatchOptions) -> Result<Option<IterationSlot>> {
    // Effective bar cadence for this group: `-f/--frequency` scoped match
    // first, else the group's `freq` column (which the frame carries per
    // row when the CSV had one), else auto-detect from the times.
    let effective_freq = calendar::pick_frequency(opts.frequency, &group.symbol)
        .or_else(|| group.freq.as_deref().and_then(|s| Frequency::from_str(s).ok()))
        .or_else(|| {
            calendar::detect_frequency(group.candles.iter().map(|(t, _)| t.as_str()))
        });

    // Substitute sigils into `--params` values before folding them into the
    // param table; the strategy YAML's `!param` substitution then sees the
    // iteration's actual (symbol, freq).
    let param_table = params::table_for(opts.param_specs, &group.symbol, effective_freq)
        .with_context(|| format!("resolving params for group {}", label(group)))?;
    let spec = StrategySpec::from_text_with_params(opts.strategy_text, &param_table)
        .with_context(|| format!("parsing strategy for group {}", label(group)))?;

    // Silent skip: the strategy resolved to a different symbol than this
    // group's — a hardcoded-symbol strategy on a multi-symbol frame. The
    // user asked for silent skip; no warning here.
    if spec.symbol != group.symbol {
        return Ok(None);
    }

    let bars_per_year =
        calendar::pick_bars_per_year(opts.bars_per_year, &group.symbol, effective_freq)
            .unwrap_or_else(|| calendar::resolve(None, opts.asset_class, effective_freq));

    let inputs = IterationInputs {
        cash: opts.cash,
        bars_per_year,
        risk_free_rate: opts.risk_free_rate,
        cost_config: opts.cost_config,
        effective_freq,
        windowed: opts.windowed,
    };
    let iteration = backtest::run_iteration(&spec, &group.candles, &inputs);
    let output_dir = sigils::expand_path(opts.out_dir, &group.symbol, effective_freq);
    Ok(Some(IterationSlot { iteration, output_dir }))
}

/// Human label for a group in error messages: `BTC[1h]` when freq known,
/// `BTC` when not.
fn label(group: &Group) -> String {
    match group.freq.as_deref() {
        Some(f) => format!("{}[{}]", group.symbol, f),
        None => group.symbol.clone(),
    }
}

/// Write every result file for one output-dir bucket per the shape rule
/// (see the module docs).
fn write_bucket(dir: &Path, bucket: &[IterationSlot], opts: &BatchOptions) -> Result<()> {
    // A bucket with exactly one iteration is byte-identical to a single-run:
    // no loose sigils, no extra columns, `metrics.yml` retained.
    let single = bucket.len() == 1;
    let symbols: std::collections::BTreeSet<_> =
        bucket.iter().map(|s| s.iteration.symbol.clone()).collect();
    let freqs: std::collections::BTreeSet<_> =
        bucket.iter().map(|s| s.iteration.freq).collect();
    let loose_symbol = symbols.len() > 1;
    let loose_freq = freqs.len() > 1;

    // Iterate sorted for deterministic row order in every output stream.
    let mut sorted: Vec<&IterationSlot> = bucket.iter().collect();
    sorted.sort_by(|a, b| {
        a.iteration
            .symbol
            .cmp(&b.iteration.symbol)
            .then_with(|| a.iteration.freq.cmp(&b.iteration.freq))
    });

    // trades.csv
    write_shared_trades(&sorted, &dir.join("trades.csv"), loose_symbol, loose_freq)?;
    // returns.csv
    write_shared_returns(&sorted, &dir.join("returns.csv"), loose_symbol, loose_freq)?;

    // Whole-run summary shape:
    //  • single-iteration bucket + not windowed → metrics.yml.
    //  • single-iteration bucket + windowed → metrics.yml + metrics.csv (windowed) + rolling.csv.
    //  • multi-iteration bucket + not windowed → metrics.csv (iteration rows with loose cols).
    //  • multi-iteration bucket + windowed → metrics.csv (iterations × windows) + rolling.csv,
    //    both with loose cols. No metrics.yml — the tabular row already carries the aggregate.
    if single {
        metrics::write_yaml(&sorted[0].iteration.metrics, &dir.join("metrics.yml"))?;
    } else if opts.windowed.is_none() {
        write_shared_metrics_csv(&sorted, &dir.join("metrics.csv"), loose_symbol, loose_freq)?;
    }

    if opts.windowed.is_some() {
        write_shared_windowed(
            &sorted,
            &dir.join("metrics.csv"),
            loose_symbol,
            loose_freq,
            WindowKind::NonOverlapping,
        )?;
        write_shared_windowed(
            &sorted,
            &dir.join("rolling.csv"),
            loose_symbol,
            loose_freq,
            WindowKind::Rolling,
        )?;
    }
    Ok(())
}

/// Which pane the windowed CSV pulls from an [`IterationResult`].
#[derive(Clone, Copy)]
enum WindowKind {
    NonOverlapping,
    Rolling,
}

/// The `(symbol, freq)` pair carried on every batch-mode row. `freq`
/// borrows a code the caller has already produced through [`sigils::freq_code`]
/// so callers control the string form.
fn loose_cols<'a>(
    slot: &'a IterationSlot,
    freq_code: &'a str,
    loose_symbol: bool,
    loose_freq: bool,
) -> Vec<(&'a str, &'a str)> {
    let mut cols = Vec::new();
    if loose_symbol {
        cols.push(("symbol", slot.iteration.symbol.as_str()));
    }
    if loose_freq {
        cols.push(("freq", freq_code));
    }
    cols
}

fn write_shared_trades(
    sorted: &[&IterationSlot],
    path: &Path,
    loose_symbol: bool,
    loose_freq: bool,
) -> Result<()> {
    // A batch worker's `costs_active` is per-iteration; when *any* bucket
    // iteration has costs active, the shared CSV grows a `commission`
    // column (kept empty for iterations that didn't have costs, matching
    // the single-run pre-costs baseline shape).
    let costs_active = sorted.iter().any(|s| s.iteration.costs_active);
    let mut w = run_mod::writer(path)?;
    let mut header: Vec<&str> = Vec::new();
    if loose_symbol {
        header.push("symbol");
    }
    if loose_freq {
        header.push("freq");
    }
    // The intra-CSV `symbol` column carried today's per-fill shape; under
    // batch it's always equal to the leading loose `symbol` column (a
    // `SingleAssetStrategy` fills only its own symbol) so we drop it when
    // the leading column is present. If SYMBOL isn't loose (all iterations
    // in this bucket share a symbol), we keep the intra-CSV column so the
    // shape matches single-run's `trades.csv` byte-for-byte.
    if loose_symbol {
        header.extend(["time", "side", "units", "price", "kind"].iter().copied());
    } else {
        header.extend(["time", "symbol", "side", "units", "price", "kind"].iter().copied());
    }
    if costs_active {
        header.push("commission");
    }
    w.write_record(&header)?;

    // Collect all rows across iterations, then sort by (symbol, freq, time).
    let mut rows: Vec<Vec<String>> = Vec::new();
    for slot in sorted {
        let fc = freq_code_or_empty(slot);
        let extras = loose_cols(slot, &fc, loose_symbol, loose_freq);
        for base in run_mod::trade_rows(&slot.iteration, &extras) {
            // `trade_rows` always emits [extras..., time, symbol, side, ...].
            // Drop the fill's `symbol` column when the leading loose column
            // has already carried it (see header logic above).
            let row = if loose_symbol {
                let leading = extras.len();
                let mut trimmed = Vec::with_capacity(base.len() - 1);
                // extras + time
                trimmed.extend_from_slice(&base[..leading + 1]);
                // skip base[leading + 1] (the fill's symbol)
                trimmed.extend_from_slice(&base[leading + 2..]);
                trimmed
            } else {
                base
            };
            rows.push(row);
        }
    }
    let leading = (loose_symbol as usize) + (loose_freq as usize);
    let time_idx = leading;
    rows.sort_by(|a, b| a[..leading].cmp(&b[..leading]).then_with(|| a[time_idx].cmp(&b[time_idx])));
    for row in rows {
        w.write_record(&row)?;
    }
    w.flush()?;
    Ok(())
}

fn write_shared_returns(
    sorted: &[&IterationSlot],
    path: &Path,
    loose_symbol: bool,
    loose_freq: bool,
) -> Result<()> {
    let mut w = run_mod::writer(path)?;
    let mut header: Vec<&str> = Vec::new();
    if loose_symbol {
        header.push("symbol");
    }
    if loose_freq {
        header.push("freq");
    }
    header.extend(["time", "equity", "return"].iter().copied());
    w.write_record(&header)?;

    let mut rows: Vec<Vec<String>> = Vec::new();
    for slot in sorted {
        let fc = freq_code_or_empty(slot);
        let extras = loose_cols(slot, &fc, loose_symbol, loose_freq);
        for base in run_mod::return_rows(&slot.iteration, &extras) {
            rows.push(base);
        }
    }
    let leading = (loose_symbol as usize) + (loose_freq as usize);
    let time_idx = leading;
    rows.sort_by(|a, b| a[..leading].cmp(&b[..leading]).then_with(|| a[time_idx].cmp(&b[time_idx])));
    for row in rows {
        w.write_record(&row)?;
    }
    w.flush()?;
    Ok(())
}

fn write_shared_metrics_csv(
    sorted: &[&IterationSlot],
    path: &Path,
    loose_symbol: bool,
    loose_freq: bool,
) -> Result<()> {
    let mut w = run_mod::writer(path)?;
    // The flattened metric name list is the same for every document; take
    // it from the first iteration.
    let names = sorted
        .first()
        .map(|s| metrics::flatten(&s.iteration.metrics))
        .unwrap_or_default();
    let mut header: Vec<&str> = Vec::new();
    if loose_symbol {
        header.push("symbol");
    }
    if loose_freq {
        header.push("freq");
    }
    for (name, _) in &names {
        header.push(name);
    }
    w.write_record(&header)?;

    for slot in sorted {
        let fc = freq_code_or_empty(slot);
        let mut row: Vec<String> = Vec::new();
        if loose_symbol {
            row.push(slot.iteration.symbol.clone());
        }
        if loose_freq {
            row.push(fc);
        }
        for (_, v) in metrics::flatten(&slot.iteration.metrics) {
            row.push(v.map(|x| x.to_string()).unwrap_or_default());
        }
        w.write_record(&row)?;
    }
    w.flush()?;
    Ok(())
}

fn write_shared_windowed(
    sorted: &[&IterationSlot],
    path: &Path,
    loose_symbol: bool,
    loose_freq: bool,
    kind: WindowKind,
) -> Result<()> {
    let mut w = run_mod::writer(path)?;
    // Header: sigils first, then window_start/window_end, then flattened metrics.
    let names = sorted
        .iter()
        .find_map(|s| pick_windowed(&s.iteration, kind).and_then(|w| w.first()))
        .map(|w| metrics::flatten(&w.metrics))
        .unwrap_or_default();
    let mut header: Vec<&str> = Vec::new();
    if loose_symbol {
        header.push("symbol");
    }
    if loose_freq {
        header.push("freq");
    }
    header.extend(["window_start", "window_end"].iter().copied());
    for (name, _) in &names {
        header.push(name);
    }
    w.write_record(&header)?;

    // Collect rows across iterations, then sort by (symbol, freq, window_start).
    let mut rows: Vec<Vec<String>> = Vec::new();
    for slot in sorted {
        let fc = freq_code_or_empty(slot);
        let windows = match pick_windowed(&slot.iteration, kind) {
            Some(w) => w,
            None => continue,
        };
        for window in windows {
            let mut row: Vec<String> = Vec::new();
            if loose_symbol {
                row.push(slot.iteration.symbol.clone());
            }
            if loose_freq {
                row.push(fc.clone());
            }
            row.push(slot.iteration.bars[window.start_bar].clone());
            row.push(slot.iteration.bars[window.end_bar].clone());
            for (_, v) in metrics::flatten(&window.metrics) {
                row.push(v.map(|x| x.to_string()).unwrap_or_default());
            }
            rows.push(row);
        }
    }
    let leading = (loose_symbol as usize) + (loose_freq as usize);
    let start_idx = leading;
    rows.sort_by(|a, b| {
        a[..leading]
            .cmp(&b[..leading])
            .then_with(|| a[start_idx].cmp(&b[start_idx]))
    });
    for row in rows {
        w.write_record(&row)?;
    }
    w.flush()?;
    Ok(())
}

fn pick_windowed(iter: &IterationResult, kind: WindowKind) -> Option<&[metrics::WindowMetrics]> {
    match kind {
        WindowKind::NonOverlapping => iter.windowed.as_deref(),
        WindowKind::Rolling => iter.rolling.as_deref(),
    }
}

/// The canonical `%FREQ` string for an iteration, or empty when the
/// iteration's freq is unknown (auto-detection failed and no `-f`).
fn freq_code_or_empty(slot: &IterationSlot) -> String {
    slot.iteration
        .freq
        .map(sigils::freq_code)
        .unwrap_or_default()
}

fn print_field(label: &str, value: &str) {
    println!("  {}{value}", style::dim(&format!("{label:<13}")));
}
