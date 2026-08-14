#![cfg(feature = "sources")]
//! Integration test for the OKX `SeriesSource` implementation.
//!
//! Spins up a `wiremock` server on a random port, stubs
//! `/api/v5/market/history-candles` with a canned two-page response, and
//! verifies the client pages *backward* through both pages (OKX serves candles
//! newest-first), decodes the string-typed JSON correctly, sorts the result
//! ascending, and stops at the short second page.

use fugazi::sources::{Interval, Okx, SeriesSource, Timestamp};
use wiremock::matchers::{method, path, query_param};
use wiremock::{Mock, MockServer, ResponseTemplate};

/// Build a JSON candle row (a nine-element array of strings, OKX's shape):
/// `[ts, open, high, low, close, vol, volCcy, volCcyQuote, confirm]`.
fn candle(ts: i64, o: &str, h: &str, l: &str, c: &str, v: &str) -> serde_json::Value {
    serde_json::json!([
        ts.to_string(),
        o,
        h,
        l,
        c,
        v,
        "0",   // volCcy
        "0",   // volCcyQuote
        "1"    // confirm
    ])
}

fn envelope(rows: &[serde_json::Value]) -> serde_json::Value {
    serde_json::json!({ "code": "0", "msg": "", "data": rows })
}

#[tokio::test]
async fn paginates_backward_and_decodes_candles() {
    let server = MockServer::start().await;

    // OKX returns newest-first. The client pages backward from `until` via the
    // `after` cursor. Page 1 (after = until) yields the three newest bars in
    // the window, descending; its oldest timestamp becomes page 2's cursor.
    let page1 = vec![
        candle(1_704_412_800_000, "42450.0", "42600.0", "42400.0", "42550.0", "60.0"),
        candle(1_704_326_400_000, "42350.0", "42500.0", "42300.0", "42450.0", "70.0"),
        candle(1_704_240_000_000, "42250.0", "42400.0", "42150.0", "42350.0", "90.0"),
    ];
    // Page 2: two candles, so a short page -> loop exit.
    let page2 = vec![
        candle(1_704_153_600_000, "42100.0", "42300.0", "42000.0", "42250.0", "80.0"),
        candle(1_704_067_200_000, "42000.0", "42500.5", "41800.25", "42100.00", "100.0"),
    ];

    Mock::given(method("GET"))
        .and(path("/api/v5/market/history-candles"))
        .and(query_param("instId", "BTC-USDT"))
        .and(query_param("bar", "1Dutc"))
        .and(query_param("after", "1704499200000"))
        .respond_with(ResponseTemplate::new(200).set_body_json(envelope(&page1)))
        .mount(&server)
        .await;

    Mock::given(method("GET"))
        .and(path("/api/v5/market/history-candles"))
        .and(query_param("instId", "BTC-USDT"))
        .and(query_param("bar", "1Dutc"))
        .and(query_param("after", "1704240000000"))
        .respond_with(ResponseTemplate::new(200).set_body_json(envelope(&page2)))
        .mount(&server)
        .await;

    let client = Okx::new().with_base_url(server.uri()).with_max_per_request(3);

    let bars = client
        .atoms(
            "BTC-USDT",
            Interval::Day(1),
            Timestamp(1_704_067_200_000),
            Some(Timestamp(1_704_499_200_000)),
        )
        .await
        .expect("fetch succeeds");

    assert_eq!(bars.len(), 5);
    // Result is sorted ascending regardless of OKX's newest-first paging.
    assert_eq!(bars[0].time, Some(Timestamp(1_704_067_200_000)));
    assert_eq!(bars[0].candle.unwrap().open, 42000.0);
    assert_eq!(bars[0].candle.unwrap().close, 42100.0);
    assert_eq!(bars[4].time, Some(Timestamp(1_704_412_800_000)));
    assert_eq!(bars[4].candle.unwrap().close, 42550.0);

    for w in bars.windows(2) {
        assert!(w[0].time < w[1].time, "times must be ascending");
    }

    let ov = bars[0].overlays.as_ref().expect("OKX atoms carry overlays");
    assert!(ov.get_by_key("vol_ccy").is_some());
    assert!(ov.get_by_key("quote_volume").is_some());
}

#[tokio::test]
async fn maps_unknown_symbol_error() {
    let server = MockServer::start().await;
    // OKX reports application errors with HTTP 200 and a non-"0" body `code`.
    Mock::given(method("GET"))
        .and(path("/api/v5/market/history-candles"))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
            "code": "51001",
            "msg": "Instrument ID does not exist",
            "data": []
        })))
        .mount(&server)
        .await;

    let client = Okx::new().with_base_url(server.uri());
    let err = client
        .atoms(
            "NOT-ASYMBOL",
            Interval::Day(1),
            Timestamp(1_704_067_200_000),
            Some(Timestamp(1_704_153_600_000)),
        )
        .await
        .expect_err("expected UnknownSymbol");
    match err {
        fugazi::sources::SourceError::UnknownSymbol(msg) => {
            assert_eq!(msg, "Instrument ID does not exist")
        }
        other => panic!("expected UnknownSymbol, got {other:?}"),
    }
}

#[tokio::test]
async fn tickers_filters_to_live_and_sorts() {
    let server = MockServer::start().await;

    Mock::given(method("GET"))
        .and(path("/api/v5/public/instruments"))
        .and(query_param("instType", "SPOT"))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
            "code": "0",
            "msg": "",
            "data": [
                { "instId": "ETH-USDT",   "state": "live" },
                { "instId": "BTC-USDT",   "state": "live" },
                { "instId": "LEGACY-BTC", "state": "suspend" },
                { "instId": "ADA-USDT",   "state": "live" },
            ]
        })))
        .mount(&server)
        .await;

    let client = Okx::new().with_base_url(server.uri());
    let tickers = <Okx as SeriesSource>::tickers(&client)
        .await
        .expect("fetch succeeds");
    assert_eq!(tickers, vec!["ADA-USDT", "BTC-USDT", "ETH-USDT"]);
}

#[tokio::test]
async fn maps_rate_limit_error() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/api/v5/market/history-candles"))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
            "code": "50011",
            "msg": "Rate limit reached",
            "data": []
        })))
        .mount(&server)
        .await;

    let client = Okx::new().with_base_url(server.uri());
    let err = client
        .atoms(
            "BTC-USDT",
            Interval::Day(1),
            Timestamp(1_704_067_200_000),
            Some(Timestamp(1_704_153_600_000)),
        )
        .await
        .expect_err("expected RateLimited");
    match err {
        fugazi::sources::SourceError::RateLimited { .. } => {}
        other => panic!("expected RateLimited, got {other:?}"),
    }
}
