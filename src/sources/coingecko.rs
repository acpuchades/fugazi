//! CoinGecko implementation of [`OverlaySource`] — market capitalisation,
//! traded volume, and derived circulating supply, with **no OHLCV**.
//!
//! Fetches `GET /api/v3/coins/{id}/market_chart/range`, which returns three
//! parallel `[timestamp_ms, value]` series (`prices`, `market_caps`,
//! `total_volumes`) sampled over the requested window.
//!
//! Four `Real` overlay columns come out (see [`coingecko_schema`]):
//!
//! | column | source |
//! |---|---|
//! | `price` | the `prices` series (vs. USD) |
//! | `market_cap` | the `market_caps` series |
//! | `total_volume` | the `total_volumes` series (24h rolling, as CoinGecko reports it) |
//! | `circulating_supply` | **derived**: `market_cap / price` |
//!
//! `circulating_supply` is the one column CoinGecko does not return directly;
//! it is the exact identity implied by the other two, and is `NaN` on any bar
//! where either input is missing or the price is zero. `price` is worth keeping
//! even though you are joining these onto someone else's candles: comparing it
//! against that series' `close` is the cheapest way to catch a wrong
//! `OUTPUT=QUERY` mapping (see [`OverlaySource`]).
//!
//! ## Granularity is chosen by CoinGecko, not by us
//!
//! `market_chart/range` has no interval parameter on the public API — the
//! sampling granularity is a function of the **requested window length**:
//! roughly 5-minutely for ≤1 day, hourly for ≤90 days, and daily beyond that.
//! So this client:
//!
//! * Rejects sub-hourly [`Interval`]s outright ([`SourceError::UnsupportedInterval`]).
//!   The 5-minutely tier only exists for windows too short to backtest over,
//!   and silently returning daily data for a `5m` request is exactly the kind
//!   of quietly-wrong number this crate refuses to produce.
//! * Paginates hourly requests in [`HOURLY_WINDOW_DAYS`]-day windows, keeping
//!   each one inside CoinGecko's hourly tier however long the caller's overall
//!   range is.
//! * Buckets whatever samples come back onto the requested interval's bar-open
//!   boundaries, keeping the **first** sample in each bucket — the value as of
//!   the bar's open, never a later reading that a strategy could not have seen
//!   at the open. Over-sampled input (hourly data bucketed to daily bars) is
//!   therefore downsampled point-in-time; under-sampled input (daily data for
//!   an hourly request, which pagination prevents) would leave gaps rather than
//!   interpolate.
//!
//! ## Authentication
//!
//! The public endpoint works without a key but is aggressively rate-limited.
//! [`CoinGecko::new`] picks up a demo key from the `COINGECKO_API_KEY`
//! environment variable and sends it as `x-cg-demo-api-key`;
//! [`CoinGecko::with_api_key`] sets it explicitly. Note that CoinGecko's free
//! tiers cap how far back `market_chart/range` will serve — a window beyond
//! that surfaces as [`SourceError::Http`] with the API's own message rather
//! than being silently truncated.

use std::collections::{BTreeMap, BTreeSet};
use std::future::Future;
use std::sync::{Arc, OnceLock};
use std::time::Duration;

use serde::Deserialize;
use time::{Date, Duration as TimeDuration, Time};

use crate::types::{OverlayInfo, OverlayValue, Real, Schema};

use super::{Interval, OverlayRow, OverlaySource, SourceError, Timestamp};

const DEFAULT_BASE_URL: &str = "https://api.coingecko.com";
const DEFAULT_MIN_DELAY_MS: u64 = 1_500;

/// CoinGecko rejects requests without a descriptive `User-Agent` (HTTP 403),
/// so this is not optional politeness — it is required for the API to answer at
/// all. Identifies the crate and version, as their docs ask.
const DEFAULT_USER_AGENT: &str = concat!(
    "fugazi/",
    env!("CARGO_PKG_VERSION"),
    " (+https://github.com/acpuchades/fugazi)"
);

/// Environment variable read by [`CoinGecko::new`] for a demo API key.
pub const API_KEY_ENV: &str = "COINGECKO_API_KEY";

/// Widest window we will ask for in one request when the caller wants hourly
/// bars. CoinGecko serves hourly samples for ranges up to ~90 days and drops to
/// daily beyond that, so we stay comfortably inside the hourly tier.
const HOURLY_WINDOW_DAYS: i64 = 80;

const MS_PER_DAY: i64 = 86_400_000;

/// The overlay columns CoinGecko exposes. Read from a strategy or `--overlay`
/// spec with `!get { key: market_cap }`, etc.
///
/// `circulating_supply` is derived (`market_cap / price`); the other three come
/// straight off the API's three series. See the [module docs](self).
pub fn coingecko_schema() -> &'static Arc<Schema> {
    static SCHEMA: OnceLock<Arc<Schema>> = OnceLock::new();
    SCHEMA.get_or_init(|| {
        let mut b = Schema::builder();
        b.add_real("price");
        b.add_real("market_cap");
        b.add_real("total_volume");
        b.add_real("circulating_supply");
        b.finish()
    })
}

/// A CoinGecko market-chart client.
///
/// Cheap to clone (the inner [`reqwest::Client`] is `Arc`-backed).
///
/// The `symbol` this provider takes is a CoinGecko **coin id** (`bitcoin`,
/// `ethereum`, `solana`) — not a ticker and not an exchange pair. Fetch the
/// vocabulary with [`OverlaySource::tickers`] (`fugazi list tickers coingecko`).
#[derive(Debug, Clone)]
pub struct CoinGecko {
    client: reqwest::Client,
    base_url: String,
    vs_currency: String,
    api_key: Option<String>,
    user_agent: String,
    min_delay_between_requests: Duration,
}

impl Default for CoinGecko {
    fn default() -> Self {
        Self::new()
    }
}

impl CoinGecko {
    /// A client pointing at the public CoinGecko endpoint, quoting in USD,
    /// pacing requests 1.5 s apart (the public tier's limit is low enough that
    /// a tighter loop reliably trips it).
    ///
    /// Picks up a demo API key from the [`API_KEY_ENV`] environment variable if
    /// one is set.
    pub fn new() -> Self {
        Self {
            client: reqwest::Client::new(),
            base_url: DEFAULT_BASE_URL.to_string(),
            vs_currency: "usd".to_string(),
            api_key: std::env::var(API_KEY_ENV).ok().filter(|k| !k.trim().is_empty()),
            user_agent: DEFAULT_USER_AGENT.to_string(),
            min_delay_between_requests: Duration::from_millis(DEFAULT_MIN_DELAY_MS),
        }
    }

    /// Override the `User-Agent` sent with every request. CoinGecko 403s a
    /// request without a descriptive one, so an empty or generic value will
    /// break the fetch.
    pub fn with_user_agent(mut self, ua: impl Into<String>) -> Self {
        self.user_agent = ua.into();
        self
    }

    /// Override the API base URL. Primarily useful for testing against a local
    /// mock server.
    pub fn with_base_url(mut self, url: impl Into<String>) -> Self {
        self.base_url = url.into();
        self
    }

    /// Quote against a currency other than USD (`"eur"`, `"btc"`, …). This
    /// scales `price` and `market_cap` together, so `circulating_supply` is
    /// unaffected.
    pub fn with_vs_currency(mut self, currency: impl Into<String>) -> Self {
        self.vs_currency = currency.into();
        self
    }

    /// Set the demo API key explicitly, overriding [`API_KEY_ENV`].
    pub fn with_api_key(mut self, key: impl Into<String>) -> Self {
        self.api_key = Some(key.into());
        self
    }

    /// Override the delay between successive requests (default 1.5 s).
    pub fn with_min_delay(mut self, d: Duration) -> Self {
        self.min_delay_between_requests = d;
        self
    }
}

impl OverlaySource for CoinGecko {
    fn name(&self) -> &'static str {
        "coingecko"
    }

    fn schema(&self) -> Arc<Schema> {
        coingecko_schema().clone()
    }

    fn tickers(&self) -> impl Future<Output = Result<Vec<String>, SourceError>> + Send {
        let client = self.client.clone();
        let base_url = self.base_url.clone();
        let api_key = self.api_key.clone();
        let user_agent = self.user_agent.clone();
        async move {
            let url = format!("{}/api/v3/coins/list", base_url.trim_end_matches('/'));
            let resp = prepare(client.get(&url), &user_agent, api_key.as_deref())
                .send()
                .await?;
            if !resp.status().is_success() {
                return Err(map_http_error(resp).await);
            }
            let body: Vec<CoinListEntry> = resp
                .json()
                .await
                .map_err(|e| SourceError::Decode(format!("coins/list JSON: {e}")))?;
            let mut out: Vec<String> = body.into_iter().map(|c| c.id).collect();
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
        let id = symbol.to_string();
        let client = self.client.clone();
        let base_url = self.base_url.clone();
        let vs_currency = self.vs_currency.clone();
        let api_key = self.api_key.clone();
        let user_agent = self.user_agent.clone();
        let min_delay = self.min_delay_between_requests;
        async move {
            check_interval(interval)?;
            let schema = coingecko_schema().clone();
            let until_ms = until.map(|t| t.0).unwrap_or_else(|| Timestamp::now().0);
            let url = format!(
                "{}/api/v3/coins/{}/market_chart/range",
                base_url.trim_end_matches('/'),
                id,
            );

            // Accumulate samples across pages before bucketing: a bucket can
            // straddle a page boundary, and "first sample wins" has to mean
            // first across the whole fetch, not first within a page.
            let mut samples = Samples::default();
            let mut cursor = since.0;
            let mut first = true;

            while cursor < until_ms {
                if !first {
                    tokio::time::sleep(min_delay).await;
                }
                first = false;

                let page_end = page_end_ms(cursor, until_ms, interval);
                // CoinGecko's range bounds are in *seconds*, both inclusive. We
                // floor `from` and ceil `to` so a bar sitting exactly on a
                // boundary is never dropped by integer truncation; anything we
                // over-fetch is filtered out by the `[since, until)` pass below.
                let from_s = cursor.div_euclid(1000);
                let to_s = page_end.div_euclid(1000) + 1;

                let query: Vec<(&str, String)> = vec![
                    ("vs_currency", vs_currency.clone()),
                    ("from", from_s.to_string()),
                    ("to", to_s.to_string()),
                ];
                let resp = prepare(
                    client.get(&url).query(&query),
                    &user_agent,
                    api_key.as_deref(),
                )
                .send()
                .await?;
                if !resp.status().is_success() {
                    return Err(map_http_error(resp).await);
                }
                let chart: MarketChart = resp
                    .json()
                    .await
                    .map_err(|e| SourceError::Decode(format!("market_chart JSON: {e}")))?;
                samples.absorb(&chart, interval);

                if page_end >= until_ms {
                    break;
                }
                cursor = page_end;
            }

            Ok(samples.into_rows(&schema, since.0, until_ms))
        }
    }
}

/// Attach the headers every CoinGecko request needs: the mandatory descriptive
/// `User-Agent` (a request without one is refused with a 403), plus the demo API
/// key when one is configured.
fn prepare(
    req: reqwest::RequestBuilder,
    user_agent: &str,
    key: Option<&str>,
) -> reqwest::RequestBuilder {
    let req = req.header(reqwest::header::USER_AGENT, user_agent);
    match key {
        Some(k) => req.header("x-cg-demo-api-key", k),
        None => req,
    }
}

/// Reject the cadences CoinGecko's `market_chart/range` cannot honestly serve.
///
/// Accepted: `Hour(n)`, `Day(1)`, `Week(1)`, `Month(1)`. Sub-hourly is refused
/// because the 5-minutely tier only covers ≤1-day windows (see the [module
/// docs](self)); multi-day/week/month cadences are refused because they have no
/// canonical bar-open anchor to bucket onto, so any choice we made would
/// silently disagree with whichever candle provider you are joining against.
fn check_interval(interval: Interval) -> Result<(), SourceError> {
    match interval {
        Interval::Hour(n) if n > 0 => Ok(()),
        Interval::Day(1) | Interval::Week(1) | Interval::Month(1) => Ok(()),
        other => Err(SourceError::UnsupportedInterval(other)),
    }
}

/// End of the request window starting at `cursor`. Hourly fetches are capped at
/// [`HOURLY_WINDOW_DAYS`] so each request stays inside CoinGecko's hourly
/// sampling tier; daily-and-coarser fetches take the whole range in one call
/// (CoinGecko already serves those as daily samples).
fn page_end_ms(cursor: i64, until_ms: i64, interval: Interval) -> i64 {
    match interval {
        Interval::Hour(_) => cursor
            .saturating_add(HOURLY_WINDOW_DAYS * MS_PER_DAY)
            .min(until_ms),
        _ => until_ms,
    }
}

/// Floor a millisecond timestamp onto the bar-open boundary of `interval`.
///
/// `Hour`/`Day` floor by epoch modulo — the Unix epoch is itself a UTC midnight
/// on an hour boundary, so this lands on real clock boundaries and matches the
/// convention every candle provider uses. `Week` floors to Monday 00:00 UTC
/// (epoch day 0 was a Thursday, so modulo would anchor weeks on Thursdays and
/// silently fail to join against a Monday-anchored weekly candle). `Month`
/// floors to the 1st at 00:00 UTC, which modulo cannot express at all.
///
/// Only the cadences [`check_interval`] admits are reachable here; anything
/// else falls back to epoch modulo.
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

/// The three `[ts_ms, value]` series, bucketed onto interval boundaries.
///
/// Each map is `bucket -> value`, **first sample wins** — the reading as of the
/// bar's open. Kept as three independent maps rather than one row map because
/// CoinGecko does not guarantee the series are the same length or share
/// timestamps (a coin with no reported market cap returns an empty
/// `market_caps` while still returning prices).
#[derive(Debug, Default)]
struct Samples {
    price: BTreeMap<i64, Real>,
    market_cap: BTreeMap<i64, Real>,
    total_volume: BTreeMap<i64, Real>,
}

impl Samples {
    fn absorb(&mut self, chart: &MarketChart, interval: Interval) {
        fill(&mut self.price, &chart.prices, interval);
        fill(&mut self.market_cap, &chart.market_caps, interval);
        fill(&mut self.total_volume, &chart.total_volumes, interval);
    }

    /// Emit one [`OverlayRow`] per bucket that any series touched, ascending by
    /// time, restricted to `[since, until)`. A series missing a bucket yields
    /// `NaN` for that cell rather than dropping the row — a coin with prices but
    /// no market cap still produces a usable `price` column.
    fn into_rows(self, schema: &Arc<Schema>, since_ms: i64, until_ms: i64) -> Vec<OverlayRow> {
        let buckets: BTreeSet<i64> = self
            .price
            .keys()
            .chain(self.market_cap.keys())
            .chain(self.total_volume.keys())
            .copied()
            .filter(|&t| t >= since_ms && t < until_ms)
            .collect();

        buckets
            .into_iter()
            .map(|t| {
                let price = self.price.get(&t).copied().unwrap_or(Real::NAN);
                let market_cap = self.market_cap.get(&t).copied().unwrap_or(Real::NAN);
                let total_volume = self.total_volume.get(&t).copied().unwrap_or(Real::NAN);
                let overlays = OverlayInfo::new(
                    schema.clone(),
                    vec![
                        OverlayValue::Real(price),
                        OverlayValue::Real(market_cap),
                        OverlayValue::Real(total_volume),
                        OverlayValue::Real(circulating_supply(market_cap, price)),
                    ],
                );
                OverlayRow {
                    time: Timestamp(t),
                    overlays,
                }
            })
            .collect()
    }
}

/// `market_cap / price`, the supply identity — `NaN` unless both inputs are
/// finite and the price is non-zero, so a missing input propagates as a missing
/// cell instead of an infinity.
fn circulating_supply(market_cap: Real, price: Real) -> Real {
    if market_cap.is_finite() && price.is_finite() && price != 0.0 {
        market_cap / price
    } else {
        Real::NAN
    }
}

/// Bucket one `[ts_ms, value]` series into `dst`, first-sample-wins per bucket.
/// Non-finite values are skipped so they don't win a bucket a real sample could
/// have filled.
fn fill(dst: &mut BTreeMap<i64, Real>, series: &[[f64; 2]], interval: Interval) {
    for point in series {
        let (ts, value) = (point[0], point[1]);
        if !ts.is_finite() || !value.is_finite() {
            continue;
        }
        dst.entry(floor_to_bucket(ts as i64, interval)).or_insert(value);
    }
}

/// Turn a non-2xx response into a [`SourceError`], preferring the specific
/// variants over the generic `Http`. CoinGecko answers an unknown coin id with
/// a 404, and a tripped rate limit with a 429.
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
        404 => SourceError::UnknownSymbol(format!(
            "CoinGecko has no such coin id (ids look like `bitcoin`, not `BTC` — \
             see `fugazi list tickers coingecko`): {body}"
        )),
        _ => SourceError::Http { status: code, body },
    }
}

/// The `market_chart/range` response. Every series is a list of
/// `[timestamp_ms, value]` pairs. A field CoinGecko omits (an asset with no
/// reported market cap) decodes to an empty vec rather than failing the fetch.
#[derive(Debug, Default, Deserialize)]
struct MarketChart {
    #[serde(default)]
    prices: Vec<[f64; 2]>,
    #[serde(default)]
    market_caps: Vec<[f64; 2]>,
    #[serde(default)]
    total_volumes: Vec<[f64; 2]>,
}

/// The subset of `/api/v3/coins/list` we read — the id vocabulary. `symbol` and
/// `name` are ignored: ids are what `market_chart/range` keys on.
#[derive(Deserialize)]
struct CoinListEntry {
    id: String,
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 2024-01-03T00:00:00Z — a Wednesday, so the weekly bucket must land on
    /// Monday 2024-01-01 and the monthly one on 2024-01-01 as well.
    const WED: i64 = 1_704_240_000_000;
    const MON: i64 = 1_704_067_200_000; // 2024-01-01T00:00:00Z

    #[test]
    fn accepts_only_the_cadences_it_can_serve() {
        assert!(check_interval(Interval::Hour(1)).is_ok());
        assert!(check_interval(Interval::Hour(4)).is_ok());
        assert!(check_interval(Interval::Day(1)).is_ok());
        assert!(check_interval(Interval::Week(1)).is_ok());
        assert!(check_interval(Interval::Month(1)).is_ok());
        // Sub-hourly: CoinGecko's 5-minutely tier only covers ≤1-day windows.
        assert!(matches!(
            check_interval(Interval::Minute(5)),
            Err(SourceError::UnsupportedInterval(_))
        ));
        // No canonical bar-open anchor for these.
        assert!(matches!(
            check_interval(Interval::Day(3)),
            Err(SourceError::UnsupportedInterval(_))
        ));
        assert!(matches!(
            check_interval(Interval::Week(2)),
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
        // Already on a boundary: floor is the identity.
        assert_eq!(floor_to_bucket(WED, Interval::Day(1)), WED);
    }

    #[test]
    fn floors_weeks_to_monday_not_to_the_epoch_thursday() {
        // The whole point of the calendar path: `WED % 7d` would anchor on a
        // Thursday and never join against a Monday-opening weekly candle.
        assert_eq!(floor_to_bucket(WED, Interval::Week(1)), MON);
        assert_ne!(WED - WED.rem_euclid(7 * MS_PER_DAY), MON);
        // A Monday is its own bucket.
        assert_eq!(floor_to_bucket(MON, Interval::Week(1)), MON);
    }

    #[test]
    fn floors_months_to_the_first() {
        // 2024-02-29T12:00:00Z (leap day) → 2024-02-01T00:00:00Z.
        let leap_day = 1_709_208_000_000;
        let feb_1 = 1_706_745_600_000;
        assert_eq!(floor_to_bucket(leap_day, Interval::Month(1)), feb_1);
    }

    #[test]
    fn buckets_keep_the_first_sample_not_the_last() {
        // Hourly samples bucketed to a daily bar: the 00:00 reading is what a
        // strategy could see at the open; the 23:00 one would be lookahead.
        let chart = MarketChart {
            prices: vec![
                [WED as f64, 42_000.0],
                [(WED + 3_600_000) as f64, 43_000.0],
                [(WED + 23 * 3_600_000) as f64, 47_000.0],
            ],
            market_caps: vec![[WED as f64, 840_000_000_000.0]],
            total_volumes: vec![[WED as f64, 20_000_000_000.0]],
        };
        let mut s = Samples::default();
        s.absorb(&chart, Interval::Day(1));
        let rows = s.into_rows(coingecko_schema(), 0, i64::MAX);

        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].time, Timestamp(WED));
        let ov = &rows[0].overlays;
        assert_eq!(ov.get_by_key("price"), Some(&OverlayValue::Real(42_000.0)));
        assert_eq!(
            ov.get_by_key("market_cap"),
            Some(&OverlayValue::Real(840_000_000_000.0))
        );
        // Derived: 840e9 / 42e3 = 20e6 coins.
        assert_eq!(
            ov.get_by_key("circulating_supply"),
            Some(&OverlayValue::Real(20_000_000.0))
        );
    }

    #[test]
    fn a_series_missing_a_bucket_yields_nan_not_a_dropped_row() {
        // A coin with prices but no reported market cap still produces rows.
        let chart = MarketChart {
            prices: vec![[WED as f64, 5.0]],
            market_caps: vec![],
            total_volumes: vec![],
        };
        let mut s = Samples::default();
        s.absorb(&chart, Interval::Day(1));
        let rows = s.into_rows(coingecko_schema(), 0, i64::MAX);

        assert_eq!(rows.len(), 1);
        let ov = &rows[0].overlays;
        assert_eq!(ov.get_by_key("price"), Some(&OverlayValue::Real(5.0)));
        for key in ["market_cap", "total_volume", "circulating_supply"] {
            match ov.get_by_key(key) {
                Some(OverlayValue::Real(v)) => assert!(v.is_nan(), "{key} should be NaN"),
                other => panic!("{key}: expected NaN, got {other:?}"),
            }
        }
    }

    #[test]
    fn rows_are_clipped_to_the_requested_half_open_window() {
        let chart = MarketChart {
            prices: vec![
                [(MON) as f64, 1.0],
                [(MON + MS_PER_DAY) as f64, 2.0],
                [(MON + 2 * MS_PER_DAY) as f64, 3.0],
            ],
            market_caps: vec![],
            total_volumes: vec![],
        };
        let mut s = Samples::default();
        s.absorb(&chart, Interval::Day(1));
        // `until` is exclusive, so the third bar is out.
        let rows = s.into_rows(coingecko_schema(), MON, MON + 2 * MS_PER_DAY);

        assert_eq!(rows.len(), 2);
        assert_eq!(rows[0].time, Timestamp(MON));
        assert_eq!(rows[1].time, Timestamp(MON + MS_PER_DAY));
    }

    #[test]
    fn absorb_across_pages_keeps_the_globally_first_sample() {
        // A bucket straddling a page boundary must not be overwritten by the
        // later page's sample.
        let mut s = Samples::default();
        s.absorb(
            &MarketChart {
                prices: vec![[WED as f64, 1.0]],
                ..Default::default()
            },
            Interval::Day(1),
        );
        s.absorb(
            &MarketChart {
                prices: vec![[(WED + 3_600_000) as f64, 999.0]],
                ..Default::default()
            },
            Interval::Day(1),
        );
        let rows = s.into_rows(coingecko_schema(), 0, i64::MAX);
        assert_eq!(rows.len(), 1);
        assert_eq!(
            rows[0].overlays.get_by_key("price"),
            Some(&OverlayValue::Real(1.0))
        );
    }

    #[test]
    fn hourly_requests_page_within_coingeckos_hourly_tier() {
        let start = MON;
        let year = start + 365 * MS_PER_DAY;
        // Hourly: capped at the 80-day window.
        assert_eq!(
            page_end_ms(start, year, Interval::Hour(1)),
            start + HOURLY_WINDOW_DAYS * MS_PER_DAY
        );
        // Daily: one shot, CoinGecko already serves daily samples there.
        assert_eq!(page_end_ms(start, year, Interval::Day(1)), year);
        // The cap never overshoots `until`.
        let short = start + 5 * MS_PER_DAY;
        assert_eq!(page_end_ms(start, short, Interval::Hour(1)), short);
    }

    #[test]
    fn non_finite_samples_are_skipped() {
        let chart = MarketChart {
            prices: vec![[WED as f64, f64::NAN], [(WED + 3_600_000) as f64, 7.0]],
            ..Default::default()
        };
        let mut s = Samples::default();
        s.absorb(&chart, Interval::Day(1));
        let rows = s.into_rows(coingecko_schema(), 0, i64::MAX);
        // The NaN did not win the bucket; the real sample behind it did.
        assert_eq!(
            rows[0].overlays.get_by_key("price"),
            Some(&OverlayValue::Real(7.0))
        );
    }
}
