#![cfg(feature = "sources")]
//! Integration test for the Kraken `SeriesSource` implementation.
//!
//! Spins up a `wiremock` server on a random port and stubs `/0/public/OHLC`.
//! The behaviours under test are the ones Kraken's API shape makes easy to get
//! wrong: the result key is not derivable from the requested pair, the response
//! mixes bare numbers with quoted strings inside one row, timestamps are
//! seconds rather than milliseconds, the final row is a still-forming bar that
//! must be dropped, and application errors arrive with **HTTP 200**.

use fugazi::sources::{Interval, Kraken, SeriesSource, SourceError, Timestamp};
use wiremock::matchers::{method, path, query_param};
use wiremock::{Mock, MockServer, ResponseTemplate};

/// Build a Kraken OHLC row: `[time, open, high, low, close, vwap, volume,
/// count]`. Deliberately mixed-typed, exactly as the real API sends it — index
/// 0 and 7 bare numbers, 1..=6 quoted strings.
fn candle(ts: i64, o: &str, h: &str, l: &str, c: &str, vwap: &str, v: &str) -> serde_json::Value {
    serde_json::json!([ts, o, h, l, c, vwap, v, 1234])
}

/// Wrap candle rows in Kraken's envelope. `key` is the internal pair id the API
/// echoes back, which the request spelling does not determine; `last` is the
/// open time of the final *committed* bar.
fn envelope(key: &str, rows: &[serde_json::Value], last: i64) -> serde_json::Value {
    serde_json::json!({
        "error": [],
        "result": { key: rows, "last": last }
    })
}

/// Five daily bars from 2024-01-01. The last one is the still-forming bar, so
/// `last` points at the fourth.
fn five_daily_bars() -> Vec<serde_json::Value> {
    vec![
        candle(
            1_704_067_200,
            "42000.0",
            "42500.5",
            "41800.25",
            "42100.0",
            "42050.0",
            "100.0",
        ),
        candle(
            1_704_153_600,
            "42100.0",
            "42300.0",
            "42000.0",
            "42250.0",
            "42180.0",
            "80.0",
        ),
        candle(
            1_704_240_000,
            "42250.0",
            "42400.0",
            "42150.0",
            "42350.0",
            "42300.0",
            "90.0",
        ),
        candle(
            1_704_326_400,
            "42350.0",
            "42500.0",
            "42300.0",
            "42450.0",
            "42400.0",
            "70.0",
        ),
        // Still forming — Kraken always appends this, and it mutates between
        // calls. `last` below excludes it.
        candle(
            1_704_412_800,
            "42450.0",
            "42600.0",
            "42400.0",
            "42550.0",
            "42500.0",
            "60.0",
        ),
    ]
}

const LAST_COMMITTED: i64 = 1_704_326_400;

async fn ohlc_server(body: serde_json::Value) -> MockServer {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/0/public/OHLC"))
        .respond_with(ResponseTemplate::new(200).set_body_json(body))
        .mount(&server)
        .await;
    server
}

#[tokio::test]
async fn decodes_candles_and_drops_the_forming_bar() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/0/public/OHLC"))
        // The pair goes out verbatim, the cadence as a minute count, and
        // `since` in *seconds* — not the crate's milliseconds.
        .and(query_param("pair", "XBTUSD"))
        .and(query_param("interval", "1440"))
        .and(query_param("since", "1704067200"))
        .respond_with(ResponseTemplate::new(200).set_body_json(envelope(
            // Requested `XBTUSD`, answered under `XXBTZUSD`: the client must
            // take the sole non-`last` key rather than transform the request.
            "XXBTZUSD",
            &five_daily_bars(),
            LAST_COMMITTED,
        )))
        .mount(&server)
        .await;

    let bars = Kraken::new()
        .with_base_url(server.uri())
        .atoms(
            "XBTUSD",
            Interval::Day(1),
            Timestamp(1_704_067_200_000),
            None,
        )
        .await
        .expect("fetch succeeds");

    // Five rows in, four out: the forming bar is dropped.
    assert_eq!(bars.len(), 4, "the still-forming bar must not be returned");
    assert!(
        bars.iter()
            .all(|b| b.time.unwrap().0 <= LAST_COMMITTED * 1_000),
        "no returned bar may post-date `last`"
    );

    // Seconds on the wire become milliseconds in the crate.
    assert_eq!(bars[0].time, Some(Timestamp(1_704_067_200_000)));
    assert_eq!(bars[3].time, Some(Timestamp(1_704_326_400_000)));

    let first = bars[0].candle.unwrap();
    assert_eq!(first.open, 42000.0);
    assert_eq!(first.high, 42500.5);
    assert_eq!(first.low, 41800.25);
    assert_eq!(first.close, 42100.0);
    // Index 6 is volume. Reading index 5 here would silently return the VWAP.
    assert_eq!(first.volume, 100.0);

    for w in bars.windows(2) {
        assert!(w[0].time < w[1].time, "times must be ascending");
    }

    let ov = bars[0]
        .overlays
        .as_ref()
        .expect("Kraken atoms carry overlays");
    assert!(ov.get_by_key("vwap").is_some());
    assert!(ov.get_by_key("n_trades").is_some());
}

#[tokio::test]
async fn until_is_exclusive() {
    let server = ohlc_server(envelope("XXBTZUSD", &five_daily_bars(), LAST_COMMITTED)).await;

    let bars = Kraken::new()
        .with_base_url(server.uri())
        .atoms(
            "XBTUSD",
            Interval::Day(1),
            Timestamp(1_704_067_200_000),
            Some(Timestamp(1_704_240_000_000)),
        )
        .await
        .expect("fetch succeeds");

    // The bar opening exactly at `until` is excluded; `since` is inclusive.
    assert_eq!(bars.len(), 2);
    assert_eq!(bars[0].time, Some(Timestamp(1_704_067_200_000)));
    assert_eq!(bars[1].time, Some(Timestamp(1_704_153_600_000)));
}

#[tokio::test]
async fn unknown_pair_is_reported_despite_http_200() {
    // Kraken answers a bad pair with HTTP 200 and a populated `error` array,
    // so a client keying off status alone would read this as an empty success.
    let server = ohlc_server(serde_json::json!({
        "error": ["EQuery:Unknown asset pair"],
        "result": {}
    }))
    .await;

    let err = Kraken::new()
        .with_base_url(server.uri())
        .atoms(
            "NOTAPAIR",
            Interval::Day(1),
            Timestamp(1_704_067_200_000),
            None,
        )
        .await
        .expect_err("expected UnknownSymbol");

    match err {
        SourceError::UnknownSymbol(msg) => assert!(msg.contains("Unknown asset pair"), "{msg:?}"),
        other => panic!("expected UnknownSymbol, got {other:?}"),
    }
}

#[tokio::test]
async fn throttling_maps_to_rate_limited() {
    // `EService:Throttled` carries a trailing retry stamp, so the match has to
    // be by prefix rather than equality.
    let server = ohlc_server(serde_json::json!({
        "error": ["EService:Throttled: 1704067200"],
        "result": {}
    }))
    .await;

    let err = Kraken::new()
        .with_base_url(server.uri())
        .atoms("XBTUSD", Interval::Day(1), Timestamp(0), None)
        .await
        .expect_err("expected RateLimited");

    assert!(
        matches!(err, SourceError::RateLimited { .. }),
        "expected RateLimited, got {err:?}"
    );
}

#[tokio::test]
async fn unsupported_cadence_rejects_before_any_request() {
    // No mock is mounted: reaching the network at all would fail the test, and
    // Kraken would answer an unsupported interval with an opaque
    // `EGeneral:Invalid arguments` anyway.
    let server = MockServer::start().await;

    let err = Kraken::new()
        .with_base_url(server.uri())
        .atoms("XBTUSD", Interval::Hour(2), Timestamp(0), None)
        .await
        .expect_err("2h is not in Kraken's vocabulary");

    assert!(
        matches!(err, SourceError::UnsupportedInterval(_)),
        "expected UnsupportedInterval, got {err:?}"
    );
    assert!(
        server
            .received_requests()
            .await
            .unwrap_or_default()
            .is_empty(),
        "an unsupported cadence must not reach the network"
    );
}

#[tokio::test]
async fn tickers_returns_sorted_altnames() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/0/public/AssetPairs"))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
            "error": [],
            "result": {
                "XETHZUSD": { "altname": "ETHUSD", "wsname": "ETH/USD", "status": "online" },
                "XXBTZUSD": { "altname": "XBTUSD", "wsname": "XBT/USD", "status": "online" },
                "ADAEUR":   { "altname": "ADAEUR", "wsname": "ADA/EUR", "status": "online" },
                "DEADPAIR": { "altname": "DEADPAIR", "status": "delisted" },
            }
        })))
        .mount(&server)
        .await;

    let tickers = <Kraken as SeriesSource>::tickers(&Kraken::new().with_base_url(server.uri()))
        .await
        .expect("fetch succeeds");

    // `altname` is reported, not the internal `XXBTZUSD` key — the altname is
    // what Kraken accepts back as `pair`. Delisted pairs are filtered out.
    assert_eq!(tickers, vec!["ADAEUR", "ETHUSD", "XBTUSD"]);
}
