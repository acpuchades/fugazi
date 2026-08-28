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
use fugazi::types::{Symbol, symbol as intern};
use fugazi::wallet::{Reference, Side, Size, Units, Wallet};
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

/// Which provider quotes this account. Venue granularity is all a provider name
/// has room to say, and the caveat is the point of asserting it: the bars that
/// match this wallet are `okx:` fetched for the **swap** instrument id, not the
/// spot pair the same provider serves under `BTC-USDT`. Needs no mock either.
#[test]
fn a_swap_account_names_the_provider_that_quotes_it() {
    use fugazi::sources::{Okx, SeriesSource};

    assert_eq!(
        wallet("http://127.0.0.1:1".to_string()).data_sources(),
        &["okx"]
    );
    // Asserted against the provider's own `name()` rather than only the
    // literal: the point of the answer is that it can be handed straight to the
    // `sources` layer, so a rename on either side has to fail here.
    assert_eq!(
        wallet("http://127.0.0.1:1".to_string()).data_sources(),
        &[Okx::new().name()]
    );
}

/// Leverage is **read** off the venue, never assumed and never set.
///
/// An account's leverage is configured out of band — in OKX's own UI — so a
/// deployment cannot report what its fills executed at unless it asks. Two
/// paths fill the cache and this pins both: a symbol the account holds carries
/// `lever` on the positions payload, so `refresh_account` keeps it current for
/// free; a symbol the account is flat in has to be asked for explicitly, since
/// `leverage` takes `&self` and every account read here answers from cache
/// rather than blocking on a request.
#[test]
fn leverage_is_read_from_the_venue_for_held_and_flat_symbols() {
    const FLAT: &str = "ETH-USDT-SWAP";

    let held = serde_json::json!({
        "code": "0",
        "data": [{
            "instId": SYMBOL, "posSide": "net", "pos": "3", "avgPx": "27000",
            "lever": "5"
        }]
    });
    let mock = serve(move |server| {
        let held = held.clone();
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
            Mock::given(method("GET"))
                .and(path("/api/v5/account/positions"))
                .respond_with(ResponseTemplate::new(200).set_body_json(held))
                .mount(server)
                .await;
            Mock::given(method("GET"))
                .and(path("/api/v5/account/leverage-info"))
                .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                    "code": "0",
                    "data": [{ "instId": FLAT, "mgnMode": "cross", "lever": "10" }]
                })))
                .mount(server)
                .await;
        })
    });

    let mut w = wallet(mock.uri.clone());
    // Nothing asked yet: `None` is "this wallet has not been able to ask",
    // never `1x` and never "no leverage" — a swap account always has one.
    assert_eq!(w.leverage(&intern(SYMBOL)), None);

    // A held symbol comes for free, off the payload the account refresh
    // already fetches.
    w.refresh_account().expect("refresh");
    assert_eq!(w.leverage(&intern(SYMBOL)), Some(5.0));
    // ...and a flat one stays unknown until asked, rather than inheriting it.
    assert_eq!(w.leverage(&intern(FLAT)), None);

    assert_eq!(w.refresh_leverage(&intern(FLAT)).expect("leverage"), 10.0);
    assert_eq!(w.leverage(&intern(FLAT)), Some(10.0));
}

/// A venue that will not answer leaves the reading absent rather than inventing
/// one — and does not re-ask on every call.
///
/// `None` has to keep meaning "does not say" here too: a wallet that fell back
/// to `1.0` would let a deployment report a leverage its fills never executed
/// at, which is the whole failure this method exists to prevent.
#[test]
fn an_unanswered_leverage_read_stays_unknown_and_is_not_retried() {
    use std::sync::Arc;
    use std::sync::atomic::{AtomicUsize, Ordering};

    let asks = Arc::new(AtomicUsize::new(0));
    let counter = asks.clone();
    let mock = serve(move |server| {
        let counter = counter.clone();
        Box::pin(async move {
            Mock::given(method("GET"))
                .and(path("/api/v5/account/leverage-info"))
                .respond_with(move |_req: &wiremock::Request| {
                    counter.fetch_add(1, Ordering::SeqCst);
                    ResponseTemplate::new(200).set_body_json(serde_json::json!({
                        "code": "51000", "msg": "parameter instId error", "data": []
                    }))
                })
                .mount(server)
                .await;
        })
    });

    let mut w = wallet(mock.uri.clone());
    assert!(w.refresh_leverage(&intern(SYMBOL)).is_err());
    assert_eq!(w.leverage(&intern(SYMBOL)), None);
    // Cached as "asked and did not get an answer", so reading it back costs
    // nothing — a broken or unauthorised endpoint is one request, not one per
    // call from a strategy that checks each bar.
    assert_eq!(w.leverage(&intern(SYMBOL)), None);
    assert_eq!(asks.load(Ordering::SeqCst), 1);
    // Asking again is the caller's explicit choice, and does re-try.
    assert!(w.refresh_leverage(&intern(SYMBOL)).is_err());
    assert_eq!(asks.load(Ordering::SeqCst), 2);
}

/// The one real translation this backend does: OKX sizes a swap in
/// **contracts**, the trait speaks base units. `ctVal = 0.01`, so a `0.03 BTC`
/// target must reach the wire as `3.0`.
///
/// The round trip — order accepted, fill reported back in base units — is
/// covered for every venue by `market_order_round_trips_and_reports_the_fill`
/// below. What is OKX's alone, and what a shared assertion could only express
/// as an `if`, is the payload.
#[test]
fn a_market_order_sizes_in_contracts_on_the_wire() {
    use common::live::{Account, MockPlan};

    let mock = common::live::mount::<OkxWallet>(MockPlan::new(Account {
        quote: 10000.0,
        base_units: 0.0,
    }));
    let mut w = wallet(mock.uri());
    w.set_position(Units {
        symbol: intern(SYMBOL),
        amount: 0.03,
    })
    .expect("submission accepted");

    let body = sent_order(&mock);
    assert_eq!(body["instId"], SYMBOL);
    assert_eq!(body["ordType"], "market");
    assert_eq!(body["side"], "buy");
    assert_eq!(body["sz"], "3.0", "0.03 BTC at ctVal 0.01 is 3 contracts");
    assert_eq!(body["tdMode"], "cross");
    assert_eq!(
        body["clOrdId"], "fugazi0",
        "the client id correlates the later fill"
    );
}

/// A stop rests as a `conditional` algo order with the **stop** trigger field
/// set and `slOrdPx = -1` (fill at market once triggered), reduce-only.
///
/// Which of the two trigger field pairs is set is how OKX encodes the
/// direction — there is no `stop_direction` here, unlike Coinbase — so it is
/// the assertion worth pinning.
#[test]
fn a_protective_stop_posts_a_reduce_only_conditional_algo() {
    use common::live::{Account, MockPlan};

    let mock = common::live::mount::<OkxWallet>(MockPlan::new(Account {
        quote: 10000.0,
        base_units: 0.03,
    }));
    let mut w = wallet(mock.uri());
    w.update(
        intern(SYMBOL),
        Candle::new(27000.0, 27100.0, 26900.0, 27000.0, 1.0),
    );

    w.set_stop(intern(SYMBOL), Reference(26000.0), Size::units(0.03))
        .unwrap_or_else(|e| panic!("stop rested: {e:?} {:?}", w.errors()));

    let body = sent_order(&mock);
    assert_eq!(body["ordType"], "conditional");
    assert_eq!(body["reduceOnly"], "true");
    assert_eq!(body["side"], "sell", "a long's protective exit sells");
    assert_eq!(body["sz"], "3.0");
    assert_eq!(body["slTriggerPx"], "26000.0");
    assert_eq!(body["slOrdPx"], "-1", "fill at market once triggered");
    assert!(
        body.get("tpTriggerPx").is_none(),
        "a stop must not set the take-profit trigger"
    );

    // The take-profit leg is the same order with the other field pair.
    w.set_take_profit(intern(SYMBOL), Reference(28000.0), Size::units(0.03))
        .unwrap_or_else(|e| panic!("take-profit rested: {e:?} {:?}", w.errors()));
    let body = sent_order(&mock);
    assert_eq!(body["tpTriggerPx"], "28000.0");
    assert_eq!(body["tpOrdPx"], "-1");
    assert!(body.get("slTriggerPx").is_none());
}

/// A resting entry posts `ordType: limit` with the price rounded onto the
/// instrument's tick.
#[test]
fn a_limit_order_posts_its_price_on_the_instrument_tick() {
    use common::live::{Account, MockPlan};

    let mock = common::live::mount::<OkxWallet>(MockPlan::new(Account {
        quote: 10000.0,
        base_units: 0.0,
    }));
    let mut w = wallet(mock.uri());
    w.update(
        intern(SYMBOL),
        Candle::new(27000.0, 27100.0, 26900.0, 27000.0, 1.0),
    );

    // `tickSz` is 0.1, so 26000.04 rounds to 26000.0.
    w.set_limit(
        intern(SYMBOL),
        Side::Buy,
        Size::units(0.03),
        Reference(26000.04),
    )
    .unwrap_or_else(|e| panic!("limit rested: {e:?} {:?}", w.errors()));

    let body = sent_order(&mock);
    assert_eq!(body["ordType"], "limit");
    assert_eq!(body["side"], "buy");
    assert_eq!(body["sz"], "3.0");
    assert_eq!(body["px"], "26000.0", "the price lands on the 0.1 tick");
}

/// A protective leg below the instrument's **minimum** is refused locally,
/// rather than POSTed for the venue to reject.
///
/// OKX checked only that the size didn't round to *zero*, so an instrument
/// whose `minSz` exceeds its `lotSz` — a real and common shape — got a
/// well-formed order the venue was always going to bounce. Coinbase already
/// checked the minimum; this is the half of the asymmetry that moved.
///
/// It cannot live in the shared suite, because expressing it needs
/// `minSz > lotSz` and the shared fixture's grid has them equal.
#[test]
fn a_protective_leg_below_the_instrument_minimum_is_refused_locally() {
    use std::sync::Arc;
    use std::sync::atomic::{AtomicUsize, Ordering};

    // `lotSz` 0.1 but `minSz` 1.0: 0.5 contracts is a legal multiple of the
    // step and still under the minimum. The old check only caught a size that
    // rounded to *zero*, so this shape sailed through to the venue.
    let coarse = serde_json::json!({
        "code": "0",
        "data": [{
            "instId": SYMBOL,
            "lotSz": "0.1", "minSz": "1", "tickSz": "0.1", "ctVal": "0.01"
        }]
    });
    let posts = Arc::new(AtomicUsize::new(0));
    let counter = posts.clone();

    let mock = serve(move |server| {
        let (coarse, counter) = (coarse.clone(), counter.clone());
        Box::pin(async move {
            Mock::given(method("GET"))
                .and(path("/api/v5/public/instruments"))
                .respond_with(ResponseTemplate::new(200).set_body_json(coarse))
                .mount(server)
                .await;
            Mock::given(method("GET"))
                .and(path("/api/v5/account/balance"))
                .respond_with(ResponseTemplate::new(200).set_body_json(balance()))
                .mount(server)
                .await;
            Mock::given(method("GET"))
                .and(path("/api/v5/account/positions"))
                .respond_with(ResponseTemplate::new(200).set_body_json(positions("3")))
                .mount(server)
                .await;
            Mock::given(method("GET"))
                .and(path("/api/v5/trade/fills"))
                .respond_with(
                    ResponseTemplate::new(200)
                        .set_body_json(serde_json::json!({ "code": "0", "data": [] })),
                )
                .mount(server)
                .await;
            // Mounted and accepting, so a submission that *does* go out reads
            // as a success — the assertion below is on the count, not on the
            // wallet happening to error for some other reason.
            Mock::given(method("POST"))
                .and(path("/api/v5/trade/order-algo"))
                .respond_with(move |_req: &wiremock::Request| {
                    counter.fetch_add(1, Ordering::SeqCst);
                    ResponseTemplate::new(200).set_body_json(serde_json::json!({
                        "code": "0", "data": [{ "algoId": "ALGO1", "sCode": "0" }]
                    }))
                })
                .mount(server)
                .await;
        })
    });

    let mut w = wallet(mock.uri.clone());
    w.update(
        intern(SYMBOL),
        Candle::new(27000.0, 27100.0, 26900.0, 27000.0, 1.0),
    );

    // 0.005 BTC is 0.5 contracts: on the 0.1 step, under the 1.0 minimum.
    let result = w.set_stop(intern(SYMBOL), Reference(26000.0), Size::units(0.005));
    assert_eq!(
        posts.load(Ordering::SeqCst),
        0,
        "a sub-minimum protective leg must never reach the venue"
    );
    assert!(
        result.is_err(),
        "and the submission must report the refusal"
    );
    assert_eq!(
        w.take_rejections().len(),
        1,
        "and the strategy must learn it is unprotected"
    );
}

/// The body of the most recent order POST, parsed.
fn sent_order(mock: &common::live::VenueMock) -> serde_json::Value {
    let raw = mock.last_order.lock().expect("uncontended").clone();
    serde_json::from_str(&raw).unwrap_or_else(|e| panic!("order body is JSON: {e} in {raw:?}"))
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

    let symbol = intern(SYMBOL);
    let mut w = OkxWallet::demo(key, secret, passphrase);
    w.refresh_account()
        .expect("account reachable on demo trading");

    let start = w.position(&symbol).amount;
    let target = start + 0.01;
    w.set_position(Units {
        symbol: symbol.clone(),
        amount: target,
    })
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
    assert!(
        moved,
        "position did not reach target; errors: {:?}",
        w.errors()
    );

    w.set_position(Units {
        symbol: symbol.clone(),
        amount: start,
    })
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
            strategy: !buy_and_hold {{ root: {SYMBOL} }}
    "#
    );
    let spec = PortfolioSpec::from_text_with_params_in(
        &yaml,
        &Default::default(),
        std::path::Path::new("."),
        std::path::Path::new("."),
        "(live)",
    )
    .expect("parse portfolio spec");
    let mut built = spec.build(10_000.0, &Schema::empty(), None);

    let snaps: Vec<Snapshot<Symbol>> = [27_000.0, 27_100.0, 27_200.0]
        .into_iter()
        .map(|p| {
            Snapshot::single(
                intern(SYMBOL),
                Atom::new(Candle::new(p, p + 50.0, p - 50.0, p, 1.0)),
            )
        })
        .collect();

    let mut w = wallet(mock.uri.clone());
    let (report, state) = built
        .drive_resumable_with(&snaps, &mut w, None, &fugazi::backtest::Closeout::Carry)
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

// --- the shared conformance suite ------------------------------------------
//
// Everything above this line is OKX-specific: the payload shapes, the
// contracts↔units translation, the `x-simulated-trading` header. Everything
// below is behaviour every venue owes, driven from `common::live` so a fix in
// one venue is a fix in both. See that module for the split.

impl common::live::LiveVenue for OkxWallet {
    fn fixture() -> common::live::VenueFixture {
        use common::live::{Account, FillRow, VenueFixture};
        VenueFixture {
            name: "okx",
            symbol: SYMBOL,
            // `ctVal = 0.01`: the one real unit translation in this backend.
            contract_multiplier: 0.01,
            grid_path: "/api/v5/public/instruments".into(),
            fills_path: "/api/v5/trade/fills".into(),
            private_read_method: "GET",
            place_order_path: "/api/v5/trade/order".into(),
            place_protective_path: "/api/v5/trade/order-algo".into(),
            cancel_entry_path: "/api/v5/trade/cancel-order".into(),
            cancel_protective_path: "/api/v5/trade/cancel-algos".into(),
            grid_body: instruments(),
            account_bodies: Box::new(|a: Account| {
                let contracts = format!("{}", a.base_units / 0.01);
                vec![
                    (
                        "/api/v5/account/balance".into(),
                        serde_json::json!({
                            "code": "0",
                            "data": [{
                                "totalEq": a.quote.to_string(),
                                "details": [{
                                    "ccy": "USDT",
                                    "availBal": a.quote.to_string(),
                                    "eq": a.quote.to_string(),
                                }],
                            }],
                        }),
                    ),
                    (
                        "/api/v5/account/positions".into(),
                        if a.base_units == 0.0 {
                            no_positions()
                        } else {
                            positions(&contracts)
                        },
                    ),
                ]
            }),
            fills_body: Box::new(|rows: &[FillRow]| {
                let data: Vec<serde_json::Value> = rows
                    .iter()
                    .map(|r| {
                        serde_json::json!({
                            // `billId` is the monotone ordinal; the suite's
                            // `sequence` is what orders the rows.
                            "billId": r.sequence.to_string(),
                            "ordId": r.order_id,
                            "side": match r.side { Side::Buy => "buy", Side::Sell => "sell" },
                            "fillSz": r.size.to_string(),
                            "fillPx": r.price.to_string(),
                            // OKX reports the fee negative-as-charged.
                            "fee": (-r.fee).to_string(),
                        })
                    })
                    .collect();
                serde_json::json!({ "code": "0", "data": data })
            }),
            order_ok: Box::new(|id: &str| {
                serde_json::json!({
                    "code": "0",
                    "data": [{ "ordId": id, "algoId": id, "sCode": "0" }],
                })
            }),
            order_refused: serde_json::json!({
                "code": "1",
                "data": [{ "sCode": "51008", "sMsg": "insufficient balance" }],
            }),
            cancel_ok: serde_json::json!({
                "code": "0",
                "data": [{ "sCode": "0" }],
            }),
        }
    }

    fn build(base_url: String) -> Self {
        wallet(base_url)
    }

    fn error_log(&self) -> Vec<String> {
        self.errors().iter().map(|e| e.to_string()).collect()
    }

    fn sync(&mut self) {
        let _ = self.refresh_account();
    }
}

macro_rules! conformance {
    ($($name:ident),* $(,)?) => {
        $(#[test] fn $name() { common::live::$name::<OkxWallet>() })*
    };
}

conformance!(
    market_order_round_trips_and_reports_the_fill,
    a_repeated_fill_is_reported_only_once,
    partial_fills_arrive_oldest_first,
    a_venue_refusal_surfaces_through_take_rejections,
    a_non_2xx_status_is_refused_and_logged,
    a_malformed_body_is_refused_and_logged,
    a_network_failure_is_logged_not_panicked,
    a_protective_leg_dedups_an_unchanged_trigger,
    a_limit_dedups_an_unchanged_resubmit,
    cancel_by_id_withdraws_a_resting_limit,
    flatten_cancels_the_resting_orders_and_closes_the_position,
    a_non_positive_protective_trigger_is_refused_locally,
    a_protective_leg_rested_before_the_first_bar_sizes_at_its_trigger,
    a_dust_sized_target_never_reaches_the_venue,
);
