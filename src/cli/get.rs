//! The `fugazi get` subcommand: fetch OHLCV bars from a remote provider and
//! write them to a `;`-delimited CSV in the same shape `--series` reads back.
//!
//! Spec grammar: `<provider>:<symbol>[<freq>(,<freq>)*](,<symbol>[<freq>(,<freq>)*])*`
//! — the brackets are required. Example:
//!
//! ```text
//! fugazi get binance:BTCUSDT[1d,1h],ETHUSDT[1d] \
//!            --since 2020-01-01 --until today \
//!            -o candles.csv
//! ```
//!
//! Output columns: `symbol;freq;time;open;high;low;close;volume`, sorted
//! ascending by `(symbol, freq, time)`. `time` is ISO 8601 UTC
//! (`YYYY-MM-DDTHH:MM:SSZ`).
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
use indicatif::{ProgressBar, ProgressStyle};
use time::format_description::well_known::Rfc3339;
use time::{Date, Duration, Month, OffsetDateTime, Time};
use tokio::runtime::Builder as RuntimeBuilder;

use fugazi::sources::{Binance, CandleSource, Interval, TimedCandle, Timestamp, Yahoo};

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
    /// The fetch spec: `<provider>:<symbol>[<freq>,<freq>,...](,<symbol>[<freq>,...])*`,
    /// e.g. `binance:BTCUSDT[1d,1h],ETHUSDT[1d]`. Frequency tokens are the
    /// familiar `1m`/`5m`/`1h`/`4h`/`1d`/`1w`/`1M`.
    #[arg(value_name = "SPEC")]
    spec: String,

    /// Start date (inclusive). Formats: ISO `YYYY-MM-DD`, EU `D-M-YYYY`,
    /// or relative (`today`, `yesterday`, `Nd ago`, `Nw ago`).
    #[arg(long, default_value = "2020-01-01")]
    since: String,

    /// End date (exclusive). Same grammar as `--since`; defaults to `today`.
    #[arg(long, default_value = "today")]
    until: String,

    /// Output CSV path. Header: `symbol;freq;time;open;high;low;close;volume`.
    /// Parent directories are created if missing.
    #[arg(short, long, value_name = "FILE")]
    output: PathBuf,

    /// Suppress the summary line printed on success.
    #[arg(short, long)]
    quiet: bool,
}

pub fn run(args: GetArgs) -> Result<()> {
    let fetch_spec =
        parse_spec(&args.spec).with_context(|| format!("parsing spec {:?}", args.spec))?;
    let now = OffsetDateTime::now_utc();
    let since = parse_date(&args.since, now).with_context(|| format!("--since {:?}", args.since))?;
    let until = parse_date(&args.until, now).with_context(|| format!("--until {:?}", args.until))?;
    if until <= since {
        bail!(
            "--until ({}) must be strictly after --since ({})",
            args.until,
            args.since
        );
    }
    let since_ts = Timestamp::from_datetime(since);
    let until_ts = Timestamp::from_datetime(until);

    if !args.quiet {
        style::print_header("get", "fetch OHLCV candles from a remote provider");
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

    let n_chunks: usize = fetch_spec
        .symbols
        .iter()
        .flat_map(|s| s.intervals.iter())
        .map(|&i| chunk_bounds(since_ts, until_ts, i).len())
        .sum();
    let progress = build_progress_bar(n_chunks as u64, args.quiet);

    let rows = rt.block_on(async {
        fetch_all(
            &fetch_spec.provider,
            &fetch_spec.symbols,
            since_ts,
            until_ts,
            &progress,
        )
        .await
    })?;

    progress.finish_and_clear();

    write_csv(&args.output, &rows).with_context(|| format!("writing {}", args.output.display()))?;

    if !args.quiet {
        let n_symbols = fetch_spec.symbols.len();
        let n_pairs: usize = fetch_spec.symbols.iter().map(|s| s.intervals.len()).sum();
        println!(
            "{}: wrote {} rows across {} symbol{}/{} interval series",
            args.output.display(),
            rows.len(),
            n_symbols,
            if n_symbols == 1 { "" } else { "s" },
            n_pairs,
        );
    }
    Ok(())
}

/// One row of output: which symbol + interval it came from, plus the timed candle.
struct Row {
    symbol: String,
    interval: Interval,
    candle: TimedCandle,
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

/// Delay between successive chunk requests, mirroring the politeness delay
/// the providers apply between their own pagination pages.
const CHUNK_DELAY: StdDuration = StdDuration::from_millis(100);

async fn fetch_all(
    provider: &str,
    symbols: &[SymbolSpec],
    since: Timestamp,
    until: Timestamp,
    progress: &ProgressBar,
) -> Result<Vec<Row>> {
    let mut all: Vec<Row> = Vec::new();
    let mut first = true;
    for sym in symbols {
        for &interval in &sym.intervals {
            let label = format!("{provider}:{}[{}]", sym.symbol, interval.as_token());
            for (chunk_since, chunk_until) in chunk_bounds(since, until, interval) {
                if !first {
                    tokio::time::sleep(CHUNK_DELAY).await;
                }
                first = false;
                progress.set_message(format!(
                    "{label} {}",
                    chunk_since.to_datetime().date()
                ));
                let bars = fetch(provider, &sym.symbol, interval, chunk_since, chunk_until)
                    .await
                    .with_context(|| format!("fetching {label}"))?;
                for tc in bars {
                    all.push(Row {
                        symbol: sym.symbol.clone(),
                        interval,
                        candle: tc,
                    });
                }
                progress.inc(1);
            }
        }
    }
    all.sort_by(|a, b| {
        (a.symbol.as_str(), a.interval.as_token(), a.candle.time)
            .cmp(&(b.symbol.as_str(), b.interval.as_token(), b.candle.time))
    });
    Ok(all)
}

/// Build the fetch-progress bar, denominated in download *chunks* (see
/// [`chunk_bounds`]). Hidden — a no-op sink — when `--quiet` is set or when
/// stderr is not a terminal, so the CLI stays silent when its output is being
/// piped or redirected.
fn build_progress_bar(total_chunks: u64, quiet: bool) -> ProgressBar {
    if quiet || !std::io::stderr().is_terminal() {
        return ProgressBar::hidden();
    }
    let bar = ProgressBar::new(total_chunks);
    bar.set_style(
        ProgressStyle::with_template("  fetching [{bar:20.cyan/blue}] {pos}/{len} {msg}")
            .expect("progress template compiles")
            .progress_chars("=> "),
    );
    // Steady tick so the bar animates while a single chunk is in flight.
    bar.enable_steady_tick(StdDuration::from_millis(120));
    bar
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

/// Write the row list to `path` as a `;`-delimited CSV. Header:
/// `symbol;freq;time;open;high;low;close;volume`.
fn write_csv(path: &Path, rows: &[Row]) -> Result<()> {
    let mut wtr = csv::WriterBuilder::new()
        .delimiter(b';')
        .from_path(path)
        .with_context(|| format!("creating {}", path.display()))?;
    wtr.write_record(["symbol", "freq", "time", "open", "high", "low", "close", "volume"])?;
    for row in rows {
        let time = row
            .candle
            .time
            .to_datetime()
            .format(&Rfc3339)
            .unwrap_or_else(|_| row.candle.time.0.to_string());
        wtr.write_record([
            row.symbol.as_str(),
            &row.interval.as_token(),
            &time,
            &format_f64(row.candle.candle.open),
            &format_f64(row.candle.candle.high),
            &format_f64(row.candle.candle.low),
            &format_f64(row.candle.candle.close),
            &format_f64(row.candle.candle.volume),
        ])?;
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
fn parse_interval(s: &str) -> Result<Interval> {
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
/// * `YYYY-MM-DD` (ISO 8601 calendar)
/// * `D-M-YYYY` (EU day-month-year)
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
    let parts: Vec<&str> = s.split('-').collect();
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
    fn rejects_bad_dates() {
        assert!(parse_date("", now()).is_err());
        assert!(parse_date("not-a-date", now()).is_err());
        assert!(parse_date("2021-02-29", now()).is_err()); // non-leap
        assert!(parse_date("0d ago", now()).is_err());
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
