//! A [`Wallet`] implementation backed by Kraken Spot.
//!
//! [`KrakenWallet`] routes the trait's order flow to Kraken's Spot REST API
//! (`/0/private/*`), authenticating every private call with the venue's
//! **HMAC-SHA512-over-a-SHA256-prehash** scheme — a third signing shape beside
//! OKX's base64-HMAC and Coinbase's ES256 JWT. See [`sign`] for the exact
//! construction; the one detail that bites is that the signature covers the
//! *literal request body*, so the body string is built once and both hashed and
//! sent, never re-serialised in between.
//!
//! ## Spot, not margin — the semantic that shapes everything
//!
//! Kraken Spot is a **cash** venue as this wallet drives it: you hold balances
//! of a base asset and you cannot hold a signed position. The mapping is the
//! same one [`CoinbaseWallet`](super::CoinbaseWallet) uses:
//!
//! * [`position`](Wallet::position) for `XBTUSD` is the account's **XXBT
//!   balance** — the pair's base asset, never negative.
//! * [`funds`](Wallet::funds) is the **quote-currency** balance (`USD` by
//!   default; override with [`with_quote_ccy`](KrakenWallet::with_quote_ccy)).
//! * [`equity`](Wallet::equity) is funds plus every marked base balance valued
//!   at its last-fed `close`, summed through [`marked_sum`] so it can't vary by
//!   a ULP between processes.
//! * [`set_position`](Wallet::set_position) diffs the target against the held
//!   balance and places a market order for the difference. A **negative** target
//!   sells down to flat and buffers a [`Rejection`] for the un-shortable
//!   remainder.
//!
//! **Going short on Kraken is possible, but only on margin**, and margin is
//! opt-in *per order* via `AddOrder`'s `leverage` parameter — omitting it makes
//! the order a cash trade, which is simply rejected for insufficient funds
//! rather than opening a short. This wallet never sends `leverage`, so
//! [`can_short`](Wallet::can_short) is `false` and margin positions (and their
//! rollover fees, which a backtest ignoring them would badly misprice) stay out
//! of scope. `OpenPositions` is deliberately not consulted: it reports *margin*
//! positions only and is permanently empty for a cash account, so reading
//! positions from it would report flat no matter what the account held.
//!
//! ## Asset codes
//!
//! Kraken's balance keys are not the currency names. Legacy assets carry an `X`
//! (crypto) or `Z` (fiat) prefix — `XXBT`, `ZUSD` — while newer ones do not
//! (`USDT`, `DOT`), and there are staking (`.S`) and yield-bearing (`.M`)
//! suffixes besides. There is no rule to derive one from the other, so the
//! **base** code is taken from the pair's own `AssetPairs` entry rather than
//! guessed, and the configured quote currency is resolved by
//! [`balance_of`](KrakenWallet::balance_of), which tries the name as given and
//! then its `Z`- and `X`-prefixed forms.
//!
//! ## How the rest of the trait maps onto the venue
//!
//! * **Reads** serve a cache refreshed from `POST /0/private/Balance` at the top
//!   of each [`update`](Wallet::update). [`price`](Wallet::price) is the last
//!   candle `close` fed in.
//! * **Orders** are `POST /0/private/AddOrder` — `ordertype: market` / `limit`,
//!   and `stop-loss` / `take-profit` for the protective legs, where Kraken reads
//!   `price` as the *trigger* rather than a limit. The venue order id comes back
//!   as `result.txid`, an **array**; the first element is taken.
//! * **Resting orders** are deduped per symbol by the shared flow, so an
//!   unchanged re-submit each bar is a no-op rather than a cancel/replace storm.
//! * **Fills** are polled from `POST /0/private/TradesHistory` filtered to the
//!   pair. Each fill carries an opaque txid *and* a monotone integer
//!   `trade_id`, so this venue gets the O(1) [`CursorModel::Watermark`] rather
//!   than a remembered-id set.
//! * **Errors** arrive as **HTTP 200 with a populated `error` array**, so status
//!   alone never decides success — every response goes through
//!   [`envelope_result`] before its body is read.

use std::collections::HashMap;
use std::time::{SystemTime, UNIX_EPOCH};

use base64::Engine as _;
use hmac::{Hmac, KeyInit, Mac};
use sha2::{Digest, Sha256, Sha512};

use crate::types::Symbol;
use crate::types::{Candle, Real};
use crate::wallet::marked_sum;
use crate::wallet::{
    Ack, Order, OrderId, OrderKind, POSITION_EPSILON, Reference, Rejection, Side, Size, Units,
    Wallet, WalletError,
};

use super::LiveError;
use super::venue::{
    CursorModel, InstrumentGrid, LiveCore, OrderClass, VenueBackend, VenueFill, flow, parse_num,
};

const MAINNET_BASE_URL: &str = "https://api.kraken.com";
/// The currency whose balance is reported as [`funds`](Wallet::funds). Resolved
/// against the venue's own asset codes by [`KrakenWallet::balance_of`].
const DEFAULT_QUOTE_CCY: &str = "USD";
/// `TradesHistory` pages at 50 by default and caps at 100; ask for the cap so a
/// burst of partial fills inside one bar is seen in a single poll.
const FILL_PAGE_LIMIT: usize = 100;

type HmacSha512 = Hmac<Sha512>;

/// What a pair's `AssetPairs` entry tells us that the grid alone does not: which
/// asset codes its two legs settle in. Cached per symbol, because the balance
/// endpoint keys by asset and nothing in the symbol string reveals the code
/// (`XBTUSD` holds `XXBT` against `ZUSD`).
#[derive(Debug, Clone)]
struct PairMeta {
    base: String,
    grid: InstrumentGrid,
}

/// A live [`Wallet`] over Kraken Spot. See the module docs for the trait-to-venue
/// mapping and the cash-balance convention.
///
/// Construct with [`mainnet`](Self::mainnet) (**real funds** — Kraken publishes
/// no demo environment for its Spot API, unlike OKX), then drive it through
/// [`backtest::run`](crate::backtest::run) exactly like a
/// [`PaperWallet`](crate::PaperWallet). Must be used from a synchronous context
/// (it owns a `tokio` runtime and blocks on each REST call).
pub struct KrakenWallet {
    /// Everything a venue backend keeps that isn't credentials, signing, or
    /// this venue's own account shape.
    core: LiveCore,
    api_key: String,
    /// The API secret, already base64-decoded — it is the HMAC key, and decoding
    /// once at construction means a malformed secret fails there rather than on
    /// the first order.
    api_secret: Vec<u8>,
    quote_ccy: String,

    /// Balances by Kraken asset code, refreshed from `/0/private/Balance`.
    balances: HashMap<String, Real>,
    /// Per-symbol pair metadata, fetched once from `/0/public/AssetPairs`.
    pair_meta: HashMap<String, PairMeta>,
    /// Monotone nonce. Seeded from the clock and only ever incremented, so an
    /// NTP step backwards cannot re-issue a nonce the venue has already seen.
    nonce: u64,
}

impl KrakenWallet {
    /// A wallet against Kraken **production** (`api.kraken.com`). This trades
    /// **real funds**.
    ///
    /// `api_key` and `api_secret` are the pair issued when the API key was
    /// created; the secret is the base64 blob Kraken displays, passed verbatim.
    /// Errors if that secret is not valid base64.
    pub fn mainnet(api_key: impl Into<String>, api_secret: &str) -> Result<Self, LiveError> {
        Self::with_base_url(MAINNET_BASE_URL, api_key, api_secret)
    }

    /// A wallet against an explicit base URL — mainly to point tests at a
    /// `wiremock` server. Panics only if a `tokio` current-thread runtime can't
    /// be built (out of OS resources).
    pub fn with_base_url(
        base_url: impl Into<String>,
        api_key: impl Into<String>,
        api_secret: &str,
    ) -> Result<Self, LiveError> {
        let api_secret = base64::engine::general_purpose::STANDARD
            .decode(api_secret.trim())
            .map_err(|e| LiveError::Decode(format!("invalid base64 API secret: {e}")))?;
        Ok(Self {
            core: LiveCore::new(base_url),
            api_key: api_key.into(),
            api_secret,
            quote_ccy: DEFAULT_QUOTE_CCY.to_string(),
            balances: HashMap::new(),
            pair_meta: HashMap::new(),
            nonce: 0,
        })
    }

    /// Override the currency whose balance is reported as
    /// [`funds`](Wallet::funds) (`USD` by default). Set it to `EUR`, `USDT`, …
    /// to match the quote leg of the pairs you trade.
    pub fn with_quote_ccy(mut self, ccy: impl Into<String>) -> Self {
        self.quote_ccy = ccy.into();
        self
    }

    /// An inert placeholder wallet — empty credentials, no cached state. It
    /// exists only as a temporary swap target (e.g. a `std::mem::replace` slot)
    /// and must never be driven or have a request made through it.
    pub fn placeholder() -> Self {
        Self {
            core: LiveCore::new(MAINNET_BASE_URL),
            api_key: String::new(),
            api_secret: Vec::new(),
            quote_ccy: DEFAULT_QUOTE_CCY.to_string(),
            balances: HashMap::new(),
            pair_meta: HashMap::new(),
            nonce: 0,
        }
    }

    /// The live errors this wallet has recorded, in order. Bounded at roughly
    /// twice [`DEFAULT_RETENTION`](crate::wallet::DEFAULT_RETENTION), so a
    /// long-running process cannot leak through it.
    pub fn errors(&self) -> &[LiveError] {
        self.core.log().errors()
    }

    /// Force an account-state refresh now, returning the [`LiveError`] on
    /// failure. [`update`](Wallet::update) calls this each bar; call it directly
    /// for a one-off sync right after construction.
    pub fn refresh_account(&mut self) -> Result<(), LiveError> {
        // The pair table has to be in hand *before* the balances are read, not
        // lazily on first trade: `position` is a `&self` read that cannot fetch,
        // and a caller who syncs and inspects its holdings before feeding the
        // first bar would otherwise be told it holds nothing.
        self.load_pairs()?;
        let value = self.signed("/0/private/Balance", &[])?;
        let result = envelope_result(&value)?;
        let map = result
            .as_object()
            .ok_or_else(|| LiveError::Decode("Balance `result` is not an object".into()))?;
        self.balances = map
            .iter()
            .filter_map(|(code, v)| parse_num(v).map(|amount| (code.clone(), amount)))
            .collect();
        Ok(())
    }

    /// A balance by currency name, tolerating Kraken's legacy asset codes.
    ///
    /// Tries the name as given (`USDT`, `DOT` — newer assets carry no prefix),
    /// then `Z`-prefixed (fiat: `USD` → `ZUSD`), then `X`-prefixed (legacy
    /// crypto: `XBT` → `XXBT`). Absent reads as `0.0`, which is what a currency
    /// the account has never held reports anyway.
    fn balance_of(&self, code: &str) -> Real {
        self.balances
            .get(code)
            .or_else(|| self.balances.get(&format!("Z{code}")))
            .or_else(|| self.balances.get(&format!("X{code}")))
            .copied()
            .unwrap_or(0.0)
    }

    /// The base-asset balance backing a pair — the trait's `position` for it.
    /// Zero until the pair's metadata has been fetched, since the asset code is
    /// not derivable from the symbol.
    fn base_balance(&self, symbol: &str) -> Real {
        self.pair_meta
            .get(symbol)
            .map(|m| self.balance_of(&m.base))
            .unwrap_or(0.0)
    }

    fn quote_balance(&self) -> Real {
        self.balance_of(&self.quote_ccy)
    }

    /// Load the whole `AssetPairs` table, once, unless it is already in hand.
    ///
    /// **Deliberately unfiltered.** A per-symbol fetch would be a smaller
    /// response, but it can only run once a symbol is known — and
    /// [`position`](Wallet::position) is a `&self` read that cannot fetch, so a
    /// wallet synced before its first bar would report every holding as zero.
    /// That is the state a strategy is in when it inspects an account it is
    /// about to trade, and reading it as flat is the kind of wrong that opens a
    /// duplicate position rather than erroring.
    ///
    /// One request buys every pair's grid *and* base asset code for the wallet's
    /// lifetime, which is also fewer round trips than one-per-symbol for
    /// anything trading more than a handful.
    fn load_pairs(&mut self) -> Result<(), LiveError> {
        if !self.pair_meta.is_empty() {
            return Ok(());
        }
        let value = self.core.http.public_get("/0/public/AssetPairs", &[])?;
        let result = envelope_result(&value)?;
        let entries = result
            .as_object()
            .ok_or_else(|| LiveError::Decode("AssetPairs `result` is not an object".into()))?;
        for (key, entry) in entries {
            let Some(meta) = parse_pair_meta(entry) else {
                // One unparseable pair out of ~1400 is not a reason to refuse
                // the whole table; the symbol being traded is checked below.
                continue;
            };
            // Indexed under both spellings Kraken accepts as `pair`: the
            // internal id it keys the response by (`XXBTZUSD`) and the altname a
            // caller is far more likely to have written (`XBTUSD`).
            if let Some(altname) = entry.get("altname").and_then(|a| a.as_str()) {
                self.pair_meta.insert(altname.to_string(), meta.clone());
            }
            self.pair_meta.insert(key.clone(), meta);
        }
        Ok(())
    }

    /// The cached metadata for a symbol, loading the table first if needed.
    ///
    /// A miss after a successful load re-loads once, so a pair listed since this
    /// wallet started is picked up rather than failing forever.
    fn ensure_pair_meta(&mut self, symbol: &str) -> Result<(), LiveError> {
        self.load_pairs()?;
        if !self.pair_meta.contains_key(symbol) {
            self.pair_meta.clear();
            self.load_pairs()?;
        }
        Ok(())
    }

    // --- REST plumbing -----------------------------------------------------

    /// A strictly increasing nonce, in milliseconds where the clock allows.
    ///
    /// Kraken compares each nonce against the last one accepted for the key, so
    /// this takes the max of the clock and its own last value before
    /// incrementing: a backwards clock step slows the nonce down but can never
    /// make it repeat, which would lock the key out with `EAPI:Invalid nonce`.
    fn next_nonce(&mut self) -> u64 {
        let now_ms = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|d| d.as_millis() as u64)
            .unwrap_or(0);
        self.nonce = self.nonce.max(now_ms).saturating_add(1);
        self.nonce
    }

    /// A signed private POST; blocks on the owned runtime.
    ///
    /// `params` are the endpoint's own fields; the nonce is prepended here so
    /// every private call has exactly one. The encoded body is built **once**
    /// and both signed and sent — Kraken's signature covers the literal bytes,
    /// so re-encoding between the two would break it.
    fn signed(
        &mut self,
        path: &str,
        params: &[(&str, String)],
    ) -> Result<serde_json::Value, LiveError> {
        let nonce = self.next_nonce();
        let mut fields: Vec<(&str, String)> = vec![("nonce", nonce.to_string())];
        fields.extend(params.iter().cloned());
        let body = form_encode(&fields);
        let signature = sign(&self.api_secret, path, nonce, &body);
        let url = self.core.http.url(path);
        let req = self
            .core
            .http
            .client()
            .post(&url)
            .header("API-Key", self.api_key.clone())
            .header("API-Sign", signature)
            .header("Content-Type", "application/x-www-form-urlencoded")
            .body(body);
        self.core.http.send(req)
    }

    /// Place an order of any type. Kraken has **one** order endpoint; the
    /// `ordertype` field is what separates a market entry from a resting limit
    /// from a protective trigger.
    fn add_order(
        &mut self,
        symbol: &str,
        side: Side,
        ordertype: &str,
        volume: String,
        price: Option<String>,
        local: OrderId,
    ) -> Result<String, LiveError> {
        let mut params: Vec<(&str, String)> = vec![
            ("pair", symbol.to_string()),
            ("type", side_token(side).to_string()),
            ("ordertype", ordertype.to_string()),
            ("volume", volume),
            ("cl_ord_id", client_order_id(local)),
        ];
        if let Some(p) = price {
            params.push(("price", p));
        }
        let value = self.signed("/0/private/AddOrder", &params)?;
        order_txid(&value)
    }
}

/// The venue-fact half: Kraken's endpoints, envelopes and request bodies. The
/// order flow that drives these lives in [`flow`](super::venue::flow) and is
/// shared with every other backend.
impl VenueBackend for KrakenWallet {
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
        self.ensure_pair_meta(symbol)?;
        self.pair_meta
            .get(symbol)
            .map(|m| m.grid)
            .ok_or_else(|| LiveError::Decode(format!("no asset pair for {symbol}")))
    }

    /// Kraken issues a monotone integer `trade_id` alongside each fill's opaque
    /// txid, so "already reported" is a single high-water mark rather than a
    /// remembered set — the O(1) model, unavailable on Coinbase.
    fn cursor_model(&self) -> CursorModel {
        CursorModel::Watermark
    }

    fn fetch_fills(&mut self, symbol: &str) -> Result<Vec<VenueFill>, LiveError> {
        let params = [
            ("pair", symbol.to_string()),
            ("limit", FILL_PAGE_LIMIT.to_string()),
            // The entry count costs an extra query on Kraken's side and nothing
            // here reads it — the watermark decides what is new.
            ("without_count", "true".to_string()),
        ];
        let value = self.signed("/0/private/TradesHistory", &params)?;
        let result = envelope_result(&value)?;
        let Some(trades) = result.get("trades").and_then(|t| t.as_object()) else {
            // No `trades` key at all is an account with no history, not a
            // malformed response.
            return Ok(Vec::new());
        };
        trades
            .iter()
            .map(|(txid, row)| parse_fill(txid, row))
            .collect()
    }

    fn place_market(
        &mut self,
        symbol: &str,
        side: Side,
        size: Real,
        grid: &InstrumentGrid,
        local: OrderId,
    ) -> Result<String, LiveError> {
        // A market order carries no `price` at all — Kraken rejects one that
        // does rather than ignoring it.
        self.add_order(symbol, side, "market", grid.size_str(size), None, local)
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
        self.add_order(
            symbol,
            side,
            "limit",
            grid.size_str(size),
            Some(grid.price_str(price)),
            local,
        )
    }

    /// A `stop-loss` or `take-profit` order, which on Kraken become **market**
    /// orders when they trigger.
    ///
    /// **The direction rides in the order type**, not in a separate field and
    /// not in which price slot is filled: `price` is the trigger for both, and
    /// `price2` (the limit leg of the `*-limit` variants) is deliberately unused
    /// so a triggered exit is marketable rather than resting at a price the
    /// market has already left.
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
        let ordertype = match kind {
            OrderKind::TakeProfit => "take-profit",
            _ => "stop-loss",
        };
        self.add_order(
            symbol,
            side,
            ordertype,
            grid.size_str(size),
            Some(grid.price_str(trigger)),
            local,
        )
    }

    /// Kraken has **one** cancel endpoint for every order type, so the
    /// [`OrderClass`] the shared flow passes is not consulted — the parameter
    /// stays on the hook because OKX needs it.
    ///
    /// A cancel that reports no such order is treated as success: the
    /// post-condition (that order isn't working) holds either way.
    fn cancel_venue_order(
        &mut self,
        _symbol: &str,
        venue_id: &str,
        _class: OrderClass,
    ) -> Result<(), LiveError> {
        let params = [("txid", venue_id.to_string())];
        let value = self.signed("/0/private/CancelOrder", &params)?;
        match envelope_result(&value) {
            Ok(_) => Ok(()),
            Err(LiveError::Http { status: 200, body }) if body.contains("EOrder:Unknown order") => {
                Ok(())
            }
            Err(e) => Err(e),
        }
    }
}

impl Wallet<Symbol> for KrakenWallet {
    fn funds(&self) -> Reference {
        Reference(self.quote_balance())
    }

    fn position(&self, symbol: &Symbol) -> Units<Symbol> {
        Units {
            symbol: symbol.clone(),
            amount: self.base_balance(symbol),
        }
    }

    /// Every marked pair's base balance. Overrides the trait default so a
    /// portfolio / baseline snapshot can enumerate what the account holds —
    /// which is also what makes the trait's `flatten` default work.
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

    /// `false` — this wallet trades Kraken as **cash spot**, where a position is
    /// an owned balance that cannot go negative.
    ///
    /// Kraken does offer margin, and a short is possible there, but it is opt-in
    /// per order through `AddOrder`'s `leverage` parameter and this wallet never
    /// sends it. Reporting `true` on the grounds that the venue *could* short
    /// would be a claim about a configuration that isn't in use.
    fn can_short(&self) -> bool {
        false
    }

    /// The currency this wallet was built against — [`DEFAULT_QUOTE_CCY`] unless
    /// [`with_quote_ccy`](KrakenWallet::with_quote_ccy) changed it. Both
    /// [`funds`](Wallet::funds) and [`equity`](Wallet::equity) are in it.
    fn quote_ccy(&self) -> Option<&str> {
        Some(&self.quote_ccy)
    }

    /// `["kraken"]` — the provider that fetches this venue's candles, keyed on
    /// the same pair-name vocabulary this wallet's symbols are (`XBTUSD`).
    ///
    /// Note the provider reaches back only 720 bars, so a strategy with a long
    /// warm-up should be primed from a file rather than a live fetch.
    fn data_sources(&self) -> &'static [&'static str] {
        &["kraken"]
    }

    /// `None`, structurally — the same fact [`can_short`](Wallet::can_short)
    /// reports as `false`, said the other way. A cash spot balance is not
    /// borrowed, so there is no multiple to report.
    fn leverage(&self, _symbol: &Symbol) -> Option<Real> {
        None
    }

    fn price(&self, symbol: &Symbol) -> Option<Reference> {
        self.core.mark(symbol).map(Reference)
    }

    /// The quote balance plus every marked base balance at its last close,
    /// summed in the canonical order [`marked_sum`] defines so it cannot vary by
    /// a ULP between processes on identical inputs.
    ///
    /// Deliberately **not** `TradeBalance`'s `e` field: that is Kraken's own
    /// valuation including margin effects, and this wallet's book is a cash one.
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

/// Kraken's request signature.
///
/// `API-Sign = base64(HMAC_SHA512(secret, path || SHA256(nonce || body)))`,
/// where `path` is the URI path including `/0/private/`, `nonce` is its own
/// decimal digits as ASCII, and `body` is the urlencoded form **as sent** — which
/// already contains `nonce=…` again. That duplication is Kraken's design, not a
/// mistake: the nonce is hashed both on its own and as part of the body.
///
/// The SHA256 digest is appended as 32 raw bytes, not hex.
fn sign(secret: &[u8], path: &str, nonce: u64, body: &str) -> String {
    let mut prehash = Sha256::new();
    prehash.update(nonce.to_string().as_bytes());
    prehash.update(body.as_bytes());
    let digest = prehash.finalize();

    let mut mac = HmacSha512::new_from_slice(secret).expect("HMAC accepts a key of any length");
    mac.update(path.as_bytes());
    mac.update(&digest);
    base64::engine::general_purpose::STANDARD.encode(mac.finalize().into_bytes())
}

/// Encode form fields as `a=1&b=2`, percent-escaping anything outside the
/// unreserved set.
///
/// Hand-rolled rather than delegated to a form serializer because the signature
/// covers these exact bytes: whatever this produces must be what goes on the
/// wire, and a serializer that reorders or re-escapes differently between the
/// two calls would produce a valid-looking request that Kraken rejects. Pair
/// names can contain `/` (`BTC/USD`) and `.` , so escaping is not optional.
fn form_encode(fields: &[(&str, String)]) -> String {
    fields
        .iter()
        .map(|(k, v)| format!("{}={}", percent_encode(k), percent_encode(v)))
        .collect::<Vec<_>>()
        .join("&")
}

/// Percent-encode everything outside RFC 3986's unreserved set
/// (`A-Z a-z 0-9 - . _ ~`).
fn percent_encode(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for b in s.bytes() {
        if b.is_ascii_alphanumeric() || matches!(b, b'-' | b'.' | b'_' | b'~') {
            out.push(b as char);
        } else {
            out.push_str(&format!("%{b:02X}"));
        }
    }
    out
}

/// Unwrap Kraken's `{error, result}` envelope.
///
/// **Every** response goes through here, because Kraken reports business
/// failures with HTTP 200 and a populated `error` array — a caller that checked
/// only the status would read a rejected order as an accepted one. The error
/// entries are joined into a `LiveError::Http { status: 200 }`, the same shape
/// the other venues use for a 200-with-a-rejection.
fn envelope_result(value: &serde_json::Value) -> Result<&serde_json::Value, LiveError> {
    let errors: Vec<&str> = value
        .get("error")
        .and_then(|e| e.as_array())
        .map(|a| a.iter().filter_map(|e| e.as_str()).collect())
        .unwrap_or_default();
    if !errors.is_empty() {
        return Err(LiveError::Http {
            status: 200,
            body: errors.join(", "),
        });
    }
    value
        .get("result")
        .ok_or_else(|| LiveError::Decode("response carries neither `error` nor `result`".into()))
}

/// The venue order id from an `AddOrder` response.
///
/// `result.txid` is an **array** — normally of one — rather than a scalar, and
/// it is absent entirely on a `validate`-only submission.
fn order_txid(value: &serde_json::Value) -> Result<String, LiveError> {
    let result = envelope_result(value)?;
    result
        .get("txid")
        .and_then(|t| t.as_array())
        .and_then(|a| a.first())
        .and_then(|v| v.as_str())
        .filter(|s| !s.is_empty())
        .map(|s| s.to_string())
        .ok_or_else(|| LiveError::Decode("order response missing txid".into()))
}

/// Pull a pair's trading grid and base asset code out of an `AssetPairs` entry.
///
/// Kraken publishes no explicit lot step, so the size step is derived from
/// `lot_decimals` (8 → `0.00000001`); `tick_size` and `pair_decimals` give the
/// price side directly. `contract_multiplier` is `1.0`: spot trades in base
/// units, so every contracts↔units conversion is the exact identity.
fn parse_pair_meta(value: &serde_json::Value) -> Option<PairMeta> {
    let base = value.get("base")?.as_str()?.to_string();
    let lot_decimals = value.get("lot_decimals")?.as_u64()? as usize;
    let price_decimals = value.get("pair_decimals")?.as_u64()? as usize;
    let price_tick = value
        .get("tick_size")
        .and_then(parse_num)
        // A pair without an explicit tick is quoted to `pair_decimals`, which is
        // the same statement in the other units.
        .unwrap_or_else(|| 10f64.powi(-(price_decimals as i32)));
    Some(PairMeta {
        base,
        grid: InstrumentGrid {
            size_step: 10f64.powi(-(lot_decimals as i32)),
            min_size: value.get("ordermin").and_then(parse_num).unwrap_or(0.0),
            price_tick,
            contract_multiplier: 1.0,
            size_decimals: lot_decimals,
            price_decimals,
        },
    })
}

/// One row of `TradesHistory`, normalized. `size` is already in base units —
/// spot has no contract wrapper.
///
/// The map key is the fill's txid; `trade_id` is the monotone integer the
/// watermark cursor rides on.
fn parse_fill(txid: &str, v: &serde_json::Value) -> Result<VenueFill, LiveError> {
    let ordinal = v.get("trade_id").and_then(|x| x.as_i64());
    let order_id = v
        .get("ordertxid")
        .and_then(|x| x.as_str())
        .unwrap_or_default()
        .to_string();
    let side = match v.get("type").and_then(|x| x.as_str()) {
        Some("sell") => Side::Sell,
        Some("buy") => Side::Buy,
        other => {
            return Err(LiveError::Decode(format!(
                "fill has unknown type {other:?}"
            )));
        }
    };
    let size = v
        .get("vol")
        .and_then(parse_num)
        .ok_or_else(|| LiveError::Decode("fill missing vol".into()))?;
    let price = v
        .get("price")
        .and_then(parse_num)
        .ok_or_else(|| LiveError::Decode("fill missing price".into()))?;
    Ok(VenueFill {
        ordinal,
        id: txid.to_string(),
        // `time` is a float unix stamp; the watermark orders by `ordinal`, so
        // this is only the tiebreak for a venue that reports no integer key.
        sequence: v
            .get("time")
            .map(|t| t.to_string())
            .unwrap_or_else(|| txid.to_string()),
        order_id,
        side,
        size,
        price,
        commission: v.get("fee").and_then(parse_num).unwrap_or(0.0),
    })
}

/// The `cl_ord_id` an order is tagged with, so a later poll can correlate.
/// Kraken caps free-text client ids at 18 characters, which `fugazi` + a u64
/// ordinal stays inside.
fn client_order_id(id: OrderId) -> String {
    format!("fugazi{}", id.0)
}

fn side_token(side: Side) -> &'static str {
    match side {
        Side::Buy => "buy",
        Side::Sell => "sell",
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Kraken's own published worked example, reproduced exactly.
    ///
    /// This is the single most valuable test in the file: signing is the one
    /// part of a venue backend with no partial credit — a wrong signature is
    /// rejected identically to a wrong key, a stale nonce or a malformed body,
    /// so without a known-good vector the first real failure would be a live
    /// `EAPI:Invalid signature` with nothing to bisect. The vector pins the
    /// prehash order, the raw-bytes (not hex) digest, the path-without-host, and
    /// the nonce being hashed both alone and inside the body.
    #[test]
    fn the_signature_matches_krakens_published_vector() {
        const SECRET: &str = "kQH5HW/8p1uGOVjbgWA7FunAmGO8lsSUXNsu3eow76sz84Q18fWxnyRzBHCd3pd5nE9qa99HAZtuZuj6F1huXg==";
        const NONCE: u64 = 1_616_492_376_594;
        const BODY: &str =
            "nonce=1616492376594&ordertype=limit&pair=XBTUSD&price=37500&type=buy&volume=1.25";
        const EXPECTED: &str = "4/dpxb3iT4tp/ZCVEwSnEsLxx0bqyhLpdfOpc6fn7OR8+UClSV5n9E6aSS8MPtnRfp32bAb0nmbRn6H8ndwLUQ==";

        let secret = base64::engine::general_purpose::STANDARD
            .decode(SECRET)
            .expect("the documented secret is valid base64");
        assert_eq!(sign(&secret, "/0/private/AddOrder", NONCE, BODY), EXPECTED);
    }

    /// The signature covers the literal body, so the encoder has to be stable
    /// and has to escape anything outside the unreserved set — a pair spelled
    /// `BTC/USD` would otherwise split the field.
    #[test]
    fn form_encoding_escapes_reserved_characters() {
        assert_eq!(
            form_encode(&[("nonce", "1".into()), ("pair", "XBTUSD".into())]),
            "nonce=1&pair=XBTUSD"
        );
        assert_eq!(form_encode(&[("pair", "BTC/USD".into())]), "pair=BTC%2FUSD");
        // `.` `-` `_` `~` are unreserved and must pass through untouched, or a
        // decimal volume would be mangled.
        assert_eq!(
            form_encode(&[("volume", "1.25".into()), ("ordertype", "stop-loss".into())]),
            "volume=1.25&ordertype=stop-loss"
        );
    }

    /// A repeat or a backwards clock step must never re-issue a nonce: Kraken
    /// compares against the last accepted value per key, and a duplicate locks
    /// the key out.
    #[test]
    fn the_nonce_is_strictly_increasing() {
        let mut w = KrakenWallet::placeholder();
        let a = w.next_nonce();
        let b = w.next_nonce();
        let c = w.next_nonce();
        assert!(a < b && b < c, "{a} {b} {c}");

        // Simulate the clock jumping backwards past the last issued nonce.
        w.nonce = u64::MAX - 2;
        let high = w.next_nonce();
        assert!(high > c, "a wall-clock regression must not lower the nonce");
    }

    /// The failure mode that makes Kraken different from a status-code venue: a
    /// rejected order is HTTP 200.
    #[test]
    fn a_populated_error_array_is_a_failure_despite_http_200() {
        let refused = serde_json::json!({ "error": ["EOrder:Insufficient funds"], "result": {} });
        match envelope_result(&refused) {
            Err(LiveError::Http { status: 200, body }) => {
                assert!(body.contains("Insufficient funds"), "{body:?}")
            }
            other => panic!("expected a 200-with-rejection, got {other:?}"),
        }
        // An empty array is the success spelling.
        let ok = serde_json::json!({ "error": [], "result": { "txid": ["OU22CG-KLAF2-FWUDD7"] } });
        assert!(envelope_result(&ok).is_ok());
    }

    /// `txid` is an array even for a single order — reading it as a scalar
    /// would lose every order id.
    #[test]
    fn the_order_id_is_the_first_element_of_the_txid_array() {
        let ok = serde_json::json!({
            "error": [],
            "result": { "descr": { "order": "buy 1.45 XBTUSD @ limit 27500.0" },
                        "txid": ["OU22CG-KLAF2-FWUDD7"] },
        });
        assert_eq!(order_txid(&ok).unwrap(), "OU22CG-KLAF2-FWUDD7");

        // A `validate`-only submission answers with `descr` and no `txid`.
        let no_txid = serde_json::json!({ "error": [], "result": { "descr": {} } });
        assert!(matches!(order_txid(&no_txid), Err(LiveError::Decode(_))));
    }

    /// The grid comes from `lot_decimals` / `pair_decimals` rather than an
    /// explicit step, and the base asset code is read rather than derived —
    /// `XBTUSD` holds `XXBT`, which no rule produces from the symbol.
    #[test]
    fn a_pair_entry_yields_the_grid_and_the_base_asset_code() {
        let entry = serde_json::json!({
            "altname": "XBTUSD",
            "base": "XXBT",
            "quote": "ZUSD",
            "lot_decimals": 8,
            "pair_decimals": 1,
            "ordermin": "0.00005",
            "tick_size": "0.1",
            "status": "online",
        });
        let meta = parse_pair_meta(&entry).expect("a complete entry parses");
        assert_eq!(meta.base, "XXBT");
        assert!((meta.grid.size_step - 1e-8).abs() < 1e-18);
        assert!((meta.grid.min_size - 0.00005).abs() < 1e-12);
        assert!((meta.grid.price_tick - 0.1).abs() < 1e-12);
        assert_eq!(meta.grid.size_decimals, 8);
        assert_eq!(meta.grid.price_decimals, 1);
        // Spot: one venue size unit is one base unit, exactly.
        assert_eq!(meta.grid.contract_multiplier, 1.0);
    }

    /// A pair with no `tick_size` falls back to `pair_decimals`, which states
    /// the same thing in other units — rather than to a zero tick, which would
    /// disable price snapping entirely.
    #[test]
    fn a_pair_without_an_explicit_tick_falls_back_to_its_price_decimals() {
        let entry = serde_json::json!({
            "base": "XXBT", "lot_decimals": 8, "pair_decimals": 2, "ordermin": "0",
        });
        let meta = parse_pair_meta(&entry).expect("parses without tick_size");
        assert!((meta.grid.price_tick - 0.01).abs() < 1e-12);
    }

    /// Balances key by Kraken's asset codes, which are not the currency names.
    #[test]
    fn balances_resolve_through_krakens_legacy_asset_prefixes() {
        let mut w = KrakenWallet::placeholder();
        w.balances.insert("ZUSD".into(), 10_000.0);
        w.balances.insert("XXBT".into(), 1.5);
        w.balances.insert("USDT".into(), 250.0);

        // Fiat carries a `Z`, legacy crypto an `X`...
        assert_eq!(w.balance_of("USD"), 10_000.0);
        assert_eq!(w.balance_of("XBT"), 1.5);
        // ...and a newer asset carries neither, so the bare name must win.
        assert_eq!(w.balance_of("USDT"), 250.0);
        // An asset the account has never held is flat, not an error.
        assert_eq!(w.balance_of("DOGE"), 0.0);
    }

    /// A fill's identity is the map key; its ordering key is the monotone
    /// integer beside it. Reading `vol` as the size and `price` as the price
    /// (rather than `cost`) is the part that silently mis-books a fill.
    #[test]
    fn a_trade_row_normalizes_into_a_venue_fill() {
        let row = serde_json::json!({
            "ordertxid": "OQCLML-BW3P3-BUCMWZ",
            "pair": "XXBTZUSD",
            "time": 1688667796.8802_f64,
            "type": "buy",
            "ordertype": "limit",
            "price": "30010.0",
            "cost": "600.2",
            "fee": "0.96",
            "vol": "0.02",
            "margin": "0.0",
            "trade_id": 40_274_859_i64,
            "maker": true,
        });
        let fill = parse_fill("THVRQM-33VKH-UCI7BS", &row).unwrap();
        assert_eq!(fill.id, "THVRQM-33VKH-UCI7BS");
        assert_eq!(fill.ordinal, Some(40_274_859));
        assert_eq!(fill.order_id, "OQCLML-BW3P3-BUCMWZ");
        assert!(matches!(fill.side, Side::Buy));
        assert_eq!(fill.size, 0.02);
        assert_eq!(fill.price, 30_010.0);
        assert_eq!(fill.commission, 0.96);
    }

    /// A row whose `type` is neither side is a decode error rather than a
    /// silently-defaulted buy.
    #[test]
    fn a_trade_row_with_no_recognisable_side_is_refused() {
        let row = serde_json::json!({ "vol": "1", "price": "1", "type": "sideways" });
        assert!(matches!(parse_fill("T1", &row), Err(LiveError::Decode(_))));
    }
}
