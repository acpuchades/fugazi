#![cfg(feature = "sources")]
//! Integration test for the live `BinanceFutures` `SeriesSource`.
//!
//! Spins up a `wiremock` server on a random port and stubs all eight `fapi`
//! endpoints the provider reads. What is worth pinning here — and cannot be
//! pinned by a unit test on the decoders — is that the eight *independent*
//! feeds collapse into one series: one atom per bar carrying the contract's
//! candle, its funding **summed**, and the positioning statistics as levels.
//!
//! The other half is what the provider asks for. The `/futures/data/*`
//! endpoints serve a rolling 30 days, so a fetch reaching further back must
//! start those feeds at the horizon rather than paging through years of
//! guaranteed-empty responses, and must ask them for a `period` from their own
//! coarser vocabulary rather than forwarding the bar cadence.

use std::collections::HashMap;

use fugazi::sources::{BinanceFutures, Interval, SeriesSource, SourceError, Timestamp};
use wiremock::matchers::{method, path, query_param};
use wiremock::{Mock, MockServer, ResponseTemplate};

const DAY: i64 = 86_400_000;
const HOUR: i64 = 3_600_000;
/// 2024-01-01 00:00 UTC.
const DAY1: i64 = 1_704_067_200_000;
const DAY2: i64 = DAY1 + DAY;

/// Every path the provider fetches, in fold order.
const PATHS: &[&str] = &[
    "/fapi/v1/klines",
    "/fapi/v1/premiumIndexKlines",
    "/fapi/v1/fundingRate",
    "/futures/data/openInterestHist",
    "/futures/data/globalLongShortAccountRatio",
    "/futures/data/topLongShortAccountRatio",
    "/futures/data/topLongShortPositionRatio",
    "/futures/data/takerlongshortRatio",
];

/// A 12-element kline row, as `fapi` spells one.
fn kline(open_time: i64, close: &str, volume: &str) -> serde_json::Value {
    serde_json::json!([
        open_time,
        "100.0",
        "110.0",
        "90.0",
        close,
        volume,
        open_time + DAY - 1,
        "334000.00", // quote_volume
        42,          // n_trades
        "6.0",       // taker_buy_base_volume
        "162500.00", // taker_buy_quote_volume
        "0"
    ])
}

/// Serve `body` at `path`, and an empty array at every other feed — a feed
/// this test says nothing about must still answer, or the fetch fails on it.
async fn serve(bodies: &[(&str, serde_json::Value)]) -> MockServer {
    let server = MockServer::start().await;
    let stubbed: HashMap<&str, &serde_json::Value> = bodies.iter().map(|(p, b)| (*p, b)).collect();
    for p in PATHS {
        let body = stubbed
            .get(p)
            .map(|b| (*b).clone())
            .unwrap_or_else(|| serde_json::json!([]));
        Mock::given(method("GET"))
            .and(path(*p))
            .respond_with(ResponseTemplate::new(200).set_body_json(body))
            .mount(&server)
            .await;
    }
    server
}

/// The query parameters of the (single) request the provider made to `path`.
async fn query_to(server: &MockServer, path: &str) -> HashMap<String, String> {
    let received = server
        .received_requests()
        .await
        .expect("the mock server records requests");
    let mut matching = received.iter().filter(|r| r.url.path() == path);
    let req = matching
        .next()
        .unwrap_or_else(|| panic!("no request to {path}"));
    assert!(
        matching.next().is_none(),
        "expected exactly one request to {path}"
    );
    req.url
        .query_pairs()
        .map(|(k, v)| (k.into_owned(), v.into_owned()))
        .collect()
}

fn real(atom: &fugazi::types::Atom, key: &str) -> Option<f64> {
    match atom.overlays.as_ref()?.get_by_key(key)? {
        fugazi::types::OverlayValue::Real(v) => Some(*v),
        _ => None,
    }
}

#[tokio::test]
async fn eight_feeds_collapse_into_one_atom_per_bar() {
    let server = serve(&[
        (
            "/fapi/v1/klines",
            serde_json::json!([kline(DAY1, "105.0", "10.0"), kline(DAY2, "107.0", "12.0")]),
        ),
        (
            "/fapi/v1/premiumIndexKlines",
            serde_json::json!([
                // Only the close is a column, and the bar must stay the
                // contract's — a premium index of 0.0003 is not a price.
                [DAY1, "0.0001", "0.0004", "-0.0002", "0.0003", "0"],
                [DAY2, "0.0002", "0.0005", "-0.0001", "0.0006", "0"],
            ]),
        ),
        (
            "/fapi/v1/fundingRate",
            serde_json::json!([
                { "symbol": "BTCUSDT", "fundingTime": DAY1, "fundingRate": "0.0001" },
                { "symbol": "BTCUSDT", "fundingTime": DAY1 + 8 * HOUR, "fundingRate": "0.0002" },
                { "symbol": "BTCUSDT", "fundingTime": DAY1 + 16 * HOUR, "fundingRate": "0.0004" },
                { "symbol": "BTCUSDT", "fundingTime": DAY2, "fundingRate": "-0.0003" },
            ]),
        ),
        (
            "/futures/data/openInterestHist",
            // `/futures/data/*` labels a window by its **close**, so the row
            // that describes day 1 is stamped at day 2's open. Verified
            // against the live endpoint and the archive, which stamps the same
            // snapshot by the window's open.
            serde_json::json!([
                { "symbol": "BTCUSDT", "sumOpenInterest": "150.0",
                  "sumOpenInterestValue": "1500.0", "timestamp": DAY2 },
                { "symbol": "BTCUSDT", "sumOpenInterest": "160.0",
                  "sumOpenInterestValue": "1600.0", "timestamp": DAY2 + DAY },
            ]),
        ),
        (
            "/futures/data/globalLongShortAccountRatio",
            serde_json::json!([
                { "symbol": "BTCUSDT", "longShortRatio": "1.8", "timestamp": DAY2 },
            ]),
        ),
        (
            "/futures/data/topLongShortAccountRatio",
            serde_json::json!([
                { "symbol": "BTCUSDT", "longShortRatio": "2.1", "timestamp": DAY2 },
            ]),
        ),
        (
            "/futures/data/topLongShortPositionRatio",
            serde_json::json!([
                { "symbol": "BTCUSDT", "longShortRatio": "2.5", "timestamp": DAY2 },
            ]),
        ),
        (
            "/futures/data/takerlongshortRatio",
            // The one `/futures/data` feed that accrues rather than snapshots:
            // it is the buy/sell volume ratio over `[t, t+period)`, stamped at
            // the open like a kline, so day 1's row is stamped at day 1.
            serde_json::json!([
                { "symbol": "BTCUSDT", "buySellRatio": "1.5586", "timestamp": DAY1 },
            ]),
        ),
    ])
    .await;

    let atoms = BinanceFutures::new()
        .with_base_url(server.uri())
        .atoms(
            "BTCUSDT",
            Interval::Day(1),
            Timestamp(DAY1),
            Some(Timestamp(DAY2 + DAY)),
        )
        .await
        .expect("fetch succeeds");

    assert_eq!(atoms.len(), 2);
    assert_eq!(atoms[0].time, Some(Timestamp(DAY1)));
    let bar = atoms[0]
        .candle
        .expect("the contract's klines carry the bar");
    assert_eq!(bar.close, 105.0);
    assert_eq!(bar.volume, 10.0);

    // The kline's own extras.
    assert_eq!(real(&atoms[0], "quote_volume"), Some(334_000.0));
    assert_eq!(real(&atoms[0], "n_trades"), Some(42.0));

    // Funding accrues: three settlements inside day 1 are that day's carry.
    let carry = real(&atoms[0], "funding_rate").expect("funding sampled");
    assert!(
        (carry - 0.0007).abs() < 1e-12,
        "day 1 carry should be the sum of its three settlements, got {carry}"
    );
    let day2 = real(&atoms[1], "funding_rate").expect("funding sampled");
    assert!((day2 + 0.0003).abs() < 1e-12, "got {day2}");

    // Each level lands on the bar it measured, one behind its own stamp.
    assert_eq!(real(&atoms[0], "open_interest"), Some(150.0));
    assert_eq!(real(&atoms[0], "open_interest_value"), Some(1500.0));
    assert_eq!(real(&atoms[0], "long_short_ratio"), Some(1.8));
    assert_eq!(real(&atoms[0], "top_trader_account_ratio"), Some(2.1));
    assert_eq!(real(&atoms[0], "top_trader_position_ratio"), Some(2.5));
    assert_eq!(real(&atoms[0], "taker_long_short_ratio"), Some(1.5586));
    assert_eq!(real(&atoms[0], "premium_index"), Some(0.0003));

    // Day 2's open interest came from the row stamped at day 3's open — the
    // one past `until`, which the request window reaches for precisely so the
    // last bar is not left unsampled.
    assert_eq!(real(&atoms[1], "open_interest"), Some(160.0));
    // Its ratios, though, had no sample at all — and that has to read as an
    // absent sample, not as a zero.
    assert_eq!(real(&atoms[1], "long_short_ratio"), None);
    assert_eq!(atoms[1].candle.expect("day 2 has a bar").close, 107.0);
}

/// The regression this pins: `/futures/data/*` stamps a row by its window's
/// **close**, so folding it by that stamp puts every level on the bar *after*
/// the one it measured — stale against its own bar, and one bar away from
/// where `binance-vision-futures` puts the identical number while the two
/// claim one schema.
#[tokio::test]
async fn a_statistic_lands_on_the_bar_it_measured_not_the_next_one() {
    let server = serve(&[
        (
            "/fapi/v1/klines",
            serde_json::json!([
                kline(DAY1, "105.0", "10.0"),
                kline(DAY1 + HOUR, "106.0", "11.0"),
            ]),
        ),
        (
            "/futures/data/openInterestHist",
            serde_json::json!([
                // The hourly row stamped 01:00 carries the snapshot taken
                // inside [00:00, 01:00) — it belongs to the 00:00 bar.
                { "symbol": "BTCUSDT", "sumOpenInterest": "111.0",
                  "sumOpenInterestValue": "1110.0", "timestamp": DAY1 + HOUR },
                { "symbol": "BTCUSDT", "sumOpenInterest": "222.0",
                  "sumOpenInterestValue": "2220.0", "timestamp": DAY1 + 2 * HOUR },
            ]),
        ),
    ])
    .await;

    let atoms = BinanceFutures::new()
        .with_base_url(server.uri())
        .atoms(
            "BTCUSDT",
            Interval::Hour(1),
            Timestamp(DAY1),
            Some(Timestamp(DAY1 + 2 * HOUR)),
        )
        .await
        .expect("fetch succeeds");

    assert_eq!(atoms.len(), 2);
    assert_eq!(real(&atoms[0], "open_interest"), Some(111.0));
    assert_eq!(real(&atoms[1], "open_interest"), Some(222.0));

    // Reaching the second bar's value means asking one period past `until`.
    let q = query_to(&server, "/futures/data/openInterestHist").await;
    assert_eq!(
        q.get("endTime"),
        Some(&(DAY1 + 3 * HOUR - 1).to_string()),
        "the row filling the last bar is stamped one period beyond it"
    );
}

#[tokio::test]
async fn the_positioning_feeds_start_at_the_thirty_day_horizon() {
    let since = DAY2 - 400 * DAY; // well past what `/futures/data/*` serves
    let until = DAY2;
    let server = serve(&[]).await;

    let atoms = BinanceFutures::new()
        .with_base_url(server.uri())
        .atoms(
            "BTCUSDT",
            Interval::Day(1),
            Timestamp(since),
            Some(Timestamp(until)),
        )
        .await
        .expect("empty feeds are an empty series, not an error");
    assert!(atoms.is_empty());

    // The klines are asked for the whole range: their history is not capped.
    let klines = query_to(&server, "/fapi/v1/klines").await;
    assert_eq!(klines.get("startTime"), Some(&since.to_string()));
    assert_eq!(klines.get("interval"), Some(&"1d".to_string()));
    assert_eq!(klines.get("endTime"), Some(&(until - 1).to_string()));

    // Funding history is not capped either.
    let funding = query_to(&server, "/fapi/v1/fundingRate").await;
    assert_eq!(funding.get("startTime"), Some(&since.to_string()));

    // The five statistics feeds are, so they start where the data does — and
    // take a `period` from their own vocabulary.
    for p in &PATHS[3..] {
        let q = query_to(&server, p).await;
        assert_eq!(
            q.get("startTime"),
            Some(&(until - 30 * DAY).to_string()),
            "{p} should start at the horizon, not at `since`"
        );
        assert_eq!(q.get("period"), Some(&"1d".to_string()), "{p}");
        // A close-labelled snapshot reaches one period past `until`, where the
        // row that fills the last bar is; the open-labelled taker ratio does
        // not, and asking for it would pull in a bar past the range.
        let close_labelled = *p != "/futures/data/takerlongshortRatio";
        let end = if close_labelled { until + DAY } else { until };
        assert_eq!(q.get("endTime"), Some(&(end - 1).to_string()), "{p}");
    }
}

#[tokio::test]
async fn a_cadence_the_statistics_endpoints_do_not_speak_falls_to_a_finer_one() {
    let server = serve(&[]).await;
    BinanceFutures::new()
        .with_base_url(server.uri())
        .atoms(
            "BTCUSDT",
            Interval::Hour(8),
            Timestamp(DAY1),
            Some(Timestamp(DAY2)),
        )
        .await
        .expect("fetch succeeds");

    // `8h` is a kline token but not a `/futures/data` period. Sampling finer
    // than the bar is always correct for a level; coarser would leave bars
    // unsampled.
    assert_eq!(
        query_to(&server, "/fapi/v1/klines").await.get("interval"),
        Some(&"8h".to_string())
    );
    assert_eq!(
        query_to(&server, "/futures/data/openInterestHist")
            .await
            .get("period"),
        Some(&"6h".to_string())
    );
}

#[tokio::test]
async fn paginates_the_klines_until_a_short_page() {
    let server = MockServer::start().await;
    // Page 1 fills the client's page size, so it asks again from the bar after
    // the last one it saw; page 2 is short, which ends the loop.
    Mock::given(method("GET"))
        .and(path("/fapi/v1/klines"))
        .and(query_param("startTime", DAY1.to_string()))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!([
            kline(DAY1, "105.0", "10.0"),
            kline(DAY2, "107.0", "12.0"),
        ])))
        .mount(&server)
        .await;
    Mock::given(method("GET"))
        .and(path("/fapi/v1/klines"))
        .and(query_param("startTime", (DAY2 + 1).to_string()))
        .respond_with(
            ResponseTemplate::new(200).set_body_json(serde_json::json!([kline(
                DAY2 + DAY,
                "109.0",
                "14.0"
            )])),
        )
        .mount(&server)
        .await;

    let atoms = BinanceFutures::new()
        .with_base_url(server.uri())
        .with_max_per_request(2)
        .with_min_delay(std::time::Duration::ZERO)
        .bars_only()
        .atoms(
            "BTCUSDT",
            Interval::Day(1),
            Timestamp(DAY1),
            Some(Timestamp(DAY2 + 2 * DAY)),
        )
        .await
        .expect("fetch succeeds");

    let closes: Vec<f64> = atoms.iter().map(|a| a.candle.expect("bar").close).collect();
    assert_eq!(closes, vec![105.0, 107.0, 109.0]);
}

#[tokio::test]
async fn bars_only_asks_for_nothing_but_the_klines() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/fapi/v1/klines"))
        .respond_with(
            ResponseTemplate::new(200)
                .set_body_json(serde_json::json!([kline(DAY1, "105.0", "1")])),
        )
        .mount(&server)
        .await;
    // Every other feed is left unmounted: a request to one would 404, and the
    // fetch would fail rather than quietly returning bars.

    let atoms = BinanceFutures::new()
        .with_base_url(server.uri())
        .bars_only()
        .atoms(
            "BTCUSDT",
            Interval::Day(1),
            Timestamp(DAY1),
            Some(Timestamp(DAY2)),
        )
        .await
        .expect("fetch succeeds");
    assert_eq!(atoms.len(), 1);
    // The columns are still declared — an overlay naming one builds and reads
    // absent, rather than failing to resolve.
    assert_eq!(real(&atoms[0], "funding_rate"), None);
    assert!(
        atoms[0]
            .overlays
            .as_ref()
            .expect("schema bound")
            .schema()
            .contains("funding_rate")
    );
}

#[tokio::test]
async fn any_feed_failing_fails_the_fetch() {
    // A side channel that 429s must not be swallowed: a series missing its
    // funding column reads exactly like a contract that never charged any.
    let server = MockServer::start().await;
    for p in PATHS {
        let response = if *p == "/futures/data/openInterestHist" {
            ResponseTemplate::new(429)
                .append_header("Retry-After", "12")
                .set_body_string("Too Many Requests")
        } else {
            ResponseTemplate::new(200).set_body_json(serde_json::json!([]))
        };
        Mock::given(method("GET"))
            .and(path(*p))
            .respond_with(response)
            .mount(&server)
            .await;
    }

    let err = BinanceFutures::new()
        .with_base_url(server.uri())
        .atoms(
            "BTCUSDT",
            Interval::Day(1),
            Timestamp(DAY1),
            Some(Timestamp(DAY2)),
        )
        .await
        .expect_err("expected RateLimited");
    match err {
        SourceError::RateLimited { retry_after_ms } => assert_eq!(retry_after_ms, 12_000),
        other => panic!("expected RateLimited, got {other:?}"),
    }
}

#[tokio::test]
async fn maps_unknown_symbol_error() {
    let server = MockServer::start().await;
    for p in PATHS {
        Mock::given(method("GET"))
            .and(path(*p))
            .respond_with(ResponseTemplate::new(400).set_body_json(serde_json::json!({
                "code": -1121,
                "msg": "Invalid symbol."
            })))
            .mount(&server)
            .await;
    }

    let err = BinanceFutures::new()
        .with_base_url(server.uri())
        .atoms(
            "NOTASYMBOL",
            Interval::Day(1),
            Timestamp(DAY1),
            Some(Timestamp(DAY2)),
        )
        .await
        .expect_err("expected UnknownSymbol");
    match err {
        SourceError::UnknownSymbol(msg) => assert_eq!(msg, "Invalid symbol."),
        other => panic!("expected UnknownSymbol, got {other:?}"),
    }
}

#[tokio::test]
async fn tickers_keeps_trading_perpetuals_only() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/fapi/v1/exchangeInfo"))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
            "timezone": "UTC",
            "symbols": [
                { "symbol": "ETHUSDT",        "status": "TRADING", "contractType": "PERPETUAL" },
                { "symbol": "BTCUSDT",        "status": "TRADING", "contractType": "PERPETUAL" },
                // A dated quarterly is a different instrument, not a spelling.
                { "symbol": "BTCUSDT_250926", "status": "TRADING", "contractType": "CURRENT_QUARTER" },
                { "symbol": "DELISTEDUSDT",   "status": "BREAK",   "contractType": "PERPETUAL" },
                { "symbol": "ADAUSDT",        "status": "TRADING", "contractType": "PERPETUAL" },
            ]
        })))
        .mount(&server)
        .await;

    let client = BinanceFutures::new().with_base_url(server.uri());
    let tickers = SeriesSource::tickers(&client)
        .await
        .expect("fetch succeeds");
    assert_eq!(tickers, vec!["ADAUSDT", "BTCUSDT", "ETHUSDT"]);
}
