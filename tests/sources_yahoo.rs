#![cfg(feature = "sources")]
//! Integration test for the Yahoo Finance `CandleSource` implementation.
//!
//! Spins up a `wiremock` server on a random port, stubs
//! `/v8/finance/chart/{symbol}` with a canned chart-API response, and verifies
//! the client decodes the flat OHLCV arrays, scales second-timestamps to
//! millisecond `Timestamp`s, skips null bars, and maps `Not Found` / `429`
//! into the right `SourceError` variants.

use fugazi::sources::{CandleSource, Interval, Timestamp, Yahoo};
use wiremock::matchers::{method, path, query_param};
use wiremock::{Mock, MockServer, ResponseTemplate};

#[tokio::test]
async fn decodes_chart_response() {
    let server = MockServer::start().await;

    let body = serde_json::json!({
        "chart": {
            "result": [{
                "meta": {"symbol": "SPY", "regularMarketPrice": 470.0},
                "timestamp": [1_704_067_200_i64, 1_704_153_600_i64, 1_704_240_000_i64],
                "indicators": {
                    "quote": [{
                        "open":   [470.0, 471.0, 472.5],
                        "high":   [473.0, 472.0, 474.0],
                        "low":    [469.5, 470.5, 472.0],
                        "close":  [471.5, 471.8, 473.5],
                        "volume": [1_000_000, 900_000, 850_000]
                    }],
                    "adjclose": [{
                        "adjclose": [468.0, 468.3, 470.1]
                    }]
                }
            }],
            "error": null
        }
    });

    Mock::given(method("GET"))
        .and(path("/v8/finance/chart/SPY"))
        .and(query_param("interval", "1d"))
        .respond_with(ResponseTemplate::new(200).set_body_json(&body))
        .mount(&server)
        .await;

    let client = Yahoo::new().with_base_url(server.uri());

    let bars = client
        .atoms(
            "SPY",
            Interval::Day(1),
            Timestamp(1_704_067_200_000),
            Some(Timestamp(1_704_326_400_000)),
        )
        .await
        .expect("fetch succeeds");

    assert_eq!(bars.len(), 3);
    assert_eq!(bars[0].time, Some(Timestamp(1_704_067_200_000)));
    assert_eq!(bars[0].candle.open, 470.0);
    assert_eq!(bars[0].candle.close, 471.5);
    assert_eq!(bars[2].time, Some(Timestamp(1_704_240_000_000)));
    assert_eq!(bars[2].candle.close, 473.5);
    for w in bars.windows(2) {
        assert!(w[0].time < w[1].time, "times must be ascending");
    }

    // adj_close made it onto every atom's overlay side channel.
    let ov = bars[0].overlays.as_ref().expect("Yahoo atoms carry overlays");
    assert_eq!(
        ov.get_by_key("adj_close"),
        Some(&fugazi::OverlayValue::Real(468.0))
    );
}

#[tokio::test]
async fn skips_bars_with_null_fields() {
    let server = MockServer::start().await;

    let body = serde_json::json!({
        "chart": {
            "result": [{
                "meta": {"symbol": "SPY"},
                "timestamp": [1_704_067_200_i64, 1_704_153_600_i64, 1_704_240_000_i64],
                "indicators": {
                    "quote": [{
                        "open":   [470.0, serde_json::Value::Null, 472.5],
                        "high":   [473.0, 472.0, 474.0],
                        "low":    [469.5, 470.5, 472.0],
                        "close":  [471.5, 471.8, 473.5],
                        "volume": [1_000_000, 900_000, 850_000]
                    }]
                }
            }],
            "error": null
        }
    });

    Mock::given(method("GET"))
        .and(path("/v8/finance/chart/SPY"))
        .respond_with(ResponseTemplate::new(200).set_body_json(&body))
        .mount(&server)
        .await;

    let client = Yahoo::new().with_base_url(server.uri());
    let bars = client
        .atoms(
            "SPY",
            Interval::Day(1),
            Timestamp(1_704_067_200_000),
            Some(Timestamp(1_704_326_400_000)),
        )
        .await
        .expect("fetch succeeds");

    assert_eq!(bars.len(), 2);
    assert_eq!(bars[0].time, Some(Timestamp(1_704_067_200_000)));
    assert_eq!(bars[1].time, Some(Timestamp(1_704_240_000_000)));
}

#[tokio::test]
async fn maps_unknown_symbol_error() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/v8/finance/chart/NOTASYMBOL"))
        .respond_with(
            ResponseTemplate::new(404).set_body_json(serde_json::json!({
                "chart": {
                    "result": null,
                    "error": {
                        "code": "Not Found",
                        "description": "No data found, symbol may be delisted"
                    }
                }
            })),
        )
        .mount(&server)
        .await;

    let client = Yahoo::new().with_base_url(server.uri());
    let err = client
        .atoms(
            "NOTASYMBOL",
            Interval::Day(1),
            Timestamp(1_704_067_200_000),
            Some(Timestamp(1_704_153_600_000)),
        )
        .await
        .expect_err("expected UnknownSymbol");
    match err {
        fugazi::sources::SourceError::UnknownSymbol(msg) => {
            assert!(msg.contains("No data found"), "got: {msg:?}")
        }
        other => panic!("expected UnknownSymbol, got {other:?}"),
    }
}

#[tokio::test]
async fn maps_rate_limit_error() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/v8/finance/chart/SPY"))
        .respond_with(
            ResponseTemplate::new(429)
                .append_header("Retry-After", "12")
                .set_body_string("Too Many Requests"),
        )
        .mount(&server)
        .await;

    let client = Yahoo::new().with_base_url(server.uri());
    let err = client
        .atoms(
            "SPY",
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

#[tokio::test]
async fn maps_inline_error_on_success_status() {
    // Yahoo can return HTTP 200 with an error object in the body.
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/v8/finance/chart/NOTASYMBOL"))
        .respond_with(
            ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "chart": {
                    "result": null,
                    "error": {
                        "code": "Not Found",
                        "description": "No data found"
                    }
                }
            })),
        )
        .mount(&server)
        .await;

    let client = Yahoo::new().with_base_url(server.uri());
    let err = client
        .atoms(
            "NOTASYMBOL",
            Interval::Day(1),
            Timestamp(1_704_067_200_000),
            Some(Timestamp(1_704_153_600_000)),
        )
        .await
        .expect_err("expected UnknownSymbol");
    assert!(matches!(err, fugazi::sources::SourceError::UnknownSymbol(_)));
}
