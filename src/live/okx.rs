//! A [`Wallet`] implementation backed by OKX V5 perpetual swaps.
//!
//! [`OkxWallet`] routes the trait's order flow to OKX's unified-account REST API
//! (`/api/v5/*`), signing every private call as
//! `base64(HMAC-SHA256(secret, timestamp + method + requestPath + body))` under
//! an `OK-ACCESS-*` header set (the scheme OKX documents). It targets
//! **net position mode** (the account default), where a swap carries a single
//! signed position — exactly the [`Units`] shape the trait models, so a
//! long/flat/short strategy maps across without translation.
//!
//! It works unchanged against OKX's free **demo trading** environment and
//! against production; both share one host (`https://www.okx.com`) and are
//! selected by a header (`x-simulated-trading: 1` for demo). Construct with
//! [`OkxWallet::demo`] / [`mainnet`](OkxWallet::mainnet), or point
//! [`with_base_url`](OkxWallet::with_base_url) at a mock.
//!
//! ## Contracts vs. base units — the one real translation
//!
//! OKX sizes a swap in **contracts**, not in the underlying asset: one
//! `BTC-USDT-SWAP` contract is `ctVal = 0.01 BTC`. The [`Wallet`] trait — and
//! every strategy driven through it — speaks in **base-asset units** (the same
//! units a [`PaperWallet`](crate::PaperWallet) and every backtest use). So this
//! wallet converts at the boundary: positions and fills the venue reports in
//! contracts are multiplied by `ctVal` into base units before they enter the
//! cache or a fill [`Order`], and a target the strategy expresses in base units
//! is divided by `ctVal` (then floored to the instrument's `lotSz`) into the
//! contract count actually submitted. Everything the trait exposes stays in base
//! units; contracts live only inside the REST calls.
//!
//! ## How the trait maps onto the venue
//!
//! * **Reads** ([`funds`](Wallet::funds) / [`equity`](Wallet::equity) /
//!   [`position`](Wallet::position)) serve a cache refreshed from
//!   `GET /api/v5/account/balance` and `GET /api/v5/account/positions` at the top
//!   of each [`update`](Wallet::update). [`price`](Wallet::price) returns the last
//!   candle `close` fed in.
//! * **Market moves** ([`set_position`](Wallet::set_position)) diff the target
//!   against the cached position, convert to contracts, round to the
//!   instrument's `lotSz`, and `POST /api/v5/trade/order` a `market` order tagged
//!   with a `clOrdId` derived from the wallet-minted [`OrderId`]. Submitting
//!   returns [`Ack::Working`]; the fill lands later.
//! * **Protective legs** ([`set_stop`](Wallet::set_stop) /
//!   [`set_take_profit`](Wallet::set_take_profit)) place `reduceOnly` conditional
//!   algo orders via `POST /api/v5/trade/order-algo` (`slTriggerPx` / `tpTriggerPx`
//!   with a `-1` market order price), **deduped** so an unchanged trigger
//!   re-submitted every bar is a no-op instead of a cancel/replace storm.
//! * **Fills** are polled from `GET /api/v5/trade/fills` (a per-symbol `billId`
//!   cursor). They surface both from [`update`](Wallet::update) (for the symbol
//!   fed) and from [`poll_fills`](Wallet::poll_fills) (for every symbol we've
//!   traded), so a fill on a symbol that didn't tick this bar still reaches the
//!   strategy. Partial fills arrive as several [`Order`]s sharing one [`OrderId`].
//! * **Refusals** — an order the venue rejects — return the
//!   [`WalletError::Venue`] category *and* are buffered onto the trait's failure
//!   stream, drained by [`take_rejections`](Wallet::take_rejections) and routed
//!   to [`Strategy::on_reject`](crate::Strategy::on_reject) by the driver, so a
//!   rejected entry/exit doesn't silently desync the strategy's view of its
//!   position. The full error detail also lands on [`errors`](OkxWallet::errors).
//!
//! REST fill polling is the MVP; a WebSocket user-data stream is the natural
//! lower-latency follow-up.

use std::collections::HashMap;

use base64::Engine as _;
use hmac::{Hmac, KeyInit, Mac};
use reqwest::Method;
use sha2::Sha256;
use time::OffsetDateTime;
use time::macros::format_description;

use crate::wallet::{POSITION_EPSILON, PRICE_EPSILON};
use crate::types::Symbol;
use crate::types::{Candle, Real};
use crate::wallet::{Ack, Order, OrderId, OrderKind, Reference, Rejection, Side, Size, Units, Wallet, WalletError};

use super::LiveError;
use super::venue::{decimals_of, floor_to_step, format_decimals, parse_num, round_to_tick, with_query};

const MAINNET_BASE_URL: &str = "https://www.okx.com";
/// The margin / quote currency a linear USDⓈ-M swap settles in — the balance
/// whose `availBal` we report as [`funds`](Wallet::funds).
const QUOTE_CCY: &str = "USDT";
/// OKX's sentinel for "execute the triggered protective order at market".
const MARKET_ORDER_PX: &str = "-1";

/// The instrument spec for one swap, needed so submitted sizes and trigger
/// prices land on the venue's grid and so contracts convert to base units.
/// Parsed once from `/api/v5/public/instruments` and cached.
#[derive(Debug, Clone, Copy)]
struct InstrumentSpec {
    /// Size step, in contracts.
    lot_sz: Real,
    /// Minimum order size, in contracts.
    min_sz: Real,
    /// Price step.
    tick: Real,
    /// Base-asset value of one contract (`ctVal`) — the contracts↔units factor.
    ct_val: Real,
    sz_decimals: usize,
    px_decimals: usize,
}

/// A resting protective algo leg we've placed, kept so a re-submit at the same
/// trigger + size is a no-op and a change cancels the previous venue order.
#[derive(Debug, Clone)]
struct RestingLeg {
    trigger: Real,
    /// Resolved size in **contracts** — part of the dedup key so re-resting the
    /// same trigger for a *different* share replaces the venue order rather than
    /// being mistaken for a no-op.
    contracts: Real,
    algo_id: String,
    local: OrderId,
}

#[derive(Debug, Clone, Default)]
struct ProtectiveState {
    stop: Option<RestingLeg>,
    take_profit: Option<RestingLeg>,
}

/// A resting limit order we've placed, kept for the same reasons as
/// [`RestingLeg`]: an unchanged re-submit is a no-op, a changed one cancels the
/// previous venue order before placing the replacement.
#[derive(Debug, Clone)]
struct RestingLimit {
    limit: Real,
    /// Size in contracts.
    contracts: Real,
    side: Side,
    ord_id: String,
    local: OrderId,
}

/// A live [`Wallet`] over OKX V5 perpetual swaps. See the module-level docs
/// for the trait-to-venue mapping and the contracts↔units convention.
///
/// Construct with [`demo`](Self::demo) / [`mainnet`](Self::mainnet), then drive
/// it through [`backtest::run`](crate::backtest::run) exactly like a
/// [`PaperWallet`](crate::PaperWallet). Must be used from a synchronous context
/// (it owns a `tokio` runtime and blocks on each REST call).
pub struct OkxWallet {
    client: reqwest::Client,
    rt: tokio::runtime::Runtime,
    base_url: String,
    api_key: String,
    api_secret: String,
    passphrase: String,
    /// `true` routes to OKX demo trading via the `x-simulated-trading` header.
    simulated: bool,
    /// `cross` or `isolated` — the margin mode every order is placed under.
    td_mode: String,

    // Cached account state, refreshed from the account endpoints.
    available_balance: Real,
    equity: Real,
    /// Signed positions in **base units** (converted from the venue's contracts).
    positions: HashMap<Symbol, Real>,
    marks: HashMap<Symbol, Real>,
    specs: HashMap<Symbol, InstrumentSpec>,

    // Order-id bookkeeping: wallet-minted local ids <-> venue order/algo ids,
    // and the kind each venue order was placed as (so a polled fill is tagged
    // Market / Stop / TakeProfit / Limit).
    next_id: u64,
    local_to_venue: HashMap<OrderId, String>,
    venue_to_local: HashMap<String, OrderId>,
    order_kind: HashMap<String, OrderKind>,

    // Resting protective legs, for idempotent re-submit / cancel-on-change.
    protective: HashMap<Symbol, ProtectiveState>,
    // Resting limit orders, one per symbol — same convention as `protective`.
    limits: HashMap<Symbol, RestingLimit>,

    // Fill polling: per-symbol last-seen billId, and the accumulated errors.
    trade_cursor: HashMap<Symbol, i64>,
    errors: Vec<LiveError>,
    // Refused orders awaiting a drain through take_rejections (the trait's
    // failure stream — the twin of the fill stream update()/poll_fills return).
    rejections: Vec<Rejection<Symbol>>,
}

impl OkxWallet {
    /// A wallet against OKX **demo trading** (production host, but every request
    /// carries the `x-simulated-trading: 1` header so it books against the paper
    /// environment). Needs demo API credentials — key, secret, and the
    /// passphrase set when the key was created.
    pub fn demo(
        api_key: impl Into<String>,
        api_secret: impl Into<String>,
        passphrase: impl Into<String>,
    ) -> Self {
        let mut w = Self::with_base_url(MAINNET_BASE_URL, api_key, api_secret, passphrase);
        w.simulated = true;
        w
    }

    /// A wallet against OKX **production** (`www.okx.com`). This trades **real
    /// funds** — supply live keys deliberately.
    pub fn mainnet(
        api_key: impl Into<String>,
        api_secret: impl Into<String>,
        passphrase: impl Into<String>,
    ) -> Self {
        Self::with_base_url(MAINNET_BASE_URL, api_key, api_secret, passphrase)
    }

    /// A wallet against an explicit base URL — mainly to point tests at a
    /// `wiremock` server. Panics only if a `tokio` current-thread runtime can't
    /// be built (out of OS resources).
    pub fn with_base_url(
        base_url: impl Into<String>,
        api_key: impl Into<String>,
        api_secret: impl Into<String>,
        passphrase: impl Into<String>,
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
            passphrase: passphrase.into(),
            simulated: false,
            td_mode: "cross".to_string(),
            available_balance: 0.0,
            equity: 0.0,
            positions: HashMap::new(),
            marks: HashMap::new(),
            specs: HashMap::new(),
            next_id: 0,
            local_to_venue: HashMap::new(),
            venue_to_local: HashMap::new(),
            order_kind: HashMap::new(),
            protective: HashMap::new(),
            limits: HashMap::new(),
            trade_cursor: HashMap::new(),
            errors: Vec::new(),
            rejections: Vec::new(),
        }
    }

    /// Override the margin mode orders are placed under (`cross` — the default —
    /// or `isolated`).
    pub fn with_td_mode(mut self, td_mode: impl Into<String>) -> Self {
        self.td_mode = td_mode.into();
        self
    }

    /// The live errors this wallet has recorded, in order. Every REST failure
    /// (the detail behind a returned [`WalletError::Venue`], plus best-effort
    /// refresh / fill-poll failures that don't have a return channel) is
    /// appended here, so a caller can see *why* a leg failed.
    pub fn errors(&self) -> &[LiveError] {
        &self.errors
    }

    /// Force an account-state refresh (balance + positions) now, returning the
    /// [`LiveError`] on failure. [`update`](Wallet::update) calls this each bar;
    /// call it directly for a one-off sync (e.g. right after construction,
    /// before the first bar).
    pub fn refresh_account(&mut self) -> Result<(), LiveError> {
        let balance = self.signed(Method::GET, "/api/v5/account/balance", &[], None)?;
        let data = ok_data(&balance)?;
        if let Some(row) = data.first() {
            self.equity = num_field(row, "totalEq").unwrap_or(self.equity);
            if let Some(details) = row.get("details").and_then(|d| d.as_array()) {
                for d in details {
                    if d.get("ccy").and_then(|c| c.as_str()) == Some(QUOTE_CCY) {
                        self.available_balance =
                            num_field(d, "availBal").unwrap_or(self.available_balance);
                    }
                }
            }
        }

        let positions =
            self.signed(Method::GET, "/api/v5/account/positions", &[("instType", "SWAP".into())], None)?;
        let rows = ok_data(&positions)?;
        self.positions.clear();
        for p in &rows {
            let Some(inst) = p.get("instId").and_then(|s| s.as_str()) else {
                continue;
            };
            let contracts = num_field(p, "pos").unwrap_or(0.0);
            if contracts.abs() <= POSITION_EPSILON {
                continue;
            }
            // Convert contracts to base units; needs the instrument's ctVal.
            let ct_val = match self.ensure_spec(inst) {
                Ok(spec) => spec.ct_val,
                Err(e) => {
                    self.errors.push(e);
                    continue;
                }
            };
            self.positions.insert(crate::types::symbol(inst), contracts * ct_val);
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
    fn map_order(&mut self, local: OrderId, venue_id: &str, kind: OrderKind) {
        self.local_to_venue.insert(local, venue_id.to_string());
        self.venue_to_local.insert(venue_id.to_string(), local);
        self.order_kind.insert(venue_id.to_string(), kind);
    }

    /// Ensure the [`InstrumentSpec`] for `symbol` is cached, fetching
    /// `/api/v5/public/instruments` if not.
    fn ensure_spec(&mut self, symbol: &str) -> Result<InstrumentSpec, LiveError> {
        if let Some(s) = self.specs.get(symbol) {
            return Ok(*s);
        }
        let params = vec![("instType", "SWAP".to_string()), ("instId", symbol.to_string())];
        let value = self.public_get("/api/v5/public/instruments", params)?;
        let spec = parse_instrument_spec(&value, symbol)
            .ok_or_else(|| LiveError::Decode(format!("no instrument spec for {symbol}")))?;
        self.specs.insert(crate::types::symbol(symbol), spec);
        Ok(spec)
    }

    /// Ensure a fill cursor exists for `symbol`, seeding it to the latest
    /// existing `billId` so we only ever report fills that happen *after* we
    /// started trading it (not the account's whole history).
    fn ensure_cursor(&mut self, symbol: &str) -> Result<(), LiveError> {
        if self.trade_cursor.contains_key(symbol) {
            return Ok(());
        }
        let trades = self.fetch_fills(symbol)?;
        let max = trades.iter().map(|t| t.bill_id).max().unwrap_or(0);
        self.trade_cursor.insert(crate::types::symbol(symbol), max);
        Ok(())
    }

    /// Poll new fills for `symbol` since its cursor, advance the cursor, and
    /// return them as [`Order`]s in base units. A venue order we placed maps back
    /// to its local [`OrderId`] and recorded [`OrderKind`]; a fill on an order we
    /// don't know (placed out-of-band) gets a fresh local id and `Market` kind.
    fn poll_symbol(&mut self, symbol: &str) -> Result<Vec<Order<Symbol>>, LiveError> {
        let ct_val = self.ensure_spec(symbol)?.ct_val;
        let cursor = self.trade_cursor.get(symbol).copied().unwrap_or(0);
        // Pull the recent fills (OKX returns the last few days, most-recent
        // first) and keep only those newer than the cursor. Polling a small
        // recent window and filtering locally avoids a bootstrap `billId`
        // cursor param — robust for the once-per-bar cadence a driver uses.
        let mut trades = self.fetch_fills(symbol)?;
        trades.sort_by_key(|t| t.bill_id);
        let mut out = Vec::new();
        let mut max = cursor;
        for t in trades {
            if t.bill_id <= cursor {
                continue;
            }
            max = max.max(t.bill_id);
            let local = match self.venue_to_local.get(&t.ord_id).copied() {
                Some(id) => id,
                None => self.mint(),
            };
            let kind = self.order_kind.get(&t.ord_id).copied().unwrap_or(OrderKind::Market);
            let order = Order::new(crate::types::symbol(symbol), t.side, t.contracts * ct_val, t.price, kind, local)
                .with_commission(t.commission);
            out.push(order);
        }
        self.trade_cursor.insert(crate::types::symbol(symbol), max);
        Ok(out)
    }

    /// Record `err` on the internal log and return the trait-facing
    /// [`WalletError::Venue`] category.
    fn fail(&mut self, err: LiveError) -> WalletError {
        self.errors.push(err);
        WalletError::Venue
    }

    /// A **refused order**: log the detail, buffer a [`Rejection`] for
    /// [`take_rejections`](Wallet::take_rejections) so the driver can route it to
    /// [`Strategy::on_reject`](crate::Strategy::on_reject), and return the
    /// trait-facing [`WalletError::Venue`]. Unlike [`fail`](Self::fail), this is
    /// for a submission the strategy expected to place — an entry the venue
    /// rejects leaves the strategy flat when it wanted a position, a rejected
    /// protective leg leaves it holding one it wanted out of.
    fn refuse(&mut self, symbol: &str, id: OrderId, kind: OrderKind, err: LiveError) -> WalletError {
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
    /// endpoint-specific query params (GET); `body` is the pre-decision JSON
    /// value serialized once and both signed and sent verbatim (POST).
    fn signed(
        &self,
        method: Method,
        path: &str,
        query: &[(&str, String)],
        body: Option<serde_json::Value>,
    ) -> Result<serde_json::Value, LiveError> {
        let body_str = match body {
            Some(v) => Some(serde_json::to_string(&v).map_err(|e| LiveError::Decode(e.to_string()))?),
            None => None,
        };
        let request_path = with_query(path, query);
        let fut = signed_request(
            &self.client,
            &self.base_url,
            &self.api_key,
            &self.api_secret,
            &self.passphrase,
            self.simulated,
            method,
            &request_path,
            body_str,
        );
        self.rt.block_on(fut)
    }

    /// An unsigned public GET (instrument specs, etc.).
    fn public_get(
        &self,
        path: &str,
        params: Vec<(&str, String)>,
    ) -> Result<serde_json::Value, LiveError> {
        let url = format!("{}{}", self.base_url.trim_end_matches('/'), with_query(path, &params));
        let fut = async {
            let resp = self
                .client
                .get(&url)
                .send()
                .await
                .map_err(|e| LiveError::Network(e.to_string()))?;
            read_json(resp).await
        };
        self.rt.block_on(fut)
    }

    /// Fetch the recent fills for `symbol` (the venue's default window). The
    /// caller keeps only those past its per-symbol `billId` cursor.
    fn fetch_fills(&self, symbol: &str) -> Result<Vec<Fill>, LiveError> {
        let params = vec![("instType", "SWAP".to_string()), ("instId", symbol.to_string())];
        let value = self.signed(Method::GET, "/api/v5/trade/fills", &params, None)?;
        let rows = ok_data(&value)?;
        rows.iter().map(parse_fill).collect()
    }

    /// Cancel a venue order (limit / market) by id on `symbol`, treating "order
    /// does not exist" as success — the post-condition (that order isn't
    /// working) holds either way.
    fn cancel_order(&mut self, symbol: &str, ord_id: &str) -> Result<(), WalletError> {
        let body = serde_json::json!({ "instId": symbol, "ordId": ord_id });
        self.cancel_call(Method::POST, "/api/v5/trade/cancel-order", body)
    }

    /// Cancel a venue **algo** order (protective leg) by id on `symbol`.
    fn cancel_algo(&mut self, symbol: &str, algo_id: &str) -> Result<(), WalletError> {
        let body = serde_json::json!([{ "instId": symbol, "algoId": algo_id }]);
        self.cancel_call(Method::POST, "/api/v5/trade/cancel-algos", body)
    }

    /// Shared cancel path: a non-existent order (`sCode` 51400/51401/51503 or a
    /// top-level not-found `code`) is treated as already gone.
    fn cancel_call(
        &mut self,
        method: Method,
        path: &str,
        body: serde_json::Value,
    ) -> Result<(), WalletError> {
        match self.signed(method, path, &[], Some(body)) {
            Ok(value) => {
                // Business-level failure rides in `code` / `data[].sCode`; the
                // "order does not exist" codes are the success-equivalents.
                if let Some(row) = value.get("data").and_then(|d| d.as_array()).and_then(|a| a.first())
                    && let Some(s_code) = row.get("sCode").and_then(|c| c.as_str())
                    && s_code != "0"
                    && !is_gone_code(s_code)
                {
                    let msg = row.get("sMsg").and_then(|m| m.as_str()).unwrap_or("cancel failed");
                    return Err(self.fail(LiveError::Http { status: 200, body: msg.to_string() }));
                }
                Ok(())
            }
            Err(e) => Err(self.fail(e)),
        }
    }

    /// Place a `reduceOnly` protective algo order (a conditional stop or
    /// take-profit) and record it. Deduped by the caller.
    fn place_protective(
        &mut self,
        symbol: &str,
        kind: OrderKind,
        trigger: Real,
        contracts: Real,
    ) -> Result<RestingLeg, WalletError> {
        let local = self.mint();
        let spec = match self.ensure_spec(symbol) {
            Ok(s) => s,
            Err(e) => return Err(self.refuse(symbol, local, kind, e)),
        };
        let pos = self.positions.get(symbol).copied().unwrap_or(0.0);
        if pos.abs() <= POSITION_EPSILON {
            // Nothing to protect — our own guard, not a venue refusal; log it but
            // don't buffer a per-bar rejection for a flat re-submit.
            return Err(self.fail(LiveError::Decode(format!(
                "no open {symbol} position to rest a protective leg against"
            ))));
        }
        // A protective exit trades the opposite side of the open position.
        let side = if pos > 0.0 { Side::Sell } else { Side::Buy };
        let price = format_decimals(round_to_tick(trigger, spec.tick), spec.px_decimals);
        let sz = format_decimals(contracts, spec.sz_decimals);
        // `conditional` algo with a single trigger; `slOrdPx`/`tpOrdPx` of -1
        // means "fill at market when triggered", the reduce-only twin of
        // Binance's STOP_MARKET / TAKE_PROFIT_MARKET.
        let mut body = serde_json::json!({
            "instId": symbol,
            "tdMode": self.td_mode,
            "side": side_token(side),
            "ordType": "conditional",
            "sz": sz,
            "reduceOnly": "true",
        });
        match kind {
            OrderKind::TakeProfit => {
                body["tpTriggerPx"] = price.clone().into();
                body["tpOrdPx"] = MARKET_ORDER_PX.into();
            }
            // Stop is the default protective leg; Market/Limit never reach here
            // (a market exit is `set_position`, a resting entry is `set_limit`).
            _ => {
                body["slTriggerPx"] = price.clone().into();
                body["slOrdPx"] = MARKET_ORDER_PX.into();
            }
        }
        if let Err(e) = self.ensure_cursor(symbol) {
            self.errors.push(e);
        }
        let value = match self.signed(Method::POST, "/api/v5/trade/order-algo", &[], Some(body)) {
            Ok(v) => v,
            Err(e) => return Err(self.refuse(symbol, local, kind, e)),
        };
        let algo_id = match order_result_id(&value, "algoId") {
            Ok(id) => id,
            Err(e) => return Err(self.refuse(symbol, local, kind, e)),
        };
        self.map_order(local, &algo_id, kind);
        Ok(RestingLeg { trigger, contracts, algo_id, local })
    }

    /// Rest a protective leg with idempotent dedup: an unchanged trigger + size
    /// is a no-op (returns the existing leg's id); a change cancels the old venue
    /// order before placing the new one.
    fn rest_protective(
        &mut self,
        symbol: Symbol,
        kind: OrderKind,
        trigger: Real,
        size: Size,
    ) -> Result<Ack<Symbol>, WalletError> {
        let spec = match self.ensure_spec(&symbol) {
            Ok(s) => s,
            Err(e) => {
                let id = self.mint();
                return Err(self.refuse(&symbol, id, kind, e));
            }
        };
        // Resolve the share against the cached position (base units), clamp to
        // the position magnitude — a protective leg is reduce-only — then convert
        // to the contract count the venue wants.
        let pos = self.positions.get(&symbol).copied().unwrap_or(0.0);
        let units = size
            .resolve(
                self.marks.get(&symbol).copied().unwrap_or(0.0),
                pos,
                self.available_balance,
                self.equity,
            )
            .min(pos.abs());
        let contracts = floor_to_step(units / spec.ct_val, spec.lot_sz);
        if contracts <= POSITION_EPSILON {
            let id = self.mint();
            return Err(self.refuse(
                &symbol,
                id,
                kind,
                LiveError::Decode("protective size rounds to zero contracts".into()),
            ));
        }
        let existing = self.protective.get(&symbol).and_then(|p| match kind {
            OrderKind::TakeProfit => p.take_profit.clone(),
            _ => p.stop.clone(),
        });
        if let Some(leg) = existing {
            if (leg.trigger - trigger).abs() <= PRICE_EPSILON
                && (leg.contracts - contracts).abs() <= POSITION_EPSILON
            {
                return Ok(Ack::Working(leg.local));
            }
            self.cancel_algo(&symbol, &leg.algo_id)?;
        }
        let leg = self.place_protective(&symbol, kind, trigger, contracts)?;
        let local = leg.local;
        let entry = self.protective.entry(symbol).or_default();
        match kind {
            OrderKind::TakeProfit => entry.take_profit = Some(leg),
            _ => entry.stop = Some(leg),
        }
        Ok(Ack::Working(local))
    }
}

impl Wallet<Symbol> for OkxWallet {
    fn funds(&self) -> Reference {
        Reference(self.available_balance)
    }

    fn position(&self, symbol: &Symbol) -> Units<Symbol> {
        Units {
            symbol: symbol.clone(),
            amount: self.positions.get(symbol).copied().unwrap_or(0.0),
        }
    }

    /// Every cached signed position, in base units — the venue's open swaps as of
    /// the last [`refresh_account`](OkxWallet::refresh_account) (which
    /// [`update`](Wallet::update) runs each bar). Overrides the trait default so a
    /// caller — e.g. a portfolio or a baseline snapshot of externally-held
    /// positions — can enumerate what the account holds, not just query one symbol.
    fn positions(&self) -> Vec<Units<Symbol>> {
        self.positions
            .iter()
            .map(|(symbol, &amount)| Units {
                symbol: symbol.clone(),
                amount,
            })
            .collect()
    }

    /// `true` — these are perpetual **swaps** in net position mode, where the
    /// venue carries one signed position per instrument, so a short is an
    /// ordinary negative target. Stated explicitly to mark the contrast with the
    /// spot `CoinbaseWallet`, which answers `false`.
    fn can_short(&self) -> bool {
        true
    }

    /// [`QUOTE_CCY`] — the margin currency a linear USDⓈ-M swap settles in, and
    /// the balance whose `availBal` [`funds`](Wallet::funds) reports.
    ///
    /// Static rather than read from the account: the instrument type fixes it.
    /// Note the mismatch documented on [`equity`](Wallet::equity) — that figure
    /// is OKX's own USD valuation, not this.
    fn quote_ccy(&self) -> Option<&str> {
        Some(QUOTE_CCY)
    }

    fn price(&self, symbol: &Symbol) -> Option<Reference> {
        self.marks.get(symbol).map(|&p| Reference(p))
    }

    /// The account's `totalEq`, which OKX reports as a **USD** valuation of every
    /// holding — *not* [`quote_ccy`](Wallet::quote_ccy)'s `USDT`, which is what
    /// [`funds`](Wallet::funds) is in. The two differ by the USDT peg, so they
    /// agree to well within a tick in practice and this is reported as the venue
    /// states it rather than silently converted (fugazi does no FX). A caller
    /// reconciling to the last cent should read the balance detail itself.
    fn equity(&self) -> Reference {
        Reference(self.equity)
    }

    fn update(&mut self, symbol: Symbol, candle: Candle) -> Vec<Order<Symbol>> {
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

    fn set_position(&mut self, target: Units<Symbol>) -> Result<Ack<Symbol>, WalletError> {
        let symbol = target.symbol;
        // Mint the id up front so a refusal before the POST still carries the
        // submission's id into its Rejection.
        let id = self.mint();
        let spec = match self.ensure_spec(&symbol) {
            Ok(s) => s,
            Err(e) => return Err(self.refuse(&symbol, id, OrderKind::Market, e)),
        };
        let current = self.positions.get(&symbol).copied().unwrap_or(0.0);
        let delta = target.amount - current;
        let contracts = floor_to_step(delta.abs() / spec.ct_val, spec.lot_sz);
        if contracts < spec.min_sz || contracts <= POSITION_EPSILON {
            // Below the venue's minimum tradable size: accept the submission but
            // place nothing (no fill will arrive under this id).
            return Ok(Ack::Working(id));
        }
        // Seed the fill cursor to the pre-trade max *before* placing, so a market
        // order that fills immediately is caught by the next poll rather than
        // skipped by a cursor advanced past its own fill.
        if let Err(e) = self.ensure_cursor(&symbol) {
            self.errors.push(e);
        }
        let side = if delta > 0.0 { Side::Buy } else { Side::Sell };
        let body = serde_json::json!({
            "instId": symbol,
            "tdMode": self.td_mode,
            "side": side_token(side),
            "ordType": "market",
            "sz": format_decimals(contracts, spec.sz_decimals),
            "clOrdId": client_order_id(id),
        });
        let value = match self.signed(Method::POST, "/api/v5/trade/order", &[], Some(body)) {
            Ok(v) => v,
            Err(e) => return Err(self.refuse(&symbol, id, OrderKind::Market, e)),
        };
        let ord_id = match order_result_id(&value, "ordId") {
            Ok(v) => v,
            Err(e) => return Err(self.refuse(&symbol, id, OrderKind::Market, e)),
        };
        self.map_order(id, &ord_id, OrderKind::Market);
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
                self.cancel_algo(symbol, &leg.algo_id)?;
            }
            if let Some(leg) = state.take_profit {
                self.cancel_algo(symbol, &leg.algo_id)?;
            }
        }
        Ok(())
    }

    /// Rest a `limit` / `GTC` order on the venue.
    ///
    /// The [`Size`] resolves at the **limit price** — that is where the order
    /// fills, so it is the price the target should be sized against, matching
    /// [`PaperWallet`](crate::PaperWallet)'s "resolve at the fill" rule.
    ///
    /// The venue order's side comes from the resolved *delta*, not from `side`
    /// directly: `side · size` is an absolute target, so reducing a long is a
    /// sell however the caller spelled the target. A limit already through the
    /// market is simply a marketable limit — the venue fills it immediately, at
    /// the limit or better.
    ///
    /// Idempotent per symbol: re-submitting the same side / price / size is a
    /// no-op that returns the existing order's id; any other change cancels the
    /// previous venue order before placing the replacement — the convention
    /// `rest_protective` uses, so a strategy can walk its
    /// limit every bar without piling up orders.
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

        let current = self.positions.get(&symbol).copied().unwrap_or(0.0);
        let units = size.resolve(limit.0, current, self.available_balance, self.equity);
        let delta = side.sign() * units - current;
        let contracts = floor_to_step(delta.abs() / spec.ct_val, spec.lot_sz);
        let price = round_to_tick(limit.0, spec.tick);
        if contracts < spec.min_sz || contracts <= POSITION_EPSILON {
            // Below the venue's minimum tradable size: accept the submission but
            // place nothing, exactly as `set_position` does.
            return Ok(Ack::Working(local));
        }
        let order_side = if delta > 0.0 { Side::Buy } else { Side::Sell };

        // Idempotent re-submit: an unchanged order stays where it is.
        if let Some(existing) = self.limits.get(&symbol).cloned() {
            if existing.side == order_side
                && (existing.limit - price).abs() <= POSITION_EPSILON
                && (existing.contracts - contracts).abs() <= POSITION_EPSILON
            {
                return Ok(Ack::Working(existing.local));
            }
            let ord_id = existing.ord_id.clone();
            self.cancel_order(&symbol, &ord_id)?;
            self.limits.remove(&symbol);
        }

        // Seed the fill cursor before placing, so a marketable limit that fills
        // instantly is caught by the next poll rather than skipped.
        if let Err(e) = self.ensure_cursor(&symbol) {
            self.errors.push(e);
        }
        let body = serde_json::json!({
            "instId": symbol,
            "tdMode": self.td_mode,
            "side": side_token(order_side),
            "ordType": "limit",
            "sz": format_decimals(contracts, spec.sz_decimals),
            "px": format_decimals(price, spec.px_decimals),
            "clOrdId": client_order_id(local),
        });
        let value = match self.signed(Method::POST, "/api/v5/trade/order", &[], Some(body)) {
            Ok(v) => v,
            Err(e) => return Err(self.refuse(&symbol, local, OrderKind::Limit, e)),
        };
        let ord_id = match order_result_id(&value, "ordId") {
            Ok(v) => v,
            Err(e) => return Err(self.refuse(&symbol, local, OrderKind::Limit, e)),
        };
        self.map_order(local, &ord_id, OrderKind::Limit);
        self.limits.insert(
            symbol,
            RestingLimit { limit: price, contracts, side: order_side, ord_id, local },
        );
        Ok(Ack::Working(local))
    }

    fn cancel_limit(&mut self, symbol: &Symbol) -> Result<(), WalletError> {
        if let Some(resting) = self.limits.remove(symbol) {
            self.cancel_order(symbol, &resting.ord_id)?;
        }
        Ok(())
    }

    fn take_rejections(&mut self) -> Vec<Rejection<Symbol>> {
        std::mem::take(&mut self.rejections)
    }

    fn poll_fills(&mut self) -> Vec<Order<Symbol>> {
        let symbols: Vec<Symbol> = self.trade_cursor.keys().cloned().collect();
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
        // Locate the resting record this id belongs to (a working market order
        // fills near-instantly and isn't tracked for cancel).
        if let Some(symbol) = self.limits.iter().find_map(|(sym, l)| {
            (l.local == id).then(|| sym.clone())
        }) {
            self.cancel_order(&symbol, &venue_id)?;
            self.limits.remove(&symbol);
            return Ok(());
        }
        let leg_symbol = self.protective.iter().find_map(|(sym, state)| {
            let hit = state.stop.as_ref().map(|l| l.local) == Some(id)
                || state.take_profit.as_ref().map(|l| l.local) == Some(id);
            hit.then(|| sym.clone())
        });
        let Some(symbol) = leg_symbol else {
            // Known venue id, but not a tracked resting order: nothing
            // actionable, treat as gone.
            return Ok(());
        };
        self.cancel_algo(&symbol, &venue_id)?;
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

type HmacSha256 = Hmac<Sha256>;

/// The current UTC time in OKX's ISO-8601-with-millis form
/// (`2020-12-08T09:08:57.715Z`), for the `OK-ACCESS-TIMESTAMP` header.
fn now_iso() -> String {
    let fmt = format_description!(
        "[year]-[month]-[day]T[hour]:[minute]:[second].[subsecond digits:3]Z"
    );
    OffsetDateTime::now_utc().format(&fmt).unwrap_or_default()
}

/// OKX's signature: `base64(HMAC-SHA256(secret, prehash))`, where `prehash` is
/// `timestamp + method + requestPath + body`.
fn sign(secret: &str, prehash: &str) -> String {
    let mut mac =
        HmacSha256::new_from_slice(secret.as_bytes()).expect("HMAC accepts a key of any length");
    mac.update(prehash.as_bytes());
    base64::engine::general_purpose::STANDARD.encode(mac.finalize().into_bytes())
}

/// The `clOrdId` we tag an order with, so a later poll can correlate. OKX
/// requires an alphanumeric client id, so no hyphen (unlike Binance).
fn client_order_id(id: OrderId) -> String {
    format!("fugazi{}", id.0)
}

fn side_token(side: Side) -> &'static str {
    match side {
        Side::Buy => "buy",
        Side::Sell => "sell",
    }
}


/// Build, sign, and send a private request, returning the parsed JSON body.
///
/// The signature covers `timestamp + method + requestPath + body`, and the
/// request carries that exact `requestPath` in the URL and that exact `body`
/// string — no client-side re-encoding, so nothing can drift between what was
/// signed and what was sent.
#[allow(clippy::too_many_arguments)]
async fn signed_request(
    client: &reqwest::Client,
    base_url: &str,
    api_key: &str,
    api_secret: &str,
    passphrase: &str,
    simulated: bool,
    method: Method,
    request_path: &str,
    body: Option<String>,
) -> Result<serde_json::Value, LiveError> {
    let timestamp = now_iso();
    let body_str = body.as_deref().unwrap_or("");
    let prehash = format!("{timestamp}{}{request_path}{body_str}", method.as_str());
    let signature = sign(api_secret, &prehash);
    let url = format!("{}{request_path}", base_url.trim_end_matches('/'));

    let mut req = client
        .request(method, &url)
        .header("OK-ACCESS-KEY", api_key)
        .header("OK-ACCESS-SIGN", signature)
        .header("OK-ACCESS-TIMESTAMP", timestamp)
        .header("OK-ACCESS-PASSPHRASE", passphrase)
        .header("Content-Type", "application/json");
    if simulated {
        req = req.header("x-simulated-trading", "1");
    }
    if let Some(body) = body {
        req = req.body(body);
    }
    let resp = req.send().await.map_err(|e| LiveError::Network(e.to_string()))?;
    read_json(resp).await
}

/// Read a response body, mapping a non-2xx status into [`LiveError::Http`].
async fn read_json(resp: reqwest::Response) -> Result<serde_json::Value, LiveError> {
    let status = resp.status();
    let body = resp.text().await.map_err(|e| LiveError::Network(e.to_string()))?;
    if !status.is_success() {
        return Err(LiveError::Http { status: status.as_u16(), body });
    }
    if body.is_empty() {
        return Ok(serde_json::Value::Null);
    }
    serde_json::from_str(&body).map_err(|e| LiveError::Decode(e.to_string()))
}

/// Unwrap an OKX envelope `{ "code": "0", "msg": "", "data": [...] }` into its
/// `data` rows, mapping a non-zero top-level `code` into a [`LiveError`].
fn ok_data(value: &serde_json::Value) -> Result<Vec<serde_json::Value>, LiveError> {
    let code = value.get("code").and_then(|c| c.as_str()).unwrap_or("0");
    if code != "0" {
        let msg = value.get("msg").and_then(|m| m.as_str()).unwrap_or("").to_string();
        return Err(LiveError::Http { status: 200, body: format!("code {code}: {msg}") });
    }
    Ok(value
        .get("data")
        .and_then(|d| d.as_array())
        .cloned()
        .unwrap_or_default())
}

/// Extract the venue id (`ordId` / `algoId`) from an order-placement response,
/// checking both the top-level `code` and the per-order `sCode`. OKX returns
/// HTTP 200 with a non-zero code for a business rejection, so a plausible 200 is
/// not enough on its own.
fn order_result_id(value: &serde_json::Value, id_field: &str) -> Result<String, LiveError> {
    let rows = ok_data(value)?;
    let Some(row) = rows.first() else {
        return Err(LiveError::Decode("order response carried no data row".into()));
    };
    if let Some(s_code) = row.get("sCode").and_then(|c| c.as_str())
        && s_code != "0"
    {
        let msg = row.get("sMsg").and_then(|m| m.as_str()).unwrap_or("order rejected");
        return Err(LiveError::Http { status: 200, body: format!("sCode {s_code}: {msg}") });
    }
    row.get(id_field)
        .and_then(|v| v.as_str())
        .filter(|s| !s.is_empty())
        .map(|s| s.to_string())
        .ok_or_else(|| LiveError::Decode(format!("order response missing {id_field}")))
}

/// The OKX `sCode`s that mean "the order you asked to cancel is already gone"
/// — cancelling it is idempotently successful, not a failure.
fn is_gone_code(s_code: &str) -> bool {
    matches!(s_code, "51400" | "51401" | "51402" | "51503")
}


/// Read a named numeric field off a JSON object (string-or-number).
fn num_field(value: &serde_json::Value, key: &str) -> Option<Real> {
    value.get(key).and_then(parse_num)
}





/// Pull one swap's grid + contract value out of `/api/v5/public/instruments`.
fn parse_instrument_spec(value: &serde_json::Value, symbol: &str) -> Option<InstrumentSpec> {
    let data = value.get("data")?.as_array()?;
    let entry = data
        .iter()
        .find(|s| s.get("instId").and_then(|v| v.as_str()) == Some(symbol))?;
    let lot_str = entry.get("lotSz").and_then(|v| v.as_str())?;
    let tick_str = entry.get("tickSz").and_then(|v| v.as_str())?;
    Some(InstrumentSpec {
        lot_sz: lot_str.parse::<Real>().ok()?,
        min_sz: entry.get("minSz").and_then(parse_num).unwrap_or(0.0),
        tick: tick_str.parse::<Real>().ok()?,
        ct_val: entry.get("ctVal").and_then(parse_num).unwrap_or(1.0),
        sz_decimals: decimals_of(lot_str),
        px_decimals: decimals_of(tick_str),
    })
}

/// One row of `GET /api/v5/trade/fills`, reduced to what a fill needs. `contracts`
/// is the venue-native `fillSz`; the caller multiplies by `ctVal` for base units.
#[derive(Debug, Clone)]
struct Fill {
    bill_id: i64,
    ord_id: String,
    side: Side,
    contracts: Real,
    price: Real,
    commission: Real,
}

fn parse_fill(v: &serde_json::Value) -> Result<Fill, LiveError> {
    let bill_id = v
        .get("billId")
        .and_then(|x| x.as_str())
        .and_then(|s| s.parse::<i64>().ok())
        .ok_or_else(|| LiveError::Decode("fill missing billId".into()))?;
    let ord_id = v.get("ordId").and_then(|x| x.as_str()).unwrap_or_default().to_string();
    let side = match v.get("side").and_then(|x| x.as_str()) {
        Some("buy") => Side::Buy,
        _ => Side::Sell,
    };
    let contracts = num_field(v, "fillSz")
        .ok_or_else(|| LiveError::Decode("fill missing fillSz".into()))?;
    let price =
        num_field(v, "fillPx").ok_or_else(|| LiveError::Decode("fill missing fillPx".into()))?;
    // OKX reports `fee` as a signed number: negative when charged, positive for
    // a rebate. Book the cost as a non-negative commission.
    let commission = num_field(v, "fee").map(|f| (-f).max(0.0)).unwrap_or(0.0);
    Ok(Fill { bill_id, ord_id, side, contracts, price, commission })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn signature_matches_a_known_hmac_vector() {
        // RFC 4231 test case 2, base64-encoded: proves the base64(HMAC-SHA256)
        // path end to end (the hex form is
        // 5bdcc146bf60754e6a042426089575c75a003f089d2739839dec58b964ec3843).
        assert_eq!(
            sign("Jefe", "what do ya want for nothing?"),
            "W9zBRr9gdU5qBCQmCJV1x1oAPwidJzmDnexYuWTsOEM="
        );
    }

    #[test]
    fn client_order_id_is_alphanumeric() {
        assert_eq!(client_order_id(OrderId(42)), "fugazi42");
    }

    #[test]
    fn query_string_is_built_verbatim() {
        assert_eq!(with_query("/x", &[]), "/x");
        assert_eq!(
            with_query("/x", &[("instType", "SWAP".into()), ("instId", "BTC-USDT-SWAP".into())]),
            "/x?instType=SWAP&instId=BTC-USDT-SWAP"
        );
    }


    #[test]
    fn parses_instrument_spec_with_contract_value() {
        let info = serde_json::json!({
            "code": "0",
            "data": [{
                "instId": "BTC-USDT-SWAP",
                "lotSz": "0.1", "minSz": "0.1", "tickSz": "0.1", "ctVal": "0.01"
            }]
        });
        let s = parse_instrument_spec(&info, "BTC-USDT-SWAP").expect("spec parsed");
        assert!((s.lot_sz - 0.1).abs() < 1e-12);
        assert!((s.min_sz - 0.1).abs() < 1e-12);
        assert!((s.ct_val - 0.01).abs() < 1e-12);
        assert_eq!(s.sz_decimals, 1);
        assert_eq!(s.px_decimals, 1);
        assert!(parse_instrument_spec(&info, "ETH-USDT-SWAP").is_none());
    }

    #[test]
    fn parses_fill_into_base_shape() {
        let row = serde_json::json!({
            "billId": "88", "ordId": "42", "side": "sell",
            "fillSz": "2", "fillPx": "27000.5", "fee": "-0.27"
        });
        let t = parse_fill(&row).expect("fill parsed");
        assert_eq!(t.bill_id, 88);
        assert_eq!(t.ord_id, "42");
        assert_eq!(t.side, Side::Sell);
        assert!((t.contracts - 2.0).abs() < 1e-9);
        assert!((t.price - 27000.5).abs() < 1e-9);
        // Fee negative-as-charged becomes a positive commission.
        assert!((t.commission - 0.27).abs() < 1e-9);
    }

    #[test]
    fn order_result_id_rejects_a_business_failure() {
        let ok = serde_json::json!({ "code": "0", "data": [{ "ordId": "77", "sCode": "0" }] });
        assert_eq!(order_result_id(&ok, "ordId").unwrap(), "77");

        let refused = serde_json::json!({
            "code": "1",
            "data": [{ "ordId": "", "sCode": "51008", "sMsg": "insufficient balance" }]
        });
        assert!(order_result_id(&refused, "ordId").is_err());
    }
}
