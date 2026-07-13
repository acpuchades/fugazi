//! CoinMarketCap implementation of [`OverlaySource`] — price, 24h volume,
//! market capitalisation, and circulating / total supply, with **no OHLCV**.
//!
//! Fetches `GET /v2/cryptocurrency/quotes/historical`, which returns a per-bar
//! quote series for one asset. Five `Real` overlay columns come out (see
//! [`coinmarketcap_schema`]):
//!
//! | column | source |
//! |---|---|
//! | `price` | `quote.<convert>.price` |
//! | `volume_24h` | `quote.<convert>.volume_24h` (24h rolling, as CMC reports it) |
//! | `market_cap` | `quote.<convert>.market_cap` |
//! | `circulating_supply` | `quote.<convert>.circulating_supply`, or **derived** `market_cap / price` when CMC omits it |
//! | `total_supply` | `quote.<convert>.total_supply` |
//!
//! `price` is worth keeping even though you are joining these onto someone
//! else's candles: comparing it against that series' `close` is the cheapest way
//! to catch a wrong `OUTPUT=QUERY` mapping (see [`OverlaySource`]). Any column
//! CMC does not return for a given bar is `NaN` rather than dropping the row.
//!
//! ## Paid tier only
//!
//! `quotes/historical` is a **historical** endpoint, and CoinMarketCap gates
//! historical data behind its paid plans (the free "Basic" plan cannot reach
//! it — the API answers `402 Payment Required`, which this client surfaces as
//! [`SourceError::Http`] with a plan hint rather than a silent empty result).
//! [`CoinMarketCap::new`] reads a key from the [`API_KEY_ENV`] environment
//! variable (falling back to [`API_KEY_ENV_ALT`]) and sends it as the
//! `X-CMC_PRO_API_KEY` header; [`CoinMarketCap::with_api_key`] sets it
//! explicitly. A request with no key is refused with `401`.
//!
//! ## Intervals map onto CMC's native cadences
//!
//! Unlike CoinGecko's `market_chart/range`, `quotes/historical` takes an
//! explicit `interval`, so this client asks for exactly the requested cadence
//! rather than bucketing an over-sampled response. The cadences it admits are
//! those with a canonical bar-open anchor to join against a candle series:
//! `Hour(1|2|3|4|6|12)`, `Day(1)` (→ `daily`), `Week(1)` (→ `weekly`),
//! `Month(1)` (→ `monthly`). Everything else is
//! [`SourceError::UnsupportedInterval`] — CMC also serves `5m`/`3d`/`90d`/… but
//! those either lack a clean join anchor or agree with no common candle
//! provider's boundaries. Returned timestamps are still floored to the interval
//! boundary ([`floor_to_bucket`]), first-sample-wins per bucket, so the value is
//! the one a strategy could have seen at the bar's open.

use std::collections::BTreeMap;
use std::future::Future;
use std::sync::{Arc, OnceLock};
use std::time::Duration;

use serde::Deserialize;
use time::format_description::well_known::Rfc3339;
use time::{Date, Duration as TimeDuration, OffsetDateTime, Time};

use crate::types::{OverlayInfo, OverlayValue, Real, Schema};

use super::{Interval, OverlayRow, OverlaySource, SourceError, Timestamp};

const DEFAULT_BASE_URL: &str = "https://pro-api.coinmarketcap.com";
const DEFAULT_MIN_DELAY_MS: u64 = 350;

/// Endpoint path for the historical quote series (a paid-tier endpoint).
const HISTORICAL_PATH: &str = "/v2/cryptocurrency/quotes/historical";
/// Endpoint path for the id map, backing [`OverlaySource::tickers`].
const MAP_PATH: &str = "/v1/cryptocurrency/map";

/// The header CoinMarketCap authenticates with.
const API_KEY_HEADER: &str = "X-CMC_PRO_API_KEY";

/// Primary environment variable read by [`CoinMarketCap::new`] for an API key —
/// the name CoinMarketCap's own examples use.
pub const API_KEY_ENV: &str = "CMC_PRO_API_KEY";
/// Fallback environment variable, for callers who prefer the spelled-out name.
pub const API_KEY_ENV_ALT: &str = "COINMARKETCAP_API_KEY";

/// Most quotes CMC will return in one `quotes/historical` request. The endpoint
/// caps `count` per plan; we ask for the ceiling and paginate on the returned
/// tail, so a lower per-plan cap only means more pages, never missing data.
const MAX_COUNT: usize = 10_000;

/// The overlay columns CoinMarketCap exposes. Read from a strategy or
/// `--overlay` spec with `!get { key: market_cap }`, etc.
///
/// `circulating_supply` falls back to the `market_cap / price` identity on any
/// bar where CMC does not report it directly; see the [module docs](self).
pub fn coinmarketcap_schema() -> &'static Arc<Schema> {
    static SCHEMA: OnceLock<Arc<Schema>> = OnceLock::new();
    SCHEMA.get_or_init(|| {
        let mut b = Schema::builder();
        b.add_real("price");
        b.add_real("volume_24h");
        b.add_real("market_cap");
        b.add_real("circulating_supply");
        b.add_real("total_supply");
        b.finish()
    })
}

/// A CoinMarketCap historical-quotes client.
///
/// Cheap to clone (the inner [`reqwest::Client`] is `Arc`-backed).
///
/// The `symbol` this provider takes is a CoinMarketCap **ticker** (`BTC`,
/// `ETH`) or a **numeric id** (`1` for Bitcoin): a value that parses as an
/// integer is sent as `id`, everything else as `symbol`. Ids are unambiguous;
/// tickers are convenient but CMC may map one ticker to several assets, in which
/// case it returns the primary one. Fetch the vocabulary with
/// [`OverlaySource::tickers`] (`fugazi list tickers cmc`).
#[derive(Debug, Clone)]
pub struct CoinMarketCap {
    client: reqwest::Client,
    base_url: String,
    convert: String,
    api_key: Option<String>,
    min_delay_between_requests: Duration,
}

impl Default for CoinMarketCap {
    fn default() -> Self {
        Self::new()
    }
}

impl CoinMarketCap {
    /// A client pointing at the CoinMarketCap Pro endpoint, quoting in USD.
    ///
    /// Picks up an API key from the [`API_KEY_ENV`] environment variable
    /// (falling back to [`API_KEY_ENV_ALT`]) if one is set. Historical data is
    /// a paid-tier feature, so a usable client needs a key from a paid plan.
    pub fn new() -> Self {
        let api_key = std::env::var(API_KEY_ENV)
            .ok()
            .or_else(|| std::env::var(API_KEY_ENV_ALT).ok())
            .filter(|k| !k.trim().is_empty());
        Self {
            client: reqwest::Client::new(),
            base_url: DEFAULT_BASE_URL.to_string(),
            convert: "USD".to_string(),
            api_key,
            min_delay_between_requests: Duration::from_millis(DEFAULT_MIN_DELAY_MS),
        }
    }

    /// Override the API base URL. Primarily useful for testing against a local
    /// mock server, or pointing at CMC's `sandbox-api` host.
    pub fn with_base_url(mut self, url: impl Into<String>) -> Self {
        self.base_url = url.into();
        self
    }

    /// Quote against a currency other than USD (`"EUR"`, `"BTC"`, …). CMC keys
    /// its quote object by this code (upper-cased), so `price` / `market_cap` /
    /// `volume_24h` are all expressed in it; `circulating_supply` and
    /// `total_supply` are unaffected.
    pub fn with_convert(mut self, currency: impl Into<String>) -> Self {
        self.convert = currency.into().to_uppercase();
        self
    }

    /// Set the API key explicitly, overriding the environment variables.
    pub fn with_api_key(mut self, key: impl Into<String>) -> Self {
        self.api_key = Some(key.into());
        self
    }

    /// Override the delay between successive requests (default 350 ms). Paid
    /// tiers permit a healthy per-minute budget, so this is a light throttle;
    /// tighten or loosen it to match your plan.
    pub fn with_min_delay(mut self, d: Duration) -> Self {
        self.min_delay_between_requests = d;
        self
    }
}

impl OverlaySource for CoinMarketCap {
    fn name(&self) -> &'static str {
        "cmc"
    }

    fn schema(&self) -> Arc<Schema> {
        coinmarketcap_schema().clone()
    }

    fn tickers(&self) -> impl Future<Output = Result<Vec<String>, SourceError>> + Send {
        let client = self.client.clone();
        let base_url = self.base_url.clone();
        let api_key = self.api_key.clone();
        async move {
            let url = format!("{}{}", base_url.trim_end_matches('/'), MAP_PATH);
            let resp = prepare(client.get(&url), api_key.as_deref())
                .query(&[("listing_status", "active,untracked")])
                .send()
                .await?;
            if !resp.status().is_success() {
                return Err(map_http_error(resp).await);
            }
            let body: MapResponse = resp
                .json()
                .await
                .map_err(|e| SourceError::Decode(format!("cryptocurrency/map JSON: {e}")))?;
            let mut out: Vec<String> = body.data.into_iter().map(|c| c.symbol).collect();
            out.sort();
            out.dedup();
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
        let query_id = symbol.to_string();
        let client = self.client.clone();
        let base_url = self.base_url.clone();
        let convert = self.convert.clone();
        let api_key = self.api_key.clone();
        let min_delay = self.min_delay_between_requests;
        async move {
            let cmc_interval = interval_token(interval)?;
            let schema = coinmarketcap_schema().clone();
            let until_ms = until.map(|t| t.0).unwrap_or_else(|| Timestamp::now().0);
            let url = format!("{}{}", base_url.trim_end_matches('/'), HISTORICAL_PATH);

            // CMC accepts `id` for a numeric identifier and `symbol` otherwise.
            // Ids are unambiguous, so prefer them when the caller gave one.
            let (id_param, id_value) = match query_id.parse::<u64>() {
                Ok(_) => ("id", query_id.clone()),
                Err(_) => ("symbol", query_id.clone()),
            };

            // Accumulate across pages before bucketing: a bucket can straddle a
            // page boundary, and "first sample wins" has to mean first across
            // the whole fetch, not first within a page.
            let mut bucketed: BTreeMap<i64, Fields> = BTreeMap::new();
            let mut cursor = since.0;
            let mut first = true;

            while cursor < until_ms {
                if !first {
                    tokio::time::sleep(min_delay).await;
                }
                first = false;

                // CMC's time bounds are seconds (ISO or epoch); send epoch
                // seconds, flooring `from` and ceiling `to` so a bar exactly on
                // a boundary is never dropped by integer truncation.
                let from_s = cursor.div_euclid(1000);
                let to_s = until_ms.div_euclid(1000) + 1;
                let count = MAX_COUNT.to_string();

                let query: Vec<(&str, String)> = vec![
                    (id_param, id_value.clone()),
                    ("interval", cmc_interval.to_string()),
                    ("time_start", from_s.to_string()),
                    ("time_end", to_s.to_string()),
                    ("count", count),
                    ("convert", convert.clone()),
                    ("aux", "price,volume_24h,market_cap,circulating_supply,total_supply".to_string()),
                ];
                let resp = prepare(client.get(&url).query(&query), api_key.as_deref())
                    .send()
                    .await?;
                if !resp.status().is_success() {
                    return Err(map_http_error(resp).await);
                }
                let parsed: HistoricalResponse = resp
                    .json()
                    .await
                    .map_err(|e| SourceError::Decode(format!("quotes/historical JSON: {e}")))?;
                parsed.status.check()?;

                let quotes = parsed.into_quotes();
                let mut newest = cursor;
                let mut count_seen = 0usize;
                for q in &quotes {
                    let Some(ms) = q.millis() else { continue };
                    count_seen += 1;
                    newest = newest.max(ms);
                    let bucket = floor_to_bucket(ms, interval);
                    bucketed
                        .entry(bucket)
                        .or_insert_with(|| q.fields(&convert));
                }

                // A short page means CMC has no more to give; a full page may be
                // truncated by `count`, so continue from just past the newest
                // sample. Guard against a page that made no forward progress.
                if count_seen < MAX_COUNT || newest <= cursor {
                    break;
                }
                cursor = newest + 1;
            }

            Ok(into_rows(bucketed, &schema, since.0, until_ms))
        }
    }
}

/// Attach the CMC API-key header when one is configured. A request without it is
/// refused with `401`.
fn prepare(req: reqwest::RequestBuilder, key: Option<&str>) -> reqwest::RequestBuilder {
    match key {
        Some(k) => req.header(API_KEY_HEADER, k),
        None => req,
    }
}

/// Map a fugazi [`Interval`] to the CMC `interval` token, rejecting cadences
/// without a canonical bar-open anchor to join against.
///
/// Accepted: `Hour(1|2|3|4|6|12)` → `1h`…`12h`, `Day(1)` → `daily`, `Week(1)` →
/// `weekly`, `Month(1)` → `monthly`. Everything else is
/// [`SourceError::UnsupportedInterval`] (see the [module docs](self)).
fn interval_token(interval: Interval) -> Result<&'static str, SourceError> {
    let token = match interval {
        Interval::Hour(1) => "1h",
        Interval::Hour(2) => "2h",
        Interval::Hour(3) => "3h",
        Interval::Hour(4) => "4h",
        Interval::Hour(6) => "6h",
        Interval::Hour(12) => "12h",
        Interval::Day(1) => "daily",
        Interval::Week(1) => "weekly",
        Interval::Month(1) => "monthly",
        other => return Err(SourceError::UnsupportedInterval(other)),
    };
    Ok(token)
}

/// Floor a millisecond timestamp onto the bar-open boundary of `interval`.
///
/// Mirrors CoinGecko's flooring: `Week` floors to Monday 00:00 UTC and `Month`
/// to the 1st at 00:00 UTC (epoch modulo would anchor weeks on the epoch's
/// Thursday and cannot express month starts at all); everything else floors by
/// epoch modulo, which lands on real clock boundaries. Only the cadences
/// [`interval_token`] admits are reachable here.
fn floor_to_bucket(ms: i64, interval: Interval) -> i64 {
    match interval {
        Interval::Week(1) => {
            let dt = Timestamp(ms).to_datetime();
            let back = dt.weekday().number_days_from_monday() as i64;
            let monday = dt.date() - TimeDuration::days(back);
            Timestamp::from_datetime(monday.with_time(Time::MIDNIGHT).assume_utc()).0
        }
        Interval::Month(1) => {
            let dt = Timestamp(ms).to_datetime();
            let first = Date::from_calendar_date(dt.year(), dt.month(), 1)
                .expect("day 1 is valid in every month");
            Timestamp::from_datetime(first.with_time(Time::MIDNIGHT).assume_utc()).0
        }
        other => {
            let step = other.duration_ms();
            if step <= 0 { ms } else { ms - ms.rem_euclid(step) }
        }
    }
}

/// The five overlay values for one bucket, `NaN` for anything CMC omitted.
#[derive(Debug, Clone, Copy)]
struct Fields {
    price: Real,
    volume_24h: Real,
    market_cap: Real,
    circulating_supply: Real,
    total_supply: Real,
}

/// Emit one [`OverlayRow`] per bucket, ascending by time, restricted to
/// `[since, until)`. A field a bucket never got is `NaN` rather than dropping
/// the row — a bar with a price but no supply still produces a usable `price`.
fn into_rows(
    bucketed: BTreeMap<i64, Fields>,
    schema: &Arc<Schema>,
    since_ms: i64,
    until_ms: i64,
) -> Vec<OverlayRow> {
    bucketed
        .into_iter()
        .filter(|&(t, _)| t >= since_ms && t < until_ms)
        .map(|(t, f)| {
            let overlays = OverlayInfo::new(
                schema.clone(),
                vec![
                    OverlayValue::Real(f.price),
                    OverlayValue::Real(f.volume_24h),
                    OverlayValue::Real(f.market_cap),
                    OverlayValue::Real(f.circulating_supply),
                    OverlayValue::Real(f.total_supply),
                ],
            );
            OverlayRow {
                time: Timestamp(t),
                overlays,
            }
        })
        .collect()
}

/// `market_cap / price`, the supply identity — `NaN` unless both inputs are
/// finite and the price is non-zero, so a missing input propagates as a missing
/// cell instead of an infinity. Used only as a fallback when CMC omits
/// `circulating_supply`.
fn derived_circulating_supply(market_cap: Real, price: Real) -> Real {
    if market_cap.is_finite() && price.is_finite() && price != 0.0 {
        market_cap / price
    } else {
        Real::NAN
    }
}

/// Turn a non-2xx response into a [`SourceError`], preferring specific variants.
/// CMC uses `402` for an endpoint outside the caller's plan (the common
/// paid-tier tripwire), `401` for a bad/missing key, `429` for rate limiting,
/// and `400` for an unknown symbol.
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
    match code {
        429 => SourceError::RateLimited {
            retry_after_ms: retry_after_ms.unwrap_or(0),
        },
        400 | 404 => SourceError::UnknownSymbol(format!(
            "CoinMarketCap rejected the identifier (pass a ticker like `BTC` or a \
             numeric id — see `fugazi list tickers cmc`): {body}"
        )),
        401 => SourceError::Http {
            status: code,
            body: format!("invalid or missing CoinMarketCap API key (set {API_KEY_ENV}): {body}"),
        },
        402 => SourceError::Http {
            status: code,
            body: format!(
                "CoinMarketCap historical data requires a paid plan — this endpoint is not \
                 available on your current tier: {body}"
            ),
        },
        _ => SourceError::Http { status: code, body },
    }
}

/// The `quotes/historical` response. `data` is polymorphic across CMC API
/// versions — a list of series, a map keyed by id/symbol, or (when a symbol maps
/// to several assets) a map whose values are lists. [`Data`] absorbs all three.
#[derive(Debug, Deserialize)]
struct HistoricalResponse {
    #[serde(default)]
    data: Option<Data>,
    status: Status,
}

impl HistoricalResponse {
    /// Flatten whatever `data` shape came back into the first series' quotes.
    fn into_quotes(self) -> Vec<QuotePoint> {
        match self.data {
            Some(Data::List(series)) => series.into_iter().flat_map(|s| s.quotes).collect(),
            Some(Data::Map(map)) => map
                .into_values()
                .flat_map(|e| e.into_series())
                .flat_map(|s| s.quotes)
                .collect(),
            None => Vec::new(),
        }
    }
}

/// The two container shapes CMC uses for `data`. `#[serde(untagged)]`
/// disambiguates on the JSON kind: an array decodes as `List`, an object as
/// `Map`.
#[derive(Debug, Deserialize)]
#[serde(untagged)]
enum Data {
    List(Vec<Series>),
    Map(std::collections::HashMap<String, DataEntry>),
}

/// A map value is either a single series (query by id) or a list of them (a
/// symbol that maps to several assets).
#[derive(Debug, Deserialize)]
#[serde(untagged)]
enum DataEntry {
    Single(Series),
    Multiple(Vec<Series>),
}

impl DataEntry {
    fn into_series(self) -> Vec<Series> {
        match self {
            DataEntry::Single(s) => vec![s],
            DataEntry::Multiple(v) => v,
        }
    }
}

/// One asset's quote series.
#[derive(Debug, Deserialize)]
struct Series {
    #[serde(default)]
    quotes: Vec<QuotePoint>,
}

/// One point in a quote series: a timestamp plus the per-currency quote map.
#[derive(Debug, Deserialize)]
struct QuotePoint {
    timestamp: String,
    #[serde(default)]
    quote: std::collections::HashMap<String, QuoteFields>,
}

impl QuotePoint {
    /// Parse the RFC3339 / epoch-seconds timestamp into epoch millis, or `None`
    /// if it is unparseable (that point is then skipped).
    fn millis(&self) -> Option<i64> {
        let s = self.timestamp.trim();
        if let Ok(dt) = OffsetDateTime::parse(s, &Rfc3339) {
            return Some(Timestamp::from_datetime(dt).0);
        }
        // Some responses stamp with a bare epoch (seconds).
        s.parse::<i64>().ok().map(|secs| secs.saturating_mul(1000))
    }

    /// Project this point's `convert`-currency quote into the five columns,
    /// deriving `circulating_supply` from `market_cap / price` when CMC omits it.
    fn fields(&self, convert: &str) -> Fields {
        let q = self.quote.get(convert);
        let price = q.and_then(|q| q.price).unwrap_or(Real::NAN);
        let volume_24h = q.and_then(|q| q.volume_24h).unwrap_or(Real::NAN);
        let market_cap = q.and_then(|q| q.market_cap).unwrap_or(Real::NAN);
        let circulating_supply = q
            .and_then(|q| q.circulating_supply)
            .unwrap_or_else(|| derived_circulating_supply(market_cap, price));
        let total_supply = q.and_then(|q| q.total_supply).unwrap_or(Real::NAN);
        Fields {
            price,
            volume_24h,
            market_cap,
            circulating_supply,
            total_supply,
        }
    }
}

/// The per-currency quote fields. Every one is optional: CMC returns only the
/// `aux` columns it has, and a missing field becomes a `NaN` cell rather than
/// failing the whole fetch.
#[derive(Debug, Default, Deserialize)]
struct QuoteFields {
    #[serde(default)]
    price: Option<Real>,
    #[serde(default)]
    volume_24h: Option<Real>,
    #[serde(default)]
    market_cap: Option<Real>,
    #[serde(default)]
    circulating_supply: Option<Real>,
    #[serde(default)]
    total_supply: Option<Real>,
}

/// The `status` object every CMC response carries. A non-zero `error_code` on an
/// otherwise-2xx response still means the call failed.
#[derive(Debug, Deserialize)]
struct Status {
    #[serde(default)]
    error_code: i64,
    #[serde(default)]
    error_message: Option<String>,
}

impl Status {
    fn check(&self) -> Result<(), SourceError> {
        if self.error_code == 0 {
            return Ok(());
        }
        let msg = self.error_message.clone().unwrap_or_default();
        Err(SourceError::Http {
            status: self.error_code as u16,
            body: msg,
        })
    }
}

/// The `/v1/cryptocurrency/map` response — the id/symbol vocabulary.
#[derive(Debug, Deserialize)]
struct MapResponse {
    #[serde(default)]
    data: Vec<MapEntry>,
}

/// One row of the id map. `id` / `name` / `slug` are ignored: we enumerate
/// tickers, which is what a `coinmarketcap:BTC` spec keys on.
#[derive(Debug, Deserialize)]
struct MapEntry {
    symbol: String,
}

#[cfg(test)]
mod tests {
    use super::*;

    const MS_PER_DAY: i64 = 86_400_000;

    /// 2024-01-03T00:00:00Z — a Wednesday, so the weekly bucket must land on
    /// Monday 2024-01-01 and the monthly one on 2024-01-01 as well.
    const WED: i64 = 1_704_240_000_000;
    const MON: i64 = 1_704_067_200_000; // 2024-01-01T00:00:00Z

    #[test]
    fn accepts_only_the_cadences_it_can_serve() {
        assert_eq!(interval_token(Interval::Hour(1)).unwrap(), "1h");
        assert_eq!(interval_token(Interval::Hour(4)).unwrap(), "4h");
        assert_eq!(interval_token(Interval::Hour(12)).unwrap(), "12h");
        assert_eq!(interval_token(Interval::Day(1)).unwrap(), "daily");
        assert_eq!(interval_token(Interval::Week(1)).unwrap(), "weekly");
        assert_eq!(interval_token(Interval::Month(1)).unwrap(), "monthly");
        // Hour multiples CMC does not offer.
        assert!(matches!(
            interval_token(Interval::Hour(5)),
            Err(SourceError::UnsupportedInterval(_))
        ));
        // Sub-hourly and multi-day: no clean join anchor.
        assert!(matches!(
            interval_token(Interval::Minute(5)),
            Err(SourceError::UnsupportedInterval(_))
        ));
        assert!(matches!(
            interval_token(Interval::Day(3)),
            Err(SourceError::UnsupportedInterval(_))
        ));
        assert!(matches!(
            interval_token(Interval::Week(2)),
            Err(SourceError::UnsupportedInterval(_))
        ));
    }

    #[test]
    fn floors_hours_and_days_by_epoch_modulo() {
        let midday = WED + 13 * 3_600_000 + 42 * 60_000;
        assert_eq!(floor_to_bucket(midday, Interval::Day(1)), WED);
        assert_eq!(
            floor_to_bucket(midday, Interval::Hour(1)),
            WED + 13 * 3_600_000
        );
        assert_eq!(
            floor_to_bucket(midday, Interval::Hour(4)),
            WED + 12 * 3_600_000
        );
        assert_eq!(floor_to_bucket(WED, Interval::Day(1)), WED);
    }

    #[test]
    fn floors_weeks_to_monday_not_to_the_epoch_thursday() {
        assert_eq!(floor_to_bucket(WED, Interval::Week(1)), MON);
        assert_ne!(WED - WED.rem_euclid(7 * MS_PER_DAY), MON);
        assert_eq!(floor_to_bucket(MON, Interval::Week(1)), MON);
    }

    #[test]
    fn floors_months_to_the_first() {
        // 2024-02-29T12:00:00Z (leap day) → 2024-02-01T00:00:00Z.
        let leap_day = 1_709_208_000_000;
        let feb_1 = 1_706_745_600_000;
        assert_eq!(floor_to_bucket(leap_day, Interval::Month(1)), feb_1);
    }

    fn point(ts: &str, price: f64, market_cap: f64) -> QuotePoint {
        let mut quote = std::collections::HashMap::new();
        quote.insert(
            "USD".to_string(),
            QuoteFields {
                price: Some(price),
                volume_24h: Some(1.0),
                market_cap: Some(market_cap),
                circulating_supply: None,
                total_supply: Some(21_000_000.0),
            },
        );
        QuotePoint {
            timestamp: ts.to_string(),
            quote,
        }
    }

    #[test]
    fn parses_rfc3339_and_epoch_timestamps() {
        assert_eq!(
            point("2024-01-01T00:00:00.000Z", 1.0, 1.0).millis(),
            Some(MON)
        );
        assert_eq!(
            point("1704067200", 1.0, 1.0).millis(),
            Some(MON)
        );
        assert_eq!(point("not-a-time", 1.0, 1.0).millis(), None);
    }

    #[test]
    fn derives_circulating_supply_when_missing() {
        // 840e9 / 42e3 = 20e6 coins, and total_supply passes straight through.
        let f = point("2024-01-01T00:00:00Z", 42_000.0, 840_000_000_000.0).fields("USD");
        assert_eq!(f.circulating_supply, 20_000_000.0);
        assert_eq!(f.total_supply, 21_000_000.0);
    }

    #[test]
    fn missing_convert_currency_yields_nan_fields() {
        let f = point("2024-01-01T00:00:00Z", 1.0, 1.0).fields("EUR");
        for v in [f.price, f.volume_24h, f.market_cap, f.circulating_supply] {
            assert!(v.is_nan());
        }
    }

    #[test]
    fn rows_are_clipped_to_the_requested_half_open_window() {
        let mut bucketed = BTreeMap::new();
        for (i, t) in [MON, MON + MS_PER_DAY, MON + 2 * MS_PER_DAY].iter().enumerate() {
            bucketed.insert(
                *t,
                Fields {
                    price: i as f64,
                    volume_24h: Real::NAN,
                    market_cap: Real::NAN,
                    circulating_supply: Real::NAN,
                    total_supply: Real::NAN,
                },
            );
        }
        // `until` is exclusive, so the third bar is out.
        let rows = into_rows(bucketed, coinmarketcap_schema(), MON, MON + 2 * MS_PER_DAY);
        assert_eq!(rows.len(), 2);
        assert_eq!(rows[0].time, Timestamp(MON));
        assert_eq!(rows[1].time, Timestamp(MON + MS_PER_DAY));
    }

    #[test]
    fn data_decodes_from_both_list_and_map_shapes() {
        let list = r#"{"status":{"error_code":0},"data":[{"quotes":[
            {"timestamp":"2024-01-01T00:00:00Z","quote":{"USD":{"price":1.0}}}]}]}"#;
        let map = r#"{"status":{"error_code":0},"data":{"1":{"quotes":[
            {"timestamp":"2024-01-01T00:00:00Z","quote":{"USD":{"price":2.0}}}]}}}"#;
        let map_multi = r#"{"status":{"error_code":0},"data":{"BTC":[{"quotes":[
            {"timestamp":"2024-01-01T00:00:00Z","quote":{"USD":{"price":3.0}}}]}]}}"#;
        for (raw, want) in [(list, 1.0), (map, 2.0), (map_multi, 3.0)] {
            let parsed: HistoricalResponse = serde_json::from_str(raw).unwrap();
            let quotes = parsed.into_quotes();
            assert_eq!(quotes.len(), 1);
            assert_eq!(quotes[0].fields("USD").price, want);
        }
    }

    #[test]
    fn nonzero_status_error_code_is_an_error() {
        let raw = r#"{"status":{"error_code":1006,"error_message":"plan required"},"data":null}"#;
        let parsed: HistoricalResponse = serde_json::from_str(raw).unwrap();
        assert!(parsed.status.check().is_err());
    }
}
