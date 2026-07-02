//! Integration test for the Binance `CandleSource` implementation.
//!
//! Spins up a `wiremock` server on a random port, stubs `/api/v3/klines` with
//! a canned two-page response, and verifies the client pages through both
//! pages, decodes the JSON correctly, and stops at the short second page.

use fugazi::sources::{Binance, CandleSource, Interval, Timestamp};
use wiremock::matchers::{method, path, query_param};
use wiremock::{Mock, MockServer, ResponseTemplate};

/// Build a JSON kline row (a heterogeneous 12-element array).
fn kline(open_time: i64, o: &str, h: &str, l: &str, c: &str, v: &str) -> serde_json::Value {
    serde_json::json!([
        open_time,
        o,
        h,
        l,
        c,
        v,
        open_time + 86_399_999_i64,
        "0",
        0_i64,
        "0",
        "0",
        "0"
    ])
}

#[tokio::test]
async fn paginates_and_decodes_klines() {
    let server = MockServer::start().await;

    // Page 1: three klines, the maximum the client asked for on this request.
    // The client's next `startTime` will be `last_open_time + 1`.
    let page1: Vec<serde_json::Value> = vec![
        kline(1_704_067_200_000, "42000.0", "42500.5", "41800.25", "42100.00", "100.0"),
        kline(1_704_153_600_000, "42100.0", "42300.0", "42000.0", "42250.0", "80.0"),
        kline(1_704_240_000_000, "42250.0", "42400.0", "42150.0", "42350.0", "90.0"),
    ];
    // Page 2: two klines, so a short page -> loop exit.
    let page2: Vec<serde_json::Value> = vec![
        kline(1_704_326_400_000, "42350.0", "42500.0", "42300.0", "42450.0", "70.0"),
        kline(1_704_412_800_000, "42450.0", "42600.0", "42400.0", "42550.0", "60.0"),
    ];

    Mock::given(method("GET"))
        .and(path("/api/v3/klines"))
        .and(query_param("symbol", "BTCEUR"))
        .and(query_param("interval", "1d"))
        .and(query_param("startTime", "1704067200000"))
        .respond_with(ResponseTemplate::new(200).set_body_json(&page1))
        .mount(&server)
        .await;

    Mock::given(method("GET"))
        .and(path("/api/v3/klines"))
        .and(query_param("symbol", "BTCEUR"))
        .and(query_param("interval", "1d"))
        .and(query_param("startTime", "1704240000001"))
        .respond_with(ResponseTemplate::new(200).set_body_json(&page2))
        .mount(&server)
        .await;

    let client = Binance::new()
        .with_base_url(server.uri())
        .with_max_per_request(3);

    let bars = client
        .candles(
            "BTCEUR",
            Interval::Day(1),
            Timestamp(1_704_067_200_000),
            Some(Timestamp(1_704_499_200_000)),
        )
        .await
        .expect("fetch succeeds");

    assert_eq!(bars.len(), 5);
    assert_eq!(bars[0].time, Timestamp(1_704_067_200_000));
    assert_eq!(bars[0].candle.open, 42000.0);
    assert_eq!(bars[0].candle.close, 42100.0);
    assert_eq!(bars[4].time, Timestamp(1_704_412_800_000));
    assert_eq!(bars[4].candle.close, 42550.0);

    // Times are strictly ascending.
    for w in bars.windows(2) {
        assert!(w[0].time < w[1].time, "times must be ascending");
    }
}

#[tokio::test]
async fn maps_unknown_symbol_error() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/api/v3/klines"))
        .respond_with(
            ResponseTemplate::new(400).set_body_json(serde_json::json!({
                "code": -1121,
                "msg": "Invalid symbol."
            })),
        )
        .mount(&server)
        .await;

    let client = Binance::new().with_base_url(server.uri());
    let err = client
        .candles(
            "NOTASYMBOL",
            Interval::Day(1),
            Timestamp(1_704_067_200_000),
            Some(Timestamp(1_704_153_600_000)),
        )
        .await
        .expect_err("expected UnknownSymbol");
    match err {
        fugazi::sources::SourceError::UnknownSymbol(msg) => assert_eq!(msg, "Invalid symbol."),
        other => panic!("expected UnknownSymbol, got {other:?}"),
    }
}

#[tokio::test]
async fn maps_rate_limit_error() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/api/v3/klines"))
        .respond_with(
            ResponseTemplate::new(429)
                .append_header("Retry-After", "12")
                .set_body_string("Too Many Requests"),
        )
        .mount(&server)
        .await;

    let client = Binance::new().with_base_url(server.uri());
    let err = client
        .candles(
            "BTCEUR",
            Interval::Day(1),
            Timestamp(1_704_067_200_000),
            Some(Timestamp(1_704_153_600_000)),
        )
        .await
        .expect_err("expected RateLimited");
    match err {
        fugazi::sources::SourceError::RateLimited { retry_after_ms } => {
            assert_eq!(retry_after_ms, 12_000)
        }
        other => panic!("expected RateLimited, got {other:?}"),
    }
}
