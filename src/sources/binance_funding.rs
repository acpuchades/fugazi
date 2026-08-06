//! Binance perpetual-futures **funding rate** as an [`OverlaySource`].
//!
//! The funding rate is the periodic payment exchanged between the two sides of
//! a perpetual swap to tether it to spot: a positive rate means longs pay
//! shorts, a negative one the reverse. It is the primary carry signal in crypto
//! — and it is *not* a price, which is exactly why this is an
//! [`OverlaySource`] rather than a [`CandleSource`](super::CandleSource). There
//! is no OHLCV here, so the mistake of feeding a synthesised candle into
//! `!close` or into wallet mark-to-market is unrepresentable.
//!
//! # The host is not the spot host
//!
//! Perpetuals live on `fapi.binance.com`, a different service from spot's
//! `api.binance.com` that [`Binance`](super::Binance) uses — different symbol
//! vocabulary, different listing set. The endpoint here,
//! `GET /fapi/v1/fundingRate`, is public: no key, no signing.
//!
//! # Cadence and aggregation
//!
//! Binance settles funding on a fixed schedule — every 8 hours for most
//! symbols (00:00 / 08:00 / 16:00 UTC), every 4 for a few. Those are *events*,
//! not bars, so a requested cadence coarser than the settlement period covers
//! several of them.
//!
//! **Samples inside a bucket are summed**, because funding is a cost that
//! accrues: three 8-hourly rates inside one day sum to that day's total carry,
//! which is the number a daily-bar strategy wants. (Contrast
//! [`CoinGecko`](super::CoinGecko), whose columns are *levels* — a market cap —
//! and so keep the first sample per bucket instead.) At `[8h]` each bucket
//! holds one settlement and the sum is the identity.
//!
//! This also removes any need to forward-fill: request the cadence you trade
//! and every bar carries its own period's funding.
//!
//! # Columns
//!
//! One column, `funding_rate`. Binance's `markPrice` field on this endpoint is
//! empty on many historical rows, so it is deliberately not exposed rather than
//! shipped as a column that is sometimes silently `NaN`.
//!
//! A trailing average is not baked in either — that is what the overlay
//! calculator is for:
//!
//! ```sh
//! fugazi get binance-funding:BTCUSDT[1d] --from 2022-01-01 -o funding.csv
//! fugazi run @carry.yml -s @btc.csv -s @funding.csv
//! ```
//!
//! ```yaml
//! # in the strategy — the raw column, and an 8-day trailing mean of it
//! enter: !above
//!   source: !sma { source: !get { key: funding_rate }, period: 8 }
//!   level: 0.0
//! ```

use std::collections::BTreeMap;
use std::future::Future;
use std::sync::{Arc, OnceLock};
use std::time::Duration;

use serde::Deserialize;

use crate::types::{OverlayInfo, OverlayValue, Real, Schema};

use super::{Interval, OverlayRow, OverlaySource, SourceError, Timestamp, floor_to_bucket};

/// The USD-margined perpetual-futures host. Distinct from spot's
/// `api.binance.com` — see the [module docs](self).
const DEFAULT_BASE_URL: &str = "https://fapi.binance.com";
/// Binance caps `limit` on this endpoint at 1000 rows.
const DEFAULT_MAX_PER_REQUEST: usize = 1_000;
const DEFAULT_MIN_DELAY_MS: u64 = 100;

/// The overlay column this provider exposes. Read it from a strategy or an
/// `--overlay` spec with `!get { key: funding_rate }`.
pub fn binance_funding_schema() -> &'static Arc<Schema> {
    static SCHEMA: OnceLock<Arc<Schema>> = OnceLock::new();
    SCHEMA.get_or_init(|| {
        let mut b = Schema::builder();
        b.add_real("funding_rate");
        b.finish()
    })
}

/// A Binance perpetual funding-rate client.
///
/// Cheap to clone (the inner [`reqwest::Client`] is `Arc`-backed).
///
/// The `symbol` is a **perpetual contract** symbol (`BTCUSDT`, `ETHUSDT`) —
/// which mostly coincides with the spot vocabulary but is not the same list.
/// Enumerate it with [`OverlaySource::tickers`]
/// (`fugazi list tickers binance-funding`).
#[derive(Debug, Clone)]
pub struct BinanceFunding {
    client: reqwest::Client,
    base_url: String,
    max_per_request: usize,
    min_delay_between_requests: Duration,
}

impl Default for BinanceFunding {
    fn default() -> Self {
        Self::new()
    }
}

impl BinanceFunding {
    /// A client pointing at the public perpetual-futures endpoint.
    pub fn new() -> Self {
        Self {
            client: reqwest::Client::new(),
            base_url: DEFAULT_BASE_URL.to_string(),
            max_per_request: DEFAULT_MAX_PER_REQUEST,
            min_delay_between_requests: Duration::from_millis(DEFAULT_MIN_DELAY_MS),
        }
    }

    /// Override the API base URL (`https://fapi.binance.com` by default).
    /// Primarily useful for testing against a local mock server.
    pub fn with_base_url(mut self, url: impl Into<String>) -> Self {
        self.base_url = url.into();
        self
    }

    /// Rows per request. Binance caps this endpoint at 1000.
    pub fn with_max_per_request(mut self, n: usize) -> Self {
        self.max_per_request = n.max(1);
        self
    }

    /// Minimum delay between paged requests.
    pub fn with_min_delay(mut self, d: Duration) -> Self {
        self.min_delay_between_requests = d;
        self
    }
}

impl OverlaySource for BinanceFunding {
    fn name(&self) -> &'static str {
        "binance-funding"
    }

    fn schema(&self) -> Arc<Schema> {
        binance_funding_schema().clone()
    }

    fn tickers(&self) -> impl Future<Output = Result<Vec<String>, SourceError>> + Send {
        let client = self.client.clone();
        let base_url = self.base_url.clone();
        async move {
            let url = format!("{}/fapi/v1/exchangeInfo", base_url.trim_end_matches('/'));
            let resp = client.get(&url).send().await?;
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
        let max_per_request = self.max_per_request;
        let min_delay = self.min_delay_between_requests;
        async move {
            check_interval(interval)?;
            let schema = binance_funding_schema().clone();
            let url = format!("{}/fapi/v1/fundingRate", base_url.trim_end_matches('/'));
            let until_ms = until.map(|t| t.0).unwrap_or_else(|| Timestamp::now().0);

            // bucket -> summed funding over that bucket.
            let mut buckets: BTreeMap<i64, Real> = BTreeMap::new();
            let mut cursor = since.0;
            let mut first = true;

            while cursor < until_ms {
                if !first {
                    tokio::time::sleep(min_delay).await;
                }
                first = false;

                let query: Vec<(&str, String)> = vec![
                    ("symbol", symbol.clone()),
                    ("startTime", cursor.to_string()),
                    // The trait treats `until` as exclusive; Binance's endTime
                    // is inclusive, so step back one millisecond.
                    ("endTime", until_ms.saturating_sub(1).to_string()),
                    ("limit", max_per_request.to_string()),
                ];

                let resp = client.get(&url).query(&query).send().await?;
                if !resp.status().is_success() {
                    return Err(map_http_error(resp).await);
                }
                let rows: Vec<FundingRow> = resp
                    .json()
                    .await
                    .map_err(|e| SourceError::Decode(format!("fundingRate JSON: {e}")))?;
                let page_len = rows.len();
                if page_len == 0 {
                    break;
                }

                let mut last_time = cursor;
                for row in &rows {
                    let rate: Real = row.funding_rate.parse().map_err(|e| {
                        SourceError::Decode(format!(
                            "fundingRate `fundingRate` = {:?}: {e}",
                            row.funding_rate
                        ))
                    })?;
                    last_time = row.funding_time;
                    if row.funding_time < since.0 || row.funding_time >= until_ms {
                        continue;
                    }
                    // Accrual, not a level: several settlements inside one bar
                    // add up to that bar's total carry.
                    *buckets
                        .entry(floor_to_bucket(row.funding_time, interval))
                        .or_insert(0.0) += rate;
                }

                // A short page means the range is exhausted; otherwise resume
                // just past the last settlement seen. The `<=` guard stops a
                // non-advancing cursor from looping forever.
                if page_len < max_per_request {
                    break;
                }
                let next = last_time.saturating_add(1);
                if next <= cursor {
                    break;
                }
                cursor = next;
            }

            Ok(buckets
                .into_iter()
                .map(|(time, rate)| OverlayRow {
                    time: Timestamp(time),
                    overlays: OverlayInfo::new(schema.clone(), vec![OverlayValue::Real(rate)]),
                })
                .collect())
        }
    }
}

/// Reject cadences that cannot honestly carry a funding series.
///
/// Funding settles every 4–8 hours, so a sub-hourly bucket would be empty on
/// almost every bar — a column of zeros that reads as "no carry" rather than
/// "no data". Hourly and coarser is admitted; `Week`/`Month` only at multiple
/// `1`, matching what [`floor_to_bucket`] can anchor to a real calendar
/// boundary.
fn check_interval(interval: Interval) -> Result<(), SourceError> {
    match interval {
        Interval::Hour(n) if n > 0 => Ok(()),
        Interval::Day(n) if n > 0 => Ok(()),
        Interval::Week(1) | Interval::Month(1) => Ok(()),
        other => Err(SourceError::UnsupportedInterval(other)),
    }
}

/// Turn a non-2xx response into a [`SourceError`], preferring the specific
/// variants (`RateLimited`, `UnknownSymbol`) over the generic `Http`. Same
/// shape as the spot client's mapper; `-1121` is Binance's "invalid symbol".
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

/// One settlement, as returned by `/fapi/v1/fundingRate`. `markPrice` is
/// present in the payload but deliberately not decoded — see the module docs.
#[derive(Deserialize)]
struct FundingRow {
    #[serde(rename = "fundingTime")]
    funding_time: i64,
    #[serde(rename = "fundingRate")]
    funding_rate: String,
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rejects_sub_hourly_cadences() {
        // Funding settles every 4-8h; a 1-minute bucket would be empty almost
        // everywhere and read as "no carry".
        assert!(check_interval(Interval::Minute(1)).is_err());
        assert!(check_interval(Interval::Minute(30)).is_err());
        assert!(check_interval(Interval::Hour(8)).is_ok());
        assert!(check_interval(Interval::Day(1)).is_ok());
        assert!(check_interval(Interval::Week(1)).is_ok());
        assert!(check_interval(Interval::Month(1)).is_ok());
        // Multi-week / multi-month have no honest calendar anchor.
        assert!(check_interval(Interval::Week(2)).is_err());
    }

    #[test]
    fn the_schema_exposes_one_real_column() {
        let schema = binance_funding_schema();
        assert_eq!(schema.len(), 1);
        assert!(schema.index_of("funding_rate").is_some());
    }
}
