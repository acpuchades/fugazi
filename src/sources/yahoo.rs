//! Yahoo Finance chart-endpoint implementation of [`CandleSource`].
//!
//! Fetches OHLCV bars from `GET /v8/finance/chart/{symbol}`. Unlike Binance,
//! Yahoo returns the whole `[period1, period2]` window in one response, so
//! there is no pagination loop — one request per `candles()` call. Timestamps
//! in the response are Unix **seconds**; they are scaled to millisecond
//! [`Timestamp`]s on decode so the returned data matches the trait's ABI.
//!
//! Errors are mapped into [`SourceError`]:
//!
//! * HTTP `429` → [`SourceError::RateLimited`] (the `Retry-After` header, if
//!   present, is echoed back as milliseconds).
//! * Response body `chart.error.code == "Not Found"` → [`SourceError::UnknownSymbol`].
//! * Any other non-2xx → [`SourceError::Http`].
//! * JSON that doesn't match the expected shape → [`SourceError::Decode`].
//!
//! Yahoo's chart endpoint tends to reject requests without a `User-Agent`
//! header, so the client sets a plain identifier by default (overridable via
//! [`Yahoo::with_user_agent`]).

use std::future::Future;

use serde::Deserialize;

use crate::types::Candle;

use super::{CandleSource, Interval, SourceError, TimedCandle, Timestamp};

const DEFAULT_BASE_URL: &str = "https://query1.finance.yahoo.com";
const DEFAULT_USER_AGENT: &str = "Mozilla/5.0 (compatible; fugazi)";

/// A Yahoo Finance chart-API client.
///
/// Cheap to clone (the inner [`reqwest::Client`] is `Arc`-backed).
#[derive(Debug, Clone)]
pub struct Yahoo {
    client: reqwest::Client,
    base_url: String,
    user_agent: String,
}

impl Default for Yahoo {
    fn default() -> Self {
        Self::new()
    }
}

impl Yahoo {
    /// A client pointing at the public Yahoo Finance endpoint with a plain
    /// default User-Agent header.
    pub fn new() -> Self {
        Self {
            client: reqwest::Client::new(),
            base_url: DEFAULT_BASE_URL.to_string(),
            user_agent: DEFAULT_USER_AGENT.to_string(),
        }
    }

    /// Override the API base URL (`https://query1.finance.yahoo.com` by default).
    /// Primarily useful for testing against a local `wiremock` server.
    pub fn with_base_url(mut self, url: impl Into<String>) -> Self {
        self.base_url = url.into();
        self
    }

    /// Override the `User-Agent` header sent with each request.
    pub fn with_user_agent(mut self, ua: impl Into<String>) -> Self {
        self.user_agent = ua.into();
        self
    }
}

impl CandleSource for Yahoo {
    fn name(&self) -> &'static str {
        "yfinance"
    }

    fn candles(
        &self,
        symbol: &str,
        interval: Interval,
        since: Timestamp,
        until: Option<Timestamp>,
    ) -> impl Future<Output = Result<Vec<TimedCandle>, SourceError>> + Send {
        // Own the strings so the returned future doesn't borrow the caller.
        let symbol = symbol.to_string();
        let client = self.client.clone();
        let base_url = self.base_url.clone();
        let user_agent = self.user_agent.clone();
        async move {
            let token = interval_to_token(interval)?;
            let period1 = (since.0 / 1_000).max(0);
            // Trait treats `until` as exclusive; Yahoo's `period2` is inclusive
            // of `openTime == period2`, so subtract one second when provided.
            let period2 = match until {
                Some(t) => (t.0 / 1_000).saturating_sub(1).max(0),
                None => (Timestamp::now().0 / 1_000).max(0),
            };
            if period2 < period1 {
                return Ok(Vec::new());
            }

            let url = format!(
                "{}/v8/finance/chart/{}",
                base_url.trim_end_matches('/'),
                percent_encode_path_segment(&symbol),
            );

            let query = [
                ("interval", token.to_string()),
                ("period1", period1.to_string()),
                ("period2", period2.to_string()),
                ("includePrePost", "false".to_string()),
                ("events", "div,split".to_string()),
            ];

            let resp = client
                .get(&url)
                .header(reqwest::header::USER_AGENT, &user_agent)
                .query(&query)
                .send()
                .await?;
            let status = resp.status();
            if !status.is_success() {
                return Err(map_http_error(resp).await);
            }

            let body: ChartResponse = resp
                .json()
                .await
                .map_err(|e| SourceError::Decode(format!("chart JSON: {e}")))?;
            if let Some(err) = body.chart.error {
                return Err(map_chart_error(err));
            }
            let result = body
                .chart
                .result
                .and_then(|mut r| r.pop())
                .ok_or_else(|| SourceError::Decode("chart.result missing".into()))?;

            decode_result(result)
        }
    }
}

/// Map an [`Interval`] to Yahoo's token vocabulary. Rejects multiples the
/// provider doesn't support (e.g. `Minute(7)` or `Day(3)`).
fn interval_to_token(interval: Interval) -> Result<&'static str, SourceError> {
    let token = match interval {
        Interval::Minute(1) => "1m",
        Interval::Minute(2) => "2m",
        Interval::Minute(5) => "5m",
        Interval::Minute(15) => "15m",
        Interval::Minute(30) => "30m",
        Interval::Minute(60) => "60m",
        Interval::Minute(90) => "90m",
        Interval::Hour(1) => "1h",
        Interval::Day(1) => "1d",
        Interval::Day(5) => "5d",
        Interval::Week(1) => "1wk",
        Interval::Month(1) => "1mo",
        Interval::Month(3) => "3mo",
        other => return Err(SourceError::UnsupportedInterval(other)),
    };
    Ok(token)
}

/// Decode the single-element `chart.result` payload into a list of
/// [`TimedCandle`]s. Skips bars where any of `open`/`high`/`low`/`close`/`volume`
/// is null (Yahoo emits nulls for scheduled bars that never printed, e.g. a
/// mid-session halt).
fn decode_result(result: ChartResult) -> Result<Vec<TimedCandle>, SourceError> {
    let quote = result
        .indicators
        .quote
        .into_iter()
        .next()
        .ok_or_else(|| SourceError::Decode("indicators.quote empty".into()))?;
    let times = result.timestamp;
    let n = times.len();
    if quote.open.len() != n
        || quote.high.len() != n
        || quote.low.len() != n
        || quote.close.len() != n
        || quote.volume.len() != n
    {
        return Err(SourceError::Decode(format!(
            "quote array length mismatch: times={n}, open={}, high={}, low={}, close={}, volume={}",
            quote.open.len(),
            quote.high.len(),
            quote.low.len(),
            quote.close.len(),
            quote.volume.len(),
        )));
    }
    let mut out = Vec::with_capacity(n);
    for (i, &t) in times.iter().enumerate() {
        let (Some(o), Some(h), Some(l), Some(c), Some(v)) = (
            quote.open[i],
            quote.high[i],
            quote.low[i],
            quote.close[i],
            quote.volume[i],
        ) else {
            continue;
        };
        out.push(TimedCandle {
            time: Timestamp(t.saturating_mul(1_000)),
            candle: Candle::new(o, h, l, c, v),
        });
    }
    Ok(out)
}

/// Turn a non-2xx response into a [`SourceError`], preferring the specific
/// variants (`RateLimited`, `UnknownSymbol`) over the generic `Http`. Yahoo
/// echoes an error object in the JSON body even on 4xx, so we try that first.
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
    if let Ok(parsed) = serde_json::from_str::<ChartResponse>(&body)
        && let Some(err) = parsed.chart.error
    {
        return map_chart_error(err);
    }
    SourceError::Http { status: code, body }
}

fn map_chart_error(err: YahooError) -> SourceError {
    match err.code.as_str() {
        "Not Found" => SourceError::UnknownSymbol(err.description),
        _ => SourceError::Decode(format!("yahoo error {}: {}", err.code, err.description)),
    }
}

/// Percent-encode a single URL path segment. Yahoo symbols include characters
/// that need escaping in the path — indices are prefixed with `^` (e.g.
/// `^GSPC`) and share classes use `.` (e.g. `BRK.B`) — so this keeps the
/// unreserved characters raw (RFC 3986) and percent-encodes the rest.
fn percent_encode_path_segment(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for &b in s.as_bytes() {
        match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => out.push(b as char),
            _ => {
                out.push('%');
                out.push_str(&format!("{b:02X}"));
            }
        }
    }
    out
}

#[derive(Deserialize)]
struct ChartResponse {
    chart: Chart,
}

#[derive(Deserialize)]
struct Chart {
    #[serde(default)]
    result: Option<Vec<ChartResult>>,
    #[serde(default)]
    error: Option<YahooError>,
}

#[derive(Deserialize)]
struct ChartResult {
    #[serde(default)]
    timestamp: Vec<i64>,
    indicators: ChartIndicators,
}

#[derive(Deserialize)]
struct ChartIndicators {
    quote: Vec<ChartQuote>,
}

#[derive(Deserialize)]
struct ChartQuote {
    #[serde(default)]
    open: Vec<Option<f64>>,
    #[serde(default)]
    high: Vec<Option<f64>>,
    #[serde(default)]
    low: Vec<Option<f64>>,
    #[serde(default)]
    close: Vec<Option<f64>>,
    #[serde(default)]
    volume: Vec<Option<f64>>,
}

#[derive(Deserialize, Debug)]
struct YahooError {
    code: String,
    description: String,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn interval_tokens_map_correctly() {
        assert_eq!(interval_to_token(Interval::Minute(1)).unwrap(), "1m");
        assert_eq!(interval_to_token(Interval::Minute(60)).unwrap(), "60m");
        assert_eq!(interval_to_token(Interval::Hour(1)).unwrap(), "1h");
        assert_eq!(interval_to_token(Interval::Day(1)).unwrap(), "1d");
        assert_eq!(interval_to_token(Interval::Week(1)).unwrap(), "1wk");
        assert_eq!(interval_to_token(Interval::Month(1)).unwrap(), "1mo");
        assert_eq!(interval_to_token(Interval::Month(3)).unwrap(), "3mo");
    }

    #[test]
    fn unsupported_interval_multiples_reject() {
        assert!(matches!(
            interval_to_token(Interval::Minute(7)),
            Err(SourceError::UnsupportedInterval(_))
        ));
        assert!(matches!(
            interval_to_token(Interval::Day(3)),
            Err(SourceError::UnsupportedInterval(_))
        ));
        assert!(matches!(
            interval_to_token(Interval::Hour(4)),
            Err(SourceError::UnsupportedInterval(_))
        ));
    }

    #[test]
    fn percent_encoding_preserves_unreserved_and_escapes_the_rest() {
        assert_eq!(percent_encode_path_segment("SPY"), "SPY");
        assert_eq!(percent_encode_path_segment("BRK.B"), "BRK.B");
        assert_eq!(percent_encode_path_segment("^GSPC"), "%5EGSPC");
        assert_eq!(percent_encode_path_segment("EUR=X"), "EUR%3DX");
    }

    #[test]
    fn decode_result_builds_candles_and_scales_seconds_to_millis() {
        let result = ChartResult {
            timestamp: vec![1_704_067_200, 1_704_153_600],
            indicators: ChartIndicators {
                quote: vec![ChartQuote {
                    open: vec![Some(100.0), Some(101.0)],
                    high: vec![Some(102.0), Some(103.0)],
                    low: vec![Some(99.5), Some(100.5)],
                    close: vec![Some(101.5), Some(102.5)],
                    volume: vec![Some(1_000.0), Some(1_200.0)],
                }],
            },
        };
        let out = decode_result(result).unwrap();
        assert_eq!(out.len(), 2);
        assert_eq!(out[0].time, Timestamp(1_704_067_200_000));
        assert_eq!(out[0].candle.open, 100.0);
        assert_eq!(out[0].candle.close, 101.5);
        assert_eq!(out[1].time, Timestamp(1_704_153_600_000));
        assert_eq!(out[1].candle.volume, 1_200.0);
    }

    #[test]
    fn decode_result_skips_bars_with_null_fields() {
        let result = ChartResult {
            timestamp: vec![1_704_067_200, 1_704_153_600, 1_704_240_000],
            indicators: ChartIndicators {
                quote: vec![ChartQuote {
                    open: vec![Some(100.0), None, Some(102.0)],
                    high: vec![Some(102.0), Some(103.0), Some(104.0)],
                    low: vec![Some(99.5), Some(100.5), Some(101.5)],
                    close: vec![Some(101.5), Some(102.5), Some(103.5)],
                    volume: vec![Some(1_000.0), Some(1_200.0), Some(1_100.0)],
                }],
            },
        };
        let out = decode_result(result).unwrap();
        assert_eq!(out.len(), 2);
        assert_eq!(out[0].time, Timestamp(1_704_067_200_000));
        assert_eq!(out[1].time, Timestamp(1_704_240_000_000));
    }

    #[tokio::test]
    async fn tickers_reports_unsupported_by_default() {
        let err = Yahoo::new().tickers().await.expect_err("expected Unsupported");
        match err {
            SourceError::Unsupported { operation, provider } => {
                assert_eq!(provider, "yfinance");
                assert_eq!(operation, "ticker enumeration");
            }
            other => panic!("expected Unsupported, got {other:?}"),
        }
    }

    #[test]
    fn decode_result_rejects_length_mismatch() {
        let result = ChartResult {
            timestamp: vec![1_704_067_200, 1_704_153_600],
            indicators: ChartIndicators {
                quote: vec![ChartQuote {
                    open: vec![Some(100.0)],
                    high: vec![Some(102.0), Some(103.0)],
                    low: vec![Some(99.5), Some(100.5)],
                    close: vec![Some(101.5), Some(102.5)],
                    volume: vec![Some(1_000.0), Some(1_200.0)],
                }],
            },
        };
        assert!(matches!(
            decode_result(result),
            Err(SourceError::Decode(_))
        ));
    }
}
