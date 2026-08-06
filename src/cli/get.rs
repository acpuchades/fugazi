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
    self, Binance, CandleSource, CoinGecko, Interval, OverlayRow, OverlaySource,
    Timestamp, Yahoo, binance::binance_schema, yahoo::yahoo_schema,
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
    /// Indicator overlays: a YAML mapping of `column_name: ExprSpec`.
    /// Deserialized as a raw JSON value so the typed ExprSpec parse can reuse
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
    let (output, query) = split_remap(s).with_context(|| format!("symbol {s:?}"))?;
    Ok(SymbolSpec {
        output,
        query,
        freqs: vec![interval],
    })
}

/// Split a symbol head into `(emitted, fetched)` on the first **unescaped** `=`,
/// unescaping both sides. With no unescaped `=`, the whole (unescaped) string is
/// both.
///
/// A ticker that legitimately contains `=` — Yahoo's `EURUSD=X`, `ES=F` — writes
/// it `\=`, so `yfinance:EURUSD\=X[1d]` fetches and emits `EURUSD=X` while
/// `cg:BTCUSDT=bitcoin[1d]` still means "fetch `bitcoin`, emit `BTCUSDT`". Both
/// sides accept the escape, so an emitted label may carry one too
/// (`ES\=F=ES=F` is a redundant but legal way to spell the same thing).
///
/// Escapes are `\=` → `=` and `\\` → `\`; any other backslash sequence is an
/// error rather than a silent passthrough, so a typo surfaces at parse time.
///
/// Note for callers writing shell: the shell eats a bare backslash, so the
/// argument needs quoting — `'yfinance:EURUSD\=X[1d]'` or `EURUSD\\=X`.
/// Thin `anyhow` wrappers over the shared symbol-escape rules in
/// [`crate::calendar`], which the `-x` / `--costs` scope splitters use too.
fn unescape(s: &str) -> Result<String> {
    crate::calendar::unescape_symbol(s).map_err(|e| anyhow!("{e}"))
}

fn escape(s: &str) -> String {
    crate::calendar::escape_symbol(s)
}

fn split_remap(head: &str) -> Result<(String, String)> {
    let mut escaped = false;
    let mut split_at = None;
    for (i, c) in head.char_indices() {
        if escaped {
            escaped = false;
            continue;
        }
        match c {
            '\\' => escaped = true,
            '=' => {
                split_at = Some(i);
                break;
            }
            _ => {}
        }
    }
    let Some(i) = split_at else {
        let whole = unescape(head)?;
        return Ok((whole.clone(), whole));
    };
    // Unescape before trimming: `A\ =B`'s left side is `A\ `, and trimming
    // first would leave a dangling `\` and report that instead of the real
    // problem (an unknown `\ ` escape).
    let output = unescape(&head[..i])?.trim().to_string();
    let query = unescape(&head[i + 1..])?.trim().to_string();
    if output.is_empty() {
        bail!("empty output symbol on the left of `=`");
    }
    if query.is_empty() {
        bail!("empty provider query on the right of `=`");
    }
    Ok((output, query))
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
        "cg" => ProviderKind::Overlays,
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
/// With no unescaped `=`, output and query are the same string. A symbol that
/// contains a literal `=` escapes it as `\=` — see [`split_remap`].
#[derive(Debug, Clone, PartialEq)]
struct SymbolSpec {
    /// The value written to the `symbol` column — the `--series` join key.
    output: String,
    /// The identifier sent to the provider.
    query: String,
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
             \x20 fugazi get cg:BTCUSDT=bitcoin[1d]     -o caps.csv\n\
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

    let mode = resolve_mode(&fetch_specs)?;
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

    match mode {
        ProviderKind::Candles => {
            run_candles(args, fetch_specs, since_ts, until_ts, since_specified, dataset_overlays, &output, &rt)
        }
        ProviderKind::Overlays => {
            run_overlay_columns(args, fetch_specs, since_ts, until_ts, &output, &rt)
        }
    }
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
                // kline extras, Yahoo's `adj_close`. Every atom in a fetch
                // will bind to this via `OverlayInfo::new(schema, ...)`.
                let schema = match provider.as_str() {
                    "binance" => binance_schema().clone(),
                    "yfinance" => yahoo_schema().clone(),
                    _ => Schema::empty(),
                };
                for sym in symbols {
                    for &freq in &sym.freqs {
                        let stable = overlay::stable_period_for(
                            &overlays,
                            &overlay_columns,
                            &sym.output,
                            freq,
                            &schema,
                        );
                        series.push(Series {
                            provider: provider.clone(),
                            output: sym.output.clone(),
                            query: sym.query.clone(),
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
                    );
                    // A `csv:` file already carries its own `symbol` and
                    // `freq` columns, so there is nothing to remap.
                    series.push(Series {
                        provider: "csv".into(),
                        output: sym.clone(),
                        query: sym,
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

    let (multi, bars) =
        build_progress_bars(&series, since_ts, until_ts, since_specified, args.quiet);

    // Async: download every series in parallel — no overlay state crosses task
    // boundaries. Overlays are applied synchronously below, per (symbol,
    // interval) group, so `DynValue`'s non-Send `Rc`-backed `Position` stub
    // stays on one thread. `csv:` series short-circuit inside `fetch_series`.
    let result = rt.block_on(fetch_all(series.clone(), since_ts, until_ts, since_specified, bars));
    let _ = multi.clear();
    let raw = result?;
    warn_short_history(&series, &raw, since_ts, since_specified, args.keep_unstable);
    let rows = apply_overlays(
        raw,
        since_ts,
        since_specified,
        args.keep_unstable,
        &overlays,
        &overlay_columns,
    );

    write_candles_csv(output, &rows, &overlay_columns)
        .with_context(|| format!("writing {}", output.display()))?;

    if !args.quiet {
        print_result_block(rows.len(), n_symbols, series.len());
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
    output: &Path,
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
        print_inputs_block(&args, since_ts, until_ts, false, &[], output);
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
            for &freq in &sym.freqs {
                series.push(Series {
                    provider: provider.clone(),
                    output: sym.output.clone(),
                    query: sym.query.clone(),
                    interval: freq,
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

    write_overlays_csv(output, &rows)
        .with_context(|| format!("writing {}", output.display()))?;

    if !args.quiet {
        print_result_block(rows.len(), n_symbols, series.len());
    }
    Ok(())
}

/// One overlay row of output: the emitted symbol + interval it belongs to, its
/// bar-open time, and the provider's per-bar values. The candle-less twin of
/// [`Row`].
struct OverlayOut {
    symbol: String,
    /// The `freq` cell — the fetched cadence's own token.
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
            freq: series.interval.as_token(),
            time: r.time,
            overlays: r.overlays,
        }));
        bar.inc(1);
    }
    // Same chunk-boundary dedup as `fetch_series` — a provider that resolves the
    // range in exchange-local time can return one row in two adjacent UTC chunks
    // (see the note on `dedup_by_time`). Keep the first occurrence per timestamp.
    dedup_by_time(&mut rows, |r| Some(r.time.0));
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
    /// The symbol this series' rows are *written* under — the `--series` join
    /// key. Equal to `query` unless the spec used the `OUTPUT=QUERY` form.
    output: String,
    /// The identifier this series is *fetched* with (a CoinGecko coin id, a
    /// Binance pair, …). See [`SymbolSpec`].
    query: String,
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
                escape(&self.output),
                self.interval.as_token()
            );
        }
        // Echo the mapping when there is one, so the progress line makes the
        // fetched-vs-emitted distinction visible while it runs. Re-escaped, so
        // what is printed is a spec that parses back to this series.
        let symbol = if self.output == self.query {
            escape(&self.query)
        } else {
            format!("{}={}", escape(&self.output), escape(&self.query))
        };
        format!("{}:{}[{}]", self.provider, symbol, self.interval.as_token())
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

/// Drop entries whose timestamp `key` has already appeared, keeping the first
/// occurrence and preserving order. This restores the per-series
/// `(symbol, interval, timestamp)` uniqueness invariant at the stitch point that
/// splices the [`chunk_bounds`] windows back together.
///
/// The windows are half-open and non-overlapping *in UTC*, but a provider that
/// resolves the fetch range in exchange-local time can still return one bar in
/// two adjacent chunks. Yahoo FX stamps a daily bar for trading day D at
/// D-1T23:00Z under European summer time, so that bar is *before* the UTC
/// midnight boundary between the two chunks: it satisfies the earlier chunk's
/// UTC upper bound *and* the later chunk's local-time range, and both chunks
/// emit it. A `None` key — a synthetic, timeless atom; remote providers always
/// stamp `time` — is never treated as a duplicate.
fn dedup_by_time<T>(rows: &mut Vec<T>, key: impl Fn(&T) -> Option<i64>) {
    let mut seen = std::collections::HashSet::new();
    rows.retain(|r| match key(r) {
        Some(t) => seen.insert(t),
        None => true,
    });
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
        // Rows are tagged with the *emitted* symbol — the join key.
        rows.extend(atoms.into_iter().map(|atom| RawBar {
            symbol: series.output.clone(),
            interval: series.interval,
            atom,
        }));
        bar.inc(1);
    }
    dedup_by_time(&mut rows, |b| b.atom.time.map(|t| t.0));
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
    out
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
            .filter(|b| b.symbol == s.output && b.interval == s.interval)
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
    let (output, query) = split_remap(head).with_context(|| format!("{s:?}"))?;
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
        output,
        query,
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
        assert_eq!(symbols[0].output, "BTCUSDT");
        assert_eq!(symbols[0].query, "BTCUSDT");
        assert_eq!(symbols[0].freqs, vec![Interval::Day(1)]);
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
    fn an_escaped_equals_is_part_of_the_symbol() {
        // Yahoo's FX / futures tickers carry `=`. Escaped, the head is one
        // symbol — no remap — and the `=` survives into both sides.
        let (provider, symbols) = remote(r"yfinance:EURUSD\=X[1d],ES\=F[1h]");
        assert_eq!(provider, "yfinance");
        assert_eq!(symbols[0].output, "EURUSD=X");
        assert_eq!(symbols[0].query, "EURUSD=X");
        assert_eq!(symbols[0].freqs, vec![Interval::Day(1)]);
        assert_eq!(symbols[1].output, "ES=F");
        assert_eq!(symbols[1].query, "ES=F");
    }

    #[test]
    fn escapes_work_on_either_side_of_a_remap() {
        // Emit under an escaped label, fetch an escaped id: the split happens
        // at the first *unescaped* `=` only.
        let (_, symbols) = remote(r"yfinance:EURUSD\=X=EURUSD\=X[1d]");
        assert_eq!(symbols[0].output, "EURUSD=X");
        assert_eq!(symbols[0].query, "EURUSD=X");

        let (_, mapped) = remote(r"yfinance:EURUSD=EURUSD\=X[1d]");
        assert_eq!(mapped[0].output, "EURUSD");
        assert_eq!(mapped[0].query, "EURUSD=X");

        // `\\` is the other escape — a literal backslash, not an escaper.
        let (_, backslash) = remote(r"binance:A\\B[1d]");
        assert_eq!(backslash[0].query, r"A\B");
    }

    #[test]
    fn rejects_unknown_and_dangling_escapes() {
        // A typo surfaces at parse time rather than silently passing through.
        assert!(parse_spec(r"binance:BTC\USDT[1d]").is_err());
        assert!(parse_spec(r"binance:BTCUSDT\[1d]").is_err());
    }

    #[test]
    fn an_escape_error_names_the_escape_not_the_padding() {
        // Unescaping runs before the whitespace trim, so `A\ =B` reports the
        // unknown `\ ` escape rather than a dangling backslash left by trimming.
        let err = parse_spec(r"binance:A\ =B[1d]").unwrap_err().to_string();
        let chain = format!("{:#}", parse_spec(r"binance:A\ =B[1d]").unwrap_err());
        assert!(chain.contains("unknown escape"), "{err} / {chain}");
    }

    #[test]
    fn dataset_symbols_share_the_remap_and_escape_rules() {
        // The bracket-less `@dataset.yml` path goes through the same splitter.
        let mapped = parse_symbol_plain("BTCUSDT=bitcoin", Interval::Day(1)).unwrap();
        assert_eq!(mapped.output, "BTCUSDT");
        assert_eq!(mapped.query, "bitcoin");
        assert_eq!(mapped.freqs, vec![Interval::Day(1)]);

        let escaped = parse_symbol_plain(r"EURUSD\=X", Interval::Day(1)).unwrap();
        assert_eq!(escaped.output, "EURUSD=X");
        assert_eq!(escaped.query, "EURUSD=X");

        assert!(parse_symbol_plain(r"BTC\USDT", Interval::Day(1)).is_err());
    }

    #[test]
    fn every_freq_token_must_be_a_real_interval() {
        // There is no relabel form: each token is parsed as a cadence.
        assert!(parse_spec("binance:BTCUSDT[1d=24h]").is_err());
        assert!(parse_spec("binance:BTCUSDT[FOO]").is_err());
        assert!(parse_spec("binance:BTCUSDT[1x]").is_err());
    }

    #[test]
    fn rejects_half_empty_output_mapping() {
        assert!(parse_spec("cg:=bitcoin[1d]").is_err());
        assert!(parse_spec("cg:BTCUSDT=[1d]").is_err());
    }

    #[test]
    fn label_echoes_a_mapping_only_when_there_is_one_and_re_escapes() {
        let mapped = Series {
            provider: "cg".into(),
            output: "BTCUSDT".into(),
            query: "bitcoin".into(),
            interval: Interval::Day(1),
            stable: 0,
            csv_bars: None,
            csv_path: None,
        };
        assert_eq!(mapped.label(), "cg:BTCUSDT=bitcoin[1d]");

        // Unmapped: the plain form, with any literal `=` escaped back so the
        // echoed spec parses to this same series.
        let plain = Series {
            provider: "yfinance".into(),
            output: "EURUSD=X".into(),
            query: "EURUSD=X".into(),
            ..mapped
        };
        assert_eq!(plain.label(), r"yfinance:EURUSD\=X[1d]");
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
    fn dedup_by_time_drops_chunk_boundary_duplicates() {
        // Simulates the stitched output of two adjacent `chunk_bounds` windows
        // whose boundary lands one hour after a Yahoo FX summer-time bar: the
        // bar stamped at the boundary-minus-1h (`230000`) is returned by both
        // the earlier chunk (its UTC upper bound) and the later chunk (which
        // Yahoo resolves in exchange-local time), so it appears twice in a row
        // once the chunks are spliced.
        const BOUNDARY: i64 = 240000;
        let mut rows = vec![
            (BOUNDARY - 3600, 'a'), // last real bar of chunk 1
            (BOUNDARY - 10, 'b'),   // summer-time boundary bar, from chunk 1
            (BOUNDARY - 10, 'b'),   // ...re-emitted by chunk 2 — the duplicate
            (BOUNDARY + 3600, 'c'), // first genuinely-new bar of chunk 2
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
