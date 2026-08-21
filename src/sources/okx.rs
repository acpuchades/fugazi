//! OKX candlesticks-endpoint implementation of [`SeriesSource`].
//!
//! Fetches OHLCV bars from `GET /api/v5/market/history-candles`, the endpoint
//! that serves the full history (the sibling `/market/candles` only reaches
//! back ~1440 bars). OKX returns each page **newest-first**, so this client
//! paginates *backward*: it walks the `after` cursor from `until` toward
//! `since`, one `max_per_request` chunk at a time, then sorts the accumulated
//! atoms ascending before returning. Between pages the client sleeps
//! `min_delay_between_requests`, staying under OKX's 20-request-per-2-second
//! budget for the history endpoint.
//!
//! Unlike Binance, OKX reports success and failure alike with HTTP `200` and a
//! string `code` in the JSON envelope (`"0"` = success). Errors are mapped
//! into [`SourceError`]:
//!
//! * HTTP `429` or body `code` `"50011"`/`"50013"` → [`SourceError::RateLimited`]
//!   (the `Retry-After` header, if present, is echoed back as milliseconds).
//! * Body `code` `"51001"` (instrument does not exist) → [`SourceError::UnknownSymbol`].
//! * Any other non-`"0"` code, or non-2xx HTTP → [`SourceError::Http`].
//! * JSON that doesn't match the expected shape → [`SourceError::Decode`].

use std::future::Future;
use std::sync::{Arc, OnceLock};
use std::time::Duration;

use serde::Deserialize;

use crate::types::{Atom, Candle, OverlayInfo, OverlayValue, Real, Schema};

use super::{Interval, SeriesSource, SourceError, Timestamp};

const DEFAULT_BASE_URL: &str = "https://www.okx.com";
// `history-candles` caps a page at 100 rows.
const DEFAULT_MAX_PER_REQUEST: usize = 100;
const DEFAULT_MIN_DELAY_MS: u64 = 120;

/// The extra candle fields OKX returns beyond OHLCV, exposed as `Real` overlay
/// columns on every atom. `vol_ccy` is the volume in the quote currency for
/// spot (contract count for derivatives); `quote_volume` is the volume in quote
/// currency (`volCcyQuote`), so a strategy or `--overlay` spec reads them via
/// `!get { key: quote_volume }` etc. Ordering matches the candle row's field
/// indexes (6, 7) so the decode step feeds `OverlayInfo::new` in schema order.
pub fn okx_schema() -> &'static Arc<Schema> {
    static SCHEMA: OnceLock<Arc<Schema>> = OnceLock::new();
    SCHEMA.get_or_init(|| {
        let mut b = Schema::builder();
        b.add_real("vol_ccy");
        b.add_real("quote_volume");
        b.finish()
    })
}

/// An OKX candlesticks client.
///
/// Cheap to clone (the inner [`reqwest::Client`] is `Arc`-backed).
#[derive(Debug, Clone)]
pub struct Okx {
    client: reqwest::Client,
    base_url: String,
    max_per_request: usize,
    min_delay_between_requests: Duration,
}

impl Default for Okx {
    fn default() -> Self {
        Self::new()
    }
}

impl Okx {
    /// A client pointing at the public OKX endpoint with sensible defaults
    /// (100 candles per page, 120 ms between pages).
    pub fn new() -> Self {
        Self {
            client: reqwest::Client::new(),
            base_url: DEFAULT_BASE_URL.to_string(),
            max_per_request: DEFAULT_MAX_PER_REQUEST,
            min_delay_between_requests: Duration::from_millis(DEFAULT_MIN_DELAY_MS),
        }
    }

    /// Override the API base URL (`https://www.okx.com` by default).
    /// Primarily useful for testing against a local `wiremock` server.
    pub fn with_base_url(mut self, url: impl Into<String>) -> Self {
        self.base_url = url.into();
        self
    }

    /// Override the max candles per HTTP request (default 100, OKX's max for
    /// the history endpoint).
    pub fn with_max_per_request(mut self, n: usize) -> Self {
        self.max_per_request = n.clamp(1, 100);
        self
    }

    /// Override the delay between successive requests (default 120 ms).
    pub fn with_min_delay(mut self, d: Duration) -> Self {
        self.min_delay_between_requests = d;
        self
    }
}

impl SeriesSource for Okx {
    fn name(&self) -> &'static str {
        "okx"
    }

    fn schema(&self) -> Option<Arc<Schema>> {
        Some(okx_schema().clone())
    }

    fn tickers(&self) -> impl Future<Output = Result<Vec<String>, SourceError>> + Send {
        let client = self.client.clone();
        let base_url = self.base_url.clone();
        async move {
            let url = format!(
                "{}/api/v5/public/instruments",
                base_url.trim_end_matches('/')
            );
            let resp = client
                .get(&url)
                .query(&[("instType", "SPOT")])
                .send()
                .await?;
            let status = resp.status();
            if !status.is_success() {
                return Err(map_http_error(resp).await);
            }
            let body: OkxEnvelope<Instrument> = resp
                .json()
                .await
                .map_err(|e| SourceError::Decode(format!("instruments JSON: {e}")))?;
            if body.code != "0" {
                return Err(map_api_error(&body.code, &body.msg));
            }
            let mut out: Vec<String> = body
                .data
                .into_iter()
                .filter(|i| i.state == "live")
                .map(|i| i.inst_id)
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
            let schema = okx_schema().clone();
            let mut out: Vec<Atom> = Vec::new();
            let since_ms = since.0;
            let until_ms = until.map(|t| t.0).unwrap_or(i64::MAX);
            let url = format!(
                "{}/api/v5/market/history-candles",
                base_url.trim_end_matches('/')
            );

            // OKX returns candles newest-first, so we page backward: `after`
            // asks for rows strictly older than the cursor. The first page has
            // no cursor (`None` → most recent bar in `[.. until)`); each later
            // page sets it to the oldest timestamp seen so far. `until` is
            // exclusive and OKX's `after` is exclusive, so passing it verbatim
            // is correct.
            let mut cursor: Option<i64> = until.map(|t| t.0);
            let mut first = true;

            loop {
                if !first {
                    tokio::time::sleep(min_delay).await;
                }
                first = false;

                let mut query: Vec<(&str, String)> = vec![
                    ("instId", symbol.clone()),
                    ("bar", token.to_string()),
                    ("limit", max_per_request.to_string()),
                ];
                if let Some(after) = cursor {
                    query.push(("after", after.to_string()));
                }

                let resp = client.get(&url).query(&query).send().await?;
                let status = resp.status();
                if !status.is_success() {
                    return Err(map_http_error(resp).await);
                }

                let body: OkxEnvelope<serde_json::Value> = resp
                    .json()
                    .await
                    .map_err(|e| SourceError::Decode(format!("candle JSON: {e}")))?;
                if body.code != "0" {
                    return Err(map_api_error(&body.code, &body.msg));
                }

                let page_len = body.data.len();
                if page_len == 0 {
                    break;
                }

                // Rows are newest-first; track the oldest timestamp to advance
                // the cursor and to know when we've reached `since`.
                let mut min_ts = i64::MAX;
                for row in &body.data {
                    let atom = decode_row(row, &schema)?;
                    let ts = atom.time.expect("OKX atoms always carry a time").0;
                    min_ts = min_ts.min(ts);
                    if ts >= since_ms && ts < until_ms {
                        out.push(atom);
                    }
                }

                // A short page means OKX had nothing older in the window.
                if page_len < max_per_request {
                    break;
                }
                // We've paged past the start of the requested window.
                if min_ts <= since_ms {
                    break;
                }
                // Advance the cursor to the oldest bar we saw. Defensive: if it
                // failed to move backward, stop rather than spin.
                if cursor.is_some_and(|c| min_ts >= c) {
                    break;
                }
                cursor = Some(min_ts);
            }

            // Pages arrive newest-first and back-to-front; present ascending.
            out.sort_by_key(|a| a.time.map(|t| t.0).unwrap_or(i64::MIN));
            Ok(out)
        }
    }
}

/// Map an [`Interval`] to OKX's `bar` vocabulary. Day/Week/Month and the 6h/12h
/// bars use the UTC-aligned variants (`1Dutc`, `1Wutc`, …) so bar opens land on
/// real UTC boundaries, matching every other candle provider and the crate's
/// [`floor_to_bucket`](super::floor_to_bucket) convention; OKX's un-suffixed
/// `1D`/`1W`/`1M`/`6H`/`12H` are Hong Kong (UTC+8) aligned. Rejects multiples
/// the exchange doesn't support (e.g. `Minute(7)`).
fn interval_to_token(interval: Interval) -> Result<&'static str, SourceError> {
    let token = match interval {
        Interval::Minute(1) => "1m",
        Interval::Minute(3) => "3m",
        Interval::Minute(5) => "5m",
        Interval::Minute(15) => "15m",
        Interval::Minute(30) => "30m",
        Interval::Hour(1) => "1H",
        Interval::Hour(2) => "2H",
        Interval::Hour(4) => "4H",
        Interval::Hour(6) => "6Hutc",
        Interval::Hour(12) => "12Hutc",
        Interval::Day(1) => "1Dutc",
        Interval::Day(2) => "2Dutc",
        Interval::Day(3) => "3Dutc",
        Interval::Week(1) => "1Wutc",
        Interval::Month(1) => "1Mutc",
        Interval::Month(3) => "3Mutc",
        other => return Err(SourceError::UnsupportedInterval(other)),
    };
    Ok(token)
}

/// Extract one candle row from the API response into an [`Atom`], populating the
/// two OKX extras (quote-currency volume, quote volume) as overlay values in
/// `schema` order.
///
/// A row with only the six leading fields (`[ts, o, h, l, c, vol]`, a minimal
/// mock) is still accepted: the missing extras become `Real::NAN`, so
/// downstream `!get { key }` consumers see a defined-but-empty column rather
/// than a hard error.
fn decode_row(row: &serde_json::Value, schema: &Arc<Schema>) -> Result<Atom, SourceError> {
    let arr = row
        .as_array()
        .ok_or_else(|| SourceError::Decode("candle is not a JSON array".into()))?;
    if arr.len() < 6 {
        return Err(SourceError::Decode(format!(
            "candle row has {} fields, expected at least 6",
            arr.len()
        )));
    }
    let open_time = parse_i64_str(&arr[0], "ts")?;
    let open = parse_num_str(&arr[1], "open")?;
    let high = parse_num_str(&arr[2], "high")?;
    let low = parse_num_str(&arr[3], "low")?;
    let close = parse_num_str(&arr[4], "close")?;
    let volume = parse_num_str(&arr[5], "volume")?;
    // Extras land at indexes 6 (volCcy) and 7 (volCcyQuote); index 8 is the
    // `confirm` flag. Missing or malformed → NaN, matching the schema's Real cell.
    let vol_ccy = arr
        .get(6)
        .map(|v| parse_num_str(v, "vol_ccy").unwrap_or(Real::NAN))
        .unwrap_or(Real::NAN);
    let quote_volume = arr
        .get(7)
        .map(|v| parse_num_str(v, "quote_volume").unwrap_or(Real::NAN))
        .unwrap_or(Real::NAN);
    let overlays = OverlayInfo::new(
        schema.clone(),
        vec![
            OverlayValue::Real(vol_ccy),
            OverlayValue::Real(quote_volume),
        ],
    );
    Ok(Atom::with_overlays_and_time(
        Candle::new(open, high, low, close, volume),
        overlays,
        Timestamp(open_time),
    ))
}

/// OKX returns every candle number — the timestamp included — as a JSON string.
/// Also accept a bare JSON number, so a mock server that returns typed numbers
/// still works.
fn parse_num_str(v: &serde_json::Value, field: &str) -> Result<f64, SourceError> {
    match v {
        serde_json::Value::String(s) => s
            .parse::<f64>()
            .map_err(|e| SourceError::Decode(format!("candle `{field}` = {s:?}: {e}"))),
        serde_json::Value::Number(n) => n
            .as_f64()
            .ok_or_else(|| SourceError::Decode(format!("candle `{field}` is not finite"))),
        other => Err(SourceError::Decode(format!(
            "candle `{field}` has unexpected JSON type: {other}"
        ))),
    }
}

/// The timestamp column, as an integer. String (OKX's real shape) or bare
/// number (a mock) both decode.
fn parse_i64_str(v: &serde_json::Value, field: &str) -> Result<i64, SourceError> {
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

/// Turn a non-2xx response into a [`SourceError`]. OKX rarely uses HTTP status
/// for application errors (it prefers the body `code`), but a `429` or a raw
/// gateway error can still arrive this way.
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
    SourceError::Http { status: code, body }
}

/// Map an OKX application error `code` (a decimal string) to the most specific
/// [`SourceError`] variant. `51001` is an unknown instrument; `50011`/`50013`
/// are the rate-limit codes; anything else surfaces as a generic `Http` error
/// carrying the code and message.
fn map_api_error(code: &str, msg: &str) -> SourceError {
    match code {
        "51001" => SourceError::UnknownSymbol(msg.to_string()),
        "50011" | "50013" => SourceError::RateLimited { retry_after_ms: 0 },
        _ => SourceError::Http {
            status: 200,
            body: format!("OKX code {code}: {msg}"),
        },
    }
}

/// OKX wraps every response in a `{code, msg, data}` envelope; `data` is the
/// only part that varies by endpoint, so it is generic here.
#[derive(Deserialize)]
struct OkxEnvelope<T> {
    code: String,
    #[serde(default)]
    msg: String,
    #[serde(default = "Vec::new")]
    data: Vec<T>,
}

/// The subset of `/api/v5/public/instruments` this crate reads — only the
/// symbol vocabulary and its trading state.
#[derive(Deserialize)]
struct Instrument {
    #[serde(rename = "instId")]
    inst_id: String,
    #[serde(default)]
    state: String,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn interval_tokens_map_correctly() {
        assert_eq!(interval_to_token(Interval::Minute(1)).unwrap(), "1m");
        assert_eq!(interval_to_token(Interval::Hour(4)).unwrap(), "4H");
        assert_eq!(interval_to_token(Interval::Hour(6)).unwrap(), "6Hutc");
        assert_eq!(interval_to_token(Interval::Day(1)).unwrap(), "1Dutc");
        assert_eq!(interval_to_token(Interval::Week(1)).unwrap(), "1Wutc");
        assert_eq!(interval_to_token(Interval::Month(1)).unwrap(), "1Mutc");
    }

    #[test]
    fn unsupported_interval_multiples_reject() {
        assert!(matches!(
            interval_to_token(Interval::Minute(7)),
            Err(SourceError::UnsupportedInterval(_))
        ));
        assert!(matches!(
            interval_to_token(Interval::Day(5)),
            Err(SourceError::UnsupportedInterval(_))
        ));
    }

    #[test]
    fn decode_row_parses_string_numbers_and_extras() {
        // OKX ships every field as a string, newest-first, with a trailing
        // `confirm` flag: [ts, o, h, l, c, vol, volCcy, volCcyQuote, confirm].
        let row = serde_json::json!([
            "1700000000000",
            "27000.50",
            "27100.00",
            "26950.10",
            "27050.75",
            "12.345",
            "334000.00", // volCcy
            "333900.00", // volCcyQuote
            "1"          // confirm
        ]);
        let schema = okx_schema().clone();
        let atom = decode_row(&row, &schema).unwrap();
        assert_eq!(atom.time, Some(Timestamp(1_700_000_000_000)));
        assert_eq!(atom.candle.unwrap().open, 27000.50);
        assert_eq!(atom.candle.unwrap().high, 27100.00);
        assert_eq!(atom.candle.unwrap().low, 26950.10);
        assert_eq!(atom.candle.unwrap().close, 27050.75);
        assert_eq!(atom.candle.unwrap().volume, 12.345);
        let overlays = atom.overlays.expect("OKX atoms carry overlays");
        assert_eq!(
            overlays.get_by_key("vol_ccy"),
            Some(&OverlayValue::Real(334000.00))
        );
        assert_eq!(
            overlays.get_by_key("quote_volume"),
            Some(&OverlayValue::Real(333900.00))
        );
    }

    #[test]
    fn decode_row_tolerates_bare_numbers_and_missing_extras() {
        // A mock server may return numbers un-stringified with only OHLCV;
        // extras collapse to NaN, atom is still built.
        let row = serde_json::json!([1_700_000_000_000_i64, 1.0, 2.0, 0.5, 1.5, 10.0]);
        let schema = okx_schema().clone();
        let atom = decode_row(&row, &schema).unwrap();
        assert_eq!(atom.candle.unwrap().close, 1.5);
        let overlays = atom.overlays.expect("OKX atoms carry overlays");
        match overlays.get_by_key("quote_volume") {
            Some(OverlayValue::Real(v)) => assert!(v.is_nan()),
            other => panic!("expected NaN, got {other:?}"),
        }
    }

    #[test]
    fn api_error_codes_map_to_variants() {
        assert!(matches!(
            map_api_error("51001", "Instrument ID does not exist"),
            SourceError::UnknownSymbol(_)
        ));
        assert!(matches!(
            map_api_error("50011", "Rate limit reached"),
            SourceError::RateLimited { .. }
        ));
        assert!(matches!(
            map_api_error("50000", "some other error"),
            SourceError::Http { status: 200, .. }
        ));
    }
}
