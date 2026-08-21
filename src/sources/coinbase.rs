//! Coinbase Advanced Trade implementation of [`SeriesSource`].
//!
//! Fetches OHLCV bars from the **public** market-data endpoint
//! `GET /api/v5.../market/products/{product_id}/candles`
//! (`/api/v3/brokerage/market/products/{id}/candles`), which needs no
//! authentication. Coinbase caps a page at 300 candles and requires an explicit
//! `[start, end]` window (Unix **seconds**), so this client pages *forward*: it
//! walks a `max_per_request`-bar window from `since` toward `until`, one chunk at
//! a time, sleeping `min_delay_between_requests` between pages to stay under the
//! public 10-request-per-second budget. Coinbase returns each page newest-first
//! and the fixed-width windows overlap on their shared boundary bar, so the
//! accumulated atoms are sorted ascending and de-duplicated by timestamp before
//! returning.
//!
//! Coinbase candles carry only OHLCV — no quote volume, trade count, or other
//! side channel — so every atom is candle-only (no [`OverlayInfo`](crate::OverlayInfo),
//! no fixed [`schema`](SeriesSource::schema)). Symbols are dash-separated product ids
//! (`BTC-USD`, `ETH-USD`, `BTC-USDC`).
//!
//! Unlike OKX, Coinbase reports failures with a real HTTP status and a JSON
//! `{ "error", "error_details", "message" }` body. Errors are mapped into
//! [`SourceError`]:
//!
//! * HTTP `404` (or an `error` of `NOT_FOUND` / `INVALID_ARGUMENT` naming the
//!   product) → [`SourceError::UnknownSymbol`].
//! * HTTP `429` → [`SourceError::RateLimited`] (echoing `Retry-After` if present).
//! * Any other non-2xx → [`SourceError::Http`].
//! * JSON that doesn't match the expected shape → [`SourceError::Decode`].

use std::future::Future;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use serde::Deserialize;

use crate::types::{Atom, Candle, Real};

use super::{Interval, SeriesSource, SourceError, Timestamp};

const DEFAULT_BASE_URL: &str = "https://api.coinbase.com";
const API_PREFIX: &str = "/api/v3/brokerage";
// The Advanced Trade candles endpoint caps a page at 300 rows.
const DEFAULT_MAX_PER_REQUEST: usize = 300;
const DEFAULT_MIN_DELAY_MS: u64 = 120;

/// A Coinbase Advanced Trade candles client.
///
/// Cheap to clone (the inner [`reqwest::Client`] is `Arc`-backed).
#[derive(Debug, Clone)]
pub struct Coinbase {
    client: reqwest::Client,
    base_url: String,
    max_per_request: usize,
    min_delay_between_requests: Duration,
}

impl Default for Coinbase {
    fn default() -> Self {
        Self::new()
    }
}

impl Coinbase {
    /// A client pointing at the public Coinbase endpoint with sensible defaults
    /// (300 candles per page, 120 ms between pages).
    pub fn new() -> Self {
        Self {
            client: reqwest::Client::new(),
            base_url: DEFAULT_BASE_URL.to_string(),
            max_per_request: DEFAULT_MAX_PER_REQUEST,
            min_delay_between_requests: Duration::from_millis(DEFAULT_MIN_DELAY_MS),
        }
    }

    /// Override the API base URL (`https://api.coinbase.com` by default).
    /// Primarily useful for testing against a local `wiremock` server.
    pub fn with_base_url(mut self, url: impl Into<String>) -> Self {
        self.base_url = url.into();
        self
    }

    /// Override the max candles per HTTP request (default 300, Coinbase's max).
    pub fn with_max_per_request(mut self, n: usize) -> Self {
        self.max_per_request = n.clamp(1, 300);
        self
    }

    /// Override the delay between successive requests (default 120 ms).
    pub fn with_min_delay(mut self, d: Duration) -> Self {
        self.min_delay_between_requests = d;
        self
    }
}

impl SeriesSource for Coinbase {
    fn name(&self) -> &'static str {
        "coinbase"
    }

    fn tickers(&self) -> impl Future<Output = Result<Vec<String>, SourceError>> + Send {
        let client = self.client.clone();
        let base_url = self.base_url.clone();
        async move {
            let url = format!(
                "{}{API_PREFIX}/market/products",
                base_url.trim_end_matches('/')
            );
            let resp = client.get(&url).send().await?;
            let status = resp.status();
            if !status.is_success() {
                return Err(map_http_error(resp).await);
            }
            let body: Products = resp
                .json()
                .await
                .map_err(|e| SourceError::Decode(format!("products JSON: {e}")))?;
            // Keep only spot products that are online and tradable.
            let mut out: Vec<String> = body
                .products
                .into_iter()
                .filter(|p| {
                    p.status.as_deref() == Some("online")
                        && !p.trading_disabled
                        && p.product_type.as_deref().is_none_or(|t| t == "SPOT")
                })
                .map(|p| p.product_id)
                .collect();
            out.sort();
            Ok(out)
        }
    }

    fn atoms(
        &self,
        symbol: &str,
        interval: Interval,
        since: Timestamp,
        until: Option<Timestamp>,
    ) -> impl Future<Output = Result<Vec<Atom>, SourceError>> + Send {
        // Own the strings so the returned future doesn't borrow the caller.
        let symbol = symbol.to_string();
        let client = self.client.clone();
        let base_url = self.base_url.clone();
        let max_per_request = self.max_per_request;
        let min_delay = self.min_delay_between_requests;
        async move {
            let token = interval_to_token(interval)?;
            let url = format!(
                "{}{API_PREFIX}/market/products/{symbol}/candles",
                base_url.trim_end_matches('/')
            );

            // Coinbase timestamps are Unix seconds; the trait works in millis.
            let step_sec = interval.duration_ms() / 1000;
            let since_s = since.0.div_euclid(1000);
            let until_s = until
                .map(|t| t.0.div_euclid(1000))
                .unwrap_or_else(now_seconds);
            // Widest window one page can cover.
            let span = step_sec
                .saturating_mul(max_per_request as i64)
                .max(step_sec);

            let mut out: Vec<Atom> = Vec::new();
            let mut cursor = since_s;
            let mut first = true;

            // Page forward in fixed-width windows. A fixed step (rather than
            // chasing the newest timestamp seen) guarantees termination and full
            // coverage even across gaps in an illiquid product; the boundary bar
            // shared by adjacent windows is dropped by the final dedup.
            while cursor < until_s {
                if !first {
                    tokio::time::sleep(min_delay).await;
                }
                first = false;

                let end = cursor.saturating_add(span).min(until_s);
                let query: [(&str, String); 4] = [
                    ("start", cursor.to_string()),
                    ("end", end.to_string()),
                    ("granularity", token.to_string()),
                    ("limit", max_per_request.to_string()),
                ];

                let resp = client.get(&url).query(&query).send().await?;
                let status = resp.status();
                if !status.is_success() {
                    return Err(map_http_error(resp).await);
                }
                let body: CandlesEnvelope = resp
                    .json()
                    .await
                    .map_err(|e| SourceError::Decode(format!("candle JSON: {e}")))?;

                for row in &body.candles {
                    let atom = decode_row(row)?;
                    let ts_ms = atom.time.expect("Coinbase atoms always carry a time").0;
                    let ts_s = ts_ms.div_euclid(1000);
                    // Respect the half-open [since, until) contract.
                    if ts_s >= since_s && ts_s < until_s {
                        out.push(atom);
                    }
                }

                cursor = end;
            }

            // Windows overlap on their shared boundary and arrive newest-first;
            // present ascending and unique by bar-open.
            out.sort_by_key(|a| a.time.map(|t| t.0).unwrap_or(i64::MIN));
            out.dedup_by_key(|a| a.time.map(|t| t.0).unwrap_or(i64::MIN));
            Ok(out)
        }
    }
}

/// The current Unix time in whole seconds — the default upper bound when the
/// caller passes `until = None` ("up to now").
fn now_seconds() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}

/// Map an [`Interval`] to Coinbase's `granularity` vocabulary. Coinbase exposes
/// a fixed, sparse set — one, five, fifteen, thirty minutes; one, two, six
/// hours; one day — with no week/month bars and no arbitrary multiples, so any
/// other cadence is rejected.
fn interval_to_token(interval: Interval) -> Result<&'static str, SourceError> {
    let token = match interval {
        Interval::Minute(1) => "ONE_MINUTE",
        Interval::Minute(5) => "FIVE_MINUTE",
        Interval::Minute(15) => "FIFTEEN_MINUTE",
        Interval::Minute(30) => "THIRTY_MINUTE",
        Interval::Hour(1) => "ONE_HOUR",
        Interval::Hour(2) => "TWO_HOUR",
        Interval::Hour(6) => "SIX_HOUR",
        Interval::Day(1) => "ONE_DAY",
        other => return Err(SourceError::UnsupportedInterval(other)),
    };
    Ok(token)
}

/// Decode one candle object into an [`Atom`]. Coinbase ships every field as a
/// JSON string; `start` is the bar-open in Unix **seconds**.
fn decode_row(row: &CandleRow) -> Result<Atom, SourceError> {
    let start_s = parse_i64(&row.start, "start")?;
    let open = parse_num(&row.open, "open")?;
    let high = parse_num(&row.high, "high")?;
    let low = parse_num(&row.low, "low")?;
    let close = parse_num(&row.close, "close")?;
    let volume = parse_num(&row.volume, "volume")?;
    Ok(Atom::with_time(
        Candle::new(open, high, low, close, volume),
        Timestamp(start_s.saturating_mul(1000)),
    ))
}

/// A Coinbase numeric field: a JSON string (`"27000.5"`) or a bare number (a
/// mock server's convenience).
fn parse_num(v: &serde_json::Value, field: &str) -> Result<Real, SourceError> {
    match v {
        serde_json::Value::String(s) => s
            .parse::<Real>()
            .map_err(|e| SourceError::Decode(format!("candle `{field}` = {s:?}: {e}"))),
        serde_json::Value::Number(n) => n
            .as_f64()
            .ok_or_else(|| SourceError::Decode(format!("candle `{field}` is not finite"))),
        other => Err(SourceError::Decode(format!(
            "candle `{field}` has unexpected JSON type: {other}"
        ))),
    }
}

/// The `start` column, as an integer count of seconds. String (Coinbase's real
/// shape) or bare number (a mock) both decode.
fn parse_i64(v: &serde_json::Value, field: &str) -> Result<i64, SourceError> {
    match v {
        serde_json::Value::String(s) => s
            .parse::<i64>()
            .map_err(|e| SourceError::Decode(format!("candle `{field}` = {s:?}: {e}"))),
        serde_json::Value::Number(n) => n
            .as_i64()
            .ok_or_else(|| SourceError::Decode(format!("candle `{field}` is not an integer"))),
        other => Err(SourceError::Decode(format!(
            "candle `{field}` has unexpected JSON type: {other}"
        ))),
    }
}

/// Turn a non-2xx response into a [`SourceError`], reading the JSON error body
/// where present. A `404` (or an `error` naming a missing/invalid product) is an
/// unknown symbol; a `429` is a rate limit; anything else is a generic `Http`.
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
    if code == 429 {
        return SourceError::RateLimited {
            retry_after_ms: retry_after_ms.unwrap_or(0),
        };
    }
    let err = serde_json::from_str::<ErrorBody>(&body).ok();
    let error_code = err.as_ref().and_then(|e| e.error.as_deref());
    if code == 404 || matches!(error_code, Some("NOT_FOUND" | "INVALID_ARGUMENT")) {
        let msg = err
            .as_ref()
            .and_then(|e| e.message.clone().or_else(|| e.error.clone()))
            .unwrap_or_else(|| body.clone());
        return SourceError::UnknownSymbol(msg);
    }
    SourceError::Http { status: code, body }
}

/// `GET .../candles` returns `{ "candles": [ ... ] }`.
#[derive(Deserialize)]
struct CandlesEnvelope {
    #[serde(default)]
    candles: Vec<CandleRow>,
}

/// One candle object. Every field is a JSON string; the numeric parse is
/// tolerant of bare numbers so a mock server can return typed values.
#[derive(Deserialize)]
struct CandleRow {
    start: serde_json::Value,
    low: serde_json::Value,
    high: serde_json::Value,
    open: serde_json::Value,
    close: serde_json::Value,
    volume: serde_json::Value,
}

/// `GET .../market/products` returns `{ "products": [ ... ] }`.
#[derive(Deserialize)]
struct Products {
    #[serde(default)]
    products: Vec<Product>,
}

/// The subset of a product this crate reads — its id, trading state, and type.
#[derive(Deserialize)]
struct Product {
    product_id: String,
    #[serde(default)]
    status: Option<String>,
    #[serde(default)]
    trading_disabled: bool,
    #[serde(default)]
    product_type: Option<String>,
}

/// Coinbase's error body: `{ "error", "error_details", "message" }`.
#[derive(Deserialize)]
struct ErrorBody {
    #[serde(default)]
    error: Option<String>,
    #[serde(default)]
    message: Option<String>,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn interval_tokens_map_correctly() {
        assert_eq!(
            interval_to_token(Interval::Minute(1)).unwrap(),
            "ONE_MINUTE"
        );
        assert_eq!(
            interval_to_token(Interval::Minute(30)).unwrap(),
            "THIRTY_MINUTE"
        );
        assert_eq!(interval_to_token(Interval::Hour(1)).unwrap(), "ONE_HOUR");
        assert_eq!(interval_to_token(Interval::Hour(6)).unwrap(), "SIX_HOUR");
        assert_eq!(interval_to_token(Interval::Day(1)).unwrap(), "ONE_DAY");
    }

    #[test]
    fn unsupported_intervals_reject() {
        for iv in [
            Interval::Minute(3),
            Interval::Hour(4),
            Interval::Hour(12),
            Interval::Day(2),
            Interval::Week(1),
            Interval::Month(1),
        ] {
            assert!(matches!(
                interval_to_token(iv),
                Err(SourceError::UnsupportedInterval(_))
            ));
        }
    }

    #[test]
    fn decode_row_parses_string_fields_and_scales_seconds_to_millis() {
        // Coinbase order is [start, low, high, open, close, volume], all strings.
        let row: CandleRow = serde_json::from_value(serde_json::json!({
            "start": "1700000000",
            "low": "26950.10",
            "high": "27100.00",
            "open": "27000.50",
            "close": "27050.75",
            "volume": "12.345"
        }))
        .unwrap();
        let atom = decode_row(&row).unwrap();
        assert_eq!(atom.time, Some(Timestamp(1_700_000_000_000)));
        let c = atom.candle.unwrap();
        assert_eq!(c.open, 27000.50);
        assert_eq!(c.high, 27100.00);
        assert_eq!(c.low, 26950.10);
        assert_eq!(c.close, 27050.75);
        assert_eq!(c.volume, 12.345);
        // Coinbase candles carry no overlay side channel.
        assert!(atom.overlays.is_none());
    }

    #[test]
    fn decode_row_tolerates_bare_numbers() {
        let row: CandleRow = serde_json::from_value(serde_json::json!({
            "start": 1_700_000_000_i64,
            "low": 0.5, "high": 2.0, "open": 1.0, "close": 1.5, "volume": 10.0
        }))
        .unwrap();
        let atom = decode_row(&row).unwrap();
        assert_eq!(atom.candle.unwrap().close, 1.5);
        assert_eq!(atom.time, Some(Timestamp(1_700_000_000_000)));
    }
}
