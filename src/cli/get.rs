//! The `fugazi get` subcommand: fetch OHLCV bars from remote providers and
//! write them to a `,`-delimited CSV in the same shape `--series` reads back.
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
//! Output columns: `symbol,freq,time,open,high,low,close,volume`, sorted
//! ascending by `time` (ties broken by symbol, then freq). `time` is ISO 8601
//! UTC (`YYYY-MM-DDTHH:MM:SSZ`).
//!
//! **String parsing lives here, not in the library.** The library's
//! [`fugazi::sources`] API is object/enum-only; this file translates the
//! CLI's user-facing strings (dates, intervals, the compound spec) into those
//! objects before invoking the fetching machinery.

use std::io::IsTerminal;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration as StdDuration;

use anyhow::{Context, Result, anyhow, bail};
use clap::Args;
use indicatif::{MultiProgress, ProgressBar, ProgressDrawTarget, ProgressStyle};
use time::format_description::well_known::Rfc3339;
use time::{Date, Duration, Month, OffsetDateTime, Time};
use tokio::runtime::Builder as RuntimeBuilder;
use tokio::task::JoinSet;

use fugazi::prelude::*;
use fugazi::sources::{
    self, Binance, CandleSource, CoinGecko, CoinMarketCap, Interval, OverlayRow, OverlaySource,
    Timestamp, Yahoo, binance::binance_schema, yahoo::yahoo_schema,
};

use crate::dyn_indicator::{DynIndicator, DynValue};
use crate::csv_source::{CsvBar, CsvSource};
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
        "cg",
        "CoinGecko market cap / volume / supply — overlay columns only, no OHLCV \
         (symbols are coin ids: `bitcoin`, not `BTC`)",
    ),
    (
        "cmc",
        "CoinMarketCap price / volume / market cap / supply — overlay columns only, \
         no OHLCV (symbols are tickers `BTC` or numeric ids; paid tier required)",
    ),
    (
        "csv",
        "Local OHLCV CSV — spec is `csv:PATH` (no `[freq]` bracket)",
    ),
    (
        "yfinance",
        "Yahoo Finance chart endpoint (stocks, ETFs, indices, FX)",
    ),
];

/// What a provider yields — the two [`fugazi::sources`] traits, as seen by the
/// CLI.
///
/// The distinction is load-bearing rather than cosmetic. An overlay provider has
/// no OHLCV, so its rows must not be written through the candle CSV writer:
/// that writer emits a fixed `open,high,low,close,volume` block, and a
/// synthesised zero-candle in those columns would silently *overwrite* the real
/// prices when the file is later joined into a `--series` dataframe (which
/// merges on `(symbol, time)` and lets the later file win each column). Hence
/// [`resolve_mode`] refuses to mix the two kinds in one invocation.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ProviderKind {
    /// Implements `CandleSource` — yields OHLCV bars. Also covers `csv:`.
    Candles,
    /// Implements `OverlaySource` — yields timestamped side-channel columns.
    Overlays,
}

fn provider_kind(provider: &str) -> ProviderKind {
    match provider {
        "cg" | "cmc" => ProviderKind::Overlays,
        _ => ProviderKind::Candles,
    }
}

/// One `[OUTPUT=]QUERY[freq,freq,...]` entry in the CLI spec.
///
/// The optional `OUTPUT=` prefix decouples the name a row is *emitted* under
/// from the identifier the provider is *queried* with. That matters whenever a
/// provider's vocabulary differs from the one your price series uses —
/// CoinGecko keys on coin ids (`bitcoin`) while a Binance series is keyed on
/// pairs (`BTCUSDT`), and the `--series` join is an exact string match on
/// `symbol`. `cg:BTCUSDT=bitcoin[1d]` fetches `bitcoin` and writes
/// `BTCUSDT`, so the two files line up.
///
/// With no `=`, output and query are the same string (the previous behaviour).
#[derive(Debug, Clone, PartialEq)]
struct SymbolSpec {
    /// The value written to the `symbol` column — the `--series` join key.
    output: String,
    /// The identifier sent to the provider.
    query: String,
    freqs: Vec<FreqSpec>,
}

/// One `[OFREQ=]FREQ` entry inside a symbol's bracket — the cadence twin of
/// [`SymbolSpec`]'s `OUTPUT=QUERY`, and the same left-is-emitted /
/// right-is-fetched rule.
///
/// The remap is for **relabelling a cadence, not changing one**: the two sides
/// normally denote the same duration under a different name, so that the `freq`
/// column agrees with whatever the series you intend to join against uses.
/// `binance:BTCUSDT[1d=24h]` fetches 24-hour bars and tags them `1d`;
/// `[1h=60m]` is the same idea.
///
/// The **output side is an opaque label, not an interval** — `[FOO=1d]` is
/// legal and writes `freq=FOO`. Only the fetched side has to be a real interval
/// token, because only it is sent to a provider. This is safe for the workflow
/// that matters: the `--series` loader treats `freq` as a reserved passthrough
/// column and never parses it, so an arbitrary label joins fine. (The one
/// consumer that *does* parse it back is `get csv:PATH`, which reads a file's
/// `freq` cells into `Interval`s — a file labelled `FOO` cannot be re-read
/// through that path.)
///
/// **It relabels; it does not resample.** Every fetched row is emitted with only
/// its `freq` cell rewritten. So if the two sides denote *different* durations
/// (`[4h=1h]`), the extra rows land on timestamps no 4-hour bar-open covers,
/// they find no price bar in the join, and `run` rejects them outright
/// (`missing required column 'open'`). That is the intended failure — this is
/// not a downsampler.
///
/// A provider's *own* token spelling (Binance's `1M`, a hypothetical `1hr`) is
/// not this mechanism's job either — that mapping lives inside each provider,
/// which translates an [`Interval`] into its native vocabulary.
#[derive(Debug, Clone, PartialEq)]
struct FreqSpec {
    /// Written verbatim to the `freq` column. Any string.
    output: String,
    /// The cadence actually fetched (and chunked, and paginated). Also what `-x`
    /// scope prefixes match against, since a scope names a real cadence.
    query: Interval,
}

/// A parsed CLI `get` spec.
///
/// Remote providers share the same
/// `<provider>:[OUTPUT=]<query>[<freq>,...],[OUTPUT=]<query>[<freq>,...]`
/// grammar; `csv:PATH` is its own variant, since the file already carries
/// symbol+freq per row and the bracket doesn't apply.
#[derive(Debug, Clone, PartialEq)]
enum FetchSpec {
    Remote {
        provider: String,
        symbols: Vec<SymbolSpec>,
    },
    Csv {
        path: PathBuf,
    },
}

impl FetchSpec {
    fn kind(&self) -> ProviderKind {
        match self {
            FetchSpec::Remote { provider, .. } => provider_kind(provider),
            FetchSpec::Csv { .. } => ProviderKind::Candles,
        }
    }
}

/// Decide which pipeline this invocation runs, rejecting a mix.
///
/// Candle and overlay providers write different CSV shapes into the single
/// `-o` file, and merging them there would mean inventing OHLCV for the overlay
/// rows — see [`ProviderKind`]. Two `get` calls and two `--series` flags do the
/// job correctly, so that is what the error tells the user to do.
fn resolve_mode(specs: &[FetchSpec]) -> Result<ProviderKind> {
    let overlay = specs.iter().any(|s| s.kind() == ProviderKind::Overlays);
    let candle = specs.iter().any(|s| s.kind() == ProviderKind::Candles);
    if overlay && candle {
        bail!(
            "cannot mix candle providers and overlay-only providers in one `get` — they write \
             different CSV shapes, and giving the overlay rows a synthetic OHLCV block would \
             zero out your real prices when the files are joined.\n\n\
             Fetch them separately and let `run` join the two on (symbol, time):\n\
             \x20 fugazi get binance:BTCUSDT[1d]           -o prices.csv\n\
             \x20 fugazi get cg:BTCUSDT=bitcoin[1d] -o caps.csv\n\
             \x20 fugazi run @strategy.yml -s @prices.csv -s @caps.csv -o out/"
        );
    }
    Ok(if overlay {
        ProviderKind::Overlays
    } else {
        ProviderKind::Candles
    })
}

#[derive(Args, Debug)]
pub struct GetArgs {
    /// Fetch specs: `<provider>:[OUT=]<symbol>[[OFREQ=]<freq>,...](,...)*`, e.g.
    /// `binance:BTCUSDT[1d,1h],ETHUSDT[1d]`. Frequency tokens are the familiar
    /// `1m`/`5m`/`1h`/`4h`/`1d`/`1w`/`1M`. All series download in parallel.
    ///
    /// Both the symbol and each freq accept an optional `EMITTED=FETCHED`
    /// remap — the left side is what gets written to the CSV, the right side is
    /// what the provider is asked for. Omit it and the two are the same (the
    /// plain form above). Use it when a provider's vocabulary differs from the
    /// price series you intend to join against, since `run` joins on an exact
    /// `(symbol, time)` match:
    ///
    /// * `cg:BTCUSDT=bitcoin[1d]` — fetch the coin id `bitcoin`, emit
    ///   `symbol=BTCUSDT`.
    /// * `binance:BTCUSDT[1d=24h]` — fetch 24-hour bars, tag them `freq=1d`.
    ///
    /// The freq form *relabels* a cadence; it does not resample one, so the two
    /// sides should denote the same duration under a different name (`1d=24h`).
    /// Only the fetched side must be a real interval token — the emitted label
    /// is free-form (`[FOO=1d]` writes `freq=FOO`).
    ///
    /// Overlay-only providers (`coingecko`) emit side-channel columns and no
    /// OHLCV, and cannot be mixed with candle providers in one invocation —
    /// fetch each to its own file and pass both to `run -s`.
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

    /// Output CSV path. Header: `symbol,freq,time,open,high,low,close,volume`.
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
    let mode = resolve_mode(&fetch_specs)?;
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

    match mode {
        ProviderKind::Candles => run_candles(args, fetch_specs, since_ts, until_ts, since_specified, &rt),
        ProviderKind::Overlays => run_overlay_columns(args, fetch_specs, since_ts, until_ts, &rt),
    }
}

/// The OHLCV pipeline: fetch candles, compute `-x` overlays over them, write
/// `symbol,freq,time,open,high,low,close,volume,...`.
fn run_candles(
    args: GetArgs,
    fetch_specs: Vec<FetchSpec>,
    since_ts: Timestamp,
    until_ts: Timestamp,
    since_specified: bool,
    rt: &tokio::runtime::Runtime,
) -> Result<()> {
    let overlays = overlay::parse_specs(&args.overlay)?;
    let overlay_columns = overlay::column_names(&overlays);

    if !args.quiet {
        style::print_header("get", "fetch OHLCV candles from remote providers");
    }

    // Expand each `FetchSpec` into one `Series` per `(symbol, interval)` — the
    // unit of parallelism. Per-series overlay warm-up is folded in here so
    // `fetch_series` can push `since` back accordingly and each task builds
    // its own indicator instances. `csv:` specs are read once up front and
    // their bar list is shared into each derived Series' `csv_bars` so the
    // async pipeline can filter without re-reading.
    let mut series: Vec<Series> = Vec::new();
    let mut n_symbols: usize = 0;
    for spec in &fetch_specs {
        match spec {
            FetchSpec::Remote { provider, symbols } => {
                n_symbols += symbols.len();
                // The remote provider's canonical Schema — Binance's four
                // kline extras, Yahoo's `adj_close`. Every atom in a fetch
                // will bind to this via `OverlayInfo::new(schema, ...)`.
                let schema = match provider.as_str() {
                    "binance" => binance_schema().clone(),
                    "yfinance" => yahoo_schema().clone(),
                    _ => Schema::empty(),
                };
                for sym in symbols {
                    for freq in &sym.freqs {
                        // `-x` scopes match the symbol the user sees, but the
                        // *fetched* cadence: a scope's `[FREQ]` names a real
                        // interval, and the emitted label may not be one.
                        let stable = overlay::stable_period_for(
                            &overlays,
                            &overlay_columns,
                            &sym.output,
                            freq.query,
                            &schema,
                        );
                        series.push(Series {
                            provider: provider.clone(),
                            output: sym.output.clone(),
                            query: sym.query.clone(),
                            interval: freq.query,
                            out_freq: freq.output.clone(),
                            stable,
                            csv_bars: None,
                            csv_path: None,
                        });
                    }
                }
            }
            FetchSpec::Csv { path } => {
                let bars = CsvSource::new(path.clone())
                    .read()
                    .with_context(|| format!("reading {}", path.display()))?;
                let shared = Arc::new(bars);
                // The CSV loader classified every non-OHLCV column into a
                // shared `Arc<Schema>` — pluck it off any atom.
                let file_schema = sources::schema_of(
                    &shared.iter().map(|b| b.atom.clone()).collect::<Vec<_>>(),
                );
                let mut pairs: Vec<(String, Interval)> = Vec::new();
                for b in shared.iter() {
                    let pair = (b.symbol.clone(), b.interval);
                    if !pairs.contains(&pair) {
                        pairs.push(pair);
                    }
                }
                let mut seen_symbols: Vec<String> = Vec::new();
                for (sym, interval) in pairs {
                    if !seen_symbols.contains(&sym) {
                        seen_symbols.push(sym.clone());
                    }
                    let stable = overlay::stable_period_for(
                        &overlays,
                        &overlay_columns,
                        &sym,
                        interval,
                        &file_schema,
                    );
                    // A `csv:` file already carries its own `symbol` and `freq`
                    // columns, so there is nothing to remap: emitted == fetched.
                    series.push(Series {
                        provider: "csv".into(),
                        output: sym.clone(),
                        query: sym,
                        interval,
                        out_freq: interval.as_token(),
                        stable,
                        csv_bars: Some(shared.clone()),
                        csv_path: Some(path.clone()),
                    });
                }
                n_symbols += seen_symbols.len();
            }
        }
    }

    let (multi, bars) =
        build_progress_bars(&series, since_ts, until_ts, since_specified, args.quiet);

    // Async: download every series in parallel — no overlay state crosses task
    // boundaries. Overlays are applied synchronously below, per (symbol,
    // interval) group, so `DynValue`'s non-Send `Rc`-backed `Position` stub
    // stays on one thread. `csv:` series short-circuit inside `fetch_series`.
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

    write_candles_csv(&args.output, &rows, &overlay_columns)
        .with_context(|| format!("writing {}", args.output.display()))?;

    if !args.quiet {
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

/// The overlay-only pipeline: fetch side-channel columns from an
/// [`OverlaySource`](fugazi::sources::OverlaySource) and write
/// `symbol,freq,time,<provider columns>` — **no OHLCV block**, so the file is
/// safe to `--series`-join on top of a price series without clobbering it (see
/// [`ProviderKind`]).
///
/// `-x/--overlay` is rejected here rather than supported: a computed overlay is
/// an indicator chain over `Atom`s, and there is no candle to build one from.
/// Compute derived columns downstream, in the strategy spec, where the two
/// files have been joined and the price bars actually exist.
fn run_overlay_columns(
    args: GetArgs,
    fetch_specs: Vec<FetchSpec>,
    since_ts: Timestamp,
    until_ts: Timestamp,
    rt: &tokio::runtime::Runtime,
) -> Result<()> {
    if !args.overlay.is_empty() {
        bail!(
            "`-x/--overlay` computes indicator columns over OHLCV bars, and an overlay-only \
             provider has none. Fetch the columns here, then compute derived values in the \
             strategy spec (`!get {{ key: market_cap }}`) once `run` has joined this file onto \
             a price series."
        );
    }

    if !args.quiet {
        style::print_header("get", "fetch overlay columns from remote providers");
    }

    // One `Series` per (symbol, interval). `stable` is 0: there are no computed
    // overlays to warm up, so no leading bars need pulling in ahead of `since`.
    let mut series: Vec<Series> = Vec::new();
    let mut n_symbols: usize = 0;
    for spec in &fetch_specs {
        let FetchSpec::Remote { provider, symbols } = spec else {
            unreachable!("resolve_mode routes `csv:` specs to the candle pipeline");
        };
        n_symbols += symbols.len();
        for sym in symbols {
            for freq in &sym.freqs {
                series.push(Series {
                    provider: provider.clone(),
                    output: sym.output.clone(),
                    query: sym.query.clone(),
                    interval: freq.query,
                    out_freq: freq.output.clone(),
                    stable: 0,
                    csv_bars: None,
                    csv_path: None,
                });
            }
        }
    }

    let (multi, bars) = build_progress_bars(&series, since_ts, until_ts, false, args.quiet);
    let result = rt.block_on(fetch_all_overlays(series.clone(), since_ts, until_ts, bars));
    let _ = multi.clear();
    let mut rows = result?;

    // Same output ordering as the candle writer: ascending by time, ties broken
    // by symbol then freq.
    rows.sort_by(|a, b| {
        (a.time, a.symbol.as_str(), a.freq.as_str())
            .cmp(&(b.time, b.symbol.as_str(), b.freq.as_str()))
    });

    write_overlays_csv(&args.output, &rows)
        .with_context(|| format!("writing {}", args.output.display()))?;

    if !args.quiet {
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

/// One overlay row of output: the emitted symbol + interval it belongs to, its
/// bar-open time, and the provider's per-bar values. The candle-less twin of
/// [`Row`].
struct OverlayOut {
    symbol: String,
    /// The `freq` cell, verbatim — an opaque label, not necessarily an interval
    /// token. See [`FreqSpec`].
    freq: String,
    time: Timestamp,
    overlays: OverlayInfo,
}

/// Download every overlay series concurrently, one task per series.
async fn fetch_all_overlays(
    series: Vec<Series>,
    since: Timestamp,
    until: Timestamp,
    bars: Vec<ProgressBar>,
) -> Result<Vec<OverlayOut>> {
    let mut tasks = JoinSet::new();
    for (s, bar) in series.into_iter().zip(bars) {
        tasks.spawn(fetch_overlay_series(s, since, until, bar));
    }
    let mut all: Vec<OverlayOut> = Vec::new();
    while let Some(joined) = tasks.join_next().await {
        all.extend(joined.context("fetch task panicked")??);
    }
    Ok(all)
}

/// Fetch one overlay series chunk-by-chunk, advancing its progress bar. Rows are
/// tagged with the series' *output* symbol — the `--series` join key.
async fn fetch_overlay_series(
    series: Series,
    since: Timestamp,
    until: Timestamp,
    bar: ProgressBar,
) -> Result<Vec<OverlayOut>> {
    let label = series.label();
    let mut rows: Vec<OverlayOut> = Vec::new();
    let mut first = true;
    for (chunk_since, chunk_until) in chunk_bounds(since, until, series.interval) {
        if !first {
            tokio::time::sleep(CHUNK_DELAY).await;
        }
        first = false;
        bar.set_message(chunk_since.to_datetime().date().to_string());
        let fetched = fetch_overlays(
            &series.provider,
            &series.query,
            series.interval,
            chunk_since,
            chunk_until,
        )
        .await
        .with_context(|| format!("fetching {label}"))?;
        rows.extend(fetched.into_iter().map(|r| OverlayOut {
            symbol: series.output.clone(),
            freq: series.out_freq.clone(),
            time: r.time,
            overlays: r.overlays,
        }));
        bar.inc(1);
    }
    bar.finish_with_message("done");
    Ok(rows)
}

/// Dispatch on the provider name to a concrete [`OverlaySource`](fugazi::sources::OverlaySource)
/// implementation. The overlay-side twin of [`fetch`].
async fn fetch_overlays(
    provider: &str,
    symbol: &str,
    interval: Interval,
    since: Timestamp,
    until: Timestamp,
) -> Result<Vec<OverlayRow>> {
    match provider {
        "cg" => Ok(CoinGecko::new()
            .overlays(symbol, interval, since, Some(until))
            .await?),
        "cmc" => Ok(CoinMarketCap::new()
            .overlays(symbol, interval, since, Some(until))
            .await?),
        other => bail!(unknown_provider_error(other)),
    }
}

/// Write overlay rows as `symbol,freq,time,<column>...`, `,`-delimited.
///
/// Columns are the union of every row's schema keys in first-appearance order
/// (all rows from one provider share one schema, so in practice this is just
/// that provider's column list). **There is deliberately no OHLCV block**: this
/// file is meant to be `--series`-joined on top of a price series, and the join
/// lets the later file win each column it carries — an `open,high,low,close`
/// block full of synthesised zeroes here would silently overwrite the real
/// prices there. A missing cell renders blank, matching the candle writer.
fn write_overlays_csv(path: &Path, rows: &[OverlayOut]) -> Result<()> {
    let mut wtr = csv::WriterBuilder::new()
        .delimiter(b',')
        .from_path(path)
        .with_context(|| format!("creating {}", path.display()))?;

    let mut columns: Vec<String> = Vec::new();
    for r in rows {
        for name in r.overlays.schema().keys() {
            if !columns.iter().any(|c| c == name) {
                columns.push(name.to_string());
            }
        }
    }

    let mut header: Vec<&str> = vec!["symbol", "freq", "time"];
    header.extend(columns.iter().map(String::as_str));
    wtr.write_record(&header)?;

    for row in rows {
        let time = row
            .time
            .to_datetime()
            .format(&Rfc3339)
            .unwrap_or_else(|_| row.time.0.to_string());
        let mut record: Vec<String> = vec![row.symbol.clone(), row.freq.clone(), time];
        for name in &columns {
            let cell = row
                .overlays
                .get_by_key(name)
                .map(format_overlay_value)
                // A `Real` cell can be NaN (the provider had no value for this
                // bar); render it blank rather than as the literal "NaN", so the
                // `--series` loader reads it back as a missing cell.
                .filter(|s| s != "NaN")
                .unwrap_or_default();
            record.push(cell);
        }
        wtr.write_record(&record)?;
    }
    wtr.flush()?;
    Ok(())
}

/// One row of output: which symbol + interval it came from, the timed candle,
/// the per-`-x`-column overlay values (aligned with the CLI's overlay column
/// layout — `None` for a column no applicable overlay covers this row's
/// group), and the pass-through extras from a `csv:` source (per-row
/// non-OHLCV cells classified as `Real`/`Bool`/`Str`).
struct Row {
    symbol: String,
    /// The `freq` cell, verbatim. See [`FreqSpec`].
    freq: String,
    /// Fully-populated bar: OHLCV, bar-open `time`, and the source-provided
    /// overlay side channel (Binance's `quote_volume` / `n_trades` / …;
    /// Yahoo's `adj_close`; or the CSV file's non-OHLCV columns).
    atom: Atom,
    /// Computed `--overlay` outputs, aligned with the CLI's requested column
    /// name list. `None` for a column whose overlay hasn't warmed up yet.
    overlays: Vec<Option<OverlayValue>>,
}

/// One downloadable (or file-backed) series: a `(provider, symbol, interval)`
/// triple plus the per-series overlay warm-up length (max `stable_period`
/// across the overlays that apply to this `(symbol, interval)`). The unit of
/// parallelism — each series gets its own fetch task and progress bar.
///
/// For `csv:` specs, the pre-read bar list is threaded through as
/// [`Series::csv_bars`], and [`fetch_series`] short-circuits into an
/// in-memory filter instead of an HTTP fetch.
#[derive(Clone)]
struct Series {
    provider: String,
    /// The symbol this series' rows are *written* under — the `--series` join
    /// key. Equal to `query` unless the spec used the `OUTPUT=QUERY` form.
    output: String,
    /// The identifier this series is *fetched* with (a CoinGecko coin id, a
    /// Binance pair, …). See [`SymbolSpec`].
    query: String,
    /// The cadence actually fetched — what chunking, pagination, the provider
    /// call, and `-x` scope matching all use.
    interval: Interval,
    /// The label written to the `freq` column, verbatim. Equal to
    /// `interval.as_token()` unless the spec used the `OFREQ=FREQ` form; may be
    /// any string. See [`FreqSpec`].
    out_freq: String,
    stable: usize,
    /// The file's pre-read bar list, shared between every series that reads
    /// from the same file. `None` for remote-provider series.
    csv_bars: Option<Arc<Vec<CsvBar>>>,
    /// The originating path — kept for the progress-bar label (`csv:./data.csv`).
    csv_path: Option<PathBuf>,
}

impl Series {
    fn label(&self) -> String {
        if let Some(path) = &self.csv_path {
            return format!(
                "csv:{}[{}:{}]",
                path.display(),
                self.output,
                self.interval.as_token()
            );
        }
        // Echo each mapping when there is one, so the progress line makes the
        // fetched-vs-emitted distinction visible while it runs.
        let symbol = if self.output == self.query {
            self.query.clone()
        } else {
            format!("{}={}", self.output, self.query)
        };
        let token = self.interval.as_token();
        let freq = if self.out_freq == token {
            token
        } else {
            format!("{}={}", self.out_freq, token)
        };
        format!("{}:{}[{}]", self.provider, symbol, freq)
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
/// symbol + interval it came from, and the fully-populated [`Atom`] the
/// source produced. The atom already carries its bar-open [`Timestamp`] and
/// (for a source that exposes them) a per-bar overlay side channel behind a
/// provider-defined [`Schema`]. `apply_overlays` walks these grouped by
/// `(symbol, interval)` to compute `-x` overlay columns before the final
/// `Row` list is emitted.
struct RawBar {
    symbol: String,
    /// The cadence actually fetched — what `-x` scopes match against.
    interval: Interval,
    /// The `freq` cell, verbatim. See [`FreqSpec`].
    freq: String,
    atom: Atom,
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
///
/// A `csv:` series short-circuits: the file has already been read into
/// [`Series::csv_bars`] up front, so this is just an in-memory filter to the
/// series' `(symbol, interval)` and the `[fetch_since, until)` window.
async fn fetch_series(
    series: Series,
    fetch_since: Timestamp,
    until: Timestamp,
    bar: ProgressBar,
) -> Result<Vec<RawBar>> {
    if let Some(csv_bars) = series.csv_bars.clone() {
        let rows: Vec<RawBar> = csv_bars
            .iter()
            .filter(|b| {
                b.symbol == series.query
                    && b.interval == series.interval
                    && b.atom.time.map(|t| t.0 >= fetch_since.0 && t.0 < until.0).unwrap_or(false)
            })
            .map(|b| RawBar {
                symbol: series.output.clone(),
                interval: b.interval,
                freq: series.out_freq.clone(),
                atom: b.atom.clone(),
            })
            .collect();
        bar.inc(1);
        bar.finish_with_message("done");
        return Ok(rows);
    }
    let label = series.label();
    let mut rows: Vec<RawBar> = Vec::new();
    let mut first = true;
    for (chunk_since, chunk_until) in chunk_bounds(fetch_since, until, series.interval) {
        if !first {
            tokio::time::sleep(CHUNK_DELAY).await;
        }
        first = false;
        bar.set_message(chunk_since.to_datetime().date().to_string());
        let atoms = fetch(
            &series.provider,
            &series.query,
            series.interval,
            chunk_since,
            chunk_until,
        )
        .await
        .with_context(|| format!("fetching {label}"))?;
        // Rows are tagged with the *emitted* symbol and freq — the join keys.
        rows.extend(atoms.into_iter().map(|atom| RawBar {
            symbol: series.output.clone(),
            interval: series.interval,
            freq: series.out_freq.clone(),
            atom,
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
    // Bin the incoming stream by `(symbol, freq-label, interval)` — order within
    // each bin is preserved by the sort below and matches the order the provider
    // paged the bars in (ascending time). The outer sort re-orders across groups.
    //
    // The key carries both the emitted label and the fetched interval: the label
    // identifies the output series, and the interval is what `-x` scopes match
    // on. Keying on only one of them would let two series with the same label
    // but different cadences (or vice versa) share one set of overlay
    // instances.
    let mut by_group: std::collections::HashMap<(String, String, Interval), Vec<RawBar>> =
        std::collections::HashMap::new();
    for bar in raw {
        by_group
            .entry((bar.symbol.clone(), bar.freq.clone(), bar.interval))
            .or_default()
            .push(bar);
    }

    let mut out: Vec<Row> = Vec::new();
    for ((symbol, _freq, interval), mut bars) in by_group {
        bars.sort_by_key(|b| b.atom.time);

        // Every atom in a group shares the same source-provided schema (one
        // `Arc<Schema>`); pluck it off the first atom that carries overlays.
        // Falls back to `Schema::empty()` for a source that exposes no
        // extras — `!get { key }` then panics at build with an unknown key,
        // matching the pre-refactor behaviour.
        let group_atoms: Vec<Atom> = bars.iter().map(|b| b.atom.clone()).collect();
        let schema = sources::schema_of(&group_atoms);

        let active: Vec<Option<&Overlay>> =
            overlay::active_for(overlays, columns, &symbol, interval);
        let mut instances: Vec<Option<Box<dyn DynIndicator>>> = active
            .iter()
            .map(|slot| slot.as_ref().map(|o| o.build(&schema)))
            .collect();
        let has_applicable = instances.iter().any(Option::is_some);

        let mut group_rows: Vec<Row> = bars
            .into_iter()
            .map(|b| {
                let values: Vec<Option<OverlayValue>> = instances
                    .iter_mut()
                    .map(|slot| {
                        slot.as_mut().and_then(|inst| {
                            dyn_value_to_overlay(inst.update(DynValue::Atom(b.atom.clone()))?)
                        })
                    })
                    .collect();
                Row {
                    symbol: b.symbol,
                    freq: b.freq,
                    atom: b.atom,
                    overlays: values,
                }
            })
            .collect();

        if !keep_unstable {
            if since_specified {
                // Extra leading bars covered the warm-up; trim to the window
                // the user asked for.
                group_rows.retain(|r| r.atom.time.map(|t| t >= since).unwrap_or(false));
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
        (a.atom.time, a.symbol.as_str(), a.freq.as_str())
            .cmp(&(b.atom.time, b.symbol.as_str(), b.freq.as_str()))
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
            // actually chunks through. `csv:` series are read once up front,
            // so their bar is a single tick that flips straight to `done`.
            let n_chunks = if s.csv_bars.is_some() {
                1
            } else {
                let start = s.fetch_since(since, since_specified);
                chunk_bounds(start, until, s.interval).len()
            };
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
) -> Result<Vec<Atom>> {
    match provider {
        "binance" => Ok(Binance::new()
            .atoms(symbol, interval, since, Some(until))
            .await?),
        "yfinance" => Ok(Yahoo::new()
            .atoms(symbol, interval, since, Some(until))
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
        "cg" => Ok(OverlaySource::tickers(&CoinGecko::new()).await?),
        "cmc" => Ok(OverlaySource::tickers(&CoinMarketCap::new()).await?),
        "yfinance" => Ok(Yahoo::new().tickers().await?),
        "csv" => bail!(
            "`csv:` reads a local CSV — the ticker list is whatever `symbol` \
             values the file itself contains; there is no canonical enumeration \
             endpoint"
        ),
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

/// Write the row list to `path` as a `,`-delimited CSV. Base header:
/// `symbol,freq,time,open,high,low,close,volume`, followed by one column per
/// overlay column name (unique, in first-appearance order across the
/// `--overlay` args) and one column per source-provided extra (`n_trades`,
/// `adj_close`, or a `csv:` file's own non-OHLCV columns — union across all
/// rows, first-appearance order). Extras whose names clash with a requested
/// `--overlay` column are skipped: the computed overlay wins that slot.
/// A `None` overlay value or a missing extra cell renders as blank; other
/// cells render per their runtime type: `Real` via [`format_f64`], `Bool` as
/// `true`/`false`, `Str` verbatim.
fn write_candles_csv(path: &Path, rows: &[Row], overlay_columns: &[String]) -> Result<()> {
    let mut wtr = csv::WriterBuilder::new()
        .delimiter(b',')
        .from_path(path)
        .with_context(|| format!("creating {}", path.display()))?;

    // Source-provided extras: union of column names across every row's
    // `atom.overlays`. Skips anything already emitted as a computed
    // `--overlay` (name collision — the computed one wins its slot).
    let extra_columns = collect_extra_columns(rows, overlay_columns);

    let mut header: Vec<&str> = vec![
        "symbol", "freq", "time", "open", "high", "low", "close", "volume",
    ];
    header.extend(overlay_columns.iter().map(String::as_str));
    header.extend(extra_columns.iter().map(String::as_str));
    wtr.write_record(&header)?;
    for row in rows {
        let time_ts = row
            .atom
            .time
            .expect("get.rs atoms always carry a bar-open time");
        let time = time_ts
            .to_datetime()
            .format(&Rfc3339)
            .unwrap_or_else(|_| time_ts.0.to_string());
        let mut record: Vec<String> = vec![
            row.symbol.clone(),
            row.freq.clone(),
            time,
            format_f64(row.atom.candle.open),
            format_f64(row.atom.candle.high),
            format_f64(row.atom.candle.low),
            format_f64(row.atom.candle.close),
            format_f64(row.atom.candle.volume),
        ];
        for v in &row.overlays {
            record.push(v.as_ref().map(format_overlay_value).unwrap_or_default());
        }
        for name in &extra_columns {
            let cell = row
                .atom
                .overlays
                .as_ref()
                .and_then(|ov| ov.get_by_key(name))
                .map(format_overlay_value)
                .unwrap_or_default();
            record.push(cell);
        }
        wtr.write_record(&record)?;
    }
    wtr.flush()?;
    Ok(())
}

/// Union of source-provided overlay column names across `rows`, in
/// first-appearance order. Preserves the input file's header order (each
/// atom's schema retains it). Skips names already appearing in
/// `overlay_columns` — a computed `--overlay` column with the same name
/// shadows the source-provided one in the output.
fn collect_extra_columns(rows: &[Row], overlay_columns: &[String]) -> Vec<String> {
    let mut out: Vec<String> = Vec::new();
    for r in rows {
        let Some(ov) = r.atom.overlays.as_ref() else {
            continue;
        };
        for name in ov.schema().keys() {
            if overlay_columns.iter().any(|c| c == name) {
                continue;
            }
            if !out.iter().any(|n| n == name) {
                out.push(name.to_string());
            }
        }
    }
    out
}

/// Convert a `DynIndicator`'s emitted `DynValue` (an overlay-spec output) into
/// the widened cell type. Overlay chains that produce an unspottable `Atom` or
/// `Candle` reach the `unreachable!` arm — they can't be a CSV cell.
fn dyn_value_to_overlay(v: DynValue) -> Option<OverlayValue> {
    match v {
        DynValue::Real(x) => Some(OverlayValue::Real(x)),
        DynValue::Bool(b) => Some(OverlayValue::Bool(b)),
        DynValue::Str(s) => Some(OverlayValue::Str(s)),
        other => unreachable!(
            "overlay's DynIndicator produced a non-scalar payload {other:?} — the spec should \
             never build one that isn't Real/Bool/Str",
        ),
    }
}

/// Format one overlay cell for CSV output. `Real` → [`format_f64`]; `Bool` →
/// `true` / `false`; `Str` → the verbatim string (the CSV writer handles any
/// quoting).
fn format_overlay_value(v: &OverlayValue) -> String {
    match v {
        OverlayValue::Real(x) => format_f64(*x),
        OverlayValue::Bool(b) => (if *b { "true" } else { "false" }).to_string(),
        OverlayValue::Str(s) => s.to_string(),
    }
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

/// Parse a `<provider>:<symbol>[<freq>,...](,<symbol>[<freq>,...])*` spec — or
/// the `csv:PATH` short form (no bracket; the file's own `symbol` + `freq`
/// columns drive the output).
fn parse_spec(spec: &str) -> Result<FetchSpec> {
    let (provider, rest) = spec
        .split_once(':')
        .ok_or_else(|| anyhow!("{spec:?} missing `<provider>:` prefix"))?;
    let provider = provider.trim();
    if provider.is_empty() {
        bail!("{spec:?}: empty provider");
    }
    if provider == "csv" {
        let path = rest.trim();
        if path.is_empty() {
            bail!("{spec:?}: `csv:` needs a path (e.g. `csv:./candles.csv`)");
        }
        return Ok(FetchSpec::Csv {
            path: PathBuf::from(path),
        });
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
    Ok(FetchSpec::Remote {
        provider: provider.to_string(),
        symbols,
    })
}

/// Parse one `[OUTPUT=]QUERY[freq,...]` entry. See [`SymbolSpec`] for what the
/// `OUTPUT=` prefix is for.
fn parse_symbol(s: &str) -> Result<SymbolSpec> {
    let s = s.trim();
    let open = s
        .find('[')
        .ok_or_else(|| anyhow!("{s:?}: missing `[freq,...]` bracket"))?;
    if !s.ends_with(']') {
        bail!("{s:?}: bracket must close at end of the symbol entry");
    }
    let head = s[..open].trim();
    if head.is_empty() {
        bail!("{s:?}: empty symbol name");
    }
    // `OUTPUT=QUERY` — emit under the left, fetch under the right. Split on the
    // first `=` only: a provider id is free to contain one, an output symbol
    // (which has to match a CSV `symbol` cell) is not going to.
    let (output, query) = match head.split_once('=') {
        Some((out, q)) => (out.trim(), q.trim()),
        None => (head, head),
    };
    if output.is_empty() {
        bail!("{s:?}: empty output symbol on the left of `=`");
    }
    if query.is_empty() {
        bail!("{s:?}: empty provider query on the right of `=`");
    }
    let inner = &s[open + 1..s.len() - 1];
    let mut freqs = Vec::new();
    for tok in inner.split(',') {
        let tok = tok.trim();
        if tok.is_empty() {
            bail!("{s:?}: empty frequency token in bracket");
        }
        // `OFREQ=FREQ` — tag rows with the left, fetch at the right. Same rule
        // as the symbol's `OUTPUT=QUERY`; absent, the label is the fetched
        // cadence's own token. Only the fetched side is parsed: the label is
        // written to the CSV verbatim and never interpreted.
        let (out_label, query_tok) = match tok.split_once('=') {
            Some((out, q)) => (out.trim(), q.trim()),
            None => (tok, tok),
        };
        if out_label.is_empty() || query_tok.is_empty() {
            bail!("{s:?}: freq {tok:?} has an empty side around `=`");
        }
        freqs.push(FreqSpec {
            output: out_label.to_string(),
            query: crate::calendar::parse_interval(query_tok)
                .with_context(|| format!("{s:?}: freq {query_tok:?}"))?,
        });
    }
    if freqs.is_empty() {
        bail!("{s:?}: empty frequency list");
    }
    Ok(SymbolSpec {
        output: output.to_string(),
        query: query.to_string(),
        freqs,
    })
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

    /// Helper: the `FreqSpec` list an unmapped bracket (`[1d,1h]`) produces —
    /// the label is just the fetched cadence's own token.
    fn plain(freqs: &[Interval]) -> Vec<FreqSpec> {
        freqs
            .iter()
            .map(|&f| FreqSpec {
                output: f.as_token(),
                query: f,
            })
            .collect()
    }

    /// Helper: unwrap the remote variant, panicking otherwise. All the
    /// non-`csv:` parse tests below use it.
    fn remote(spec: &str) -> (String, Vec<SymbolSpec>) {
        match parse_spec(spec).unwrap() {
            FetchSpec::Remote { provider, symbols } => (provider, symbols),
            FetchSpec::Csv { path } => panic!("expected Remote, got Csv({})", path.display()),
        }
    }

    #[test]
    fn parses_single_symbol_single_freq() {
        let (provider, symbols) = remote("binance:BTCUSDT[1d]");
        assert_eq!(provider, "binance");
        assert_eq!(symbols.len(), 1);
        // No `=`: the fetched symbol is also the emitted one.
        assert_eq!(symbols[0].output, "BTCUSDT");
        assert_eq!(symbols[0].query, "BTCUSDT");
        assert_eq!(symbols[0].freqs, plain(&[Interval::Day(1)]));
    }

    #[test]
    fn freq_prefix_relabels_the_emitted_cadence() {
        // The intended use: same duration under a different name. Fetch 24h
        // bars, tag them `1d` so the `freq` column agrees with the series being
        // joined against.
        let (_, symbols) = remote("binance:BTCUSDT[1d=24h,1d]");
        assert_eq!(
            symbols[0].freqs[0],
            FreqSpec {
                output: "1d".to_string(),
                query: Interval::Hour(24),
            }
        );
        // A plain entry in the same bracket stays unmapped.
        assert_eq!(symbols[0].freqs[1], plain(&[Interval::Day(1)])[0]);
    }

    #[test]
    fn the_emitted_freq_label_is_an_opaque_string() {
        // Only the fetched side has to be an interval; the label is written to
        // the CSV verbatim and never parsed back by the `--series` loader.
        let (_, symbols) = remote("cg:BTCUSDT=bitcoin[FOO=1d]");
        assert_eq!(
            symbols[0].freqs[0],
            FreqSpec {
                output: "FOO".to_string(),
                query: Interval::Day(1),
            }
        );
    }

    #[test]
    fn rejects_malformed_freq_mapping() {
        assert!(parse_spec("cg:BTCUSDT=bitcoin[=1h]").is_err());
        assert!(parse_spec("cg:BTCUSDT=bitcoin[4h=]").is_err());
        // The *fetched* side must be a real interval token — it is what gets
        // sent to the provider. (The emitted label is free-form.)
        assert!(parse_spec("cg:BTCUSDT=bitcoin[4h=1x]").is_err());
    }

    #[test]
    fn output_prefix_remaps_the_emitted_symbol() {
        let (provider, symbols) = remote("cg:BTCUSDT=bitcoin[1d],ETHUSDT=ethereum[1d]");
        assert_eq!(provider, "cg");
        assert_eq!(symbols.len(), 2);
        // Fetch under the provider's id, emit under the price series' key.
        assert_eq!(symbols[0].query, "bitcoin");
        assert_eq!(symbols[0].output, "BTCUSDT");
        assert_eq!(symbols[1].query, "ethereum");
        assert_eq!(symbols[1].output, "ETHUSDT");
    }

    #[test]
    fn output_prefix_tolerates_whitespace_and_mixes_with_plain_entries() {
        let (_, symbols) = remote("binance: BTCEUR = BTCUSDT [1d] , ETHEUR[1d]");
        assert_eq!(symbols[0].output, "BTCEUR");
        assert_eq!(symbols[0].query, "BTCUSDT");
        // A plain entry alongside a mapped one still defaults output = query.
        assert_eq!(symbols[1].output, "ETHEUR");
        assert_eq!(symbols[1].query, "ETHEUR");
    }

    #[test]
    fn rejects_half_empty_output_mapping() {
        assert!(parse_spec("cg:=bitcoin[1d]").is_err());
        assert!(parse_spec("cg:BTCUSDT=[1d]").is_err());
    }

    #[test]
    fn label_shows_each_mapping_only_when_there_is_one() {
        let mapped = Series {
            provider: "cg".into(),
            output: "BTCUSDT".into(),
            query: "bitcoin".into(),
            interval: Interval::Day(1),
            out_freq: "1d".to_string(),
            stable: 0,
            csv_bars: None,
            csv_path: None,
        };
        // Symbol remapped, freq not.
        assert_eq!(mapped.label(), "cg:BTCUSDT=bitcoin[1d]");

        // Both remapped.
        let both = Series {
            interval: Interval::Day(1),
            out_freq: "FOO".to_string(),
            ..mapped.clone()
        };
        assert_eq!(both.label(), "cg:BTCUSDT=bitcoin[FOO=1d]");

        // Neither: the plain form is unchanged from before this grammar existed.
        let plain = Series {
            query: "BTCUSDT".into(),
            output: "BTCUSDT".into(),
            provider: "binance".into(),
            ..mapped
        };
        assert_eq!(plain.label(), "binance:BTCUSDT[1d]");
    }

    #[test]
    fn overlay_and_candle_providers_cannot_be_mixed() {
        let candles = parse_spec("binance:BTCUSDT[1d]").unwrap();
        let overlays = parse_spec("cg:BTCUSDT=bitcoin[1d]").unwrap();
        let csv = parse_spec("csv:./x.csv").unwrap();

        assert_eq!(
            resolve_mode(std::slice::from_ref(&candles)).unwrap(),
            ProviderKind::Candles
        );
        assert_eq!(
            resolve_mode(std::slice::from_ref(&overlays)).unwrap(),
            ProviderKind::Overlays
        );
        // `csv:` is a candle source, so it clashes with an overlay provider too.
        assert!(resolve_mode(&[candles, overlays.clone()]).is_err());
        assert!(resolve_mode(&[csv, overlays]).is_err());
    }

    #[test]
    fn parses_multi_symbol_multi_freq() {
        let (_, symbols) = remote("binance:BTCUSDT[1d,1h],ETHUSDT[1d]");
        assert_eq!(symbols.len(), 2);
        assert_eq!(
            symbols[0].freqs,
            plain(&[Interval::Day(1), Interval::Hour(1)])
        );
        assert_eq!(symbols[1].freqs, plain(&[Interval::Day(1)]));
    }

    #[test]
    fn parses_csv_spec_without_bracket() {
        let got = parse_spec("csv:./candles.csv").unwrap();
        match got {
            FetchSpec::Csv { path } => assert_eq!(path, PathBuf::from("./candles.csv")),
            other => panic!("expected Csv, got {other:?}"),
        }
    }

    #[test]
    fn parses_csv_spec_with_absolute_path() {
        let got = parse_spec("csv:/tmp/data.csv").unwrap();
        match got {
            FetchSpec::Csv { path } => assert_eq!(path, PathBuf::from("/tmp/data.csv")),
            other => panic!("expected Csv, got {other:?}"),
        }
    }

    #[test]
    fn rejects_empty_csv_path() {
        assert!(parse_spec("csv:").is_err());
        assert!(parse_spec("csv:   ").is_err());
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
        let (_, symbols) = remote("binance: BTCUSDT [ 1d , 1h ] , ETHUSDT [1d]");
        assert_eq!(symbols.len(), 2);
        assert_eq!(
            symbols[0].freqs,
            plain(&[Interval::Day(1), Interval::Hour(1)])
        );
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
