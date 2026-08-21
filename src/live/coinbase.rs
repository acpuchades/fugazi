//! A [`Wallet`] implementation backed by Coinbase Advanced Trade **spot**.
//!
//! [`CoinbaseWallet`] routes the trait's order flow to Coinbase's Advanced Trade
//! REST API (`/api/v3/brokerage/*`), authenticating every private call with a
//! short-lived **ES256 JWT** (ECDSA over the P-256 curve) — the scheme Coinbase's
//! CDP keys use, in place of OKX's HMAC. Each request builds a fresh token whose
//! `uri` claim pins it to that one `METHOD host+path`, signs it with the key's
//! PEM private key, and sends it as `Authorization: Bearer …`.
//!
//! ## Spot, not swaps — the one real semantic difference
//!
//! Coinbase Advanced Trade is a **spot** venue: you hold balances of a base
//! asset, you cannot hold a signed/short position. So this wallet maps the
//! trait's [`Units`] onto balances:
//!
//! * [`position`](Wallet::position) for `BTC-USD` is the account's **BTC
//!   available balance** (the product's base currency); it is never negative.
//! * [`funds`](Wallet::funds) is the **quote-currency** available balance
//!   (`USD` by default; override with [`with_quote_ccy`](CoinbaseWallet::with_quote_ccy)).
//! * [`equity`](Wallet::equity) is funds plus every marked base balance valued at
//!   its last-fed `close`.
//! * [`set_position`](Wallet::set_position) diffs the target against the current
//!   base balance and places a **market** order for the difference — a buy for a
//!   shortfall, a sell for an excess. A **negative** target can't be honoured on
//!   spot: the wallet sells down to flat and buffers a [`Rejection`] for the
//!   un-shortable remainder, so the strategy learns its short didn't take rather
//!   than silently believing it did.
//!
//! ## How the rest of the trait maps onto the venue
//!
//! * **Reads** serve a cache refreshed from `GET /api/v3/brokerage/accounts` at
//!   the top of each [`update`](Wallet::update). [`price`](Wallet::price) returns
//!   the last candle `close` fed in.
//! * **Market moves** round the base-unit difference down to the product's
//!   `base_increment` and `POST /api/v3/brokerage/orders` a `market_market_ioc`
//!   order tagged with a `client_order_id` derived from the wallet-minted
//!   [`OrderId`]. Submitting returns [`Ack::Working`]; the fill lands later.
//! * **Resting orders** — [`set_limit`](Wallet::set_limit) (`limit_limit_gtc`)
//!   and the protective [`set_stop`](Wallet::set_stop) /
//!   [`set_take_profit`](Wallet::set_take_profit) (`stop_limit_stop_limit_gtc`,
//!   sized reduce-only against the held balance) — are **deduped** per symbol, so
//!   an unchanged re-submit each bar is a no-op instead of a cancel/replace storm.
//! * **Fills** are polled from `GET /api/v3/brokerage/orders/historical/fills`
//!   (a per-symbol set of already-reported `trade_id`s). They surface from both
//!   [`update`](Wallet::update) and [`poll_fills`](Wallet::poll_fills), so a fill
//!   on a symbol that didn't tick this bar still reaches the strategy.
//! * **Refusals** return the [`WalletError::Venue`] category *and* are buffered
//!   onto the failure stream drained by
//!   [`take_rejections`](Wallet::take_rejections). The full detail also lands on
//!   [`errors`](CoinbaseWallet::errors).
//!
//! REST fill polling is the MVP; a WebSocket user-data stream is the natural
//! lower-latency follow-up.

use std::collections::{HashMap, HashSet};
use std::time::{SystemTime, UNIX_EPOCH};

use base64::Engine as _;
use p256::ecdsa::{Signature, SigningKey, signature::Signer};
use reqwest::Method;

use crate::types::Symbol;
use crate::types::{Candle, Real};
use crate::wallet::{
    Ack, Order, OrderId, OrderKind, Reference, Rejection, Side, Size, Units, Wallet, WalletError,
};
use crate::wallet::{POSITION_EPSILON, PRICE_EPSILON};

use super::LiveError;
use super::venue::{
    HttpCore, decimals_of, floor_to_step, format_decimals, parse_num, round_to_tick, with_query,
};

const MAINNET_BASE_URL: &str = "https://api.coinbase.com";
const API_PREFIX: &str = "/api/v3/brokerage";
/// The quote currency whose available balance is reported as [`funds`](Wallet::funds).
const DEFAULT_QUOTE_CCY: &str = "USD";
/// How long each signed JWT is valid, in seconds (Coinbase's fixed window).
const JWT_TTL_SECS: u64 = 120;

/// The trading grid for one product, needed so submitted sizes and prices land
/// on the venue's increments. Parsed once from
/// `GET /api/v3/brokerage/market/products/{id}` and cached.
#[derive(Debug, Clone, Copy)]
struct ProductSpec {
    /// Base-asset size step.
    base_increment: Real,
    /// Minimum base-asset order size.
    base_min: Real,
    /// Quote-price step.
    quote_increment: Real,
    base_decimals: usize,
    price_decimals: usize,
}

/// A resting order we've placed (a limit entry or a protective stop leg), kept so
/// a re-submit at the same parameters is a no-op and a change cancels the venue
/// order before replacing it.
#[derive(Debug, Clone)]
struct RestingOrder {
    /// Trigger for a protective leg; the limit price for a plain limit entry.
    price: Real,
    /// Resolved size in base units.
    base_size: Real,
    side: Side,
    order_id: String,
    local: OrderId,
}

#[derive(Debug, Clone, Default)]
struct ProtectiveState {
    stop: Option<RestingOrder>,
    take_profit: Option<RestingOrder>,
}

/// A live [`Wallet`] over Coinbase Advanced Trade spot. See the module-level
/// docs for the trait-to-venue mapping and the spot-balance convention.
///
/// Construct with [`mainnet`](Self::mainnet) (**real funds**), then drive it
/// through [`backtest::run`](crate::backtest::run) exactly like a
/// [`PaperWallet`](crate::PaperWallet). Must be used from a synchronous context
/// (it owns a `tokio` runtime and blocks on each REST call).
pub struct CoinbaseWallet {
    http: HttpCore,
    /// The host used in the JWT `uri` claim (`api.coinbase.com`).
    host: String,
    /// The CDP key name — the JWT `kid` header and `sub` claim.
    key_name: String,
    /// The P-256 signing key parsed from the CDP key's PEM.
    signing_key: SigningKey,
    quote_ccy: String,

    // Cached account state, refreshed from the accounts endpoint.
    balances: HashMap<String, Real>,
    marks: HashMap<Symbol, Real>,
    specs: HashMap<Symbol, ProductSpec>,

    // Order-id bookkeeping: wallet-minted local ids <-> venue order ids, and the
    // kind each venue order was placed as (so a polled fill is tagged).
    next_id: u64,
    nonce_counter: u64,
    local_to_venue: HashMap<OrderId, String>,
    venue_to_local: HashMap<String, OrderId>,
    order_kind: HashMap<String, OrderKind>,

    // Resting orders, for idempotent re-submit / cancel-on-change.
    protective: HashMap<Symbol, ProtectiveState>,
    limits: HashMap<Symbol, RestingOrder>,

    // Fill polling: per-symbol set of already-reported trade ids, and the log.
    seen_trades: HashMap<String, HashSet<String>>,
    errors: Vec<LiveError>,
    // Refused orders awaiting a drain through take_rejections.
    rejections: Vec<Rejection<Symbol>>,
}

impl CoinbaseWallet {
    /// A wallet against Coinbase **production** (`api.coinbase.com`). This trades
    /// **real funds** — supply live CDP credentials deliberately.
    ///
    /// `key_name` is the CDP API key name
    /// (`organizations/{org}/apiKeys/{key}`); `private_key_pem` is that key's
    /// EC private key in PEM form (either `EC PRIVATE KEY` / SEC1 or
    /// `PRIVATE KEY` / PKCS#8). Errors if the PEM does not parse as a P-256 key.
    pub fn mainnet(key_name: impl Into<String>, private_key_pem: &str) -> Result<Self, LiveError> {
        Self::with_base_url(MAINNET_BASE_URL, key_name, private_key_pem)
    }

    /// A wallet against an explicit base URL — mainly to point tests at a
    /// `wiremock` server. Panics only if a `tokio` current-thread runtime can't
    /// be built (out of OS resources).
    pub fn with_base_url(
        base_url: impl Into<String>,
        key_name: impl Into<String>,
        private_key_pem: &str,
    ) -> Result<Self, LiveError> {
        let signing_key = parse_private_key(private_key_pem)?;
        let base_url = base_url.into();
        let host = host_of(&base_url);
        Ok(Self {
            http: HttpCore::new(base_url),
            host,
            key_name: key_name.into(),
            signing_key,
            quote_ccy: DEFAULT_QUOTE_CCY.to_string(),
            balances: HashMap::new(),
            marks: HashMap::new(),
            specs: HashMap::new(),
            next_id: 0,
            nonce_counter: 0,
            local_to_venue: HashMap::new(),
            venue_to_local: HashMap::new(),
            order_kind: HashMap::new(),
            protective: HashMap::new(),
            limits: HashMap::new(),
            seen_trades: HashMap::new(),
            errors: Vec::new(),
            rejections: Vec::new(),
        })
    }

    /// Override the quote currency whose available balance is reported as
    /// [`funds`](Wallet::funds) (`USD` by default). Set it to `USDC`, `EUR`, …
    /// to match the quote leg of the products you trade.
    pub fn with_quote_ccy(mut self, ccy: impl Into<String>) -> Self {
        self.quote_ccy = ccy.into();
        self
    }

    /// An inert placeholder wallet — a fixed throwaway key, empty credentials,
    /// no cached state. It exists only as a temporary swap target (e.g. a
    /// `std::mem::replace` slot in a wrapper's setup) and must never be driven or
    /// have a request made through it. Cannot fail: the key is built from a fixed
    /// valid scalar rather than parsed from input.
    pub fn placeholder() -> Self {
        let signing_key = SigningKey::from_bytes((&[1u8; 32]).into())
            .expect("a fixed nonzero scalar is a valid P-256 key");
        Self {
            http: HttpCore::new(MAINNET_BASE_URL),
            host: host_of(MAINNET_BASE_URL),
            key_name: String::new(),
            signing_key,
            quote_ccy: DEFAULT_QUOTE_CCY.to_string(),
            balances: HashMap::new(),
            marks: HashMap::new(),
            specs: HashMap::new(),
            next_id: 0,
            nonce_counter: 0,
            local_to_venue: HashMap::new(),
            venue_to_local: HashMap::new(),
            order_kind: HashMap::new(),
            protective: HashMap::new(),
            limits: HashMap::new(),
            seen_trades: HashMap::new(),
            errors: Vec::new(),
            rejections: Vec::new(),
        }
    }

    /// The live errors this wallet has recorded, in order. Every REST failure
    /// (the detail behind a returned [`WalletError::Venue`], plus best-effort
    /// refresh / fill-poll failures that don't have a return channel) is appended
    /// here, so a caller can see *why* a leg failed.
    pub fn errors(&self) -> &[LiveError] {
        &self.errors
    }

    /// The base currency of a product id (`BTC-USD` → `BTC`) — the balance that
    /// [`position`](Wallet::position) reports for it.
    fn base_ccy(symbol: &str) -> &str {
        symbol.split('-').next().unwrap_or(symbol)
    }

    /// Force an account-state refresh (balances + equity) now, returning the
    /// [`LiveError`] on failure. [`update`](Wallet::update) calls this each bar;
    /// call it directly for a one-off sync (e.g. right after construction).
    pub fn refresh_account(&mut self) -> Result<(), LiveError> {
        let mut balances: HashMap<String, Real> = HashMap::new();
        let mut cursor: Option<String> = None;
        loop {
            let mut query = vec![("limit", "250".to_string())];
            if let Some(c) = &cursor {
                query.push(("cursor", c.clone()));
            }
            let value = self.signed(Method::GET, "/accounts", &query, None)?;
            if let Some(accounts) = value.get("accounts").and_then(|a| a.as_array()) {
                for acct in accounts {
                    let Some(ccy) = acct.get("currency").and_then(|c| c.as_str()) else {
                        continue;
                    };
                    let avail = acct
                        .get("available_balance")
                        .and_then(|b| b.get("value"))
                        .and_then(parse_num)
                        .unwrap_or(0.0);
                    balances.insert(ccy.to_string(), avail);
                }
            }
            let has_next = value
                .get("has_next")
                .and_then(|h| h.as_bool())
                .unwrap_or(false);
            cursor = value
                .get("cursor")
                .and_then(|c| c.as_str())
                .filter(|s| !s.is_empty())
                .map(|s| s.to_string());
            if !has_next || cursor.is_none() {
                break;
            }
        }
        self.balances = balances;
        Ok(())
    }

    /// Quote-currency available balance — the trait's [`funds`](Wallet::funds).
    fn quote_balance(&self) -> Real {
        self.balances.get(&self.quote_ccy).copied().unwrap_or(0.0)
    }

    /// Current base-asset balance for `symbol` — the trait's spot "position".
    fn base_balance(&self, symbol: &str) -> Real {
        self.balances
            .get(Self::base_ccy(symbol))
            .copied()
            .unwrap_or(0.0)
    }

    /// Mint the next unique local [`OrderId`].
    fn mint(&mut self) -> OrderId {
        let id = OrderId(self.next_id);
        self.next_id += 1;
        id
    }

    /// A unique JWT nonce (time-in-nanos plus a monotonic counter, hex).
    fn next_nonce(&mut self) -> String {
        let nanos = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|d| d.as_nanos())
            .unwrap_or(0);
        let c = self.nonce_counter;
        self.nonce_counter = self.nonce_counter.wrapping_add(1);
        format!("{nanos:x}{c:x}")
    }

    /// Record a placed order's venue id + kind against a local id.
    fn map_order(&mut self, local: OrderId, venue_id: &str, kind: OrderKind) {
        self.local_to_venue.insert(local, venue_id.to_string());
        self.venue_to_local.insert(venue_id.to_string(), local);
        self.order_kind.insert(venue_id.to_string(), kind);
    }

    /// Ensure the [`ProductSpec`] for `symbol` is cached, fetching the public
    /// product endpoint if not.
    fn ensure_spec(&mut self, symbol: &str) -> Result<ProductSpec, LiveError> {
        if let Some(s) = self.specs.get(symbol) {
            return Ok(*s);
        }
        let path = format!("{API_PREFIX}/market/products/{symbol}");
        let value = self.public_get(&path)?;
        let spec = parse_product_spec(&value)
            .ok_or_else(|| LiveError::Decode(format!("no product spec for {symbol}")))?;
        self.specs.insert(crate::types::symbol(symbol), spec);
        Ok(spec)
    }

    /// Ensure the seen-trades set for `symbol` exists, seeding it with the
    /// product's current fills so we only ever report fills that happen *after*
    /// we started trading it (not the account's whole history).
    fn ensure_cursor(&mut self, symbol: &str) -> Result<(), LiveError> {
        if self.seen_trades.contains_key(symbol) {
            return Ok(());
        }
        let fills = self.fetch_fills(symbol)?;
        let seen: HashSet<String> = fills.into_iter().map(|f| f.trade_id).collect();
        self.seen_trades.insert(symbol.to_string(), seen);
        Ok(())
    }

    /// Poll fills for `symbol` we haven't reported yet, mark them seen, and
    /// return them as [`Order`]s. A venue order we placed maps back to its local
    /// [`OrderId`] and recorded [`OrderKind`]; a fill on an order we don't know
    /// (placed out-of-band) gets a fresh local id and `Market` kind.
    fn poll_symbol(&mut self, symbol: &str) -> Result<Vec<Order<Symbol>>, LiveError> {
        let mut fills = self.fetch_fills(symbol)?;
        // Oldest-first, so partial fills reach the strategy in execution order.
        fills.sort_by(|a, b| a.sequence.cmp(&b.sequence));
        let seen = self.seen_trades.entry(symbol.to_string()).or_default();
        let fresh: Vec<Fill> = fills
            .into_iter()
            .filter(|f| seen.insert(f.trade_id.clone()))
            .collect();
        let mut out = Vec::new();
        for f in fresh {
            let local = match self.venue_to_local.get(&f.order_id).copied() {
                Some(id) => id,
                None => self.mint(),
            };
            let kind = self
                .order_kind
                .get(&f.order_id)
                .copied()
                .unwrap_or(OrderKind::Market);
            let order = Order::new(
                crate::types::symbol(symbol),
                f.side,
                f.size,
                f.price,
                kind,
                local,
            )
            .with_commission(f.commission);
            out.push(order);
        }
        Ok(out)
    }

    /// Record `err` on the internal log and return the trait-facing
    /// [`WalletError::Venue`] category.
    fn fail(&mut self, err: LiveError) -> WalletError {
        self.errors.push(err);
        WalletError::Venue
    }

    /// A **refused order**: log the detail, buffer a [`Rejection`] for
    /// [`take_rejections`](Wallet::take_rejections), and return
    /// [`WalletError::Venue`].
    fn refuse(
        &mut self,
        symbol: &str,
        id: OrderId,
        kind: OrderKind,
        err: LiveError,
    ) -> WalletError {
        self.errors.push(err);
        self.rejections.push(Rejection {
            symbol: crate::types::symbol(symbol),
            id,
            error: WalletError::Venue,
            kind,
        });
        WalletError::Venue
    }

    // --- REST plumbing -----------------------------------------------------

    /// A signed private request; blocks on the owned runtime. `query` is the
    /// endpoint-specific query params (GET); `body` is the JSON value sent (POST).
    /// `path` is relative to [`API_PREFIX`] (`/accounts`, `/orders`, …).
    fn signed(
        &mut self,
        method: Method,
        path: &str,
        query: &[(&str, String)],
        body: Option<serde_json::Value>,
    ) -> Result<serde_json::Value, LiveError> {
        let full_path = format!("{API_PREFIX}{path}");
        let jwt = self.build_jwt(method.as_str(), &full_path)?;
        let url = self.http.url(&with_query(&full_path, query));
        let mut req = self
            .http
            .client()
            .request(method, &url)
            .bearer_auth(jwt)
            .header("Content-Type", "application/json");
        if let Some(b) = body {
            req = req.json(&b);
        }
        self.http.send(req)
    }

    /// An unsigned public GET (product specs, etc.). `path` is a full API path.
    fn public_get(&self, path: &str) -> Result<serde_json::Value, LiveError> {
        self.http.public_get(path, &[])
    }

    /// Build an ES256 JWT for one `METHOD full_path` request. The `uri` claim
    /// carries the bare path (no query string), matching Coinbase's convention.
    fn build_jwt(&mut self, method: &str, full_path: &str) -> Result<String, LiveError> {
        let now = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|d| d.as_secs())
            .unwrap_or(0);
        let nonce = self.next_nonce();
        build_jwt(
            &self.key_name,
            &self.host,
            method,
            full_path,
            &nonce,
            now,
            &self.signing_key,
        )
    }

    /// Fetch the recent fills for `symbol`.
    fn fetch_fills(&mut self, symbol: &str) -> Result<Vec<Fill>, LiveError> {
        let query = vec![
            ("product_id", symbol.to_string()),
            ("limit", "100".to_string()),
        ];
        let value = self.signed(Method::GET, "/orders/historical/fills", &query, None)?;
        let rows = value
            .get("fills")
            .and_then(|f| f.as_array())
            .cloned()
            .unwrap_or_default();
        rows.iter().map(parse_fill).collect()
    }

    /// Cancel a venue order by id, treating "already gone" as success — the
    /// post-condition (that order isn't working) holds either way.
    fn cancel_order(&mut self, order_id: &str) -> Result<(), WalletError> {
        let body = serde_json::json!({ "order_ids": [order_id] });
        match self.signed(Method::POST, "/orders/batch_cancel", &[], Some(body)) {
            Ok(_) => Ok(()),
            Err(e) => Err(self.fail(e)),
        }
    }

    /// Place a `market_market_ioc` order for `base_size` of `symbol` on `side`,
    /// mapping the minted `id`. Deduped/sized by the caller.
    fn place_market(
        &mut self,
        symbol: &str,
        side: Side,
        base_size: String,
        id: OrderId,
    ) -> Result<(), WalletError> {
        if let Err(e) = self.ensure_cursor(symbol) {
            self.errors.push(e);
        }
        let body = serde_json::json!({
            "client_order_id": client_order_id(id),
            "product_id": symbol,
            "side": side_token(side),
            "order_configuration": { "market_market_ioc": { "base_size": base_size } },
        });
        let value = match self.signed(Method::POST, "/orders", &[], Some(body)) {
            Ok(v) => v,
            Err(e) => return Err(self.refuse(symbol, id, OrderKind::Market, e)),
        };
        let order_id = match order_result_id(&value) {
            Ok(v) => v,
            Err(e) => return Err(self.refuse(symbol, id, OrderKind::Market, e)),
        };
        self.map_order(id, &order_id, OrderKind::Market);
        Ok(())
    }

    /// Rest a protective leg (a reduce-only `stop_limit_stop_limit_gtc` sell)
    /// with idempotent dedup, mirroring [`set_limit`](Wallet::set_limit).
    fn rest_protective(
        &mut self,
        symbol: Symbol,
        kind: OrderKind,
        trigger: Real,
        size: Size,
    ) -> Result<Ack<Symbol>, WalletError> {
        let local = self.mint();
        if trigger <= 0.0 {
            return Err(self.refuse(
                &symbol,
                local,
                kind,
                LiveError::Decode(format!(
                    "protective trigger must be positive, got {trigger}"
                )),
            ));
        }
        let spec = match self.ensure_spec(&symbol) {
            Ok(s) => s,
            Err(e) => return Err(self.refuse(&symbol, local, kind, e)),
        };
        // A protective exit sells the held base balance; clamp the share to what
        // we hold (reduce-only) and round to the base increment.
        let held = self.base_balance(&symbol);
        let units = size
            .resolve(
                self.marks.get(&symbol).copied().unwrap_or(trigger),
                held,
                self.quote_balance(),
                self.equity().0,
            )
            .min(held);
        let base_size = floor_to_step(units, spec.base_increment);
        if base_size < spec.base_min || base_size <= POSITION_EPSILON {
            return Err(self.refuse(
                &symbol,
                local,
                kind,
                LiveError::Decode("protective size rounds below the product minimum".into()),
            ));
        }
        let price = round_to_tick(trigger, spec.quote_increment);

        // Idempotent re-submit: an unchanged leg stays where it is.
        let existing = self.protective.get(&symbol).and_then(|p| match kind {
            OrderKind::TakeProfit => p.take_profit.clone(),
            _ => p.stop.clone(),
        });
        if let Some(leg) = existing {
            if (leg.price - price).abs() <= PRICE_EPSILON
                && (leg.base_size - base_size).abs() <= POSITION_EPSILON
            {
                return Ok(Ack::Working(leg.local));
            }
            self.cancel_order(&leg.order_id)?;
        }

        // Stop-loss triggers on the way *down* (sell as price falls); take-profit
        // triggers on the way *up*. limit == stop keeps the resting order
        // marketable once triggered.
        let stop_direction = match kind {
            OrderKind::TakeProfit => "STOP_DIRECTION_STOP_UP",
            _ => "STOP_DIRECTION_STOP_DOWN",
        };
        let px = format_decimals(price, spec.price_decimals);
        let body = serde_json::json!({
            "client_order_id": client_order_id(local),
            "product_id": symbol,
            "side": side_token(Side::Sell),
            "order_configuration": { "stop_limit_stop_limit_gtc": {
                "base_size": format_decimals(base_size, spec.base_decimals),
                "limit_price": px.clone(),
                "stop_price": px,
                "stop_direction": stop_direction,
            }},
        });
        if let Err(e) = self.ensure_cursor(&symbol) {
            self.errors.push(e);
        }
        let value = match self.signed(Method::POST, "/orders", &[], Some(body)) {
            Ok(v) => v,
            Err(e) => return Err(self.refuse(&symbol, local, kind, e)),
        };
        let order_id = match order_result_id(&value) {
            Ok(v) => v,
            Err(e) => return Err(self.refuse(&symbol, local, kind, e)),
        };
        self.map_order(local, &order_id, kind);
        let leg = RestingOrder {
            price,
            base_size,
            side: Side::Sell,
            order_id,
            local,
        };
        let entry = self.protective.entry(symbol).or_default();
        match kind {
            OrderKind::TakeProfit => entry.take_profit = Some(leg),
            _ => entry.stop = Some(leg),
        }
        Ok(Ack::Working(local))
    }
}

impl Wallet<Symbol> for CoinbaseWallet {
    fn funds(&self) -> Reference {
        Reference(self.quote_balance())
    }

    fn position(&self, symbol: &Symbol) -> Units<Symbol> {
        Units {
            symbol: symbol.clone(),
            amount: self.base_balance(symbol),
        }
    }

    /// Every marked product's base balance — the spot holdings we can name a
    /// product id for (a bare currency balance we've never fed a candle for has
    /// no product to key on). Overrides the trait default so a portfolio /
    /// baseline snapshot can enumerate what the account holds.
    fn positions(&self) -> Vec<Units<Symbol>> {
        self.marks
            .keys()
            .filter_map(|symbol| {
                let amount = self.base_balance(symbol);
                (amount.abs() > POSITION_EPSILON).then(|| Units {
                    symbol: symbol.clone(),
                    amount,
                })
            })
            .collect()
    }

    /// `false` — Advanced Trade is **spot**: a position is an owned base-asset
    /// balance, which cannot go negative. [`set_position`](Wallet::set_position)
    /// clamps a negative target to flat and books a [`Rejection`] for the
    /// un-shortable remainder; this lets a caller find that out beforehand.
    fn can_short(&self) -> bool {
        false
    }

    /// The quote currency this wallet was built against — [`DEFAULT_QUOTE_CCY`]
    /// unless [`with_quote_ccy`](CoinbaseWallet::with_quote_ccy) changed it.
    ///
    /// Unlike OKX's, this is genuinely per-account: Advanced Trade quotes the
    /// same base against several currencies, so it is the caller's choice of
    /// product leg rather than a property of the venue. Both
    /// [`funds`](Wallet::funds) and [`equity`](Wallet::equity) are in it.
    fn quote_ccy(&self) -> Option<&str> {
        Some(&self.quote_ccy)
    }

    fn price(&self, symbol: &Symbol) -> Option<Reference> {
        self.marks.get(symbol).map(|&p| Reference(p))
    }

    fn equity(&self) -> Reference {
        // Quote balance plus every marked base balance valued at its last close.
        let mut eq = self.quote_balance();
        for (symbol, &mark) in &self.marks {
            eq += self.base_balance(symbol) * mark;
        }
        Reference(eq)
    }

    fn update(&mut self, symbol: Symbol, candle: Candle) -> Vec<Order<Symbol>> {
        self.marks.insert(symbol.clone(), candle.close);
        if let Err(e) = self.refresh_account() {
            self.errors.push(e);
        }
        if let Err(e) = self.ensure_cursor(&symbol) {
            self.errors.push(e);
            return Vec::new();
        }
        match self.poll_symbol(&symbol) {
            Ok(fills) => fills,
            Err(e) => {
                self.errors.push(e);
                Vec::new()
            }
        }
    }

    fn set_position(&mut self, target: Units<Symbol>) -> Result<Ack<Symbol>, WalletError> {
        let symbol = target.symbol;
        let id = self.mint();
        let spec = match self.ensure_spec(&symbol) {
            Ok(s) => s,
            Err(e) => return Err(self.refuse(&symbol, id, OrderKind::Market, e)),
        };
        let current = self.base_balance(&symbol);
        // Spot can't hold a short: a negative target is clamped to flat, and the
        // un-shortable remainder is reported so the strategy isn't misled.
        let effective_target = target.amount.max(0.0);
        if target.amount < -POSITION_EPSILON {
            self.rejections.push(Rejection {
                symbol: symbol.clone(),
                id,
                error: WalletError::UnsupportedOperation,
                kind: OrderKind::Market,
            });
        }
        let delta = effective_target - current;
        let side = if delta >= 0.0 { Side::Buy } else { Side::Sell };
        let base_size = floor_to_step(delta.abs(), spec.base_increment);
        if base_size < spec.base_min || base_size <= POSITION_EPSILON {
            // Below the venue's minimum tradable size: accept but place nothing.
            return Ok(Ack::Working(id));
        }
        self.place_market(
            &symbol,
            side,
            format_decimals(base_size, spec.base_decimals),
            id,
        )?;
        Ok(Ack::Working(id))
    }

    fn set_stop(
        &mut self,
        symbol: Symbol,
        trigger: Reference,
        size: Size,
    ) -> Result<Ack<Symbol>, WalletError> {
        self.rest_protective(symbol, OrderKind::Stop, trigger.0, size)
    }

    fn set_take_profit(
        &mut self,
        symbol: Symbol,
        trigger: Reference,
        size: Size,
    ) -> Result<Ack<Symbol>, WalletError> {
        self.rest_protective(symbol, OrderKind::TakeProfit, trigger.0, size)
    }

    fn cancel_protective(&mut self, symbol: &Symbol) -> Result<(), WalletError> {
        if let Some(state) = self.protective.remove(symbol) {
            if let Some(leg) = state.stop {
                self.cancel_order(&leg.order_id)?;
            }
            if let Some(leg) = state.take_profit {
                self.cancel_order(&leg.order_id)?;
            }
        }
        Ok(())
    }

    /// Rest a `limit_limit_gtc` order on the venue.
    ///
    /// The [`Size`] resolves at the **limit price** — where the order fills — so
    /// it is the price the target is sized against, matching
    /// [`PaperWallet`](crate::PaperWallet)'s "resolve at the fill" rule. The
    /// order's side comes from the resolved *delta*: `side · size` is an absolute
    /// target, so reducing a long is a sell however the caller spelled it. A
    /// negative delta larger than the held balance is clamped (spot is
    /// reduce-only on the sell side).
    ///
    /// Idempotent per symbol, like the protective legs.
    fn set_limit(
        &mut self,
        symbol: Symbol,
        side: Side,
        size: Size,
        limit: Reference,
    ) -> Result<Ack<Symbol>, WalletError> {
        let local = self.mint();
        if limit.0 <= 0.0 {
            return Err(self.refuse(
                &symbol,
                local,
                OrderKind::Limit,
                LiveError::Decode(format!("limit price must be positive, got {}", limit.0)),
            ));
        }
        let spec = match self.ensure_spec(&symbol) {
            Ok(s) => s,
            Err(e) => return Err(self.refuse(&symbol, local, OrderKind::Limit, e)),
        };
        let current = self.base_balance(&symbol);
        let units = size.resolve(limit.0, current, self.quote_balance(), self.equity().0);
        let mut delta = side.sign() * units - current;
        // Spot can't sell more base than it holds.
        if delta < 0.0 {
            delta = delta.max(-current);
        }
        let order_side = if delta >= 0.0 { Side::Buy } else { Side::Sell };
        let base_size = floor_to_step(delta.abs(), spec.base_increment);
        let price = round_to_tick(limit.0, spec.quote_increment);
        if base_size < spec.base_min || base_size <= POSITION_EPSILON {
            return Ok(Ack::Working(local));
        }

        // Idempotent re-submit: an unchanged order stays where it is.
        if let Some(existing) = self.limits.get(&symbol).cloned() {
            if existing.side == order_side
                && (existing.price - price).abs() <= PRICE_EPSILON
                && (existing.base_size - base_size).abs() <= POSITION_EPSILON
            {
                return Ok(Ack::Working(existing.local));
            }
            let order_id = existing.order_id.clone();
            self.cancel_order(&order_id)?;
            self.limits.remove(&symbol);
        }

        if let Err(e) = self.ensure_cursor(&symbol) {
            self.errors.push(e);
        }
        let body = serde_json::json!({
            "client_order_id": client_order_id(local),
            "product_id": symbol,
            "side": side_token(order_side),
            "order_configuration": { "limit_limit_gtc": {
                "base_size": format_decimals(base_size, spec.base_decimals),
                "limit_price": format_decimals(price, spec.price_decimals),
            }},
        });
        let value = match self.signed(Method::POST, "/orders", &[], Some(body)) {
            Ok(v) => v,
            Err(e) => return Err(self.refuse(&symbol, local, OrderKind::Limit, e)),
        };
        let order_id = match order_result_id(&value) {
            Ok(v) => v,
            Err(e) => return Err(self.refuse(&symbol, local, OrderKind::Limit, e)),
        };
        self.map_order(local, &order_id, OrderKind::Limit);
        self.limits.insert(
            symbol,
            RestingOrder {
                price,
                base_size,
                side: order_side,
                order_id,
                local,
            },
        );
        Ok(Ack::Working(local))
    }

    fn cancel_limit(&mut self, symbol: &Symbol) -> Result<(), WalletError> {
        if let Some(resting) = self.limits.remove(symbol) {
            self.cancel_order(&resting.order_id)?;
        }
        Ok(())
    }

    fn take_rejections(&mut self) -> Vec<Rejection<Symbol>> {
        std::mem::take(&mut self.rejections)
    }

    fn poll_fills(&mut self) -> Vec<Order<Symbol>> {
        let symbols: Vec<String> = self.seen_trades.keys().cloned().collect();
        let mut out = Vec::new();
        for symbol in symbols {
            match self.poll_symbol(&symbol) {
                Ok(mut fills) => out.append(&mut fills),
                Err(e) => self.errors.push(e),
            }
        }
        out
    }

    fn cancel(&mut self, id: OrderId) -> Result<(), WalletError> {
        let Some(venue_id) = self.local_to_venue.get(&id).cloned() else {
            return Ok(());
        };
        if let Some(symbol) = self
            .limits
            .iter()
            .find_map(|(sym, l)| (l.local == id).then(|| sym.clone()))
        {
            self.cancel_order(&venue_id)?;
            self.limits.remove(&symbol);
            return Ok(());
        }
        let leg_symbol = self.protective.iter().find_map(|(sym, state)| {
            let hit = state.stop.as_ref().map(|l| l.local) == Some(id)
                || state.take_profit.as_ref().map(|l| l.local) == Some(id);
            hit.then(|| sym.clone())
        });
        let Some(symbol) = leg_symbol else {
            return Ok(());
        };
        self.cancel_order(&venue_id)?;
        if let Some(state) = self.protective.get_mut(&symbol) {
            if state.stop.as_ref().map(|l| l.local) == Some(id) {
                state.stop = None;
            }
            if state.take_profit.as_ref().map(|l| l.local) == Some(id) {
                state.take_profit = None;
            }
        }
        Ok(())
    }
}

// --- Free helpers ----------------------------------------------------------

/// Parse an EC private key PEM (SEC1 `EC PRIVATE KEY` or PKCS#8 `PRIVATE KEY`)
/// into a P-256 [`SigningKey`]. Accepts the CDP key file's escaped-newline form
/// (`\n` written literally) for convenience.
fn parse_private_key(pem: &str) -> Result<SigningKey, LiveError> {
    use p256::SecretKey;
    use p256::pkcs8::DecodePrivateKey;

    // A CDP key JSON stores the PEM with literal `\n`; restore real newlines if
    // the string carries no actual newline of its own.
    let owned;
    let pem = if pem.contains("\\n") && !pem.contains('\n') {
        owned = pem.replace("\\n", "\n");
        owned.as_str()
    } else {
        pem
    };
    let pem = pem.trim();
    let secret = SecretKey::from_pkcs8_pem(pem)
        .or_else(|_| SecretKey::from_sec1_pem(pem))
        .map_err(|e| LiveError::Decode(format!("invalid EC private key: {e}")))?;
    Ok(SigningKey::from(secret))
}

/// The host portion of a base URL (`https://api.coinbase.com/` →
/// `api.coinbase.com`), for the JWT `uri` claim.
fn host_of(base_url: &str) -> String {
    base_url
        .trim_end_matches('/')
        .split_once("://")
        .map(|(_, rest)| rest)
        .unwrap_or(base_url)
        .to_string()
}

/// Build an ES256 JWT: `base64url(header).base64url(payload).base64url(sig)`,
/// the signature being the JOSE fixed-width `r‖s` ECDSA form over the signing
/// input.
fn build_jwt(
    key_name: &str,
    host: &str,
    method: &str,
    path: &str,
    nonce: &str,
    now: u64,
    key: &SigningKey,
) -> Result<String, LiveError> {
    let header = serde_json::json!({
        "alg": "ES256",
        "kid": key_name,
        "typ": "JWT",
        "nonce": nonce,
    });
    let payload = serde_json::json!({
        "sub": key_name,
        "iss": "cdp",
        "nbf": now,
        "exp": now + JWT_TTL_SECS,
        "uri": format!("{method} {host}{path}"),
    });
    let b64 = base64::engine::general_purpose::URL_SAFE_NO_PAD;
    let header_b64 =
        b64.encode(serde_json::to_vec(&header).map_err(|e| LiveError::Decode(e.to_string()))?);
    let payload_b64 =
        b64.encode(serde_json::to_vec(&payload).map_err(|e| LiveError::Decode(e.to_string()))?);
    let signing_input = format!("{header_b64}.{payload_b64}");
    let sig: Signature = key.sign(signing_input.as_bytes());
    let sig_b64 = b64.encode(sig.to_bytes());
    Ok(format!("{signing_input}.{sig_b64}"))
}

/// The `client_order_id` we tag an order with, so a later poll can correlate.
fn client_order_id(id: OrderId) -> String {
    format!("fugazi{}", id.0)
}

fn side_token(side: Side) -> &'static str {
    match side {
        Side::Buy => "BUY",
        Side::Sell => "SELL",
    }
}

/// Extract the venue `order_id` from a create-order response, mapping a
/// `success == false` (or a `2xx` with an `error_response`) into a [`LiveError`].
fn order_result_id(value: &serde_json::Value) -> Result<String, LiveError> {
    let success = value
        .get("success")
        .and_then(|s| s.as_bool())
        .unwrap_or(false);
    if !success {
        let msg = value
            .get("error_response")
            .and_then(|e| e.get("message").or_else(|| e.get("error")))
            .and_then(|m| m.as_str())
            .or_else(|| value.get("failure_reason").and_then(|m| m.as_str()))
            .unwrap_or("order rejected");
        return Err(LiveError::Http {
            status: 200,
            body: msg.to_string(),
        });
    }
    value
        .get("success_response")
        .and_then(|r| r.get("order_id"))
        .and_then(|v| v.as_str())
        .filter(|s| !s.is_empty())
        .map(|s| s.to_string())
        .or_else(|| {
            value
                .get("order_id")
                .and_then(|v| v.as_str())
                .map(|s| s.to_string())
        })
        .ok_or_else(|| LiveError::Decode("order response missing order_id".into()))
}

/// Pull one product's trading grid out of a `market/products/{id}` response.
fn parse_product_spec(value: &serde_json::Value) -> Option<ProductSpec> {
    let base_str = value.get("base_increment").and_then(|v| v.as_str())?;
    let quote_str = value.get("quote_increment").and_then(|v| v.as_str())?;
    Some(ProductSpec {
        base_increment: base_str.parse::<Real>().ok()?,
        base_min: value
            .get("base_min_size")
            .and_then(parse_num)
            .unwrap_or(0.0),
        quote_increment: quote_str.parse::<Real>().ok()?,
        base_decimals: decimals_of(base_str),
        price_decimals: decimals_of(quote_str),
    })
}

/// One row of the fills endpoint, reduced to what a fill needs.
#[derive(Debug, Clone)]
struct Fill {
    trade_id: String,
    order_id: String,
    /// A monotonic ordering key (the `sequence_timestamp` string, or `trade_id`).
    sequence: String,
    side: Side,
    size: Real,
    price: Real,
    commission: Real,
}

fn parse_fill(v: &serde_json::Value) -> Result<Fill, LiveError> {
    let trade_id = v
        .get("trade_id")
        .and_then(|x| x.as_str())
        .map(|s| s.to_string())
        .ok_or_else(|| LiveError::Decode("fill missing trade_id".into()))?;
    let order_id = v
        .get("order_id")
        .and_then(|x| x.as_str())
        .unwrap_or_default()
        .to_string();
    let sequence = v
        .get("sequence_timestamp")
        .and_then(|x| x.as_str())
        .map(|s| s.to_string())
        .unwrap_or_else(|| trade_id.clone());
    // Coinbase reports `side` as BUY/SELL.
    let side = match v.get("side").and_then(|x| x.as_str()) {
        Some(s) if s.eq_ignore_ascii_case("SELL") => Side::Sell,
        _ => Side::Buy,
    };
    let size = v
        .get("size")
        .and_then(parse_num)
        .ok_or_else(|| LiveError::Decode("fill missing size".into()))?;
    let price = v
        .get("price")
        .and_then(parse_num)
        .ok_or_else(|| LiveError::Decode("fill missing price".into()))?;
    let commission = v
        .get("commission")
        .and_then(parse_num)
        .map(|c| c.max(0.0))
        .unwrap_or(0.0);
    Ok(Fill {
        trade_id,
        order_id,
        sequence,
        side,
        size,
        price,
        commission,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use p256::ecdsa::{VerifyingKey, signature::Verifier};

    /// A throwaway P-256 signing key from a fixed scalar — deterministic, so
    /// tests don't need randomness.
    fn test_key() -> SigningKey {
        let scalar = [7u8; 32];
        SigningKey::from_bytes((&scalar).into()).expect("valid P-256 scalar")
    }

    #[test]
    fn jwt_has_the_expected_structure_and_verifies() {
        let key = test_key();
        let jwt = build_jwt(
            "organizations/o/apiKeys/k",
            "api.coinbase.com",
            "GET",
            "/api/v3/brokerage/accounts",
            "deadbeef",
            1_700_000_000,
            &key,
        )
        .expect("jwt built");

        let parts: Vec<&str> = jwt.split('.').collect();
        assert_eq!(parts.len(), 3, "header.payload.signature");

        let b64 = base64::engine::general_purpose::URL_SAFE_NO_PAD;
        let header: serde_json::Value =
            serde_json::from_slice(&b64.decode(parts[0]).unwrap()).unwrap();
        assert_eq!(header["alg"], "ES256");
        assert_eq!(header["typ"], "JWT");
        assert_eq!(header["kid"], "organizations/o/apiKeys/k");
        assert_eq!(header["nonce"], "deadbeef");

        let payload: serde_json::Value =
            serde_json::from_slice(&b64.decode(parts[1]).unwrap()).unwrap();
        assert_eq!(payload["iss"], "cdp");
        assert_eq!(payload["sub"], "organizations/o/apiKeys/k");
        assert_eq!(payload["nbf"], 1_700_000_000_u64);
        assert_eq!(payload["exp"], 1_700_000_120_u64);
        assert_eq!(
            payload["uri"],
            "GET api.coinbase.com/api/v3/brokerage/accounts"
        );

        // The signature verifies against the public key over the signing input.
        let signing_input = format!("{}.{}", parts[0], parts[1]);
        let sig = Signature::from_slice(&b64.decode(parts[2]).unwrap()).unwrap();
        let vk = VerifyingKey::from(&key);
        assert!(vk.verify(signing_input.as_bytes(), &sig).is_ok());
    }

    #[test]
    fn private_key_pem_round_trips() {
        use p256::SecretKey;
        use p256::pkcs8::EncodePrivateKey;
        let secret = SecretKey::from(test_key());
        let pem = secret.to_pkcs8_pem(Default::default()).unwrap();
        let parsed = parse_private_key(pem.as_str()).expect("pkcs8 pem parses");
        // Same key → same signature over a fixed input.
        let a: Signature = parsed.sign(b"hello");
        let b: Signature = test_key().sign(b"hello");
        assert_eq!(a.to_bytes(), b.to_bytes());
    }

    #[test]
    fn escaped_newline_pem_is_restored() {
        use p256::SecretKey;
        use p256::pkcs8::EncodePrivateKey;
        let secret = SecretKey::from(test_key());
        let pem = secret.to_pkcs8_pem(Default::default()).unwrap();
        let escaped = pem.replace('\n', "\\n");
        assert!(parse_private_key(&escaped).is_ok());
    }

    #[test]
    fn host_is_extracted_from_base_url() {
        assert_eq!(host_of("https://api.coinbase.com"), "api.coinbase.com");
        assert_eq!(host_of("https://api.coinbase.com/"), "api.coinbase.com");
        assert_eq!(host_of("http://127.0.0.1:8080"), "127.0.0.1:8080");
    }

    #[test]
    fn client_order_id_is_stable() {
        assert_eq!(client_order_id(OrderId(42)), "fugazi42");
    }

    #[test]
    fn parses_product_spec() {
        let info = serde_json::json!({
            "product_id": "BTC-USD",
            "base_increment": "0.00000001",
            "quote_increment": "0.01",
            "base_min_size": "0.0001"
        });
        let s = parse_product_spec(&info).expect("spec parsed");
        assert!((s.base_increment - 0.00000001).abs() < 1e-16);
        assert!((s.quote_increment - 0.01).abs() < 1e-12);
        assert!((s.base_min - 0.0001).abs() < 1e-12);
        assert_eq!(s.base_decimals, 8);
        assert_eq!(s.price_decimals, 2);
    }

    #[test]
    fn parses_fill_into_base_shape() {
        let row = serde_json::json!({
            "trade_id": "111", "order_id": "abc", "side": "SELL",
            "size": "2", "price": "27000.5", "commission": "0.27",
            "sequence_timestamp": "2024-01-01T00:00:00Z"
        });
        let f = parse_fill(&row).expect("fill parsed");
        assert_eq!(f.trade_id, "111");
        assert_eq!(f.order_id, "abc");
        assert_eq!(f.side, Side::Sell);
        assert!((f.size - 2.0).abs() < 1e-9);
        assert!((f.price - 27000.5).abs() < 1e-9);
        assert!((f.commission - 0.27).abs() < 1e-9);
    }

    #[test]
    fn order_result_id_reads_success_and_rejects_failure() {
        let ok = serde_json::json!({
            "success": true,
            "success_response": { "order_id": "77" }
        });
        assert_eq!(order_result_id(&ok).unwrap(), "77");

        let refused = serde_json::json!({
            "success": false,
            "error_response": { "message": "insufficient balance" }
        });
        assert!(order_result_id(&refused).is_err());
    }
}
