#![cfg(feature = "live")]
//! Integration tests for the Coinbase Advanced Trade spot live [`Wallet`].
//!
//! `CoinbaseWallet` owns its own `tokio` runtime and blocks on each REST call, so
//! it must be driven from a **synchronous** context — calling it from inside a
//! `#[tokio::test]` would nest runtimes and panic. These tests host the
//! `wiremock` server on a multi-threaded runtime kept alive for the test's
//! duration, then exercise the wallet on the main thread.
//!
//! The mock never verifies the ES256 JWT (that path is unit-tested in
//! `src/live/coinbase.rs`), but constructing the wallet parses a real P-256 key,
//! so each test builds a throwaway one. They pin the spot convention: a
//! `position` is a base-asset balance, `set_position` diffs it and market-orders
//! the difference, and a fill comes back in base units.

mod common;

use common::net::serve;
use fugazi::Candle;
use fugazi::live::CoinbaseWallet;
use fugazi::wallet::{Ack, Reference, Side, Size, Units, Wallet};
use wiremock::matchers::{method, path};
use wiremock::{Mock, ResponseTemplate};

const SYMBOL: &str = "BTC-USD";
const KEY_NAME: &str = "organizations/o/apiKeys/k";

/// A throwaway P-256 private key PEM (PKCS#8) from a fixed scalar — deterministic,
/// so the test needs no randomness and commits no real secret.
fn test_pem() -> String {
    use p256::SecretKey;
    use p256::pkcs8::EncodePrivateKey;
    let secret = SecretKey::from_bytes((&[7u8; 32]).into()).expect("valid P-256 scalar");
    secret.to_pkcs8_pem(Default::default()).unwrap().to_string()
}

/// The product grid: 1e-8 base increment, 0.01 quote increment, no minimum.
fn product() -> serde_json::Value {
    serde_json::json!({
        "product_id": SYMBOL,
        "base_increment": "0.00000001",
        "quote_increment": "0.01",
        "base_min_size": "0"
    })
}

/// An accounts envelope with the given BTC and USD available balances.
fn accounts(btc: &str, usd: &str) -> serde_json::Value {
    serde_json::json!({
        "accounts": [
            { "uuid": "a1", "currency": "BTC",
              "available_balance": { "value": btc, "currency": "BTC" } },
            { "uuid": "a2", "currency": "USD",
              "available_balance": { "value": usd, "currency": "USD" } },
        ],
        "has_next": false,
        "cursor": ""
    })
}

fn no_fills() -> serde_json::Value {
    serde_json::json!({ "fills": [] })
}


fn wallet(uri: String) -> CoinbaseWallet {
    CoinbaseWallet::with_base_url(uri, KEY_NAME, &test_pem()).expect("key parses")
}

/// Unlike OKX's, this one is genuinely per-account — Advanced Trade quotes the
/// same base against several currencies, so the answer follows the wallet's own
/// configured quote leg rather than the venue. Needs no mock: it is read back
/// from construction, not from the balance endpoint.
#[test]
fn a_spot_account_reports_the_quote_currency_it_was_built_against() {
    let dead = || "http://127.0.0.1:1".to_string();
    // The default is what `DEFAULT_QUOTE_CCY` names, and it is stated rather
    // than left implicit — the whole point is that a caller never has to assume.
    assert_eq!(wallet(dead()).quote_ccy(), Some("USD"));
    // And it follows the override, since `funds` reads that currency's balance.
    assert_eq!(
        wallet(dead()).with_quote_ccy("EUR").quote_ccy(),
        Some("EUR")
    );
}

#[test]
fn set_position_market_buys_the_difference_and_update_reports_the_fill() {
    use std::sync::Arc;
    use std::sync::atomic::{AtomicUsize, Ordering};

    // The fills endpoint is stateful: the cursor-seed poll during submission is
    // empty, and every later poll returns the fill.
    let fill_calls = Arc::new(AtomicUsize::new(0));
    let counter = fill_calls.clone();

    let mock = serve(move |server| {
        let counter = counter.clone();
        Box::pin(async move {
            Mock::given(method("GET"))
                .and(path("/api/v3/brokerage/market/products/BTC-USD"))
                .respond_with(ResponseTemplate::new(200).set_body_json(product()))
                .mount(server)
                .await;
            // After the fill the account holds 0.03 BTC and 10000 USD.
            Mock::given(method("GET"))
                .and(path("/api/v3/brokerage/accounts"))
                .respond_with(ResponseTemplate::new(200).set_body_json(accounts("0.03", "10000")))
                .mount(server)
                .await;
            Mock::given(method("GET"))
                .and(path("/api/v3/brokerage/orders/historical/fills"))
                .respond_with(move |_req: &wiremock::Request| {
                    let n = counter.fetch_add(1, Ordering::SeqCst);
                    let fills = if n == 0 {
                        serde_json::json!([])
                    } else {
                        serde_json::json!([{
                            "trade_id": "111", "order_id": "ORD1", "side": "BUY",
                            "size": "0.03", "price": "27000", "commission": "0.08",
                            "sequence_timestamp": "2024-01-01T00:00:00Z"
                        }])
                    };
                    ResponseTemplate::new(200).set_body_json(serde_json::json!({ "fills": fills }))
                })
                .mount(server)
                .await;
            Mock::given(method("POST"))
                .and(path("/api/v3/brokerage/orders"))
                .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                    "success": true, "success_response": { "order_id": "ORD1" }
                })))
                .mount(server)
                .await;
        })
    });
    let uri = mock.uri.clone();

    let mut w = wallet(uri);

    // From flat, target 0.03 BTC → a market buy of 0.03.
    let ack = w
        .set_position(Units { symbol: SYMBOL.to_string(), amount: 0.03 })
        .expect("submission accepted");
    assert!(matches!(ack, Ack::Working(_)), "market order returns Working");

    // Next bar: account refresh shows the balance, poll returns the fill.
    let fills = w.update(SYMBOL.to_string(), Candle::new(27000.0, 27100.0, 26900.0, 27050.0, 1.0));
    assert_eq!(fills.len(), 1, "expected one fill; errors: {:?}", w.errors());
    let fill = &fills[0];
    assert_eq!(fill.side, Side::Buy);
    assert!((fill.units - 0.03).abs() < 1e-9, "fill in base units, got {}", fill.units);
    assert!((fill.price - 27000.0).abs() < 1e-9);
    assert!((fill.commission - 0.08).abs() < 1e-9);

    // Reads reflect the refreshed account state (spot balances).
    assert!((w.position(&SYMBOL.to_string()).amount - 0.03).abs() < 1e-9);
    assert!((w.funds().0 - 10000.0).abs() < 1e-9, "funds = quote balance");
    // Equity = quote + base marked at the last close (10000 + 0.03 * 27050).
    assert!((w.equity().0 - 10811.5).abs() < 1e-6, "equity = {}", w.equity().0);
    assert!((w.price(&SYMBOL.to_string()).unwrap().0 - 27050.0).abs() < 1e-9);

    // Polling again is idempotent: the trade id is already seen.
    assert!(w.poll_fills().is_empty(), "fill must not be re-reported");
}

#[test]
fn a_protective_stop_dedups_an_unchanged_trigger() {
    use std::sync::Arc;
    use std::sync::atomic::{AtomicUsize, Ordering};

    let order_posts = Arc::new(AtomicUsize::new(0));
    let cancels = Arc::new(AtomicUsize::new(0));
    let (c_posts, c_cancels) = (order_posts.clone(), cancels.clone());

    let mock = serve(move |server| {
        let (c_posts, c_cancels) = (c_posts.clone(), c_cancels.clone());
        Box::pin(async move {
            Mock::given(method("GET"))
                .and(path("/api/v3/brokerage/market/products/BTC-USD"))
                .respond_with(ResponseTemplate::new(200).set_body_json(product()))
                .mount(server)
                .await;
            // A held 0.03 BTC long, so a stop rests reduce-only on the SELL side.
            Mock::given(method("GET"))
                .and(path("/api/v3/brokerage/accounts"))
                .respond_with(ResponseTemplate::new(200).set_body_json(accounts("0.03", "10000")))
                .mount(server)
                .await;
            Mock::given(method("GET"))
                .and(path("/api/v3/brokerage/orders/historical/fills"))
                .respond_with(ResponseTemplate::new(200).set_body_json(no_fills()))
                .mount(server)
                .await;
            Mock::given(method("POST"))
                .and(path("/api/v3/brokerage/orders"))
                .respond_with(move |_req: &wiremock::Request| {
                    let n = c_posts.fetch_add(1, Ordering::SeqCst);
                    ResponseTemplate::new(200).set_body_json(serde_json::json!({
                        "success": true, "success_response": { "order_id": format!("ORD{n}") }
                    }))
                })
                .mount(server)
                .await;
            Mock::given(method("POST"))
                .and(path("/api/v3/brokerage/orders/batch_cancel"))
                .respond_with(move |_req: &wiremock::Request| {
                    c_cancels.fetch_add(1, Ordering::SeqCst);
                    ResponseTemplate::new(200).set_body_json(serde_json::json!({
                        "results": [{ "success": true, "order_id": "ORD0" }]
                    }))
                })
                .mount(server)
                .await;
        })
    });
    let uri = mock.uri.clone();

    let mut w = wallet(uri);
    // Prime the balance cache so the stop knows what it is protecting.
    w.update(SYMBOL.to_string(), Candle::new(27000.0, 27100.0, 26900.0, 27000.0, 1.0));

    // Rest the same stop three bars running — only the first hits the venue.
    for _ in 0..3 {
        w.set_stop(SYMBOL.to_string(), Reference(26000.0), Size::position_frac(1.0))
            .expect("stop accepted");
    }
    assert_eq!(
        order_posts.load(Ordering::SeqCst),
        1,
        "an unchanged stop trigger must not re-submit each bar"
    );

    // Moving the trigger cancels + replaces: one more POST, one cancel.
    w.set_stop(SYMBOL.to_string(), Reference(26500.0), Size::position_frac(1.0))
        .expect("moved stop accepted");
    assert_eq!(order_posts.load(Ordering::SeqCst), 2, "a moved trigger re-submits");
    assert_eq!(cancels.load(Ordering::SeqCst), 1, "and cancels the old leg");
}

#[test]
fn a_limit_order_sends_a_limit_config_and_dedups_an_unchanged_resubmit() {
    use std::sync::Arc;
    use std::sync::Mutex;
    use std::sync::atomic::{AtomicUsize, Ordering};

    let posts = Arc::new(AtomicUsize::new(0));
    let cancels = Arc::new(AtomicUsize::new(0));
    let last_body = Arc::new(Mutex::new(String::new()));
    let (c_posts, c_cancels, c_body) = (posts.clone(), cancels.clone(), last_body.clone());

    let mock = serve(move |server| {
        let (c_posts, c_cancels, c_body) = (c_posts.clone(), c_cancels.clone(), c_body.clone());
        Box::pin(async move {
            Mock::given(method("GET"))
                .and(path("/api/v3/brokerage/market/products/BTC-USD"))
                .respond_with(ResponseTemplate::new(200).set_body_json(product()))
                .mount(server)
                .await;
            // Flat, so a buy target is a plain buy of the whole size.
            Mock::given(method("GET"))
                .and(path("/api/v3/brokerage/accounts"))
                .respond_with(ResponseTemplate::new(200).set_body_json(accounts("0", "10000")))
                .mount(server)
                .await;
            Mock::given(method("GET"))
                .and(path("/api/v3/brokerage/orders/historical/fills"))
                .respond_with(ResponseTemplate::new(200).set_body_json(no_fills()))
                .mount(server)
                .await;
            Mock::given(method("POST"))
                .and(path("/api/v3/brokerage/orders"))
                .respond_with(move |req: &wiremock::Request| {
                    *c_body.lock().unwrap() = String::from_utf8_lossy(&req.body).to_string();
                    let n = c_posts.fetch_add(1, Ordering::SeqCst);
                    ResponseTemplate::new(200).set_body_json(serde_json::json!({
                        "success": true, "success_response": { "order_id": format!("ORD{n}") }
                    }))
                })
                .mount(server)
                .await;
            Mock::given(method("POST"))
                .and(path("/api/v3/brokerage/orders/batch_cancel"))
                .respond_with(move |_req: &wiremock::Request| {
                    c_cancels.fetch_add(1, Ordering::SeqCst);
                    ResponseTemplate::new(200).set_body_json(serde_json::json!({
                        "results": [{ "success": true, "order_id": "ORD0" }]
                    }))
                })
                .mount(server)
                .await;
        })
    });
    let uri = mock.uri.clone();

    let mut w = wallet(uri);
    w.update(SYMBOL.to_string(), Candle::new(27000.0, 27100.0, 26900.0, 27000.0, 1.0));

    w.set_limit(SYMBOL.to_string(), Side::Buy, Size::units(0.05), Reference(26000.0))
        .expect("limit accepted");

    let body = last_body.lock().unwrap().clone();
    assert!(body.contains("limit_limit_gtc"), "not a limit order: {body}");
    assert!(body.contains("\"side\":\"BUY\""), "wrong side: {body}");
    assert!(body.contains("\"limit_price\":\"26000.00\""), "limit price not sent: {body}");
    assert!(body.contains("\"base_size\":\"0.05000000\""), "base size not sent: {body}");

    // Re-submitting the same order every bar must not pile up venue orders.
    for _ in 0..3 {
        w.set_limit(SYMBOL.to_string(), Side::Buy, Size::units(0.05), Reference(26000.0))
            .expect("unchanged limit accepted");
    }
    assert_eq!(posts.load(Ordering::SeqCst), 1, "an unchanged limit must not re-submit each bar");

    // Moving the price cancels and replaces.
    w.set_limit(SYMBOL.to_string(), Side::Buy, Size::units(0.05), Reference(26500.0))
        .expect("moved limit accepted");
    assert_eq!(posts.load(Ordering::SeqCst), 2, "a moved limit re-submits");
    assert_eq!(cancels.load(Ordering::SeqCst), 1, "and cancels the old one");

    // And an explicit cancel withdraws it.
    w.cancel_limit(&SYMBOL.to_string()).expect("cancel ok");
    assert_eq!(cancels.load(Ordering::SeqCst), 2);
}

#[test]
fn a_venue_rejected_order_surfaces_through_take_rejections() {
    let mock = serve(|server| {
        Box::pin(async move {
            Mock::given(method("GET"))
                .and(path("/api/v3/brokerage/market/products/BTC-USD"))
                .respond_with(ResponseTemplate::new(200).set_body_json(product()))
                .mount(server)
                .await;
            Mock::given(method("GET"))
                .and(path("/api/v3/brokerage/orders/historical/fills"))
                .respond_with(ResponseTemplate::new(200).set_body_json(no_fills()))
                .mount(server)
                .await;
            // Coinbase returns 200 with `success: false` for a business rejection.
            Mock::given(method("POST"))
                .and(path("/api/v3/brokerage/orders"))
                .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                    "success": false,
                    "error_response": { "message": "Insufficient balance" }
                })))
                .mount(server)
                .await;
        })
    });
    let uri = mock.uri.clone();

    let mut w = wallet(uri);

    let err = w
        .set_position(Units { symbol: SYMBOL.to_string(), amount: 0.03 })
        .expect_err("venue refuses the order");
    assert_eq!(err, fugazi::wallet::WalletError::Venue);

    let refused = w.take_rejections();
    assert_eq!(refused.len(), 1, "one refused order; errors: {:?}", w.errors());
    assert_eq!(refused[0].symbol, SYMBOL);
    assert_eq!(refused[0].kind, fugazi::wallet::OrderKind::Market);
    assert_eq!(refused[0].error, fugazi::wallet::WalletError::Venue);
    assert!(w.take_rejections().is_empty(), "already drained");
}

#[test]
fn a_short_target_sells_to_flat_and_reports_the_unshortable_remainder() {
    let mock = serve(|server| {
        Box::pin(async move {
            Mock::given(method("GET"))
                .and(path("/api/v3/brokerage/market/products/BTC-USD"))
                .respond_with(ResponseTemplate::new(200).set_body_json(product()))
                .mount(server)
                .await;
            Mock::given(method("GET"))
                .and(path("/api/v3/brokerage/accounts"))
                .respond_with(ResponseTemplate::new(200).set_body_json(accounts("0.03", "10000")))
                .mount(server)
                .await;
            Mock::given(method("GET"))
                .and(path("/api/v3/brokerage/orders/historical/fills"))
                .respond_with(ResponseTemplate::new(200).set_body_json(no_fills()))
                .mount(server)
                .await;
            Mock::given(method("POST"))
                .and(path("/api/v3/brokerage/orders"))
                .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                    "success": true, "success_response": { "order_id": "ORDX" }
                })))
                .mount(server)
                .await;
        })
    });
    let uri = mock.uri.clone();

    let mut w = wallet(uri);
    // Prime the balance cache: the account holds 0.03 BTC.
    w.update(SYMBOL.to_string(), Candle::new(27000.0, 27100.0, 26900.0, 27000.0, 1.0));

    // The limit is introspectable up front, so a caller can pick a long-only
    // path instead of learning it from the rejection below.
    assert!(!w.can_short(), "spot cannot hold a negative position");

    // Ask for a short (-0.02): spot can only sell down to flat.
    let ack = w
        .set_position(Units { symbol: SYMBOL.to_string(), amount: -0.02 })
        .expect("sell-to-flat accepted");
    assert!(matches!(ack, Ack::Working(_)));

    // The un-shortable remainder is reported so the strategy isn't misled.
    let refused = w.take_rejections();
    assert_eq!(refused.len(), 1, "the short remainder must be reported");
    assert_eq!(refused[0].error, fugazi::wallet::WalletError::UnsupportedOperation);
}

/// Opt-in connectivity check against Coinbase Advanced Trade **production**.
///
/// Ignored by default and gated on `COINBASE_KEY_NAME` / `COINBASE_PRIVATE_KEY`
/// (a CDP API key + its PEM). Read-only — it refreshes the account and asserts
/// the endpoint is reachable; it never places an order. Run with:
///
/// ```text
/// COINBASE_KEY_NAME=… COINBASE_PRIVATE_KEY="$(cat key.pem)" \
///   cargo test --features live --test live_coinbase -- --ignored live_account_reachable
/// ```
#[test]
#[ignore = "hits Coinbase production; needs COINBASE_KEY_NAME / COINBASE_PRIVATE_KEY"]
fn live_account_reachable() {
    let (Ok(key_name), Ok(pem)) = (
        std::env::var("COINBASE_KEY_NAME"),
        std::env::var("COINBASE_PRIVATE_KEY"),
    ) else {
        eprintln!("skipping: set COINBASE_KEY_NAME / COINBASE_PRIVATE_KEY to run");
        return;
    };

    let mut w = CoinbaseWallet::mainnet(key_name, &pem).expect("key parses");
    w.refresh_account().expect("account reachable on production");
    // A funded account reports a non-negative quote balance; the point is that
    // the signed request authenticated, not the specific number.
    assert!(w.funds().0 >= 0.0, "funds readable; errors: {:?}", w.errors());
}
