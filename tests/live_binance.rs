#![cfg(feature = "live")]
//! Integration tests for the Binance USDⓈ-M Futures live [`Wallet`].
//!
//! `BinanceFuturesWallet` owns its own `tokio` runtime and blocks on each REST
//! call, so it must be driven from a **synchronous** context — calling it from
//! inside a `#[tokio::test]` would nest runtimes and panic. These tests instead
//! host the `wiremock` server on a multi-threaded runtime kept alive for the
//! test's duration (its worker threads keep serving after `block_on` returns),
//! then exercise the wallet on the main thread, outside any runtime context.

use fugazi::live::BinanceFuturesWallet;
use fugazi::wallet::{Ack, Side, Units, Wallet};
use fugazi::Candle;
use wiremock::matchers::{method, path, query_param, query_param_is_missing};
use wiremock::{Mock, MockServer, ResponseTemplate};

fn exchange_info(symbol: &str) -> serde_json::Value {
    serde_json::json!({
        "symbols": [{
            "symbol": symbol,
            "filters": [
                {"filterType": "PRICE_FILTER", "tickSize": "0.10"},
                {"filterType": "LOT_SIZE", "stepSize": "0.001", "minQty": "0.001"}
            ]
        }]
    })
}

fn account(symbol: &str, position_amt: &str) -> serde_json::Value {
    serde_json::json!({
        "availableBalance": "10000.00",
        "totalWalletBalance": "10000.00",
        "totalUnrealizedProfit": "0.00",
        "positions": [
            {"symbol": symbol, "positionAmt": position_amt, "positionSide": "BOTH", "unrealizedProfit": "0.00"}
        ]
    })
}

/// Host the mock server on a kept-alive multi-threaded runtime and return it
/// alongside the runtime (both must outlive the wallet calls).
fn serve<F>(setup: F) -> (tokio::runtime::Runtime, MockServer, String)
where
    F: for<'a> FnOnce(&'a MockServer) -> std::pin::Pin<Box<dyn std::future::Future<Output = ()> + 'a>>,
{
    let rt = tokio::runtime::Runtime::new().expect("multi-thread runtime");
    let server = rt.block_on(async {
        let server = MockServer::start().await;
        setup(&server).await;
        server
    });
    let uri = server.uri();
    (rt, server, uri)
}

#[test]
fn set_position_places_market_order_and_update_reports_the_fill() {
    let (_rt, _server, uri) = serve(|server| {
        Box::pin(async move {
            Mock::given(method("GET"))
                .and(path("/fapi/v1/exchangeInfo"))
                .respond_with(ResponseTemplate::new(200).set_body_json(exchange_info("BTCUSDT")))
                .mount(server)
                .await;
            // Cursor seed (no fromId): nothing traded yet.
            Mock::given(method("GET"))
                .and(path("/fapi/v1/userTrades"))
                .and(query_param_is_missing("fromId"))
                .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!([])))
                .mount(server)
                .await;
            // The market order submission.
            Mock::given(method("POST"))
                .and(path("/fapi/v1/order"))
                .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                    "orderId": 42, "clientOrderId": "fugazi-0", "status": "NEW"
                })))
                .mount(server)
                .await;
            // Account reports the filled position on the next refresh.
            Mock::given(method("GET"))
                .and(path("/fapi/v2/account"))
                .respond_with(ResponseTemplate::new(200).set_body_json(account("BTCUSDT", "0.003")))
                .mount(server)
                .await;
            // Poll after the order (fromId present): the fill for order 42.
            Mock::given(method("GET"))
                .and(path("/fapi/v1/userTrades"))
                .and(query_param("fromId", "1"))
                .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!([
                    {"id": 5, "orderId": 42, "side": "BUY", "qty": "0.003",
                     "price": "27000.0", "commission": "0.08", "commissionAsset": "USDT"}
                ])))
                .mount(server)
                .await;
        })
    });

    let mut w = BinanceFuturesWallet::with_base_url(uri, "key", "secret");

    // Submit an absolute-units market order; it queues at the venue.
    let ack = w
        .set_position(Units { symbol: "BTCUSDT".to_string(), amount: 0.003 })
        .expect("submission accepted");
    assert!(matches!(ack, Ack::Working(_)), "market order returns Working");

    // Next bar: account refresh shows the position, poll returns the fill.
    let fills = w.update(
        "BTCUSDT".to_string(),
        Candle::new(27000.0, 27100.0, 26900.0, 27050.0, 1.0),
    );
    assert_eq!(fills.len(), 1, "expected one fill; errors: {:?}", w.errors());
    let fill = &fills[0];
    assert_eq!(fill.side, Side::Buy);
    assert!((fill.units - 0.003).abs() < 1e-9);
    assert!((fill.price - 27000.0).abs() < 1e-9);
    assert!((fill.commission - 0.08).abs() < 1e-9);

    // Reads reflect the refreshed account state.
    assert!((w.position(&"BTCUSDT".to_string()).amount - 0.003).abs() < 1e-9);
    assert!((w.funds().0 - 10000.0).abs() < 1e-9);
    assert!((w.price(&"BTCUSDT".to_string()).unwrap().0 - 27050.0).abs() < 1e-9);

    // Polling again is idempotent: the cursor advanced past the fill.
    assert!(w.poll_fills().is_empty(), "fill must not be re-reported");
}

#[test]
fn protective_stop_dedups_an_unchanged_trigger() {
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::sync::Arc;

    let order_posts = Arc::new(AtomicUsize::new(0));
    let counter = order_posts.clone();

    let (_rt, _server, uri) = serve(move |server| {
        let counter = counter.clone();
        Box::pin(async move {
            Mock::given(method("GET"))
                .and(path("/fapi/v1/exchangeInfo"))
                .respond_with(ResponseTemplate::new(200).set_body_json(exchange_info("BTCUSDT")))
                .mount(server)
                .await;
            Mock::given(method("GET"))
                .and(path("/fapi/v1/userTrades"))
                .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!([])))
                .mount(server)
                .await;
            Mock::given(method("GET"))
                .and(path("/fapi/v2/account"))
                // A held long, so a stop rests on the SELL side.
                .respond_with(ResponseTemplate::new(200).set_body_json(account("BTCUSDT", "0.003")))
                .mount(server)
                .await;
            // Count STOP_MARKET submissions; each returns a distinct orderId.
            Mock::given(method("POST"))
                .and(path("/fapi/v1/order"))
                .respond_with(move |_req: &wiremock::Request| {
                    let n = counter.fetch_add(1, Ordering::SeqCst);
                    ResponseTemplate::new(200).set_body_json(serde_json::json!({
                        "orderId": 100 + n as i64, "status": "NEW"
                    }))
                })
                .mount(server)
                .await;
            // Cancel (for the moved-trigger replace).
            Mock::given(method("DELETE"))
                .and(path("/fapi/v1/order"))
                .respond_with(
                    ResponseTemplate::new(200).set_body_json(serde_json::json!({"status": "CANCELED"})),
                )
                .mount(server)
                .await;
        })
    });

    let mut w = BinanceFuturesWallet::with_base_url(uri, "key", "secret");
    // Prime the position cache (account refresh) so the stop knows the side.
    w.update("BTCUSDT".to_string(), Candle::new(27000.0, 27100.0, 26900.0, 27000.0, 1.0));

    // Rest the same stop three bars running — only the first should hit the venue.
    for _ in 0..3 {
        w.set_stop("BTCUSDT".to_string(), fugazi::wallet::Reference(26000.0))
            .expect("stop accepted");
    }
    assert_eq!(
        order_posts.load(Ordering::SeqCst),
        1,
        "an unchanged stop trigger must not re-submit each bar"
    );

    // Moving the trigger cancels + replaces: one more POST (plus a DELETE).
    w.set_stop("BTCUSDT".to_string(), fugazi::wallet::Reference(26500.0))
        .expect("moved stop accepted");
    assert_eq!(order_posts.load(Ordering::SeqCst), 2, "a moved trigger re-submits");
}

/// Opt-in end-to-end test against the real Binance futures **testnet**.
///
/// Ignored by default and additionally gated on `BINANCE_TESTNET_KEY` /
/// `BINANCE_TESTNET_SECRET` (free keys via GitHub login at
/// <https://testnet.binancefuture.com>). Run with:
///
/// ```text
/// BINANCE_TESTNET_KEY=… BINANCE_TESTNET_SECRET=… \
///   cargo test --features live --test live_binance -- --ignored live_testnet
/// ```
///
/// Places a tiny `BTCUSDT` market order, polls the fill, asserts the position
/// moved, then flattens — leaving the testnet account as it started.
#[test]
#[ignore = "hits the real Binance futures testnet; needs BINANCE_TESTNET_{KEY,SECRET}"]
fn live_testnet_round_trip() {
    let (Ok(key), Ok(secret)) = (
        std::env::var("BINANCE_TESTNET_KEY"),
        std::env::var("BINANCE_TESTNET_SECRET"),
    ) else {
        eprintln!("skipping: set BINANCE_TESTNET_KEY / BINANCE_TESTNET_SECRET to run");
        return;
    };

    let symbol = "BTCUSDT".to_string();
    let mut w = BinanceFuturesWallet::testnet(key, secret);
    w.refresh_account().expect("account reachable on testnet");

    let start = w.position(&symbol).amount;
    let target = start + 0.002;
    w.set_position(Units { symbol: symbol.clone(), amount: target })
        .expect("market order accepted");

    // Poll for the fill (market orders fill within a bar or two on testnet).
    let mut moved = false;
    for _ in 0..10 {
        std::thread::sleep(std::time::Duration::from_millis(500));
        // A synthetic candle just carries a mark; position comes from the
        // account refresh update() performs.
        let _ = w.update(symbol.clone(), Candle::new(0.0, 0.0, 0.0, 0.0, 0.0));
        if (w.position(&symbol).amount - target).abs() < 1e-6 {
            moved = true;
            break;
        }
    }
    assert!(moved, "position did not reach target; errors: {:?}", w.errors());

    // Flatten back to where we started.
    w.set_position(Units { symbol: symbol.clone(), amount: start })
        .expect("flatten accepted");
    for _ in 0..10 {
        std::thread::sleep(std::time::Duration::from_millis(500));
        let _ = w.update(symbol.clone(), Candle::new(0.0, 0.0, 0.0, 0.0, 0.0));
        if (w.position(&symbol).amount - start).abs() < 1e-6 {
            break;
        }
    }
    assert!(
        (w.position(&symbol).amount - start).abs() < 1e-6,
        "failed to flatten back to start; errors: {:?}",
        w.errors()
    );
}
