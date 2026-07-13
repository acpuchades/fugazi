#![cfg(feature = "sources")]
//! Integration test for the CoinMarketCap `OverlaySource` implementation.
//!
//! Spins up a `wiremock` server on a random port, stubs
//! `/v2/cryptocurrency/quotes/historical` with a canned quote series, and
//! verifies the client sends the paid-tier auth header, projects the
//! `convert`-currency quote into the five overlay columns, floors timestamps to
//! bar-open boundaries, and maps `402` / `429` into the right `SourceError`
//! variants.

use fugazi::sources::{CoinMarketCap, Interval, OverlaySource, Timestamp};
use wiremock::matchers::{header, method, path, query_param};
use wiremock::{Mock, MockServer, ResponseTemplate};

const HISTORICAL: &str = "/v2/cryptocurrency/quotes/historical";

#[tokio::test]
async fn decodes_historical_quotes() {
    let server = MockServer::start().await;

    let body = serde_json::json!({
        "status": {"error_code": 0, "error_message": serde_json::Value::Null},
        "data": {
            "1": {
                "id": 1,
                "name": "Bitcoin",
                "symbol": "BTC",
                "quotes": [
                    {
                        "timestamp": "2024-01-01T00:00:00.000Z",
                        "quote": {"USD": {
                            "price": 42000.0,
                            "volume_24h": 20e9,
                            "market_cap": 840e9,
                            "circulating_supply": 19_600_000.0,
                            "total_supply": 21_000_000.0
                        }}
                    },
                    {
                        // Later-in-day reading of the same bar: must not win the
                        // 2024-01-01 bucket over the 00:00 sample above.
                        "timestamp": "2024-01-01T12:00:00.000Z",
                        "quote": {"USD": {"price": 99999.0}}
                    },
                    {
                        "timestamp": "2024-01-02T00:00:00.000Z",
                        "quote": {"USD": {
                            "price": 43000.0,
                            "volume_24h": 21e9,
                            "market_cap": 843e9
                        }}
                    }
                ]
            }
        }
    });

    Mock::given(method("GET"))
        .and(path(HISTORICAL))
        .and(header("X-CMC_PRO_API_KEY", "test-key"))
        .and(query_param("id", "1"))
        .and(query_param("interval", "daily"))
        .and(query_param("convert", "USD"))
        .respond_with(ResponseTemplate::new(200).set_body_json(&body))
        .mount(&server)
        .await;

    let client = CoinMarketCap::new()
        .with_base_url(server.uri())
        .with_api_key("test-key");

    let rows = client
        .overlays(
            "1",
            Interval::Day(1),
            Timestamp(1_704_067_200_000),          // 2024-01-01
            Some(Timestamp(1_704_240_000_000)),    // 2024-01-03 (exclusive)
        )
        .await
        .expect("fetch succeeds");

    assert_eq!(rows.len(), 2, "two daily buckets");

    // First bucket keeps the 00:00 sample, not the 12:00 one.
    assert_eq!(rows[0].time, Timestamp(1_704_067_200_000));
    let ov = &rows[0].overlays;
    assert_eq!(ov.get_by_key("price"), Some(&fugazi::OverlayValue::Real(42000.0)));
    assert_eq!(ov.get_by_key("market_cap"), Some(&fugazi::OverlayValue::Real(840e9)));
    assert_eq!(
        ov.get_by_key("circulating_supply"),
        Some(&fugazi::OverlayValue::Real(19_600_000.0))
    );
    assert_eq!(
        ov.get_by_key("total_supply"),
        Some(&fugazi::OverlayValue::Real(21_000_000.0))
    );

    // Second bucket has no circulating_supply reported → derived market_cap/price.
    assert_eq!(rows[1].time, Timestamp(1_704_153_600_000));
    let cs = match rows[1].overlays.get_by_key("circulating_supply") {
        Some(fugazi::OverlayValue::Real(v)) => *v,
        other => panic!("expected a Real, got {other:?}"),
    };
    assert!((cs - 843e9 / 43000.0).abs() < 1.0, "derived supply: {cs}");

    for w in rows.windows(2) {
        assert!(w[0].time < w[1].time, "times must be ascending");
    }
}

#[tokio::test]
async fn a_symbol_query_routes_to_the_symbol_param() {
    let server = MockServer::start().await;
    let body = serde_json::json!({
        "status": {"error_code": 0},
        "data": {"BTC": [{"quotes": [
            {"timestamp": "2024-01-01T00:00:00Z", "quote": {"USD": {"price": 1.0}}}
        ]}]}
    });
    Mock::given(method("GET"))
        .and(path(HISTORICAL))
        .and(query_param("symbol", "BTC"))
        .respond_with(ResponseTemplate::new(200).set_body_json(&body))
        .mount(&server)
        .await;

    let client = CoinMarketCap::new()
        .with_base_url(server.uri())
        .with_api_key("k");
    let rows = client
        .overlays("BTC", Interval::Day(1), Timestamp(1_704_067_200_000), Some(Timestamp(1_704_153_600_000)))
        .await
        .expect("fetch succeeds");
    assert_eq!(rows.len(), 1);
}

#[tokio::test]
async fn maps_payment_required_to_a_plan_hint() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path(HISTORICAL))
        .respond_with(ResponseTemplate::new(402).set_body_json(serde_json::json!({
            "status": {"error_code": 1006, "error_message": "plan required"}
        })))
        .mount(&server)
        .await;

    let client = CoinMarketCap::new()
        .with_base_url(server.uri())
        .with_api_key("k");
    let err = client
        .overlays("1", Interval::Day(1), Timestamp(1_704_067_200_000), Some(Timestamp(1_704_153_600_000)))
        .await
        .expect_err("expected a paid-tier error");
    match err {
        fugazi::sources::SourceError::Http { status, body } => {
            assert_eq!(status, 402);
            assert!(body.contains("paid plan"), "got: {body:?}");
        }
        other => panic!("expected Http 402, got {other:?}"),
    }
}

#[tokio::test]
async fn maps_rate_limit() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path(HISTORICAL))
        .respond_with(
            ResponseTemplate::new(429)
                .insert_header("Retry-After", "30")
                .set_body_json(serde_json::json!({"status": {"error_code": 1008}})),
        )
        .mount(&server)
        .await;

    let client = CoinMarketCap::new()
        .with_base_url(server.uri())
        .with_api_key("k");
    let err = client
        .overlays("1", Interval::Day(1), Timestamp(1_704_067_200_000), Some(Timestamp(1_704_153_600_000)))
        .await
        .expect_err("expected RateLimited");
    match err {
        fugazi::sources::SourceError::RateLimited { retry_after_ms } => {
            assert_eq!(retry_after_ms, 30_000);
        }
        other => panic!("expected RateLimited, got {other:?}"),
    }
}
