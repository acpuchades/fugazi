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

use std::collections::HashMap;
use std::time::{SystemTime, UNIX_EPOCH};

use base64::Engine as _;
use p256::ecdsa::{Signature, SigningKey, signature::Signer};
use reqwest::Method;

use crate::types::Symbol;
use crate::types::{Candle, Real};
use crate::wallet::marked_sum;
use crate::wallet::{
    Ack, Order, OrderId, OrderKind, POSITION_EPSILON, Reference, Rejection, Side, Size, Units,
    Wallet, WalletError,
};

use super::LiveError;
use super::venue::{
    CursorModel, InstrumentGrid, LiveCore, OrderClass, VenueBackend, VenueFill, decimals_of, flow,
    parse_num, with_query,
};

const MAINNET_BASE_URL: &str = "https://api.coinbase.com";
const API_PREFIX: &str = "/api/v3/brokerage";
/// The quote currency whose available balance is reported as [`funds`](Wallet::funds).
const DEFAULT_QUOTE_CCY: &str = "USD";
/// How long each signed JWT is valid, in seconds (Coinbase's fixed window).
const JWT_TTL_SECS: u64 = 120;
/// Advanced Trade issues opaque `trade_id`s with no monotone key, so a fill is
/// deduped against the ids already reported. The bound is far above the
/// hundred-row page `GET /orders/historical/fills` returns, which is what makes
/// evicting the oldest safe: a forgotten id can no longer come back in a poll.
const CURSOR_MODEL: CursorModel = CursorModel::SeenIds { capacity: 4096 };

/// A live [`Wallet`] over Coinbase Advanced Trade spot. See the module-level
/// docs for the trait-to-venue mapping and the spot-balance convention.
///
/// Construct with [`mainnet`](Self::mainnet) (**real funds**), then drive it
/// through [`backtest::run`](crate::backtest::run) exactly like a
/// [`PaperWallet`](crate::PaperWallet). Must be used from a synchronous context
/// (it owns a `tokio` runtime and blocks on each REST call).
pub struct CoinbaseWallet {
    /// Everything a venue backend keeps that isn't credentials, signing, or
    /// this venue's own account shape.
    core: LiveCore,
    /// The host used in the JWT `uri` claim (`api.coinbase.com`).
    host: String,
    /// The CDP key name — the JWT `kid` header and `sub` claim.
    key_name: String,
    /// The P-256 signing key parsed from the CDP key's PEM.
    signing_key: SigningKey,
    quote_ccy: String,

    // Cached account state, refreshed from the accounts endpoint.
    balances: HashMap<String, Real>,
    nonce_counter: u64,
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
            core: LiveCore::new(base_url),
            host,
            key_name: key_name.into(),
            signing_key,
            quote_ccy: DEFAULT_QUOTE_CCY.to_string(),
            balances: HashMap::new(),
            nonce_counter: 0,
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
            core: LiveCore::new(MAINNET_BASE_URL),
            host: host_of(MAINNET_BASE_URL),
            key_name: String::new(),
            signing_key,
            quote_ccy: DEFAULT_QUOTE_CCY.to_string(),
            balances: HashMap::new(),
            nonce_counter: 0,
        }
    }

    /// The live errors this wallet has recorded, in order. Every REST failure
    /// (the detail behind a returned [`WalletError::Venue`], plus best-effort
    /// refresh / fill-poll failures that don't have a return channel) is appended
    /// here, so a caller can see *why* a leg failed.
    ///
    /// Bounded: the oldest entries are dropped once the log grows past roughly
    /// twice [`DEFAULT_RETENTION`](crate::wallet::DEFAULT_RETENTION), so a
    /// long-running live process cannot leak through it. A caller who needs the
    /// whole history wants their own durable store, not this accessor.
    pub fn errors(&self) -> &[LiveError] {
        self.core.log().errors()
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
        let url = self.core.http.url(&with_query(&full_path, query));
        let mut req = self
            .core
            .http
            .client()
            .request(method, &url)
            .bearer_auth(jwt)
            .header("Content-Type", "application/json");
        if let Some(b) = body {
            req = req.json(&b);
        }
        self.core.http.send(req)
    }

    /// An unsigned public GET (product specs, etc.). `path` is a full API path.
    fn public_get(&self, path: &str) -> Result<serde_json::Value, LiveError> {
        self.core.http.public_get(path, &[])
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

    /// Cancel a venue order. Advanced Trade has **one** cancel endpoint for
    /// every order type, so the [`OrderClass`] the shared flow passes is not
    /// consulted — but the parameter stays on the hook because OKX needs it.
    ///
    /// The batch endpoint answers `200` with a per-order result; a cancel that
    /// reports failure is treated as already gone, since the post-condition
    /// (that order isn't working) holds either way.
    fn cancel_venue(&mut self, order_id: &str) -> Result<(), LiveError> {
        let body = serde_json::json!({ "order_ids": [order_id] });
        self.signed(Method::POST, "/orders/batch_cancel", &[], Some(body))?;
        Ok(())
    }
}

/// The venue-fact half: Coinbase's endpoints, envelopes and request bodies. The
/// order flow that drives these lives in [`flow`](super::venue::flow) and is
/// shared with every other backend.
impl VenueBackend for CoinbaseWallet {
    fn core(&self) -> &LiveCore {
        &self.core
    }

    fn core_mut(&mut self) -> &mut LiveCore {
        &mut self.core
    }

    fn refresh(&mut self) -> Result<(), LiveError> {
        self.refresh_account()
    }

    fn fetch_grid(&mut self, symbol: &str) -> Result<InstrumentGrid, LiveError> {
        let path = format!("{API_PREFIX}/market/products/{symbol}");
        let value = self.public_get(&path)?;
        parse_product_grid(&value)
            .ok_or_else(|| LiveError::Decode(format!("no product spec for {symbol}")))
    }

    fn cursor_model(&self) -> CursorModel {
        CURSOR_MODEL
    }

    fn fetch_fills(&mut self, symbol: &str) -> Result<Vec<VenueFill>, LiveError> {
        let query = [
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

    fn place_market(
        &mut self,
        symbol: &str,
        side: Side,
        size: Real,
        grid: &InstrumentGrid,
        local: OrderId,
    ) -> Result<String, LiveError> {
        let body = serde_json::json!({
            "client_order_id": client_order_id(local),
            "product_id": symbol,
            "side": side_token(side),
            "order_configuration": {
                "market_market_ioc": { "base_size": grid.size_str(size) },
            },
        });
        let value = self.signed(Method::POST, "/orders", &[], Some(body))?;
        order_result_id(&value)
    }

    fn place_limit(
        &mut self,
        symbol: &str,
        side: Side,
        size: Real,
        price: Real,
        grid: &InstrumentGrid,
        local: OrderId,
    ) -> Result<String, LiveError> {
        let body = serde_json::json!({
            "client_order_id": client_order_id(local),
            "product_id": symbol,
            "side": side_token(side),
            "order_configuration": { "limit_limit_gtc": {
                "base_size": grid.size_str(size),
                "limit_price": grid.price_str(price),
            }},
        });
        let value = self.signed(Method::POST, "/orders", &[], Some(body))?;
        order_result_id(&value)
    }

    /// A `stop_limit_stop_limit_gtc`, with `limit_price == stop_price` so the
    /// order is marketable the moment it triggers.
    ///
    /// **The direction rides in `stop_direction`**, not in which field is set —
    /// the opposite of OKX. A stop triggers on the way *down*, a take-profit on
    /// the way *up*, and `side` is a sell for both: a spot account can only
    /// exit by selling what it holds.
    fn place_protective(
        &mut self,
        symbol: &str,
        kind: OrderKind,
        side: Side,
        size: Real,
        trigger: Real,
        grid: &InstrumentGrid,
        local: OrderId,
    ) -> Result<String, LiveError> {
        let stop_direction = match kind {
            OrderKind::TakeProfit => "STOP_DIRECTION_STOP_UP",
            _ => "STOP_DIRECTION_STOP_DOWN",
        };
        let px = grid.price_str(trigger);
        let body = serde_json::json!({
            "client_order_id": client_order_id(local),
            "product_id": symbol,
            "side": side_token(side),
            "order_configuration": { "stop_limit_stop_limit_gtc": {
                "base_size": grid.size_str(size),
                "limit_price": px.clone(),
                "stop_price": px,
                "stop_direction": stop_direction,
            }},
        });
        let value = self.signed(Method::POST, "/orders", &[], Some(body))?;
        order_result_id(&value)
    }

    fn cancel_venue_order(
        &mut self,
        _symbol: &str,
        venue_id: &str,
        _class: OrderClass,
    ) -> Result<(), LiveError> {
        self.cancel_venue(venue_id)
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
        self.core
            .marked_symbols()
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

    /// `["coinbase"]` — the same venue this wallet trades, and the cleanest of
    /// the pairings: the `coinbase` provider fetches the Advanced Trade candles
    /// endpoint, keyed on the very `product_id` vocabulary this wallet's symbols
    /// already are (`BTC-USD`), for the same spot market.
    ///
    /// It publishes no overlay columns — OHLCV only — and serves fixed cadences
    /// (1m/5m/15m/30m, 1h/2h/6h, 1d).
    fn data_sources(&self) -> &'static [&'static str] {
        &["coinbase"]
    }

    fn price(&self, symbol: &Symbol) -> Option<Reference> {
        self.core.mark(symbol).map(Reference)
    }

    /// The quote balance plus every marked base balance valued at its last
    /// close, summed in the canonical order [`marked_sum`] defines.
    ///
    /// `marks` is a `HashMap`, and folding it in iteration order made this vary
    /// by a ULP between processes on identical inputs — the same drift
    /// [`PaperWallet`](crate::PaperWallet)'s equity was already sorted to avoid.
    /// OKX is immune because its equity is a scalar the venue reports; a spot
    /// account has to value the book itself, so it has to sum it canonically.
    fn equity(&self) -> Reference {
        Reference(marked_sum(
            self.quote_balance(),
            self.core
                .marks()
                .map(|(symbol, mark)| self.base_balance(symbol) * mark),
        ))
    }

    fn update(&mut self, symbol: Symbol, candle: Candle) -> Vec<Order<Symbol>> {
        flow::update(self, symbol, candle)
    }

    fn set_position(&mut self, target: Units<Symbol>) -> Result<Ack<Symbol>, WalletError> {
        flow::set_position(self, target)
    }

    fn set_stop(
        &mut self,
        symbol: Symbol,
        trigger: Reference,
        size: Size,
    ) -> Result<Ack<Symbol>, WalletError> {
        flow::rest_protective(self, symbol, OrderKind::Stop, trigger.0, size)
    }

    fn set_take_profit(
        &mut self,
        symbol: Symbol,
        trigger: Reference,
        size: Size,
    ) -> Result<Ack<Symbol>, WalletError> {
        flow::rest_protective(self, symbol, OrderKind::TakeProfit, trigger.0, size)
    }

    fn cancel_protective(&mut self, symbol: &Symbol) -> Result<(), WalletError> {
        flow::cancel_protective(self, symbol)
    }

    fn set_limit(
        &mut self,
        symbol: Symbol,
        side: Side,
        size: Size,
        limit: Reference,
    ) -> Result<Ack<Symbol>, WalletError> {
        flow::set_limit(self, symbol, side, size, limit)
    }

    fn cancel_limit(&mut self, symbol: &Symbol) -> Result<(), WalletError> {
        flow::cancel_limit(self, symbol)
    }

    fn take_rejections(&mut self) -> Vec<Rejection<Symbol>> {
        self.core.log_mut().take_rejections()
    }

    fn poll_fills(&mut self) -> Vec<Order<Symbol>> {
        flow::poll_fills(self)
    }

    fn cancel(&mut self, id: OrderId) -> Result<(), WalletError> {
        flow::cancel(self, id)
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
fn parse_product_grid(value: &serde_json::Value) -> Option<InstrumentGrid> {
    let base_str = value.get("base_increment").and_then(|v| v.as_str())?;
    let quote_str = value.get("quote_increment").and_then(|v| v.as_str())?;
    Some(InstrumentGrid {
        size_step: base_str.parse::<Real>().ok()?,
        min_size: value
            .get("base_min_size")
            .and_then(parse_num)
            .unwrap_or(0.0),
        price_tick: quote_str.parse::<Real>().ok()?,
        // Spot trades in base units: one venue size unit *is* one base unit, so
        // every contracts↔units conversion on this venue is the identity.
        contract_multiplier: 1.0,
        size_decimals: decimals_of(base_str),
        price_decimals: decimals_of(quote_str),
    })
}

/// One row of the fills endpoint, normalized. `size` is already in base units:
/// spot has no contract wrapper.
fn parse_fill(v: &serde_json::Value) -> Result<VenueFill, LiveError> {
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
    Ok(VenueFill {
        // No monotone key on this venue — `sequence_timestamp` orders the
        // fills, and `trade_id` is what dedupes them.
        ordinal: None,
        id: trade_id,
        sequence,
        order_id,
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
        let g = parse_product_grid(&info).expect("spec parsed");
        assert!((g.size_step - 0.00000001).abs() < 1e-16);
        assert!((g.price_tick - 0.01).abs() < 1e-12);
        assert!((g.min_size - 0.0001).abs() < 1e-12);
        assert_eq!(g.size_decimals, 8);
        assert_eq!(g.price_decimals, 2);
        // Spot: one venue size unit is one base unit.
        assert_eq!(g.contract_multiplier, 1.0);
    }

    #[test]
    fn parses_fill_into_base_shape() {
        let row = serde_json::json!({
            "trade_id": "111", "order_id": "abc", "side": "SELL",
            "size": "2", "price": "27000.5", "commission": "0.27",
            "sequence_timestamp": "2024-01-01T00:00:00Z"
        });
        let f = parse_fill(&row).expect("fill parsed");
        // No monotone key on this venue: the id dedupes, the timestamp orders.
        assert_eq!(f.ordinal, None);
        assert_eq!(f.id, "111");
        assert_eq!(f.sequence, "2024-01-01T00:00:00Z");
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
