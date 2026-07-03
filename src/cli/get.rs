//! The `fugazi get` subcommand: fetch OHLCV bars from remote providers and
//! write them to a `;`-delimited CSV in the same shape `--series` reads back.
//!
//! Takes one or more specs, each
//! `<provider>:<symbol>[<freq>(,<freq>)*](,<symbol>[<freq>(,<freq>)*])*`
//! — the brackets are required. Every symbol/interval series across all specs
//! downloads concurrently, one progress bar per series. Example:
//!
//! ```text
//! fugazi get binance:BTCUSDT[1d,1h],ETHUSDT[1d] yfinance:AAPL[1d] \
//!            --since 2020-01-01 --until today \
//!            -o candles.csv
//! ```
//!
//! Output columns: `symbol;freq;time;open;high;low;close;volume`, sorted
//! ascending by `time` (ties broken by symbol, then freq). `time` is ISO 8601
//! UTC (`YYYY-MM-DDTHH:MM:SSZ`).
//!
//! **String parsing lives here, not in the library.** The library's
//! [`fugazi::sources`] API is object/enum-only; this file translates the
//! CLI's user-facing strings (dates, intervals, the compound spec) into those
//! objects before invoking the fetching machinery.

use std::io::IsTerminal;
use std::path::{Path, PathBuf};
use std::time::Duration as StdDuration;

use anyhow::{Context, Result, anyhow, bail};
use clap::Args;
use indicatif::{MultiProgress, ProgressBar, ProgressDrawTarget, ProgressStyle};
use time::format_description::well_known::Rfc3339;
use time::{Date, Duration, Month, OffsetDateTime, Time};
use tokio::runtime::Builder as RuntimeBuilder;
use tokio::task::JoinSet;

use fugazi::prelude::*;
use fugazi::sources::{Binance, CandleSource, Interval, TimedCandle, Timestamp, Yahoo};

use crate::dynd::DynValue;
use crate::input::Source as InputSource;
use crate::overlay::{self, Overlay};
use crate::style;

/// The remote candle providers this CLI can fetch from. Kept as `(name,
/// description)` so `fugazi list sources` and the "unknown provider" error
/// message both render from the same table — no drift possible.
pub(crate) const KNOWN_PROVIDERS: &[(&str, &str)] = &[
    (
        "binance",
        "Binance spot klines endpoint (BTC/ETH/... vs. USDT/EUR/...)",
    ),
    (
        "yfinance",
        "Yahoo Finance chart endpoint (stocks, ETFs, indices, FX)",
    ),
];

/// One `SYMBOL[freq,freq,...]` entry in the CLI spec.
#[derive(Debug, Clone, PartialEq)]
struct SymbolSpec {
    symbol: String,
    intervals: Vec<Interval>,
}

/// A parsed `<provider>:<symbol>[...],<symbol>[...]` spec.
#[derive(Debug, Clone, PartialEq)]
struct FetchSpec {
    provider: String,
    symbols: Vec<SymbolSpec>,
}

#[derive(Args, Debug)]
pub struct GetArgs {
    /// Fetch specs: `<provider>:<symbol>[<freq>,<freq>,...](,<symbol>[<freq>,...])*`,
    /// e.g. `binance:BTCUSDT[1d,1h],ETHUSDT[1d]`. Frequency tokens are the
    /// familiar `1m`/`5m`/`1h`/`4h`/`1d`/`1w`/`1M`. All series download in
    /// parallel.
    #[arg(value_name = "SPEC", required = true, num_args = 1..)]
    specs: Vec<String>,

    /// Start date (inclusive). Formats: ISO `YYYY-MM-DD`, EU `D-M-YYYY`,
    /// relative (`today`, `yesterday`, `7d ago`, `3 weeks ago`, `last monday`),
    /// or human-readable (`1 March 2020`, `Mar 1, 2020`, `01/03/2020`).
    ///
    /// If omitted, bars are fetched from the fugazi default (`2020-01-01`) and,
    /// unless `--keep-unstable` is set, any leading rows where the overlays
    /// have not yet warmed up are dropped from the output. When `--since` is
    /// set, `stable_period` extra leading bars are fetched instead so the
    /// first row emitted at `--since` already has the overlays stable.
    #[arg(long, value_name = "DATE")]
    since: Option<String>,

    /// End date (exclusive). Same grammar as `--since`; defaults to `today`.
    #[arg(long, default_value = "today")]
    until: String,

    /// Output CSV path. Header: `symbol;freq;time;open;high;low;close;volume`.
    /// Parent directories are created if missing.
    #[arg(short, long, value_name = "FILE")]
    output: PathBuf,

    /// Overlay definition(s) — extra columns computed on top of the fetched
    /// bars. Repeatable, and each argument takes an optional scope prefix plus
    /// one of two body forms:
    ///
    /// * scope prefix (optional): `SYMBOL[FREQ]:`, `SYMBOL:`, or `[FREQ]:` —
    ///   restricts the overlay to matching `(symbol, interval)` fetches. A
    ///   missing component is a wildcard; no prefix at all applies to every
    ///   fetch.
    /// * body: inline `col=expr[,col=expr,...]`
    ///   (`sma20=!sma { period: 20 },ema50=!ema { period: 50 }`), or
    ///   `@file.yml` — a YAML mapping of column name → source expression.
    ///
    /// Each expression is the same YAML source spec `run` accepts (`close`,
    /// `!sma { period: N }`, `!add { lhs, rhs }`, …). Unless `--keep-unstable`
    /// is given, warm-up bars are handled per fetch: with `--since`, extra
    /// leading bars are fetched so the first row at `--since` already has the
    /// overlays stable; without `--since`, the leading rows are dropped until
    /// every applicable overlay is warmed up.
    #[arg(short = 'x', long = "overlay", value_name = "SPEC")]
    overlay: Vec<InputSource>,

    /// Emit the warm-up bars instead of dropping them. Overlay columns are
    /// blank on rows where the applicable overlays have not yet warmed up.
    #[arg(long = "keep-unstable")]
    keep_unstable: bool,

    /// Suppress the summary line printed on success.
    #[arg(short, long)]
    quiet: bool,
}

/// Default `--since` when the flag is omitted — anchors the fetch far enough
/// back that the free-form default covers most historical windows a user cares
/// about, without dragging down the fetch when the flag *is* set.
const DEFAULT_SINCE: &str = "2020-01-01";

pub fn run(args: GetArgs) -> Result<()> {
    let fetch_specs: Vec<FetchSpec> = args
        .specs
        .iter()
        .map(|s| parse_spec(s).with_context(|| format!("parsing spec {s:?}")))
        .collect::<Result<_>>()?;
    let now = OffsetDateTime::now_utc();
    let since_specified = args.since.is_some();
    let since_raw = args.since.as_deref().unwrap_or(DEFAULT_SINCE);
    let since = parse_date(since_raw, now).with_context(|| format!("--since {since_raw:?}"))?;
    let until = parse_date(&args.until, now).with_context(|| format!("--until {:?}", args.until))?;
    if until <= since {
        bail!(
            "--until ({}) must be strictly after --since ({since_raw})",
            args.until,
        );
    }
    let since_ts = Timestamp::from_datetime(since);
    let until_ts = Timestamp::from_datetime(until);

    let overlays = overlay::parse_specs(&args.overlay)?;
    let overlay_columns = overlay::column_names(&overlays);

    if !args.quiet {
        style::print_header("get", "fetch OHLCV candles from remote providers");
    }

    if let Some(parent) = args.output.parent()
        && !parent.as_os_str().is_empty()
    {
        std::fs::create_dir_all(parent)
            .with_context(|| format!("creating {}", parent.display()))?;
    }

    let rt = RuntimeBuilder::new_current_thread()
        .enable_all()
        .build()
        .context("building tokio runtime")?;

    // One `Series` per (provider, symbol, interval) — the unit of parallelism.
    // Per-series overlay warm-up is folded in here so `fetch_series` can push
    // `since` back accordingly and each task builds its own indicator instances.
    let series: Vec<Series> = fetch_specs
        .iter()
        .flat_map(|spec| {
            spec.symbols.iter().flat_map(|sym| {
                sym.intervals.iter().map(|&interval| {
                    let stable = overlay::stable_period_for(
                        &overlays,
                        &overlay_columns,
                        &sym.symbol,
                        interval,
                    );
                    Series {
                        provider: spec.provider.clone(),
                        symbol: sym.symbol.clone(),
                        interval,
                        stable,
                    }
                })
            })
        })
        .collect();

    let (multi, bars) =
        build_progress_bars(&series, since_ts, until_ts, since_specified, args.quiet);

    // Async: download every series in parallel — no overlay state crosses task
    // boundaries. Overlays are applied synchronously below, per (symbol,
    // interval) group, so `DynValue`'s non-Send `Rc`-backed `Position` stub
    // stays on one thread.
    let result = rt.block_on(fetch_all(series.clone(), since_ts, until_ts, since_specified, bars));
    let _ = multi.clear();
    let raw = result?;
    let rows = apply_overlays(
        raw,
        since_ts,
        since_specified,
        args.keep_unstable,
        &overlays,
        &overlay_columns,
    );

    write_csv(&args.output, &rows, &overlay_columns)
        .with_context(|| format!("writing {}", args.output.display()))?;

    if !args.quiet {
        let n_symbols: usize = fetch_specs.iter().map(|s| s.symbols.len()).sum();
        println!(
            "{}: wrote {} rows across {} symbol{}/{} interval series",
            args.output.display(),
            rows.len(),
            n_symbols,
            if n_symbols == 1 { "" } else { "s" },
            series.len(),
        );
    }
    Ok(())
}

/// One row of output: which symbol + interval it came from, the timed candle,
/// and the per-column overlay values (aligned with the CLI's overlay column
/// layout — `None` for a column no applicable overlay covers this row's group).
struct Row {
    symbol: String,
    interval: Interval,
    candle: TimedCandle,
    overlays: Vec<Option<Real>>,
}

/// One downloadable series: a `(provider, symbol, interval)` triple plus the
/// per-series overlay warm-up length (max `stable_period` across the overlays
/// that apply to this `(symbol, interval)`). The unit of parallelism — each
/// series gets its own fetch task and progress bar.
#[derive(Clone)]
struct Series {
    provider: String,
    symbol: String,
    interval: Interval,
    stable: usize,
}

impl Series {
    fn label(&self) -> String {
        format!(
            "{}:{}[{}]",
            self.provider,
            self.symbol,
            self.interval.as_token()
        )
    }

    /// Where this series' fetch actually starts: `since` on the nose when the
    /// user didn't pass `--since` (leading unready rows get dropped downstream);
    /// pushed back by `stable` bars otherwise so the first row at `since` is
    /// already warmed up. `Interval::Month`'s 30-day approximation is fine here
    /// — over-fetching a handful of days is harmless.
    fn fetch_since(&self, since: Timestamp, since_specified: bool) -> Timestamp {
        if since_specified {
            Timestamp(
                since
                    .0
                    .saturating_sub((self.stable as i64).saturating_mul(self.interval.duration_ms())),
            )
        } else {
            since
        }
    }
}

/// Bars per download chunk. Matches Binance's max klines per request, so on
/// that provider one chunk is roughly one HTTP request; on providers that
/// return the whole window in one call (Yahoo) it just bounds the request
/// size the same way.
const CHUNK_BARS: i64 = 1000;

/// Split `[since, until)` into consecutive `[start, end)` windows of at most
/// [`CHUNK_BARS`] bars each, so a long fetch advances the progress bar as it
/// goes rather than in one jump per symbol/interval pair.
fn chunk_bounds(since: Timestamp, until: Timestamp, interval: Interval) -> Vec<(Timestamp, Timestamp)> {
    let step = interval.duration_ms().saturating_mul(CHUNK_BARS);
    let mut chunks = Vec::new();
    let mut cursor = since.0;
    while cursor < until.0 {
        let end = cursor.saturating_add(step).min(until.0);
        chunks.push((Timestamp(cursor), Timestamp(end)));
        cursor = end;
    }
    chunks
}

/// Delay between successive chunk requests *within one series*, mirroring the
/// politeness delay the providers apply between their own pagination pages.
/// Series run concurrently; the delay paces each series' own request stream.
const CHUNK_DELAY: StdDuration = StdDuration::from_millis(100);

/// One un-overlaid downloaded bar in the intermediate fetch result: which
/// symbol + interval it came from and its timed candle. `apply_overlays` walks
/// these grouped by `(symbol, interval)` to attach overlay columns before the
/// final `Row` list is emitted.
struct RawBar {
    symbol: String,
    interval: Interval,
    candle: TimedCandle,
}

/// Download every series concurrently (one task per series) and return the
/// merged raw bars. Overlay computation is deliberately kept synchronous
/// (`apply_overlays`), since [`DynValue`]'s stub `Position` uses `Rc` and can't
/// cross task boundaries.
async fn fetch_all(
    series: Vec<Series>,
    since: Timestamp,
    until: Timestamp,
    since_specified: bool,
    bars: Vec<ProgressBar>,
) -> Result<Vec<RawBar>> {
    let mut tasks = JoinSet::new();
    for (s, bar) in series.into_iter().zip(bars) {
        let fetch_since = s.fetch_since(since, since_specified);
        tasks.spawn(fetch_series(s, fetch_since, until, bar));
    }
    let mut all: Vec<RawBar> = Vec::new();
    while let Some(joined) = tasks.join_next().await {
        all.extend(joined.context("fetch task panicked")??);
    }
    Ok(all)
}

/// Fetch one series chunk-by-chunk (sequentially — the politeness delay is
/// per series), advancing its own progress bar. Overlay-agnostic.
async fn fetch_series(
    series: Series,
    fetch_since: Timestamp,
    until: Timestamp,
    bar: ProgressBar,
) -> Result<Vec<RawBar>> {
    let label = series.label();
    let mut rows: Vec<RawBar> = Vec::new();
    let mut first = true;
    for (chunk_since, chunk_until) in chunk_bounds(fetch_since, until, series.interval) {
        if !first {
            tokio::time::sleep(CHUNK_DELAY).await;
        }
        first = false;
        bar.set_message(chunk_since.to_datetime().date().to_string());
        let candles = fetch(
            &series.provider,
            &series.symbol,
            series.interval,
            chunk_since,
            chunk_until,
        )
        .await
        .with_context(|| format!("fetching {label}"))?;
        rows.extend(candles.into_iter().map(|tc| RawBar {
            symbol: series.symbol.clone(),
            interval: series.interval,
            candle: tc,
        }));
        bar.inc(1);
    }
    bar.finish_with_message("done");
    Ok(rows)
}

/// Group raw bars by `(symbol, interval)`, feed each group's bars through its
/// per-group active overlays (last-defined applicable one wins per column;
/// see [`overlay::active_for`]), and drop the leading warm-up rows unless the
/// caller opted to keep them. Bars are then sorted ascending by time (ties
/// broken by symbol, then freq) — the shape the previous overlay-less writer
/// already committed to.
fn apply_overlays(
    raw: Vec<RawBar>,
    since: Timestamp,
    since_specified: bool,
    keep_unstable: bool,
    overlays: &[Overlay],
    columns: &[String],
) -> Vec<Row> {
    // Bin the incoming stream by `(symbol, interval)` — order within each bin
    // is preserved by the sort below and matches the order the provider paged
    // the bars in (ascending time). The outer sort re-orders across groups.
    let mut by_group: std::collections::HashMap<(String, Interval), Vec<RawBar>> =
        std::collections::HashMap::new();
    for bar in raw {
        by_group
            .entry((bar.symbol.clone(), bar.interval))
            .or_default()
            .push(bar);
    }

    let mut out: Vec<Row> = Vec::new();
    for ((symbol, interval), mut bars) in by_group {
        bars.sort_by_key(|b| b.candle.time);

        let active: Vec<Option<&Overlay>> =
            overlay::active_for(overlays, columns, &symbol, interval);
        let mut instances: Vec<Option<DynValue>> = active
            .iter()
            .map(|slot| slot.as_ref().map(|o| o.build()))
            .collect();
        let has_applicable = instances.iter().any(Option::is_some);

        let mut group_rows: Vec<Row> = bars
            .into_iter()
            .map(|b| {
                let values: Vec<Option<Real>> = instances
                    .iter_mut()
                    .map(|slot| slot.as_mut().and_then(|inst| inst.update(b.candle.candle)))
                    .collect();
                Row {
                    symbol: b.symbol,
                    interval: b.interval,
                    candle: b.candle,
                    overlays: values,
                }
            })
            .collect();

        if !keep_unstable {
            if since_specified {
                // Extra leading bars covered the warm-up; trim to the window
                // the user asked for.
                group_rows.retain(|r| r.candle.time >= since);
            } else if has_applicable {
                // No `--since` — drop leading rows until every applicable
                // overlay is warmed up.
                if let Some(cut) = group_rows.iter().position(|r| {
                    r.overlays
                        .iter()
                        .zip(active.iter())
                        .all(|(v, slot)| slot.is_none() || v.is_some())
                }) {
                    group_rows.drain(..cut);
                } else {
                    group_rows.clear();
                }
            }
        }

        out.extend(group_rows);
    }

    out.sort_by(|a, b| {
        (a.candle.time, a.symbol.as_str(), a.interval.as_token())
            .cmp(&(b.candle.time, b.symbol.as_str(), b.interval.as_token()))
    });
    out
}

/// Build one fetch-progress bar per series, denominated in download *chunks*
/// (see [`chunk_bounds`]), grouped under a [`MultiProgress`] so they render
/// stacked and update independently. Hidden — a no-op sink — when `--quiet`
/// is set or when stderr is not a terminal, so the CLI stays silent when its
/// output is being piped or redirected.
fn build_progress_bars(
    series: &[Series],
    since: Timestamp,
    until: Timestamp,
    since_specified: bool,
    quiet: bool,
) -> (MultiProgress, Vec<ProgressBar>) {
    let multi = if quiet || !std::io::stderr().is_terminal() {
        MultiProgress::with_draw_target(ProgressDrawTarget::hidden())
    } else {
        MultiProgress::new()
    };
    let width = series.iter().map(|s| s.label().len()).max().unwrap_or(0);
    let style = ProgressStyle::with_template("  {prefix} [{bar:20.cyan/blue}] {pos}/{len} {msg}")
        .expect("progress template compiles")
        .progress_chars("=> ");
    let bars = series
        .iter()
        .map(|s| {
            // Per-series bar accounts for the overlay warm-up window pulled in
            // ahead of `since` so the progress count matches what fetch_series
            // actually chunks through.
            let start = s.fetch_since(since, since_specified);
            let n_chunks = chunk_bounds(start, until, s.interval).len();
            let bar = multi.add(ProgressBar::new(n_chunks as u64));
            bar.set_style(style.clone());
            bar.set_prefix(format!("{:<width$}", s.label()));
            // Steady tick so the bar animates while a single chunk is in flight.
            bar.enable_steady_tick(StdDuration::from_millis(120));
            bar
        })
        .collect();
    (multi, bars)
}

/// Dispatch on the provider name to a concrete [`CandleSource`] implementation.
async fn fetch(
    provider: &str,
    symbol: &str,
    interval: Interval,
    since: Timestamp,
    until: Timestamp,
) -> Result<Vec<TimedCandle>> {
    match provider {
        "binance" => Ok(Binance::new()
            .candles(symbol, interval, since, Some(until))
            .await?),
        "yfinance" => Ok(Yahoo::new()
            .candles(symbol, interval, since, Some(until))
            .await?),
        other => bail!(unknown_provider_error(other)),
    }
}

/// Fetch the provider's full ticker vocabulary. Used by `fugazi list tickers`.
/// Providers that don't offer a canonical enumeration endpoint (Yahoo, most
/// retail equity APIs) surface `SourceError::Unsupported` through here.
pub(crate) async fn tickers_of(provider: &str) -> Result<Vec<String>> {
    match provider {
        "binance" => Ok(Binance::new().tickers().await?),
        "yfinance" => Ok(Yahoo::new().tickers().await?),
        other => bail!(unknown_provider_error(other)),
    }
}

fn unknown_provider_error(other: &str) -> String {
    let known: Vec<&str> = KNOWN_PROVIDERS.iter().map(|(n, _)| *n).collect();
    format!(
        "unknown provider {other:?}. Known providers: {}",
        known.join(", ")
    )
}

/// Write the row list to `path` as a `;`-delimited CSV. Base header:
/// `symbol;freq;time;open;high;low;close;volume`, followed by one column per
/// overlay column name (unique, in first-appearance order across the
/// `--overlay` args). A `None` overlay value — either the column's applicable
/// overlay is still warming up or no overlay is scoped to this row's group —
/// renders as an empty cell.
fn write_csv(path: &Path, rows: &[Row], overlay_columns: &[String]) -> Result<()> {
    let mut wtr = csv::WriterBuilder::new()
        .delimiter(b';')
        .from_path(path)
        .with_context(|| format!("creating {}", path.display()))?;
    let mut header: Vec<&str> = vec![
        "symbol", "freq", "time", "open", "high", "low", "close", "volume",
    ];
    header.extend(overlay_columns.iter().map(String::as_str));
    wtr.write_record(&header)?;
    for row in rows {
        let time = row
            .candle
            .time
            .to_datetime()
            .format(&Rfc3339)
            .unwrap_or_else(|_| row.candle.time.0.to_string());
        let mut record: Vec<String> = vec![
            row.symbol.clone(),
            row.interval.as_token(),
            time,
            format_f64(row.candle.candle.open),
            format_f64(row.candle.candle.high),
            format_f64(row.candle.candle.low),
            format_f64(row.candle.candle.close),
            format_f64(row.candle.candle.volume),
        ];
        for v in &row.overlays {
            record.push(v.map(format_f64).unwrap_or_default());
        }
        wtr.write_record(&record)?;
    }
    wtr.flush()?;
    Ok(())
}

/// Format a float without trailing `.0` for integers, and without exponent
/// notation.
fn format_f64(v: f64) -> String {
    if v.is_finite() && v.fract() == 0.0 && v.abs() < 1e16 {
        format!("{}", v as i64)
    } else {
        format!("{v}")
    }
}

// ---------------------------------------------------------------------------
// Parsers — CLI-only. The library sources module intentionally takes
// only objects/enums; these translate the user-facing CLI strings into them.
// ---------------------------------------------------------------------------

/// Parse a `<provider>:<symbol>[<freq>,...](,<symbol>[<freq>,...])*` spec.
fn parse_spec(spec: &str) -> Result<FetchSpec> {
    let (provider, rest) = spec
        .split_once(':')
        .ok_or_else(|| anyhow!("{spec:?} missing `<provider>:` prefix"))?;
    let provider = provider.trim();
    if provider.is_empty() {
        bail!("{spec:?}: empty provider");
    }
    let mut symbols: Vec<SymbolSpec> = Vec::new();
    let mut start = 0usize;
    let mut depth: i32 = 0;
    let bytes = rest.as_bytes();
    for (i, &b) in bytes.iter().enumerate() {
        match b {
            b'[' => depth += 1,
            b']' => {
                depth -= 1;
                if depth < 0 {
                    bail!("{spec:?}: unexpected `]`");
                }
            }
            b',' if depth == 0 => {
                symbols.push(parse_symbol(&rest[start..i])?);
                start = i + 1;
            }
            _ => {}
        }
    }
    if depth != 0 {
        bail!("{spec:?}: unclosed `[` bracket");
    }
    let tail = &rest[start..];
    if !tail.trim().is_empty() {
        symbols.push(parse_symbol(tail)?);
    }
    if symbols.is_empty() {
        bail!("{spec:?}: no symbols specified");
    }
    Ok(FetchSpec {
        provider: provider.to_string(),
        symbols,
    })
}

fn parse_symbol(s: &str) -> Result<SymbolSpec> {
    let s = s.trim();
    let open = s
        .find('[')
        .ok_or_else(|| anyhow!("{s:?}: missing `[freq,...]` bracket"))?;
    if !s.ends_with(']') {
        bail!("{s:?}: bracket must close at end of the symbol entry");
    }
    let symbol = s[..open].trim();
    if symbol.is_empty() {
        bail!("{s:?}: empty symbol name");
    }
    let inner = &s[open + 1..s.len() - 1];
    let mut intervals = Vec::new();
    for tok in inner.split(',') {
        let tok = tok.trim();
        if tok.is_empty() {
            bail!("{s:?}: empty frequency token in bracket");
        }
        intervals.push(parse_interval(tok).with_context(|| format!("{s:?}: freq {tok:?}"))?);
    }
    if intervals.is_empty() {
        bail!("{s:?}: empty frequency list");
    }
    Ok(SymbolSpec {
        symbol: symbol.to_string(),
        intervals,
    })
}

/// Parse a Binance-style interval token (`1m`, `5m`, `1h`, `4h`, `1d`, `1w`,
/// `1M`). Case-sensitive on the unit letter: `m` = minute, `M` = month.
pub(crate) fn parse_interval(s: &str) -> Result<Interval> {
    let s = s.trim();
    if s.is_empty() {
        bail!("empty interval token");
    }
    let (num, unit) = s.split_at(s.len() - 1);
    let n: u32 = if num.is_empty() {
        1
    } else {
        num.parse().with_context(|| format!("bad interval {s:?}"))?
    };
    if n == 0 {
        bail!("interval {s:?}: multiplier must be positive");
    }
    match unit {
        "m" => Ok(Interval::Minute(n)),
        "h" => Ok(Interval::Hour(n)),
        "d" => Ok(Interval::Day(n)),
        "w" => Ok(Interval::Week(n)),
        "M" => Ok(Interval::Month(n)),
        _ => bail!("interval {s:?}: unknown unit letter {unit:?}"),
    }
}

/// Parse a date string against `now`, returning an [`OffsetDateTime`] at UTC
/// midnight. Grammar:
///
/// * `today` / `yesterday`
/// * `Nd ago` / `Nw ago`
/// * `YYYY-MM-DD` (ISO 8601 calendar; `/` works as separator too)
/// * `D-M-YYYY` (EU day-month-year; `/` works as separator too)
/// * anything [`interim`] understands (day-first dialect): `1 March 2020`,
///   `Mar 1, 2020`, `3 weeks ago`, `last monday`, ...
fn parse_date(input: &str, now: OffsetDateTime) -> Result<OffsetDateTime> {
    let raw = input.trim();
    let lower = raw.to_ascii_lowercase();

    if lower == "today" {
        return Ok(midnight_utc(now.date()));
    }
    if lower == "yesterday" {
        return Ok(midnight_utc(now.date() - Duration::days(1)));
    }
    if let Some(rel) = parse_relative(&lower) {
        let (n, unit) = rel;
        let d = match unit {
            'd' => Duration::days(n as i64),
            'w' => Duration::weeks(n as i64),
            _ => unreachable!(),
        };
        return Ok(midnight_utc(now.date() - d));
    }
    if let Some(date) = parse_absolute(raw) {
        return Ok(midnight_utc(date));
    }
    // Everything else goes through `interim`'s human-date grammar. `Uk` keeps
    // ambiguous numeric dates day-first, matching the EU form above. Whatever
    // time-of-day it resolves is floored to keep the midnight invariant.
    if let Ok(dt) = interim::parse_date_string(raw, now, interim::Dialect::Uk) {
        return Ok(midnight_utc(dt.date()));
    }
    bail!("invalid date {input:?}")
}

fn midnight_utc(date: Date) -> OffsetDateTime {
    date.with_time(Time::MIDNIGHT).assume_utc()
}

fn parse_relative(s: &str) -> Option<(u32, char)> {
    let rest = s.strip_suffix("ago")?.trim_end();
    let idx = rest.find(['d', 'w'])?;
    let unit = rest.as_bytes()[idx] as char;
    if !rest[idx + 1..].trim().is_empty() {
        return None;
    }
    let n: u32 = rest[..idx].trim().parse().ok()?;
    if n == 0 {
        return None;
    }
    Some((n, unit))
}

fn parse_absolute(s: &str) -> Option<Date> {
    let parts: Vec<&str> = s.split(['-', '/']).collect();
    if parts.len() != 3 {
        return None;
    }
    if !parts.iter().all(|p| !p.is_empty() && p.bytes().all(|b| b.is_ascii_digit())) {
        return None;
    }
    let first_len = parts[0].len();
    let (year, month, day) = if first_len == 4 {
        let y: i32 = parts[0].parse().ok()?;
        let m: u32 = parts[1].parse().ok()?;
        let d: u32 = parts[2].parse().ok()?;
        (y, m, d)
    } else if first_len == 1 || first_len == 2 {
        if parts[2].len() != 4 {
            return None;
        }
        let d: u32 = parts[0].parse().ok()?;
        let m: u32 = parts[1].parse().ok()?;
        let y: i32 = parts[2].parse().ok()?;
        (y, m, d)
    } else {
        return None;
    };
    let month = Month::try_from(u8::try_from(month).ok()?).ok()?;
    Date::from_calendar_date(year, month, u8::try_from(day).ok()?).ok()
}

#[cfg(test)]
mod tests {
    use super::*;
    use time::macros::datetime;

    fn now() -> OffsetDateTime {
        datetime!(2024-03-15 12:34:56 UTC)
    }

    #[test]
    fn parses_single_symbol_single_freq() {
        let got = parse_spec("binance:BTCUSDT[1d]").unwrap();
        assert_eq!(got.provider, "binance");
        assert_eq!(got.symbols.len(), 1);
        assert_eq!(got.symbols[0].symbol, "BTCUSDT");
        assert_eq!(got.symbols[0].intervals, vec![Interval::Day(1)]);
    }

    #[test]
    fn parses_multi_symbol_multi_freq() {
        let got = parse_spec("binance:BTCUSDT[1d,1h],ETHUSDT[1d]").unwrap();
        assert_eq!(got.symbols.len(), 2);
        assert_eq!(
            got.symbols[0].intervals,
            vec![Interval::Day(1), Interval::Hour(1)]
        );
        assert_eq!(got.symbols[1].intervals, vec![Interval::Day(1)]);
    }

    #[test]
    fn rejects_missing_provider_colon() {
        assert!(parse_spec("BTCUSDT[1d]").is_err());
    }

    #[test]
    fn rejects_missing_bracket() {
        assert!(parse_spec("binance:BTCUSDT").is_err());
    }

    #[test]
    fn rejects_empty_bracket() {
        assert!(parse_spec("binance:BTCUSDT[]").is_err());
    }

    #[test]
    fn rejects_unclosed_bracket() {
        assert!(parse_spec("binance:BTCUSDT[1d,1h").is_err());
    }

    #[test]
    fn rejects_bad_freq_token() {
        assert!(parse_spec("binance:BTCUSDT[1x]").is_err());
    }

    #[test]
    fn tolerates_whitespace() {
        let got = parse_spec("binance: BTCUSDT [ 1d , 1h ] , ETHUSDT [1d]").unwrap();
        assert_eq!(got.symbols.len(), 2);
        assert_eq!(
            got.symbols[0].intervals,
            vec![Interval::Day(1), Interval::Hour(1)]
        );
    }

    #[test]
    fn parses_all_interval_units() {
        assert_eq!(parse_interval("5m").unwrap(), Interval::Minute(5));
        assert_eq!(parse_interval("4h").unwrap(), Interval::Hour(4));
        assert_eq!(parse_interval("1d").unwrap(), Interval::Day(1));
        assert_eq!(parse_interval("1w").unwrap(), Interval::Week(1));
        assert_eq!(parse_interval("1M").unwrap(), Interval::Month(1));
    }

    #[test]
    fn rejects_zero_multiplier() {
        assert!(parse_interval("0d").is_err());
    }

    #[test]
    fn today_yesterday_and_relative_dates() {
        assert_eq!(parse_date("today", now()).unwrap(), datetime!(2024-03-15 0:00 UTC));
        assert_eq!(parse_date("yesterday", now()).unwrap(), datetime!(2024-03-14 0:00 UTC));
        assert_eq!(parse_date("7d ago", now()).unwrap(), datetime!(2024-03-08 0:00 UTC));
        assert_eq!(parse_date("2w ago", now()).unwrap(), datetime!(2024-03-01 0:00 UTC));
    }

    #[test]
    fn iso_and_eu_dates() {
        assert_eq!(parse_date("2020-01-01", now()).unwrap(), datetime!(2020-01-01 0:00 UTC));
        assert_eq!(parse_date("1-1-2020", now()).unwrap(), datetime!(2020-01-01 0:00 UTC));
        assert_eq!(parse_date("15-03-2024", now()).unwrap(), datetime!(2024-03-15 0:00 UTC));
        // `01-02-2020` is EU (Feb 1 2020), disambiguated by first-component length.
        assert_eq!(parse_date("01-02-2020", now()).unwrap(), datetime!(2020-02-01 0:00 UTC));
    }

    #[test]
    fn human_readable_dates() {
        // Month names: day-first freely, month-first with a comma.
        assert_eq!(parse_date("1 March 2020", now()).unwrap(), datetime!(2020-03-01 0:00 UTC));
        assert_eq!(parse_date("Mar 1, 2020", now()).unwrap(), datetime!(2020-03-01 0:00 UTC));
        // Slash dates follow the dashed rules: ISO year-first or EU day-first.
        assert_eq!(parse_date("2020/03/01", now()).unwrap(), datetime!(2020-03-01 0:00 UTC));
        assert_eq!(parse_date("01/03/2020", now()).unwrap(), datetime!(2020-03-01 0:00 UTC));
        // Spelled-out relative offsets and weekday anchors, against a fixed
        // `now` of Friday 2024-03-15.
        assert_eq!(parse_date("3 weeks ago", now()).unwrap(), datetime!(2024-02-23 0:00 UTC));
        assert_eq!(parse_date("2 months ago", now()).unwrap(), datetime!(2024-01-15 0:00 UTC));
        assert_eq!(parse_date("1 year ago", now()).unwrap(), datetime!(2023-03-15 0:00 UTC));
        assert_eq!(parse_date("last monday", now()).unwrap(), datetime!(2024-03-11 0:00 UTC));
        // A time-of-day is accepted but floored to midnight.
        assert_eq!(parse_date("2020-03-01 14:30", now()).unwrap(), datetime!(2020-03-01 0:00 UTC));
    }

    #[test]
    fn rejects_bad_dates() {
        assert!(parse_date("", now()).is_err());
        assert!(parse_date("not-a-date", now()).is_err());
        assert!(parse_date("2021-02-29", now()).is_err()); // non-leap
        assert!(parse_date("7d agox", now()).is_err());
    }

    #[test]
    fn chunk_bounds_splits_long_windows() {
        // 3000 daily bars -> 3 full chunks of CHUNK_BARS days each.
        let day = Interval::Day(1).duration_ms();
        let since = Timestamp(0);
        let until = Timestamp(3000 * day);
        let chunks = chunk_bounds(since, until, Interval::Day(1));
        assert_eq!(chunks.len(), 3);
        assert_eq!(chunks[0], (Timestamp(0), Timestamp(1000 * day)));
        assert_eq!(chunks[1], (Timestamp(1000 * day), Timestamp(2000 * day)));
        assert_eq!(chunks[2], (Timestamp(2000 * day), Timestamp(3000 * day)));
    }

    #[test]
    fn chunk_bounds_partitions_exactly_with_ragged_tail() {
        let day = Interval::Day(1).duration_ms();
        let since = Timestamp(5);
        let until = Timestamp(1500 * day + 7);
        let chunks = chunk_bounds(since, until, Interval::Day(1));
        assert_eq!(chunks.len(), 2);
        // Consecutive, gap-free, and covering [since, until) exactly.
        assert_eq!(chunks.first().unwrap().0, since);
        assert_eq!(chunks.last().unwrap().1, until);
        for pair in chunks.windows(2) {
            assert_eq!(pair[0].1, pair[1].0);
        }
    }

    #[test]
    fn chunk_bounds_short_window_is_one_chunk() {
        let since = Timestamp(0);
        let until = Timestamp(30 * Interval::Day(1).duration_ms());
        let chunks = chunk_bounds(since, until, Interval::Day(1));
        assert_eq!(chunks, vec![(since, until)]);
    }

    #[test]
    fn chunk_bounds_empty_window_yields_no_chunks() {
        assert!(chunk_bounds(Timestamp(100), Timestamp(100), Interval::Day(1)).is_empty());
    }

    #[test]
    fn format_f64_strips_trailing_zero() {
        assert_eq!(format_f64(27000.0), "27000");
        assert_eq!(format_f64(27000.5), "27000.5");
        assert_eq!(format_f64(0.00012345), "0.00012345");
    }
}
