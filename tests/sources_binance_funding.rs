#![cfg(feature = "sources")]
//! Integration test for the Binance perpetual funding-rate `OverlaySource`.
//!
//! Spins up a `wiremock` server, stubs `/fapi/v1/fundingRate`, and verifies
//! that the client pages through the range, buckets settlements onto the
//! requested cadence by **summing** them (funding is an accrual, not a level),
//! and maps errors to the specific `SourceError` variants.

use fugazi::sources::{BinanceFunding, Interval, OverlaySource, Timestamp};
use wiremock::matchers::{method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};

const DAY_MS: i64 = 86_400_000;
const H8_MS: i64 = 8 * 3_600_000;
/// 2024-01-01T00:00:00Z.
const T0: i64 = 1_704_067_200_000;

/// One settlement row, in the endpoint's wire shape. `markPrice` is included
/// (and often empty in real responses) to pin that the decoder ignores it.
fn settlement(funding_time: i64, rate: &str) -> serde_json::Value {
    serde_json::json!({
        "symbol": "BTCUSDT",
        "fundingTime": funding_time,
        "fundingRate": rate,
        "markPrice": "",
    })
}

/// The `funding_rate` cell of a row.
fn rate_of(row: &fugazi::sources::OverlayRow) -> f64 {
    let schema = row.overlays.schema().clone();
    let idx = schema.index_of("funding_rate").expect("column present");
    match row.overlays.get(idx) {
        Some(fugazi::types::OverlayValue::Real(x)) => *x,
        other => panic!("expected a Real funding_rate, got {other:?}"),
    }
}

#[tokio::test]
async fn sums_intraday_settlements_into_daily_buckets() {
    let server = MockServer::start().await;

    // Two days, three 8-hourly settlements each.
    let rows: Vec<serde_json::Value> = vec![
        settlement(T0, "0.0001"),
        settlement(T0 + H8_MS, "0.0002"),
        settlement(T0 + 2 * H8_MS, "0.0003"),
        settlement(T0 + DAY_MS, "-0.0001"),
        settlement(T0 + DAY_MS + H8_MS, "0.0005"),
        settlement(T0 + DAY_MS + 2 * H8_MS, "0.0002"),
    ];
    Mock::given(method("GET"))
        .and(path("/fapi/v1/fundingRate"))
        .respond_with(ResponseTemplate::new(200).set_body_json(&rows))
        .mount(&server)
        .await;

    let out = BinanceFunding::new()
        .with_base_url(server.uri())
        .overlays(
            "BTCUSDT",
            Interval::Day(1),
            Timestamp(T0),
            Some(Timestamp(T0 + 2 * DAY_MS)),
        )
        .await
        .expect("fetch succeeds");

    // One row per day, each the sum of that day's three settlements.
    assert_eq!(out.len(), 2);
    assert_eq!(out[0].time.0, T0);
    assert_eq!(out[1].time.0, T0 + DAY_MS);
    assert!(
        (rate_of(&out[0]) - 0.0006).abs() < 1e-12,
        "{}",
        rate_of(&out[0])
    );
    assert!(
        (rate_of(&out[1]) - 0.0006).abs() < 1e-12,
        "{}",
        rate_of(&out[1])
    );
}

#[tokio::test]
async fn at_the_native_cadence_each_bucket_holds_one_settlement() {
    let server = MockServer::start().await;
    let rows: Vec<serde_json::Value> = vec![
        settlement(T0, "0.0001"),
        settlement(T0 + H8_MS, "0.0002"),
        settlement(T0 + 2 * H8_MS, "0.0003"),
    ];
    Mock::given(method("GET"))
        .and(path("/fapi/v1/fundingRate"))
        .respond_with(ResponseTemplate::new(200).set_body_json(&rows))
        .mount(&server)
        .await;

    let out = BinanceFunding::new()
        .with_base_url(server.uri())
        .overlays(
            "BTCUSDT",
            Interval::Hour(8),
            Timestamp(T0),
            Some(Timestamp(T0 + DAY_MS)),
        )
        .await
        .expect("fetch succeeds");

    assert_eq!(out.len(), 3);
    let rates: Vec<f64> = out.iter().map(rate_of).collect();
    assert!((rates[0] - 0.0001).abs() < 1e-12);
    assert!((rates[1] - 0.0002).abs() < 1e-12);
    assert!((rates[2] - 0.0003).abs() < 1e-12);
}

#[tokio::test]
async fn settlements_outside_the_requested_range_are_dropped() {
    let server = MockServer::start().await;
    // The endpoint can return rows on either side of the asked-for window.
    let rows: Vec<serde_json::Value> = vec![
        settlement(T0 - DAY_MS, "9.0"),
        settlement(T0, "0.0001"),
        settlement(T0 + 2 * DAY_MS, "9.0"),
    ];
    Mock::given(method("GET"))
        .and(path("/fapi/v1/fundingRate"))
        .respond_with(ResponseTemplate::new(200).set_body_json(&rows))
        .mount(&server)
        .await;

    let out = BinanceFunding::new()
        .with_base_url(server.uri())
        .overlays(
            "BTCUSDT",
            Interval::Day(1),
            Timestamp(T0),
            Some(Timestamp(T0 + DAY_MS)),
        )
        .await
        .expect("fetch succeeds");

    assert_eq!(out.len(), 1);
    assert_eq!(out[0].time.0, T0);
    assert!((rate_of(&out[0]) - 0.0001).abs() < 1e-12);
}

#[tokio::test]
async fn maps_unknown_symbol_error() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/fapi/v1/fundingRate"))
        .respond_with(ResponseTemplate::new(400).set_body_json(serde_json::json!({
            "code": -1121,
            "msg": "Invalid symbol.",
        })))
        .mount(&server)
        .await;

    let err = BinanceFunding::new()
        .with_base_url(server.uri())
        .overlays(
            "NOPE",
            Interval::Day(1),
            Timestamp(T0),
            Some(Timestamp(T0 + DAY_MS)),
        )
        .await
        .expect_err("unknown symbol should fail");
    assert!(
        matches!(err, fugazi::sources::SourceError::UnknownSymbol(_)),
        "expected UnknownSymbol, got {err:?}"
    );
}

#[tokio::test]
async fn maps_rate_limit_error() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/fapi/v1/fundingRate"))
        .respond_with(
            ResponseTemplate::new(429)
                .insert_header("retry-after", "3")
                .set_body_string("too many requests"),
        )
        .mount(&server)
        .await;

    let err = BinanceFunding::new()
        .with_base_url(server.uri())
        .overlays(
            "BTCUSDT",
            Interval::Day(1),
            Timestamp(T0),
            Some(Timestamp(T0 + DAY_MS)),
        )
        .await
        .expect_err("429 should fail");
    match err {
        fugazi::sources::SourceError::RateLimited { retry_after_ms } => {
            assert_eq!(retry_after_ms, 3_000);
        }
        other => panic!("expected RateLimited, got {other:?}"),
    }
}

#[tokio::test]
async fn rejects_a_sub_hourly_cadence_before_any_request() {
    // No mock mounted: if the client made a request this would fail differently.
    let server = MockServer::start().await;
    let err = BinanceFunding::new()
        .with_base_url(server.uri())
        .overlays(
            "BTCUSDT",
            Interval::Minute(15),
            Timestamp(T0),
            Some(Timestamp(T0 + DAY_MS)),
        )
        .await
        .expect_err("sub-hourly should be rejected");
    assert!(
        matches!(err, fugazi::sources::SourceError::UnsupportedInterval(_)),
        "expected UnsupportedInterval, got {err:?}"
    );
}
