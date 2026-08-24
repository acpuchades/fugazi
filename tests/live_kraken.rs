#![cfg(feature = "live")]
//! Integration tests for the Kraken Spot live [`Wallet`].
//!
//! `KrakenWallet` owns its own `tokio` runtime and blocks on each REST call, so
//! it must be driven from a **synchronous** context — calling it from inside a
//! `#[tokio::test]` would nest runtimes and panic. These tests host the
//! `wiremock` server on a multi-threaded runtime kept alive for the test's
//! duration, then exercise the wallet on the main thread.
//!
//! The mock never verifies the signature (that path is unit-tested in
//! `src/live/kraken.rs` against Kraken's own published vector). What is asserted
//! here is the half a shared suite cannot reach: the **request bodies**, which
//! are the venue contract. Kraken differs from the other two backends in three
//! ways that a payload test is the only way to pin — every private call is a
//! signed form `POST` rather than a JSON body, one endpoint serves all four
//! order types with `ordertype` as the discriminator, and a rejection arrives
//! as HTTP 200.

mod common;

use common::live::{Account, FillRow, MockPlan, VenueFixture};
use common::net::serve;
use fugazi::Candle;
use fugazi::live::KrakenWallet;
use fugazi::types::symbol as intern;
use fugazi::wallet::{Reference, Side, Size, Units, Wallet};
use wiremock::matchers::{method, path};
use wiremock::{Mock, ResponseTemplate};

const SYMBOL: &str = "XBTUSD";
const KEY: &str = "test-api-key";
/// A throwaway secret — any valid base64 works, since the mock never verifies.
const SECRET: &str = "c2VjcmV0LXRlc3Qta2V5LWZvci11bml0LXRlc3Rpbmc=";

fn wallet(uri: String) -> KrakenWallet {
    KrakenWallet::with_base_url(uri, KEY, SECRET).expect("the secret is valid base64")
}

/// The `AssetPairs` envelope. Keyed by the pair's internal id, which is
/// deliberately *not* the requested spelling — the wallet must take the sole
/// entry rather than derive the key.
fn asset_pairs() -> serde_json::Value {
    serde_json::json!({
        "error": [],
        "result": {
            "XXBTZUSD": {
                "altname": SYMBOL,
                "wsname": "XBT/USD",
                "base": "XXBT",
                "quote": "ZUSD",
                "lot_decimals": 8,
                "pair_decimals": 2,
                "ordermin": "0",
                "tick_size": "0.01",
                "status": "online",
            }
        }
    })
}

/// A `Balance` envelope, keyed by Kraken's asset codes rather than the currency
/// names — `XXBT` and `ZUSD`, not `BTC` and `USD`.
fn balances(xbt: &str, usd: &str) -> serde_json::Value {
    serde_json::json!({
        "error": [],
        "result": { "XXBT": xbt, "ZUSD": usd }
    })
}

fn no_fills() -> serde_json::Value {
    serde_json::json!({ "error": [], "result": { "trades": {} } })
}

/// Parse a urlencoded request body into a lookup, so a payload assertion reads
/// as `field == value` rather than a substring search over the raw string.
fn form_fields(body: &str) -> std::collections::HashMap<String, String> {
    body.split('&')
        .filter_map(|kv| kv.split_once('='))
        .map(|(k, v)| {
            let decoded = v
                .replace("%2F", "/")
                .replace("%3A", ":")
                .replace("%20", " ");
            (k.to_string(), decoded)
        })
        .collect()
}

// --- venue-specific: the account shape --------------------------------------

/// Per-account rather than per-venue: Kraken quotes the same base against USD,
/// EUR, USDT and more, so the numeraire follows the caller's chosen pair leg.
/// Needs no mock — it is read back from construction.
#[test]
fn a_spot_account_reports_the_quote_currency_it_was_built_against() {
    let dead = || "http://127.0.0.1:1".to_string();
    assert_eq!(wallet(dead()).quote_ccy(), Some("USD"));
    assert_eq!(
        wallet(dead()).with_quote_ccy("EUR").quote_ccy(),
        Some("EUR")
    );
}

/// Kraken Spot as this wallet drives it is a **cash** venue, so a position is an
/// owned balance that cannot go negative. Shorting Kraken is possible, but only
/// by opting into margin per order — which this wallet never does, so claiming
/// `true` here would describe a configuration that is not in use.
#[test]
fn a_cash_spot_account_cannot_short_and_reports_no_leverage() {
    let w = wallet("http://127.0.0.1:1".to_string());
    assert!(!w.can_short());
    assert_eq!(w.leverage(&intern(SYMBOL)), None);
}

/// The name is only useful if it is the one the `sources` layer answers to.
#[test]
fn a_spot_account_names_the_provider_that_quotes_it() {
    use fugazi::sources::{Kraken, SeriesSource};

    let w = wallet("http://127.0.0.1:1".to_string());
    assert_eq!(w.data_sources(), &["kraken"]);
    assert_eq!(w.data_sources(), &[Kraken::new().name()]);
}

/// Balances key by asset code, and the base code is read off the pair rather
/// than derived — nothing in `XBTUSD` says its base balance lives under `XXBT`.
#[test]
fn a_position_reads_the_pairs_base_asset_balance() {
    let mock = serve(|server| {
        Box::pin(async move {
            Mock::given(method("GET"))
                .and(path("/0/public/AssetPairs"))
                .respond_with(ResponseTemplate::new(200).set_body_json(asset_pairs()))
                .mount(server)
                .await;
            Mock::given(method("POST"))
                .and(path("/0/private/Balance"))
                .respond_with(ResponseTemplate::new(200).set_body_json(balances("0.25", "10000")))
                .mount(server)
                .await;
            Mock::given(method("POST"))
                .and(path("/0/private/TradesHistory"))
                .respond_with(ResponseTemplate::new(200).set_body_json(no_fills()))
                .mount(server)
                .await;
        })
    });

    let sym = intern(SYMBOL);
    let mut w = wallet(mock.uri.clone());
    w.update(
        sym.clone(),
        Candle::new(27_000.0, 27_100.0, 26_900.0, 27_000.0, 1.0),
    );

    assert!(
        (w.position(&sym).amount - 0.25).abs() < 1e-12,
        "XXBT balance"
    );
    // `funds` resolves the configured `USD` through to the `ZUSD` key.
    assert!((w.funds().0 - 10_000.0).abs() < 1e-9, "ZUSD balance");
    // Equity values the held base at the last close on top of the quote leg.
    assert!(
        (w.equity().0 - (10_000.0 + 0.25 * 27_000.0)).abs() < 1e-6,
        "got {}",
        w.equity().0
    );
    assert_eq!(w.positions().len(), 1);
}

// --- venue-specific: the request payloads ------------------------------------

/// A market order carries **no `price`**. Kraken rejects a market order that
/// sends one rather than ignoring it, so an extra field here is a live failure.
#[test]
fn a_market_order_posts_a_signed_form_with_no_price() {
    let mock = common::live::mount::<KrakenWallet>(MockPlan::new(Account {
        quote: 10_000.0,
        base_units: 0.0,
    }));
    let mut w = wallet(mock.uri());

    w.set_position(Units {
        symbol: intern(SYMBOL),
        amount: 0.5,
    })
    .expect("submission accepted");

    let body = mock.last_order();
    let f = form_fields(&body);
    assert_eq!(f.get("pair").map(String::as_str), Some(SYMBOL));
    assert_eq!(f.get("ordertype").map(String::as_str), Some("market"));
    assert_eq!(f.get("type").map(String::as_str), Some("buy"));
    assert_eq!(f.get("volume").map(String::as_str), Some("0.50000000"));
    assert!(
        !f.contains_key("price"),
        "a market order must not carry a price: {body}"
    );
    // The nonce rides in the body, not a header, and is what the signature
    // prehashes.
    assert!(
        f.contains_key("nonce"),
        "every private call carries a nonce"
    );
    assert!(
        f.get("cl_ord_id").is_some_and(|s| s.starts_with("fugazi")),
        "the order is tagged so a later fill poll can correlate: {body}"
    );
}

/// One endpoint serves every order type, so `ordertype` is the only thing
/// separating a resting entry from a market one.
#[test]
fn a_limit_order_posts_its_price_as_the_limit() {
    let mock = common::live::mount::<KrakenWallet>(MockPlan::new(Account {
        quote: 10_000.0,
        base_units: 0.0,
    }));
    let mut w = wallet(mock.uri());

    w.set_limit(
        intern(SYMBOL),
        Side::Buy,
        Size::Units(0.25),
        Reference(26_500.0),
    )
    .expect("submission accepted");

    let f = form_fields(&mock.last_order());
    assert_eq!(f.get("ordertype").map(String::as_str), Some("limit"));
    assert_eq!(f.get("price").map(String::as_str), Some("26500.00"));
    assert_eq!(f.get("volume").map(String::as_str), Some("0.25000000"));
}

/// **The direction rides in the order type**, and `price` is the *trigger*, not
/// a limit — the opposite reading would rest the exit at a price the market has
/// already left. `price2` is deliberately absent, so a triggered leg is
/// marketable.
#[test]
fn protective_legs_post_their_trigger_as_price_and_their_direction_as_ordertype() {
    for (kind, expected) in [("stop", "stop-loss"), ("take_profit", "take-profit")] {
        let mock = common::live::mount::<KrakenWallet>(MockPlan::new(Account {
            quote: 10_000.0,
            base_units: 1.0,
        }));
        let mut w = wallet(mock.uri());
        let sym = intern(SYMBOL);
        // Feed a bar so the leg can size against the held balance.
        w.update(
            sym.clone(),
            Candle::new(27_000.0, 27_100.0, 26_900.0, 27_000.0, 1.0),
        );

        let trigger = if kind == "stop" { 26_000.0 } else { 28_000.0 };
        if kind == "stop" {
            w.set_stop(sym.clone(), Reference(trigger), Size::PositionFraction(1.0))
        } else {
            w.set_take_profit(sym.clone(), Reference(trigger), Size::PositionFraction(1.0))
        }
        .expect("submission accepted");

        let body = mock.last_order();
        let f = form_fields(&body);
        assert_eq!(
            f.get("ordertype").map(String::as_str),
            Some(expected),
            "{kind}: {body}"
        );
        // A spot account exits only by selling what it holds — both legs are
        // sells, which is why the type has to carry the direction.
        assert_eq!(f.get("type").map(String::as_str), Some("sell"), "{body}");
        assert_eq!(
            f.get("price").map(String::as_str),
            Some(format!("{trigger:.2}").as_str()),
            "{kind}: price is the trigger: {body}"
        );
        assert!(
            !f.contains_key("price2"),
            "{kind}: no limit leg, so a triggered exit is marketable: {body}"
        );
    }
}

/// Every private call authenticates in **headers** while the nonce rides in the
/// signed body — the split that makes Kraken's scheme different from OKX's
/// (everything in headers) and Coinbase's (a bearer token).
///
/// Uses a local mock rather than the shared one because header capture is a
/// Kraken-specific assertion; teaching the shared harness to record them would
/// be scaffolding only one venue reads.
#[test]
fn a_private_call_sends_the_api_key_and_signature_headers() {
    use std::sync::{Arc, Mutex};

    let seen: Arc<Mutex<Vec<(String, String)>>> = Arc::new(Mutex::new(Vec::new()));
    let recorder = Arc::clone(&seen);
    let mock = serve(move |server| {
        let recorder = Arc::clone(&recorder);
        Box::pin(async move {
            // `refresh_account` loads the pair table before reading balances.
            Mock::given(method("GET"))
                .and(path("/0/public/AssetPairs"))
                .respond_with(ResponseTemplate::new(200).set_body_json(asset_pairs()))
                .mount(server)
                .await;
            Mock::given(method("POST"))
                .and(path("/0/private/Balance"))
                .respond_with(move |req: &wiremock::Request| {
                    let header = |name: &str| {
                        req.headers
                            .get(name)
                            .and_then(|v| v.to_str().ok())
                            .unwrap_or_default()
                            .to_string()
                    };
                    recorder
                        .lock()
                        .expect("uncontended")
                        .push((header("API-Key"), header("API-Sign")));
                    ResponseTemplate::new(200).set_body_json(balances("0", "10000"))
                })
                .mount(server)
                .await;
        })
    });

    let mut w = wallet(mock.uri.clone());
    w.refresh_account().expect("the balance read succeeds");

    let calls = seen.lock().expect("uncontended");
    let (key, sign) = calls.first().expect("a private call was made");
    assert_eq!(key, KEY);
    // The signature is base64 of a 64-byte HMAC-SHA512 digest — the length is
    // what catches a digest built with the wrong hash.
    use base64::Engine as _;
    let raw = base64::engine::general_purpose::STANDARD
        .decode(sign)
        .expect("API-Sign is base64");
    assert_eq!(raw.len(), 64, "HMAC-SHA512 is 64 bytes");
}

/// A cash account cannot hold a short. The wallet sells down to flat and books
/// the remainder as a rejection, so the strategy learns its short didn't take
/// rather than silently believing it did.
#[test]
fn a_short_target_sells_to_flat_and_reports_the_unshortable_remainder() {
    let mock = common::live::mount::<KrakenWallet>(MockPlan::new(Account {
        quote: 10_000.0,
        base_units: 0.03,
    }));
    let mut w = wallet(mock.uri());
    let sym = intern(SYMBOL);
    w.update(
        sym.clone(),
        Candle::new(27_000.0, 27_100.0, 26_900.0, 27_000.0, 1.0),
    );

    w.set_position(Units {
        symbol: sym.clone(),
        amount: -0.01,
    })
    .expect("the sell-to-flat leg is accepted");

    let f = form_fields(&mock.last_order());
    assert_eq!(f.get("type").map(String::as_str), Some("sell"));
    assert_eq!(
        f.get("volume").map(String::as_str),
        Some("0.03000000"),
        "sells the whole held balance, not the signed difference"
    );

    let rejections = w.take_rejections();
    assert_eq!(
        rejections.len(),
        1,
        "the un-shortable remainder is reported; errors: {:?}",
        w.errors().iter().map(|e| e.to_string()).collect::<Vec<_>>()
    );
}

/// The failure mode that separates Kraken from a status-code venue: the order
/// was refused, and the transport said `200 OK`.
#[test]
fn an_http_200_carrying_an_error_array_is_a_refusal() {
    let mock = common::live::mount::<KrakenWallet>(
        MockPlan::new(Account {
            quote: 10_000.0,
            base_units: 0.0,
        })
        .with_order(common::live::OrderOutcome::Refused),
    );
    let mut w = wallet(mock.uri());

    let err = w
        .set_position(Units {
            symbol: intern(SYMBOL),
            amount: 0.5,
        })
        .expect_err("a populated `error` array is a refusal");
    assert!(matches!(err, fugazi::wallet::WalletError::Venue), "{err:?}");
    assert!(
        w.errors().iter().any(|e| e.to_string().contains("200")),
        "the detail records the 200-with-rejection: {:?}",
        w.errors().iter().map(|e| e.to_string()).collect::<Vec<_>>()
    );
}

// --- the shared conformance suite ------------------------------------------
//
// Everything above this line is Kraken-specific: the signed form bodies, the
// asset-code resolution, the one-endpoint-many-ordertypes shape. Everything
// below is behaviour every venue owes, driven from `common::live` so a fix in
// one venue is a fix in all three.

impl common::live::LiveVenue for KrakenWallet {
    fn fixture() -> VenueFixture {
        VenueFixture {
            name: "kraken",
            symbol: SYMBOL,
            // Spot: a venue size unit *is* a base unit.
            contract_multiplier: 1.0,
            grid_path: "/0/public/AssetPairs".into(),
            fills_path: "/0/private/TradesHistory".into(),
            // Kraken signs a form body, so even a pure read is a POST.
            private_read_method: "POST",
            place_order_path: "/0/private/AddOrder".into(),
            // One order endpoint, so the protective leg shares it.
            place_protective_path: "/0/private/AddOrder".into(),
            cancel_entry_path: "/0/private/CancelOrder".into(),
            cancel_protective_path: "/0/private/CancelOrder".into(),
            grid_body: asset_pairs(),
            account_bodies: Box::new(|a: Account| {
                vec![(
                    "/0/private/Balance".to_string(),
                    balances(&a.base_units.to_string(), &a.quote.to_string()),
                )]
            }),
            fills_body: Box::new(|rows: &[FillRow]| {
                let trades: serde_json::Map<String, serde_json::Value> = rows
                    .iter()
                    .map(|r| {
                        (
                            r.id.to_string(),
                            serde_json::json!({
                                "ordertxid": r.order_id,
                                "pair": "XXBTZUSD",
                                "time": 1688667796.88_f64,
                                // The monotone integer the watermark rides on.
                                "trade_id": r.sequence as i64,
                                "type": match r.side { Side::Buy => "buy", Side::Sell => "sell" },
                                "ordertype": "market",
                                "price": r.price.to_string(),
                                "vol": r.size.to_string(),
                                "fee": r.fee.to_string(),
                                "margin": "0.0",
                                "maker": false,
                            }),
                        )
                    })
                    .collect();
                serde_json::json!({ "error": [], "result": { "trades": trades } })
            }),
            order_ok: Box::new(|id: &str| {
                serde_json::json!({
                    "error": [],
                    "result": { "descr": { "order": "buy 1.0 XBTUSD @ market" }, "txid": [id] },
                })
            }),
            // HTTP 200, and refused.
            order_refused: serde_json::json!({
                "error": ["EOrder:Insufficient funds"],
                "result": {},
            }),
            cancel_ok: serde_json::json!({ "error": [], "result": { "count": 1 } }),
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
        $(#[test] fn $name() { common::live::$name::<KrakenWallet>() })*
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
);
