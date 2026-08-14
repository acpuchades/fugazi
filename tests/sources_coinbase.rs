#![cfg(feature = "sources")]
//! Integration test for the Coinbase Advanced Trade `SeriesSource`.
//!
//! Spins up a `wiremock` server, stubs the public
//! `/api/v3/brokerage/market/products/{id}/candles` endpoint, and verifies the
//! client pages *forward* through fixed-width windows (Coinbase requires an
//! explicit `[start, end]` and caps a page at 300 bars), decodes the
//! string-typed JSON, scales the second-granularity `start` up to millis, sorts
//! ascending, de-duplicates the bar shared by adjacent windows, and honours the
//! half-open `[since, until)` contract.

use fugazi::sources::{Coinbase, Interval, SeriesSource, Timestamp};
use wiremock::matchers::{method, path, query_param};
use wiremock::{Mock, MockServer, ResponseTemplate};

const CANDLES_PATH: &str = "/api/v3/brokerage/market/products/BTC-USD/candles";

// Day-aligned bar opens, in Unix seconds (2024-01-01 00:00 UTC onward).
const DAY0: i64 = 1_704_067_200;
const DAY: i64 = 86_400;

/// Build a Coinbase candle object: `{start, low, high, open, close, volume}`,
/// every field a string, `start` in Unix seconds.
fn candle(start_s: i64, o: &str, h: &str, l: &str, c: &str, v: &str) -> serde_json::Value {
    serde_json::json!({
        "start": start_s.to_string(),
        "low": l,
        "high": h,
        "open": o,
        "close": c,
        "volume": v,
    })
}

fn envelope(rows: &[serde_json::Value]) -> serde_json::Value {
    serde_json::json!({ "candles": rows })
}

#[tokio::test]
async fn fetches_one_window_and_decodes_candles() {
    let server = MockServer::start().await;

    // The default page size (300) covers the whole 5-day window in one request.
    // Coinbase returns newest-first; a bar at `until` (day 5) must be excluded.
    let rows = vec![
        candle(DAY0 + 5 * DAY, "5", "5", "5", "5", "5"), // == until, excluded
        candle(DAY0 + 4 * DAY, "42450.0", "42600.0", "42400.0", "42550.0", "60.0"),
        candle(DAY0 + 3 * DAY, "42350.0", "42500.0", "42300.0", "42450.0", "70.0"),
        candle(DAY0 + 2 * DAY, "42250.0", "42400.0", "42150.0", "42350.0", "90.0"),
        candle(DAY0 + DAY, "42100.0", "42300.0", "42000.0", "42250.0", "80.0"),
        candle(DAY0, "42000.0", "42500.5", "41800.25", "42100.00", "100.0"),
    ];

    Mock::given(method("GET"))
        .and(path(CANDLES_PATH))
        .and(query_param("granularity", "ONE_DAY"))
        .and(query_param("start", DAY0.to_string()))
        .and(query_param("end", (DAY0 + 5 * DAY).to_string()))
        .respond_with(ResponseTemplate::new(200).set_body_json(envelope(&rows)))
        .mount(&server)
        .await;

    let client = Coinbase::new().with_base_url(server.uri());
    let bars = client
        .atoms(
            "BTC-USD",
            Interval::Day(1),
            Timestamp(DAY0 * 1000),
            Some(Timestamp((DAY0 + 5 * DAY) * 1000)),
        )
        .await
        .expect("fetch succeeds");

    // Five bars: day 0..=day 4; the day-5 bar at `until` is excluded (half-open).
    assert_eq!(bars.len(), 5);
    assert_eq!(bars[0].time, Some(Timestamp(DAY0 * 1000)));
    assert_eq!(bars[0].candle.unwrap().open, 42000.0);
    assert_eq!(bars[0].candle.unwrap().low, 41800.25);
    assert_eq!(bars[4].time, Some(Timestamp((DAY0 + 4 * DAY) * 1000)));
    assert_eq!(bars[4].candle.unwrap().close, 42550.0);
    for w in bars.windows(2) {
        assert!(w[0].time < w[1].time, "times must be ascending");
    }
    // Coinbase candles carry no overlay side channel.
    assert!(bars[0].overlays.is_none());
}

#[tokio::test]
async fn pages_forward_and_dedups_boundary_bars() {
    let server = MockServer::start().await;

    // A 2-bar page size makes the span two days, so the 5-day window needs three
    // forward windows whose boundary bars (day 2, day 4) overlap.
    let win1 = vec![
        candle(DAY0 + 2 * DAY, "22", "22", "22", "22", "1"),
        candle(DAY0 + DAY, "11", "11", "11", "11", "1"),
        candle(DAY0, "0", "0", "0", "0", "1"),
    ];
    let win2 = vec![
        candle(DAY0 + 4 * DAY, "44", "44", "44", "44", "1"),
        candle(DAY0 + 3 * DAY, "33", "33", "33", "33", "1"),
        candle(DAY0 + 2 * DAY, "22", "22", "22", "22", "1"),
    ];
    let win3 = vec![candle(DAY0 + 4 * DAY, "44", "44", "44", "44", "1")];

    for (start, body) in [
        (DAY0, &win1),
        (DAY0 + 2 * DAY, &win2),
        (DAY0 + 4 * DAY, &win3),
    ] {
        Mock::given(method("GET"))
            .and(path(CANDLES_PATH))
            .and(query_param("start", start.to_string()))
            .respond_with(ResponseTemplate::new(200).set_body_json(envelope(body)))
            .mount(&server)
            .await;
    }

    let client = Coinbase::new().with_base_url(server.uri()).with_max_per_request(2);
    let bars = client
        .atoms(
            "BTC-USD",
            Interval::Day(1),
            Timestamp(DAY0 * 1000),
            Some(Timestamp((DAY0 + 5 * DAY) * 1000)),
        )
        .await
        .expect("fetch succeeds");

    // Five unique bars, ascending, despite day 2 and day 4 appearing twice.
    assert_eq!(bars.len(), 5);
    let opens: Vec<f64> = bars.iter().map(|b| b.candle.unwrap().open).collect();
    assert_eq!(opens, vec![0.0, 11.0, 22.0, 33.0, 44.0]);
}

#[tokio::test]
async fn maps_unknown_symbol_error() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/api/v3/brokerage/market/products/NO-SUCH/candles"))
        .respond_with(ResponseTemplate::new(404).set_body_json(serde_json::json!({
            "error": "NOT_FOUND",
            "error_details": "product not found",
            "message": "product NO-SUCH not found"
        })))
        .mount(&server)
        .await;

    let client = Coinbase::new().with_base_url(server.uri());
    let err = client
        .atoms(
            "NO-SUCH",
            Interval::Day(1),
            Timestamp(DAY0 * 1000),
            Some(Timestamp((DAY0 + DAY) * 1000)),
        )
        .await
        .expect_err("expected UnknownSymbol");
    match err {
        fugazi::sources::SourceError::UnknownSymbol(msg) => {
            assert_eq!(msg, "product NO-SUCH not found")
        }
        other => panic!("expected UnknownSymbol, got {other:?}"),
    }
}

#[tokio::test]
async fn maps_rate_limit_error() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path(CANDLES_PATH))
        .respond_with(
            ResponseTemplate::new(429)
                .insert_header("Retry-After", "3")
                .set_body_string("rate limited"),
        )
        .mount(&server)
        .await;

    let client = Coinbase::new().with_base_url(server.uri());
    let err = client
        .atoms(
            "BTC-USD",
            Interval::Day(1),
            Timestamp(DAY0 * 1000),
            Some(Timestamp((DAY0 + DAY) * 1000)),
        )
        .await
        .expect_err("expected RateLimited");
    match err {
        fugazi::sources::SourceError::RateLimited { retry_after_ms } => {
            assert_eq!(retry_after_ms, 3000)
        }
        other => panic!("expected RateLimited, got {other:?}"),
    }
}

#[tokio::test]
async fn tickers_filters_to_online_spot_and_sorts() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/api/v3/brokerage/market/products"))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
            "products": [
                { "product_id": "ETH-USD", "status": "online", "trading_disabled": false, "product_type": "SPOT" },
                { "product_id": "BTC-USD", "status": "online", "trading_disabled": false, "product_type": "SPOT" },
                { "product_id": "OLD-USD", "status": "delisted", "trading_disabled": true, "product_type": "SPOT" },
                { "product_id": "BTC-PERP", "status": "online", "trading_disabled": false, "product_type": "FUTURE" },
                { "product_id": "ADA-USD", "status": "online", "trading_disabled": false, "product_type": "SPOT" },
            ]
        })))
        .mount(&server)
        .await;

    let client = Coinbase::new().with_base_url(server.uri());
    let tickers = <Coinbase as SeriesSource>::tickers(&client)
        .await
        .expect("fetch succeeds");
    assert_eq!(tickers, vec!["ADA-USD", "BTC-USD", "ETH-USD"]);
}
