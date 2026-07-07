//! Binance klines-endpoint implementation of [`CandleSource`].
//!
//! Fetches OHLCV bars from `GET /api/v3/klines`, auto-paginating in
//! `max_per_request` chunks until either the API returns a short page or the
//! cursor crosses `until`. Between pages the client sleeps
//! `min_delay_between_requests`, well under Binance's per-minute weight
//! budget for the default (weight-2 for `limit≤1000`) call.
//!
//! Errors are mapped into [`SourceError`]:
//!
//! * HTTP `429` / `418` → [`SourceError::RateLimited`] (the `Retry-After`
//!   header, if present, is echoed back as milliseconds).
//! * Response body `{"code":-1121,…}` → [`SourceError::UnknownSymbol`].
//! * Any other non-2xx → [`SourceError::Http`].
//! * JSON that doesn't match the expected shape → [`SourceError::Decode`].

use std::future::Future;
use std::sync::{Arc, OnceLock};
use std::time::Duration;

use serde::Deserialize;

use crate::types::{Atom, Candle, OverlayInfo, OverlayValue, Real, Schema};

use super::{CandleSource, Interval, SourceError, Timestamp};

const DEFAULT_BASE_URL: &str = "https://api.binance.com";
const DEFAULT_MAX_PER_REQUEST: usize = 1000;
const DEFAULT_MIN_DELAY_MS: u64 = 100;

/// The extra kline fields Binance returns beyond OHLCV, exposed as `Real`
/// overlay columns on every atom. The names follow snake_case; a strategy
/// or `--overlay` spec reads them via `!get { key: quote_volume }` etc.
/// Ordering matches the kline row's field indexes (7, 8, 9, 10) so the
/// decode step feeds `OverlayInfo::new` in schema order.
pub fn binance_schema() -> &'static Arc<Schema> {
    static SCHEMA: OnceLock<Arc<Schema>> = OnceLock::new();
    SCHEMA.get_or_init(|| {
        let mut b = Schema::builder();
        b.add_real("quote_volume");
        b.add_real("n_trades");
        b.add_real("taker_buy_base_volume");
        b.add_real("taker_buy_quote_volume");
        b.finish()
    })
}

/// A Binance klines client.
///
/// Cheap to clone (the inner [`reqwest::Client`] is `Arc`-backed).
#[derive(Debug, Clone)]
pub struct Binance {
    client: reqwest::Client,
    base_url: String,
    max_per_request: usize,
    min_delay_between_requests: Duration,
}

impl Default for Binance {
    fn default() -> Self {
        Self::new()
    }
}

impl Binance {
    /// A client pointing at the public Binance endpoint with sensible defaults
    /// (1000 klines per page, 100 ms between pages).
    pub fn new() -> Self {
        Self {
            client: reqwest::Client::new(),
            base_url: DEFAULT_BASE_URL.to_string(),
            max_per_request: DEFAULT_MAX_PER_REQUEST,
            min_delay_between_requests: Duration::from_millis(DEFAULT_MIN_DELAY_MS),
        }
    }

    /// Override the API base URL (`https://api.binance.com` by default).
    /// Primarily useful for testing against a local `wiremock` server.
    pub fn with_base_url(mut self, url: impl Into<String>) -> Self {
        self.base_url = url.into();
        self
    }

    /// Override the max klines per HTTP request (default 1000, Binance's max).
    pub fn with_max_per_request(mut self, n: usize) -> Self {
        self.max_per_request = n.max(1);
        self
    }

    /// Override the delay between successive requests (default 100 ms).
    pub fn with_min_delay(mut self, d: Duration) -> Self {
        self.min_delay_between_requests = d;
        self
    }
}

impl CandleSource for Binance {
    fn name(&self) -> &'static str {
        "binance"
    }

    fn tickers(&self) -> impl Future<Output = Result<Vec<String>, SourceError>> + Send {
        let client = self.client.clone();
        let base_url = self.base_url.clone();
        async move {
            let url = format!("{}/api/v3/exchangeInfo", base_url.trim_end_matches('/'));
            let resp = client.get(&url).send().await?;
            let status = resp.status();
            if !status.is_success() {
                return Err(map_http_error(resp).await);
            }
            let body: ExchangeInfo = resp
                .json()
                .await
                .map_err(|e| SourceError::Decode(format!("exchangeInfo JSON: {e}")))?;
            let mut out: Vec<String> = body
                .symbols
                .into_iter()
                .filter(|s| s.status == "TRADING")
                .map(|s| s.symbol)
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
            let schema = binance_schema().clone();
            let mut out: Vec<Atom> = Vec::new();
            let mut cursor = since.0;
            let until_ms = until.map(|t| t.0).unwrap_or(i64::MAX);
            let mut first = true;
            let url = format!("{}/api/v3/klines", base_url.trim_end_matches('/'));

            while cursor < until_ms {
                if !first {
                    tokio::time::sleep(min_delay).await;
                }
                first = false;

                let mut query: Vec<(&str, String)> = vec![
                    ("symbol", symbol.clone()),
                    ("interval", token.to_string()),
                    ("startTime", cursor.to_string()),
                    ("limit", max_per_request.to_string()),
                ];
                if let Some(end) = until {
                    // The trait treats `until` as exclusive; Binance's endTime is
                    // inclusive of `openTime == endTime`, so subtract one millisecond.
                    query.push(("endTime", end.0.saturating_sub(1).to_string()));
                }

                let resp = client.get(&url).query(&query).send().await?;
                let status = resp.status();
                if !status.is_success() {
                    return Err(map_http_error(resp).await);
                }

                let rows: Vec<serde_json::Value> = resp
                    .json()
                    .await
                    .map_err(|e| SourceError::Decode(format!("kline JSON: {e}")))?;
                let page_len = rows.len();
                if page_len == 0 {
                    break;
                }

                for row in rows {
                    out.push(decode_row(&row, &schema)?);
                }

                // Advance the cursor past the last kline we received.
                let last_open = out
                    .last()
                    .expect("just pushed at least one row")
                    .time
                    .expect("Binance atoms always carry a time")
                    .0;
                let next_cursor = last_open.saturating_add(1);
                if next_cursor <= cursor {
                    // Defensive: an anomaly in the response could stall the loop.
                    break;
                }
                cursor = next_cursor;

                // A short page means Binance had nothing more in the window.
                if page_len < max_per_request {
                    break;
                }
            }

            Ok(out)
        }
    }
}

/// Map an [`Interval`] to Binance's token vocabulary. Rejects multiples the
/// exchange doesn't support (e.g. `Minute(7)`).
fn interval_to_token(interval: Interval) -> Result<&'static str, SourceError> {
    let token = match interval {
        Interval::Minute(1) => "1m",
        Interval::Minute(3) => "3m",
        Interval::Minute(5) => "5m",
        Interval::Minute(15) => "15m",
        Interval::Minute(30) => "30m",
        Interval::Hour(1) => "1h",
        Interval::Hour(2) => "2h",
        Interval::Hour(4) => "4h",
        Interval::Hour(6) => "6h",
        Interval::Hour(8) => "8h",
        Interval::Hour(12) => "12h",
        Interval::Day(1) => "1d",
        Interval::Day(3) => "3d",
        Interval::Week(1) => "1w",
        Interval::Month(1) => "1M",
        other => return Err(SourceError::UnsupportedInterval(other)),
    };
    Ok(token)
}

/// Extract one kline row from the API response into an [`Atom`], populating
/// the four Binance extras (quote asset volume, number of trades, taker-buy
/// base/quote asset volumes) as overlay values in `schema` order.
///
/// A row with only the six OHLCV fields (a minimal mock) is still accepted:
/// the missing extras become `Real::NAN`, so downstream `!get { key }`
/// consumers see a defined-but-empty column rather than a hard error.
fn decode_row(row: &serde_json::Value, schema: &Arc<Schema>) -> Result<Atom, SourceError> {
    let arr = row
        .as_array()
        .ok_or_else(|| SourceError::Decode("kline is not a JSON array".into()))?;
    if arr.len() < 6 {
        return Err(SourceError::Decode(format!(
            "kline row has {} fields, expected at least 6",
            arr.len()
        )));
    }
    let open_time = arr[0]
        .as_i64()
        .ok_or_else(|| SourceError::Decode("kline openTime is not an integer".into()))?;
    let open = parse_num_str(&arr[1], "open")?;
    let high = parse_num_str(&arr[2], "high")?;
    let low = parse_num_str(&arr[3], "low")?;
    let close = parse_num_str(&arr[4], "close")?;
    let volume = parse_num_str(&arr[5], "volume")?;
    // Extras land at indexes 7 / 8 / 9 / 10 (index 6 = closeTime, index 11 =
    // "ignore"). Missing or malformed → NaN, matching the schema's Real cell.
    let quote_volume = arr.get(7).map(|v| parse_num_str(v, "quote_volume").unwrap_or(Real::NAN)).unwrap_or(Real::NAN);
    let n_trades = arr.get(8).and_then(|v| v.as_i64().map(|n| n as Real).or_else(|| v.as_f64())).unwrap_or(Real::NAN);
    let taker_buy_base_volume = arr.get(9).map(|v| parse_num_str(v, "taker_buy_base_volume").unwrap_or(Real::NAN)).unwrap_or(Real::NAN);
    let taker_buy_quote_volume = arr.get(10).map(|v| parse_num_str(v, "taker_buy_quote_volume").unwrap_or(Real::NAN)).unwrap_or(Real::NAN);
    let overlays = OverlayInfo::new(
        schema.clone(),
        vec![
            OverlayValue::Real(quote_volume),
            OverlayValue::Real(n_trades),
            OverlayValue::Real(taker_buy_base_volume),
            OverlayValue::Real(taker_buy_quote_volume),
        ],
    );
    Ok(Atom::with_overlays_and_time(
        Candle::new(open, high, low, close, volume),
        overlays,
        Timestamp(open_time),
    ))
}

/// Binance returns OHLCV numbers as JSON strings. Also accept a bare JSON
/// number, so a mock server that returns typed numbers still works.
fn parse_num_str(v: &serde_json::Value, field: &str) -> Result<f64, SourceError> {
    match v {
        serde_json::Value::String(s) => s
            .parse::<f64>()
            .map_err(|e| SourceError::Decode(format!("kline `{field}` = {s:?}: {e}"))),
        serde_json::Value::Number(n) => n
            .as_f64()
            .ok_or_else(|| SourceError::Decode(format!("kline `{field}` is not finite"))),
        other => Err(SourceError::Decode(format!(
            "kline `{field}` has unexpected JSON type: {other}"
        ))),
    }
}

/// Turn a non-2xx response into a [`SourceError`], preferring the specific
/// variants (`RateLimited`, `UnknownSymbol`) over the generic `Http`.
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

/// The subset of `/api/v3/exchangeInfo` this crate reads. Everything else in
/// the response (rate-limit config, per-symbol filters, precision fields) is
/// ignored — we only need the symbol vocabulary.
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
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn interval_tokens_map_correctly() {
        assert_eq!(interval_to_token(Interval::Minute(1)).unwrap(), "1m");
        assert_eq!(interval_to_token(Interval::Hour(4)).unwrap(), "4h");
        assert_eq!(interval_to_token(Interval::Day(1)).unwrap(), "1d");
        assert_eq!(interval_to_token(Interval::Week(1)).unwrap(), "1w");
        assert_eq!(interval_to_token(Interval::Month(1)).unwrap(), "1M");
    }

    #[test]
    fn unsupported_interval_multiples_reject() {
        assert!(matches!(
            interval_to_token(Interval::Minute(7)),
            Err(SourceError::UnsupportedInterval(_))
        ));
        assert!(matches!(
            interval_to_token(Interval::Day(2)),
            Err(SourceError::UnsupportedInterval(_))
        ));
    }

    #[test]
    fn decode_row_parses_string_numbers_and_extras() {
        let row = serde_json::json!([
            1_700_000_000_000_i64,
            "27000.50",
            "27100.00",
            "26950.10",
            "27050.75",
            "12.345",
            1_700_003_599_999_i64,
            "334000.00", // quote_volume
            42,          // n_trades
            "6.0",       // taker_buy_base_volume
            "162500.00", // taker_buy_quote_volume
            "0"          // ignore
        ]);
        let schema = binance_schema().clone();
        let atom = decode_row(&row, &schema).unwrap();
        assert_eq!(atom.time, Some(Timestamp(1_700_000_000_000)));
        assert_eq!(atom.candle.open, 27000.50);
        assert_eq!(atom.candle.high, 27100.00);
        assert_eq!(atom.candle.low, 26950.10);
        assert_eq!(atom.candle.close, 27050.75);
        assert_eq!(atom.candle.volume, 12.345);
        let overlays = atom.overlays.expect("Binance atoms carry overlays");
        assert_eq!(overlays.get_by_key("quote_volume"), Some(&OverlayValue::Real(334000.00)));
        assert_eq!(overlays.get_by_key("n_trades"), Some(&OverlayValue::Real(42.0)));
        assert_eq!(overlays.get_by_key("taker_buy_base_volume"), Some(&OverlayValue::Real(6.0)));
        assert_eq!(overlays.get_by_key("taker_buy_quote_volume"), Some(&OverlayValue::Real(162500.00)));
    }

    #[test]
    fn decode_row_tolerates_bare_numbers_and_missing_extras() {
        // A mock server may return numbers un-stringified with only OHLCV;
        // extras collapse to NaN, atom is still built.
        let row = serde_json::json!([1_700_000_000_000_i64, 1.0, 2.0, 0.5, 1.5, 10.0]);
        let schema = binance_schema().clone();
        let atom = decode_row(&row, &schema).unwrap();
        assert_eq!(atom.candle.close, 1.5);
        let overlays = atom.overlays.expect("Binance atoms carry overlays");
        match overlays.get_by_key("n_trades") {
            Some(OverlayValue::Real(v)) => assert!(v.is_nan()),
            other => panic!("expected NaN, got {other:?}"),
        }
    }
}
