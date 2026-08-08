use crate::prelude::*;
// The binding modules were one flat namespace before the split and still read
// as one: each pulls in its siblings, so a cross-module reference needs no path.
#[allow(unused_imports)]
use crate::carriers::*;
#[allow(unused_imports)]
use crate::classes::*;
#[allow(unused_imports)]
use crate::strategy::*;
#[allow(unused_imports)]
use crate::constructors::*;
#[allow(unused_imports)]
use crate::metrics::*;
#[allow(unused_imports)]
use crate::spec::*;

// ---------------------------------------------------------------------------
// Remote candle sources
//
// The library-level `fugazi::sources` API takes only objects/enums; the string
// parsing that maps user-facing kwargs (`freq="1d"`, `since="2024-01-01"`) to
// those objects lives here.
// ---------------------------------------------------------------------------

/// Process-wide tokio runtime, lazily built on first use. Sharing one runtime
/// across fetch calls avoids the ~10ms startup cost of building a fresh one
/// per call and keeps the fetcher thread pool warm.
pub(crate) static SOURCES_RUNTIME: std::sync::OnceLock<tokio::runtime::Runtime> = std::sync::OnceLock::new();

pub(crate) fn sources_runtime() -> &'static tokio::runtime::Runtime {
    SOURCES_RUNTIME.get_or_init(|| {
        tokio::runtime::Builder::new_multi_thread()
            .enable_all()
            .worker_threads(2)
            .thread_name("fugazi-sources")
            .build()
            .expect("build tokio runtime")
    })
}

/// Map a fugazi [`SourceError`] to an appropriate Python exception type.
pub(crate) fn source_error_to_py(e: SourceError) -> PyErr {
    match e {
        SourceError::UnknownSymbol(msg) => PyValueError::new_err(format!("unknown symbol: {msg}")),
        SourceError::UnsupportedInterval(i) => {
            PyValueError::new_err(format!("unsupported interval: {i:?}"))
        }
        other => PyValueError::new_err(other.to_string()),
    }
}

/// Chosen DataFrame library for the return value of `Binance.fetch()` /
/// `fugazi.fetch()`.
#[derive(Clone, Copy)]
pub(crate) enum CandlesOutput {
    Polars,
    Pandas,
    Numpy,
}

impl CandlesOutput {
    pub(crate) fn from_kwarg(s: &str) -> PyResult<Self> {
        match s.to_ascii_lowercase().as_str() {
            "polars" => Ok(CandlesOutput::Polars),
            "pandas" => Ok(CandlesOutput::Pandas),
            "numpy" | "dict" => Ok(CandlesOutput::Numpy),
            other => Err(PyValueError::new_err(format!(
                "output must be 'polars', 'pandas', or 'numpy' (got {other:?})"
            ))),
        }
    }
}

// -- Interval token parser (accepts `1m`, `4h`, `1d`, `1w`, `1M`) -----------

pub(crate) fn parse_interval_token(s: &str) -> PyResult<Interval> {
    let s = s.trim();
    if s.is_empty() {
        return Err(PyValueError::new_err("interval token is empty"));
    }
    let (num, unit) = s.split_at(s.len() - 1);
    let n: u32 = if num.is_empty() {
        1
    } else {
        num.parse()
            .map_err(|_| PyValueError::new_err(format!("invalid interval {s:?}")))?
    };
    if n == 0 {
        return Err(PyValueError::new_err(format!(
            "interval {s:?}: multiplier must be positive"
        )));
    }
    match unit {
        "m" => Ok(Interval::Minute(n)),
        "h" => Ok(Interval::Hour(n)),
        "d" => Ok(Interval::Day(n)),
        "w" => Ok(Interval::Week(n)),
        "M" => Ok(Interval::Month(n)),
        _ => Err(PyValueError::new_err(format!(
            "interval {s:?}: unknown unit {unit:?}"
        ))),
    }
}

// -- Date parser (`today` / `yesterday` / `Nd ago` / ISO / EU) --------------

pub(crate) fn parse_date_token(input: &str, now: time::OffsetDateTime) -> PyResult<time::OffsetDateTime> {
    let raw = input.trim();
    let lower = raw.to_ascii_lowercase();
    if lower == "today" {
        return Ok(midnight_utc(now.date()));
    }
    if lower == "yesterday" {
        return Ok(midnight_utc(now.date() - time::Duration::days(1)));
    }
    if let Some((n, unit)) = parse_relative(&lower) {
        let d = match unit {
            'd' => time::Duration::days(n as i64),
            'w' => time::Duration::weeks(n as i64),
            _ => unreachable!(),
        };
        return Ok(midnight_utc(now.date() - d));
    }
    if let Some(date) = parse_absolute(raw) {
        return Ok(midnight_utc(date));
    }
    Err(PyValueError::new_err(format!("invalid date {input:?}")))
}

pub(crate) fn midnight_utc(date: time::Date) -> time::OffsetDateTime {
    date.with_time(time::Time::MIDNIGHT).assume_utc()
}

pub(crate) fn parse_relative(s: &str) -> Option<(u32, char)> {
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

pub(crate) fn parse_absolute(s: &str) -> Option<time::Date> {
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
    let month = time::Month::try_from(u8::try_from(month).ok()?).ok()?;
    time::Date::from_calendar_date(year, month, u8::try_from(day).ok()?).ok()
}

pub(crate) fn resolve_since_until(
    since: &str,
    until: Option<&str>,
) -> PyResult<(Timestamp, Option<Timestamp>)> {
    let now = time::OffsetDateTime::now_utc();
    let since_dt = parse_date_token(since, now)?;
    let until_dt = match until {
        Some(u) => Some(parse_date_token(u, now)?),
        None => None,
    };
    if let Some(u) = until_dt
        && u <= since_dt
    {
        return Err(PyValueError::new_err(format!(
            "until ({}) must be strictly after since ({})",
            until.unwrap_or(""),
            since
        )));
    }
    Ok((
        Timestamp::from_datetime(since_dt),
        until_dt.map(Timestamp::from_datetime),
    ))
}

/// Format a UTC millisecond stamp as `YYYY-MM-DDTHH:MM:SSZ`.
pub(crate) fn format_ts_iso(ms: i64) -> String {
    let nanos = (ms as i128).saturating_mul(1_000_000);
    match time::OffsetDateTime::from_unix_timestamp_nanos(nanos) {
        Ok(dt) => dt
            .format(&time::format_description::well_known::Rfc3339)
            .unwrap_or_else(|_| ms.to_string()),
        Err(_) => ms.to_string(),
    }
}

/// Materialise a single-symbol/single-interval fetch into a DataFrame.
///
/// Columns: `time` (ISO 8601 UTC str), `open`/`high`/`low`/`close`/`volume`
/// (f64), then one column per source-provided overlay (Binance's
/// `quote_volume` / `n_trades` / `taker_buy_base_volume` /
/// `taker_buy_quote_volume`; Yahoo's `raw_close`, or `adj_close` when the
/// client is built `adjusted=False`) — same names as the atom
/// schema's keys. Bool / Str overlay columns land as Python-native lists;
/// Real ones as `f64` lists.
/// Materialise a [`SeriesSource`] fetch into a DataFrame.
///
/// Columns: `time` (ISO 8601 UTC str), then the OHLCV block **only when at
/// least one atom carries a bar**, then one column per `schema` key. A provider
/// whose atoms are all price-less — an overlay series like CoinGecko's market
/// caps or Binance-Vision's funding — yields a frame with no `open`/`high`/
/// `low`/`close`/`volume`, meant to be joined onto a price frame on `time`.
/// This mirrors the CLI's single `get` pipeline, where `Atom::candle` is
/// optional and the writer omits the OHLCV block when no row has a bar.
pub(crate) fn build_series_frame(
    py: Python<'_>,
    output: CandlesOutput,
    schema: std::sync::Arc<fugazi_core::Schema>,
    atoms: Vec<Atom>,
) -> PyResult<Py<PyAny>> {
    let n = atoms.len();
    let has_bars = atoms.iter().any(|a| a.candle.is_some());
    let mut times: Vec<String> = Vec::with_capacity(n);
    let mut opens: Vec<f64> = Vec::with_capacity(n);
    let mut highs: Vec<f64> = Vec::with_capacity(n);
    let mut lows: Vec<f64> = Vec::with_capacity(n);
    let mut closes: Vec<f64> = Vec::with_capacity(n);
    let mut volumes: Vec<f64> = Vec::with_capacity(n);
    let mut overlays: Vec<Option<OverlayInfo>> = Vec::with_capacity(n);
    for atom in atoms {
        let time = atom
            .time
            .expect("series-source atoms always carry a time")
            .0;
        times.push(format_ts_iso(time));
        // A price-less atom leaves a NaN cell — what polars/pandas read as
        // missing, keeping the column the same length as the rest of the frame.
        if has_bars {
            let c = atom.candle;
            opens.push(c.map_or(Real::NAN, |c| c.open));
            highs.push(c.map_or(Real::NAN, |c| c.high));
            lows.push(c.map_or(Real::NAN, |c| c.low));
            closes.push(c.map_or(Real::NAN, |c| c.close));
            volumes.push(c.map_or(Real::NAN, |c| c.volume));
        }
        overlays.push(atom.overlays);
    }
    let data = PyDict::new(py);
    data.set_item("time", &times)?;
    if has_bars {
        data.set_item("open", &opens)?;
        data.set_item("high", &highs)?;
        data.set_item("low", &lows)?;
        data.set_item("close", &closes)?;
        data.set_item("volume", &volumes)?;
    }
    set_overlay_columns(&data, &schema, overlays)?;
    match output {
        CandlesOutput::Polars => {
            let polars = py.import("polars").map_err(|_| {
                PyValueError::new_err(
                    "output='polars' requested but the polars package is not installed",
                )
            })?;
            Ok(polars.getattr("DataFrame")?.call1((data,))?.unbind())
        }
        CandlesOutput::Pandas => {
            let pandas = py.import("pandas").map_err(|_| {
                PyValueError::new_err(
                    "output='pandas' requested but the pandas package is not installed",
                )
            })?;
            Ok(pandas.getattr("DataFrame")?.call1((data,))?.unbind())
        }
        CandlesOutput::Numpy => Ok(data.into_any().unbind()),
    }
}

/// Set one DataFrame column per [`Schema`] key, from a per-row overlay list.
///
/// Shared by the candle frame (a provider's OHLCV extras) and the overlay frame
/// (an overlay-only series's whole payload) — both are "a schema plus one
/// `OverlayInfo` per row", so the Real/Bool/Str dispatch lives here once.
///
/// A row missing a cell reads as `NaN` / `false` / `""` by column type, matching
/// how the CLI's CSV loader fills a missing cell.
pub(crate) fn set_overlay_columns(
    data: &Bound<'_, PyDict>,
    schema: &std::sync::Arc<fugazi_core::Schema>,
    rows: Vec<Option<OverlayInfo>>,
) -> PyResult<()> {
    let n = rows.len();
    let n_over = schema.len();
    let mut over_real: Vec<Vec<f64>> = (0..n_over).map(|_| Vec::with_capacity(n)).collect();
    let mut over_bool: Vec<Vec<bool>> = (0..n_over).map(|_| Vec::with_capacity(n)).collect();
    let mut over_str: Vec<Vec<String>> = (0..n_over).map(|_| Vec::with_capacity(n)).collect();
    for row in &rows {
        for i in 0..n_over {
            let cell = row.as_ref().and_then(|ov| ov.get(i));
            match schema.type_of(i).expect("schema has N columns") {
                fugazi_core::OverlayType::Real => over_real[i].push(match cell {
                    Some(OverlayValue::Real(x)) => *x,
                    _ => f64::NAN,
                }),
                fugazi_core::OverlayType::Bool => over_bool[i].push(match cell {
                    Some(OverlayValue::Bool(b)) => *b,
                    _ => false,
                }),
                fugazi_core::OverlayType::Str => over_str[i].push(match cell {
                    Some(OverlayValue::Str(s)) => s.to_string(),
                    _ => String::new(),
                }),
            }
        }
    }
    for (i, name) in schema.keys().enumerate() {
        match schema.type_of(i).expect("schema has N columns") {
            fugazi_core::OverlayType::Real => data.set_item(name, &over_real[i])?,
            fugazi_core::OverlayType::Bool => data.set_item(name, &over_bool[i])?,
            fugazi_core::OverlayType::Str => data.set_item(name, &over_str[i])?,
        }
    }
    Ok(())
}

/// Fetch one `(symbol, interval)` window from any [`SeriesSource`] and build
/// its DataFrame. The provider's fixed [`schema`](SeriesSource::schema) wins
/// when it has one (overlay providers know their columns before the fetch);
/// otherwise the schema is picked off the returned atoms with [`schema_of`].
pub(crate) fn fetch_frame<C>(
    py: Python<'_>,
    source: &C,
    output: CandlesOutput,
    symbol: &str,
    interval: Interval,
    since: Timestamp,
    until: Option<Timestamp>,
) -> PyResult<Py<PyAny>>
where
    C: SeriesSource + Clone,
{
    let atoms = fetch_bars(py, source, symbol, interval, since, until)?;
    let schema = source
        .schema()
        .unwrap_or_else(|| fugazi_core::sources::schema_of(&atoms));
    build_series_frame(py, output, schema, atoms)
}

/// Fetch a single (symbol, interval) window through the shared runtime,
/// releasing the GIL for the network I/O.
pub(crate) fn fetch_bars<C>(
    py: Python<'_>,
    source: &C,
    symbol: &str,
    interval: Interval,
    since: Timestamp,
    until: Option<Timestamp>,
) -> PyResult<Vec<Atom>>
where
    C: SeriesSource + Clone,
{
    let client = source.clone();
    let symbol = symbol.to_string();
    py.detach(|| {
        sources_runtime()
            .block_on(async move { client.atoms(&symbol, interval, since, until).await })
    })
    .map_err(source_error_to_py)
}

/// A Binance klines client.
///
/// ```python
/// b = fugazi.Binance()                  # public endpoint, defaults
/// df = b.fetch(symbol="BTCUSDT", freq="1d",
///              since="2020-01-01", until="today")
/// ```
///
/// One call = one (symbol, freq) fetch = one DataFrame. Batch multiple
/// symbols or frequencies by looping in Python.
#[pyclass(name = "Binance", frozen)]
pub(crate) struct PyBinance {
    pub(crate) inner: Binance,
}

#[pymethods]
impl PyBinance {
    /// Construct a client. `base_url` overrides the API endpoint (default
    /// `https://api.binance.com`), useful for local test servers.
    #[new]
    #[pyo3(signature = (base_url = None))]
    pub(crate) fn new(base_url: Option<String>) -> Self {
        let mut inner = Binance::new();
        if let Some(url) = base_url {
            inner = inner.with_base_url(url);
        }
        Self { inner }
    }

    /// Fetch OHLCV candles for one `(symbol, freq)` window.
    ///
    /// * `symbol` — e.g. `"BTCUSDT"`, `"ETHEUR"`. Sent verbatim to Binance.
    /// * `freq` — bar cadence: `"1m"`/`"5m"`/`"1h"`/`"4h"`/`"1d"`/`"1w"`/`"1M"`.
    /// * `since` / `until` — dates. Formats: ISO `"YYYY-MM-DD"`, EU
    ///   `"D-M-YYYY"`, or relative (`"today"`, `"yesterday"`, `"Nd ago"`,
    ///   `"Nw ago"`). `until` is exclusive; `None` means "up to now".
    /// * `output` — `"polars"` (default), `"pandas"`, or `"numpy"` (dict of arrays).
    ///
    /// Returned DataFrame columns: `time` (ISO 8601 UTC), `open`, `high`,
    /// `low`, `close`, `volume`, plus the Binance kline extras
    /// `quote_volume`, `n_trades`, `taker_buy_base_volume`,
    /// `taker_buy_quote_volume` (all f64).
    #[pyo3(signature = (symbol, freq = "1d", since = "2020-01-01", until = None, output = "polars"))]
    pub(crate) fn fetch(
        &self,
        py: Python<'_>,
        symbol: &str,
        freq: &str,
        since: &str,
        until: Option<&str>,
        output: &str,
    ) -> PyResult<Py<PyAny>> {
        let interval = parse_interval_token(freq)?;
        let (since_ts, until_ts) = resolve_since_until(since, until)?;
        let out = CandlesOutput::from_kwarg(output)?;
        fetch_frame(py, &self.inner, out, symbol, interval, since_ts, until_ts)
    }
}

/// A Yahoo Finance chart-API client (stocks, ETFs, indices, FX).
///
/// ```python
/// y = fugazi.Yahoo()                     # public endpoint, defaults
/// df = y.fetch(symbol="AAPL", freq="1d",
///              since="2020-01-01", until="today")
/// ```
///
/// One call = one (symbol, freq) fetch = one DataFrame. Batch multiple
/// symbols or frequencies by looping in Python.
#[pyclass(name = "Yahoo", frozen)]
pub(crate) struct PyYahoo {
    pub(crate) inner: Yahoo,
}

#[pymethods]
impl PyYahoo {
    /// Construct a client. `adjusted` (default `True`) back-adjusts each candle
    /// for splits and dividends at fetch time — every price is rescaled by its
    /// `adj_close / close` factor so `close` is the adjusted price, and the raw
    /// close is preserved as a `raw_close` column. Set it `False` to get the
    /// untouched prints with `adj_close` as the extra column instead. `base_url`
    /// overrides the API endpoint (default `https://query1.finance.yahoo.com`),
    /// useful for local test servers; `user_agent` overrides the default
    /// `User-Agent` header Yahoo's chart endpoint requires.
    #[new]
    #[pyo3(signature = (adjusted = true, base_url = None, user_agent = None))]
    pub(crate) fn new(adjusted: bool, base_url: Option<String>, user_agent: Option<String>) -> Self {
        let mut inner = Yahoo::new().with_adjusted(adjusted);
        if let Some(url) = base_url {
            inner = inner.with_base_url(url);
        }
        if let Some(ua) = user_agent {
            inner = inner.with_user_agent(ua);
        }
        Self { inner }
    }

    /// Fetch OHLCV candles for one `(symbol, freq)` window.
    ///
    /// * `symbol` — e.g. `"AAPL"`, `"^GSPC"`, `"EURUSD=X"`. Sent verbatim to Yahoo.
    /// * `freq` — bar cadence: `"1m"`/`"5m"`/`"1h"`/`"4h"`/`"1d"`/`"1w"`/`"1M"`.
    /// * `since` / `until` — dates. Formats: ISO `"YYYY-MM-DD"`, EU
    ///   `"D-M-YYYY"`, or relative (`"today"`, `"yesterday"`, `"Nd ago"`,
    ///   `"Nw ago"`). `until` is exclusive; `None` means "up to now".
    /// * `output` — `"polars"` (default), `"pandas"`, or `"numpy"` (dict of arrays).
    ///
    /// Returned DataFrame columns: `time` (ISO 8601 UTC), `open`, `high`,
    /// `low`, `close`, `volume`, plus one Yahoo extra (all f64). With the
    /// default `adjusted=True` the OHLCV are split/dividend-adjusted and the
    /// extra is `raw_close` (the untouched close); with `adjusted=False` the
    /// OHLCV are raw and the extra is `adj_close` (the adjusted close).
    #[pyo3(signature = (symbol, freq = "1d", since = "2020-01-01", until = None, output = "polars"))]
    pub(crate) fn fetch(
        &self,
        py: Python<'_>,
        symbol: &str,
        freq: &str,
        since: &str,
        until: Option<&str>,
        output: &str,
    ) -> PyResult<Py<PyAny>> {
        let interval = parse_interval_token(freq)?;
        let (since_ts, until_ts) = resolve_since_until(since, until)?;
        let out = CandlesOutput::from_kwarg(output)?;
        fetch_frame(py, &self.inner, out, symbol, interval, since_ts, until_ts)
    }
}

/// A CoinGecko client — market-cap / volume / supply columns, **no OHLCV**.
///
/// ```python
/// cg = fugazi.CoinGecko()                        # public endpoint, defaults
/// df = cg.fetch(symbol="bitcoin", freq="1d",
///               since="30d ago", until="today")
/// ```
///
/// Every provider fetches through the same `.fetch(...)` method, but unlike
/// `Binance` / `Yahoo` this one carries no price: it returns data that is a
/// property of an asset at a point in time rather than a price bar,
/// so the frame has `time` plus `price`, `market_cap`, `total_volume` and
/// `circulating_supply` — and no `open`/`high`/`low`/`close`. Join it onto a
/// price frame on `time` to use both.
///
/// `symbol` is a CoinGecko **coin id** (`"bitcoin"`, `"ethereum"`), not a
/// ticker and not an exchange pair.
///
/// Two limits of the public tier worth knowing: it serves only the **last 365
/// days** (a wider `since` raises `ValueError`), and CoinGecko picks the
/// sampling granularity from the window length, so sub-hourly `freq` values are
/// rejected. Set `COINGECKO_API_KEY` (or pass `api_key=`) for a demo key.
#[pyclass(name = "CoinGecko", frozen)]
pub(crate) struct PyCoinGecko {
    pub(crate) inner: CoinGecko,
}

#[pymethods]
impl PyCoinGecko {
    /// Construct a client. `api_key` is a CoinGecko demo key (defaults to the
    /// `COINGECKO_API_KEY` environment variable); `vs_currency` is the quote
    /// currency (default `"usd"`); `base_url` overrides the API endpoint,
    /// useful for local test servers; `user_agent` overrides the descriptive
    /// `User-Agent` CoinGecko requires (it rejects requests without one).
    #[new]
    #[pyo3(signature = (api_key = None, vs_currency = None, base_url = None, user_agent = None))]
    pub(crate) fn new(
        api_key: Option<String>,
        vs_currency: Option<String>,
        base_url: Option<String>,
        user_agent: Option<String>,
    ) -> Self {
        let mut inner = CoinGecko::new();
        if let Some(key) = api_key {
            inner = inner.with_api_key(key);
        }
        if let Some(cur) = vs_currency {
            inner = inner.with_vs_currency(cur);
        }
        if let Some(url) = base_url {
            inner = inner.with_base_url(url);
        }
        if let Some(ua) = user_agent {
            inner = inner.with_user_agent(ua);
        }
        Self { inner }
    }

    /// Fetch overlay columns for one `(symbol, freq)` window.
    ///
    /// * `symbol` — a CoinGecko coin id: `"bitcoin"`, `"ethereum"`, `"solana"`.
    ///   Use `.ids()` for the full vocabulary.
    /// * `freq` — bar cadence. `"1h"`/`"4h"`/`"1d"`/`"1w"`/`"1M"`; sub-hourly is
    ///   rejected (CoinGecko only samples that finely over windows too short to
    ///   backtest on).
    /// * `since` / `until` — dates, same grammar as the candle providers.
    ///   `until` is exclusive; `None` means "up to now".
    /// * `output` — `"polars"` (default), `"pandas"`, or `"numpy"` (dict of arrays).
    ///
    /// Returned DataFrame columns: `time` (ISO 8601 UTC), `price`,
    /// `market_cap`, `total_volume`, `circulating_supply` (all f64). The last is
    /// derived as `market_cap / price`, and is `NaN` on any bar where either
    /// input is missing. **No OHLCV columns** — see the class docs.
    #[pyo3(signature = (symbol, freq = "1d", since = "2020-01-01", until = None, output = "polars"))]
    pub(crate) fn fetch(
        &self,
        py: Python<'_>,
        symbol: &str,
        freq: &str,
        since: &str,
        until: Option<&str>,
        output: &str,
    ) -> PyResult<Py<PyAny>> {
        let interval = parse_interval_token(freq)?;
        let (since_ts, until_ts) = resolve_since_until(since, until)?;
        let out = CandlesOutput::from_kwarg(output)?;
        fetch_frame(py, &self.inner, out, symbol, interval, since_ts, until_ts)
    }

    /// Every coin id CoinGecko exposes, sorted — the vocabulary `symbol` accepts.
    pub(crate) fn ids(&self, py: Python<'_>) -> PyResult<Vec<String>> {
        let client = self.inner.clone();
        py.detach(|| sources_runtime().block_on(async move { client.tickers().await }))
            .map_err(source_error_to_py)
    }
}

/// Binance perpetual **funding rate** — an overlay provider, not a candle one.
///
/// The funding rate is the periodic payment between the two sides of a
/// perpetual swap (positive = longs pay shorts), the primary carry signal in
/// crypto. It is not a price, so the frame has `time` plus `funding_rate` and
/// no `open`/`high`/`low`/`close`. Join it onto a price frame on `time` to use
/// both.
///
/// `symbol` is a **perpetual contract** symbol (`"BTCUSDT"`), served from
/// `fapi.binance.com` — a different host and a different listing set from the
/// spot vocabulary `Binance` uses. `.symbols()` enumerates it.
///
/// Binance settles funding every 4–8 hours. Those are events, not bars, so a
/// coarser `freq` covers several of them and **their rates are summed**:
/// `freq="1d"` gives that day's total carry, which is the number a daily-bar
/// strategy wants. At `freq="8h"` each bucket holds one settlement. Sub-hourly
/// `freq` values are rejected — they would be empty on almost every bar, which
/// reads as "no carry" rather than "no data".
#[pyclass(name = "BinanceVision", frozen)]
pub(crate) struct PyBinanceVision {
    pub(crate) inner: BinanceVision,
}

#[pymethods]
impl PyBinanceVision {
    /// Construct a client. `market` picks the archive tree — `"spot"` (the
    /// default) or `"futures"`, which adds the funding rate, premium index and
    /// positioning columns a derivative has and a cash market does not.
    /// `base_url` overrides the archive host, useful for local test servers.
    #[new]
    #[pyo3(signature = (market = "spot", base_url = None))]
    pub(crate) fn new(market: &str, base_url: Option<String>) -> PyResult<Self> {
        let market = match market.to_ascii_lowercase().as_str() {
            "spot" => fugazi_core::sources::binance_vision::Market::Spot,
            "futures" | "um" => fugazi_core::sources::binance_vision::Market::UsdMFutures,
            other => {
                return Err(PyValueError::new_err(format!(
                    "market must be 'spot' or 'futures' (got {other:?})"
                )));
            }
        };
        let mut inner = BinanceVision::for_market(market);
        if let Some(url) = base_url {
            inner = inner.with_base_url(url);
        }
        Ok(Self { inner })
    }

    /// Fetch the funding-rate column for one `(symbol, freq)` window.
    ///
    /// * `symbol` — a perpetual contract symbol: `"BTCUSDT"`, `"ETHUSDT"`.
    /// * `freq` — bar cadence, hourly or coarser (`"8h"`, `"1d"`, `"1w"`,
    ///   `"1M"`). Settlements inside a bar are summed.
    /// * `since` / `until` — dates, same grammar as the candle providers.
    ///   `until` is exclusive; `None` means "up to now".
    /// * `output` — `"polars"` (default), `"pandas"`, or `"numpy"`.
    ///
    /// Returned columns: `time` (ISO 8601 UTC) and `funding_rate` (f64).
    /// **No OHLCV columns** — see the class docs.
    #[pyo3(signature = (symbol, freq = "1d", since = "2020-01-01", until = None, output = "polars"))]
    pub(crate) fn fetch(
        &self,
        py: Python<'_>,
        symbol: &str,
        freq: &str,
        since: &str,
        until: Option<&str>,
        output: &str,
    ) -> PyResult<Py<PyAny>> {
        let interval = parse_interval_token(freq)?;
        let (since_ts, until_ts) = resolve_since_until(since, until)?;
        let out = CandlesOutput::from_kwarg(output)?;
        fetch_frame(py, &self.inner, out, symbol, interval, since_ts, until_ts)
    }

    /// Every perpetual contract symbol currently trading, sorted.
    pub(crate) fn symbols(&self, py: Python<'_>) -> PyResult<Vec<String>> {
        let client = self.inner.clone();
        py.detach(|| {
            sources_runtime().block_on(async move { SeriesSource::tickers(&client).await })
        })
        .map_err(source_error_to_py)
    }
}

/// Fetch a series from a named provider and return a DataFrame.
///
/// ```python
/// df = fugazi.fetch(provider="binance", symbol="BTCUSDT", freq="1d",
///                   since="2020-01-01", until="today", output="polars")
/// ```
///
/// Same shape as `Binance().fetch(...)` / `Yahoo().fetch(...)`; the extra
/// `provider` argument dispatches to the right client. Every provider fetches
/// the same way now that they share one `SeriesSource` trait — a candle
/// provider yields an OHLCV frame, an overlay provider (`"cg"`) yields a
/// price-less one (`time` + its own columns), and the frame builder omits the
/// OHLCV block when no row carries a bar.
///
/// Providers: `"binance"`, `"yfinance"`, `"cg"` (CoinGecko). `BinanceVision`
/// needs a `market` (`"spot"`/`"futures"`) that this flat signature can't
/// carry — construct it explicitly (`BinanceVision(market=...).fetch(...)`).
#[pyfunction]
#[pyo3(signature = (provider, symbol, freq = "1d", since = "2020-01-01", until = None, output = "polars"))]
pub(crate) fn fetch(
    py: Python<'_>,
    provider: &str,
    symbol: &str,
    freq: &str,
    since: &str,
    until: Option<&str>,
    output: &str,
) -> PyResult<Py<PyAny>> {
    let interval = parse_interval_token(freq)?;
    let (since_ts, until_ts) = resolve_since_until(since, until)?;
    let out = CandlesOutput::from_kwarg(output)?;
    match provider {
        "binance" => fetch_frame(py, &Binance::new(), out, symbol, interval, since_ts, until_ts),
        "yfinance" => fetch_frame(py, &Yahoo::new(), out, symbol, interval, since_ts, until_ts),
        "cg" => fetch_frame(py, &CoinGecko::new(), out, symbol, interval, since_ts, until_ts),
        "binance-vision" => Err(PyValueError::new_err(
            "binance-vision needs a market ('spot' or 'futures') that fetch()'s flat signature \
             can't carry. Construct it explicitly: BinanceVision(market='futures').fetch(...).",
        )),
        other => Err(PyValueError::new_err(format!(
            "unknown provider {other:?}. Known providers: binance, yfinance, cg"
        ))),
    }
}

