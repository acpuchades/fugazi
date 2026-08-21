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
use fugazi::types::symbol as intern;
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
    w.update(
        intern(SYMBOL),
        Candle::new(27000.0, 27100.0, 26900.0, 27000.0, 1.0),
    );

    // The limit is introspectable up front, so a caller can pick a long-only
    // path instead of learning it from the rejection below.
    assert!(!w.can_short(), "spot cannot hold a negative position");

    // Ask for a short (-0.02): spot can only sell down to flat.
    let ack = w
        .set_position(Units {
            symbol: intern(SYMBOL),
            amount: -0.02,
        })
        .expect("sell-to-flat accepted");
    assert!(matches!(ack, Ack::Working(_)));

    // The un-shortable remainder is reported so the strategy isn't misled.
    let refused = w.take_rejections();
    assert_eq!(refused.len(), 1, "the short remainder must be reported");
    assert_eq!(
        refused[0].error,
        fugazi::wallet::WalletError::UnsupportedOperation
    );
}

/// A market order posts a `market_market_ioc` configuration sized in base
/// units — spot has no contract wrapper, so the number on the wire is the
/// number the strategy asked for.
///
/// The round trip is covered for every venue by
/// `market_order_round_trips_and_reports_the_fill` below. The payload is
/// Coinbase's alone.
#[test]
fn a_market_order_posts_a_market_ioc_configuration() {
    use common::live::{Account, MockPlan};

    let mock = common::live::mount::<CoinbaseWallet>(MockPlan::new(Account {
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
    assert_eq!(body["product_id"], SYMBOL);
    assert_eq!(body["side"], "BUY");
    assert_eq!(body["client_order_id"], "fugazi0");
    assert_eq!(
        body["order_configuration"]["market_market_ioc"]["base_size"], "0.03000000",
        "sized in base units, at the product's 8-decimal increment"
    );
}

/// A protective leg posts a reduce-only `stop_limit_stop_limit_gtc` **sell**
/// with `limit_price == stop_price`, and encodes the direction in
/// `stop_direction` — where OKX encodes it by which trigger field it sets.
///
/// A spot account can only exit by selling what it holds, so both legs are
/// sells; the direction is the only thing separating a stop from a
/// take-profit.
#[test]
fn a_protective_leg_posts_a_stop_limit_with_its_direction() {
    use common::live::{Account, MockPlan};

    let mock = common::live::mount::<CoinbaseWallet>(MockPlan::new(Account {
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
    let cfg = &body["order_configuration"]["stop_limit_stop_limit_gtc"];
    assert_eq!(body["side"], "SELL", "spot exits by selling what it holds");
    assert_eq!(cfg["base_size"], "0.03000000");
    assert_eq!(cfg["stop_price"], "26000.00");
    assert_eq!(
        cfg["limit_price"], cfg["stop_price"],
        "limit == stop keeps the triggered order marketable"
    );
    assert_eq!(cfg["stop_direction"], "STOP_DIRECTION_STOP_DOWN");

    // A take-profit is the same order pointing the other way.
    w.set_take_profit(intern(SYMBOL), Reference(28000.0), Size::units(0.03))
        .unwrap_or_else(|e| panic!("take-profit rested: {e:?} {:?}", w.errors()));
    let body = sent_order(&mock);
    let cfg = &body["order_configuration"]["stop_limit_stop_limit_gtc"];
    assert_eq!(body["side"], "SELL");
    assert_eq!(cfg["stop_price"], "28000.00");
    assert_eq!(cfg["stop_direction"], "STOP_DIRECTION_STOP_UP");
}

/// A resting entry posts a `limit_limit_gtc` configuration with the price
/// rounded onto the product's quote increment.
#[test]
fn a_limit_order_posts_a_limit_gtc_configuration() {
    use common::live::{Account, MockPlan};

    let mock = common::live::mount::<CoinbaseWallet>(MockPlan::new(Account {
        quote: 10000.0,
        base_units: 0.0,
    }));
    let mut w = wallet(mock.uri());
    w.update(
        intern(SYMBOL),
        Candle::new(27000.0, 27100.0, 26900.0, 27000.0, 1.0),
    );

    // `quote_increment` is 0.01, so 26000.004 rounds to 26000.00.
    w.set_limit(
        intern(SYMBOL),
        Side::Buy,
        Size::units(0.03),
        Reference(26000.004),
    )
    .unwrap_or_else(|e| panic!("limit rested: {e:?} {:?}", w.errors()));

    let body = sent_order(&mock);
    let cfg = &body["order_configuration"]["limit_limit_gtc"];
    assert_eq!(body["side"], "BUY");
    assert_eq!(cfg["base_size"], "0.03000000");
    assert_eq!(cfg["limit_price"], "26000.00");
}

/// The body of the most recent order POST, parsed.
fn sent_order(mock: &common::live::VenueMock) -> serde_json::Value {
    let raw = mock.last_order.lock().expect("uncontended").clone();
    serde_json::from_str(&raw).unwrap_or_else(|e| panic!("order body is JSON: {e} in {raw:?}"))
}

/// Equity must sum the marked base balances in **ascending value order**, not
/// in `HashMap` order.
///
/// A spot account values its own book — unlike OKX, which reports a scalar — so
/// it folds one float per marked product. `marks` is a `HashMap` with a
/// per-process `RandomState`, and folding it in iteration order made `equity()`
/// vary by a ULP between processes on identical inputs. A ULP either side of a
/// threshold is a different trade.
///
/// That cross-process drift cannot be reproduced inside one test process — the
/// seed is fixed for the run. What *is* testable is the convention: pin
/// `equity()` to an independently-computed ascending fold built from the public
/// `funds` / `position` / `price` accessors. The discriminating power lives in
/// `wallet::types`'s `marked_sum` unit tests, which prove the fixture's
/// magnitudes really do sum differently in different orders.
///
/// Balances span ~16 decades and are scrambled so neither insertion order nor
/// symbol order correlates with magnitude — a realistic book of same-magnitude
/// legs sums identically in every order and would prove nothing.
#[test]
fn equity_sums_the_marked_balances_in_canonical_order() {
    // Past the 32-leg stack/heap boundary in `marked_sum`, and verified below
    // to discriminate between summation orders — n = 40 does not, so the size
    // is chosen, not arbitrary.
    const N: usize = 33;

    let legs: Vec<(String, f64, f64)> = (0..N)
        .map(|i| {
            let exp = ((i * 7 + 3) % 17) as i32 - 8; // -8 ..= 8
            (
                format!("S{i:03}"),
                1.0 + (i as f64) * 0.5,
                10.0_f64.powi(exp),
            )
        })
        .collect();

    let balances = legs.clone();
    let mock = serve(move |server| {
        let balances = balances.clone();
        Box::pin(async move {
            let mut rows: Vec<serde_json::Value> = balances
                .iter()
                .map(|(ccy, units, _)| {
                    serde_json::json!({
                        "uuid": ccy, "currency": ccy,
                        "available_balance": { "value": format!("{units}"), "currency": ccy },
                    })
                })
                .collect();
            // A zero quote balance on purpose: the fold seeds from `funds`, and
            // a large seed would swamp the small legs in *every* order, leaving
            // nothing for the ordering to change.
            rows.push(serde_json::json!({
                "uuid": "usd", "currency": "USD",
                "available_balance": { "value": "0", "currency": "USD" },
            }));
            Mock::given(method("GET"))
                .and(path("/api/v3/brokerage/accounts"))
                .respond_with(ResponseTemplate::new(200).set_body_json(
                    serde_json::json!({ "accounts": rows, "has_next": false, "cursor": "" }),
                ))
                .mount(server)
                .await;
            Mock::given(method("GET"))
                .and(path("/api/v3/brokerage/orders/historical/fills"))
                .respond_with(ResponseTemplate::new(200).set_body_json(no_fills()))
                .mount(server)
                .await;
        })
    });

    let mut w = wallet(mock.uri.clone());
    // `update` feeds the mark and refreshes the balances; one bar per product.
    for (ccy, _, price) in &legs {
        let symbol = intern(format!("{ccy}-USD"));
        w.update(symbol, Candle::new(*price, *price, *price, *price, 1.0));
    }

    // The independent fold, from the public accessors only.
    let mut marked: Vec<f64> = legs
        .iter()
        .map(|(ccy, _, _)| {
            let symbol = intern(format!("{ccy}-USD"));
            let units = w.position(&symbol).amount;
            let price = w.price(&symbol).expect("mark was fed").0;
            units * price
        })
        .collect();
    assert_eq!(marked.len(), N, "every leg was marked");
    // Distinguishes "the balances never loaded" from "the sum is misordered":
    // an all-zero book sums identically in every order.
    assert!(
        marked.iter().any(|v| *v != 0.0),
        "the account balances did not reach the wallet; errors: {:?}",
        w.errors(),
    );
    marked.sort_by(|a, b| a.total_cmp(b));
    let want = marked.iter().fold(w.funds().0, |acc, v| acc + v);

    // Guard: if descending gives the same bits the fixture is not
    // discriminating and the assertion below would be vacuous.
    let desc = marked.iter().rev().fold(w.funds().0, |acc, v| acc + v);
    assert_ne!(
        want.to_bits(),
        desc.to_bits(),
        "fixture does not discriminate between summation orders",
    );

    let got = w.equity().0;
    assert_eq!(
        got.to_bits(),
        want.to_bits(),
        "equity {got:?} is not the ascending-order fold {want:?} \
         — a spot account must sum its book canonically; errors: {:?}",
        w.errors(),
    );
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
    w.refresh_account()
        .expect("account reachable on production");
    // A funded account reports a non-negative quote balance; the point is that
    // the signed request authenticated, not the specific number.
    assert!(
        w.funds().0 >= 0.0,
        "funds readable; errors: {:?}",
        w.errors()
    );
}

// --- the shared conformance suite ------------------------------------------
//
// Everything above this line is Coinbase-specific: the JWT, the spot-balance
// convention, the `order_configuration` payload shapes, the un-shortable
// remainder. Everything below is behaviour every venue owes, driven from
// `common::live` so a fix in one venue is a fix in both.

impl common::live::LiveVenue for CoinbaseWallet {
    fn fixture() -> common::live::VenueFixture {
        use common::live::{Account, FillRow, VenueFixture};
        const API: &str = "/api/v3/brokerage";
        VenueFixture {
            name: "coinbase",
            symbol: SYMBOL,
            // Spot: a venue size unit *is* a base unit.
            contract_multiplier: 1.0,
            grid_path: format!("{API}/market/products/{SYMBOL}"),
            fills_path: format!("{API}/orders/historical/fills"),
            place_order_path: format!("{API}/orders"),
            // One order endpoint, so the protective leg shares it.
            place_protective_path: format!("{API}/orders"),
            cancel_entry_path: format!("{API}/orders/batch_cancel"),
            cancel_protective_path: format!("{API}/orders/batch_cancel"),
            grid_body: product(),
            account_bodies: Box::new(|a: Account| {
                vec![(
                    format!("{API}/accounts"),
                    accounts(&a.base_units.to_string(), &a.quote.to_string()),
                )]
            }),
            fills_body: Box::new(|rows: &[FillRow]| {
                let fills: Vec<serde_json::Value> = rows
                    .iter()
                    .map(|r| {
                        serde_json::json!({
                            "trade_id": r.id,
                            "order_id": r.order_id,
                            // No monotone key here: the timestamp orders, the
                            // id dedupes.
                            "sequence_timestamp": r.sequence.to_string(),
                            "side": match r.side { Side::Buy => "BUY", Side::Sell => "SELL" },
                            "size": r.size.to_string(),
                            "price": r.price.to_string(),
                            "commission": r.fee.to_string(),
                        })
                    })
                    .collect();
                serde_json::json!({ "fills": fills })
            }),
            order_ok: Box::new(|id: &str| {
                serde_json::json!({
                    "success": true,
                    "success_response": { "order_id": id },
                })
            }),
            order_refused: serde_json::json!({
                "success": false,
                "error_response": { "message": "Insufficient balance" },
            }),
            cancel_ok: serde_json::json!({
                "results": [{ "success": true, "order_id": "VENUE1" }],
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
        $(#[test] fn $name() { common::live::$name::<CoinbaseWallet>() })*
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
