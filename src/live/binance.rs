//! A [`Wallet`] implementation backed by Binance USDⓈ-M Futures.
//!
//! [`BinanceFuturesWallet`] routes the trait's order flow to Binance's futures
//! REST API (`/fapi/*`), signing every private call with HMAC-SHA256. It targets
//! **one-way position mode** (the account default), where a symbol carries a
//! single signed position — exactly the [`Units`] shape the trait models, so a
//! long/flat/short strategy maps across without translation.
//!
//! It works unchanged against the free **testnet** (`testnet.binancefuture.com`,
//! keys via GitHub login) and mainnet (`fapi.binance.com`); pick with
//! [`BinanceFuturesWallet::testnet`] / [`mainnet`](BinanceFuturesWallet::mainnet),
//! or point [`with_base_url`](BinanceFuturesWallet::with_base_url) at a mock.
//!
//! ## How the trait maps onto the venue
//!
//! * **Reads** ([`funds`](Wallet::funds) / [`equity`](Wallet::equity) /
//!   [`position`](Wallet::position)) serve a cache refreshed from
//!   `GET /fapi/v2/account` at the top of each [`update`](Wallet::update).
//!   [`price`](Wallet::price) returns the last candle `close` fed in.
//! * **Market moves** ([`set_position`](Wallet::set_position)) diff the target
//!   against the cached position, round to the symbol's `LOT_SIZE`, and
//!   `POST /fapi/v1/order` a `MARKET` order tagged with a `newClientOrderId`
//!   derived from the wallet-minted [`OrderId`]. Submitting returns
//!   [`Ack::Working`]; the fill lands later.
//! * **Protective legs** ([`set_stop`](Wallet::set_stop) /
//!   [`set_take_profit`](Wallet::set_take_profit)) place `reduceOnly`
//!   `STOP_MARKET` / `TAKE_PROFIT_MARKET` orders, **deduped** so an unchanged
//!   trigger re-submitted every bar is a no-op instead of a cancel/replace storm.
//! * **Fills** are polled from `GET /fapi/v1/userTrades` (a per-symbol
//!   `tradeId` cursor). They surface both from [`update`](Wallet::update) (for
//!   the symbol fed) and from [`poll_fills`](Wallet::poll_fills) (for every
//!   symbol we've traded), so a fill on a symbol that didn't tick this bar still
//!   reaches the strategy. Partial fills arrive as several [`Order`]s sharing one
//!   [`OrderId`].
//!
//! REST fill polling is the MVP; a user-data websocket stream is the natural
//! lower-latency follow-up.

use std::collections::HashMap;
use std::time::{SystemTime, UNIX_EPOCH};

use hmac::{Hmac, Mac};
use reqwest::Method;
use sha2::Sha256;

use crate::indicators::DEFAULT_EPSILON;
use crate::types::{Candle, Real};
use crate::wallet::{Ack, Order, OrderId, OrderKind, Reference, Side, Units, Wallet, WalletError};

use super::LiveError;

const TESTNET_BASE_URL: &str = "https://testnet.binancefuture.com";
const MAINNET_BASE_URL: &str = "https://fapi.binance.com";
const DEFAULT_RECV_WINDOW_MS: u64 = 5_000;

/// The exchange filters for one symbol, needed so submitted quantities and
/// stop prices land on the venue's allowed grid (Binance rejects off-grid
/// values). Parsed once from `/fapi/v1/exchangeInfo` and cached.
#[derive(Debug, Clone, Copy)]
struct SymbolFilter {
    step: Real,
    min_qty: Real,
    tick: Real,
    qty_decimals: usize,
    price_decimals: usize,
}

/// A resting protective leg we've placed, kept so a re-submit at the same
/// trigger is a no-op and a changed trigger cancels the previous venue order.
#[derive(Debug, Clone, Copy)]
struct RestingLeg {
    trigger: Real,
    venue_id: i64,
    local: OrderId,
}

#[derive(Debug, Clone, Copy, Default)]
struct ProtectiveState {
    stop: Option<RestingLeg>,
    take_profit: Option<RestingLeg>,
}

/// A live [`Wallet`] over Binance USDⓈ-M Futures. See the [module
/// docs](self) for the trait-to-venue mapping.
///
/// Construct with [`testnet`](Self::testnet) / [`mainnet`](Self::mainnet), then
/// drive it through [`backtest::run`](crate::backtest::run) exactly like a
/// [`PaperWallet`](crate::PaperWallet). Must be used from a synchronous context
/// (it owns a `tokio` runtime and blocks on each REST call).
pub struct BinanceFuturesWallet {
    client: reqwest::Client,
    rt: tokio::runtime::Runtime,
    base_url: String,
    api_key: String,
    api_secret: String,
    recv_window: u64,

    // Cached account state, refreshed from GET /fapi/v2/account.
    available_balance: Real,
    wallet_balance: Real,
    unrealized: Real,
    positions: HashMap<String, Real>,
    marks: HashMap<String, Real>,
    filters: HashMap<String, SymbolFilter>,

    // Order-id bookkeeping: wallet-minted local ids <-> venue order ids, and
    // the kind each venue order was placed as (so a polled fill is tagged
    // Market / Stop / TakeProfit).
    next_id: u64,
    local_to_venue: HashMap<OrderId, i64>,
    venue_to_local: HashMap<i64, OrderId>,
    order_kind: HashMap<i64, OrderKind>,

    // Resting protective legs, for idempotent re-submit / cancel-on-change.
    protective: HashMap<String, ProtectiveState>,

    // Fill polling: per-symbol last-seen tradeId, and the accumulated errors.
    trade_cursor: HashMap<String, i64>,
    errors: Vec<LiveError>,
}

impl BinanceFuturesWallet {
    /// A wallet against the Binance **futures testnet**
    /// (`testnet.binancefuture.com`) with the given API key / secret.
    pub fn testnet(api_key: impl Into<String>, api_secret: impl Into<String>) -> Self {
        Self::with_base_url(TESTNET_BASE_URL, api_key, api_secret)
    }

    /// A wallet against Binance **mainnet** futures (`fapi.binance.com`). This
    /// trades **real funds** — supply live keys deliberately.
    pub fn mainnet(api_key: impl Into<String>, api_secret: impl Into<String>) -> Self {
        Self::with_base_url(MAINNET_BASE_URL, api_key, api_secret)
    }

    /// A wallet against an explicit base URL — mainly to point tests at a
    /// `wiremock` server. Panics only if a `tokio` current-thread runtime can't
    /// be built (out of OS resources).
    pub fn with_base_url(
        base_url: impl Into<String>,
        api_key: impl Into<String>,
        api_secret: impl Into<String>,
    ) -> Self {
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .expect("failed to build a tokio runtime for the live wallet");
        Self {
            client: reqwest::Client::new(),
            rt,
            base_url: base_url.into(),
            api_key: api_key.into(),
            api_secret: api_secret.into(),
            recv_window: DEFAULT_RECV_WINDOW_MS,
            available_balance: 0.0,
            wallet_balance: 0.0,
            unrealized: 0.0,
            positions: HashMap::new(),
            marks: HashMap::new(),
            filters: HashMap::new(),
            next_id: 0,
            local_to_venue: HashMap::new(),
            venue_to_local: HashMap::new(),
            order_kind: HashMap::new(),
            protective: HashMap::new(),
            trade_cursor: HashMap::new(),
            errors: Vec::new(),
        }
    }

    /// Override the signed-request `recvWindow` (milliseconds; Binance default
    /// 5000, max 60000).
    pub fn with_recv_window(mut self, ms: u64) -> Self {
        self.recv_window = ms.min(60_000);
        self
    }

    /// The live errors this wallet has recorded, in order. Every REST failure
    /// (the detail behind a returned [`WalletError::Venue`], plus best-effort
    /// refresh / fill-poll failures that don't have a return channel) is
    /// appended here, so a caller can see *why* a leg failed.
    pub fn errors(&self) -> &[LiveError] {
        &self.errors
    }

    /// Force an account-state refresh (`GET /fapi/v2/account`) now, returning
    /// the [`LiveError`] on failure. [`update`](Wallet::update) calls this
    /// each bar; call it directly for a one-off sync (e.g. right after
    /// construction, before the first bar).
    pub fn refresh_account(&mut self) -> Result<(), LiveError> {
        let value = self.signed(Method::GET, "/fapi/v2/account", Vec::new())?;
        self.available_balance = num_field(&value, "availableBalance").unwrap_or(self.available_balance);
        self.wallet_balance = num_field(&value, "totalWalletBalance").unwrap_or(self.wallet_balance);
        self.unrealized = num_field(&value, "totalUnrealizedProfit").unwrap_or(self.unrealized);
        if let Some(positions) = value.get("positions").and_then(|p| p.as_array()) {
            self.positions.clear();
            for p in positions {
                // One-way mode reports a single BOTH row per symbol.
                if p.get("positionSide").and_then(|s| s.as_str()) == Some("SHORT")
                    || p.get("positionSide").and_then(|s| s.as_str()) == Some("LONG")
                {
                    continue;
                }
                let Some(symbol) = p.get("symbol").and_then(|s| s.as_str()) else {
                    continue;
                };
                let amt = num_field(p, "positionAmt").unwrap_or(0.0);
                if amt.abs() > DEFAULT_EPSILON {
                    self.positions.insert(symbol.to_string(), amt);
                }
            }
        }
        Ok(())
    }

    /// Mint the next unique local [`OrderId`].
    fn mint(&mut self) -> OrderId {
        let id = OrderId(self.next_id);
        self.next_id += 1;
        id
    }

    /// Record a placed order's venue id + kind against a local id.
    fn map_order(&mut self, local: OrderId, venue_id: i64, kind: OrderKind) {
        self.local_to_venue.insert(local, venue_id);
        self.venue_to_local.insert(venue_id, local);
        self.order_kind.insert(venue_id, kind);
    }

    /// Ensure the [`SymbolFilter`] for `symbol` is cached, fetching
    /// `/fapi/v1/exchangeInfo` if not. Errors are logged and surfaced.
    fn ensure_filter(&mut self, symbol: &str) -> Result<SymbolFilter, LiveError> {
        if let Some(f) = self.filters.get(symbol) {
            return Ok(*f);
        }
        let params = vec![("symbol", symbol.to_string())];
        let value = self.public_get("/fapi/v1/exchangeInfo", params)?;
        let filter = parse_symbol_filter(&value, symbol)
            .ok_or_else(|| LiveError::Decode(format!("no filters for symbol {symbol}")))?;
        self.filters.insert(symbol.to_string(), filter);
        Ok(filter)
    }

    /// Ensure a fill cursor exists for `symbol`, seeding it to the latest
    /// existing `tradeId` so we only ever report fills that happen *after* we
    /// started trading it (not the account's whole history).
    fn ensure_cursor(&mut self, symbol: &str) -> Result<(), LiveError> {
        if self.trade_cursor.contains_key(symbol) {
            return Ok(());
        }
        let trades = self.fetch_user_trades(symbol, None)?;
        let max = trades.iter().map(|t| t.id).max().unwrap_or(0);
        self.trade_cursor.insert(symbol.to_string(), max);
        Ok(())
    }

    /// Poll new fills for `symbol` since its cursor, advance the cursor, and
    /// return them as [`Order`]s. A venue order we placed maps back to its local
    /// [`OrderId`] and recorded [`OrderKind`]; a fill on an order we don't know
    /// (placed out-of-band) gets a fresh local id and `Market` kind.
    fn poll_symbol(&mut self, symbol: &str) -> Result<Vec<Order<String>>, LiveError> {
        let cursor = self.trade_cursor.get(symbol).copied().unwrap_or(0);
        let mut trades = self.fetch_user_trades(symbol, Some(cursor + 1))?;
        trades.sort_by_key(|t| t.id);
        let mut out = Vec::new();
        let mut max = cursor;
        for t in trades {
            if t.id <= cursor {
                continue;
            }
            max = max.max(t.id);
            let local = match self.venue_to_local.get(&t.order_id).copied() {
                Some(id) => id,
                None => self.mint(),
            };
            let kind = self.order_kind.get(&t.order_id).copied().unwrap_or(OrderKind::Market);
            let order = Order::new(symbol.to_string(), t.side, t.qty, t.price, kind, local)
                .with_commission(t.commission);
            out.push(order);
        }
        self.trade_cursor.insert(symbol.to_string(), max);
        Ok(out)
    }

    /// Record `err` on the internal log and return the trait-facing
    /// [`WalletError::Venue`] category.
    fn fail(&mut self, err: LiveError) -> WalletError {
        self.errors.push(err);
        WalletError::Venue
    }

    // --- REST plumbing -----------------------------------------------------

    /// A signed private request; blocks on the owned runtime. Params are the
    /// endpoint-specific ones — `recvWindow`, `timestamp`, and `signature` are
    /// appended here.
    fn signed(
        &self,
        method: Method,
        path: &str,
        params: Vec<(&str, String)>,
    ) -> Result<serde_json::Value, LiveError> {
        let fut = signed_request(
            &self.client,
            &self.base_url,
            &self.api_key,
            &self.api_secret,
            self.recv_window,
            method,
            path,
            params,
        );
        self.rt.block_on(fut)
    }

    /// An unsigned public GET (exchange info, etc.).
    fn public_get(
        &self,
        path: &str,
        params: Vec<(&str, String)>,
    ) -> Result<serde_json::Value, LiveError> {
        let url = format!("{}{}", self.base_url.trim_end_matches('/'), path);
        let fut = async {
            let resp = self
                .client
                .get(&url)
                .query(&params)
                .send()
                .await
                .map_err(|e| LiveError::Network(e.to_string()))?;
            read_json(resp).await
        };
        self.rt.block_on(fut)
    }

    /// Fetch user trades for `symbol`, optionally from `from_id` inclusive.
    fn fetch_user_trades(
        &self,
        symbol: &str,
        from_id: Option<i64>,
    ) -> Result<Vec<UserTrade>, LiveError> {
        let mut params = vec![("symbol", symbol.to_string())];
        if let Some(id) = from_id {
            params.push(("fromId", id.to_string()));
        }
        let value = self.signed(Method::GET, "/fapi/v1/userTrades", params)?;
        let rows = value
            .as_array()
            .ok_or_else(|| LiveError::Decode("userTrades is not a JSON array".into()))?;
        rows.iter().map(parse_user_trade).collect()
    }

    /// Cancel a venue order by id on `symbol`, treating "unknown order" (code
    /// -2011) as success — the post-condition (that order isn't working) holds.
    fn cancel_venue(&mut self, symbol: &str, venue_id: i64) -> Result<(), WalletError> {
        let params = vec![
            ("symbol", symbol.to_string()),
            ("orderId", venue_id.to_string()),
        ];
        match self.signed(Method::DELETE, "/fapi/v1/order", params) {
            Ok(_) => Ok(()),
            Err(LiveError::Http { status, body }) if body.contains("-2011") => {
                let _ = status;
                Ok(())
            }
            Err(e) => Err(self.fail(e)),
        }
    }

    /// Place a `reduceOnly` protective order (`STOP_MARKET` or
    /// `TAKE_PROFIT_MARKET`) and record it. Deduped by the caller.
    fn place_protective(
        &mut self,
        symbol: &str,
        kind: OrderKind,
        trigger: Real,
    ) -> Result<RestingLeg, WalletError> {
        let filter = self.ensure_filter(symbol).map_err(|e| self.fail(e))?;
        let pos = self.positions.get(symbol).copied().unwrap_or(0.0);
        if pos.abs() <= DEFAULT_EPSILON {
            // Nothing to protect; treat as a no-op resting leg record.
            return Err(WalletError::Venue);
        }
        // A protective exit trades the opposite side of the open position.
        let side = if pos > 0.0 { Side::Sell } else { Side::Buy };
        let type_token = match kind {
            OrderKind::Stop => "STOP_MARKET",
            OrderKind::TakeProfit => "TAKE_PROFIT_MARKET",
            OrderKind::Market => "STOP_MARKET",
        };
        if let Err(e) = self.ensure_cursor(symbol) {
            self.errors.push(e);
        }
        let local = self.mint();
        let params = vec![
            ("symbol", symbol.to_string()),
            ("side", side_token(side).to_string()),
            ("type", type_token.to_string()),
            (
                "stopPrice",
                format_decimals(round_to_tick(trigger, filter.tick), filter.price_decimals),
            ),
            ("closePosition", "true".to_string()),
            ("newClientOrderId", client_order_id(local)),
        ];
        let value = self
            .signed(Method::POST, "/fapi/v1/order", params)
            .map_err(|e| self.fail(e))?;
        let venue_id = value
            .get("orderId")
            .and_then(|v| v.as_i64())
            .ok_or(WalletError::Venue)?;
        self.map_order(local, venue_id, kind);
        Ok(RestingLeg { trigger, venue_id, local })
    }

    /// Rest a protective leg with idempotent dedup: an unchanged trigger is a
    /// no-op (returns the existing leg's id); a changed trigger cancels the old
    /// venue order before placing the new one.
    fn rest_protective(
        &mut self,
        symbol: String,
        kind: OrderKind,
        trigger: Real,
    ) -> Result<Ack<String>, WalletError> {
        let existing = self.protective.get(&symbol).and_then(|p| match kind {
            OrderKind::TakeProfit => p.take_profit,
            _ => p.stop,
        });
        if let Some(leg) = existing {
            if (leg.trigger - trigger).abs() <= DEFAULT_EPSILON {
                return Ok(Ack::Working(leg.local));
            }
            self.cancel_venue(&symbol, leg.venue_id)?;
        }
        let leg = self.place_protective(&symbol, kind, trigger)?;
        let entry = self.protective.entry(symbol).or_default();
        match kind {
            OrderKind::TakeProfit => entry.take_profit = Some(leg),
            _ => entry.stop = Some(leg),
        }
        Ok(Ack::Working(leg.local))
    }
}

impl Wallet<String> for BinanceFuturesWallet {
    fn funds(&self) -> Reference {
        Reference(self.available_balance)
    }

    fn position(&self, symbol: &String) -> Units<String> {
        Units {
            symbol: symbol.clone(),
            amount: self.positions.get(symbol).copied().unwrap_or(0.0),
        }
    }

    fn price(&self, symbol: &String) -> Option<Reference> {
        self.marks.get(symbol).map(|&p| Reference(p))
    }

    fn equity(&self) -> Reference {
        Reference(self.wallet_balance + self.unrealized)
    }

    fn update(&mut self, symbol: String, candle: Candle) -> Vec<Order<String>> {
        self.marks.insert(symbol.clone(), candle.close);
        // Refresh account state best-effort; a failure just leaves the cache
        // stale (logged) rather than breaking the bar.
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

    fn set_position(&mut self, target: Units<String>) -> Result<Ack<String>, WalletError> {
        let symbol = target.symbol;
        let filter = self.ensure_filter(&symbol).map_err(|e| self.fail(e))?;
        let current = self.positions.get(&symbol).copied().unwrap_or(0.0);
        let delta = target.amount - current;
        let qty = floor_to_step(delta.abs(), filter.step);
        let id = self.mint();
        if qty < filter.min_qty || qty <= DEFAULT_EPSILON {
            // Below the venue's minimum tradable size: accept the submission
            // but place nothing (no fill will arrive under this id).
            return Ok(Ack::Working(id));
        }
        // Seed the fill cursor to the pre-trade max *before* placing, so a
        // market order that fills immediately is caught by the next poll rather
        // than skipped by a cursor advanced past its own fill.
        if let Err(e) = self.ensure_cursor(&symbol) {
            self.errors.push(e);
        }
        let side = if delta > 0.0 { Side::Buy } else { Side::Sell };
        let params = vec![
            ("symbol", symbol.clone()),
            ("side", side_token(side).to_string()),
            ("type", "MARKET".to_string()),
            ("quantity", format_decimals(qty, filter.qty_decimals)),
            ("newClientOrderId", client_order_id(id)),
        ];
        let value = self
            .signed(Method::POST, "/fapi/v1/order", params)
            .map_err(|e| self.fail(e))?;
        let venue_id = value
            .get("orderId")
            .and_then(|v| v.as_i64())
            .ok_or(WalletError::Venue)?;
        self.map_order(id, venue_id, OrderKind::Market);
        Ok(Ack::Working(id))
    }

    fn set_stop(&mut self, symbol: String, trigger: Reference) -> Result<Ack<String>, WalletError> {
        self.rest_protective(symbol, OrderKind::Stop, trigger.0)
    }

    fn set_take_profit(
        &mut self,
        symbol: String,
        trigger: Reference,
    ) -> Result<Ack<String>, WalletError> {
        self.rest_protective(symbol, OrderKind::TakeProfit, trigger.0)
    }

    fn cancel_protective(&mut self, symbol: &String) -> Result<(), WalletError> {
        if let Some(state) = self.protective.remove(symbol) {
            if let Some(leg) = state.stop {
                self.cancel_venue(symbol, leg.venue_id)?;
            }
            if let Some(leg) = state.take_profit {
                self.cancel_venue(symbol, leg.venue_id)?;
            }
        }
        Ok(())
    }

    fn poll_fills(&mut self) -> Vec<Order<String>> {
        let symbols: Vec<String> = self.trade_cursor.keys().cloned().collect();
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
        // Find the venue order and the symbol it belongs to (via the resting
        // protective records; a working market order fills near-instantly and
        // isn't tracked for cancel).
        let venue_id = match self.local_to_venue.get(&id).copied() {
            Some(v) => v,
            None => return Ok(()),
        };
        let symbol = self.protective.iter().find_map(|(sym, state)| {
            let hit = state.stop.map(|l| l.local) == Some(id)
                || state.take_profit.map(|l| l.local) == Some(id);
            hit.then(|| sym.clone())
        });
        let Some(symbol) = symbol else {
            // We know the venue id but not which symbol's resting bracket it
            // is (e.g. a market order): nothing actionable, treat as gone.
            return Ok(());
        };
        self.cancel_venue(&symbol, venue_id)?;
        if let Some(state) = self.protective.get_mut(&symbol) {
            if state.stop.map(|l| l.local) == Some(id) {
                state.stop = None;
            }
            if state.take_profit.map(|l| l.local) == Some(id) {
                state.take_profit = None;
            }
        }
        Ok(())
    }
}

// --- Free helpers ----------------------------------------------------------

type HmacSha256 = Hmac<Sha256>;

/// Milliseconds since the Unix epoch, for the `timestamp` parameter.
fn now_ms() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as i64)
        .unwrap_or(0)
}

/// HMAC-SHA256 of `query` under `secret`, hex-encoded — Binance's signature.
fn sign(secret: &str, query: &str) -> String {
    let mut mac = HmacSha256::new_from_slice(secret.as_bytes())
        .expect("HMAC accepts a key of any length");
    mac.update(query.as_bytes());
    hex::encode(mac.finalize().into_bytes())
}

/// The `newClientOrderId` we tag an order with, so a later poll can correlate.
fn client_order_id(id: OrderId) -> String {
    format!("fugazi-{}", id.0)
}

fn side_token(side: Side) -> &'static str {
    match side {
        Side::Buy => "BUY",
        Side::Sell => "SELL",
    }
}

/// Build, sign, and send a private request, returning the parsed JSON body.
///
/// The signature covers the exact query string we send (params in insertion
/// order, then `recvWindow` + `timestamp`), so the request carries that same
/// string verbatim in the URL — avoiding any client-side re-encoding mismatch.
#[allow(clippy::too_many_arguments)]
async fn signed_request(
    client: &reqwest::Client,
    base_url: &str,
    api_key: &str,
    api_secret: &str,
    recv_window: u64,
    method: Method,
    path: &str,
    mut params: Vec<(&str, String)>,
) -> Result<serde_json::Value, LiveError> {
    params.push(("recvWindow", recv_window.to_string()));
    params.push(("timestamp", now_ms().to_string()));
    let query = params
        .iter()
        .map(|(k, v)| format!("{k}={v}"))
        .collect::<Vec<_>>()
        .join("&");
    let signature = sign(api_secret, &query);
    let url = format!(
        "{}{}?{}&signature={}",
        base_url.trim_end_matches('/'),
        path,
        query,
        signature
    );
    let resp = client
        .request(method, &url)
        .header("X-MBX-APIKEY", api_key)
        .send()
        .await
        .map_err(|e| LiveError::Network(e.to_string()))?;
    read_json(resp).await
}

/// Read a response body, mapping a non-2xx status into [`LiveError::Http`].
async fn read_json(resp: reqwest::Response) -> Result<serde_json::Value, LiveError> {
    let status = resp.status();
    let body = resp
        .text()
        .await
        .map_err(|e| LiveError::Network(e.to_string()))?;
    if !status.is_success() {
        return Err(LiveError::Http {
            status: status.as_u16(),
            body,
        });
    }
    if body.is_empty() {
        return Ok(serde_json::Value::Null);
    }
    serde_json::from_str(&body).map_err(|e| LiveError::Decode(e.to_string()))
}

/// A Binance number that may be a JSON string (`"27000.50"`) or a bare number.
fn parse_num(v: &serde_json::Value) -> Option<Real> {
    match v {
        serde_json::Value::String(s) => s.parse::<Real>().ok(),
        serde_json::Value::Number(n) => n.as_f64(),
        _ => None,
    }
}

/// Read a named numeric field off a JSON object (string-or-number).
fn num_field(value: &serde_json::Value, key: &str) -> Option<Real> {
    value.get(key).and_then(parse_num)
}

/// Round a quantity **down** to a multiple of `step` (so we never submit more
/// than the diff we intend). A non-positive step leaves the value untouched.
fn floor_to_step(value: Real, step: Real) -> Real {
    if step <= 0.0 {
        return value;
    }
    (value / step).floor() * step
}

/// Round a price to the nearest multiple of `tick` (the venue's price grid). A
/// non-positive tick leaves the value untouched.
fn round_to_tick(value: Real, tick: Real) -> Real {
    if tick <= 0.0 {
        return value;
    }
    (value / tick).round() * tick
}

/// Format a value with a fixed number of decimals — the string form Binance
/// wants for quantity / price (no scientific notation, matches the filter grid).
fn format_decimals(value: Real, decimals: usize) -> String {
    format!("{value:.decimals$}")
}

/// Count the significant decimal places in a Binance step/tick string
/// (`"0.001"` → 3, `"1"` → 0), the precision to format that field to.
fn decimals_of(s: &str) -> usize {
    match s.split_once('.') {
        Some((_, frac)) => frac.trim_end_matches('0').len(),
        None => 0,
    }
}

/// Pull one symbol's `LOT_SIZE` + `PRICE_FILTER` out of `/fapi/v1/exchangeInfo`.
fn parse_symbol_filter(value: &serde_json::Value, symbol: &str) -> Option<SymbolFilter> {
    let symbols = value.get("symbols")?.as_array()?;
    let entry = symbols
        .iter()
        .find(|s| s.get("symbol").and_then(|v| v.as_str()) == Some(symbol))?;
    let filters = entry.get("filters")?.as_array()?;
    let mut step = None;
    let mut min_qty = None;
    let mut qty_decimals = 0;
    let mut tick = None;
    let mut price_decimals = 0;
    for f in filters {
        match f.get("filterType").and_then(|v| v.as_str()) {
            Some("LOT_SIZE") | Some("MARKET_LOT_SIZE") => {
                if let Some(s) = f.get("stepSize").and_then(|v| v.as_str()) {
                    step = s.parse::<Real>().ok();
                    qty_decimals = decimals_of(s);
                }
                min_qty = f.get("minQty").and_then(parse_num).or(min_qty);
            }
            Some("PRICE_FILTER") => {
                if let Some(s) = f.get("tickSize").and_then(|v| v.as_str()) {
                    tick = s.parse::<Real>().ok();
                    price_decimals = decimals_of(s);
                }
            }
            _ => {}
        }
    }
    Some(SymbolFilter {
        step: step?,
        min_qty: min_qty.unwrap_or(0.0),
        tick: tick.unwrap_or(0.0),
        qty_decimals,
        price_decimals,
    })
}

/// One row of `GET /fapi/v1/userTrades`, reduced to what a fill needs.
#[derive(Debug, Clone, Copy)]
struct UserTrade {
    id: i64,
    order_id: i64,
    side: Side,
    qty: Real,
    price: Real,
    commission: Real,
}

fn parse_user_trade(v: &serde_json::Value) -> Result<UserTrade, LiveError> {
    let id = v
        .get("id")
        .and_then(|x| x.as_i64())
        .ok_or_else(|| LiveError::Decode("userTrade missing id".into()))?;
    let order_id = v.get("orderId").and_then(|x| x.as_i64()).unwrap_or(-1);
    // Binance marks the trade's aggressor side either as an explicit "side"
    // field (futures) or via the "buyer" flag; prefer the explicit field.
    let side = match v.get("side").and_then(|x| x.as_str()) {
        Some("BUY") => Side::Buy,
        Some("SELL") => Side::Sell,
        _ => match v.get("buyer").and_then(|x| x.as_bool()) {
            Some(true) => Side::Buy,
            _ => Side::Sell,
        },
    };
    let qty = num_field(v, "qty")
        .ok_or_else(|| LiveError::Decode("userTrade missing qty".into()))?;
    let price = num_field(v, "price")
        .ok_or_else(|| LiveError::Decode("userTrade missing price".into()))?;
    let commission = num_field(v, "commission").unwrap_or(0.0);
    Ok(UserTrade {
        id,
        order_id,
        side,
        qty,
        price,
        commission,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn signature_matches_known_vector() {
        // Binance's documented example: secret + query -> HMAC-SHA256 hex.
        let secret = "NhqPtmdSJYdKjVHjA7PZj4Mge3R5YNiP1e3UZjInClVN65XAbvqqM6A7H5fATj0j";
        let query = "symbol=LTCBTC&side=BUY&type=LIMIT&timeInForce=GTC&quantity=1&price=0.1&recvWindow=5000&timestamp=1499827319559";
        assert_eq!(
            sign(secret, query),
            "c8db56825ae71d6d79447849e617115f4a920fa2acdcab2b053c4b2838bd6b71"
        );
    }

    #[test]
    fn client_order_id_encodes_local_id() {
        assert_eq!(client_order_id(OrderId(42)), "fugazi-42");
    }

    #[test]
    fn decimals_and_step_rounding() {
        assert_eq!(decimals_of("0.001"), 3);
        assert_eq!(decimals_of("1"), 0);
        assert_eq!(decimals_of("0.10"), 1);
        assert!((floor_to_step(0.0037, 0.001) - 0.003).abs() < 1e-9);
        assert_eq!(format_decimals(0.003, 3), "0.003");
        assert_eq!(format_decimals(27000.5, 2), "27000.50");
    }

    #[test]
    fn parses_lot_and_price_filters() {
        let info = serde_json::json!({
            "symbols": [{
                "symbol": "BTCUSDT",
                "filters": [
                    {"filterType": "PRICE_FILTER", "tickSize": "0.10"},
                    {"filterType": "LOT_SIZE", "stepSize": "0.001", "minQty": "0.001"}
                ]
            }]
        });
        let f = parse_symbol_filter(&info, "BTCUSDT").expect("filter parsed");
        assert!((f.step - 0.001).abs() < 1e-12);
        assert!((f.min_qty - 0.001).abs() < 1e-12);
        assert_eq!(f.qty_decimals, 3);
        assert_eq!(f.price_decimals, 1);
        assert!(parse_symbol_filter(&info, "ETHUSDT").is_none());
    }

    #[test]
    fn parses_user_trade_into_fill_shape() {
        let row = serde_json::json!({
            "id": 88, "orderId": 42, "side": "SELL",
            "qty": "0.500", "price": "27000.50", "commission": "0.27", "commissionAsset": "USDT"
        });
        let t = parse_user_trade(&row).expect("trade parsed");
        assert_eq!(t.id, 88);
        assert_eq!(t.order_id, 42);
        assert_eq!(t.side, Side::Sell);
        assert!((t.qty - 0.5).abs() < 1e-9);
        assert!((t.price - 27000.50).abs() < 1e-9);
        assert!((t.commission - 0.27).abs() < 1e-9);
    }
}
