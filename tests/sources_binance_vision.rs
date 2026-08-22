#![cfg(feature = "sources")]
//! Integration test for `BinanceVision`'s ticker enumeration.
//!
//! The archive publishes no index, so the symbol vocabulary is read from the
//! *live* exchange — and the two markets have different endpoints on different
//! hosts. These tests pin that a client reads the endpoint its `Market`
//! implies and filters for that market's vocabulary; a spot client that read
//! the futures list would report a plausible-but-wrong universe rather than
//! erroring.

use fugazi::sources::SeriesSource;
use fugazi::sources::binance_vision::{BinanceVision, Market};
use wiremock::matchers::{method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};

/// The shape `api.binance.com/api/v3/exchangeInfo` returns: no `contractType`
/// key at all, which is exactly why the perpetual filter cannot be applied to
/// a spot response.
fn spot_exchange_info() -> serde_json::Value {
    serde_json::json!({
        "timezone": "UTC",
        "symbols": [
            { "symbol": "ETHUSDT",   "status": "TRADING" },
            { "symbol": "BTCUSDT",   "status": "TRADING" },
            { "symbol": "LEGACYBTC", "status": "BREAK" },
            { "symbol": "ADAUSDT",   "status": "TRADING" },
        ]
    })
}

/// The shape `fapi.binance.com/fapi/v1/exchangeInfo` returns — `contractType`
/// separates perpetuals from the dated quarterlies listed beside them.
fn futures_exchange_info() -> serde_json::Value {
    serde_json::json!({
        "timezone": "UTC",
        "symbols": [
            { "symbol": "ETHUSDT",        "status": "TRADING", "contractType": "PERPETUAL" },
            { "symbol": "BTCUSDT",        "status": "TRADING", "contractType": "PERPETUAL" },
            { "symbol": "BTCUSDT_250926", "status": "TRADING", "contractType": "CURRENT_QUARTER" },
            { "symbol": "DELISTEDUSDT",   "status": "BREAK",   "contractType": "PERPETUAL" },
            { "symbol": "ADAUSDT",        "status": "TRADING", "contractType": "PERPETUAL" },
        ]
    })
}

async fn serve(body: serde_json::Value) -> MockServer {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/exchangeInfo"))
        .respond_with(ResponseTemplate::new(200).set_body_json(body))
        .mount(&server)
        .await;
    server
}

/// A spot client must keep every `TRADING` symbol. Filtering on
/// `contractType == "PERPETUAL"` against a spot response — which carries no
/// such key — would empty the list.
#[tokio::test]
async fn spot_tickers_keep_every_trading_symbol_and_sort() {
    let server = serve(spot_exchange_info()).await;
    let client =
        BinanceVision::new().with_exchange_info_url(format!("{}/exchangeInfo", server.uri()));

    let tickers = client.tickers().await.expect("fetch succeeds");

    assert_eq!(tickers, vec!["ADAUSDT", "BTCUSDT", "ETHUSDT"]);
}

/// The futures client still drops the dated contracts and the halted symbol.
#[tokio::test]
async fn futures_tickers_filter_to_trading_perpetuals_and_sort() {
    let server = serve(futures_exchange_info()).await;
    let client =
        BinanceVision::futures().with_exchange_info_url(format!("{}/exchangeInfo", server.uri()));

    let tickers = client.tickers().await.expect("fetch succeeds");

    assert_eq!(tickers, vec!["ADAUSDT", "BTCUSDT", "ETHUSDT"]);
}

/// The regression proper: before the fix both markets read the hardcoded
/// futures endpoint, so a spot client reported the perpetual vocabulary. The
/// two endpoints are distinguished here by serving *different* bodies and
/// checking each client gets its own.
#[tokio::test]
async fn each_market_reads_its_own_exchange_info_endpoint() {
    let spot_server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/api/v3/exchangeInfo"))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
            "symbols": [{ "symbol": "SPOTONLY", "status": "TRADING" }]
        })))
        .mount(&spot_server)
        .await;

    let futures_server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/fapi/v1/exchangeInfo"))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
            "symbols": [{
                "symbol": "PERPONLY", "status": "TRADING", "contractType": "PERPETUAL"
            }]
        })))
        .mount(&futures_server)
        .await;

    let spot = BinanceVision::for_market(Market::Spot)
        .with_exchange_info_url(format!("{}/api/v3/exchangeInfo", spot_server.uri()));
    let futures = BinanceVision::for_market(Market::UsdMFutures)
        .with_exchange_info_url(format!("{}/fapi/v1/exchangeInfo", futures_server.uri()));

    assert_eq!(spot.tickers().await.expect("spot fetch"), vec!["SPOTONLY"]);
    assert_eq!(
        futures.tickers().await.expect("futures fetch"),
        vec!["PERPONLY"]
    );
}
