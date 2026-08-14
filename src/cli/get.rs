//! The `fugazi get` subcommand: fetch OHLCV bars from remote providers and
//! write them to a `,`-delimited CSV in the same shape `--series` reads back.
//!
//! Takes one or more specs, each either a provider spec string or a `@dataset.yml`
//! file reference:
//!
//! * **Inline spec:** `<provider>:<symbol>[<freq>(,<freq>)*](,<symbol>[...])*`
//! * **Dataset file:** `@path/to/dataset.yml` — YAML with `name`, optional
//!   `description`, a single `interval`, and a list of `sources` (each with
//!   `provider` and `symbols`). The interval is shared across all sources;
//!   this enforces that a dataset is always a single-frequency universe.
//!
//! Every symbol/interval series across all specs downloads concurrently, one
//! progress bar per series. Example:
//!
//! ```text
//! fugazi get binance:BTCUSDT[1d,1h],ETHUSDT[1d] yfinance:AAPL[1d] \
//!            --since 2020-01-01 --until today \
//!            -o candles.csv
//!
//! fugazi get @datasets/crypto/large-cap-1d.yml --since 2019-01-01 -o candles.csv
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

use std::collections::HashMap;
use std::io::IsTerminal;
use std::path::{Path, PathBuf};
use std::str::FromStr;
use std::sync::Arc;
use std::time::Duration as StdDuration;

use anyhow::{Context, Result, anyhow, bail};
use clap::Args;
use indicatif::{ProgressBar, ProgressStyle};
use time::format_description::well_known::Rfc3339;
use time::{Date, Duration, Month, OffsetDateTime, Time};
use tokio::runtime::Builder as RuntimeBuilder;
use tokio::task::JoinSet;

use fugazi::prelude::*;
use fugazi::sources::{
    self, Binance, BinanceVision, Coinbase, CoinGecko, Interval, Okx, SeriesSource,
    Timestamp, Yahoo, binance::binance_schema, okx::okx_schema, yahoo::yahoo_schema,
};

use serde_json::Value as Json;

use crate::dyn_indicator::{DynIndicator, DynValue};
use crate::csv_source::{CsvBar, CsvSource};
use crate::input::Source as InputSource;
use crate::overlay::{self, Overlay};
use crate::params;
use crate::style;

/// Metadata extracted from a `@file.yml` spec: the dataset name, any default
/// time-range hints declared in the YAML, and overlay columns to compute.
/// CLI flags always override the time-range hints; CLI `--overlay` args are
/// appended after the dataset overlays so same-name columns prefer the CLI.
struct DatasetMeta {
    name: String,
    since: Option<String>,
    until: Option<String>,
    /// Indicator overlays declared in the dataset YAML — pre-parsed so they
    /// don't need re-parsing in `run_candles`. CLI `--overlay` args are
    /// appended after these, so the CLI wins for same-name columns.
    overlays: Vec<Overlay>,
}

/// A dataset descriptor loaded from a `@file.yml` spec argument.
///
/// A dataset defines a fixed universe of symbols and a single shared interval.
/// The interval is intentionally top-level (not per-source) to enforce that
/// every series in a dataset runs at the same cadence — multi-frequency datasets
/// are not yet well-supported in the backtesting workflow.
///
/// `since` and `until` are optional hints that set the default time range when
/// those flags are omitted on the CLI. CLI flags always win. Output path is
/// always a runtime concern (`-o`); the descriptor never opinions on it.
///
/// Each source entry is a single-key YAML mapping whose key is the provider
/// name and whose value holds the provider-specific parameters:
///
/// ```yaml
/// sources:
///   - binance:
///       symbols: [BTCUSDT, ETHUSDT]
///   - csv:
///       path: /data/extra.csv
/// ```
///
/// The optional `overlays` mapping defines indicator columns computed from the
/// fetched candles — same format as a standalone `@overlays.yml` file:
///
/// ```yaml
/// overlays:
///   sma20: !sma { period: 20 }
///   ema50: !ema { period: 50 }
/// ```
///
/// `!import path` is resolved before typed parsing, so a shared overlay library
/// can be pulled in with `overlays: !import shared/indicators.yml`.
#[derive(serde::Deserialize)]
struct DatasetSpec {
    name: String,
    #[allow(dead_code)]
    #[serde(default)]
    description: Option<String>,
    interval: String,
    /// Default `--since` (overridden by the CLI flag). ISO `YYYY-MM-DD`.
    #[serde(default)]
    since: Option<String>,
    /// Default `--until` (overridden by the CLI flag). ISO `YYYY-MM-DD` or `today`.
    #[serde(default)]
    until: Option<String>,
    /// Sources as single-key maps `{ provider_name: { params } }`.
    /// Deserialized after `yaml_to_json`, so each entry is already a JSON map.
    sources: Vec<serde_json::Map<String, Json>>,
    /// Indicator overlays: a YAML mapping of `column_name: NodeSpec`.
    /// Deserialized as a raw JSON value so the typed NodeSpec parse can reuse
    /// the same `serde_json::from_value` path the strategy spec uses.
    #[serde(default)]
    overlays: Option<Json>,
}

/// Parse a `@path/to/dataset.yml` spec argument into one [`FetchSpec`] per
/// source entry plus a [`DatasetMeta`] carrying the dataset name, default
/// time-range hints, and pre-parsed overlay columns.
///
/// The full parse pipeline runs here so that `!import` and YAML tags (`!sma`,
/// `!ema`, …) work inside dataset files exactly as they do in strategy specs:
/// `serde_norway::from_str` → `imports::resolve` → `yaml_to_json` →
/// `serde_json::from_value`.
fn parse_dataset(path: &str) -> Result<(Vec<FetchSpec>, DatasetMeta)> {
    let content = std::fs::read_to_string(path)
        .with_context(|| format!("reading dataset {path:?}"))?;
    let base = Path::new(path).parent().unwrap_or(Path::new("."));
    let yaml: serde_norway::Value = serde_norway::from_str(&content)
        .with_context(|| format!("parsing dataset YAML {path:?}"))?;
    let json = crate::spec::convert::yaml_to_json(yaml)
        .with_context(|| format!("normalising tags in dataset {path:?}"))?;
    let json = crate::imports::resolve(json, base)
        .with_context(|| format!("resolving !import in dataset {path:?}"))?;
    let dataset: DatasetSpec = serde_json::from_value(json)
        .with_context(|| format!("parsing dataset {path:?}"))?;
    let interval = crate::calendar::parse_interval(&dataset.interval)
        .with_context(|| format!("dataset {path:?}: interval {:?}", dataset.interval))?;
    let specs = dataset
        .sources
        .into_iter()
        .map(|src_map| {
            if src_map.len() != 1 {
                bail!(
                    "dataset {path:?}: each source entry must have exactly one provider key, got {}",
                    src_map.len()
                );
            }
            let (provider, params) = src_map.into_iter().next().unwrap();
            if provider == "csv" {
                let csv_path = params
                    .get("path")
                    .and_then(|v| v.as_str())
                    .ok_or_else(|| anyhow!("dataset {path:?}: csv source requires a `path` field"))?;
                Ok(FetchSpec::Csv { path: PathBuf::from(csv_path) })
            } else {
                let symbols_raw = params
                    .get("symbols")
                    .and_then(|v| v.as_array())
                    .ok_or_else(|| {
                        anyhow!("dataset {path:?}: source {provider:?} requires a `symbols` list")
                    })?;
                let symbols = symbols_raw
                    .iter()
                    .map(|v| {
                        v.as_str()
                            .ok_or_else(|| anyhow!("symbol must be a string"))
                            .and_then(|s| parse_symbol_plain(s, interval))
                    })
                    .collect::<Result<Vec<_>>>()
                    .with_context(|| {
                        format!("dataset {path:?}: provider {provider:?}")
                    })?;
                Ok(FetchSpec::Remote { provider, symbols })
            }
        })
        .collect::<Result<Vec<_>>>()?;
    let overlays = if let Some(overlays_json) = dataset.overlays {
        overlay::parse_from_value(
            overlays_json,
            &Default::default(),
            &format!("dataset {path:?}"),
        )
        .with_context(|| format!("parsing overlays in dataset {path:?}"))?
    } else {
        Vec::new()
    };
    let meta = DatasetMeta {
        name: dataset.name,
        since: dataset.since,
        until: dataset.until,
        overlays,
    };
    Ok((specs, meta))
}

/// Parse a plain symbol string (no `[freq]` bracket) into a [`SymbolSpec`]
/// with the given interval. Accepts the `OUTPUT=QUERY` remap form used by
/// overlay providers (e.g. `BTCUSDT=bitcoin` for CoinGecko), and the same
/// `\=` escape — see [`split_remap`].
///
/// A dataset YAML listing a provider id that contains `=` (Yahoo's `EURUSD=X`,
/// `JPY=X`) must escape it, or the head splits and only `X` reaches the
/// provider: write `- EURUSD\=X` as a plain or single-quoted scalar, both of
/// which keep the backslash. A *double*-quoted YAML scalar processes escapes
/// itself and rejects `\=`, so don't use that form here.
fn parse_symbol_plain(s: &str, interval: Interval) -> Result<SymbolSpec> {
    let s = s.trim();
    if s.is_empty() {
        bail!("empty symbol name");
    }
    let symbol = s.to_string();
    Ok(SymbolSpec {
        symbol,
        freqs: vec![interval],
    })
}

/// Parse one CLI spec argument into zero or more [`FetchSpec`]s and an optional
/// [`DatasetMeta`]. A `@path` argument loads a dataset YAML and may expand to
/// multiple specs (one per source); any other argument is a single inline spec
/// with no metadata.
fn parse_spec_arg(s: &str) -> Result<(Vec<FetchSpec>, Option<DatasetMeta>)> {
    if let Some(path) = s.strip_prefix('@') {
        let (specs, meta) = parse_dataset(path)
            .with_context(|| format!("loading dataset {path:?}"))?;
        Ok((specs, Some(meta)))
    } else {
        parse_spec(s).map(|f| (vec![f], None))
    }
}

/// The remote candle providers this CLI can fetch from. Kept as `(name,
/// description)` so `fugazi list sources` and the "unknown provider" error
/// message both render from the same table — no drift possible.
pub(crate) const KNOWN_PROVIDERS: &[(&str, &str)] = &[
    (
        "binance",
        "Binance spot klines endpoint (BTC/ETH/... vs. USDT/EUR/...)",
    ),
    (
        "binance-vision",
        "Binance spot klines from the public archive (data.binance.vision) — \
         deeper and cheaper than the live endpoint, one request per month and no \
         rate limit, at the cost of a ~2-day lag. Same columns as `binance`",
    ),
    (
        "binance-vision-futures",
        "Binance USDⓈ-M perpetual klines from the same archive, plus the side \
         channels only a derivative has: funding rate (summed within a bar, so \
         `[1d]` is that day's carry), premium index, open interest and the \
         long/short ratios. Hourly to daily only",
    ),
    (
        "okx",
        "OKX spot candlesticks endpoint (symbols are dash-separated: `BTC-USDT`, \
         `ETH-USDT`). Day/week/month bars are UTC-aligned",
    ),
    (
        "coinbase",
        "Coinbase Advanced Trade candles endpoint (symbols are dash-separated \
         product ids: `BTC-USD`, `ETH-USD`). Fixed cadences only: 1m/5m/15m/30m, \
         1h/2h/6h, 1d",
    ),
    (
        "cg",
        "CoinGecko market cap / volume / supply — overlay columns only, no OHLCV \
         (symbols are coin ids: `bitcoin`, not `BTC`)",
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
/// With no unescaped `=`, output and query are the same string. A symbol that
/// contains a literal `=` escapes it as `\=` — see [`split_remap`].
#[derive(Debug, Clone, PartialEq)]
struct SymbolSpec {
    /// The symbol: sent to the provider, and written to the `symbol` column.
    symbol: String,
    /// The cadences to fetch. Each is written to the `freq` column as its own
    /// canonical token, and is what `-x` scope prefixes match against.
    freqs: Vec<Interval>,
}

/// A parsed CLI `get` spec.
///
/// Remote providers share the same
/// `<provider>:<symbol>[<freq>,...],<symbol>[<freq>,...]`
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
}

#[derive(Args, Debug)]
pub struct GetArgs {
    /// Fetch specs: one or more of the following, all series downloading in parallel:
    ///
    /// * **Inline:** `<provider>:[OUT=]<symbol>[<freq>,...](,...)*`, e.g.
    ///   `binance:BTCUSDT[1d,1h],ETHUSDT[1d]`. Frequency tokens: `1m`/`5m`/`1h`/`4h`/`1d`/`1w`/`1M`.
    ///
    /// * **Dataset file:** `@path/to/dataset.yml` — a YAML descriptor with `name`,
    ///   optional `description`, a single top-level `interval`, and `sources` (list
    ///   of `{ provider, symbols }`). All sources share the dataset's interval.
    ///   Example: `fugazi get @datasets/crypto/large-cap-1d.yml --since 2019-01-01 -o out.csv`.
    ///
    /// The symbol accepts an optional `EMITTED=FETCHED` remap — the left side is
    /// written to the CSV, the right side is what the provider is asked for.
    /// Omit it and the two are the same. Use it when a provider's vocabulary
    /// differs from the price series you intend to join against, since `run`
    /// joins on an exact `(symbol, time)` match: `cg:BTCUSDT=bitcoin[1d]`
    /// fetches the coin id `bitcoin` and emits `symbol=BTCUSDT`. The same form
    /// works for a dataset file's `symbols:` entries.
    ///
    /// A ticker that itself contains `=` — Yahoo's `EURUSD=X`, `ES=F` — escapes
    /// it as `\=`, on either side of the remap. Quote the argument so the shell
    /// doesn't eat the backslash: `fugazi get 'yfinance:EURUSD\=X[1d]'`. The
    /// only escapes are `\=` and `\\`; anything else is an error.
    ///
    /// Each row's `freq` cell is the fetched cadence's own token — cadences are
    /// not relabellable.
    ///
    /// Overlay-only providers (`cg`) emit side-channel
    /// columns and no OHLCV, and cannot be mixed with candle providers in one
    /// invocation — fetch each to its own file and pass both to `run -s`.
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

    /// End date (exclusive). Same grammar as `--since`; defaults to `today`,
    /// or to the `until` field in a `@dataset.yml` if it declares one.
    #[arg(long)]
    until: Option<String>,

    /// Output CSV path. Header: `symbol,freq,time,open,high,low,close,volume`.
    /// Parent directories are created if missing.
    /// When a single `@dataset.yml` is given and `-o` is omitted, defaults to
    /// `{name}_{YYYYMMDD}.csv` in the current directory, where the date is today.
    #[arg(short, long, value_name = "FILE")]
    output: Option<PathBuf>,

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

    /// Resolve `!param` placeholders inside `--overlay` expressions. Same shape
    /// as `run --params`: `,`-separated `NAME=value` terms and `@file.yml`
    /// mappings, repeatable, later terms winning. So
    /// `--params FAST=20 -x 'ma=!sma { period: !param FAST }'` parameterizes an
    /// overlay exactly as it does a strategy document. Ignored by candle
    /// providers that define no overlays.
    #[arg(short, long = "params", value_name = "SPEC")]
    params: Vec<params::ParamSpec>,

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

pub fn run(mut args: GetArgs) -> Result<()> {
    let parsed: Vec<(Vec<FetchSpec>, Option<DatasetMeta>)> = args
        .specs
        .iter()
        .map(|s| parse_spec_arg(s).with_context(|| format!("parsing spec {s:?}")))
        .collect::<Result<_>>()?;
    let mut metas: Vec<DatasetMeta> = Vec::new();
    let mut fetch_specs: Vec<FetchSpec> = Vec::new();
    for (specs, meta) in parsed {
        fetch_specs.extend(specs);
        if let Some(m) = meta {
            metas.push(m);
        }
    }

    let now = OffsetDateTime::now_utc();

    // Extract per-dataset metadata upfront (cloned so we can consume `metas` later).
    let n_datasets = metas.len();
    let (dataset_since, dataset_until, dataset_name): (Option<String>, Option<String>, Option<String>) =
        if n_datasets == 1 {
            let m = &metas[0];
            (m.since.clone(), m.until.clone(), Some(m.name.clone()))
        } else {
            (None, None, None)
        };
    // Consume metas to take ownership of the pre-parsed overlays.
    let dataset_overlays: Vec<Overlay> = metas.into_iter().flat_map(|m| m.overlays).collect();

    // --since: CLI wins; fall back to dataset hint; then the hardcoded default.
    let since_specified = args.since.is_some();
    let since_raw = args
        .since
        .as_deref()
        .or(dataset_since.as_deref())
        .unwrap_or(DEFAULT_SINCE);
    let since = parse_date(since_raw, now).with_context(|| format!("--since {since_raw:?}"))?;

    // --until: CLI wins; fall back to dataset hint; then "today".
    let until_raw = args
        .until
        .as_deref()
        .or(dataset_until.as_deref())
        .unwrap_or("today");
    let until = parse_date(until_raw, now).with_context(|| format!("--until {until_raw:?}"))?;

    if until <= since {
        bail!("--until ({until_raw}) must be strictly after --since ({since_raw})");
    }
    let since_ts = Timestamp::from_datetime(since);
    let until_ts = Timestamp::from_datetime(until);

    // -o: explicit path wins; fall back to `{name}_{YYYYMMDD}.csv` when a single
    // dataset is given; otherwise the flag is required.
    let output: PathBuf = match args.output.take() {
        Some(path) => path,
        None => match dataset_name.as_deref() {
            Some(name) => {
                let safe = name.replace(['/', '\\', ' '], "-");
                let d = now.date();
                PathBuf::from(format!(
                    "{safe}_{:04}{:02}{:02}.csv",
                    d.year(),
                    d.month() as u8,
                    d.day(),
                ))
            }
            None if n_datasets == 0 => {
                bail!("-o/--output is required when no @dataset.yml spec is given")
            }
            None => bail!("-o/--output is required when multiple @dataset.yml specs are given"),
        },
    };

    if let Some(parent) = output.parent()
        && !parent.as_os_str().is_empty()
    {
        std::fs::create_dir_all(parent)
            .with_context(|| format!("creating {}", parent.display()))?;
    }

    let rt = RuntimeBuilder::new_current_thread()
        .enable_all()
        .build()
        .context("building tokio runtime")?;

    // One pipeline for every provider. A series that is not a price is an
    // ordinary series whose atoms carry no candle, so there is nothing left to
    // route around: the writer omits the OHLCV block when no row has a bar and
    // blanks it per-row when only some do.
    run_candles(
        args,
        fetch_specs,
        since_ts,
        until_ts,
        since_specified,
        dataset_overlays,
        &output,
        &rt,
    )
}

/// The OHLCV pipeline: fetch candles, compute `-x` overlays over them, write
/// `symbol,freq,time,open,high,low,close,volume,...`.
///
/// `dataset_overlays` are the pre-parsed indicator columns from a `@dataset.yml`
/// spec; CLI `--overlay` args are appended after them so the CLI wins for any
/// same-name column.
#[allow(clippy::too_many_arguments)]
fn run_candles(
    args: GetArgs,
    fetch_specs: Vec<FetchSpec>,
    since_ts: Timestamp,
    until_ts: Timestamp,
    since_specified: bool,
    dataset_overlays: Vec<Overlay>,
    output: &Path,
    rt: &tokio::runtime::Runtime,
) -> Result<()> {
    let param_table = params::table(&args.params)?;
    let cli_overlays = overlay::parse_specs(&args.overlay, &param_table)?;
    // Dataset overlays first; CLI overlays appended so same-name CLI column wins.
    let overlays: Vec<Overlay> = dataset_overlays.into_iter().chain(cli_overlays).collect();
    let overlay_columns = overlay::column_names(&overlays);

    if !args.quiet {
        style::print_header("get", "fetch OHLCV candles from remote providers");
        print_inputs_block(&args, since_ts, until_ts, since_specified, &overlay_columns, output);
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
                // kline extras, Yahoo's `raw_close` (its candles are adjusted
                // by default). Every atom in a fetch will bind to this via
                // `OverlayInfo::new(schema, ...)`.
                let schema = match provider.as_str() {
                    "binance" => binance_schema().clone(),
                    "okx" => okx_schema().clone(),
                    // The CLI fetches Yahoo with the provider default (adjusted
                    // candles + `raw_close` overlay).
                    "yfinance" => yahoo_schema(true).clone(),
                    _ => Schema::empty(),
                };
                for sym in symbols {
                    for &freq in &sym.freqs {
                        let stable = overlay::stable_period_for(
                            &overlays,
                            &overlay_columns,
                            &sym.symbol,
                            freq,
                            &schema,
                        )?;
                        series.push(Series {
                            provider: provider.clone(),
                            symbol: sym.symbol.clone(),
                            interval: freq,
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
                    )?;
                    // A `csv:` file already carries its own `symbol` and
                    // `freq` columns, so there is nothing to remap.
                    series.push(Series {
                        provider: "csv".into(),
                        symbol: sym,
                        interval,
                        stable,
                        csv_bars: Some(shared.clone()),
                        csv_path: Some(path.clone()),
                    });
                }
                n_symbols += seen_symbols.len();
            }
        }
    }

    // Level the warm-up preroll across every series. Each group's figure above
    // covers only the overlays that run *for that group*, but a cross-symbol
    // column makes one group's output depend on another group's history — a
    // `!correlation` against SPY is only settled once SPY is settled too.
    // Rather than trace which columns reference which symbols, fetch every
    // series back to the deepest warm-up any of them needs. Over-fetching is
    // free: the extra leading rows are trimmed right back out below.
    if let Some(deepest) = series.iter().map(|s| s.stable).max() {
        for s in series.iter_mut() {
            s.stable = deepest;
        }
    }

    let progress = build_progress(series.len(), args.quiet);

    // Async: download every series in parallel — no overlay state crosses task
    // boundaries. Overlays are applied synchronously below, per (symbol,
    // interval) group, so `DynValue`'s non-Send `Rc`-backed `Position` stub
    // stays on one thread. `csv:` series short-circuit inside `fetch_series`.
    let result = rt.block_on(fetch_all(
        series.clone(),
        since_ts,
        until_ts,
        since_specified,
        progress.clone(),
    ));
    progress.finish_and_clear();
    let raw = result?;
    warn_short_history(&series, &raw, since_ts, since_specified, args.keep_unstable);
    let rows = apply_overlays(
        raw,
        since_ts,
        since_specified,
        args.keep_unstable,
        &overlays,
        &overlay_columns,
    )?;
    warn_empty_overlay_columns(&rows, &overlay_columns, args.quiet);

    write_candles_csv(output, &rows, &overlay_columns)
        .with_context(|| format!("writing {}", output.display()))?;

    if !args.quiet {
        print_result_block(rows.len(), n_symbols, series.len());
    }
    Ok(())
}






/// One row of output: which symbol + interval it came from, the timed candle,
/// the per-`-x`-column overlay values (aligned with the CLI's overlay column
/// layout — `None` for a column no applicable overlay covers this row's
/// group), and the pass-through extras from a `csv:` source (per-row
/// non-OHLCV cells classified as `Real`/`Bool`/`Str`).
struct Row {
    symbol: String,
    /// The `freq` cell — the fetched cadence's own token.
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
    /// The symbol: sent to the provider, and written to the `symbol` column
    /// that `--series` joins on.
    symbol: String,
    /// The cadence fetched — what chunking, pagination, the provider call, `-x`
    /// scope matching, and the emitted `freq` cell all use.
    interval: Interval,
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
                self.symbol,
                self.interval.as_token()
            );
        }
        // Re-escaped, so what is printed is a spec that parses back to this
        // series.
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

/// Drop entries whose timestamp `key` has already appeared, keeping the first
/// occurrence and preserving order — the per-series
/// `(symbol, interval, timestamp)` uniqueness invariant, enforced at the CLI
/// boundary regardless of how the provider assembled the series.
///
/// Each series is one `atoms()` call, and a provider is free to paginate that
/// range internally; a provider that resolves the fetch range in exchange-local
/// time can return the same bar twice across an internal page boundary. Yahoo
/// FX stamps a daily bar for trading day D at D-1T23:00Z under European summer
/// time, straddling a UTC page boundary, so it can be emitted on both sides.
/// De-duplicating here keeps that provider-side quirk from leaking a repeated
/// row into the output. A `None` key — a synthetic, timeless atom; remote
/// providers always stamp `time` — is never treated as a duplicate.
fn dedup_by_time<T>(rows: &mut Vec<T>, key: impl Fn(&T) -> Option<i64>) {
    let mut seen = std::collections::HashSet::new();
    rows.retain(|r| match key(r) {
        Some(t) => seen.insert(t),
        None => true,
    });
}

/// One un-overlaid downloaded bar in the intermediate fetch result: which
/// symbol + interval it came from, and the fully-populated [`Atom`] the
/// source produced. The atom already carries its bar-open [`Timestamp`] and
/// (for a source that exposes them) a per-bar overlay side channel behind a
/// provider-defined [`Schema`]. `apply_overlays` walks these grouped by
/// `(symbol, interval)` to compute `-x` overlay columns before the final
/// `Row` list is emitted.
struct RawBar {
    symbol: String,
    /// The cadence fetched — what `-x` scopes match against, and what the
    /// emitted `freq` cell spells.
    interval: Interval,
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
    progress: ProgressBar,
) -> Result<Vec<RawBar>> {
    let mut tasks = JoinSet::new();
    for s in series.into_iter() {
        let fetch_since = s.fetch_since(since, since_specified);
        // One shared global bar; each task ticks it by one when its series
        // finishes. `ProgressBar` clones share the same underlying state.
        tasks.spawn(fetch_series(s, fetch_since, until, progress.clone()));
    }
    let mut all: Vec<RawBar> = Vec::new();
    while let Some(joined) = tasks.join_next().await {
        all.extend(joined.context("fetch task panicked")??);
    }
    Ok(all)
}

/// Fetch one series in a single `atoms()` call over the whole
/// `[fetch_since, until)` range, advancing its own progress bar. Overlay-agnostic.
///
/// Pagination and request concurrency are the **provider's** concern — Binance
/// auto-paginates its klines endpoint, BinanceVision fans its archive files out
/// concurrently, Yahoo returns the window in one request — so the CLI hands over
/// the full range and lets each source apply its own transport policy (chunk
/// size, rate-limit pacing, in-flight concurrency) rather than imposing a
/// provider-agnostic one on top. Series still run concurrently (one task each,
/// see [`fetch_all`]).
///
/// A `csv:` series short-circuits: the file has already been read into
/// [`Series::csv_bars`] up front, so this is just an in-memory filter to the
/// series' `(symbol, interval)` and the `[fetch_since, until)` window.
async fn fetch_series(
    series: Series,
    fetch_since: Timestamp,
    until: Timestamp,
    progress: ProgressBar,
) -> Result<Vec<RawBar>> {
    if let Some(csv_bars) = series.csv_bars.clone() {
        let rows: Vec<RawBar> = csv_bars
            .iter()
            .filter(|b| {
                b.symbol == series.symbol
                    && b.interval == series.interval
                    && b.atom.time.map(|t| t.0 >= fetch_since.0 && t.0 < until.0).unwrap_or(false)
            })
            .map(|b| RawBar {
                symbol: series.symbol.clone(),
                interval: b.interval,
                atom: b.atom.clone(),
            })
            .collect();
        progress.inc(1);
        return Ok(rows);
    }
    let label = series.label();
    let atoms = fetch(
        &series.provider,
        &series.symbol,
        series.interval,
        fetch_since,
        until,
    )
    .await
    .with_context(|| format!("fetching {label}"))?;
    // Rows are tagged with the *emitted* symbol — the join key.
    let mut rows: Vec<RawBar> = atoms
        .into_iter()
        .map(|atom| RawBar {
            symbol: series.symbol.clone(),
            interval: series.interval,
            atom,
        })
        .collect();
    dedup_by_time(&mut rows, |b| b.atom.time.map(|t| t.0));
    progress.inc(1);
    Ok(rows)
}

/// Assemble one [`Snapshot`] per bar timestamp, carrying **every** series'
/// atom for that instant, tagged with its `(symbol, freq)`.
///
/// This is what makes a cross-symbol overlay resolve: a column computed for
/// XLF is driven with a snapshot that also holds SPY, so
/// `!close { source: !pick { symbol: SPY } }` finds it, while XLF's own
/// `source:`-omitted leaves read XLF through the group root. Bars are inserted
/// in `(time, symbol, freq)` order so a partial selector — `!pick { freq: 1d }`
/// with no symbol — resolves deterministically via
/// [`Snapshot::find`]'s first-match rule.
///
/// Atoms with no timestamp can't be aligned with anything and are left out;
/// the caller falls back to a size-1 snapshot for those.
fn snapshots_by_time(raw: &[RawBar]) -> HashMap<i64, Snapshot<String>> {
    let mut ordered: Vec<&RawBar> = raw.iter().filter(|b| b.atom.time.is_some()).collect();
    ordered.sort_by(|a, b| {
        (a.atom.time, a.symbol.as_str(), a.interval.as_token())
            .cmp(&(b.atom.time, b.symbol.as_str(), b.interval.as_token()))
    });

    let mut by_time: HashMap<i64, Snapshot<String>> = HashMap::new();
    for bar in ordered {
        let t = bar.atom.time.expect("filtered to timestamped bars").0;
        by_time.entry(t).or_default().push(
            Some(bar.symbol.clone()),
            Frequency::from_str(&bar.interval.as_token()).ok(),
            bar.atom.clone(),
        );
    }
    by_time
}

/// Group raw bars by `(symbol, interval)`, feed each group's bars through its
/// per-group active overlays (last-defined applicable one wins per column;
/// see [`overlay::active_for`]), and drop the leading warm-up rows unless the
/// caller opted to keep them. Bars are then sorted ascending by time (ties
/// broken by symbol, then freq) — the shape the previous overlay-less writer
/// already committed to.
///
/// Each group's indicators are driven with the **whole-market snapshot** for
/// the bar (see [`snapshots_by_time`]), rooted on that group's own
/// `(symbol, freq)`. Groups still hold independent indicator state — a column
/// is one series' answer — but they can now *read* each other.
fn apply_overlays(
    raw: Vec<RawBar>,
    since: Timestamp,
    since_specified: bool,
    keep_unstable: bool,
    overlays: &[Overlay],
    columns: &[String],
) -> Result<Vec<Row>> {
    let by_time = snapshots_by_time(&raw);

    // Bin the incoming stream by `(symbol, interval)` — order within each bin is
    // preserved by the sort below and matches the order the provider paged the
    // bars in (ascending time). The outer sort re-orders across groups. The
    // interval is what `-x` scopes match on, so two cadences of one symbol get
    // their own overlay instances.
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
        bars.sort_by_key(|b| b.atom.time);

        // Every atom in a group shares the same source-provided schema (one
        // `Arc<Schema>`); pluck it off the first atom that carries overlays.
        // Falls back to `Schema::empty()` for a source that exposes no
        // extras — `!get { key }` then fails the build with an unknown key.
        let group_atoms: Vec<Atom> = bars.iter().map(|b| b.atom.clone()).collect();
        let schema = sources::schema_of(&group_atoms);

        // This group's blessed series: a `source:`-omitted leaf in any of its
        // columns reads this symbol out of the shared snapshot.
        let root = overlay::group_root(&symbol, interval);

        let active: Vec<Option<&Overlay>> =
            overlay::active_for(overlays, columns, &symbol, interval);
        let mut instances: Vec<Option<Box<dyn DynIndicator>>> = Vec::with_capacity(active.len());
        for slot in &active {
            instances.push(match slot {
                Some(o) => Some(o.build(&schema, Some(&root))?),
                None => None,
            });
        }
        let has_applicable = instances.iter().any(Option::is_some);

        let mut group_rows: Vec<Row> = bars
            .into_iter()
            .map(|b| {
                // The whole market at this instant. A bar with no timestamp
                // can't be aligned with the rest, so it sees only itself —
                // tagged, so the group root still resolves.
                let snap = b
                    .atom
                    .time
                    .and_then(|t| by_time.get(&t.0))
                    .cloned()
                    .unwrap_or_else(|| Snapshot::single(b.symbol.clone(), b.atom.clone()));
                let values: Vec<Option<OverlayValue>> = instances
                    .iter_mut()
                    .map(|slot| {
                        slot.as_mut().and_then(|inst| {
                            dyn_value_to_overlay(inst.update(DynValue::Snapshot(snap.clone()))?)
                        })
                    })
                    .collect();
                Row {
                    symbol: b.symbol,
                    freq: b.interval.as_token(),
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
    Ok(out)
}

/// Warn (to stderr) about any series whose fetched history falls short of the
/// requested `--since`. Two failure modes hide here, and both are silent
/// otherwise:
///
/// * **Earliest bar later than `--since`.** The provider simply has no history
///   that far back, so the output begins wherever the data actually starts —
///   not at the date the user asked for.
/// * **Incomplete warm-up preroll.** With `--since` set, [`Series::fetch_since`]
///   pulls `stable` extra bars *before* `since` so the first emitted row is
///   already settled, then `apply_overlays` trims back to `>= since` trusting
///   that preroll was there. When the provider's history doesn't reach back far
///   enough to supply it, the leading rows that survive the trim are still
///   inside their warm-up window — unstable / pre-warm overlay values rather
///   than settled ones.
///
/// Only meaningful when the user actually passed `--since` (an omitted flag uses
/// the default anchor and drops leading unstable rows on its own), and only when
/// warm-up bars aren't being kept deliberately. Prints regardless of `--quiet`,
/// which governs the success summary, not correctness warnings.
fn warn_short_history(
    series: &[Series],
    raw: &[RawBar],
    since: Timestamp,
    since_specified: bool,
    keep_unstable: bool,
) {
    if !since_specified {
        return;
    }
    for s in series {
        let Some(earliest) = raw
            .iter()
            .filter(|b| b.symbol == s.symbol && b.interval == s.interval)
            .filter_map(|b| b.atom.time)
            .min()
        else {
            // An empty series returned no bars at all — a fetch problem, not a
            // short-history one; leave it to the (zero-row) summary to surface.
            continue;
        };
        // Preroll is incomplete when the earliest available bar lands after the
        // warm-up start `fetch_since` pushed back to. `earliest > since` implies
        // this (there are then no bars before `since` at all), but it also
        // catches the subtler case where *some* pre-`since` history exists but
        // not the full `stable` bars the trim assumes.
        let warm_incomplete =
            !keep_unstable && s.stable > 0 && earliest > s.fetch_since(since, since_specified);
        let earliest_date = earliest.to_datetime().date();
        let since_date = since.to_datetime().date();

        if earliest > since {
            let mut msg = format!(
                "{}: earliest available candle is {earliest_date}, later than --since \
                 {since_date} — output starts there instead.",
                s.label(),
            );
            if warm_incomplete {
                msg.push_str(&format!(
                    " No warm-up history precedes it, so the first ~{} row(s) may be \
                     unstable/pre-warm; pass --keep-unstable to inspect them.",
                    s.stable,
                ));
            }
            eprintln!("  {} {msg}", style::yellow("warn"));
        } else if warm_incomplete {
            eprintln!(
                "  {} {}: only partial warm-up history precedes --since {since_date} \
                 (provider starts {earliest_date}), so the first rows at --since may be \
                 unstable/pre-warm; pass --keep-unstable to inspect them.",
                style::yellow("warn"),
                s.label(),
            );
        }
    }
}

/// Warn about any overlay column that produced **no value on any row**.
///
/// A uniformly-empty column is indistinguishable, in the CSV, from one whose
/// indicator is still warming up — which is exactly how cross-symbol overlays
/// shipped broken and went unnoticed. The remaining ways to land here are a
/// `!pick { symbol: … }` naming a symbol the fetch doesn't carry (a typo, or a
/// spec pointed at the wrong dataset), a `!get` reading another series' column
/// across a provider boundary (the schema `Arc` differs, so the guard on
/// [`fugazi::indicators::GetReal`] declines), or a warm-up longer than the
/// available history.
///
/// A warning rather than an error, because an all-empty column is legitimate
/// for a leaf that never fires outside a strategy — `!entry` / `!peak` read a
/// stub `Position` here and are documented as producing an empty column. Goes
/// to stderr regardless of `--quiet`, which governs the success summary rather
/// than correctness warnings; suppressed only when there are no rows at all,
/// where every column is trivially empty and the row count already says so.
fn warn_empty_overlay_columns(rows: &[Row], columns: &[String], _quiet: bool) {
    if rows.is_empty() {
        return;
    }
    for (i, name) in columns.iter().enumerate() {
        let any = rows
            .iter()
            .any(|r| r.overlays.get(i).is_some_and(Option::is_some));
        if !any {
            eprintln!(
                "  {} overlay column {name:?} is empty on all {} row(s) — the expression \
                 never produced a value. Check that any `!pick {{ symbol: ... }}` names a \
                 symbol present in the fetched data, and that the warm-up fits the range.",
                style::yellow("warn"),
                rows.len(),
            );
        }
    }
}

/// Build one **global** fetch-progress bar, denominated in series completed —
/// each of the `n_series` series is a single `atoms()` call whose internal
/// pagination the CLI doesn't see, so per-series sub-progress isn't available,
/// and the meaningful aggregate is "how many of N series are done". Series
/// finish out of order (they run concurrently); each ticks the bar by one on
/// completion. A live spinner shows the fetch is working between ticks. Hidden
/// — a no-op sink — when `--quiet` is set or when stderr is not a terminal, so
/// the CLI stays silent when its output is being piped or redirected.
fn build_progress(n_series: usize, quiet: bool) -> ProgressBar {
    let bar = if quiet || !std::io::stderr().is_terminal() {
        ProgressBar::hidden()
    } else {
        ProgressBar::new(n_series as u64)
    };
    bar.set_style(
        ProgressStyle::with_template(
            "  {spinner:.cyan} fetching [{bar:24.cyan/blue}] {pos}/{len} series",
        )
        .expect("progress template compiles")
        .progress_chars("=> "),
    );
    // Steady tick so the spinner animates while requests are in flight.
    bar.enable_steady_tick(StdDuration::from_millis(120));
    bar
}

/// Dispatch on the provider name to a concrete [`SeriesSource`] implementation.
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
        "okx" => Ok(Okx::new()
            .atoms(symbol, interval, since, Some(until))
            .await?),
        "coinbase" => Ok(Coinbase::new()
            .atoms(symbol, interval, since, Some(until))
            .await?),
        "yfinance" => Ok(Yahoo::new()
            .atoms(symbol, interval, since, Some(until))
            .await?),
        "cg" => Ok(CoinGecko::new()
            .atoms(symbol, interval, since, Some(until))
            .await?),
        "binance-vision" => Ok(BinanceVision::new()
            .atoms(symbol, interval, since, Some(until))
            .await?),
        "binance-vision-futures" => Ok(BinanceVision::futures()
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
        "binance-vision" => Ok(BinanceVision::new().tickers().await?),
        "binance-vision-futures" => Ok(BinanceVision::futures().tickers().await?),
        "okx" => Ok(Okx::new().tickers().await?),
        "coinbase" => Ok(Coinbase::new().tickers().await?),
        "cg" => Ok(CoinGecko::new().tickers().await?),
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

/// The `get` inputs block — same structural shape as `run`/`optimize`:
/// specs (what was asked for), period (resolved date range), overlay columns
/// (when present), output file. Uses the shared `style::print_field` so the
/// label column lines up across subcommands.
fn print_inputs_block(
    args: &GetArgs,
    since: Timestamp,
    until: Timestamp,
    since_specified: bool,
    overlay_columns: &[String],
    output: &Path,
) {
    style::print_section("inputs");
    let specs = if args.specs.len() == 1 {
        args.specs[0].clone()
    } else {
        args.specs.join(", ")
    };
    style::print_field("specs", &specs, 8);
    let period_note = if since_specified { "" } else { " (default)" };
    style::print_field(
        "period",
        &format!(
            "{}{period_note} → {}",
            format_date(since),
            format_date(until),
        ),
        8,
    );
    if !overlay_columns.is_empty() {
        style::print_field(
            "overlay",
            &format!(
                "{} column{}: {}",
                overlay_columns.len(),
                if overlay_columns.len() == 1 { "" } else { "s" },
                overlay_columns.join(", "),
            ),
            8,
        );
    }
    style::print_field("output", &output.display().to_string(), 8);
}

/// The `get` result block — rows written, symbol/interval-series count.
fn print_result_block(rows: usize, n_symbols: usize, n_series: usize) {
    println!();
    style::print_section("result");
    style::print_field("rows", &rows.to_string(), 8);
    style::print_field(
        "series",
        &format!(
            "{n_symbols} symbol{} · {n_series} interval series",
            if n_symbols == 1 { "" } else { "s" },
        ),
        8,
    );
}

/// Format a fetch `Timestamp` as `YYYY-MM-DD` for the console — dates only,
/// since the fetch grammar is date-precision and printing HH:MM:SS would add
/// noise the user never gave us.
fn format_date(t: Timestamp) -> String {
    t.to_datetime()
        .date()
        .to_string()
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

    // The OHLCV block is emitted only when some row actually has a bar. A
    // fetch of nothing but overlay series — a funding rate, an open interest —
    // would otherwise carry five columns that are blank in every row, and
    // asserting a price shape a file does not have invites a reader to treat
    // the blanks as zeros. A *mixed* fetch keeps the block: the candle rows
    // need it, and a table has one header, so the overlay rows blank those
    // cells instead (blank rather than zero — `--series` full-joins with
    // last-writer-wins per column, and a zero would clobber a real price).
    let any_candle = rows.iter().any(|r| r.atom.candle.is_some());

    let mut header: Vec<&str> = vec!["symbol", "freq", "time"];
    if any_candle {
        header.extend(["open", "high", "low", "close", "volume"]);
    }
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
        let mut record: Vec<String> = vec![row.symbol.clone(), row.freq.clone(), time];
        if any_candle {
            let ohlcv = |f: fn(&Candle) -> Real| {
                row.atom.candle.as_ref().map(f).map(format_f64).unwrap_or_default()
            };
            record.extend([
                ohlcv(|c| c.open),
                ohlcv(|c| c.high),
                ohlcv(|c| c.low),
                ohlcv(|c| c.close),
                ohlcv(|c| c.volume),
            ]);
        }
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
/// `OUTPUT=` prefix is for and [`split_remap`] for the `\=` escape.
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
    let symbol = head.trim().to_string();
    let inner = &s[open + 1..s.len() - 1];
    let mut freqs = Vec::new();
    for tok in inner.split(',') {
        let tok = tok.trim();
        if tok.is_empty() {
            bail!("{s:?}: empty frequency token in bracket");
        }
        freqs.push(
            crate::calendar::parse_interval(tok)
                .with_context(|| format!("{s:?}: freq {tok:?}"))?,
        );
    }
    if freqs.is_empty() {
        bail!("{s:?}: empty frequency list");
    }
    Ok(SymbolSpec {
        symbol,
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
        assert_eq!(symbols[0].symbol, "BTCUSDT");
        assert_eq!(symbols[0].symbol, "BTCUSDT");
        assert_eq!(symbols[0].freqs, vec![Interval::Day(1)]);
    }








    #[test]
    fn a_symbol_carrying_an_equals_needs_no_escape() {
        // With the `OUT=QUERY` remap gone, nothing in a spec head is
        // `=`-delimited, so Yahoo's tickers are written plainly.
        let (_, symbols) = remote("yfinance:EURUSD=X[1d],ES=F[1d]");
        assert_eq!(symbols[0].symbol, "EURUSD=X");
        assert_eq!(symbols[1].symbol, "ES=F");

        // Same through the bracket-less `@dataset.yml` path.
        let plain = parse_symbol_plain("EURUSD=X", Interval::Day(1)).unwrap();
        assert_eq!(plain.symbol, "EURUSD=X");
    }

    #[test]
    fn a_symbol_carrying_a_colon_survives_the_provider_split() {
        // The provider is split off at the *first* colon, and no provider name
        // contains one — so a CCXT-style perpetual symbol needs no escape here
        // either.
        let (provider, symbols) = remote("binance-vision:BTC/USDT:USDT[1d]");
        assert_eq!(provider, "binance-vision");
        assert_eq!(symbols[0].symbol, "BTC/USDT:USDT");
    }

    #[test]
    fn every_freq_token_must_be_a_real_interval() {
        // There is no relabel form: each token is parsed as a cadence.
        assert!(parse_spec("binance:BTCUSDT[1d=24h]").is_err());
        assert!(parse_spec("binance:BTCUSDT[FOO]").is_err());
        assert!(parse_spec("binance:BTCUSDT[1x]").is_err());
    }


    #[test]
    fn label_round_trips_to_a_spec_that_parses_back() {
        let series = Series {
            provider: "yfinance".into(),
            symbol: "EURUSD=X".into(),
            interval: Interval::Day(1),
            stable: 0,
            csv_bars: None,
            csv_path: None,
        };
        // Verbatim: with no `=`-delimited grammar left in a spec head, the
        // ticker needs no escaping to parse back.
        assert_eq!(series.label(), "yfinance:EURUSD=X[1d]");
        let (_, parsed) = remote(&series.label());
        assert_eq!(parsed[0].symbol, "EURUSD=X");
    }


    #[test]
    fn parses_multi_symbol_multi_freq() {
        let (_, symbols) = remote("binance:BTCUSDT[1d,1h],ETHUSDT[1d]");
        assert_eq!(symbols.len(), 2);
        assert_eq!(symbols[0].freqs, vec![Interval::Day(1), Interval::Hour(1)]);
        assert_eq!(symbols[1].freqs, vec![Interval::Day(1)]);
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
        assert_eq!(symbols[0].freqs, vec![Interval::Day(1), Interval::Hour(1)]);
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
    fn dedup_by_time_drops_duplicate_timestamps() {
        // A provider that resolves its fetch range in exchange-local time can
        // emit the same bar twice across an internal pagination boundary: a
        // Yahoo FX summer-time daily bar stamped at `boundary - 1h` straddles a
        // UTC page edge and comes back on both sides, appearing twice in a row.
        const BOUNDARY: i64 = 240000;
        let mut rows = vec![
            (BOUNDARY - 3600, 'a'), // an earlier bar
            (BOUNDARY - 10, 'b'),   // the summer-time boundary bar
            (BOUNDARY - 10, 'b'),   // ...re-emitted across the page edge — the duplicate
            (BOUNDARY + 3600, 'c'), // a later, genuinely-new bar
        ];
        dedup_by_time(&mut rows, |&(t, _)| Some(t));
        // Duplicate removed, first occurrence kept, order and every distinct
        // timestamp preserved — nothing that appeared only once is dropped.
        assert_eq!(
            rows,
            vec![(BOUNDARY - 3600, 'a'), (BOUNDARY - 10, 'b'), (BOUNDARY + 3600, 'c')]
        );
    }

    #[test]
    fn dedup_by_time_keeps_timeless_atoms() {
        // A `None` key is a synthetic, timeless atom, not a duplicate: two of
        // them must both survive even though their keys compare equal.
        let mut rows = vec![(Some(1_i64), 'a'), (None, 'b'), (None, 'c'), (Some(1), 'd')];
        dedup_by_time(&mut rows, |&(t, _)| t);
        assert_eq!(rows, vec![(Some(1), 'a'), (None, 'b'), (None, 'c')]);
    }

    #[test]
    fn format_f64_strips_trailing_zero() {
        assert_eq!(format_f64(27000.0), "27000");
        assert_eq!(format_f64(27000.5), "27000.5");
        assert_eq!(format_f64(0.00012345), "0.00012345");
    }
}
