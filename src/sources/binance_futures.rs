//! Binance USDⓈ-M perpetuals, live from `fapi.binance.com`, as a
//! [`SeriesSource`].
//!
//! The live twin of
//! [`BinanceVision::futures`](super::BinanceVision::futures): the **same
//! twelve columns**, fetched from the query API instead of the dated archive.
//! They publish one schema between them
//! ([`binance_futures_schema`] *is* the archive's futures schema), so a series
//! fetched either way carries the same columns in the same order and a
//! strategy reading `funding_rate` does not care which one produced its input.
//!
//! # Which of the two to reach for
//!
//! They differ in *when*, not in *what*:
//!
//! | | `binance-futures` (here) | `binance-vision-futures` |
//! |---|---|---|
//! | freshness | up to the current bar | ~2-day lag |
//! | bars, funding, premium index | full history | full history |
//! | open interest, the four ratios | **last 30 days only** | from 2021 |
//! | cost of five years of daily bars | ~15 requests, rate-limited | ~1800 requests, no limit |
//!
//! The 30-day floor is Binance's, not this crate's: `/futures/data/*` serves a
//! rolling month and nothing before it, which is the reason the archive
//! provider exists. So the division of labour is **this one for the recent
//! tail, the archive for depth** — and because the columns line up, the two
//! CSVs concatenate (see *Where it differs* for the two that agree only
//! approximately).
//!
//! A column with no sample in a bar reads as an *absent* sample, never a zero:
//! at `[1d]` a bar older than the positioning window carries bars, funding and
//! premium index and simply no `open_interest`. For an accrual that difference
//! is "no carry recorded" against "carry was nil".
//!
//! # Where it differs from the archive
//!
//! Same columns, same order, same aggregation — and, on a day both cover,
//! `open_interest` and `open_interest_value` come back **bit-identical**. Two
//! columns do not, and neither is a defect on either side:
//!
//! * **The three account ratios are rounded.** `fapi` publishes
//!   `longShortRatio` to four decimals where the archive carries eight, so the
//!   two agree to ~2e-4 relative and no further.
//! * **`taker_long_short_ratio` is a different statistic at a coarse bar.**
//!   It is the only `/futures/data` feed that *accrues*: the live endpoint
//!   answers the buy/sell volume ratio accumulated over the whole requested
//!   period, so at `[1h]` the bar carries its own hour of taker flow, while
//!   the archive — which is published at 5-minute granularity and nothing
//!   coarser — carries the last 5-minute sample inside that hour. The live
//!   answer is the better summary of the bar; it is simply not the same
//!   number. At `[5m]` they agree.
//!
//! # Aggregation
//!
//! Identical to the archive's, and shared with it in code (`sources::bucket`'s
//! `Fold`): **funding is summed** over the bar — it is
//! a cost that accrues, so `[1d]` is that day's total carry — and everything
//! else is a **level**, keeping the newest sample by that sample's own
//! timestamp.
//!
//! ```sh
//! fugazi get binance-futures:BTCUSDT[1h] --since '7d ago' -o recent.csv
//! ```
//!
//! # Errors
//!
//! Mapped as the spot [`Binance`](super::Binance) client maps them: HTTP
//! `429`/`418` → [`SourceError::RateLimited`], a `{"code":-1121,…}` body →
//! [`SourceError::UnknownSymbol`], any other non-2xx → [`SourceError::Http`],
//! a shape the decode doesn't recognise → [`SourceError::Decode`]. A failure
//! on **any** feed abandons the fetch: a series silently missing its funding
//! column is indistinguishable from a contract that never charged any.

use std::future::Future;
use std::sync::Arc;
use std::time::Duration;

use serde::Deserialize;

use crate::types::{Atom, Candle, Real, Schema};

use super::binance_vision::{Market, binance_vision_schema};
use super::bucket::{Aggregation, Fold};
use super::{Interval, SeriesSource, SourceError, Timestamp};

const DEFAULT_BASE_URL: &str = "https://fapi.binance.com";
/// Klines page size. `fapi`'s ceiling is 1500, against spot's 1000.
const KLINE_PAGE: usize = 1500;
/// Funding-history page size, the endpoint's own ceiling.
const FUNDING_PAGE: usize = 1000;
/// `/futures/data/*` page size, the endpoints' own ceiling.
const STATS_PAGE: usize = 500;
const DEFAULT_MIN_DELAY_MS: u64 = 100;
/// How far back `/futures/data/*` serves. Binance publishes a rolling month;
/// asking for anything older returns an empty page, so the cursor starts here
/// rather than paging through years of guaranteed-empty responses.
const STATS_HORIZON_MS: i64 = 30 * 24 * 60 * 60 * 1000;

/// The columns a `binance-futures` fetch publishes — **the same object** the
/// archive's futures tree publishes, deliberately.
///
/// The two providers cover different halves of one instrument's history, so a
/// caller that concatenates them (or swaps one for the other) needs the column
/// set and its order to be identical rather than merely similar. Sharing the
/// `OnceLock` makes that a compile-time fact instead of a convention two files
/// apart: there is nothing here to drift.
pub fn binance_futures_schema() -> &'static Arc<Schema> {
    binance_vision_schema(Market::UsdMFutures)
}

/// A Binance USDⓈ-M futures client.
///
/// Cheap to clone (the inner [`reqwest::Client`] is `Arc`-backed).
///
/// The `symbol` domain is the **perpetual contract's**, not spot's: they
/// mostly coincide (`BTCUSDT`, `ETHUSDT`) but are not the same list, and
/// [`SeriesSource::tickers`] enumerates the contracts — the perpetuals of
/// `/fapi/v1/exchangeInfo`, which is also what
/// `fugazi list tickers binance-futures` prints.
#[derive(Debug, Clone)]
pub struct BinanceFutures {
    client: reqwest::Client,
    base_url: String,
    kline_page: usize,
    min_delay_between_requests: Duration,
    side_channels: bool,
}

impl Default for BinanceFutures {
    fn default() -> Self {
        Self::new()
    }
}

impl BinanceFutures {
    /// A client pointing at the public `fapi` endpoint with sensible defaults
    /// (1500 klines per page, 100 ms between pages, side channels on).
    pub fn new() -> Self {
        Self {
            client: reqwest::Client::new(),
            base_url: DEFAULT_BASE_URL.to_string(),
            kline_page: KLINE_PAGE,
            min_delay_between_requests: Duration::from_millis(DEFAULT_MIN_DELAY_MS),
            side_channels: true,
        }
    }

    /// Override the API base URL (`https://fapi.binance.com` by default).
    /// Primarily useful for testing against a local `wiremock` server.
    pub fn with_base_url(mut self, url: impl Into<String>) -> Self {
        self.base_url = url.into();
        self
    }

    /// Override the max klines per HTTP request (default and maximum 1500).
    pub fn with_max_per_request(mut self, n: usize) -> Self {
        self.kline_page = n.clamp(1, KLINE_PAGE);
        self
    }

    /// Override the delay between successive requests (default 100 ms).
    pub fn with_min_delay(mut self, d: Duration) -> Self {
        self.min_delay_between_requests = d;
        self
    }

    /// Fetch bars only, skipping the seven side-channel feeds.
    ///
    /// The columns stay in the schema — this is a client that leaves them
    /// unsampled, not a different series — so an overlay naming one still
    /// builds and simply reads absent. Worth it when the perpetual's *price*
    /// is all that is wanted: it turns eight paginated feeds back into one.
    pub fn bars_only(mut self) -> Self {
        self.side_channels = false;
        self
    }

    /// The feeds this client fetches, in fold order.
    fn feeds(&self) -> &'static [Feed] {
        if self.side_channels {
            Feed::ALL
        } else {
            &[Feed::Klines]
        }
    }
}

impl SeriesSource for BinanceFutures {
    fn name(&self) -> &'static str {
        "binance-futures"
    }

    fn schema(&self) -> Option<Arc<Schema>> {
        Some(binance_futures_schema().clone())
    }

    /// Every perpetual currently trading, sorted. Read from
    /// `/fapi/v1/exchangeInfo` — the contract vocabulary, which is a different
    /// list from spot's `/api/v3/exchangeInfo` and not interchangeable with it.
    fn tickers(&self) -> impl Future<Output = Result<Vec<String>, SourceError>> + Send {
        let client = self.client.clone();
        let url = format!(
            "{}/fapi/v1/exchangeInfo",
            self.base_url.trim_end_matches('/')
        );
        async move {
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
        let base = self.base_url.trim_end_matches('/').to_string();
        let kline_page = self.kline_page;
        let min_delay = self.min_delay_between_requests;
        let feeds = self.feeds();
        async move {
            let token = interval_to_token(interval)?;
            let (stats_period, stats_period_ms) = stats_period(interval);
            let until_ms = until.map(|t| t.0).unwrap_or_else(|| Timestamp::now().0);

            // The feeds are independent endpoints, so they run concurrently —
            // a five-year fetch is six kline/funding pages against seven
            // single-page statistics, and running them one behind the other
            // would be dominated by round-trip latency. Each feed pages
            // *sequentially within itself*: its cursor is the last row it saw.
            let mut set: tokio::task::JoinSet<(usize, Result<Vec<Sample>, SourceError>)> =
                tokio::task::JoinSet::new();
            for (nth, &feed) in feeds.iter().enumerate() {
                let req = Request {
                    client: client.clone(),
                    base: base.clone(),
                    symbol: symbol.clone(),
                    token,
                    stats_period,
                    stats_period_ms,
                    kline_page,
                    min_delay,
                    since: since.0,
                    until: until_ms,
                };
                set.spawn(async move { (nth, feed.fetch(req).await) });
            }

            let mut fetched: Vec<(usize, Feed, Vec<Sample>)> = Vec::with_capacity(feeds.len());
            while let Some(joined) = set.join_next().await {
                let (nth, got) =
                    joined.map_err(|e| SourceError::Decode(format!("feed task panicked: {e}")))?;
                fetched.push((nth, feeds[nth], got?));
            }
            // Fold in feed order, not completion order: `Aggregation::Last`
            // breaks a tie between two feeds carrying the same instant by
            // which folded later, and floating-point addition is not
            // associative. Either would otherwise make the series depend on
            // what the network settled first.
            fetched.sort_unstable_by_key(|&(nth, ..)| nth);

            let mut fold = Fold::new(
                binance_futures_schema().clone(),
                interval,
                since.0,
                until_ms,
            );
            for (_, feed, samples) in &fetched {
                let agg = feed.aggregation();
                for sample in samples {
                    if let Some(candle) = sample.candle {
                        fold.bar(sample.time, candle);
                    }
                    for &(slot, value) in &sample.values {
                        fold.sample(sample.time, slot, value, agg);
                    }
                }
            }
            Ok(fold.finish())
        }
    }
}

/// One row of one feed, already projected onto schema slots.
#[derive(Debug, Clone, PartialEq)]
struct Sample {
    /// The sample's own timestamp, epoch milliseconds, before bucketing.
    time: i64,
    /// The bar, for the one feed that carries one.
    candle: Option<Candle>,
    /// `(schema slot, value)` — small and fixed per feed, so inline rather
    /// than a map.
    values: Vec<(usize, Real)>,
}

/// Everything one feed's pagination loop needs, so the spawn site doesn't
/// repeat nine `clone`s per feed.
#[derive(Debug, Clone)]
struct Request {
    client: reqwest::Client,
    base: String,
    symbol: String,
    /// The kline interval token, for the two feeds partitioned by cadence.
    token: &'static str,
    /// The `/futures/data/*` sampling period — a coarser vocabulary than the
    /// klines'. See [`stats_period`].
    stats_period: &'static str,
    /// That period's duration, which is also how far a statistics sample's
    /// timestamp has to move. See [`Feed::stamp_shift`].
    stats_period_ms: i64,
    kline_page: usize,
    min_delay: Duration,
    since: i64,
    until: i64,
}

/// One `fapi` endpoint this provider reads.
///
/// They differ in path, in page size, in how far back they serve, in whether
/// their rows are kline arrays or objects, and in which schema columns they
/// fill — but not in how they page, which is `startTime` forward until a short
/// page.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Feed {
    /// The perpetual's own klines: the candle, plus the kline's order-flow
    /// extras. Pairing the funding rate with the contract it is charged on is
    /// the point — a spot bar and a perp funding rate are different
    /// instruments.
    Klines,
    /// `premiumIndexKlines` — the basis `(mark - index) / index`, whose
    /// *close* is the column.
    Premium,
    /// Settlement events, every 4–8h. The one accrual here.
    Funding,
    /// Open interest, in contracts and in quote currency. 30-day horizon.
    OpenInterest,
    /// Long/short account ratio across all accounts. 30-day horizon.
    LongShortRatio,
    /// Long/short ratio among top accounts, counted by account. 30-day horizon.
    TopAccountRatio,
    /// Long/short ratio among top accounts, weighted by position size. 30-day
    /// horizon.
    TopPositionRatio,
    /// Taker buy volume against taker sell volume. 30-day horizon.
    TakerRatio,
}

impl Feed {
    /// Every feed, in the order their samples are folded.
    const ALL: &'static [Feed] = &[
        Feed::Klines,
        Feed::Premium,
        Feed::Funding,
        Feed::OpenInterest,
        Feed::LongShortRatio,
        Feed::TopAccountRatio,
        Feed::TopPositionRatio,
        Feed::TakerRatio,
    ];

    fn path(self) -> &'static str {
        match self {
            Feed::Klines => "/fapi/v1/klines",
            Feed::Premium => "/fapi/v1/premiumIndexKlines",
            Feed::Funding => "/fapi/v1/fundingRate",
            Feed::OpenInterest => "/futures/data/openInterestHist",
            Feed::LongShortRatio => "/futures/data/globalLongShortAccountRatio",
            Feed::TopAccountRatio => "/futures/data/topLongShortAccountRatio",
            Feed::TopPositionRatio => "/futures/data/topLongShortPositionRatio",
            Feed::TakerRatio => "/futures/data/takerlongshortRatio",
        }
    }

    /// Funding is the only accrual: it is charged, so settlements inside a bar
    /// add up. Open interest is a stock and every ratio is a proportion.
    fn aggregation(self) -> Aggregation {
        match self {
            Feed::Funding => Aggregation::Sum,
            _ => Aggregation::Last,
        }
    }

    /// How this feed's rows are spelled, and which columns they fill.
    fn shape(self) -> Shape {
        match self {
            // Extras at kline indexes 7 / 8 / 9 / 10 → schema slots 0..=3;
            // index 6 is closeTime and 11 is "ignore".
            Feed::Klines => Shape::Kline {
                candle: true,
                fields: &[(7, 0), (8, 1), (9, 2), (10, 3)],
            },
            // The premium index has no volume worth carrying, and its bar
            // would overwrite the contract's own — only the close is a column.
            Feed::Premium => Shape::Kline {
                candle: false,
                fields: &[(4, 5)],
            },
            Feed::Funding => Shape::Records {
                time: "fundingTime",
                fields: &[("fundingRate", 4)],
            },
            Feed::OpenInterest => Shape::Records {
                time: "timestamp",
                fields: &[("sumOpenInterest", 6), ("sumOpenInterestValue", 7)],
            },
            Feed::LongShortRatio => Shape::Records {
                time: "timestamp",
                fields: &[("longShortRatio", 8)],
            },
            Feed::TopAccountRatio => Shape::Records {
                time: "timestamp",
                fields: &[("longShortRatio", 9)],
            },
            Feed::TopPositionRatio => Shape::Records {
                time: "timestamp",
                fields: &[("longShortRatio", 10)],
            },
            Feed::TakerRatio => Shape::Records {
                time: "timestamp",
                fields: &[("buySellRatio", 11)],
            },
        }
    }

    /// Rows per request — each endpoint family's own ceiling.
    fn page_size(self) -> usize {
        match self {
            Feed::Klines | Feed::Premium => KLINE_PAGE,
            Feed::Funding => FUNDING_PAGE,
            _ => STATS_PAGE,
        }
    }

    /// Whether this feed only serves the last [`STATS_HORIZON_MS`].
    fn is_windowed(self) -> bool {
        !matches!(self, Feed::Klines | Feed::Premium | Feed::Funding)
    }

    /// Whether this feed labels a row by the **close** of the window it
    /// covers rather than by its open.
    ///
    /// The `/futures/data/*` family is not consistent about this, and the two
    /// halves are told apart by what they measure rather than by anything in
    /// the response. The four **snapshots** — open interest and the three
    /// account ratios — are stamped at the close: the hourly row stamped
    /// `01:00` carries the reading taken inside `[00:00, 01:00)`, and the
    /// 5-minute row stamped `00:05` carries the one the archive files under
    /// `00:00`. `takerlongshortRatio` is not a snapshot but the buy/sell
    /// **volume ratio accumulated over** `[t, t+period)`, and like a kline it
    /// is stamped at the open.
    ///
    /// Both spellings were verified value-for-value against the `metrics`
    /// archive, which stamps every sample by the window's open: live `t`
    /// equals archive `t - period` for the four, and archive `t` for the
    /// taker ratio. Getting it wrong is silent — a level one bar stale, and
    /// one bar away from where `binance-vision-futures` puts the identical
    /// number while the two claim one schema.
    fn labels_by_close(self) -> bool {
        matches!(
            self,
            Feed::OpenInterest
                | Feed::LongShortRatio
                | Feed::TopAccountRatio
                | Feed::TopPositionRatio
        )
    }

    /// How far back a sample's timestamp has to move to land on the bar it
    /// measured — one period for the four close-labelled snapshots, nothing
    /// for everything else. See [`labels_by_close`](Self::labels_by_close).
    fn stamp_shift(self, req: &Request) -> i64 {
        if self.labels_by_close() {
            req.stats_period_ms
        } else {
            0
        }
    }

    /// The half-open range to page over, **in the endpoint's own stamps**.
    ///
    /// Two adjustments, each from one property of the feed: a statistics feed
    /// starts no earlier than the horizon it serves, and a close-labelled one
    /// ends one period *past* `until`, because the row that fills the last bar
    /// is stamped one period beyond it.
    fn window(self, req: &Request) -> (i64, i64) {
        let start = if self.is_windowed() {
            // Nothing older than the horizon exists, and a request for it is
            // not an error — it is an empty page per step, all the way back to
            // `since`. Start where the data does.
            req.since.max(req.until - STATS_HORIZON_MS)
        } else {
            req.since
        };
        (start, req.until.saturating_add(self.stamp_shift(req)))
    }

    /// This feed's query parameters for one page starting at `cursor`.
    fn query(
        self,
        req: &Request,
        cursor: i64,
        end: i64,
        limit: usize,
    ) -> Vec<(&'static str, String)> {
        let mut query = vec![
            ("symbol", req.symbol.clone()),
            ("startTime", cursor.to_string()),
            ("limit", limit.to_string()),
        ];
        match self {
            Feed::Klines | Feed::Premium => query.push(("interval", req.token.to_string())),
            Feed::Funding => {}
            _ => query.push(("period", req.stats_period.to_string())),
        }
        // The window is half-open; every `fapi` endTime is inclusive of a row
        // stamped exactly at it.
        query.push(("endTime", end.saturating_sub(1).to_string()));
        query
    }

    /// Page this feed from `since` (or the horizon, whichever is later) until
    /// a short page, an empty page, or the cursor crossing `until`.
    async fn fetch(self, req: Request) -> Result<Vec<Sample>, SourceError> {
        let url = format!("{}{}", req.base, self.path());
        let limit = match self {
            Feed::Klines => req.kline_page,
            other => other.page_size(),
        };
        let (mut cursor, end) = self.window(&req);
        let mut out: Vec<Sample> = Vec::new();
        let mut first = true;

        while cursor < end {
            if !first {
                tokio::time::sleep(req.min_delay).await;
            }
            first = false;

            let resp = req
                .client
                .get(&url)
                .query(&self.query(&req, cursor, end, limit))
                .send()
                .await?;
            if !resp.status().is_success() {
                return Err(map_http_error(resp).await);
            }
            let rows: Vec<serde_json::Value> = resp
                .json()
                .await
                .map_err(|e| SourceError::Decode(format!("{} JSON: {e}", self.path())))?;
            let page_len = rows.len();
            if page_len == 0 {
                break;
            }
            let before = out.len();
            for row in &rows {
                out.push(self.decode(row)?);
            }

            // Advance past the newest row this page carried. `max` rather than
            // "the last one": every one of these endpoints answers ascending,
            // but a cursor that trusts that and is wrong loops forever.
            let newest = out[before..]
                .iter()
                .map(|s| s.time)
                .max()
                .expect("the page was not empty");
            let next = newest.saturating_add(1);
            if next <= cursor {
                // Defensive: an anomaly in the response could stall the loop.
                break;
            }
            cursor = next;

            // A short page means the endpoint had nothing more in the window.
            if page_len < limit {
                break;
            }
        }

        // After paging, never during it: the cursor walks the endpoint's own
        // stamps, and shifting under it would re-request the last page.
        let shift = self.stamp_shift(&req);
        for sample in &mut out {
            sample.time -= shift;
        }
        Ok(out)
    }

    /// Decode one row into a [`Sample`], reading only the fields this feed
    /// contributes.
    ///
    /// A field that is absent or unparseable is **dropped**, not zeroed:
    /// `Fold` treats a missing slot as an absent sample, which is the honest
    /// answer for a row the endpoint served without it.
    fn decode(self, row: &serde_json::Value) -> Result<Sample, SourceError> {
        match self.shape() {
            Shape::Kline { candle, fields } => {
                let arr = row.as_array().ok_or_else(|| {
                    SourceError::Decode(format!("{}: kline is not a JSON array", self.path()))
                })?;
                if arr.len() < 6 {
                    return Err(SourceError::Decode(format!(
                        "{}: kline row has {} fields, expected at least 6",
                        self.path(),
                        arr.len()
                    )));
                }
                let time = arr[0].as_i64().ok_or_else(|| {
                    SourceError::Decode(format!(
                        "{}: kline openTime is not an integer",
                        self.path()
                    ))
                })?;
                let bar = if candle {
                    Some(Candle::new(
                        parse_num(&arr[1], "open")?,
                        parse_num(&arr[2], "high")?,
                        parse_num(&arr[3], "low")?,
                        parse_num(&arr[4], "close")?,
                        parse_num(&arr[5], "volume")?,
                    ))
                } else {
                    None
                };
                let values = fields
                    .iter()
                    .filter_map(|&(index, slot)| {
                        let v = arr.get(index)?;
                        parse_num(v, "kline field").ok().map(|x| (slot, x))
                    })
                    .collect();
                Ok(Sample {
                    time,
                    candle: bar,
                    values,
                })
            }
            Shape::Records { time, fields } => {
                let obj = row.as_object().ok_or_else(|| {
                    SourceError::Decode(format!("{}: row is not a JSON object", self.path()))
                })?;
                let at = obj.get(time).and_then(|v| v.as_i64()).ok_or_else(|| {
                    SourceError::Decode(format!("{}: row has no integer `{time}`", self.path()))
                })?;
                let values = fields
                    .iter()
                    .filter_map(|&(key, slot)| {
                        let v = obj.get(key)?;
                        parse_num(v, key).ok().map(|x| (slot, x))
                    })
                    .collect();
                Ok(Sample {
                    time: at,
                    candle: None,
                    values,
                })
            }
        }
    }
}

/// How a feed's rows are spelled.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Shape {
    /// A heterogeneous kline array. `fields` is `(array index, schema slot)`.
    Kline {
        /// Whether this feed's OHLCV *is* the series' bar. Only the contract's
        /// own klines are; the premium index rides in a column.
        candle: bool,
        fields: &'static [(usize, usize)],
    },
    /// An object per row, timestamped by the named field. `fields` is
    /// `(JSON key, schema slot)`.
    Records {
        time: &'static str,
        fields: &'static [(&'static str, usize)],
    },
}

/// Map an [`Interval`] to `fapi`'s kline token vocabulary — the same one spot
/// speaks.
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

/// The `/futures/data/*` sampling period to request for a run at `interval`.
///
/// Those endpoints speak a **coarser and different** vocabulary than the
/// klines — no `1m`, no `3m`, no `8h`, nothing above `1d` — so the requested
/// cadence cannot simply be forwarded. Every column they serve is a level and
/// the fold keeps a bar's newest sample, so **sampling finer than the bar is
/// always correct** and only costs requests; sampling coarser would leave bars
/// unsampled. Hence: the coarsest supported period that still fits inside the
/// bar, floored at `5m` for a bar shorter than any of them. The duration comes
/// back with the token because [`Feed::stamp_shift`] is exactly one period.
fn stats_period(interval: Interval) -> (&'static str, i64) {
    const PERIODS: &[(i64, &str)] = &[
        (86_400_000, "1d"),
        (43_200_000, "12h"),
        (21_600_000, "6h"),
        (14_400_000, "4h"),
        (7_200_000, "2h"),
        (3_600_000, "1h"),
        (1_800_000, "30m"),
        (900_000, "15m"),
        (300_000, "5m"),
    ];
    let bar = interval.duration_ms();
    PERIODS
        .iter()
        .find(|&&(ms, _)| ms <= bar)
        .map(|&(ms, token)| (token, ms))
        .unwrap_or(("5m", 300_000))
}

/// Binance returns most numbers as JSON strings. Also accept a bare JSON
/// number, so a mock server that returns typed numbers still works.
fn parse_num(v: &serde_json::Value, field: &str) -> Result<Real, SourceError> {
    match v {
        serde_json::Value::String(s) => s
            .parse::<Real>()
            .map_err(|e| SourceError::Decode(format!("`{field}` = {s:?}: {e}"))),
        serde_json::Value::Number(n) => n
            .as_f64()
            .ok_or_else(|| SourceError::Decode(format!("`{field}` is not finite"))),
        other => Err(SourceError::Decode(format!(
            "`{field}` has unexpected JSON type: {other}"
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
    if let Ok(err) = serde_json::from_str::<FuturesError>(&body)
        && err.code == -1121
    {
        return SourceError::UnknownSymbol(err.msg);
    }
    SourceError::Http { status: code, body }
}

#[derive(Deserialize)]
struct FuturesError {
    code: i64,
    msg: String,
}

/// The subset of `/fapi/v1/exchangeInfo` this crate reads — the symbol
/// vocabulary and the two fields that decide whether an entry belongs to it.
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
    fn the_schema_is_the_archives_futures_schema() {
        // Not "has the same columns" — the same object. A caller concatenating
        // an archive fetch with a live one needs the order to match too.
        let live = binance_futures_schema();
        let archive = binance_vision_schema(Market::UsdMFutures);
        assert!(Arc::ptr_eq(live, archive));
        assert_eq!(
            live.keys().collect::<Vec<_>>(),
            [
                "quote_volume",
                "n_trades",
                "taker_buy_base_volume",
                "taker_buy_quote_volume",
                "funding_rate",
                "premium_index",
                "open_interest",
                "open_interest_value",
                "long_short_ratio",
                "top_trader_account_ratio",
                "top_trader_position_ratio",
                "taker_long_short_ratio",
            ]
        );
    }

    #[test]
    fn every_feed_fills_slots_that_exist_and_no_two_share_one() {
        let schema = binance_futures_schema();
        let mut seen: Vec<usize> = Vec::new();
        for &feed in Feed::ALL {
            let slots: Vec<usize> = match feed.shape() {
                Shape::Kline { fields, .. } => fields.iter().map(|&(_, slot)| slot).collect(),
                Shape::Records { fields, .. } => fields.iter().map(|&(_, slot)| slot).collect(),
            };
            for slot in slots {
                assert!(slot < schema.len(), "{feed:?} writes slot {slot}");
                assert!(!seen.contains(&slot), "{feed:?} writes slot {slot} twice");
                seen.push(slot);
            }
        }
        seen.sort_unstable();
        // Every column is fed by something: a schema slot nothing writes would
        // be a column that always reads absent.
        assert_eq!(seen, (0..schema.len()).collect::<Vec<_>>());
    }

    #[test]
    fn interval_tokens_map_correctly() {
        assert_eq!(interval_to_token(Interval::Minute(1)).unwrap(), "1m");
        assert_eq!(interval_to_token(Interval::Hour(8)).unwrap(), "8h");
        assert_eq!(interval_to_token(Interval::Day(1)).unwrap(), "1d");
        assert_eq!(interval_to_token(Interval::Month(1)).unwrap(), "1M");
        assert!(matches!(
            interval_to_token(Interval::Minute(7)),
            Err(SourceError::UnsupportedInterval(_))
        ));
    }

    #[test]
    fn stats_period_never_exceeds_the_bar() {
        // The vocabulary is coarser than the klines': `8h` has no period of
        // its own and falls to `6h`, and a bar below `5m` takes the floor.
        assert_eq!(stats_period(Interval::Day(1)), ("1d", 86_400_000));
        assert_eq!(stats_period(Interval::Week(1)), ("1d", 86_400_000));
        assert_eq!(stats_period(Interval::Hour(8)), ("6h", 21_600_000));
        assert_eq!(stats_period(Interval::Hour(1)), ("1h", 3_600_000));
        assert_eq!(stats_period(Interval::Minute(5)), ("5m", 300_000));
        assert_eq!(stats_period(Interval::Minute(1)), ("5m", 300_000));
        // The duration is the shift, so a token whose milliseconds were wrong
        // would move every level by the wrong amount rather than fail here.
        for interval in [Interval::Day(1), Interval::Hour(1), Interval::Minute(5)] {
            let (token, ms) = stats_period(interval);
            assert_eq!(
                Some(ms),
                PERIOD_MS_BY_TOKEN
                    .iter()
                    .find(|(t, _)| *t == token)
                    .map(|&(_, ms)| ms)
            );
        }
    }

    /// The vocabulary spelled out independently of the table under test.
    const PERIOD_MS_BY_TOKEN: &[(&str, i64)] = &[
        ("5m", 300_000),
        ("15m", 900_000),
        ("30m", 1_800_000),
        ("1h", 3_600_000),
        ("2h", 7_200_000),
        ("4h", 14_400_000),
        ("6h", 21_600_000),
        ("12h", 43_200_000),
        ("1d", 86_400_000),
    ];

    #[test]
    fn only_the_statistics_feeds_move_their_stamps() {
        let req = Request {
            client: reqwest::Client::new(),
            base: String::new(),
            symbol: "BTCUSDT".into(),
            token: "1h",
            stats_period: "1h",
            stats_period_ms: 3_600_000,
            kline_page: KLINE_PAGE,
            min_delay: Duration::ZERO,
            since: 0,
            until: 86_400_000,
        };
        // A kline is stamped by open time and a funding row is an event; the
        // four snapshots are labelled by their window's close.
        for feed in [Feed::Klines, Feed::Premium, Feed::Funding] {
            assert_eq!(feed.stamp_shift(&req), 0, "{feed:?}");
        }
        for feed in [
            Feed::OpenInterest,
            Feed::LongShortRatio,
            Feed::TopAccountRatio,
            Feed::TopPositionRatio,
        ] {
            assert_eq!(feed.stamp_shift(&req), 3_600_000, "{feed:?}");
        }
        // The taker ratio is the one `/futures/data` feed that is an accrued
        // volume ratio rather than a snapshot, so it is stamped at the open
        // like a kline — moving it would be a real one-bar error, not a
        // cosmetic one.
        assert_eq!(Feed::TakerRatio.stamp_shift(&req), 0);

        // …and only a close-labelled feed's request stretches one period past
        // `until`, where the row that fills the last bar is.
        assert_eq!(Feed::Klines.window(&req), (0, 86_400_000));
        assert_eq!(Feed::TakerRatio.window(&req), (0, 86_400_000));
        assert_eq!(Feed::OpenInterest.window(&req), (0, 86_400_000 + 3_600_000));
    }

    #[test]
    fn kline_rows_decode_into_a_bar_and_its_extras() {
        let row = serde_json::json!([
            1_700_000_000_000_i64,
            "27000.50",
            "27100.00",
            "26950.10",
            "27050.75",
            "12.345",
            1_700_003_599_999_i64,
            "334000.00",
            42,
            "6.0",
            "162500.00",
            "0"
        ]);
        let sample = Feed::Klines.decode(&row).unwrap();
        assert_eq!(sample.time, 1_700_000_000_000);
        let candle = sample.candle.expect("the contract's klines carry the bar");
        assert_eq!(candle.close, 27050.75);
        assert_eq!(candle.volume, 12.345);
        assert_eq!(
            sample.values,
            vec![(0, 334000.0), (1, 42.0), (2, 6.0), (3, 162500.0)]
        );
    }

    #[test]
    fn premium_klines_contribute_a_column_and_no_bar() {
        let row = serde_json::json!([
            1_700_000_000_000_i64,
            "0.0001",
            "0.0004",
            "-0.0002",
            "0.0003",
            "0"
        ]);
        let sample = Feed::Premium.decode(&row).unwrap();
        assert!(
            sample.candle.is_none(),
            "the premium index must not overwrite the contract's bar"
        );
        assert_eq!(sample.values, vec![(5, 0.0003)]);
    }

    #[test]
    fn record_rows_decode_by_key() {
        let funding = serde_json::json!({
            "symbol": "BTCUSDT",
            "fundingTime": 1_700_000_000_000_i64,
            "fundingRate": "0.0001",
            "markPrice": "27000.0",
        });
        let sample = Feed::Funding.decode(&funding).unwrap();
        assert_eq!(sample.time, 1_700_000_000_000);
        assert_eq!(sample.values, vec![(4, 0.0001)]);

        let oi = serde_json::json!({
            "symbol": "BTCUSDT",
            "sumOpenInterest": "20403.63700000",
            "sumOpenInterestValue": "150570784.07809979",
            "timestamp": 1_700_000_000_000_i64,
        });
        let sample = Feed::OpenInterest.decode(&oi).unwrap();
        assert_eq!(
            sample.values,
            vec![(6, 20403.637), (7, 150_570_784.078_099_8)]
        );

        let taker = serde_json::json!({
            "buySellRatio": "1.5586",
            "buyVol": "387.3300",
            "sellVol": "248.5030",
            "timestamp": 1_700_000_000_000_i64,
        });
        assert_eq!(
            Feed::TakerRatio.decode(&taker).unwrap().values,
            vec![(11, 1.5586)]
        );
    }

    #[test]
    fn a_row_missing_a_field_drops_it_rather_than_zeroing_it() {
        // Zero is a meaningful value for every one of these columns, so a row
        // served without one has to read as no sample.
        let row = serde_json::json!({ "timestamp": 1_700_000_000_000_i64 });
        let sample = Feed::LongShortRatio.decode(&row).unwrap();
        assert!(sample.values.is_empty());
        // A row with no timestamp at all is a shape this decode does not
        // recognise, which is a different thing from a missing value.
        let untimed = serde_json::json!({ "longShortRatio": "1.0" });
        assert!(matches!(
            Feed::LongShortRatio.decode(&untimed),
            Err(SourceError::Decode(_))
        ));
    }

    #[test]
    fn only_the_windowed_feeds_clamp_to_the_horizon() {
        assert!(!Feed::Klines.is_windowed());
        assert!(!Feed::Premium.is_windowed());
        assert!(!Feed::Funding.is_windowed());
        for feed in [
            Feed::OpenInterest,
            Feed::LongShortRatio,
            Feed::TopAccountRatio,
            Feed::TopPositionRatio,
            Feed::TakerRatio,
        ] {
            assert!(feed.is_windowed(), "{feed:?}");
        }
    }

    #[test]
    fn bars_only_drops_the_side_channel_feeds_but_keeps_the_columns() {
        let client = BinanceFutures::new().bars_only();
        assert_eq!(client.feeds(), &[Feed::Klines]);
        assert_eq!(
            client.schema().expect("fixed schema").len(),
            binance_futures_schema().len(),
            "an unsampled column is still a declared column",
        );
    }
}
