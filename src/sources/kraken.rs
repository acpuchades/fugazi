//! Kraken OHLC-endpoint implementation of [`SeriesSource`].
//!
//! Fetches OHLCV bars from `GET /0/public/OHLC`. Two properties of that endpoint
//! shape this client, and both are worth knowing before you point a backtest at
//! it:
//!
//! * **It returns at most 720 committed bars, and `since` cannot reach past
//!   them.** Kraken's `since` filter only moves the window's *start* forward —
//!   it truncates from the front rather than paging backward, so asking for
//!   2017 on daily candles returns the same trailing 720 rows an unfiltered
//!   call does. The reachable history is therefore `720 × interval`: roughly
//!   two years of daily bars, thirty days of hourly, twelve hours of 1-minute.
//!   There is no pagination to write, because a single request already returns
//!   everything the endpoint will ever give. Deeper history needs
//!   `/0/public/Trades` aggregation or Kraken's downloadable OHLCVT dumps,
//!   neither of which this provider implements. `fugazi get` surfaces the
//!   shortfall through its existing "earliest available candle is later than
//!   --since" warning, so a truncated window is reported rather than silently
//!   backtested.
//! * **The final row is the current, still-forming bar.** Kraken always appends
//!   it, and it mutates between calls. The `last` field in the response is the
//!   open time of the last *committed* bar, so this client keeps only rows with
//!   `time <= last` — an unsettled partial bar is exactly the kind of value the
//!   crate's safe-default rule says to wait on rather than emit.
//!
//! Kraken reports application failures with **HTTP 200** and a non-empty
//! `error` array (`["EQuery:Unknown asset pair"]`), so status alone never
//! decides success. Errors map into [`SourceError`]:
//!
//! * `EQuery:Unknown asset pair` → [`SourceError::UnknownSymbol`].
//! * `EAPI:Rate limit exceeded` / `EService:Throttled` → [`SourceError::RateLimited`].
//! * Any other non-empty `error` array → [`SourceError::Http`] with status 200.
//! * HTTP 429 (Kraken fronts the API with Cloudflare, which can throttle above
//!   the application layer) → [`SourceError::RateLimited`]; other non-2xx →
//!   [`SourceError::Http`].
//!
//! Pair naming has no derivable rule: `XBTUSD`, `BTCUSD` and `XXBTZUSD` all
//! resolve to the result key `XXBTZUSD`, while `BTC/USD` echoes its own form.
//! Rather than transform the request, this client takes the single key in
//! `result` that is not `last` — the approach every mature Kraken client uses.

use std::future::Future;
use std::sync::{Arc, OnceLock};

use serde::Deserialize;

use crate::types::{Atom, Candle, OverlayInfo, OverlayValue, Real, Schema};

use super::{Interval, SeriesSource, SourceError, Timestamp};

const DEFAULT_BASE_URL: &str = "https://api.kraken.com";

/// The most committed bars `/0/public/OHLC` will return, whatever `since` asks
/// for. Documented by Kraken as "up to 720 of the most recent entries (older
/// data cannot be retrieved, regardless of the value of `since`)" — the cap is
/// on the *window*, not on a page, so it bounds the reachable history at
/// `MAX_BARS × interval` rather than merely the size of one response.
pub const MAX_BARS: usize = 720;

/// The extra candle fields Kraken returns beyond OHLCV, exposed as `Real`
/// overlay columns on every atom: `vwap` (the bar's volume-weighted average
/// price) and `n_trades` (the trade count, named to match the `binance`
/// provider's column rather than Kraken's own `count`). Ordering matches the
/// candle row's field indexes (5, 7) so the decode step feeds
/// [`OverlayInfo::new`] in schema order.
pub fn kraken_schema() -> &'static Arc<Schema> {
    static SCHEMA: OnceLock<Arc<Schema>> = OnceLock::new();
    SCHEMA.get_or_init(|| {
        let mut b = Schema::builder();
        b.add_real("vwap");
        b.add_real("n_trades");
        b.finish()
    })
}

/// A Kraken spot OHLC client.
///
/// Cheap to clone (the inner [`reqwest::Client`] is `Arc`-backed).
#[derive(Debug, Clone)]
pub struct Kraken {
    client: reqwest::Client,
    base_url: String,
}

impl Default for Kraken {
    fn default() -> Self {
        Self::new()
    }
}

impl Kraken {
    /// A client pointing at the public Kraken endpoint.
    pub fn new() -> Self {
        Self {
            client: reqwest::Client::new(),
            base_url: DEFAULT_BASE_URL.to_string(),
        }
    }

    /// Override the API base URL (`https://api.kraken.com` by default).
    /// Primarily useful for testing against a local `wiremock` server.
    pub fn with_base_url(mut self, url: impl Into<String>) -> Self {
        self.base_url = url.into();
        self
    }
}

impl SeriesSource for Kraken {
    fn name(&self) -> &'static str {
        "kraken"
    }

    fn schema(&self) -> Option<Arc<Schema>> {
        Some(kraken_schema().clone())
    }

    fn tickers(&self) -> impl Future<Output = Result<Vec<String>, SourceError>> + Send {
        let client = self.client.clone();
        let base_url = self.base_url.clone();
        async move {
            let url = format!("{}/0/public/AssetPairs", base_url.trim_end_matches('/'));
            let resp = client.get(&url).send().await?;
            let status = resp.status();
            if !status.is_success() {
                return Err(map_http_error(resp).await);
            }
            let body: KrakenEnvelope = resp
                .json()
                .await
                .map_err(|e| SourceError::Decode(format!("asset pairs JSON: {e}")))?;
            body.check_error()?;
            let pairs = body.result.as_object().ok_or_else(|| {
                SourceError::Decode("asset pairs `result` is not an object".into())
            })?;
            // `altname` is the compact spelling Kraken accepts back as `pair`
            // (`XBTUSD`), and it is unique across the whole pair list — unlike
            // the map key, which is the legacy internal id (`XXBTZUSD`). A pair
            // missing it is skipped rather than reported under a name that
            // would not round-trip.
            let mut out: Vec<String> = pairs
                .values()
                .filter(|p| {
                    // `status` is absent on some pairs; treat that as tradable
                    // rather than dropping it.
                    p.get("status")
                        .and_then(|s| s.as_str())
                        .is_none_or(|s| s == "online")
                })
                .filter_map(|p| p.get("altname")?.as_str().map(str::to_string))
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
        async move {
            let minutes = interval_to_minutes(interval)?;
            let schema = kraken_schema().clone();
            let since_ms = since.0;
            let until_ms = until.map(|t| t.0).unwrap_or(i64::MAX);
            let url = format!("{}/0/public/OHLC", base_url.trim_end_matches('/'));

            // One request is the whole fetch. Kraken's `since` is in *seconds*
            // and selects bars whose open time is `>= since`, so flooring the
            // millisecond stamp keeps the boundary bar rather than dropping it.
            // There is no cursor to advance: the response always runs to the
            // present and is capped at `MAX_BARS`, so a second call with a later
            // `since` could only ever return a subset of this one.
            let since_sec = since_ms.div_euclid(1_000);
            let query: Vec<(&str, String)> = vec![
                ("pair", symbol.clone()),
                ("interval", minutes.to_string()),
                ("since", since_sec.to_string()),
            ];

            let resp = client.get(&url).query(&query).send().await?;
            let status = resp.status();
            if !status.is_success() {
                return Err(map_http_error(resp).await);
            }

            let body: KrakenEnvelope = resp
                .json()
                .await
                .map_err(|e| SourceError::Decode(format!("candle JSON: {e}")))?;
            body.check_error()?;

            let result = body
                .result
                .as_object()
                .ok_or_else(|| SourceError::Decode("OHLC `result` is not an object".into()))?;

            // `last` is the open time of the final *committed* bar, in seconds.
            // Everything after it is the bar currently forming.
            let last_ms = result
                .get("last")
                .and_then(|v| v.as_i64())
                .map(|s| s.saturating_mul(1_000));

            // The result key is the pair's internal id, which the request
            // spelling does not determine — take the sole non-`last` entry.
            let rows = result
                .iter()
                .find(|(k, _)| k.as_str() != "last")
                .map(|(_, v)| v)
                .ok_or_else(|| {
                    SourceError::Decode("OHLC `result` carries no candle series".into())
                })?
                .as_array()
                .ok_or_else(|| SourceError::Decode("OHLC candle series is not an array".into()))?;

            let mut out: Vec<Atom> = Vec::with_capacity(rows.len());
            for row in rows {
                let atom = decode_row(row, &schema)?;
                let ts = atom.time.expect("Kraken atoms always carry a time").0;
                // Drop the still-forming bar: its OHLCV is partial and mutates
                // between calls.
                if last_ms.is_some_and(|last| ts > last) {
                    continue;
                }
                if ts >= since_ms && ts < until_ms {
                    out.push(atom);
                }
            }

            // Kraken serves ascending already; sorting makes that a guarantee
            // rather than an observation, and costs nothing on sorted input.
            out.sort_by_key(|a| a.time.map(|t| t.0).unwrap_or(i64::MIN));
            Ok(out)
        }
    }
}

/// Map an [`Interval`] to Kraken's `interval` vocabulary, which is a count of
/// **minutes** drawn from a fixed set: 1, 5, 15, 30, 60, 240, 1440, 10080,
/// 21600. Anything else — `Minute(3)`, `Hour(2)`, `Month(1)` — is rejected
/// rather than silently rounded; Kraken answers an unsupported value with
/// `EGeneral:Invalid arguments`, which would surface as an opaque HTTP error.
///
/// `Day(15)` is the spelling of Kraken's 21600-minute bar (15 days); there is
/// no month cadence, so `Month(_)` has no mapping at all.
fn interval_to_minutes(interval: Interval) -> Result<u32, SourceError> {
    let minutes = match interval {
        Interval::Minute(1) => 1,
        Interval::Minute(5) => 5,
        Interval::Minute(15) => 15,
        Interval::Minute(30) => 30,
        Interval::Hour(1) => 60,
        Interval::Hour(4) => 240,
        Interval::Day(1) => 1440,
        Interval::Week(1) => 10080,
        Interval::Day(15) => 21600,
        other => return Err(SourceError::UnsupportedInterval(other)),
    };
    Ok(minutes)
}

/// Extract one candle row into an [`Atom`], populating the two Kraken extras
/// (VWAP, trade count) as overlay values in `schema` order.
///
/// Kraken's row is `[time, open, high, low, close, vwap, volume, count]` with a
/// mixed shape that is easy to get wrong: **index 0 and 7 are bare JSON
/// numbers, 1–6 are quoted decimal strings**. Both spellings are accepted for
/// every field so a mock server returning uniform types still decodes. A row
/// carrying only the seven leading fields leaves `n_trades` as `Real::NAN`, so
/// a downstream `!get { key }` sees a defined-but-empty column rather than an
/// error.
fn decode_row(row: &serde_json::Value, schema: &Arc<Schema>) -> Result<Atom, SourceError> {
    let arr = row
        .as_array()
        .ok_or_else(|| SourceError::Decode("candle is not a JSON array".into()))?;
    if arr.len() < 7 {
        return Err(SourceError::Decode(format!(
            "candle row has {} fields, expected at least 7",
            arr.len()
        )));
    }
    // Kraken timestamps are seconds since the epoch; the crate is milliseconds.
    let open_time = parse_i64_str(&arr[0], "time")?.saturating_mul(1_000);
    let open = parse_num_str(&arr[1], "open")?;
    let high = parse_num_str(&arr[2], "high")?;
    let low = parse_num_str(&arr[3], "low")?;
    let close = parse_num_str(&arr[4], "close")?;
    let vwap = parse_num_str(&arr[5], "vwap")?;
    let volume = parse_num_str(&arr[6], "volume")?;
    let n_trades = arr
        .get(7)
        .map(|v| parse_num_str(v, "n_trades").unwrap_or(Real::NAN))
        .unwrap_or(Real::NAN);
    let overlays = OverlayInfo::new(
        schema.clone(),
        vec![OverlayValue::Real(vwap), OverlayValue::Real(n_trades)],
    );
    Ok(Atom::with_overlays_and_time(
        Candle::new(open, high, low, close, volume),
        overlays,
        Timestamp(open_time),
    ))
}

/// Kraken quotes the OHLC price/volume columns as strings. Also accept a bare
/// JSON number, so a mock server that returns typed numbers still works.
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

/// The timestamp column, as an integer. Kraken sends a bare number here (unlike
/// the price columns); a string still decodes, for symmetry with the mocks.
fn parse_i64_str(v: &serde_json::Value, field: &str) -> Result<i64, SourceError> {
    match v {
        serde_json::Value::Number(n) => n
            .as_i64()
            .ok_or_else(|| SourceError::Decode(format!("candle `{field}` is not an integer"))),
        serde_json::Value::String(s) => s
            .parse::<i64>()
            .map_err(|e| SourceError::Decode(format!("candle `{field}` = {s:?}: {e}"))),
        other => Err(SourceError::Decode(format!(
            "candle `{field}` has unexpected JSON type: {other}"
        ))),
    }
}

/// Turn a non-2xx response into a [`SourceError`]. Kraken itself answers
/// application errors with HTTP 200 and an `error` array, so this path is
/// mostly the CDN in front of it: a Cloudflare `429` (possibly with an HTML
/// body, which is why the body is captured verbatim rather than parsed).
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

/// Map Kraken's `error` array to the most specific [`SourceError`] variant.
///
/// Entries are shaped `E<Category>:<Message>`; the categories that have a
/// dedicated variant are matched by prefix, since Kraken appends detail to some
/// of them (`EService:Throttled: 1699999999` carries a retry stamp). Anything
/// else surfaces as a generic `Http` error carrying the joined array, because a
/// status code alone would say nothing — every one of these arrives as 200.
fn map_api_error(errors: &[String]) -> SourceError {
    for e in errors {
        if e.starts_with("EQuery:Unknown asset pair") {
            return SourceError::UnknownSymbol(e.clone());
        }
        if e.starts_with("EAPI:Rate limit exceeded") || e.starts_with("EService:Throttled") {
            // `EService:Throttled: <unix seconds>` names the *absolute* moment
            // the block lifts, but `RateLimited` carries a relative delay, and
            // deriving one would mean reading a clock inside a decode path.
            // `0` is the crate's "unspecified" spelling, matching `okx`.
            return SourceError::RateLimited { retry_after_ms: 0 };
        }
    }
    SourceError::Http {
        status: 200,
        body: format!("Kraken error {}", errors.join(", ")),
    }
}

/// Kraken wraps every response in `{error: [...], result: {...}}`. `error` is
/// empty on success and `result` varies by endpoint, so it stays a raw
/// [`serde_json::Value`] — the OHLC payload mixes a candle array and a scalar
/// `last` under one object, which no single typed shape describes.
#[derive(Deserialize)]
struct KrakenEnvelope {
    #[serde(default)]
    error: Vec<String>,
    #[serde(default)]
    result: serde_json::Value,
}

impl KrakenEnvelope {
    /// `Err` when the envelope carries any application error. Kraken returns
    /// HTTP 200 for these, so every response must be checked here before
    /// `result` is touched.
    fn check_error(&self) -> Result<(), SourceError> {
        if self.error.is_empty() {
            Ok(())
        } else {
            Err(map_api_error(&self.error))
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn interval_minutes_map_correctly() {
        assert_eq!(interval_to_minutes(Interval::Minute(1)).unwrap(), 1);
        assert_eq!(interval_to_minutes(Interval::Minute(30)).unwrap(), 30);
        assert_eq!(interval_to_minutes(Interval::Hour(1)).unwrap(), 60);
        assert_eq!(interval_to_minutes(Interval::Hour(4)).unwrap(), 240);
        assert_eq!(interval_to_minutes(Interval::Day(1)).unwrap(), 1440);
        assert_eq!(interval_to_minutes(Interval::Week(1)).unwrap(), 10080);
        assert_eq!(interval_to_minutes(Interval::Day(15)).unwrap(), 21600);
    }

    #[test]
    fn unsupported_intervals_reject() {
        // Kraken's vocabulary is a fixed set, not "any multiple of a minute" —
        // 3m and 2h are real cadences elsewhere but not here, and there is no
        // month bar at all.
        for bad in [
            Interval::Minute(3),
            Interval::Hour(2),
            Interval::Hour(12),
            Interval::Day(5),
            Interval::Month(1),
        ] {
            assert!(
                matches!(
                    interval_to_minutes(bad),
                    Err(SourceError::UnsupportedInterval(_))
                ),
                "{bad:?} should be rejected"
            );
        }
    }

    #[test]
    fn decode_row_parses_krakens_mixed_types() {
        // The shape that trips every naive client: index 0 and 7 are bare JSON
        // numbers while 1-6 are quoted strings. A uniform `[f64; 8]` fails here.
        let row = serde_json::json!([
            1_700_000_000_i64,
            "27000.5",
            "27100.0",
            "26950.1",
            "27050.75",
            "27020.3", // vwap
            "12.345",  // volume
            25883_i64  // count
        ]);
        let atom = decode_row(&row, &kraken_schema().clone()).unwrap();
        // Seconds on the wire, milliseconds in the crate.
        assert_eq!(atom.time, Some(Timestamp(1_700_000_000_000)));
        let c = atom.candle.unwrap();
        assert_eq!(c.open, 27000.5);
        assert_eq!(c.high, 27100.0);
        assert_eq!(c.low, 26950.1);
        assert_eq!(c.close, 27050.75);
        // Index 6 is volume; index 5 is VWAP and must not be mistaken for it.
        assert_eq!(c.volume, 12.345);
        let ov = atom.overlays.expect("Kraken atoms carry overlays");
        assert_eq!(ov.get_by_key("vwap"), Some(&OverlayValue::Real(27020.3)));
        assert_eq!(
            ov.get_by_key("n_trades"),
            Some(&OverlayValue::Real(25883.0))
        );
    }

    #[test]
    fn decode_row_tolerates_missing_count() {
        // Seven fields is the documented minimum; `count` collapsing to NaN
        // keeps the column defined-but-empty rather than failing the fetch.
        let row = serde_json::json!([1_700_000_000_i64, 1.0, 2.0, 0.5, 1.5, 1.2, 10.0]);
        let atom = decode_row(&row, &kraken_schema().clone()).unwrap();
        assert_eq!(atom.candle.unwrap().close, 1.5);
        let ov = atom.overlays.expect("Kraken atoms carry overlays");
        match ov.get_by_key("n_trades") {
            Some(OverlayValue::Real(v)) => assert!(v.is_nan()),
            other => panic!("expected NaN, got {other:?}"),
        }
    }

    #[test]
    fn short_row_is_rejected() {
        let row = serde_json::json!([1_700_000_000_i64, 1.0, 2.0]);
        assert!(matches!(
            decode_row(&row, &kraken_schema().clone()),
            Err(SourceError::Decode(_))
        ));
    }

    #[test]
    fn api_errors_map_to_variants() {
        assert!(matches!(
            map_api_error(&["EQuery:Unknown asset pair".to_string()]),
            SourceError::UnknownSymbol(_)
        ));
        assert!(matches!(
            map_api_error(&["EAPI:Rate limit exceeded".to_string()]),
            SourceError::RateLimited { .. }
        ));
        // Kraken appends a retry stamp to this one, so the match is by prefix.
        assert!(matches!(
            map_api_error(&["EService:Throttled: 1699999999".to_string()]),
            SourceError::RateLimited { .. }
        ));
        // Everything else arrives as HTTP 200 too, so the status carries no
        // information and the body is what a caller has to read.
        match map_api_error(&["EGeneral:Invalid arguments".to_string()]) {
            SourceError::Http { status: 200, body } => {
                assert!(body.contains("EGeneral:Invalid arguments"), "got {body:?}")
            }
            other => panic!("expected Http, got {other:?}"),
        }
    }

    #[test]
    fn empty_error_array_is_success() {
        let env = KrakenEnvelope {
            error: Vec::new(),
            result: serde_json::json!({}),
        };
        assert!(env.check_error().is_ok());
    }
}
