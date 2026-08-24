//! A conformance suite every live venue backend must pass.
//!
//! `live_okx.rs` and `live_coinbase.rs` were near-mirrors: six structurally
//! identical tests with the endpoint paths, the response envelopes and the
//! fixture values swapped, ~450 lines of scaffolding per venue. A behaviour
//! pinned for one venue was pinned for the other only by whoever remembered to
//! copy it across, and most of the time nobody did — neither file covered a
//! network failure, a non-2xx status, a malformed body, partial fills across
//! polls, `flatten`, or `cancel` by id.
//!
//! The split is: **this module asserts counts and outcomes** (one POST reached
//! the venue, one rejection was booked, this fill reached the strategy), and
//! **each venue file asserts payloads** — because the payload *is* the venue
//! contract, and a shared assertion over it would be an `if venue ==` in
//! disguise.
//!
//! Adding a venue means implementing [`LiveVenue`] and adding one delegating
//! `#[test]` per body below. Editing this module should be rare, and only ever
//! to describe a *shape* a venue can differ in rather than a venue itself —
//! [`VenueFixture::private_read_method`] exists because Kraken signs a form body
//! and so reads over `POST` where the other two read over `GET`, which is a
//! property of its auth scheme, not a special case for Kraken.

use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};

use fugazi::Candle;
use fugazi::types::{Symbol, symbol as intern};
use fugazi::wallet::{Ack, OrderId, Reference, Side, Size, Units, Wallet};
use wiremock::matchers::{method, path};
use wiremock::{Mock, MockServer, Request, ResponseTemplate};

use super::net::{Server, serve};

/// The account state a venue's balance endpoints should report.
#[derive(Debug, Clone, Copy)]
pub struct Account {
    /// Available quote-currency balance.
    pub quote: f64,
    /// Held position in **base units** — the trait's unit, whatever the venue
    /// quotes in. A fixture converts to contracts if its venue needs them.
    pub base_units: f64,
}

/// One fill, venue-neutral, that a fixture renders into its own envelope.
///
/// `size` is **venue-native** (contracts where the venue has them), matching
/// what the venue would actually report; the suite multiplies by
/// [`VenueFixture::contract_multiplier`] to get the base units it expects the
/// strategy to see.
#[derive(Debug, Clone)]
pub struct FillRow {
    pub id: &'static str,
    pub order_id: &'static str,
    /// Orders the fills for a venue with no monotone key. Ascending = oldest
    /// first.
    pub sequence: usize,
    pub side: Side,
    pub size: f64,
    pub price: f64,
    pub fee: f64,
}

/// How a venue should answer the next order POST.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum OrderOutcome {
    /// `200` with an accepted envelope.
    Accepted,
    /// `200` whose envelope reports a business rejection — the case that makes
    /// "HTTP 200 does not mean accepted" load-bearing.
    Refused,
    /// A transport-level failure status.
    Status(u16),
    /// `200` with a body that is not the shape the parser expects.
    Malformed,
}

/// Renders an [`Account`] into the endpoints and bodies a venue's balance
/// reads expect. A venue with separate balance and position endpoints returns
/// two entries.
pub type AccountBodies = Box<dyn Fn(Account) -> Vec<(String, serde_json::Value)> + Send + Sync>;

/// Renders venue-neutral [`FillRow`]s into a venue's own fills envelope.
pub type FillsBody = Box<dyn Fn(&[FillRow]) -> serde_json::Value + Send + Sync>;

/// Renders an accepted-order envelope carrying a venue order id.
pub type OrderOk = Box<dyn Fn(&str) -> serde_json::Value + Send + Sync>;

/// Everything a venue must tell the suite about itself: how to build a wallet,
/// which paths its mock has to answer, and the envelopes those answers take.
pub struct VenueFixture {
    pub name: &'static str,
    pub symbol: &'static str,
    /// Base units per venue-native size unit. `1.0` on a spot venue.
    pub contract_multiplier: f64,
    /// The instrument / product grid endpoint.
    pub grid_path: String,
    /// The fills endpoint.
    pub fills_path: String,
    /// The HTTP method the account and fills reads use.
    ///
    /// `GET` on a venue that authenticates a read with headers alone (OKX,
    /// Coinbase). Kraken signs a form body instead, so *every* private endpoint
    /// is a `POST` — including the ones that only read. The grid is not listed
    /// because it is a public, unsigned `GET` on all three.
    pub private_read_method: &'static str,
    /// Where a market or limit order is POSTed.
    pub place_order_path: String,
    /// Where a protective leg is POSTed. Equal to `place_order_path` on a venue
    /// with one order endpoint.
    pub place_protective_path: String,
    /// Where a resting entry is cancelled.
    pub cancel_entry_path: String,
    /// Where a protective leg is cancelled. Equal to `cancel_entry_path` on a
    /// venue with one cancel endpoint.
    pub cancel_protective_path: String,
    /// The grid body: size step, price tick, minimum, contract value.
    pub grid_body: serde_json::Value,
    /// The account endpoints and their bodies for a given [`Account`].
    pub account_bodies: AccountBodies,
    /// The fills envelope wrapping these rows.
    pub fills_body: FillsBody,
    /// An accepted-order envelope carrying this venue order id.
    pub order_ok: OrderOk,
    /// A `200` envelope reporting a business rejection.
    pub order_refused: serde_json::Value,
    /// A successful cancel envelope.
    pub cancel_ok: serde_json::Value,
}

/// A live venue the conformance suite can drive.
///
/// A local trait over a foreign type, so the orphan rule is satisfied by
/// implementing it here rather than in the crate.
pub trait LiveVenue: Wallet<Symbol> + Sized {
    fn fixture() -> VenueFixture;
    fn build(base_url: String) -> Self;
    /// The wallet's error log, rendered — so the suite needs no `LiveError`
    /// import and can put the detail in every failure message.
    fn error_log(&self) -> Vec<String>;
    /// Force an account sync without feeding a bar. Both wallets expose this
    /// publicly and the README tells callers to use it right after
    /// construction; it is how the suite reaches the "positions known, no mark
    /// yet" state a strategy is in before its first `update`.
    fn sync(&mut self);
}

/// What the mock should serve.
pub struct MockPlan {
    pub account: Account,
    /// Fills returned from the **second** poll onwards. The first poll is the
    /// cursor seed during submission and always answers empty, so a fill placed
    /// now is not mistaken for pre-existing history.
    pub fills: Vec<FillRow>,
    pub order: OrderOutcome,
}

impl MockPlan {
    pub fn new(account: Account) -> Self {
        Self {
            account,
            fills: Vec::new(),
            order: OrderOutcome::Accepted,
        }
    }

    pub fn with_fills(mut self, fills: Vec<FillRow>) -> Self {
        self.fills = fills;
        self
    }

    pub fn with_order(mut self, order: OrderOutcome) -> Self {
        self.order = order;
        self
    }
}

/// A mock wired from a fixture, plus the counters the suite asserts on.
pub struct VenueMock {
    pub server: Server,
    /// POSTs that reached any order endpoint.
    pub orders: Arc<AtomicUsize>,
    /// POSTs that reached either cancel endpoint.
    pub cancels: Arc<AtomicUsize>,
    /// GETs that reached the fills endpoint.
    pub polls: Arc<AtomicUsize>,
    /// The body of the most recent order POST, as sent.
    pub last_order: Arc<Mutex<String>>,
}

impl VenueMock {
    pub fn uri(&self) -> String {
        self.server.uri.clone()
    }
    pub fn orders(&self) -> usize {
        self.orders.load(Ordering::SeqCst)
    }
    pub fn cancels(&self) -> usize {
        self.cancels.load(Ordering::SeqCst)
    }
    /// The body of the most recent order POST, as sent — where a venue file
    /// asserts on its own payload shape.
    pub fn last_order(&self) -> String {
        self.last_order.lock().expect("uncontended").clone()
    }
}

/// Stand up a mock answering every endpoint `V`'s fixture names.
pub fn mount<V: LiveVenue>(plan: MockPlan) -> VenueMock {
    let fx = V::fixture();
    let orders = Arc::new(AtomicUsize::new(0));
    let cancels = Arc::new(AtomicUsize::new(0));
    let polls = Arc::new(AtomicUsize::new(0));
    let last_order = Arc::new(Mutex::new(String::new()));

    let (o, c, pl, lo) = (
        orders.clone(),
        cancels.clone(),
        polls.clone(),
        last_order.clone(),
    );
    let server = serve(move |server| {
        Box::pin(async move {
            mount_grid(server, &fx).await;
            mount_accounts(server, &fx, plan.account).await;
            mount_fills(server, &fx, plan.fills, pl).await;
            mount_orders(server, &fx, plan.order, o, lo).await;
            mount_cancels(server, &fx, c).await;
        })
    });

    VenueMock {
        server,
        orders,
        cancels,
        polls,
        last_order,
    }
}

async fn mount_grid(server: &MockServer, fx: &VenueFixture) {
    Mock::given(method("GET"))
        .and(path(fx.grid_path.clone()))
        .respond_with(ResponseTemplate::new(200).set_body_json(fx.grid_body.clone()))
        .mount(server)
        .await;
}

async fn mount_accounts(server: &MockServer, fx: &VenueFixture, account: Account) {
    for (p, body) in (fx.account_bodies)(account) {
        Mock::given(method(fx.private_read_method))
            .and(path(p))
            .respond_with(ResponseTemplate::new(200).set_body_json(body))
            .mount(server)
            .await;
    }
}

async fn mount_fills(
    server: &MockServer,
    fx: &VenueFixture,
    fills: Vec<FillRow>,
    polls: Arc<AtomicUsize>,
) {
    let empty = (fx.fills_body)(&[]);
    let loaded = (fx.fills_body)(&fills);
    Mock::given(method(fx.private_read_method))
        .and(path(fx.fills_path.clone()))
        .respond_with(move |_: &Request| {
            // One stateful responder rather than two overlapping mocks, so the
            // seed-then-report sequence is deterministic regardless of the
            // order wiremock happens to match in.
            let n = polls.fetch_add(1, Ordering::SeqCst);
            let body = if n == 0 { &empty } else { &loaded };
            ResponseTemplate::new(200).set_body_json(body.clone())
        })
        .mount(server)
        .await;
}

async fn mount_orders(
    server: &MockServer,
    fx: &VenueFixture,
    outcome: OrderOutcome,
    orders: Arc<AtomicUsize>,
    last: Arc<Mutex<String>>,
) {
    let ok = (fx.order_ok)("VENUE1");
    let refused = fx.order_refused.clone();
    let mut paths = vec![fx.place_order_path.clone()];
    if fx.place_protective_path != fx.place_order_path {
        paths.push(fx.place_protective_path.clone());
    }
    for p in paths {
        let (ok, refused) = (ok.clone(), refused.clone());
        let (orders, last) = (orders.clone(), last.clone());
        Mock::given(method("POST"))
            .and(path(p))
            .respond_with(move |req: &Request| {
                orders.fetch_add(1, Ordering::SeqCst);
                *last.lock().expect("uncontended") =
                    String::from_utf8_lossy(&req.body).into_owned();
                match outcome {
                    OrderOutcome::Accepted => ResponseTemplate::new(200).set_body_json(ok.clone()),
                    OrderOutcome::Refused => {
                        ResponseTemplate::new(200).set_body_json(refused.clone())
                    }
                    OrderOutcome::Status(code) => {
                        ResponseTemplate::new(code).set_body_string("venue said no")
                    }
                    OrderOutcome::Malformed => {
                        ResponseTemplate::new(200).set_body_string("{not json at all")
                    }
                }
            })
            .mount(server)
            .await;
    }
}

async fn mount_cancels(server: &MockServer, fx: &VenueFixture, cancels: Arc<AtomicUsize>) {
    let mut paths = vec![fx.cancel_entry_path.clone()];
    if fx.cancel_protective_path != fx.cancel_entry_path {
        paths.push(fx.cancel_protective_path.clone());
    }
    for p in paths {
        let (body, cancels) = (fx.cancel_ok.clone(), cancels.clone());
        Mock::given(method("POST"))
            .and(path(p))
            .respond_with(move |_: &Request| {
                cancels.fetch_add(1, Ordering::SeqCst);
                ResponseTemplate::new(200).set_body_json(body.clone())
            })
            .mount(server)
            .await;
    }
}

// --- shared helpers ---------------------------------------------------------

fn bar(price: f64) -> Candle {
    Candle::new(price, price * 1.01, price * 0.99, price, 1.0)
}

/// Drive one bar and return the fills it reported.
fn tick<V: LiveVenue>(w: &mut V, sym: &Symbol, price: f64) -> Vec<fugazi::wallet::Order<Symbol>> {
    w.update(sym.clone(), bar(price))
}

// --- the conformance bodies -------------------------------------------------

/// A market order reaches the venue, and the fill comes back **in base units**.
///
/// The unit conversion is the point on a contract venue: a wallet that forgot
/// to multiply by the contract value would report a position 100x wrong and
/// nothing above it would know.
pub fn market_order_round_trips_and_reports_the_fill<V: LiveVenue>() {
    let fx = V::fixture();
    let sym = intern(fx.symbol);
    let venue_size = 3.0;
    let base = venue_size * fx.contract_multiplier;

    let mock = mount::<V>(
        MockPlan::new(Account {
            quote: 10_000.0,
            base_units: base,
        })
        .with_fills(vec![FillRow {
            id: "88",
            order_id: "VENUE1",
            sequence: 1,
            side: Side::Buy,
            size: venue_size,
            price: 27_000.0,
            fee: 0.08,
        }]),
    );
    let mut w = V::build(mock.uri());

    let ack = w
        .set_position(Units {
            symbol: sym.clone(),
            amount: base,
        })
        .expect("submission accepted");
    assert!(
        matches!(ack, Ack::Working(_)),
        "{}: a market order returns Working — the fill lands later",
        fx.name
    );
    assert_eq!(mock.orders(), 1, "{}: exactly one order POSTed", fx.name);

    let fills = tick(&mut w, &sym, 27_050.0);
    assert_eq!(
        fills.len(),
        1,
        "{}: expected one fill; errors: {:?}",
        fx.name,
        w.error_log()
    );
    assert_eq!(fills[0].side, Side::Buy);
    assert!(
        (fills[0].units - base).abs() < 1e-9,
        "{}: {venue_size} venue units -> {base} base units, got {}",
        fx.name,
        fills[0].units
    );
    assert!((fills[0].price - 27_000.0).abs() < 1e-9);
    assert!((fills[0].commission - 0.08).abs() < 1e-9);

    // Reads reflect the refreshed account, in base units.
    assert!((w.position(&sym).amount - base).abs() < 1e-9);
    assert!((w.price(&sym).expect("mark fed").0 - 27_050.0).abs() < 1e-9);

    assert!(
        w.poll_fills().is_empty(),
        "{}: a reported fill must not come back",
        fx.name
    );
}

/// The same fill returned by poll after poll reaches the strategy **once**.
///
/// Both venues poll a recent *window* rather than a since-cursor endpoint, so
/// the venue keeps handing the fill back; without dedupe the strategy would see
/// its position move again on every bar.
pub fn a_repeated_fill_is_reported_only_once<V: LiveVenue>() {
    let fx = V::fixture();
    let sym = intern(fx.symbol);
    let base = 3.0 * fx.contract_multiplier;

    let mock = mount::<V>(
        MockPlan::new(Account {
            quote: 10_000.0,
            base_units: base,
        })
        .with_fills(vec![FillRow {
            id: "88",
            order_id: "VENUE1",
            sequence: 1,
            side: Side::Buy,
            size: 3.0,
            price: 27_000.0,
            fee: 0.0,
        }]),
    );
    let mut w = V::build(mock.uri());
    let _ = w.set_position(Units {
        symbol: sym.clone(),
        amount: base,
    });

    let mut seen = 0;
    for _ in 0..10 {
        seen += tick(&mut w, &sym, 27_000.0).len();
    }
    assert_eq!(
        seen,
        1,
        "{}: ten polls of the same window reported the fill {seen} times; errors: {:?}",
        fx.name,
        w.error_log()
    );
}

/// Partial fills reach the strategy oldest-first, across polls.
///
/// A venue answers a large order with several fills, and a strategy that books
/// them out of order gets the wrong average entry.
pub fn partial_fills_arrive_oldest_first<V: LiveVenue>() {
    let fx = V::fixture();
    let sym = intern(fx.symbol);
    let base = 6.0 * fx.contract_multiplier;

    // Deliberately out of order in the response — the venue returns
    // most-recent-first, and the wallet is what re-orders them.
    let mock = mount::<V>(
        MockPlan::new(Account {
            quote: 10_000.0,
            base_units: base,
        })
        .with_fills(vec![
            FillRow {
                id: "3",
                order_id: "VENUE1",
                sequence: 3,
                side: Side::Buy,
                size: 1.0,
                price: 27_300.0,
                fee: 0.0,
            },
            FillRow {
                id: "1",
                order_id: "VENUE1",
                sequence: 1,
                side: Side::Buy,
                size: 3.0,
                price: 27_100.0,
                fee: 0.0,
            },
            FillRow {
                id: "2",
                order_id: "VENUE1",
                sequence: 2,
                side: Side::Buy,
                size: 2.0,
                price: 27_200.0,
                fee: 0.0,
            },
        ]),
    );
    let mut w = V::build(mock.uri());
    let _ = w.set_position(Units {
        symbol: sym.clone(),
        amount: base,
    });

    let fills = tick(&mut w, &sym, 27_300.0);
    let prices: Vec<f64> = fills.iter().map(|f| f.price).collect();
    assert_eq!(
        prices.len(),
        3,
        "{}: expected three partial fills, got {prices:?}; errors: {:?}",
        fx.name,
        w.error_log()
    );
    assert!(
        prices.windows(2).all(|w| w[0] <= w[1]),
        "{}: partial fills must arrive in execution order, got {prices:?}",
        fx.name
    );
}

/// A venue that answers `200` with a rejection envelope must surface it on the
/// **failure stream**, not just as an `Err` the strategy never sees.
///
/// `Strategy::trade` returns `()`, so a synchronous `Err` has nowhere to go;
/// `take_rejections` is how a refused entry reaches `on_reject`.
pub fn a_venue_refusal_surfaces_through_take_rejections<V: LiveVenue>() {
    let fx = V::fixture();
    let sym = intern(fx.symbol);
    let mock = mount::<V>(
        MockPlan::new(Account {
            quote: 10_000.0,
            base_units: 0.0,
        })
        .with_order(OrderOutcome::Refused),
    );
    let mut w = V::build(mock.uri());

    let err = w.set_position(Units {
        symbol: sym.clone(),
        amount: 3.0 * fx.contract_multiplier,
    });
    assert!(err.is_err(), "{}: a refused order returns Err", fx.name);

    let rejections = w.take_rejections();
    assert_eq!(
        rejections.len(),
        1,
        "{}: exactly one rejection buffered, got {rejections:?}",
        fx.name
    );
    assert_eq!(rejections[0].symbol, sym);
    assert!(
        !w.error_log().is_empty(),
        "{}: the detail behind the rejection must be readable",
        fx.name
    );
    assert!(
        w.take_rejections().is_empty(),
        "{}: a second drain yields nothing",
        fx.name
    );
}

/// A non-2xx status is an ordinary refusal, not a panic.
pub fn a_non_2xx_status_is_refused_and_logged<V: LiveVenue>() {
    assert_transport_failure::<V>(OrderOutcome::Status(500), "http 500");
}

/// So is a body that doesn't parse.
pub fn a_malformed_body_is_refused_and_logged<V: LiveVenue>() {
    assert_transport_failure::<V>(OrderOutcome::Malformed, "decode error");
}

fn assert_transport_failure<V: LiveVenue>(outcome: OrderOutcome, expect: &str) {
    let fx = V::fixture();
    let sym = intern(fx.symbol);
    let mock = mount::<V>(
        MockPlan::new(Account {
            quote: 10_000.0,
            base_units: 0.0,
        })
        .with_order(outcome),
    );
    let mut w = V::build(mock.uri());

    let result = w.set_position(Units {
        symbol: sym.clone(),
        amount: 3.0 * fx.contract_multiplier,
    });
    assert!(
        result.is_err(),
        "{}: {expect:?} must not read as an accepted order",
        fx.name
    );
    let log = w.error_log();
    assert!(
        log.iter().any(|e| e.contains(expect)),
        "{}: expected {expect:?} in the error log, got {log:?}",
        fx.name
    );
    assert_eq!(
        w.take_rejections().len(),
        1,
        "{}: the strategy must learn its order died",
        fx.name
    );
}

/// A dead endpoint is logged and the wallet carries on with stale state.
///
/// `update` has no error channel — it returns fills — so a venue that has gone
/// away must degrade to "no fills this bar", not abort the run.
pub fn a_network_failure_is_logged_not_panicked<V: LiveVenue>() {
    let fx = V::fixture();
    let sym = intern(fx.symbol);
    // Port 1 refuses connections on every platform CI runs on.
    let mut w = V::build("http://127.0.0.1:1".to_string());

    let fills = tick(&mut w, &sym, 27_000.0);
    assert!(
        fills.is_empty(),
        "{}: an unreachable venue reports no fills",
        fx.name
    );
    assert!(
        !w.error_log().is_empty(),
        "{}: the failure must be visible in the log, not swallowed",
        fx.name
    );
    // Still usable: the reads answer from (empty) cache rather than panicking.
    assert_eq!(w.position(&sym).amount, 0.0);
}

/// Re-resting an unchanged protective leg is a no-op; moving it replaces the
/// venue order.
///
/// A strategy walks its stop every bar, so without dedupe this would pile up a
/// new algo order per bar and cancel none of them.
pub fn a_protective_leg_dedups_an_unchanged_trigger<V: LiveVenue>() {
    let fx = V::fixture();
    let sym = intern(fx.symbol);
    let base = 3.0 * fx.contract_multiplier;
    let mock = mount::<V>(MockPlan::new(Account {
        quote: 10_000.0,
        base_units: base,
    }));
    let mut w = V::build(mock.uri());

    // A bar first, so the wallet holds the position the leg is sized against.
    tick(&mut w, &sym, 27_000.0);

    let first = w
        .set_stop(sym.clone(), Reference(26_000.0), Size::units(base))
        .unwrap_or_else(|e| panic!("{}: stop rested: {e:?} {:?}", fx.name, w.error_log()));
    assert_eq!(mock.orders(), 1, "{}: one algo order placed", fx.name);

    let again = w
        .set_stop(sym.clone(), Reference(26_000.0), Size::units(base))
        .expect("unchanged re-submit accepted");
    assert_eq!(
        mock.orders(),
        1,
        "{}: an unchanged trigger must not place a second order",
        fx.name
    );
    assert!(
        matches!((first, again), (Ack::Working(a), Ack::Working(b)) if a == b),
        "{}: an unchanged re-submit returns the resting order's own id",
        fx.name
    );

    // A moved trigger cancels the old leg and places a new one.
    w.set_stop(sym.clone(), Reference(26_500.0), Size::units(base))
        .expect("moved stop rested");
    assert_eq!(
        mock.orders(),
        2,
        "{}: a moved trigger places a replacement",
        fx.name
    );
    assert_eq!(
        mock.cancels(),
        1,
        "{}: and cancels the one it replaced",
        fx.name
    );
}

/// The same contract for a resting limit entry.
pub fn a_limit_dedups_an_unchanged_resubmit<V: LiveVenue>() {
    let fx = V::fixture();
    let sym = intern(fx.symbol);
    let base = 3.0 * fx.contract_multiplier;
    let mock = mount::<V>(MockPlan::new(Account {
        quote: 10_000.0,
        base_units: 0.0,
    }));
    let mut w = V::build(mock.uri());
    tick(&mut w, &sym, 27_000.0);

    let first = w
        .set_limit(
            sym.clone(),
            Side::Buy,
            Size::units(base),
            Reference(26_000.0),
        )
        .unwrap_or_else(|e| panic!("{}: limit rested: {e:?} {:?}", fx.name, w.error_log()));
    assert_eq!(mock.orders(), 1, "{}: one limit placed", fx.name);

    let again = w
        .set_limit(
            sym.clone(),
            Side::Buy,
            Size::units(base),
            Reference(26_000.0),
        )
        .expect("unchanged re-submit accepted");
    assert_eq!(
        mock.orders(),
        1,
        "{}: an unchanged limit must not place a second order",
        fx.name
    );
    assert!(
        matches!((first, again), (Ack::Working(a), Ack::Working(b)) if a == b),
        "{}: an unchanged re-submit returns the resting order's own id",
        fx.name
    );

    w.set_limit(
        sym.clone(),
        Side::Buy,
        Size::units(base),
        Reference(25_000.0),
    )
    .expect("moved limit rested");
    assert_eq!(mock.orders(), 2, "{}: a moved limit is replaced", fx.name);
    assert_eq!(mock.cancels(), 1, "{}: and the old one cancelled", fx.name);
}

/// `cancel(id)` withdraws a resting limit by the id its submission returned.
///
/// The wallet has to map its own [`OrderId`] back to the venue's, and know
/// which of its resting records — and so which cancel endpoint — the id belongs
/// to.
pub fn cancel_by_id_withdraws_a_resting_limit<V: LiveVenue>() {
    let fx = V::fixture();
    let sym = intern(fx.symbol);
    let base = 3.0 * fx.contract_multiplier;
    let mock = mount::<V>(MockPlan::new(Account {
        quote: 10_000.0,
        base_units: 0.0,
    }));
    let mut w = V::build(mock.uri());
    tick(&mut w, &sym, 27_000.0);

    let Ack::Working(id) = w
        .set_limit(
            sym.clone(),
            Side::Buy,
            Size::units(base),
            Reference(26_000.0),
        )
        .unwrap_or_else(|e| panic!("{}: limit rested: {e:?} {:?}", fx.name, w.error_log()))
    else {
        panic!("{}: a resting limit acks as Working", fx.name)
    };

    w.cancel(id)
        .unwrap_or_else(|e| panic!("{}: cancel by id: {e:?}", fx.name));
    assert_eq!(mock.cancels(), 1, "{}: the venue order was pulled", fx.name);

    // An id the wallet never issued is a no-op, not an error: the
    // post-condition ("that order is not resting") already holds.
    w.cancel(OrderId(9_999)).unwrap_or_else(|e| {
        panic!(
            "{}: cancelling an unknown id must be a no-op: {e:?}",
            fx.name
        )
    });
    assert_eq!(mock.cancels(), 1, "{}: and nothing else was", fx.name);
}

/// `flatten` cancels every resting order and closes the position.
///
/// It is the trait default, and it only works because both wallets override
/// `positions()` — a venue that can't enumerate flattens nothing, silently.
/// Neither venue file covered this before.
pub fn flatten_cancels_the_resting_orders_and_closes_the_position<V: LiveVenue>() {
    let fx = V::fixture();
    let sym = intern(fx.symbol);
    let base = 3.0 * fx.contract_multiplier;
    let mock = mount::<V>(MockPlan::new(Account {
        quote: 10_000.0,
        base_units: base,
    }));
    let mut w = V::build(mock.uri());
    tick(&mut w, &sym, 27_000.0);

    w.set_stop(sym.clone(), Reference(26_000.0), Size::units(base))
        .unwrap_or_else(|e| panic!("{}: stop rested: {e:?} {:?}", fx.name, w.error_log()));
    let after_stop = mock.orders();

    w.flatten();

    assert!(
        mock.cancels() >= 1,
        "{}: flatten must cancel the resting protective leg",
        fx.name
    );
    assert!(
        mock.orders() > after_stop,
        "{}: flatten must submit a close for the open position; errors: {:?}",
        fx.name,
        w.error_log()
    );
}

/// A non-positive trigger is refused **before** it reaches the venue.
///
/// Nonsense on any venue, and a wallet that forwards it is asking to have an
/// order rejected on arrival with the detail buried in a REST envelope.
pub fn a_non_positive_protective_trigger_is_refused_locally<V: LiveVenue>() {
    let fx = V::fixture();
    let sym = intern(fx.symbol);
    let base = 3.0 * fx.contract_multiplier;
    let mock = mount::<V>(MockPlan::new(Account {
        quote: 10_000.0,
        base_units: base,
    }));
    let mut w = V::build(mock.uri());
    tick(&mut w, &sym, 27_000.0);
    let placed_before = mock.orders();

    let result = w.set_stop(sym.clone(), Reference(0.0), Size::units(base));
    assert!(result.is_err(), "{}: a zero trigger is refused", fx.name);
    assert_eq!(
        mock.orders(),
        placed_before,
        "{}: nothing may reach the venue",
        fx.name
    );
    assert_eq!(
        w.take_rejections().len(),
        1,
        "{}: and the strategy must learn its stop did not rest",
        fx.name
    );
}

/// A protective leg rested **before the first bar** sizes against its own
/// trigger.
///
/// A strategy that syncs its account and rests a stop before feeding a candle
/// has no mark yet. Falling back to zero makes `Size::resolve` answer `0.0` for
/// every fraction-shaped size — so every such stop refuses, and the position
/// sits unprotected with only a line in the error log to say so. The trigger is
/// the only price the caller has actually named, and it is the price the leg
/// will fill near.
pub fn a_protective_leg_rested_before_the_first_bar_sizes_at_its_trigger<V: LiveVenue>() {
    let fx = V::fixture();
    let sym = intern(fx.symbol);
    let base = 3.0 * fx.contract_multiplier;
    let mock = mount::<V>(MockPlan::new(Account {
        quote: 10_000.0,
        base_units: base,
    }));
    let mut w = V::build(mock.uri());

    // Account synced, no candle fed: `price()` is None on purpose.
    w.sync();
    assert!(
        w.price(&sym).is_none(),
        "{}: the fixture must have no mark, or this proves nothing",
        fx.name
    );
    assert!(
        (w.position(&sym).amount - base).abs() < 1e-9,
        "{}: the synced position is what the leg is sized against",
        fx.name
    );

    w.set_stop(sym.clone(), Reference(26_000.0), Size::value_frac(0.5))
        .unwrap_or_else(|e| {
            panic!(
                "{}: a fraction-sized stop before the first bar: {e:?} {:?}",
                fx.name,
                w.error_log()
            )
        });
    assert_eq!(
        mock.orders(),
        1,
        "{}: the protective leg must reach the venue",
        fx.name
    );
}
