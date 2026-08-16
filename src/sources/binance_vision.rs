//! Binance Vision — the public historical archive — as an [`SeriesSource`].
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
//! Both markets are **candle** sources: every atom carries an OHLCV bar plus
//! the kline's own order-flow extras (`quote_volume`, `n_trades`,
//! `taker_buy_base_volume`, `taker_buy_quote_volume`), the same columns the
//! live [`Binance`](super::Binance) provider exposes. `UsdMFutures` adds the
//! side channels below, which ride alongside the bar rather than replacing it:
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
//! proportions — so a bar keeps its **latest sample by that sample's own
//! timestamp**, the number that was true when the bar ended.
//!
//! "Latest by timestamp" rather than "last one folded in" is load-bearing,
//! because the archives do not partition cleanly by bucket: a `metrics` file
//! for day *D* runs from `D 00:05` to a closing row stamped a second or two
//! into *D+1*, so the bucket at each midnight is written by two different
//! files. The fetches run concurrently, so which of the two lands first is
//! whatever the network decided — and a fold that took the last writer handed
//! back a different series on every run.
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

use crate::types::{Atom, Candle, OverlayInfo, OverlayValue, Real, Schema};

use super::{Interval, SeriesSource, SourceError, Timestamp, floor_to_bucket};

/// Which of the archive's two trees a client reads.
///
/// They are different instruments, not two spellings of one: a perpetual's
/// funding rate belongs to the contract it is charged on, and pairing it with a
/// spot bar would quietly assert the two are the same thing. So the market is
/// chosen at construction and decides both the paths and the schema.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Market {
    /// `spot/` — the exchange's cash market. OHLCV plus the kline's own
    /// order-flow extras, the same four columns the live `binance` provider
    /// exposes.
    Spot,
    /// `futures/um/` — USDⓈ-M perpetuals. Everything spot carries, plus the
    /// funding rate, the premium index and the positioning statistics that only
    /// exist for a derivative.
    UsdMFutures,
}

impl Market {
    fn path(self) -> &'static str {
        match self {
            Market::Spot => "spot",
            Market::UsdMFutures => "futures/um",
        }
    }
}

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

/// The columns a client exposes, by market.
///
/// The spot schema is a **prefix** of the futures one, so a slot index means
/// the same column in both and `Archive::columns` can name one set of
/// indexes. Order is the contract those indexes rest on; the two must be
/// changed together.
pub fn binance_vision_schema(market: Market) -> &'static Arc<Schema> {
    static SPOT: OnceLock<Arc<Schema>> = OnceLock::new();
    static FUTURES: OnceLock<Arc<Schema>> = OnceLock::new();
    fn kline_extras(b: &mut crate::market::SchemaBuilder) {
        b.add_real("quote_volume"); // 0
        b.add_real("n_trades"); // 1
        b.add_real("taker_buy_base_volume"); // 2
        b.add_real("taker_buy_quote_volume"); // 3
    }
    match market {
        Market::Spot => SPOT.get_or_init(|| {
            let mut b = Schema::builder();
            kline_extras(&mut b);
            b.finish()
        }),
        Market::UsdMFutures => FUTURES.get_or_init(|| {
            let mut b = Schema::builder();
            kline_extras(&mut b);
            b.add_real("funding_rate"); // 4
            b.add_real("premium_index"); // 5
            b.add_real("open_interest"); // 6 — contracts
            b.add_real("open_interest_value"); // 7 — quote currency
            b.add_real("long_short_ratio"); // 8 — all accounts
            b.add_real("top_trader_account_ratio"); // 9 — top accounts, by count
            b.add_real("top_trader_position_ratio"); // 10 — top accounts, by size
            b.add_real("taker_long_short_ratio"); // 11 — taker buy vs sell volume
            b.finish()
        }),
    }
}

/// A Binance Vision archive client.
///
/// Cheap to clone (the inner [`reqwest::Client`] is `Arc`-backed).
///
/// The `symbol` is a **perpetual contract** symbol (`BTCUSDT`, `ETHUSDT`) —
/// which mostly coincides with the spot vocabulary but is not the same list.
/// Enumerate it with [`SeriesSource::tickers`]
/// (`fugazi list tickers binance-vision`).
#[derive(Debug, Clone)]
pub struct BinanceVision {
    market: Market,
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
    /// A spot client. See [`futures`](Self::futures) for the perpetual tree.
    pub fn new() -> Self {
        Self::for_market(Market::Spot)
    }

    /// A USDⓈ-M perpetuals client.
    pub fn futures() -> Self {
        Self::for_market(Market::UsdMFutures)
    }

    /// A client for `market`.
    pub fn for_market(market: Market) -> Self {
        Self {
            market,
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

impl SeriesSource for BinanceVision {
    fn name(&self) -> &'static str {
        "binance-vision"
    }

    fn schema(&self) -> Option<Arc<Schema>> {
        Some(binance_vision_schema(self.market).clone())
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
        let min_delay = self.min_delay_between_requests;
        let max_in_flight = self.max_in_flight;
        let market = self.market;
        async move {
            let token = interval_token(market, interval)?;
            let schema = binance_vision_schema(market).clone();
            let until_ms = until.map(|t| t.0).unwrap_or_else(|| Timestamp::now().0);
            let base = base_url.trim_end_matches('/').to_string();

            // Every archive this range touches, across all three trees. The
            // list is built up front so the fetches can run concurrently:
            // these are independent static objects, so the unit of parallelism
            // is the file, not a time chunk of one series.
            let mut jobs: Vec<(Archive, String)> = Vec::new();
            for &kind in Archive::all(market) {
                for stamp in kind.periods(since.0, until_ms) {
                    jobs.push((kind, kind.url(market, &base, &symbol, token, &stamp)));
                }
            }

            let fetched = fetch_concurrently(&client, jobs, max_in_flight, min_delay).await?;
            assemble(&fetched, &schema, interval, since.0, until_ms)
        }
    }
}

/// One bucket's value for one column, tagged with the timestamp of the sample
/// that set it.
///
/// The tag is what lets [`Aggregation::Last`] mean *newest sample* rather than
/// *last one folded in*. The two are different questions whenever a bucket is
/// written by more than one archive, which at every UTC midnight it is: a
/// `metrics` file for day *D* closes with a row stamped a second or two into
/// *D+1*, so both that file and *D+1*'s own contribute to *D+1*'s first bucket.
/// Fetches complete out of order, so the last writer is the network's choice
/// and the newest sample is not.
///
/// [`Aggregation::Sum`] doesn't consult it — addition doesn't care which
/// sample came last — but still keeps it current, so the field means the same
/// thing in every cell.
#[derive(Debug, Clone, Copy, PartialEq)]
struct Cell {
    /// Sample timestamp, in epoch milliseconds, before bucketing.
    at: i64,
    value: Real,
}

/// Fold every fetched archive into one [`Atom`] per bucket of `interval`,
/// keeping only samples inside `[since, until)`.
///
/// Split out of [`SeriesSource::atoms`] so the assembly can be exercised
/// against a fixed set of archives in an arbitrary order — which is exactly
/// what the concurrent fetch hands it.
fn assemble(
    fetched: &[(Archive, String, String)],
    schema: &Arc<Schema>,
    interval: Interval,
    since: i64,
    until: i64,
) -> Result<Vec<Atom>, SourceError> {
    // One `bucket -> cell` map per schema column, plus the bars the kline
    // archive contributes.
    let mut columns: Vec<BTreeMap<i64, Cell>> = vec![BTreeMap::new(); schema.len()];
    let mut bars: BTreeMap<i64, Candle> = BTreeMap::new();
    for (kind, url, csv) in fetched {
        if *kind != Archive::Klines {
            continue;
        }
        for (time, candle) in parse_candles(csv, url)? {
            if time < since || time >= until {
                continue;
            }
            bars.insert(floor_to_bucket(time, interval), candle);
        }
    }
    for (kind, url, csv) in fetched {
        for (time, slot, value) in parse_archive(*kind, csv, url)? {
            if time < since || time >= until {
                continue;
            }
            let bucket = floor_to_bucket(time, interval);
            let cell = columns[slot].entry(bucket).or_insert(Cell {
                at: i64::MIN,
                value: 0.0,
            });
            match kind.aggregation() {
                // An accrual: samples inside one bar add up, whatever order
                // they arrive in.
                Aggregation::Sum => {
                    cell.value += value;
                    cell.at = cell.at.max(time);
                }
                // A level: the bar keeps its newest sample. `>=` so that two
                // archives carrying the same instant resolve to the later of
                // them in job order, which `fetch_concurrently` fixes.
                Aggregation::Last => {
                    if time >= cell.at {
                        *cell = Cell { at: time, value };
                    }
                }
            }
        }
    }

    let mut buckets: Vec<i64> = columns
        .iter()
        .flat_map(|c| c.keys().copied())
        .chain(bars.keys().copied())
        .collect();
    buckets.sort_unstable();
    buckets.dedup();

    Ok(buckets
        .into_iter()
        .map(|time| Atom {
            // A bar when the kline archive covered this bucket; `None` for a
            // bucket only the overlay archives reached — early funding history
            // predates nothing, but `metrics` and the klines start at
            // different dates.
            candle: bars.get(&time).copied(),
            time: Some(Timestamp(time)),
            overlays: Some(OverlayInfo::sparse(
                schema.clone(),
                columns
                    .iter()
                    .map(|c| c.get(&time).map(|cell| OverlayValue::Real(cell.value))),
            )),
        })
        .collect())
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
    /// Monthly, partitioned by interval — the **perpetual's own klines**, which
    /// give each atom its candle. Pairing the funding rate with the contract it
    /// is charged on is the point: a spot bar and a perp funding rate are two
    /// different instruments, and joining them by symbol would quietly imply
    /// otherwise. On the same series there is no join at all — one symbol, one
    /// set of atoms carrying both the bar and the side channel.
    Klines,
    /// **Daily**, and the reason this source exists: `fapi`'s
    /// `/futures/data/*` endpoints serve these statistics for the last 30 days
    /// only, while the archive keeps them from 2021. The cost is one file per
    /// day rather than per month — five years is ~1800 requests per symbol,
    /// which is why the fetches run concurrently.
    Metrics,
}

impl Archive {
    /// The archives a market is assembled from. Spot has only its klines: the
    /// funding rate, premium index and positioning statistics describe a
    /// derivative and have no spot counterpart.
    fn all(market: Market) -> &'static [Archive] {
        match market {
            Market::Spot => &[Archive::Klines],
            Market::UsdMFutures => &[
                Archive::Klines,
                Archive::Funding,
                Archive::Premium,
                Archive::Metrics,
            ],
        }
    }

    fn url(self, market: Market, base: &str, symbol: &str, token: &str, stamp: &str) -> String {
        let m = market.path();
        match self {
            Archive::Klines => format!(
                "{base}/data/{m}/monthly/klines/{symbol}/{token}/{symbol}-{token}-{stamp}.zip"
            ),
            Archive::Funding => format!(
                "{base}/data/{m}/monthly/fundingRate/{symbol}/{symbol}-fundingRate-{stamp}.zip"
            ),
            Archive::Premium => format!(
                "{base}/data/{m}/monthly/premiumIndexKlines/{symbol}/{token}/{symbol}-{token}-{stamp}.zip"
            ),
            Archive::Metrics => format!(
                "{base}/data/{m}/daily/metrics/{symbol}/{symbol}-metrics-{stamp}.zip"
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
            Archive::Klines => "open_time",
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
            _ => raw.parse::<i64>().map(to_millis).map_err(|e| e.to_string()),
        }
    }

    /// The value columns this archive contributes, as
    /// `(CSV header, index into the provider schema)`.
    fn columns(self) -> &'static [(&'static str, usize)] {
        match self {
            Archive::Funding => &[("last_funding_rate", 4)],
            Archive::Premium => &[("close", 5)],
            // The candle columns are read separately (see `parse_candles`);
            // these are the kline's side-channel extras.
            Archive::Klines => &[
                ("quote_volume", 0),
                ("count", 1),
                ("taker_buy_volume", 2),
                ("taker_buy_quote_volume", 3),
            ],
            Archive::Metrics => &[
                ("sum_open_interest", 6),
                ("sum_open_interest_value", 7),
                ("count_long_short_ratio", 8),
                ("count_toptrader_long_short_ratio", 9),
                ("sum_toptrader_long_short_ratio", 10),
                ("sum_taker_long_short_vol_ratio", 11),
            ],
        }
    }

    /// The archive's column layout, in file order — the fallback for the
    /// pre-2024 archives that ship no header row. Only names this provider
    /// reads have to be right; the rest are placeholders holding position.
    fn layout(self) -> &'static [&'static str] {
        match self {
            Archive::Funding => &["calc_time", "funding_interval_hours", "last_funding_rate"],
            Archive::Premium | Archive::Klines => &[
                "open_time",
                "open",
                "high",
                "low",
                "close",
                "volume",
                "close_time",
                "quote_volume",
                "count",
                "taker_buy_volume",
                "taker_buy_quote_volume",
                "ignore",
            ],
            Archive::Metrics => &[
                "create_time",
                "symbol",
                "sum_open_interest",
                "sum_open_interest_value",
                "count_toptrader_long_short_ratio",
                "sum_toptrader_long_short_ratio",
                "count_long_short_ratio",
                "sum_taker_long_short_vol_ratio",
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
/// Results come back in **job order**, not completion order. The fold that
/// consumes them is order-sensitive — floating-point addition isn't
/// associative, and [`Aggregation::Last`] has to break ties between two
/// archives carrying the same instant — so handing it whatever order the
/// network happened to settle in would make the assembled series depend on
/// that. Re-ordering a few thousand already-downloaded results costs nothing
/// against the requests that produced them.
///
/// One failed fetch abandons the rest: a partial series would be
/// indistinguishable from a genuinely sparse one.
async fn fetch_concurrently(
    client: &reqwest::Client,
    jobs: Vec<(Archive, String)>,
    max_in_flight: usize,
    min_delay: Duration,
) -> Result<Vec<(Archive, String, String)>, SourceError> {
    /// What one fetch task resolves to: the job's index — which is what fixes
    /// the order results are folded in — then what was asked for and what came
    /// back.
    type Fetched = (usize, Archive, String, Result<Option<String>, SourceError>);

    let mut pending = jobs.into_iter().enumerate();
    let mut set: tokio::task::JoinSet<Fetched> = tokio::task::JoinSet::new();
    let mut out = Vec::new();

    let mut spawn_next = |set: &mut tokio::task::JoinSet<_>| -> bool {
        let Some((nth, (kind, url))) = pending.next() else {
            return false;
        };
        let client = client.clone();
        set.spawn(async move {
            let got = fetch_archive(&client, &url).await;
            (nth, kind, url, got)
        });
        true
    };

    for _ in 0..max_in_flight.max(1) {
        if !spawn_next(&mut set) {
            break;
        }
    }

    while let Some(joined) = set.join_next().await {
        let (nth, kind, url, got) =
            joined.map_err(|e| SourceError::Decode(format!("archive task panicked: {e}")))?;
        if let Some(csv) = got? {
            out.push((nth, kind, url, csv));
        }
        if !min_delay.is_zero() {
            tokio::time::sleep(min_delay).await;
        }
        spawn_next(&mut set);
    }

    out.sort_unstable_by_key(|&(nth, ..)| nth);
    Ok(out
        .into_iter()
        .map(|(_, kind, url, csv)| (kind, url, csv))
        .collect())
}

/// Normalise an archive timestamp to milliseconds.
///
/// Binance changed the unit partway through 2025: an archive written before the
/// switch stamps rows in milliseconds, one written after stamps them in
/// microseconds, and both spellings are still served side by side. Left alone,
/// the newer files land a thousand times past every range filter and simply
/// vanish — no error, just an empty fetch, which is how this was found.
///
/// The two are unambiguous by magnitude: a millisecond epoch for any date this
/// archive covers is 13 digits, a microsecond one is 16, and nothing plausible
/// falls between.
fn to_millis(raw: i64) -> i64 {
    const MICROS_FLOOR: i64 = 100_000_000_000_000; // 1e14 — past any ms epoch
    if raw.abs() >= MICROS_FLOOR { raw / 1_000 } else { raw }
}

/// Resolve the CSV column names to positions, whether or not the archive has a
/// header row.
///
/// Binance only started shipping headers around mid-2024; every archive older
/// than that opens straight on data. Reading by name is still the right default
/// — it is what keeps an added column upstream from shifting the values — but
/// it cannot be the only mode, or the provider works for recent history and
/// fails on everything before it, which is most of what an archive is for.
///
/// A header is detected by its first cell: every archive's timestamp column is
/// an integer, so a first cell that does not parse as one is a name. When there
/// is no header the caller's declared order *is* the layout, which is safe here
/// because a headerless archive predates every column Binance has since added.
fn resolve_columns(
    text: &str,
    wanted: &[&str],
    layout: &[&str],
    url: &str,
) -> Result<(Vec<usize>, bool), SourceError> {
    let first = text.lines().next().unwrap_or_default();
    let first_cell = first.split(',').next().unwrap_or_default().trim();
    let has_header = first_cell.parse::<i64>().is_err();

    let names: Vec<&str> = if has_header {
        first.split(',').map(str::trim).collect()
    } else {
        layout.to_vec()
    };
    let idx = wanted
        .iter()
        .map(|name| {
            names.iter().position(|h| h == name).ok_or_else(|| {
                SourceError::Decode(format!("{url}: missing column `{name}`"))
            })
        })
        .collect::<Result<Vec<_>, _>>()?;
    Ok((idx, has_header))
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
    let time_col = kind.time_column();
    let mut names: Vec<&str> = vec![time_col];
    names.extend(kind.columns().iter().map(|(n, _)| *n));
    let (idx, has_header) = resolve_columns(text, &names, kind.layout(), url)?;
    let i_time = idx[0];
    let wanted: Vec<(usize, usize, &'static str)> = kind
        .columns()
        .iter()
        .zip(&idx[1..])
        .map(|((name, slot), i)| (*i, *slot, *name))
        .collect();

    let mut reader = csv::ReaderBuilder::new()
        .has_headers(has_header)
        .from_reader(text.as_bytes());

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

/// Pull `(epoch_ms, candle)` pairs out of a kline archive. Read by header name
/// like [`parse_archive`], for the same reason: an added column upstream must
/// not shift the values.
fn parse_candles(text: &str, url: &str) -> Result<Vec<(i64, Candle)>, SourceError> {
    let (idx, has_header) = resolve_columns(
        text,
        &["open_time", "open", "high", "low", "close", "volume"],
        Archive::Klines.layout(),
        url,
    )?;

    let mut reader = csv::ReaderBuilder::new()
        .has_headers(has_header)
        .from_reader(text.as_bytes());

    let mut out = Vec::new();
    for record in reader.records() {
        let record = record.map_err(|e| SourceError::Decode(format!("{url}: row: {e}")))?;
        let cell = |i: usize, name: &str| -> Result<Real, SourceError> {
            let raw = record.get(idx[i]).unwrap_or_default().trim();
            raw.parse().map_err(|e| {
                SourceError::Decode(format!("{url}: `{name}` = {raw:?}: {e}"))
            })
        };
        let time = to_millis(cell(0, "open_time")? as i64);
        out.push((
            time,
            Candle::new(
                cell(1, "open")?,
                cell(2, "high")?,
                cell(3, "low")?,
                cell(4, "close")?,
                cell(5, "volume")?,
            ),
        ));
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
fn interval_token(market: Market, interval: Interval) -> Result<&'static str, SourceError> {
    // Spot is klines only, so it admits the whole kline vocabulary. Futures is
    // bounded by `premiumIndexKlines`, which stops at `1d`.
    if market == Market::Spot {
        return match interval {
            Interval::Minute(n @ (1 | 3 | 5 | 15 | 30)) => Ok(match n {
                1 => "1m",
                3 => "3m",
                5 => "5m",
                15 => "15m",
                _ => "30m",
            }),
            Interval::Week(1) => Ok("1w"),
            Interval::Month(1) => Ok("1mo"),
            other => hourly_or_daily_token(other),
        };
    }
    hourly_or_daily_token(interval)
}

/// The tokens both markets share.
fn hourly_or_daily_token(interval: Interval) -> Result<&'static str, SourceError> {
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
        for market in [Market::Spot, Market::UsdMFutures] {
            let schema = binance_vision_schema(market);
            let mut seen = vec![false; schema.len()];
            for &kind in Archive::all(market) {
                for &(_, slot) in kind.columns() {
                    assert!(slot < schema.len(), "{kind:?} indexes past {market:?}");
                    assert!(!seen[slot], "slot {slot} claimed twice in {market:?}");
                    seen[slot] = true;
                }
            }
            assert!(seen.iter().all(|&s| s), "{market:?} has an unsourced slot");
        }
    }

    #[test]
    fn the_spot_schema_is_a_prefix_of_the_futures_one() {
        // The slot indexes in `Archive::columns` are shared between markets, so
        // the shorter schema has to agree column-for-column with the longer.
        let spot = binance_vision_schema(Market::Spot);
        let futures = binance_vision_schema(Market::UsdMFutures);
        assert_eq!(spot.len(), 4);
        assert_eq!(futures.len(), 12);
        for (i, name) in spot.keys().enumerate() {
            assert_eq!(futures.index_of(name), Some(i), "column `{name}`");
        }
    }

    #[test]
    fn the_schema_names_are_stable() {
        let schema = binance_vision_schema(Market::UsdMFutures);
        for (i, name) in [
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
        assert!(interval_token(Market::UsdMFutures, Interval::Minute(1)).is_err());
        assert!(interval_token(Market::UsdMFutures, Interval::Minute(30)).is_err());

        for (interval, token) in [
            (Interval::Hour(1), "1h"),
            (Interval::Hour(2), "2h"),
            (Interval::Hour(4), "4h"),
            (Interval::Hour(6), "6h"),
            (Interval::Hour(8), "8h"),
            (Interval::Hour(12), "12h"),
            (Interval::Day(1), "1d"),
        ] {
            assert_eq!(interval_token(Market::UsdMFutures, interval).unwrap(), token);
        }

        // Above a day the premium archive is not published at all. Admitting
        // these would return funding with a silently empty premium column.
        assert!(interval_token(Market::UsdMFutures, Interval::Day(3)).is_err());
        assert!(interval_token(Market::UsdMFutures, Interval::Week(1)).is_err());
        assert!(interval_token(Market::UsdMFutures, Interval::Month(1)).is_err());

        // Spot is klines only, so it admits the whole vocabulary — including
        // the cadences the futures premium archive does not publish.
        assert_eq!(interval_token(Market::Spot, Interval::Minute(1)).unwrap(), "1m");
        assert_eq!(interval_token(Market::Spot, Interval::Week(1)).unwrap(), "1w");
        assert_eq!(interval_token(Market::Spot, Interval::Month(1)).unwrap(), "1mo");
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
            Archive::Funding.url(Market::UsdMFutures, base, "BTCUSDT", "1d", "2024-01"),
            "https://data.binance.vision/data/futures/um/monthly/fundingRate/BTCUSDT/BTCUSDT-fundingRate-2024-01.zip"
        );
        assert_eq!(
            Archive::Premium.url(Market::UsdMFutures, base, "BTCUSDT", "1d", "2024-01"),
            "https://data.binance.vision/data/futures/um/monthly/premiumIndexKlines/BTCUSDT/1d/BTCUSDT-1d-2024-01.zip"
        );
        assert_eq!(
            Archive::Klines.url(Market::Spot, base, "BTCUSDT", "1d", "2024-01"),
            "https://data.binance.vision/data/spot/monthly/klines/BTCUSDT/1d/BTCUSDT-1d-2024-01.zip"
        );
        assert_eq!(
            Archive::Metrics.url(Market::UsdMFutures, base, "BTCUSDT", "1d", "2024-01-15"),
            "https://data.binance.vision/data/futures/um/daily/metrics/BTCUSDT/BTCUSDT-metrics-2024-01-15.zip"
        );
    }

    #[test]
    fn parses_each_archive_by_header_name() {
        let funding = "calc_time,funding_interval_hours,last_funding_rate\n\
                       1704067200000,8,0.00037409\n\
                       1704096000000,8,0.00027213\n";
        assert_eq!(
            parse_archive(Archive::Funding, funding, "u").unwrap(),
            vec![(1704067200000, 4, 0.00037409), (1704096000000, 4, 0.00027213)],
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
            vec![(1704067200000, 5, 0.00120254)],
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
                (at, 6, 105550.985),
                (at, 7, 6858675634.706254),
                (at, 8, 1.19972635),
                (at, 9, 1.28623339),
                (at, 10, 1.47112400),
                (at, 11, 1.55827200),
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
        assert!(parsed.iter().all(|&(_, slot, _)| slot == 6 || slot == 7));
    }

    #[test]
    fn a_headerless_archive_reads_by_position() {
        // Binance only started shipping header rows partway through 2024, and
        // both spellings are still served. Reading by name has to degrade to
        // the declared layout, or the provider works for recent history and
        // fails on most of the archive.
        let headerless = "1594339200000,0.003989,0.003989,0.003340,0.003535,\
                          7578204800,1594425599999,27852027.907311,168920,\
                          3846820083,14160877.361201,0\n";
        let bars = parse_candles(headerless, "u").unwrap();
        assert_eq!(bars.len(), 1);
        assert_eq!(bars[0].0, 1594339200000);
        assert_eq!(bars[0].1.close, 0.003535);

        // The same file with a header must give the same answer.
        let headed = format!(
            "open_time,open,high,low,close,volume,close_time,quote_volume,count,\
             taker_buy_volume,taker_buy_quote_volume,ignore\n{headerless}"
        );
        assert_eq!(parse_candles(&headed, "u").unwrap(), bars);
    }

    #[test]
    fn microsecond_stamps_normalise_to_milliseconds() {
        // Binance switched the archive's unit during 2025. Left alone the newer
        // rows land a thousand times past every range filter and vanish with no
        // error at all — an empty fetch, which is how this was found.
        assert_eq!(to_millis(1_704_067_200_000), 1_704_067_200_000);
        assert_eq!(to_millis(1_748_736_000_000_000), 1_748_736_000_000);

        let micros = "1748736000000000,104591.88,105866.0,104000.0,105000.0,1.0,\
                      1748822399999999,2.0,3,4.0,5.0,0\n";
        let bars = parse_candles(micros, "u").unwrap();
        assert_eq!(bars[0].0, 1_748_736_000_000);

        // And on the overlay side, which stamps its own columns.
        let funding = "1748736000000000,8,0.0001\n";
        assert_eq!(
            parse_archive(Archive::Funding, funding, "u").unwrap(),
            vec![(1_748_736_000_000, 4, 0.0001)],
        );
    }

    #[test]
    fn a_reordered_header_still_reads_the_right_column() {
        let swapped = "last_funding_rate,calc_time,funding_interval_hours\n\
                       0.00037409,1704067200000,8\n";
        assert_eq!(
            parse_archive(Archive::Funding, swapped, "u").unwrap(),
            vec![(1704067200000, 4, 0.00037409)],
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

    // ---- assembly order-independence -----------------------------------
    //
    // The fixtures below are real rows from
    // `BTCUSDT-metrics-2024-04-0{5,6}.zip`, trimmed to the ones that decide a
    // bucket. Note where each file starts and stops: `…-04-05` runs from
    // `04-05 00:05` to a closing row stamped `04-06 00:00:02`, so it and
    // `…-04-06` both write the 2024-04-06 bucket. That overlap is what made a
    // fold on arrival order non-deterministic.

    const METRICS_HEADER: &str = "create_time,symbol,sum_open_interest,\
        sum_open_interest_value,count_toptrader_long_short_ratio,\
        sum_toptrader_long_short_ratio,count_long_short_ratio,\
        sum_taker_long_short_vol_ratio\n";

    const METRICS_04_05: &str = "2024-04-05 00:05:00,BTCUSDT,78432.716,5360432578.13101,\
         1.42340146,1.20394800,1.47815614,0.37383300\n\
         2024-04-05 23:55:00,BTCUSDT,75257.616,5112326469.8017235,\
         1.77040313,1.22562400,1.62500662,0.66955500\n\
         2024-04-06 00:00:02,BTCUSDT,75172.170,5098412003.9472,\
         1.77730106,1.22438300,1.62792057,0.30651700\n";

    const METRICS_04_06: &str = "2024-04-06 00:05:00,BTCUSDT,75190.008,5092690472.907608,\
         1.77050439,1.22454500,1.62308548,0.63514900\n\
         2024-04-06 23:55:00,BTCUSDT,76614.894,5279801107.328188,\
         1.39970506,1.19442500,1.33473734,0.94815600\n\
         2024-04-07 00:00:00,BTCUSDT,76496.262,5270382006.24284,\
         1.40269659,1.19779800,1.33616894,0.89304100\n";

    fn metrics(day: &str, rows: &str) -> (Archive, String, String) {
        (
            Archive::Metrics,
            format!("metrics-{day}"),
            format!("{METRICS_HEADER}{rows}"),
        )
    }

    /// Every archive a `[1d]` futures fetch of 2024-04-05..08 would collect:
    /// the two overlapping `metrics` days, the perpetual's own klines, the
    /// three funding settlements on 2024-04-06, and the premium index.
    fn every_archive() -> Vec<(Archive, String, String)> {
        vec![
            (
                Archive::Klines,
                "klines-2024-04".to_string(),
                "open_time,open,high,low,close,volume,close_time,quote_volume,count,\
                 taker_buy_volume,taker_buy_quote_volume,ignore\n\
                 1712361600000,68896.0,69700.0,68050.0,69362.6,100.0,1712447999999,\
                 7000000.0,1234,50.0,3500000.0,0\n"
                    .to_string(),
            ),
            (
                Archive::Funding,
                "funding-2024-04".to_string(),
                "calc_time,funding_interval_hours,last_funding_rate\n\
                 1712361600000,8,0.00010000\n\
                 1712390400000,8,0.00002000\n\
                 1712419200000,8,0.00000500\n"
                    .to_string(),
            ),
            (
                Archive::Premium,
                "premium-2024-04".to_string(),
                "open_time,open,high,low,close,volume,close_time,quote_volume,count,\
                 taker_buy_volume,taker_buy_quote_volume,ignore\n\
                 1712361600000,0.00082094,0.00233507,0.00017569,0.00117217,0,\
                 1712447999999,0,17280,0,0,0\n"
                    .to_string(),
            ),
            metrics("2024-04-05", METRICS_04_05),
            metrics("2024-04-06", METRICS_04_06),
        ]
    }

    /// `assemble` over 2024-04-05..08 at `[1d]`, projected into something
    /// comparable — `Atom` carries `Arc`s and no `PartialEq`.
    type Row = (i64, Option<Candle>, Vec<Option<Real>>);
    fn assembled(fetched: &[(Archive, String, String)]) -> Vec<Row> {
        let schema = binance_vision_schema(Market::UsdMFutures).clone();
        assemble(
            fetched,
            &schema,
            Interval::Day(1),
            1712275200000, // 2024-04-05
            1712534400000, // 2024-04-08
        )
        .expect("fixtures parse")
        .into_iter()
        .map(|atom| {
            let values = atom
                .overlays
                .expect("every atom carries the provider schema")
                .values()
                .iter()
                .map(|v| match v {
                    Some(OverlayValue::Real(r)) => Some(*r),
                    _ => None,
                })
                .collect();
            (atom.time.expect("bucketed").0, atom.candle, values)
        })
        .collect()
    }

    #[test]
    fn a_bucket_keeps_its_newest_sample_not_the_file_that_landed_last() {
        // The 2024-04-06 bucket is written by both daily files: `04-06`'s own
        // 23:55 print, and `04-05`'s closing row two seconds past midnight.
        // The one that describes the end of 2024-04-06 is the 23:55 print, so
        // that is what the bar keeps — whichever file the fold saw first.
        for order in [
            vec![
                metrics("2024-04-05", METRICS_04_05),
                metrics("2024-04-06", METRICS_04_06),
            ],
            vec![
                metrics("2024-04-06", METRICS_04_06),
                metrics("2024-04-05", METRICS_04_05),
            ],
        ] {
            let rows = assembled(&order);
            let bucket = rows
                .iter()
                .find(|(time, ..)| *time == 1712361600000)
                .expect("2024-04-06");
            // Slots 6..=11 — the six that moved together in the bug report.
            assert_eq!(bucket.2[6], Some(76614.894), "open_interest");
            assert_eq!(bucket.2[7], Some(5279801107.328188), "open_interest_value");
            assert_eq!(bucket.2[8], Some(1.33473734), "long_short_ratio");
            assert_eq!(bucket.2[9], Some(1.39970506), "top_trader_account_ratio");
            assert_eq!(bucket.2[10], Some(1.194425), "top_trader_position_ratio");
            assert_eq!(bucket.2[11], Some(0.948156), "taker_long_short_ratio");
        }
    }

    #[test]
    fn assembly_does_not_depend_on_the_order_archives_arrive_in() {
        // A concurrent fetch settles in whatever order the network chose, so
        // the fold has to be a function of the archives alone.
        let expected = assembled(&every_archive());
        let n = every_archive().len();
        for skip in 0..n {
            let mut rotated = every_archive();
            rotated.rotate_left(skip);
            assert_eq!(assembled(&rotated), expected, "rotated by {skip}");
        }
        let mut reversed = every_archive();
        reversed.reverse();
        assert_eq!(assembled(&reversed), expected, "reversed");

        // And the fold is still doing its job: the bar has its candle, its
        // three funding settlements summed into one day's carry, and the
        // premium index alongside.
        let (time, candle, values) = expected
            .iter()
            .find(|(time, ..)| *time == 1712361600000)
            .expect("2024-04-06");
        assert_eq!(*time, 1712361600000);
        assert_eq!(candle.expect("kline archive covered it").close, 69362.6);
        assert!(
            (values[4].expect("funding_rate") - 0.000125).abs() < 1e-12,
            "three settlements accrue: {:?}",
            values[4],
        );
        assert_eq!(values[5], Some(0.00117217), "premium_index");
    }

    #[test]
    fn fetches_are_returned_in_job_order_not_completion_order() {
        use wiremock::matchers::{method, path};
        use wiremock::{Mock, MockServer, ResponseTemplate};

        // Hold the first job back so it cannot possibly complete first. Job
        // order has to survive that, or the fold's tie-breaking — and the
        // floating-point sum — depend on the network.
        let body = |csv: &str| {
            let mut buf = Vec::new();
            {
                let mut w = zip::ZipWriter::new(std::io::Cursor::new(&mut buf));
                w.start_file::<_, ()>("a.csv", zip::write::SimpleFileOptions::default())
                    .expect("zip entry");
                std::io::Write::write_all(&mut w, csv.as_bytes()).expect("write");
                w.finish().expect("finish");
            }
            buf
        };

        let runtime = tokio::runtime::Runtime::new().expect("runtime");
        runtime.block_on(async {
            let server = MockServer::start().await;
            for (n, delay_ms) in [(0u32, 300u64), (1, 0), (2, 0)] {
                Mock::given(method("GET"))
                    .and(path(format!("/{n}")))
                    .respond_with(
                        ResponseTemplate::new(200)
                            .set_body_bytes(body(&format!("row-{n}")))
                            .set_delay(Duration::from_millis(delay_ms)),
                    )
                    .mount(&server)
                    .await;
            }

            let jobs = (0..3)
                .map(|n| (Archive::Metrics, format!("{}/{n}", server.uri())))
                .collect();
            let got = fetch_concurrently(&reqwest::Client::new(), jobs, 8, Duration::ZERO)
                .await
                .expect("all three fetch");
            let csvs: Vec<&str> = got.iter().map(|(_, _, csv)| csv.as_str()).collect();
            assert_eq!(csvs, vec!["row-0", "row-1", "row-2"]);
        });
    }

    #[test]
    fn a_missing_archive_is_no_data_and_anything_else_is_an_error() {
        use wiremock::matchers::{method, path};
        use wiremock::{Mock, MockServer, ResponseTemplate};

        // A 404 is the normal shape of both the pre-listing past and the
        // not-yet-published present, so it folds in as an empty period. A 5xx
        // is not: swallowing it would drop a mid-history day silently, and
        // differently on every run.
        let runtime = tokio::runtime::Runtime::new().expect("runtime");
        runtime.block_on(async {
            let server = MockServer::start().await;
            Mock::given(method("GET"))
                .and(path("/missing"))
                .respond_with(ResponseTemplate::new(404))
                .mount(&server)
                .await;
            Mock::given(method("GET"))
                .and(path("/broken"))
                .respond_with(ResponseTemplate::new(503))
                .mount(&server)
                .await;

            let client = reqwest::Client::new();
            let absent = fetch_archive(&client, &format!("{}/missing", server.uri()))
                .await
                .expect("404 is not an error");
            assert!(absent.is_none());

            let err = fetch_archive(&client, &format!("{}/broken", server.uri()))
                .await
                .expect_err("a 503 must not read as an empty period");
            assert!(
                matches!(err, SourceError::Http { status: 503, .. }),
                "got {err:?}",
            );
        });
    }
}
