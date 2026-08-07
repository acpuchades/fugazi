//! Binance Vision — the public historical archive — as an [`OverlaySource`].
//!
//! Binance publishes its own market data as dated ZIP-of-CSV files at
//! `data.binance.vision` rather than behind a query API: one archive per symbol
//! per month, addressed by path. For a multi-year range that is both cheaper and
//! deeper than the REST endpoints — one request per month instead of one per
//! thousand rows, and no rate limit — and it is the only public source with real
//! history for the perpetual statistics `fapi`'s `/futures/data/*` endpoints cap
//! at the last 30 days.
//!
//! # This is history, not a live feed
//!
//! An archive appears about two days after the period it covers: today's and
//! yesterday's files do not exist yet. A fetch that runs to `now` therefore
//! stops at the last published month rather than at the current bar, and a
//! missing archive is treated as "no data for that period", not as an error —
//! which is also what a month before the contract was listed looks like.
//!
//! # Columns
//!
//! | column | archive | cadence |
//! |---|---|---|
//! | `funding_rate` | `monthly/fundingRate` | settlement events, every 4–8h |
//! | `premium_index` | `monthly/premiumIndexKlines` | the requested interval |
//! | `open_interest` | `daily/metrics` | 5 min |
//! | `open_interest_value` | `daily/metrics` | 5 min |
//! | `long_short_ratio` | `daily/metrics` | 5 min |
//! | `top_trader_account_ratio` | `daily/metrics` | 5 min |
//! | `top_trader_position_ratio` | `daily/metrics` | 5 min |
//! | `taker_long_short_ratio` | `daily/metrics` | 5 min |
//!
//! They aggregate differently inside a bar, because they are different kinds of
//! quantity. **Funding is summed**: it is a cost that accrues, so the three
//! 8-hourly settlements inside a day add up to that day's total carry.
//! **Everything else is a level** — the premium index is the basis
//! `(mark - index) / index`, open interest is a stock, the ratios are
//! proportions — so a bar keeps the last sample it saw, the number that was
//! true when the bar ended.
//!
//! A bar may carry some columns and not others: at `[1h]` only every eighth bar
//! sees a funding settlement, and `metrics` begins years after `fundingRate`
//! does. An absent column reads as an absent sample rather than as a zero,
//! which for an accrual is the difference between "no carry recorded" and
//! "carry was nil".
//!
//! ```sh
//! fugazi get binance-vision:BTCUSDT[1d] --since 2022-01-01 -o carry.csv
//! fugazi run @strategy.yml -s @btc.csv -s @carry.csv
//! ```
//!
//! ```yaml
//! # in the strategy — an 8-day trailing mean of the carry
//! enter: !above
//!   source: !sma { source: !get { key: funding_rate }, period: 8 }
//!   level: 0.0
//! ```

use std::collections::BTreeMap;
use std::future::Future;
use std::io::Read;
use std::sync::{Arc, OnceLock};
use std::time::Duration;

use serde::Deserialize;

use crate::types::{OverlayInfo, OverlayValue, Real, Schema};

use super::{Interval, OverlayRow, OverlaySource, SourceError, Timestamp, floor_to_bucket};

/// The archive host. Static files, no API key, no rate limit.
const DEFAULT_BASE_URL: &str = "https://data.binance.vision";
/// Where the ticker list comes from — the archive has no index endpoint, so the
/// symbol vocabulary is read from the live exchange instead. See
/// [`BinanceVision::tickers`].
const EXCHANGE_INFO_URL: &str = "https://fapi.binance.com/fapi/v1/exchangeInfo";
const DEFAULT_MIN_DELAY_MS: u64 = 20;
/// Archive requests outstanding at once, per series. Chosen to hide
/// round-trip latency on a multi-thousand-file `metrics` range without
/// hammering the host once `fugazi get`'s per-series tasks multiply it.
const DEFAULT_MAX_IN_FLIGHT: usize = 8;

/// The overlay columns this provider exposes. Read them from a strategy or an
/// `--overlay` spec with `!get { key: funding_rate }` /
/// `!get { key: premium_index }`.
/// The column order here is the contract [`Archive::columns`] indexes into;
/// the two must be changed together.
pub fn binance_vision_schema() -> &'static Arc<Schema> {
    static SCHEMA: OnceLock<Arc<Schema>> = OnceLock::new();
    SCHEMA.get_or_init(|| {
        let mut b = Schema::builder();
        b.add_real("funding_rate"); // 0
        b.add_real("premium_index"); // 1
        b.add_real("open_interest"); // 2 — contracts
        b.add_real("open_interest_value"); // 3 — quote currency
        b.add_real("long_short_ratio"); // 4 — all accounts
        b.add_real("top_trader_account_ratio"); // 5 — top accounts, by count
        b.add_real("top_trader_position_ratio"); // 6 — top accounts, by size
        b.add_real("taker_long_short_ratio"); // 7 — taker buy vs sell volume
        b.finish()
    })
}

/// A Binance Vision archive client.
///
/// Cheap to clone (the inner [`reqwest::Client`] is `Arc`-backed).
///
/// The `symbol` is a **perpetual contract** symbol (`BTCUSDT`, `ETHUSDT`) —
/// which mostly coincides with the spot vocabulary but is not the same list.
/// Enumerate it with [`OverlaySource::tickers`]
/// (`fugazi list tickers binance-vision`).
#[derive(Debug, Clone)]
pub struct BinanceVision {
    client: reqwest::Client,
    base_url: String,
    min_delay_between_requests: Duration,
    max_in_flight: usize,
}

impl Default for BinanceVision {
    fn default() -> Self {
        Self::new()
    }
}

impl BinanceVision {
    /// A client pointing at the public archive.
    pub fn new() -> Self {
        Self {
            client: reqwest::Client::new(),
            base_url: DEFAULT_BASE_URL.to_string(),
            min_delay_between_requests: Duration::from_millis(DEFAULT_MIN_DELAY_MS),
            max_in_flight: DEFAULT_MAX_IN_FLIGHT,
        }
    }

    /// How many archive requests may be outstanding at once. The archives are
    /// static objects with no published rate limit, but `fugazi get` already
    /// runs one task per series, so the real concurrency is this times the
    /// number of series — keep it modest.
    pub fn with_max_in_flight(mut self, n: usize) -> Self {
        self.max_in_flight = n.max(1);
        self
    }

    /// Override the archive base URL. Primarily useful for testing against a
    /// local mock server.
    pub fn with_base_url(mut self, url: impl Into<String>) -> Self {
        self.base_url = url.into();
        self
    }

    /// Minimum delay between archive requests.
    pub fn with_min_delay(mut self, d: Duration) -> Self {
        self.min_delay_between_requests = d;
        self
    }
}

impl OverlaySource for BinanceVision {
    fn name(&self) -> &'static str {
        "binance-vision"
    }

    fn schema(&self) -> Arc<Schema> {
        binance_vision_schema().clone()
    }

    /// The archive is a plain file tree with no index endpoint, so the symbol
    /// list comes from the live exchange's `exchangeInfo` instead — the same
    /// vocabulary the archive is keyed by. A symbol listed here may still have
    /// no archive for an early month; that reads as no data.
    fn tickers(&self) -> impl Future<Output = Result<Vec<String>, SourceError>> + Send {
        let client = self.client.clone();
        async move {
            let resp = client.get(EXCHANGE_INFO_URL).send().await?;
            if !resp.status().is_success() {
                return Err(map_http_error(resp).await);
            }
            let info: ExchangeInfo = resp
                .json()
                .await
                .map_err(|e| SourceError::Decode(format!("exchangeInfo JSON: {e}")))?;
            let mut out: Vec<String> = info
                .symbols
                .into_iter()
                .filter(|s| s.status == "TRADING" && s.contract_type == "PERPETUAL")
                .map(|s| s.symbol)
                .collect();
            out.sort();
            Ok(out)
        }
    }

    fn overlays(
        &self,
        symbol: &str,
        interval: Interval,
        since: Timestamp,
        until: Option<Timestamp>,
    ) -> impl Future<Output = Result<Vec<OverlayRow>, SourceError>> + Send {
        // Own the strings so the returned future doesn't borrow the caller.
        let symbol = symbol.to_string();
        let client = self.client.clone();
        let base_url = self.base_url.clone();
        let min_delay = self.min_delay_between_requests;
        let max_in_flight = self.max_in_flight;
        async move {
            let token = interval_token(interval)?;
            let schema = binance_vision_schema().clone();
            let until_ms = until.map(|t| t.0).unwrap_or_else(|| Timestamp::now().0);
            let base = base_url.trim_end_matches('/').to_string();

            // Every archive this range touches, across all three trees. The
            // list is built up front so the fetches can run concurrently:
            // these are independent static objects, so the unit of parallelism
            // is the file, not a time chunk of one series.
            let mut jobs: Vec<(Archive, String)> = Vec::new();
            for kind in Archive::ALL {
                for stamp in kind.periods(since.0, until_ms) {
                    jobs.push((kind, kind.url(&base, &symbol, token, &stamp)));
                }
            }

            let fetched = fetch_concurrently(&client, jobs, max_in_flight, min_delay).await?;

            // One `bucket -> value` map per schema column.
            let mut columns: Vec<BTreeMap<i64, Real>> = vec![BTreeMap::new(); schema.len()];
            for (kind, url, csv) in fetched {
                for (time, slot, value) in parse_archive(kind, &csv, &url)? {
                    if time < since.0 || time >= until_ms {
                        continue;
                    }
                    let bucket = floor_to_bucket(time, interval);
                    match kind.aggregation() {
                        // An accrual: samples inside one bar add up.
                        Aggregation::Sum => *columns[slot].entry(bucket).or_insert(0.0) += value,
                        // A level: the bar keeps the last value it saw.
                        Aggregation::Last => {
                            columns[slot].insert(bucket, value);
                        }
                    }
                }
            }

            let mut buckets: Vec<i64> = columns.iter().flat_map(|c| c.keys().copied()).collect();
            buckets.sort_unstable();
            buckets.dedup();

            Ok(buckets
                .into_iter()
                .map(|time| OverlayRow {
                    time: Timestamp(time),
                    overlays: OverlayInfo::sparse(
                        schema.clone(),
                        columns
                            .iter()
                            .map(|c| c.get(&time).copied().map(OverlayValue::Real)),
                    ),
                })
                .collect())
        }
    }
}

/// How a column's samples collapse into one bar.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Aggregation {
    /// Add them up — for a quantity that accrues over the bar.
    Sum,
    /// Keep the last — for a quantity that is a level at a point in time.
    Last,
}

/// Which archive tree a request targets. The three differ in path shape, in
/// period length, in how their timestamps are spelled, and in how their samples
/// collapse into a bar.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Archive {
    /// Monthly. Settlement events, so there is one series whatever cadence it
    /// is later bucketed into — the path carries no interval.
    Funding,
    /// Monthly, partitioned by interval.
    Premium,
    /// **Daily**, and the reason this source exists: `fapi`'s
    /// `/futures/data/*` endpoints serve these statistics for the last 30 days
    /// only, while the archive keeps them from 2021. The cost is one file per
    /// day rather than per month — five years is ~1800 requests per symbol,
    /// which is why the fetches run concurrently.
    Metrics,
}

impl Archive {
    const ALL: [Archive; 3] = [Archive::Funding, Archive::Premium, Archive::Metrics];

    fn url(self, base: &str, symbol: &str, token: &str, stamp: &str) -> String {
        match self {
            Archive::Funding => format!(
                "{base}/data/futures/um/monthly/fundingRate/{symbol}/{symbol}-fundingRate-{stamp}.zip"
            ),
            Archive::Premium => format!(
                "{base}/data/futures/um/monthly/premiumIndexKlines/{symbol}/{token}/{symbol}-{token}-{stamp}.zip"
            ),
            Archive::Metrics => format!(
                "{base}/data/futures/um/daily/metrics/{symbol}/{symbol}-metrics-{stamp}.zip"
            ),
        }
    }

    /// The period stamps this archive needs to cover `[since, until)` — months
    /// for the monthly trees, days for `metrics`.
    fn periods(self, since: i64, until: i64) -> Vec<String> {
        match self {
            Archive::Metrics => days_between(since, until),
            _ => months_between(since, until)
                .into_iter()
                .map(|(y, m)| format!("{y:04}-{m:02}"))
                .collect(),
        }
    }

    /// The CSV column holding each row's timestamp.
    fn time_column(self) -> &'static str {
        match self {
            Archive::Funding => "calc_time",
            Archive::Premium => "open_time",
            Archive::Metrics => "create_time",
        }
    }

    /// Parse this archive's timestamp spelling into epoch milliseconds. The
    /// monthly trees carry epoch millis; `metrics` carries a UTC
    /// `YYYY-MM-DD HH:MM:SS` datetime instead.
    fn parse_time(self, raw: &str) -> Result<i64, String> {
        match self {
            Archive::Metrics => {
                let fmt = time::macros::format_description!(
                    "[year]-[month]-[day] [hour]:[minute]:[second]"
                );
                time::PrimitiveDateTime::parse(raw, &fmt)
                    .map(|dt| Timestamp::from_datetime(dt.assume_utc()).0)
                    .map_err(|e| e.to_string())
            }
            _ => raw.parse::<i64>().map_err(|e| e.to_string()),
        }
    }

    /// The value columns this archive contributes, as
    /// `(CSV header, index into the provider schema)`.
    fn columns(self) -> &'static [(&'static str, usize)] {
        match self {
            Archive::Funding => &[("last_funding_rate", 0)],
            Archive::Premium => &[("close", 1)],
            Archive::Metrics => &[
                ("sum_open_interest", 2),
                ("sum_open_interest_value", 3),
                ("count_long_short_ratio", 4),
                ("count_toptrader_long_short_ratio", 5),
                ("sum_toptrader_long_short_ratio", 6),
                ("sum_taker_long_short_vol_ratio", 7),
            ],
        }
    }

    /// Funding is the only accrual here. Open interest is a stock and every
    /// ratio is a level, so all of them keep the bar's last sample.
    fn aggregation(self) -> Aggregation {
        match self {
            Archive::Funding => Aggregation::Sum,
            _ => Aggregation::Last,
        }
    }
}

/// Fetch one archive and return its single CSV entry as text. `Ok(None)` for a
/// 404: an archive that does not exist is a period with no data, which is the
/// normal shape of both the pre-listing past and the not-yet-published present.
async fn fetch_archive(
    client: &reqwest::Client,
    url: &str,
) -> Result<Option<String>, SourceError> {
    let resp = client.get(url).send().await?;
    if resp.status() == reqwest::StatusCode::NOT_FOUND {
        return Ok(None);
    }
    if !resp.status().is_success() {
        return Err(map_http_error(resp).await);
    }
    let bytes = resp.bytes().await?;
    unzip_single(&bytes).map(Some).map_err(|e| {
        SourceError::Decode(format!("{url}: {e}"))
    })
}

/// Read the one CSV entry out of an in-memory ZIP. Binance ships exactly one
/// file per archive, named after the archive itself.
fn unzip_single(bytes: &[u8]) -> Result<String, String> {
    let mut zip = zip::ZipArchive::new(std::io::Cursor::new(bytes))
        .map_err(|e| format!("not a readable zip: {e}"))?;
    if zip.is_empty() {
        return Err("zip archive is empty".to_string());
    }
    let mut entry = zip
        .by_index(0)
        .map_err(|e| format!("reading zip entry: {e}"))?;
    let mut out = String::new();
    entry
        .read_to_string(&mut out)
        .map_err(|e| format!("decompressing zip entry: {e}"))?;
    Ok(out)
}

/// Fetch every job with at most `max_in_flight` requests outstanding.
///
/// The archives are independent static objects with no rate limit, so the unit
/// of parallelism is the file: a `metrics` range of several years is thousands
/// of small requests, and running them one behind the other would be dominated
/// by round-trip latency. `min_delay` paces the *launches*, not the requests,
/// so a burst does not all leave at once.
///
/// Results come back unordered — every caller merges them into bucket-keyed
/// maps, so order carries no information. One failed fetch abandons the rest:
/// a partial series would be indistinguishable from a genuinely sparse one.
async fn fetch_concurrently(
    client: &reqwest::Client,
    jobs: Vec<(Archive, String)>,
    max_in_flight: usize,
    min_delay: Duration,
) -> Result<Vec<(Archive, String, String)>, SourceError> {
    let mut pending = jobs.into_iter();
    let mut set: tokio::task::JoinSet<(Archive, String, Result<Option<String>, SourceError>)> =
        tokio::task::JoinSet::new();
    let mut out = Vec::new();

    let mut spawn_next = |set: &mut tokio::task::JoinSet<_>| -> bool {
        let Some((kind, url)) = pending.next() else {
            return false;
        };
        let client = client.clone();
        set.spawn(async move {
            let got = fetch_archive(&client, &url).await;
            (kind, url, got)
        });
        true
    };

    for _ in 0..max_in_flight.max(1) {
        if !spawn_next(&mut set) {
            break;
        }
    }

    while let Some(joined) = set.join_next().await {
        let (kind, url, got) =
            joined.map_err(|e| SourceError::Decode(format!("archive task panicked: {e}")))?;
        if let Some(csv) = got? {
            out.push((kind, url, csv));
        }
        if !min_delay.is_zero() {
            tokio::time::sleep(min_delay).await;
        }
        spawn_next(&mut set);
    }
    Ok(out)
}

/// Pull `(epoch_ms, schema slot, value)` triples out of an archive's CSV,
/// reading every column it needs **by header name** rather than by position, so
/// an added column upstream doesn't silently shift the values.
///
/// A blank cell is skipped rather than parsed: the archives leave a ratio empty
/// on a bar with no trades, and a `0.0` there would read as a real
/// all-short reading.
fn parse_archive(
    kind: Archive,
    text: &str,
    url: &str,
) -> Result<Vec<(i64, usize, Real)>, SourceError> {
    let mut reader = csv::Reader::from_reader(text.as_bytes());
    let headers = reader
        .headers()
        .map_err(|e| SourceError::Decode(format!("{url}: header: {e}")))?
        .clone();
    let index_of = |name: &str| {
        headers
            .iter()
            .position(|h| h.trim() == name)
            .ok_or_else(|| SourceError::Decode(format!("{url}: missing column `{name}`")))
    };
    let time_col = kind.time_column();
    let i_time = index_of(time_col)?;
    let wanted: Vec<(usize, usize, &'static str)> = kind
        .columns()
        .iter()
        .map(|(name, slot)| index_of(name).map(|i| (i, *slot, *name)))
        .collect::<Result<_, _>>()?;

    let mut out = Vec::new();
    for record in reader.records() {
        let record = record.map_err(|e| SourceError::Decode(format!("{url}: row: {e}")))?;
        let raw_time = record.get(i_time).unwrap_or_default().trim();
        let time = kind
            .parse_time(raw_time)
            .map_err(|e| SourceError::Decode(format!("{url}: `{time_col}` = {raw_time:?}: {e}")))?;
        for &(i, slot, name) in &wanted {
            let raw = record.get(i).unwrap_or_default().trim();
            if raw.is_empty() {
                continue;
            }
            let value: Real = raw.parse().map_err(|e| {
                SourceError::Decode(format!("{url}: `{name}` = {raw:?}: {e}"))
            })?;
            out.push((time, slot, value));
        }
    }
    Ok(out)
}

/// Every `YYYY-MM-DD` the half-open range `[since, until)` touches.
fn days_between(since: i64, until: i64) -> Vec<String> {
    const DAY_MS: i64 = 86_400_000;
    if until <= since {
        return Vec::new();
    }
    let first = since - since.rem_euclid(DAY_MS);
    let mut out = Vec::new();
    let mut day = first;
    while day < until {
        let date = Timestamp(day).to_datetime().date();
        out.push(format!(
            "{:04}-{:02}-{:02}",
            date.year(),
            date.month() as u8,
            date.day()
        ));
        day += DAY_MS;
    }
    out
}

/// Every `(year, month)` the half-open range `[since, until)` touches, in
/// ascending order. The archives are monthly, so a range covering one hour of
/// one day still needs that whole month's file.
fn months_between(since: i64, until: i64) -> Vec<(i32, u8)> {
    if until <= since {
        return Vec::new();
    }
    let start = Timestamp(since).to_datetime();
    let end = Timestamp(until.saturating_sub(1)).to_datetime();
    let (mut year, mut month) = (start.year(), start.month() as u8);
    let last = (end.year(), end.month() as u8);

    let mut out = Vec::new();
    while (year, month) <= last {
        out.push((year, month));
        if month == 12 {
            year += 1;
            month = 1;
        } else {
            month += 1;
        }
    }
    out
}

/// The archive's interval token for the premium-kline path, and the single
/// cadence gate for this source.
///
/// The admitted set is exactly what Binance publishes under
/// `premiumIndexKlines`: `1h` through `1d`. Two constraints happen to agree on
/// it. Below an hour, funding — which settles every 4–8 hours — would be absent
/// from almost every bar. Above a day the archive simply does not exist: `3d`,
/// `1w` and monthly all 404, and admitting them would hand back a series with a
/// `funding_rate` column and a silently empty `premium_index` one, which reads
/// as "no premium" rather than "never published".
fn interval_token(interval: Interval) -> Result<&'static str, SourceError> {
    let token = match interval {
        Interval::Hour(1) => "1h",
        Interval::Hour(2) => "2h",
        Interval::Hour(4) => "4h",
        Interval::Hour(6) => "6h",
        Interval::Hour(8) => "8h",
        Interval::Hour(12) => "12h",
        Interval::Day(1) => "1d",
        other => return Err(SourceError::UnsupportedInterval(other)),
    };
    Ok(token)
}

/// Turn a non-2xx response into a [`SourceError`], preferring the specific
/// variants over the generic `Http`.
async fn map_http_error(resp: reqwest::Response) -> SourceError {
    let status = resp.status();
    let code = status.as_u16();
    let retry_after_ms = resp
        .headers()
        .get(reqwest::header::RETRY_AFTER)
        .and_then(|v| v.to_str().ok())
        .and_then(|s| s.trim().parse::<u64>().ok())
        .map(|secs| secs.saturating_mul(1000));
    let body = resp.text().await.unwrap_or_default();
    if code == 429 || code == 418 {
        return SourceError::RateLimited {
            retry_after_ms: retry_after_ms.unwrap_or(0),
        };
    }
    if let Ok(err) = serde_json::from_str::<BinanceError>(&body)
        && err.code == -1121
    {
        return SourceError::UnknownSymbol(err.msg);
    }
    SourceError::Http { status: code, body }
}

#[derive(Deserialize)]
struct BinanceError {
    code: i64,
    msg: String,
}

#[derive(Deserialize)]
struct ExchangeInfo {
    #[serde(default)]
    symbols: Vec<ExchangeSymbol>,
}

#[derive(Deserialize)]
struct ExchangeSymbol {
    symbol: String,
    #[serde(default)]
    status: String,
    #[serde(default, rename = "contractType")]
    contract_type: String,
}

/// Midnight UTC on the first of `(year, month)`, as epoch ms — the helper the
/// month-enumeration tests compare against.
#[cfg(test)]
fn month_start_ms(year: i32, month: u8) -> i64 {
    use time::{Date, Month, Time};
    let m = Month::try_from(month).expect("1..=12");
    let d = Date::from_calendar_date(year, m, 1).expect("day 1 is valid in every month");
    Timestamp::from_datetime(d.with_time(Time::MIDNIGHT).assume_utc()).0
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_archive_column_maps_to_a_real_schema_slot() {
        // `Archive::columns` indexes into the schema by position, so the two
        // definitions have to be checked against each other or a reordered
        // schema would silently write values into the wrong column.
        let schema = binance_vision_schema();
        assert_eq!(schema.len(), 8);
        let mut seen = vec![false; schema.len()];
        for kind in Archive::ALL {
            for &(_, slot) in kind.columns() {
                assert!(slot < schema.len(), "{kind:?} indexes past the schema");
                assert!(!seen[slot], "slot {slot} claimed twice");
                seen[slot] = true;
            }
        }
        assert!(seen.iter().all(|&s| s), "every schema slot must have a source");
    }

    #[test]
    fn the_schema_names_are_stable() {
        let schema = binance_vision_schema();
        for (i, name) in [
            "funding_rate",
            "premium_index",
            "open_interest",
            "open_interest_value",
            "long_short_ratio",
            "top_trader_account_ratio",
            "top_trader_position_ratio",
            "taker_long_short_ratio",
        ]
        .into_iter()
        .enumerate()
        {
            assert_eq!(schema.index_of(name), Some(i), "column `{name}`");
        }
    }

    #[test]
    fn admits_exactly_the_published_cadences() {
        // Funding settles every 4-8h, so a sub-hourly bucket would be empty
        // almost everywhere and read as "no carry".
        assert!(interval_token(Interval::Minute(1)).is_err());
        assert!(interval_token(Interval::Minute(30)).is_err());

        for (interval, token) in [
            (Interval::Hour(1), "1h"),
            (Interval::Hour(2), "2h"),
            (Interval::Hour(4), "4h"),
            (Interval::Hour(6), "6h"),
            (Interval::Hour(8), "8h"),
            (Interval::Hour(12), "12h"),
            (Interval::Day(1), "1d"),
        ] {
            assert_eq!(interval_token(interval).unwrap(), token);
        }

        // Above a day the premium archive is not published at all. Admitting
        // these would return funding with a silently empty premium column.
        assert!(interval_token(Interval::Day(3)).is_err());
        assert!(interval_token(Interval::Week(1)).is_err());
        assert!(interval_token(Interval::Month(1)).is_err());
    }

    #[test]
    fn months_between_covers_every_touched_month() {
        // A range of one hour still needs that month's whole archive.
        let start = month_start_ms(2024, 3);
        assert_eq!(months_between(start, start + 3_600_000), vec![(2024, 3)]);
        // And a range spanning a year boundary walks across it.
        assert_eq!(
            months_between(month_start_ms(2023, 11), month_start_ms(2024, 2)),
            vec![(2023, 11), (2023, 12), (2024, 1)],
        );
        // `until` is exclusive: landing exactly on a month start must not pull
        // that month's archive.
        assert_eq!(
            months_between(month_start_ms(2024, 1), month_start_ms(2024, 2)),
            vec![(2024, 1)],
        );
        assert!(months_between(start, start).is_empty());
    }

    #[test]
    fn archive_urls_match_the_published_layout() {
        let base = "https://data.binance.vision";
        assert_eq!(
            Archive::Funding.url(base, "BTCUSDT", "1d", "2024-01"),
            "https://data.binance.vision/data/futures/um/monthly/fundingRate/BTCUSDT/BTCUSDT-fundingRate-2024-01.zip"
        );
        assert_eq!(
            Archive::Premium.url(base, "BTCUSDT", "1d", "2024-01"),
            "https://data.binance.vision/data/futures/um/monthly/premiumIndexKlines/BTCUSDT/1d/BTCUSDT-1d-2024-01.zip"
        );
    }

    #[test]
    fn parses_each_archive_by_header_name() {
        let funding = "calc_time,funding_interval_hours,last_funding_rate\n\
                       1704067200000,8,0.00037409\n\
                       1704096000000,8,0.00027213\n";
        assert_eq!(
            parse_archive(Archive::Funding, funding, "u").unwrap(),
            vec![(1704067200000, 0, 0.00037409), (1704096000000, 0, 0.00027213)],
        );

        // The premium archive is kline-shaped; only `open_time` and `close`
        // are read, and reading them by name is what keeps an added column
        // upstream from shifting the values.
        let premium = "open_time,open,high,low,close,volume,close_time,quote_volume,count,\
                       taker_buy_volume,taker_buy_quote_volume,ignore\n\
                       1704067200000,0.00075030,0.00206526,-0.00000803,0.00120254,0,\
                       1704153599999,0,17280,0,0,0\n";
        assert_eq!(
            parse_archive(Archive::Premium, premium, "u").unwrap(),
            vec![(1704067200000, 1, 0.00120254)],
        );
    }

    #[test]
    fn metrics_carries_six_columns_and_a_datetime_stamp() {
        // Unlike the monthly archives, `metrics` stamps rows with a UTC
        // datetime string rather than epoch millis.
        let metrics = "create_time,symbol,sum_open_interest,sum_open_interest_value,\
                       count_toptrader_long_short_ratio,sum_toptrader_long_short_ratio,\
                       count_long_short_ratio,sum_taker_long_short_vol_ratio\n\
                       2026-07-15 00:00:00,BTCUSDT,105550.985,6858675634.706254,\
                       1.28623339,1.47112400,1.19972635,1.55827200\n";
        let parsed = parse_archive(Archive::Metrics, metrics, "u").unwrap();
        let at = Timestamp::from_datetime(
            time::macros::datetime!(2026-07-15 00:00:00).assume_utc(),
        )
        .0;
        assert_eq!(
            parsed,
            vec![
                (at, 2, 105550.985),
                (at, 3, 6858675634.706254),
                (at, 4, 1.19972635),
                (at, 5, 1.28623339),
                (at, 6, 1.47112400),
                (at, 7, 1.55827200),
            ],
        );
    }

    #[test]
    fn a_blank_cell_is_absent_rather_than_zero() {
        // The archives leave a ratio empty on a bar with no trades. A `0.0`
        // there would read as a genuine all-short print.
        let metrics = "create_time,sum_open_interest,sum_open_interest_value,\
                       count_toptrader_long_short_ratio,sum_toptrader_long_short_ratio,\
                       count_long_short_ratio,sum_taker_long_short_vol_ratio\n\
                       2026-07-15 00:00:00,105550.985,6858675634.706254,,,,\n";
        let parsed = parse_archive(Archive::Metrics, metrics, "u").unwrap();
        assert_eq!(parsed.len(), 2, "only the two populated cells survive");
        assert!(parsed.iter().all(|&(_, slot, _)| slot == 2 || slot == 3));
    }

    #[test]
    fn a_reordered_header_still_reads_the_right_column() {
        let swapped = "last_funding_rate,calc_time,funding_interval_hours\n\
                       0.00037409,1704067200000,8\n";
        assert_eq!(
            parse_archive(Archive::Funding, swapped, "u").unwrap(),
            vec![(1704067200000, 0, 0.00037409)],
        );
    }

    #[test]
    fn a_missing_column_is_an_error_not_a_silent_zero() {
        let wrong = "time,rate\n1704067200000,0.0003\n";
        let err = parse_archive(Archive::Funding, wrong, "u").unwrap_err();
        assert!(format!("{err}").contains("calc_time"), "got {err}");
    }

    #[test]
    fn days_between_covers_every_touched_day() {
        let day = 86_400_000_i64;
        let start = month_start_ms(2024, 3);
        assert_eq!(days_between(start, start + 1), vec!["2024-03-01"]);
        assert_eq!(
            days_between(start, start + 2 * day),
            vec!["2024-03-01", "2024-03-02"]
        );
        // `until` is exclusive: landing exactly on a day boundary must not
        // pull that day's archive.
        assert_eq!(days_between(start, start + day), vec!["2024-03-01"]);
        assert!(days_between(start, start).is_empty());
    }
}
