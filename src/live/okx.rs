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

use base64::Engine as _;
use hmac::{Hmac, KeyInit, Mac};
use reqwest::Method;
use sha2::Sha256;
use time::OffsetDateTime;
use time::macros::format_description;

use crate::hash::SymMap;
use crate::types::Symbol;
use crate::types::{Candle, Real};
use crate::wallet::{
    Ack, Order, OrderId, OrderKind, POSITION_EPSILON, Reference, Rejection, Side, Size, Units,
    Wallet, WalletError,
};

use super::LiveError;
use super::venue::{
    CursorModel, HttpCore, InstrumentGrid, LiveCore, OrderClass, VenueBackend, VenueFill,
    decimals_of, flow, parse_num, with_query,
};

const MAINNET_BASE_URL: &str = "https://www.okx.com";
/// The margin / quote currency a linear USDⓈ-M swap settles in — the balance
/// whose `availBal` we report as [`funds`](Wallet::funds).
const QUOTE_CCY: &str = "USDT";
/// OKX's sentinel for "execute the triggered protective order at market".
const MARKET_ORDER_PX: &str = "-1";
/// `billId` is monotone across the account, so one high-water mark per symbol
/// is enough to tell a fresh fill from one already reported.
const CURSOR_MODEL: CursorModel = CursorModel::Watermark;

/// A live [`Wallet`] over OKX V5 perpetual swaps. See the module-level docs
/// for the trait-to-venue mapping and the contracts↔units convention.
///
/// Construct with [`demo`](Self::demo) / [`mainnet`](Self::mainnet), then drive
/// it through [`backtest::run`](crate::backtest::run) exactly like a
/// [`PaperWallet`](crate::PaperWallet). Must be used from a synchronous context
/// (it owns a `tokio` runtime and blocks on each REST call).
pub struct OkxWallet {
    /// Everything a venue backend keeps that isn't credentials, signing, or
    /// this venue's own account shape.
    core: LiveCore,
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
    positions: SymMap<Symbol, Real>,
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
        Self {
            core: LiveCore::new(base_url),
            api_key: api_key.into(),
            api_secret: api_secret.into(),
            passphrase: passphrase.into(),
            simulated: false,
            td_mode: "cross".to_string(),
            available_balance: 0.0,
            equity: 0.0,
            positions: SymMap::default(),
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
    ///
    /// Bounded: the oldest entries are dropped once the log grows past roughly
    /// twice [`DEFAULT_RETENTION`](crate::wallet::DEFAULT_RETENTION), so a
    /// long-running live process cannot leak through it. A caller who needs the
    /// whole history wants their own durable store, not this accessor.
    pub fn errors(&self) -> &[LiveError] {
        self.core.log().errors()
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

        let positions = self.signed(
            Method::GET,
            "/api/v5/account/positions",
            &[("instType", "SWAP".into())],
            None,
        )?;
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
            let grid = match flow::ensure_grid(self, inst) {
                Ok(g) => g,
                Err(e) => {
                    self.core.log_mut().note(e);
                    continue;
                }
            };
            self.positions
                .insert(crate::types::symbol(inst), grid.base_units(contracts));
        }
        Ok(())
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
            Some(v) => {
                Some(serde_json::to_string(&v).map_err(|e| LiveError::Decode(e.to_string()))?)
            }
            None => None,
        };
        let request_path = with_query(path, query);
        let req = signed_request(
            &self.core.http,
            &self.api_key,
            &self.api_secret,
            &self.passphrase,
            self.simulated,
            method,
            &request_path,
            body_str,
        );
        self.core.http.send(req)
    }

    /// An unsigned public GET (instrument specs, etc.).
    fn public_get(
        &self,
        path: &str,
        params: Vec<(&str, String)>,
    ) -> Result<serde_json::Value, LiveError> {
        self.core.http.public_get(path, &params)
    }

    /// Cancel a venue order, treating "order does not exist" as success — the
    /// post-condition (that order isn't working) holds either way.
    ///
    /// OKX splits this across two endpoints: a resting entry is an ordinary
    /// order, a protective leg is an *algo* order, and each has its own cancel
    /// path and body shape. That split is the reason [`OrderClass`] exists.
    fn cancel_by_class(
        &mut self,
        symbol: &str,
        venue_id: &str,
        class: OrderClass,
    ) -> Result<(), LiveError> {
        let (path, body) = match class {
            OrderClass::Entry => (
                "/api/v5/trade/cancel-order",
                serde_json::json!({ "instId": symbol, "ordId": venue_id }),
            ),
            OrderClass::Protective => (
                "/api/v5/trade/cancel-algos",
                serde_json::json!([{ "instId": symbol, "algoId": venue_id }]),
            ),
        };
        self.cancel_call(Method::POST, path, body)
    }

    /// Shared cancel path: a non-existent order (an `sCode` [`is_gone_code`]
    /// recognises) is treated as already gone.
    ///
    /// Returns `LiveError` rather than `WalletError`: whether a cancel failure
    /// is worth a `Rejection` is the caller's call, and the caller is `flow`.
    fn cancel_call(
        &mut self,
        method: Method,
        path: &str,
        body: serde_json::Value,
    ) -> Result<(), LiveError> {
        let value = self.signed(method, path, &[], Some(body))?;
        // Business-level failure rides in `code` / `data[].sCode`; the
        // "order does not exist" codes are the success-equivalents.
        if let Some(row) = value
            .get("data")
            .and_then(|d| d.as_array())
            .and_then(|a| a.first())
            && let Some(s_code) = row.get("sCode").and_then(|c| c.as_str())
            && s_code != "0"
            && !is_gone_code(s_code)
        {
            let msg = row
                .get("sMsg")
                .and_then(|m| m.as_str())
                .unwrap_or("cancel failed");
            return Err(LiveError::Http {
                status: 200,
                body: msg.to_string(),
            });
        }
        Ok(())
    }
}

/// The venue-fact half: OKX's endpoints, envelopes and request bodies. The
/// order flow that drives these lives in [`flow`](super::venue::flow) and is
/// shared with every other backend.
impl VenueBackend for OkxWallet {
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
        let params = vec![
            ("instType", "SWAP".to_string()),
            ("instId", symbol.to_string()),
        ];
        let value = self.public_get("/api/v5/public/instruments", params)?;
        parse_instrument_grid(&value, symbol)
            .ok_or_else(|| LiveError::Decode(format!("no instrument spec for {symbol}")))
    }

    fn cursor_model(&self) -> CursorModel {
        CURSOR_MODEL
    }

    /// The recent fills for `symbol` — OKX answers with the last few days,
    /// most-recent first, and the caller filters against its cursor.
    fn fetch_fills(&mut self, symbol: &str) -> Result<Vec<VenueFill>, LiveError> {
        let params = vec![
            ("instType", "SWAP".to_string()),
            ("instId", symbol.to_string()),
        ];
        let value = self.signed(Method::GET, "/api/v5/trade/fills", &params, None)?;
        let rows = ok_data(&value)?;
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
            "instId": symbol,
            "tdMode": self.td_mode,
            "side": side_token(side),
            "ordType": "market",
            "sz": grid.size_str(size),
            "clOrdId": client_order_id(local),
        });
        let value = self.signed(Method::POST, "/api/v5/trade/order", &[], Some(body))?;
        order_result_id(&value, "ordId")
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
            "instId": symbol,
            "tdMode": self.td_mode,
            "side": side_token(side),
            "ordType": "limit",
            "sz": grid.size_str(size),
            "px": grid.price_str(price),
            "clOrdId": client_order_id(local),
        });
        let value = self.signed(Method::POST, "/api/v5/trade/order", &[], Some(body))?;
        order_result_id(&value, "ordId")
    }

    /// A `conditional` algo order with a single trigger. `slOrdPx` / `tpOrdPx`
    /// of `-1` means "fill at market when triggered" — the reduce-only twin of
    /// Binance's `STOP_MARKET` / `TAKE_PROFIT_MARKET`.
    ///
    /// **Which field pair is set is how OKX encodes the direction.** There is no
    /// `stop_direction` here, unlike Coinbase, so the stop and take-profit legs
    /// differ only in the two keys written below.
    fn place_protective(
        &mut self,
        symbol: &str,
        kind: OrderKind,
        side: Side,
        size: Real,
        trigger: Real,
        grid: &InstrumentGrid,
        _local: OrderId,
    ) -> Result<String, LiveError> {
        let price = grid.price_str(trigger);
        let mut body = serde_json::json!({
            "instId": symbol,
            "tdMode": self.td_mode,
            "side": side_token(side),
            "ordType": "conditional",
            "sz": grid.size_str(size),
            "reduceOnly": "true",
        });
        match kind {
            OrderKind::TakeProfit => {
                body["tpTriggerPx"] = price.into();
                body["tpOrdPx"] = MARKET_ORDER_PX.into();
            }
            // Stop is the default protective leg; Market / Limit never reach
            // here (a market exit is `set_position`, a resting entry is
            // `set_limit`).
            _ => {
                body["slTriggerPx"] = price.into();
                body["slOrdPx"] = MARKET_ORDER_PX.into();
            }
        }
        let value = self.signed(Method::POST, "/api/v5/trade/order-algo", &[], Some(body))?;
        order_result_id(&value, "algoId")
    }

    fn cancel_venue_order(
        &mut self,
        symbol: &str,
        venue_id: &str,
        class: OrderClass,
    ) -> Result<(), LiveError> {
        self.cancel_by_class(symbol, venue_id, class)
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
        self.core.mark(symbol).map(Reference)
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

type HmacSha256 = Hmac<Sha256>;

/// The current UTC time in OKX's ISO-8601-with-millis form
/// (`2020-12-08T09:08:57.715Z`), for the `OK-ACCESS-TIMESTAMP` header.
fn now_iso() -> String {
    let fmt =
        format_description!("[year]-[month]-[day]T[hour]:[minute]:[second].[subsecond digits:3]Z");
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

/// Build and sign a private request. The caller sends it through
/// [`HttpCore::send`].
///
/// The signature covers `timestamp + method + requestPath + body`, and the
/// request carries that exact `requestPath` in the URL and that exact `body`
/// string — no client-side re-encoding, so nothing can drift between what was
/// signed and what was sent.
#[allow(clippy::too_many_arguments)]
fn signed_request(
    http: &HttpCore,
    api_key: &str,
    api_secret: &str,
    passphrase: &str,
    simulated: bool,
    method: Method,
    request_path: &str,
    body: Option<String>,
) -> reqwest::RequestBuilder {
    let timestamp = now_iso();
    let body_str = body.as_deref().unwrap_or("");
    let prehash = format!("{timestamp}{}{request_path}{body_str}", method.as_str());
    let signature = sign(api_secret, &prehash);
    let url = http.url(request_path);

    let mut req = http
        .client()
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
    req
}

/// Unwrap an OKX envelope `{ "code": "0", "msg": "", "data": [...] }` into its
/// `data` rows, mapping a non-zero top-level `code` into a [`LiveError`].
fn ok_data(value: &serde_json::Value) -> Result<Vec<serde_json::Value>, LiveError> {
    let code = value.get("code").and_then(|c| c.as_str()).unwrap_or("0");
    if code != "0" {
        let msg = value
            .get("msg")
            .and_then(|m| m.as_str())
            .unwrap_or("")
            .to_string();
        return Err(LiveError::Http {
            status: 200,
            body: format!("code {code}: {msg}"),
        });
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
        return Err(LiveError::Decode(
            "order response carried no data row".into(),
        ));
    };
    if let Some(s_code) = row.get("sCode").and_then(|c| c.as_str())
        && s_code != "0"
    {
        let msg = row
            .get("sMsg")
            .and_then(|m| m.as_str())
            .unwrap_or("order rejected");
        return Err(LiveError::Http {
            status: 200,
            body: format!("sCode {s_code}: {msg}"),
        });
    }
    row.get(id_field)
        .and_then(|v| v.as_str())
        .filter(|s| !s.is_empty())
        .map(|s| s.to_string())
        .ok_or_else(|| LiveError::Decode(format!("order response missing {id_field}")))
}

/// The OKX `sCode`s that mean "the order you asked to cancel is already gone"
/// — cancelling it is idempotently successful, not a failure.
///
/// | code | OKX's message |
/// |---|---|
/// | `51400` | cancellation failed: the order does not exist |
/// | `51401` | cancellation failed: the order is already cancelled |
/// | `51402` | cancellation failed: the order is already completed |
/// | `51503` | cancellation failed: the algo order does not exist |
///
/// "Already filled" belongs here with "never existed": the caller asked for
/// that order not to be working, and it isn't. Anything else — a bad
/// instrument, a rejected parameter, a rate limit — is a real failure and
/// reaches the error log.
fn is_gone_code(s_code: &str) -> bool {
    matches!(s_code, "51400" | "51401" | "51402" | "51503")
}

/// Read a named numeric field off a JSON object (string-or-number).
fn num_field(value: &serde_json::Value, key: &str) -> Option<Real> {
    value.get(key).and_then(parse_num)
}

/// Pull one swap's grid + contract value out of `/api/v5/public/instruments`,
/// which answers with every matching instrument rather than a single object.
fn parse_instrument_grid(value: &serde_json::Value, symbol: &str) -> Option<InstrumentGrid> {
    let data = value.get("data")?.as_array()?;
    let entry = data
        .iter()
        .find(|s| s.get("instId").and_then(|v| v.as_str()) == Some(symbol))?;
    let lot_str = entry.get("lotSz").and_then(|v| v.as_str())?;
    let tick_str = entry.get("tickSz").and_then(|v| v.as_str())?;
    Some(InstrumentGrid {
        size_step: lot_str.parse::<Real>().ok()?,
        min_size: entry.get("minSz").and_then(parse_num).unwrap_or(0.0),
        price_tick: tick_str.parse::<Real>().ok()?,
        // A swap quotes `ctVal` base units per contract; an instrument that
        // omits it trades in base units already.
        contract_multiplier: entry.get("ctVal").and_then(parse_num).unwrap_or(1.0),
        size_decimals: decimals_of(lot_str),
        price_decimals: decimals_of(tick_str),
    })
}

/// One row of `GET /api/v5/trade/fills`, normalized. `size` is the venue-native
/// `fillSz`, in contracts; the caller multiplies by `ctVal` for base units.
fn parse_fill(v: &serde_json::Value) -> Result<VenueFill, LiveError> {
    let bill_id = v
        .get("billId")
        .and_then(|x| x.as_str())
        .and_then(|s| s.parse::<i64>().ok())
        .ok_or_else(|| LiveError::Decode("fill missing billId".into()))?;
    let order_id = v
        .get("ordId")
        .and_then(|x| x.as_str())
        .unwrap_or_default()
        .to_string();
    let side = match v.get("side").and_then(|x| x.as_str()) {
        Some("buy") => Side::Buy,
        _ => Side::Sell,
    };
    let size =
        num_field(v, "fillSz").ok_or_else(|| LiveError::Decode("fill missing fillSz".into()))?;
    let price =
        num_field(v, "fillPx").ok_or_else(|| LiveError::Decode("fill missing fillPx".into()))?;
    // OKX reports `fee` as a signed number: negative when charged, positive for
    // a rebate. Book the cost as a non-negative commission.
    let commission = num_field(v, "fee").map(|f| (-f).max(0.0)).unwrap_or(0.0);
    Ok(VenueFill {
        // `billId` is monotone across the account, which is what lets this
        // venue dedupe on a single high-water mark.
        ordinal: Some(bill_id),
        id: bill_id.to_string(),
        sequence: bill_id.to_string(),
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
            with_query(
                "/x",
                &[
                    ("instType", "SWAP".into()),
                    ("instId", "BTC-USDT-SWAP".into())
                ]
            ),
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
        let g = parse_instrument_grid(&info, "BTC-USDT-SWAP").expect("spec parsed");
        assert!((g.size_step - 0.1).abs() < 1e-12);
        assert!((g.min_size - 0.1).abs() < 1e-12);
        assert!((g.contract_multiplier - 0.01).abs() < 1e-12);
        assert_eq!(g.size_decimals, 1);
        assert_eq!(g.price_decimals, 1);
        // The endpoint answers with every matching instrument, so the parser
        // has to pick the right row rather than trust the first.
        assert!(parse_instrument_grid(&info, "ETH-USDT-SWAP").is_none());
    }

    /// The contracts↔units conversion the whole OKX backend rests on: a swap
    /// quoting `ctVal = 0.01` trades 5 contracts for 0.05 base units.
    ///
    /// Sizes floor rather than round, so a rounded order is never *larger* than
    /// the diff it was meant to close.
    #[test]
    fn the_grid_converts_between_contracts_and_base_units() {
        let grid = InstrumentGrid {
            size_step: 0.1,
            min_size: 0.1,
            price_tick: 0.1,
            contract_multiplier: 0.01,
            size_decimals: 1,
            price_decimals: 1,
        };
        assert!((grid.venue_size(0.05) - 5.0).abs() < 1e-12);
        assert!((grid.base_units(5.0) - 0.05).abs() < 1e-12);
        assert_eq!(grid.size_str(grid.venue_size(0.05)), "5.0");
        // Floors to the step: 0.0509 base units is 5.09 contracts, submitted
        // as 5.0 rather than 5.1.
        assert_eq!(grid.size_str(grid.venue_size(0.0509)), "5.0");
        assert!(grid.below_minimum(grid.venue_size(0.0005)));
    }

    /// The already-gone set, pinned against the table in the doc above — the
    /// two drifted once, with `51402` accepted by the code and missing from
    /// the prose.
    #[test]
    fn already_gone_codes_are_exactly_the_four_documented() {
        for gone in ["51400", "51401", "51402", "51503"] {
            assert!(is_gone_code(gone), "{gone} means the order is already gone");
        }
        // A real failure must not be swallowed as an idempotent cancel.
        for live in ["0", "51008", "51000", "51500", "1"] {
            assert!(!is_gone_code(live), "{live} is a genuine cancel failure");
        }
    }

    #[test]
    fn parses_fill_into_base_shape() {
        let row = serde_json::json!({
            "billId": "88", "ordId": "42", "side": "sell",
            "fillSz": "2", "fillPx": "27000.5", "fee": "-0.27"
        });
        let f = parse_fill(&row).expect("fill parsed");
        // `billId` is monotone, so it is the ordinal the cursor watermarks on.
        assert_eq!(f.ordinal, Some(88));
        assert_eq!(f.id, "88");
        assert_eq!(f.order_id, "42");
        assert_eq!(f.side, Side::Sell);
        // Venue-native: `fillSz` is contracts, converted by the grid on the way out.
        assert!((f.size - 2.0).abs() < 1e-9);
        assert!((f.price - 27000.5).abs() < 1e-9);
        // Fee negative-as-charged becomes a positive commission.
        assert!((f.commission - 0.27).abs() < 1e-9);
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
