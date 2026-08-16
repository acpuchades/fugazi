#![cfg(feature = "live")]
//! Integration tests for the OKX V5 perpetual-swap live [`Wallet`].
//!
//! `OkxWallet` owns its own `tokio` runtime and blocks on each REST call, so it
//! must be driven from a **synchronous** context — calling it from inside a
//! `#[tokio::test]` would nest runtimes and panic. These tests instead host the
//! `wiremock` server on a multi-threaded runtime kept alive for the test's
//! duration (its worker threads keep serving after `block_on` returns), then
//! exercise the wallet on the main thread, outside any runtime context.
//!
//! They also pin the one real translation this wallet does: OKX sizes a swap in
//! **contracts** (`ctVal = 0.01 BTC` here), while the trait speaks base units —
//! so a `0.03 BTC` target becomes `3` contracts on the wire, and a `3`-contract
//! fill comes back as `0.03` units.

mod common;

use common::net::serve;
use fugazi::Candle;
use fugazi::live::OkxWallet;
use fugazi::wallet::{Ack, Reference, Side, Size, Units, Wallet};
use wiremock::matchers::{method, path};
use wiremock::{Mock, ResponseTemplate};

const SYMBOL: &str = "BTC-USDT-SWAP";

/// One swap whose contract is worth `0.01 BTC` — the contracts↔units factor.
fn instruments() -> serde_json::Value {
    serde_json::json!({
        "code": "0",
        "data": [{
            "instId": SYMBOL,
            "lotSz": "0.1", "minSz": "0.1", "tickSz": "0.1", "ctVal": "0.01"
        }]
    })
}

fn balance() -> serde_json::Value {
    serde_json::json!({
        "code": "0",
        "data": [{
            "totalEq": "10000",
            "details": [{ "ccy": "USDT", "availBal": "10000", "eq": "10000" }]
        }]
    })
}

/// A positions envelope reporting `contracts` contracts of the swap (net mode).
fn positions(contracts: &str) -> serde_json::Value {
    serde_json::json!({
        "code": "0",
        "data": [{ "instId": SYMBOL, "posSide": "net", "pos": contracts, "avgPx": "27000" }]
    })
}

fn no_positions() -> serde_json::Value {
    serde_json::json!({ "code": "0", "data": [] })
}


fn wallet(uri: String) -> OkxWallet {
    OkxWallet::with_base_url(uri, "key", "secret", "pass")
}

/// Swaps in net position mode carry one signed position, so a short is an
/// ordinary target — the opposite of the spot `CoinbaseWallet`, which reports
/// `false` and clamps. Needs no mock: it's a statement about the venue.
#[test]
fn a_swap_account_reports_that_it_can_short() {
    assert!(wallet("http://127.0.0.1:1".to_string()).can_short());
}

/// The margin currency a linear USDⓈ-M swap settles in — fixed by the instrument
/// type rather than read off the account, so this needs no mock either. It is
/// what `funds` (the `USDT` row's `availBal`) is denominated in; `equity` is
/// OKX's own USD valuation, which is the one asymmetry a caller has to know.
#[test]
fn a_swap_account_reports_the_currency_its_funds_are_in() {
    assert_eq!(
        wallet("http://127.0.0.1:1".to_string()).quote_ccy(),
        Some("USDT")
    );
}

#[test]
fn set_position_places_a_market_order_and_update_reports_the_fill_in_base_units() {
    use std::sync::Arc;
    use std::sync::atomic::{AtomicUsize, Ordering};

    // The fills endpoint is stateful: the first poll (the cursor seed during
    // submission) is empty, and every later poll returns the fill. One
    // responder makes this deterministic regardless of mock-overlap ordering.
    let fill_calls = Arc::new(AtomicUsize::new(0));
    let counter = fill_calls.clone();

    let mock = serve(move |server| {
        let counter = counter.clone();
        Box::pin(async move {
            Mock::given(method("GET"))
                .and(path("/api/v5/public/instruments"))
                .respond_with(ResponseTemplate::new(200).set_body_json(instruments()))
                .mount(server)
                .await;
            Mock::given(method("GET"))
                .and(path("/api/v5/account/balance"))
                .respond_with(ResponseTemplate::new(200).set_body_json(balance()))
                .mount(server)
                .await;
            // The account reports the filled position (3 contracts = 0.03 BTC).
            Mock::given(method("GET"))
                .and(path("/api/v5/account/positions"))
                .respond_with(ResponseTemplate::new(200).set_body_json(positions("3")))
                .mount(server)
                .await;
            // First poll (the cursor seed) is empty; later polls return the fill.
            Mock::given(method("GET"))
                .and(path("/api/v5/trade/fills"))
                .respond_with(move |_req: &wiremock::Request| {
                    let n = counter.fetch_add(1, Ordering::SeqCst);
                    let data = if n == 0 {
                        serde_json::json!([])
                    } else {
                        serde_json::json!([{
                            "billId": "88", "ordId": "ORD1", "side": "buy",
                            "fillSz": "3", "fillPx": "27000", "fee": "-0.08"
                        }])
                    };
                    ResponseTemplate::new(200)
                        .set_body_json(serde_json::json!({ "code": "0", "data": data }))
                })
                .mount(server)
                .await;
            Mock::given(method("POST"))
                .and(path("/api/v5/trade/order"))
                .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                    "code": "0", "data": [{ "ordId": "ORD1", "clOrdId": "fugazi0", "sCode": "0" }]
                })))
                .mount(server)
                .await;
        })
    });
    let uri = mock.uri.clone();

    let mut w = wallet(uri);

    // Target 0.03 BTC — the venue should see 3 contracts.
    let ack = w
        .set_position(Units { symbol: SYMBOL.to_string(), amount: 0.03 })
        .expect("submission accepted");
    assert!(matches!(ack, Ack::Working(_)), "market order returns Working");

    // Next bar: account refresh shows the position, poll returns the fill.
    let fills = w.update(SYMBOL.to_string(), Candle::new(27000.0, 27100.0, 26900.0, 27050.0, 1.0));
    assert_eq!(fills.len(), 1, "expected one fill; errors: {:?}", w.errors());
    let fill = &fills[0];
    assert_eq!(fill.side, Side::Buy);
    assert!((fill.units - 0.03).abs() < 1e-9, "3 contracts -> 0.03 BTC, got {}", fill.units);
    assert!((fill.price - 27000.0).abs() < 1e-9);
    assert!((fill.commission - 0.08).abs() < 1e-9);

    // Reads reflect the refreshed account state (contracts converted to units).
    assert!((w.position(&SYMBOL.to_string()).amount - 0.03).abs() < 1e-9);
    assert!((w.funds().0 - 10000.0).abs() < 1e-9);
    assert!((w.equity().0 - 10000.0).abs() < 1e-9);
    assert!((w.price(&SYMBOL.to_string()).unwrap().0 - 27050.0).abs() < 1e-9);

    // Polling again is idempotent: the cursor advanced past the fill.
    assert!(w.poll_fills().is_empty(), "fill must not be re-reported");
}

#[test]
fn a_protective_stop_dedups_an_unchanged_trigger() {
    use std::sync::Arc;
    use std::sync::atomic::{AtomicUsize, Ordering};

    let algo_posts = Arc::new(AtomicUsize::new(0));
    let counter = algo_posts.clone();

    let mock = serve(move |server| {
        let counter = counter.clone();
        Box::pin(async move {
            Mock::given(method("GET"))
                .and(path("/api/v5/public/instruments"))
                .respond_with(ResponseTemplate::new(200).set_body_json(instruments()))
                .mount(server)
                .await;
            Mock::given(method("GET"))
                .and(path("/api/v5/account/balance"))
                .respond_with(ResponseTemplate::new(200).set_body_json(balance()))
                .mount(server)
                .await;
            // A held long (3 contracts), so a stop rests on the SELL side.
            Mock::given(method("GET"))
                .and(path("/api/v5/account/positions"))
                .respond_with(ResponseTemplate::new(200).set_body_json(positions("3")))
                .mount(server)
                .await;
            Mock::given(method("GET"))
                .and(path("/api/v5/trade/fills"))
                .respond_with(ResponseTemplate::new(200).set_body_json(no_positions()))
                .mount(server)
                .await;
            // Count algo submissions; each returns a distinct algoId.
            Mock::given(method("POST"))
                .and(path("/api/v5/trade/order-algo"))
                .respond_with(move |_req: &wiremock::Request| {
                    let n = counter.fetch_add(1, Ordering::SeqCst);
                    ResponseTemplate::new(200).set_body_json(serde_json::json!({
                        "code": "0", "data": [{ "algoId": format!("ALGO{n}"), "sCode": "0" }]
                    }))
                })
                .mount(server)
                .await;
            // The cancel for the moved-trigger replace.
            Mock::given(method("POST"))
                .and(path("/api/v5/trade/cancel-algos"))
                .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                    "code": "0", "data": [{ "algoId": "ALGO0", "sCode": "0" }]
                })))
                .mount(server)
                .await;
        })
    });
    let uri = mock.uri.clone();

    let mut w = wallet(uri);
    // Prime the position cache (account refresh) so the stop knows the side.
    w.update(SYMBOL.to_string(), Candle::new(27000.0, 27100.0, 26900.0, 27000.0, 1.0));

    // Rest the same stop three bars running — only the first should hit the venue.
    for _ in 0..3 {
        w.set_stop(SYMBOL.to_string(), Reference(26000.0), Size::position_frac(1.0))
            .expect("stop accepted");
    }
    assert_eq!(
        algo_posts.load(Ordering::SeqCst),
        1,
        "an unchanged stop trigger must not re-submit each bar"
    );

    // Moving the trigger cancels + replaces: one more algo POST.
    w.set_stop(SYMBOL.to_string(), Reference(26500.0), Size::position_frac(1.0))
        .expect("moved stop accepted");
    assert_eq!(algo_posts.load(Ordering::SeqCst), 2, "a moved trigger re-submits");
}

#[test]
fn a_limit_order_places_a_limit_and_dedups_an_unchanged_resubmit() {
    use std::sync::Arc;
    use std::sync::Mutex;
    use std::sync::atomic::{AtomicUsize, Ordering};

    let posts = Arc::new(AtomicUsize::new(0));
    let cancels = Arc::new(AtomicUsize::new(0));
    // OKX carries the order payload in the JSON *body*; capture the last one so
    // we can assert the venue really got a limit rather than something that
    // merely returned a 200.
    let last_body = Arc::new(Mutex::new(String::new()));
    let (c_posts, c_cancels, c_body) = (posts.clone(), cancels.clone(), last_body.clone());

    let mock = serve(move |server| {
        let (c_posts, c_cancels, c_body) = (c_posts.clone(), c_cancels.clone(), c_body.clone());
        Box::pin(async move {
            Mock::given(method("GET"))
                .and(path("/api/v5/public/instruments"))
                .respond_with(ResponseTemplate::new(200).set_body_json(instruments()))
                .mount(server)
                .await;
            Mock::given(method("GET"))
                .and(path("/api/v5/account/balance"))
                .respond_with(ResponseTemplate::new(200).set_body_json(balance()))
                .mount(server)
                .await;
            // Flat, so a buy target is a plain buy of the whole size.
            Mock::given(method("GET"))
                .and(path("/api/v5/account/positions"))
                .respond_with(ResponseTemplate::new(200).set_body_json(no_positions()))
                .mount(server)
                .await;
            Mock::given(method("GET"))
                .and(path("/api/v5/trade/fills"))
                .respond_with(ResponseTemplate::new(200).set_body_json(no_positions()))
                .mount(server)
                .await;
            Mock::given(method("POST"))
                .and(path("/api/v5/trade/order"))
                .respond_with(move |req: &wiremock::Request| {
                    *c_body.lock().unwrap() = String::from_utf8_lossy(&req.body).to_string();
                    let n = c_posts.fetch_add(1, Ordering::SeqCst);
                    ResponseTemplate::new(200).set_body_json(serde_json::json!({
                        "code": "0", "data": [{ "ordId": format!("ORD{n}"), "sCode": "0" }]
                    }))
                })
                .mount(server)
                .await;
            Mock::given(method("POST"))
                .and(path("/api/v5/trade/cancel-order"))
                .respond_with(move |_req: &wiremock::Request| {
                    c_cancels.fetch_add(1, Ordering::SeqCst);
                    ResponseTemplate::new(200).set_body_json(serde_json::json!({
                        "code": "0", "data": [{ "ordId": "ORD0", "sCode": "0" }]
                    }))
                })
                .mount(server)
                .await;
        })
    });
    let uri = mock.uri.clone();

    let mut w = wallet(uri);
    w.update(SYMBOL.to_string(), Candle::new(27000.0, 27100.0, 26900.0, 27000.0, 1.0));

    // 0.05 BTC at 26000 → 5 contracts.
    w.set_limit(SYMBOL.to_string(), Side::Buy, Size::units(0.05), Reference(26000.0))
        .expect("limit accepted");

    let body = last_body.lock().unwrap().clone();
    assert!(body.contains("\"ordType\":\"limit\""), "not a limit order: {body}");
    assert!(body.contains("\"side\":\"buy\""), "wrong side: {body}");
    assert!(body.contains("\"px\":\"26000.0\""), "limit price not sent: {body}");
    assert!(body.contains("\"sz\":\"5.0\""), "size not in contracts: {body}");

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
                .and(path("/api/v5/public/instruments"))
                .respond_with(ResponseTemplate::new(200).set_body_json(instruments()))
                .mount(server)
                .await;
            Mock::given(method("GET"))
                .and(path("/api/v5/trade/fills"))
                .respond_with(ResponseTemplate::new(200).set_body_json(no_positions()))
                .mount(server)
                .await;
            // OKX returns HTTP 200 with a non-zero sCode for a business rejection.
            Mock::given(method("POST"))
                .and(path("/api/v5/trade/order"))
                .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                    "code": "1",
                    "data": [{ "ordId": "", "sCode": "51008", "sMsg": "Insufficient balance." }]
                })))
                .mount(server)
                .await;
        })
    });
    let uri = mock.uri.clone();

    let mut w = wallet(uri);

    // The submission fails synchronously with the Venue category …
    let err = w
        .set_position(Units { symbol: SYMBOL.to_string(), amount: 0.03 })
        .expect_err("venue refuses the order");
    assert_eq!(err, fugazi::wallet::WalletError::Venue);

    // … and — the point of this test — the refusal is drained through the
    // failure stream so a driver can route it to `Strategy::on_reject`.
    let refused = w.take_rejections();
    assert_eq!(refused.len(), 1, "one refused order; errors: {:?}", w.errors());
    assert_eq!(refused[0].symbol, SYMBOL);
    assert_eq!(refused[0].kind, fugazi::wallet::OrderKind::Market);
    assert_eq!(refused[0].error, fugazi::wallet::WalletError::Venue);
    // Draining is destructive: a second call yields nothing.
    assert!(w.take_rejections().is_empty(), "already drained");
}

/// Opt-in end-to-end test against OKX **demo trading**.
///
/// Ignored by default and additionally gated on `OKX_DEMO_KEY` /
/// `OKX_DEMO_SECRET` / `OKX_DEMO_PASSPHRASE` (create a demo-trading API key in
/// your OKX account). Run with:
///
/// ```text
/// OKX_DEMO_KEY=… OKX_DEMO_SECRET=… OKX_DEMO_PASSPHRASE=… \
///   cargo test --features live --test live_okx -- --ignored live_demo_round_trip
/// ```
///
/// Places a tiny `BTC-USDT-SWAP` market order, polls the fill, asserts the
/// position moved, then flattens — leaving the demo account as it started.
#[test]
#[ignore = "hits OKX demo trading; needs OKX_DEMO_{KEY,SECRET,PASSPHRASE}"]
fn live_demo_round_trip() {
    let (Ok(key), Ok(secret), Ok(passphrase)) = (
        std::env::var("OKX_DEMO_KEY"),
        std::env::var("OKX_DEMO_SECRET"),
        std::env::var("OKX_DEMO_PASSPHRASE"),
    ) else {
        eprintln!("skipping: set OKX_DEMO_KEY / OKX_DEMO_SECRET / OKX_DEMO_PASSPHRASE to run");
        return;
    };

    let symbol = SYMBOL.to_string();
    let mut w = OkxWallet::demo(key, secret, passphrase);
    w.refresh_account().expect("account reachable on demo trading");

    let start = w.position(&symbol).amount;
    let target = start + 0.01;
    w.set_position(Units { symbol: symbol.clone(), amount: target })
        .expect("market order accepted");

    let mut moved = false;
    for _ in 0..10 {
        std::thread::sleep(std::time::Duration::from_millis(500));
        let _ = w.update(symbol.clone(), Candle::new(0.0, 0.0, 0.0, 0.0, 0.0));
        if (w.position(&symbol).amount - target).abs() < 1e-6 {
            moved = true;
            break;
        }
    }
    assert!(moved, "position did not reach target; errors: {:?}", w.errors());

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

/// A **strategy spec** — a portfolio, the shape that was hardest to reach —
/// driven straight against the live wallet.
///
/// This is the whole point of [`RunnableStrategyExt::drive_resumable_with`]:
/// `RunnableStrategy` is object-safe, so `drive_resumable` can only ever build
/// its own `PaperWallet`. Before the extension trait there was no way to run a
/// spec against a venue at all, portfolio least of all, which blocked
/// broker-funded deployment outright.
///
/// Two things are asserted: an order actually reaches the venue, and the
/// captured `RunState` carries `wallet: null` — a live account's positions and
/// cash belong to the broker, so a stale local snapshot must never be replayed
/// over them on resume.
#[test]
#[cfg(feature = "spec")]
fn a_portfolio_spec_runs_against_a_live_wallet() {
    use fugazi::market::Schema;
    use fugazi::spec::{PortfolioSpec, RunnableStrategyExt};
    use fugazi::types::{Atom, Snapshot};

    let orders = std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0));
    let counter = orders.clone();
    let mock = serve(move |server| {
        let counter = counter.clone();
        Box::pin(async move {
            for (p, body) in [
                ("/api/v5/public/instruments", instruments()),
                ("/api/v5/account/balance", balance()),
                ("/api/v5/account/positions", no_positions()),
                (
                    "/api/v5/trade/fills",
                    serde_json::json!({ "code": "0", "data": [] }),
                ),
            ] {
                Mock::given(method("GET"))
                    .and(path(p))
                    .respond_with(ResponseTemplate::new(200).set_body_json(body))
                    .mount(server)
                    .await;
            }
            Mock::given(method("POST"))
                .and(path("/api/v5/trade/order"))
                .respond_with(move |_req: &wiremock::Request| {
                    counter.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                    ResponseTemplate::new(200).set_body_json(serde_json::json!({
                        "code": "0",
                        "data": [{ "ordId": "ORD1", "clOrdId": "fugazi0", "sCode": "0" }]
                    }))
                })
                .mount(server)
                .await;
        })
    });

    let yaml = format!(
        r#"
        children:
          - name: hold
            strategy: !buy_and_hold {{ symbol: {SYMBOL} }}
    "#
    );
    let spec = PortfolioSpec::from_text_with_params_in(
        &yaml,
        &Default::default(),
        std::path::Path::new("."),
        "(live)",
    )
    .expect("parse portfolio spec");
    let mut built = spec.build(10_000.0, &Schema::empty(), None);

    let snaps: Vec<Snapshot<String>> = [27_000.0, 27_100.0, 27_200.0]
        .into_iter()
        .map(|p| {
            Snapshot::single(
                SYMBOL.to_string(),
                Atom::new(Candle::new(p, p + 50.0, p - 50.0, p, 1.0)),
            )
        })
        .collect();

    let mut w = wallet(mock.uri.clone());
    let (report, state) = built
        .drive_resumable_with(&snaps, &mut w, None, false)
        .expect("live spec run");

    assert_eq!(report.equity_curve.len(), snaps.len());
    assert!(
        orders.load(std::sync::atomic::Ordering::SeqCst) > 0,
        "the portfolio's child should have reached the venue; errors: {:?}",
        w.errors()
    );
    assert_eq!(state.kind, "portfolio");
    assert!(
        state.wallet.is_null(),
        "a live account's book belongs to the venue, not the state file: {}",
        state.wallet
    );
    assert!(
        !state.strategy.is_null(),
        "the strategy's own state must still be captured"
    );
}
