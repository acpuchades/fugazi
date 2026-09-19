//! The live venue wallet bindings — `OkxWallet` / `CoinbaseWallet` /
//! `KrakenWallet`, split out of `strategy.rs` along the same seam the Rust
//! side draws (`src/live/{okx,coinbase,kraken}.rs`), so the structural
//! correspondence a parity review leans on holds module-for-module. The
//! shared order-flow helpers they call (`coerce_bar`, `wrap_ack`,
//! `parse_side`, `coerce_size`, `wallet_restore`) stay in `strategy.rs`
//! beside `PaperWallet`, whose surface defines them.

#[allow(unused_imports)]
use crate::panel::*;
use crate::prelude::*;
// The binding modules were one flat namespace before the split and still read
// as one: each pulls in its siblings, so a cross-module reference needs no path.
#[allow(unused_imports)]
use crate::carriers::*;
#[allow(unused_imports)]
use crate::classes::*;
#[allow(unused_imports)]
use crate::constructors::*;
#[allow(unused_imports)]
use crate::metrics::*;
#[allow(unused_imports)]
use crate::sources::*;
#[allow(unused_imports)]
use crate::spec::*;
#[allow(unused_imports)]
use crate::strategy::*;
// Aliased for the same reason as in `strategy.rs`: the prelude glob already
// binds `fugazi_core::wallet::WalletError`; this is the Python exception type.
use crate::errors::WalletError as PyWalletError;

/// A live [`Wallet`] over OKX V5 perpetual swaps — the same order-flow surface as
/// [`PaperWallet`](PyWallet), but routed to OKX's REST API instead of an in-memory
/// book. Construct with [`OkxWallet.demo`](Self::demo) (the free demo-trading
/// environment) or [`OkxWallet.mainnet`](Self::mainnet) (**real funds**), each
/// taking the API key / secret / passphrase set when the key was created and an
/// optional `td_mode` (`"cross"` — the default — or `"isolated"`).
///
/// Drive it exactly like the paper wallet: [`update`](Self::update) each bar marks
/// price and returns fills, [`set_position`](Self::set_position) / [`set`](Self::set)
/// / [`close`](Self::close) send market orders, and [`set_stop`](Self::set_stop) /
/// [`set_take_profit`](Self::set_take_profit) / [`set_limit`](Self::set_limit) rest
/// protective / entry legs. Submitting a market or resting order returns `None`
/// (working) — the fill lands later, surfaced by a subsequent
/// [`update`](Self::update) or by [`poll_fills`](Self::poll_fills). Reads
/// ([`funds`](Self::funds) / [`equity`](Self::equity) / [`position`](Self::position))
/// serve a cache refreshed each `update`; call [`refresh_account`](Self::refresh_account)
/// for a one-off sync. A REST failure surfaces as a `ValueError` (with the detail
/// also appended to [`errors`](Self::errors)).
///
/// It owns a private async runtime and blocks on each request, so it must be
/// driven from synchronous Python. The higher-level `Strategy.run(...)` builders
/// take a `PaperWallet`; an `OkxWallet` is driven manually, one bar at a time.
#[pyclass(name = "OkxWallet", module = "fugazi")]
pub(crate) struct PyOkxWallet {
    pub(crate) inner: OkxWallet,
}

#[pymethods]
impl PyOkxWallet {
    /// A wallet against OKX **demo trading** (production host, requests carry the
    /// simulated-trading header). Needs demo API credentials.
    #[staticmethod]
    #[pyo3(signature = (api_key, api_secret, passphrase, td_mode = None))]
    pub(crate) fn demo(
        api_key: String,
        api_secret: String,
        passphrase: String,
        td_mode: Option<String>,
    ) -> Self {
        let mut inner = OkxWallet::demo(api_key, api_secret, passphrase);
        if let Some(mode) = td_mode {
            inner = inner.with_td_mode(mode);
        }
        PyOkxWallet { inner }
    }

    /// A wallet against OKX **production** (`www.okx.com`). This trades **real
    /// funds** — supply live keys deliberately.
    #[staticmethod]
    #[pyo3(signature = (api_key, api_secret, passphrase, td_mode = None))]
    pub(crate) fn mainnet(
        api_key: String,
        api_secret: String,
        passphrase: String,
        td_mode: Option<String>,
    ) -> Self {
        let mut inner = OkxWallet::mainnet(api_key, api_secret, passphrase);
        if let Some(mode) = td_mode {
            inner = inner.with_td_mode(mode);
        }
        PyOkxWallet { inner }
    }

    /// The available cash balance (the quote-currency `availBal`), from the cache.
    #[getter]
    pub(crate) fn funds(&self) -> f64 {
        self.inner.funds().0
    }

    /// The signed position in `symbol` (positive long, negative short), in base
    /// units, from the cache.
    pub(crate) fn position(&self, symbol: &str) -> f64 {
        self.inner.position(&intern(symbol)).amount
    }

    /// The last price fed for `symbol` via `update`, or `None` if never fed.
    pub(crate) fn price(&self, symbol: &str) -> Option<f64> {
        self.inner.price(&intern(symbol)).map(|p| p.0)
    }

    /// Mark-to-market account equity (`totalEq`), from the cache.
    #[getter]
    pub(crate) fn equity(&self) -> f64 {
        self.inner.equity().0
    }

    /// `True` — these are perpetual swaps in net position mode, so the venue
    /// carries one signed position per instrument and a short is an ordinary
    /// negative target.
    #[getter]
    pub(crate) fn can_short(&self) -> bool {
        self.inner.can_short()
    }

    /// `"USDT"` — the margin currency a linear USDⓈ-M swap settles in, and what
    /// `funds` reports. Note `equity` is OKX's own USD valuation of the account
    /// rather than this; the two differ by the USDT peg, and nothing here
    /// converts between them.
    #[getter]
    pub(crate) fn quote_ccy(&self) -> Option<&str> {
        self.inner.quote_ccy()
    }

    /// `["okx"]` — the venue this wallet trades, whose candlesticks the `okx`
    /// provider fetches. Venue granularity only: this account trades swaps, so
    /// the matching bars are that provider's answer for the **swap** instrument
    /// id (`okx:BTC-USDT-SWAP[1h]`), not the spot pair it serves under
    /// `BTC-USDT`. Pairing the right instrument is still yours to do.
    #[getter]
    pub(crate) fn data_sources(&self) -> Vec<&'static str> {
        self.inner.data_sources().to_vec()
    }

    /// The leverage OKX has `symbol` configured at, from cache — or `None` when
    /// this wallet has not been able to ask.
    ///
    /// Filled for free on every symbol the account holds a position in (the
    /// positions payload carries it), and on demand for anything else through
    /// `refresh_leverage(symbol)`. `None` is never `1x` and never "no
    /// leverage": a swap account always has one.
    ///
    /// **Reporting, not control.** Nothing here sets the number — it is
    /// configured out of band in OKX's own UI, under the `(instId, margin mode)`
    /// pair this wallet trades, and can change under a running strategy. Record
    /// it at connect and check it on reconcile rather than assuming the account
    /// still sits where it was left; compare it against the `max_gross` of the
    /// `PaperWallet` whose backtest this deployment is meant to be tracking.
    pub(crate) fn leverage(&self, symbol: &str) -> Option<f64> {
        self.inner.leverage(&intern(symbol))
    }

    /// Read `symbol`'s leverage from the venue now and cache it for
    /// `leverage(symbol)`. Raises `ValueError` on a venue error.
    ///
    /// The only path that fetches it for a symbol the account is flat in —
    /// `leverage` itself answers from cache and never blocks on a request.
    /// A failure is cached as "asked and did not get an answer", so a broken or
    /// unauthorised endpoint costs one request rather than one per call; call
    /// again to retry.
    pub(crate) fn refresh_leverage(&mut self, symbol: &str) -> PyResult<f64> {
        self.inner
            .refresh_leverage(&intern(symbol))
            .map_err(|error| PyValueError::new_err(error.to_string()))
    }

    /// Force an account-state refresh (balance + positions) now. Raises
    /// `ValueError` on a REST failure. `update` calls this each bar; call it
    /// directly for a one-off sync (e.g. right after construction).
    pub(crate) fn refresh_account(&mut self) -> PyResult<()> {
        self.inner
            .refresh_account()
            .map_err(|e| PyWalletError::new_err(e.to_string()))
    }

    /// The live errors this wallet has recorded, in order — every REST failure
    /// (the detail behind a raised `ValueError`, plus best-effort refresh /
    /// fill-poll failures that have no return channel), as strings.
    pub(crate) fn errors(&self) -> Vec<String> {
        self.inner.errors().iter().map(|e| e.to_string()).collect()
    }

    /// Feed `symbol`'s current bar (whose `close` marks price) and return any fills
    /// polled for it. Accepts a `Candle` or a bare price `float`. Refreshes the
    /// account cache first.
    pub(crate) fn update(
        &mut self,
        symbol: String,
        bar: &Bound<'_, PyAny>,
    ) -> PyResult<Vec<PyOrder>> {
        let candle = if let Ok(candle) = bar.cast::<PyCandle>() {
            candle.borrow().inner
        } else {
            let price: f64 = bar.extract()?;
            Candle::new(price, price, price, price, 0.0)
        };
        Ok(self
            .inner
            .update(intern(symbol), candle)
            .into_iter()
            .map(|inner| PyOrder { inner })
            .collect())
    }

    /// Send a market order driving `symbol` to `target` signed base units. Returns
    /// `None` (working — the fill surfaces from a later `update` / `poll_fills`).
    pub(crate) fn set_position(
        &mut self,
        symbol: String,
        target: f64,
    ) -> PyResult<Option<PyOrder>> {
        wrap_ack(self.inner.set_position(Units {
            symbol: intern(symbol),
            amount: target,
        }))
    }

    /// Send a market order targeting `side` `size` of `symbol`. Returns `None`.
    pub(crate) fn set(
        &mut self,
        symbol: String,
        side: &str,
        size: &Bound<'_, PyAny>,
    ) -> PyResult<Option<PyOrder>> {
        wrap_ack(
            self.inner
                .set(intern(symbol), parse_side(side)?, coerce_size(size)?),
        )
    }

    /// Send a market order flattening `symbol`. Returns `None`.
    pub(crate) fn close(&mut self, symbol: String) -> PyResult<Option<PyOrder>> {
        wrap_ack(self.inner.close(intern(symbol)))
    }

    /// Rest a `reduceOnly` stop-loss on `symbol` at `trigger`. Idempotent,
    /// latest-wins per symbol; re-submit to trail. `size` (a number of units or a
    /// `Size`) is how much of the position the leg takes off, defaulting to all of
    /// it. Returns `None` (working until it triggers).
    #[pyo3(signature = (symbol, trigger, size = None))]
    pub(crate) fn set_stop(
        &mut self,
        symbol: String,
        trigger: f64,
        size: Option<PySize>,
    ) -> PyResult<Option<PyOrder>> {
        let size = size.map_or(Size::position_frac(1.0), |s| s.inner);
        wrap_ack(
            self.inner
                .set_stop(intern(symbol), Reference(trigger), size),
        )
    }

    /// Rest a `reduceOnly` take-profit on `symbol` at `trigger` — the favourable
    /// twin of `set_stop`, same reduce-only `size` semantics. Returns `None`.
    #[pyo3(signature = (symbol, trigger, size = None))]
    pub(crate) fn set_take_profit(
        &mut self,
        symbol: String,
        trigger: f64,
        size: Option<PySize>,
    ) -> PyResult<Option<PyOrder>> {
        let size = size.map_or(Size::position_frac(1.0), |s| s.inner);
        wrap_ack(
            self.inner
                .set_take_profit(intern(symbol), Reference(trigger), size),
        )
    }

    /// Cancel both resting protective legs (stop and take-profit) on `symbol`.
    pub(crate) fn cancel_protective(&mut self, symbol: String) -> PyResult<()> {
        self.inner
            .cancel_protective(&intern(symbol))
            .map_err(|error| PyValueError::new_err(error.to_string()))
    }

    /// Rest a limit order on `symbol`: drive the position to `side · size` once the
    /// market trades through `limit`, filling at that price or better. Idempotent,
    /// latest-wins per symbol. Returns `None` (working until it triggers).
    pub(crate) fn set_limit(
        &mut self,
        symbol: String,
        side: &str,
        size: &Bound<'_, PyAny>,
        limit: f64,
    ) -> PyResult<Option<PyOrder>> {
        wrap_ack(self.inner.set_limit(
            intern(symbol),
            parse_side(side)?,
            coerce_size(size)?,
            Reference(limit),
        ))
    }

    /// Cancel any resting limit order on `symbol`. A no-op when none rests.
    pub(crate) fn cancel_limit(&mut self, symbol: String) -> PyResult<()> {
        self.inner
            .cancel_limit(&intern(symbol))
            .map_err(|error| PyValueError::new_err(error.to_string()))
    }

    /// Cancel a working order by its `id` (see `Order.id`). An unknown id is a
    /// no-op.
    pub(crate) fn cancel(&mut self, id: u64) -> PyResult<()> {
        self.inner
            .cancel(OrderId(id))
            .map_err(|error| PyValueError::new_err(error.to_string()))
    }

    /// Poll every traded symbol for fills booked out of band (not on a specific
    /// `update`) and return them — a fill on a symbol that didn't tick this bar
    /// still reaches the caller here.
    pub(crate) fn poll_fills(&mut self) -> Vec<PyOrder> {
        self.inner
            .poll_fills()
            .into_iter()
            .map(|inner| PyOrder { inner })
            .collect()
    }

    /// `"null"` — a live account's book is **not** snapshotted. The venue owns
    /// the positions and the cash, so replaying them from a state that may have
    /// gone stale would contradict the broker; they are re-read instead. Present
    /// for parity with `PaperWallet.snapshot_state`, and it is what
    /// `RunState.wallet` holds for a live run.
    pub(crate) fn snapshot_state(&self) -> PyResult<String> {
        wallet_snapshot(&self.inner)
    }

    /// Accepts and ignores, for the same reason
    /// [`snapshot_state`](Self::snapshot_state) produces nothing: the account is
    /// re-read from the venue on the next call, which is the only source that
    /// can be trusted about it.
    pub(crate) fn restore_state(&mut self, state: &str) -> PyResult<()> {
        wallet_restore(&mut self.inner, state)
    }
}

/// A live [`Wallet`] over Coinbase Advanced Trade **spot** — the same order-flow
/// surface as [`PaperWallet`](PyWallet), but routed to Coinbase's REST API and
/// authenticated with a per-request ES256 JWT. Construct with
/// [`CoinbaseWallet.mainnet`](Self::mainnet) (**real funds**), passing the CDP
/// key name and its EC private-key PEM (and an optional `quote_ccy`, `USD` by
/// default).
///
/// Spot, not swaps: a `position` is a base-asset **balance** (never negative),
/// `funds` is the quote-currency balance, and `set_position` diffs the target
/// against the held balance and market-orders the difference. A negative target
/// can't be shorted — the wallet sells to flat and records a rejection for the
/// remainder (drained like any other, through the strategy driver).
///
/// Drive it exactly like the paper wallet: [`update`](Self::update) each bar
/// marks price and returns fills; [`set_position`](Self::set_position) /
/// [`set`](Self::set) / [`close`](Self::close) send market orders;
/// [`set_stop`](Self::set_stop) / [`set_take_profit`](Self::set_take_profit) /
/// [`set_limit`](Self::set_limit) rest legs. Submitting returns `None` (working)
/// — the fill lands later, surfaced by a subsequent [`update`](Self::update) or
/// [`poll_fills`](Self::poll_fills). A REST failure surfaces as a `ValueError`
/// (detail also on [`errors`](Self::errors)).
///
/// It owns a private async runtime and blocks on each request, so it must be
/// driven from synchronous Python, one bar at a time.
#[pyclass(name = "CoinbaseWallet", module = "fugazi")]
pub(crate) struct PyCoinbaseWallet {
    pub(crate) inner: CoinbaseWallet,
}

#[pymethods]
impl PyCoinbaseWallet {
    /// A wallet against Coinbase **production** (`api.coinbase.com`). This trades
    /// **real funds** — supply live CDP credentials deliberately. `key_name` is
    /// the CDP key name (`organizations/{org}/apiKeys/{key}`); `private_key_pem`
    /// is that key's EC private key in PEM form. Raises `ValueError` if the PEM
    /// does not parse as a P-256 key.
    #[staticmethod]
    #[pyo3(signature = (key_name, private_key_pem, quote_ccy = None))]
    pub(crate) fn mainnet(
        key_name: String,
        private_key_pem: String,
        quote_ccy: Option<String>,
    ) -> PyResult<Self> {
        let mut inner = CoinbaseWallet::mainnet(key_name, &private_key_pem)
            .map_err(|e| PyWalletError::new_err(e.to_string()))?;
        if let Some(ccy) = quote_ccy {
            inner = inner.with_quote_ccy(ccy);
        }
        Ok(PyCoinbaseWallet { inner })
    }

    /// The available cash balance (the quote-currency balance), from the cache.
    #[getter]
    pub(crate) fn funds(&self) -> f64 {
        self.inner.funds().0
    }

    /// The base-asset balance held for `symbol` (never negative on spot), from
    /// the cache.
    pub(crate) fn position(&self, symbol: &str) -> f64 {
        self.inner.position(&intern(symbol)).amount
    }

    /// The last price fed for `symbol` via `update`, or `None` if never fed.
    pub(crate) fn price(&self, symbol: &str) -> Option<f64> {
        self.inner.price(&intern(symbol)).map(|p| p.0)
    }

    /// Mark-to-market account equity (quote balance plus marked base balances),
    /// from the cache.
    #[getter]
    pub(crate) fn equity(&self) -> f64 {
        self.inner.equity().0
    }

    /// `False` — Advanced Trade is spot, so a position is an owned base-asset
    /// balance that cannot go negative. `set_position` clamps a negative target
    /// to flat and reports the un-shortable remainder; read this first to take a
    /// long-only path instead.
    #[getter]
    pub(crate) fn can_short(&self) -> bool {
        self.inner.can_short()
    }

    /// The quote currency this wallet was built against — `"USD"` unless the
    /// constructor's `quote_ccy` said otherwise. Both `funds` and `equity` are
    /// in it. Unlike OKX's, this is genuinely per-account: Advanced Trade quotes
    /// the same base against several currencies.
    #[getter]
    pub(crate) fn quote_ccy(&self) -> Option<&str> {
        self.inner.quote_ccy()
    }

    /// `["coinbase"]` — the venue this wallet trades. The cleanest of the
    /// pairings: the `coinbase` provider fetches the same Advanced Trade spot
    /// market, keyed on the very product ids this wallet's symbols already are
    /// (`BTC-USD`). It publishes no overlay columns and serves fixed cadences
    /// (1m/5m/15m/30m, 1h/2h/6h, 1d).
    #[getter]
    pub(crate) fn data_sources(&self) -> Vec<&'static str> {
        self.inner.data_sources().to_vec()
    }

    /// `None`, structurally — the same fact `can_short` reports as `False`, said
    /// the other way.
    ///
    /// Advanced Trade is **spot**: a position is an owned base-asset balance, so
    /// there is nothing borrowed and no multiple to configure. `symbol` is
    /// accepted and ignored.
    pub(crate) fn leverage(&self, symbol: &str) -> Option<f64> {
        let _ = symbol;
        None
    }

    /// Force an account-state refresh (balances) now. Raises `ValueError` on a
    /// REST failure. `update` calls this each bar; call it directly for a one-off
    /// sync (e.g. right after construction).
    pub(crate) fn refresh_account(&mut self) -> PyResult<()> {
        self.inner
            .refresh_account()
            .map_err(|e| PyWalletError::new_err(e.to_string()))
    }

    /// The live errors this wallet has recorded, in order — every REST failure
    /// (the detail behind a raised `ValueError`, plus best-effort refresh /
    /// fill-poll failures that have no return channel), as strings.
    pub(crate) fn errors(&self) -> Vec<String> {
        self.inner.errors().iter().map(|e| e.to_string()).collect()
    }

    /// Feed `symbol`'s current bar (whose `close` marks price) and return any
    /// fills polled for it. Accepts a `Candle` or a bare price `float`. Refreshes
    /// the account cache first.
    pub(crate) fn update(
        &mut self,
        symbol: String,
        bar: &Bound<'_, PyAny>,
    ) -> PyResult<Vec<PyOrder>> {
        let candle = if let Ok(candle) = bar.cast::<PyCandle>() {
            candle.borrow().inner
        } else {
            let price: f64 = bar.extract()?;
            Candle::new(price, price, price, price, 0.0)
        };
        Ok(self
            .inner
            .update(intern(symbol), candle)
            .into_iter()
            .map(|inner| PyOrder { inner })
            .collect())
    }

    /// Send a market order driving `symbol` to `target` base units (spot: a
    /// negative target sells to flat). Returns `None` (working — the fill
    /// surfaces from a later `update` / `poll_fills`).
    pub(crate) fn set_position(
        &mut self,
        symbol: String,
        target: f64,
    ) -> PyResult<Option<PyOrder>> {
        wrap_ack(self.inner.set_position(Units {
            symbol: intern(symbol),
            amount: target,
        }))
    }

    /// Send a market order targeting `side` `size` of `symbol`. Returns `None`.
    pub(crate) fn set(
        &mut self,
        symbol: String,
        side: &str,
        size: &Bound<'_, PyAny>,
    ) -> PyResult<Option<PyOrder>> {
        wrap_ack(
            self.inner
                .set(intern(symbol), parse_side(side)?, coerce_size(size)?),
        )
    }

    /// Send a market order flattening `symbol`. Returns `None`.
    pub(crate) fn close(&mut self, symbol: String) -> PyResult<Option<PyOrder>> {
        wrap_ack(self.inner.close(intern(symbol)))
    }

    /// Rest a reduce-only stop-loss on `symbol` at `trigger` (a `stop_limit`
    /// sell). Idempotent, latest-wins per symbol; re-submit to trail. `size` is
    /// how much of the holding the leg takes off, defaulting to all of it.
    /// Returns `None` (working until it triggers).
    #[pyo3(signature = (symbol, trigger, size = None))]
    pub(crate) fn set_stop(
        &mut self,
        symbol: String,
        trigger: f64,
        size: Option<PySize>,
    ) -> PyResult<Option<PyOrder>> {
        let size = size.map_or(Size::position_frac(1.0), |s| s.inner);
        wrap_ack(
            self.inner
                .set_stop(intern(symbol), Reference(trigger), size),
        )
    }

    /// Rest a reduce-only take-profit on `symbol` at `trigger` — the favourable
    /// twin of `set_stop`, same reduce-only `size` semantics. Returns `None`.
    #[pyo3(signature = (symbol, trigger, size = None))]
    pub(crate) fn set_take_profit(
        &mut self,
        symbol: String,
        trigger: f64,
        size: Option<PySize>,
    ) -> PyResult<Option<PyOrder>> {
        let size = size.map_or(Size::position_frac(1.0), |s| s.inner);
        wrap_ack(
            self.inner
                .set_take_profit(intern(symbol), Reference(trigger), size),
        )
    }

    /// Cancel both resting protective legs (stop and take-profit) on `symbol`.
    pub(crate) fn cancel_protective(&mut self, symbol: String) -> PyResult<()> {
        self.inner
            .cancel_protective(&intern(symbol))
            .map_err(|error| PyValueError::new_err(error.to_string()))
    }

    /// Rest a limit order on `symbol`: drive the position to `side · size` once
    /// the market trades through `limit`, filling at that price or better.
    /// Idempotent, latest-wins per symbol. Returns `None` (working until it
    /// triggers).
    pub(crate) fn set_limit(
        &mut self,
        symbol: String,
        side: &str,
        size: &Bound<'_, PyAny>,
        limit: f64,
    ) -> PyResult<Option<PyOrder>> {
        wrap_ack(self.inner.set_limit(
            intern(symbol),
            parse_side(side)?,
            coerce_size(size)?,
            Reference(limit),
        ))
    }

    /// Cancel any resting limit order on `symbol`. A no-op when none rests.
    pub(crate) fn cancel_limit(&mut self, symbol: String) -> PyResult<()> {
        self.inner
            .cancel_limit(&intern(symbol))
            .map_err(|error| PyValueError::new_err(error.to_string()))
    }

    /// Cancel a working order by its `id` (see `Order.id`). An unknown id is a
    /// no-op.
    pub(crate) fn cancel(&mut self, id: u64) -> PyResult<()> {
        self.inner
            .cancel(OrderId(id))
            .map_err(|error| PyValueError::new_err(error.to_string()))
    }

    /// Poll every traded symbol for fills booked out of band (not on a specific
    /// `update`) and return them — a fill on a symbol that didn't tick this bar
    /// still reaches the caller here.
    pub(crate) fn poll_fills(&mut self) -> Vec<PyOrder> {
        self.inner
            .poll_fills()
            .into_iter()
            .map(|inner| PyOrder { inner })
            .collect()
    }

    /// `"null"` — a live account's book is **not** snapshotted. The venue owns
    /// the positions and the cash, so replaying them from a state that may have
    /// gone stale would contradict the broker; they are re-read instead. Present
    /// for parity with `PaperWallet.snapshot_state`, and it is what
    /// `RunState.wallet` holds for a live run.
    pub(crate) fn snapshot_state(&self) -> PyResult<String> {
        wallet_snapshot(&self.inner)
    }

    /// Accepts and ignores, for the same reason
    /// [`snapshot_state`](Self::snapshot_state) produces nothing: the account is
    /// re-read from the venue on the next call, which is the only source that
    /// can be trusted about it.
    pub(crate) fn restore_state(&mut self, state: &str) -> PyResult<()> {
        wallet_restore(&mut self.inner, state)
    }
}

/// A live [`Wallet`] over Kraken **spot** — the same order-flow surface as
/// [`PaperWallet`](PyWallet), but routed to Kraken's Spot REST API and
/// authenticated with an HMAC-SHA512 signature over a SHA256 prehash. Construct
/// with [`KrakenWallet.mainnet`](Self::mainnet) (**real funds** — Kraken
/// publishes no demo environment for its Spot API, unlike OKX), passing the API
/// key and secret (and an optional `quote_ccy`, `USD` by default).
///
/// Cash spot: a `position` is a base-asset **balance** (never negative), `funds`
/// is the quote-currency balance, and `set_position` diffs the target against
/// the held balance and market-orders the difference. A negative target can't be
/// shorted — the wallet sells to flat and records a rejection for the remainder
/// (drained like any other, through the strategy driver). Kraken *does* offer
/// shorting on margin, but that is opt-in per order and this wallet never asks
/// for it, so `can_short` is `False`.
///
/// Drive it exactly like the paper wallet: [`update`](Self::update) each bar
/// marks price and returns fills; [`set_position`](Self::set_position) /
/// [`set`](Self::set) / [`close`](Self::close) send market orders;
/// [`set_stop`](Self::set_stop) / [`set_take_profit`](Self::set_take_profit) /
/// [`set_limit`](Self::set_limit) rest legs. Submitting returns `None` (working)
/// — the fill lands later, surfaced by a subsequent [`update`](Self::update) or
/// [`poll_fills`](Self::poll_fills). A REST failure surfaces as a `ValueError`
/// (detail also on [`errors`](Self::errors)).
///
/// It owns a private async runtime and blocks on each request, so it must be
/// driven from synchronous Python, one bar at a time.
#[pyclass(name = "KrakenWallet", module = "fugazi")]
pub(crate) struct PyKrakenWallet {
    pub(crate) inner: KrakenWallet,
}

#[pymethods]
impl PyKrakenWallet {
    /// A wallet against Kraken **production** (`api.kraken.com`). This trades
    /// **real funds** — Kraken has no demo Spot endpoint, so there is no safe
    /// rehearsal mode here the way `OkxWallet.demo` gives one.
    ///
    /// `api_key` and `api_secret` are the pair issued when the API key was
    /// created; pass the secret as the base64 blob Kraken displays. Raises
    /// `ValueError` if that secret is not valid base64.
    #[staticmethod]
    #[pyo3(signature = (api_key, api_secret, quote_ccy = None))]
    pub(crate) fn mainnet(
        api_key: String,
        api_secret: String,
        quote_ccy: Option<String>,
    ) -> PyResult<Self> {
        let mut inner = KrakenWallet::mainnet(api_key, &api_secret)
            .map_err(|e| PyWalletError::new_err(e.to_string()))?;
        if let Some(ccy) = quote_ccy {
            inner = inner.with_quote_ccy(ccy);
        }
        Ok(PyKrakenWallet { inner })
    }

    /// The available cash balance (the quote-currency balance), from the cache.
    #[getter]
    pub(crate) fn funds(&self) -> f64 {
        self.inner.funds().0
    }

    /// The base-asset balance held for `symbol` (never negative on spot), from
    /// the cache.
    pub(crate) fn position(&self, symbol: &str) -> f64 {
        self.inner.position(&intern(symbol)).amount
    }

    /// The last price fed for `symbol` via `update`, or `None` if never fed.
    pub(crate) fn price(&self, symbol: &str) -> Option<f64> {
        self.inner.price(&intern(symbol)).map(|p| p.0)
    }

    /// Mark-to-market account equity (quote balance plus marked base balances),
    /// from the cache.
    #[getter]
    pub(crate) fn equity(&self) -> f64 {
        self.inner.equity().0
    }

    /// `False` — this wallet trades Kraken as cash spot, so a position is an
    /// owned base-asset balance that cannot go negative. `set_position` clamps a
    /// negative target to flat and reports the un-shortable remainder; read this
    /// first to take a long-only path instead. Shorting Kraken needs margin,
    /// which is opt-in per order and not something this wallet requests.
    #[getter]
    pub(crate) fn can_short(&self) -> bool {
        self.inner.can_short()
    }

    /// The quote currency this wallet was built against — `"USD"` unless the
    /// constructor's `quote_ccy` said otherwise. Both `funds` and `equity` are
    /// in it. Like Coinbase's and unlike OKX's, this is genuinely per-account:
    /// Kraken quotes the same base against USD, EUR, USDT and more.
    #[getter]
    pub(crate) fn quote_ccy(&self) -> Option<&str> {
        self.inner.quote_ccy()
    }

    /// `["kraken"]` — the venue this wallet trades. The `kraken` provider
    /// fetches the same spot market, keyed on the very pair names this wallet's
    /// symbols already are (`XBTUSD`). Note it reaches back only 720 bars, so a
    /// strategy with a long warm-up should be primed from a file rather than a
    /// live fetch.
    #[getter]
    pub(crate) fn data_sources(&self) -> Vec<&'static str> {
        self.inner.data_sources().to_vec()
    }

    /// `None`, structurally — the same fact `can_short` reports as `False`, said
    /// the other way.
    ///
    /// A cash spot balance is not borrowed, so there is no multiple to report.
    /// `symbol` is accepted and ignored.
    pub(crate) fn leverage(&self, symbol: &str) -> Option<f64> {
        let _ = symbol;
        None
    }

    /// Force an account-state refresh (balances) now. Raises `ValueError` on a
    /// REST failure. `update` calls this each bar; call it directly for a one-off
    /// sync (e.g. right after construction).
    pub(crate) fn refresh_account(&mut self) -> PyResult<()> {
        self.inner
            .refresh_account()
            .map_err(|e| PyWalletError::new_err(e.to_string()))
    }

    /// The live errors this wallet has recorded, in order — every REST failure
    /// (the detail behind a raised `ValueError`, plus best-effort refresh /
    /// fill-poll failures that have no return channel), as strings.
    pub(crate) fn errors(&self) -> Vec<String> {
        self.inner.errors().iter().map(|e| e.to_string()).collect()
    }

    /// Feed `symbol`'s current bar (whose `close` marks price) and return any
    /// fills polled for it. Accepts a `Candle` or a bare price `float`. Refreshes
    /// the account cache first.
    pub(crate) fn update(
        &mut self,
        symbol: String,
        bar: &Bound<'_, PyAny>,
    ) -> PyResult<Vec<PyOrder>> {
        let candle = if let Ok(candle) = bar.cast::<PyCandle>() {
            candle.borrow().inner
        } else {
            let price: f64 = bar.extract()?;
            Candle::new(price, price, price, price, 0.0)
        };
        Ok(self
            .inner
            .update(intern(symbol), candle)
            .into_iter()
            .map(|inner| PyOrder { inner })
            .collect())
    }

    /// Send a market order driving `symbol` to `target` base units (spot: a
    /// negative target sells to flat). Returns `None` (working — the fill
    /// surfaces from a later `update` / `poll_fills`).
    pub(crate) fn set_position(
        &mut self,
        symbol: String,
        target: f64,
    ) -> PyResult<Option<PyOrder>> {
        wrap_ack(self.inner.set_position(Units {
            symbol: intern(symbol),
            amount: target,
        }))
    }

    /// Send a market order targeting `side` `size` of `symbol`. Returns `None`.
    pub(crate) fn set(
        &mut self,
        symbol: String,
        side: &str,
        size: &Bound<'_, PyAny>,
    ) -> PyResult<Option<PyOrder>> {
        wrap_ack(
            self.inner
                .set(intern(symbol), parse_side(side)?, coerce_size(size)?),
        )
    }

    /// Send a market order flattening `symbol`. Returns `None`.
    pub(crate) fn close(&mut self, symbol: String) -> PyResult<Option<PyOrder>> {
        wrap_ack(self.inner.close(intern(symbol)))
    }

    /// Rest a reduce-only stop-loss on `symbol` at `trigger` (a `stop_limit`
    /// sell). Idempotent, latest-wins per symbol; re-submit to trail. `size` is
    /// how much of the holding the leg takes off, defaulting to all of it.
    /// Returns `None` (working until it triggers).
    #[pyo3(signature = (symbol, trigger, size = None))]
    pub(crate) fn set_stop(
        &mut self,
        symbol: String,
        trigger: f64,
        size: Option<PySize>,
    ) -> PyResult<Option<PyOrder>> {
        let size = size.map_or(Size::position_frac(1.0), |s| s.inner);
        wrap_ack(
            self.inner
                .set_stop(intern(symbol), Reference(trigger), size),
        )
    }

    /// Rest a reduce-only take-profit on `symbol` at `trigger` — the favourable
    /// twin of `set_stop`, same reduce-only `size` semantics. Returns `None`.
    #[pyo3(signature = (symbol, trigger, size = None))]
    pub(crate) fn set_take_profit(
        &mut self,
        symbol: String,
        trigger: f64,
        size: Option<PySize>,
    ) -> PyResult<Option<PyOrder>> {
        let size = size.map_or(Size::position_frac(1.0), |s| s.inner);
        wrap_ack(
            self.inner
                .set_take_profit(intern(symbol), Reference(trigger), size),
        )
    }

    /// Cancel both resting protective legs (stop and take-profit) on `symbol`.
    pub(crate) fn cancel_protective(&mut self, symbol: String) -> PyResult<()> {
        self.inner
            .cancel_protective(&intern(symbol))
            .map_err(|error| PyValueError::new_err(error.to_string()))
    }

    /// Rest a limit order on `symbol`: drive the position to `side · size` once
    /// the market trades through `limit`, filling at that price or better.
    /// Idempotent, latest-wins per symbol. Returns `None` (working until it
    /// triggers).
    pub(crate) fn set_limit(
        &mut self,
        symbol: String,
        side: &str,
        size: &Bound<'_, PyAny>,
        limit: f64,
    ) -> PyResult<Option<PyOrder>> {
        wrap_ack(self.inner.set_limit(
            intern(symbol),
            parse_side(side)?,
            coerce_size(size)?,
            Reference(limit),
        ))
    }

    /// Cancel any resting limit order on `symbol`. A no-op when none rests.
    pub(crate) fn cancel_limit(&mut self, symbol: String) -> PyResult<()> {
        self.inner
            .cancel_limit(&intern(symbol))
            .map_err(|error| PyValueError::new_err(error.to_string()))
    }

    /// Cancel a working order by its `id` (see `Order.id`). An unknown id is a
    /// no-op.
    pub(crate) fn cancel(&mut self, id: u64) -> PyResult<()> {
        self.inner
            .cancel(OrderId(id))
            .map_err(|error| PyValueError::new_err(error.to_string()))
    }

    /// Poll every traded symbol for fills booked out of band (not on a specific
    /// `update`) and return them — a fill on a symbol that didn't tick this bar
    /// still reaches the caller here.
    pub(crate) fn poll_fills(&mut self) -> Vec<PyOrder> {
        self.inner
            .poll_fills()
            .into_iter()
            .map(|inner| PyOrder { inner })
            .collect()
    }

    /// `"null"` — a live account's book is **not** snapshotted. The venue owns
    /// the positions and the cash, so replaying them from a state that may have
    /// gone stale would contradict the broker; they are re-read instead. Present
    /// for parity with `PaperWallet.snapshot_state`, and it is what
    /// `RunState.wallet` holds for a live run.
    pub(crate) fn snapshot_state(&self) -> PyResult<String> {
        wallet_snapshot(&self.inner)
    }

    /// Accepts and ignores, for the same reason
    /// [`snapshot_state`](Self::snapshot_state) produces nothing: the account is
    /// re-read from the venue on the next call, which is the only source that
    /// can be trusted about it.
    pub(crate) fn restore_state(&mut self, state: &str) -> PyResult<()> {
        wallet_restore(&mut self.inner, state)
    }
}
