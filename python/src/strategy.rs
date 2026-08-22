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
// Aliased: the prelude glob already binds `fugazi_core::wallet::WalletError`,
// which `wrap_ack` names in its signature. This is the Python exception type.
use crate::errors::WalletError as PyWalletError;

// ---------------------------------------------------------------------------
// Wallet-preparation seam — the one place the "external positions" concern lives.
//
// Every `.run(...)` treats whatever the wallet *already holds at run start* as
// the user's own, externally-managed book: it is snapshotted as a baseline and
// left untouched, and the strategy sizes against its **own** capital (cash + only
// what it opens). A flat wallet has an empty baseline, so this collapses to the
// fast path — drive the wallet directly, no offset, no move — behaving byte for
// byte as before. A non-flat wallet is driven through an [`SleeveWallet`].
// Neither the strategy nor the portfolio run code decides any of this; they call
// one of the two seams below.
// ---------------------------------------------------------------------------

/// Run `$body` over one already type-resolved wallet cell, with `$w` bound to
/// the wallet to trade. Snapshots the baseline, binds `$seed` to *our* opening
/// equity (account minus the external value — identical to plain equity when
/// flat), and either lends out the wallet in place (flat, the fast path) or
/// moves it into an [`SleeveWallet`] for the duration and back afterward
/// (non-flat). `$body` may also use `py`, where it is in scope.
macro_rules! over_prepared_wallet {
    ($cell:expr, $placeholder:expr, $seed:ident, $w:ident => $body:expr) => {{
        let mut guard = $cell.borrow_mut();
        let baseline = external_baseline(&guard.inner);
        let $seed = own_equity(&guard.inner, &baseline);
        if baseline.is_empty() {
            let $w = &mut guard.inner;
            $body
        } else {
            let real = std::mem::replace(&mut guard.inner, $placeholder);
            let mut sleeve = SleeveWallet::new(real, baseline);
            let out = {
                let $w = &mut sleeve;
                $body
            };
            guard.inner = sleeve.into_inner();
            out
        }
    }};
}

/// Resolve `$wallet` to one of the three concrete pyclasses — [`PaperWallet`](PyWallet),
/// [`OkxWallet`](PyOkxWallet), [`CoinbaseWallet`](PyCoinbaseWallet); anything
/// else is a `TypeError` — and hand off to [`over_prepared_wallet!`]. For a live
/// wallet it first refreshes the account so `positions()`/`equity()` reflect the
/// venue before the baseline is snapshotted. `$py` binds the GIL token (pass
/// `_py` when the body doesn't need it).
///
/// This is what lets both the hand-built shapes (via [`run_over_wallet!`]) and
/// the spec surface (`StrategySpec.run` / `.run_resumable` / `.warm_up`) accept
/// a live venue: `backtest::run` is generic over the wallet, so each arm
/// monomorphizes the body for its own concrete type.
macro_rules! over_any_wallet {
    ($wallet:expr, $py:ident, $seed:ident, $w:ident => $body:expr) => {{
        let wallet = $wallet;
        let $py = wallet.py();
        if let Ok(cell) = wallet.cast::<PyWallet>() {
            over_prepared_wallet!(cell, PaperWallet::new(0.0), $seed, $w => $body)
        } else if let Ok(cell) = wallet.cast::<PyOkxWallet>() {
            cell.borrow_mut()
                .inner
                .refresh_account()
                // Fully qualified: this macro also expands inside `spec.rs`,
                // which has no `PyWalletError` alias in scope.
                .map_err(|e| crate::errors::WalletError::new_err(e.to_string()))?;
            over_prepared_wallet!(cell, OkxWallet::demo("", "", ""), $seed, $w => $body)
        } else if let Ok(cell) = wallet.cast::<PyCoinbaseWallet>() {
            cell.borrow_mut()
                .inner
                .refresh_account()
                // Fully qualified: this macro also expands inside `spec.rs`,
                // which has no `PyWalletError` alias in scope.
                .map_err(|e| crate::errors::WalletError::new_err(e.to_string()))?;
            over_prepared_wallet!(cell, CoinbaseWallet::placeholder(), $seed, $w => $body)
        } else {
            Err(PyTypeError::new_err(
                "wallet must be a PaperWallet, an OkxWallet, or a CoinbaseWallet",
            ))
        }
    }};
}

/// [`over_any_wallet!`] specialized to "build a strategy and run it" — the
/// original shape, kept so the five hand-built call sites read unchanged.
macro_rules! run_over_wallet {
    ($wallet:expr, $py:ident, $snaps:expr, $seed:ident => $strat:expr) => {
        over_any_wallet!($wallet, $py, $seed, wallet => {
            // Built with the GIL held — a basket's per-symbol factories are
            // Python callables, and this is where they run.
            let mut strat = $strat;
            // Then dropped for the drive itself, so a long run stops blocking
            // every other thread in the process. `interruptible` re-attaches
            // every few thousand bars to poll for Ctrl-C; the parked error is
            // re-raised here. The core loop stays Python-unaware throughout.
            let interrupt = std::sync::Mutex::new(None);
            let report = $py.detach(|| {
                fugazi_core::backtest::run(
                    &mut strat,
                    wallet,
                    crate::classes::interruptible($snaps, &interrupt),
                )
            });
            crate::classes::raise_if_interrupted(&interrupt, PyRunReport { inner: report })
        })
    };
}

// ---------------------------------------------------------------------------
// Strategy layer: Wallet + Order + Size
//
// A strategy in Python is just code that, each bar, reads signals/indicators and
// acts on a Wallet. So rather than binding a Rust strategy trait, we expose the
// Wallet the strategy trades into. Symbols are plain strings; sides are "buy" /
// "sell"; sizes are a unit count or a `Size`.
// ---------------------------------------------------------------------------

/// How much to trade: a bare number is units, or use the relative constructors.
#[pyclass(name = "Size", module = "fugazi", frozen, from_py_object)]
#[derive(Clone, Copy)]
pub(crate) struct PySize {
    pub(crate) inner: Size,
}

#[pymethods]
impl PySize {
    /// An absolute number of units.
    #[staticmethod]
    pub(crate) fn units(units: f64) -> Self {
        PySize {
            inner: Size::Units(units),
        }
    }
    /// A fraction of available funds (cash), converted to units at the price.
    #[staticmethod]
    pub(crate) fn funds_frac(fraction: f64) -> Self {
        PySize {
            inner: Size::FundsFraction(fraction),
        }
    }
    /// A fraction of total equity, converted to units at the price.
    /// `value_frac(1.0)` is "all-in" and reverses cleanly on a flip.
    #[staticmethod]
    pub(crate) fn value_frac(fraction: f64) -> Self {
        PySize {
            inner: Size::ValueFraction(fraction),
        }
    }
    /// A fraction of the symbol's current position (adjust-only).
    #[staticmethod]
    pub(crate) fn position_frac(fraction: f64) -> Self {
        PySize {
            inner: Size::PositionFraction(fraction),
        }
    }

    /// Which of the four constructors made this — `"units"`, `"funds_frac"`,
    /// `"value_frac"` or `"position_frac"`.
    ///
    /// A `Size` was write-only before: four static constructors in, and no way
    /// to ask an existing one what it meant. `kind` + `value` close that, and
    /// are what `__reduce__` reconstructs from.
    #[getter]
    pub(crate) fn kind(&self) -> &'static str {
        match self.inner {
            Size::Units(_) => "units",
            Size::FundsFraction(_) => "funds_frac",
            Size::ValueFraction(_) => "value_frac",
            Size::PositionFraction(_) => "position_frac",
        }
    }

    /// The number the constructor was handed — a unit count for `"units"`, a
    /// fraction otherwise.
    #[getter]
    pub(crate) fn value(&self) -> f64 {
        match self.inner {
            Size::Units(v)
            | Size::FundsFraction(v)
            | Size::ValueFraction(v)
            | Size::PositionFraction(v) => v,
        }
    }

    pub(crate) fn __reduce__(&self, py: Python<'_>) -> PyResult<Py<PyAny>> {
        crate::classes::reduce_with(
            py,
            py.import("fugazi")?.getattr("_rebuild_size")?,
            (self.kind(), self.value()),
        )
    }

    pub(crate) fn __repr__(&self) -> String {
        format!("Size.{}({})", self.kind(), self.value())
    }
}

/// A filled order: `symbol`, `side` ("buy"/"sell"), and a positive `units`.
///
/// A wallet produces these on `update()`, but they are plain data and can be
/// built directly — which is how fills that were *stored* (a Parquet blotter, a
/// resumed run, a database) get back into
/// [`metrics.reconstruct_trades`](reconstruct_trades) and
/// [`metrics.exposure_ratio`](exposure_ratio):
///
/// ```python
/// order = fugazi.Order(symbol="BTC", side="buy", units=1.0, price=100.0)
/// trades = fugazi.metrics.reconstruct_trades([fugazi.Fill(bar=0, order=order)])
/// ```
///
/// Only `symbol`, `side`, `units` and `price` are required; `kind` defaults to
/// `"market"`, and `id` / `commission` to `0`.
#[pyclass(name = "Order", module = "fugazi", frozen, skip_from_py_object)]
#[derive(Clone)]
pub(crate) struct PyOrder {
    pub(crate) inner: Order<Symbol>,
}

#[pymethods]
impl PyOrder {
    /// A `side` order for `units` units of `symbol`, filled at `price`.
    #[new]
    #[pyo3(signature = (symbol, side, units, price, *, kind = "market", id = 0, commission = 0.0, requested_units = None))]
    #[allow(clippy::too_many_arguments)]
    pub(crate) fn new(
        symbol: String,
        side: &str,
        units: f64,
        price: f64,
        kind: &str,
        id: u64,
        commission: f64,
        requested_units: Option<f64>,
    ) -> PyResult<Self> {
        let inner = Order::new(
            intern(symbol),
            parse_side(side)?,
            units,
            price,
            parse_kind(kind)?,
            OrderId(id),
        )
        .with_commission(commission);
        Ok(PyOrder {
            inner: match requested_units {
                Some(requested) => inner.with_requested_units(requested),
                None => inner,
            },
        })
    }

    #[getter]
    pub(crate) fn symbol(&self) -> String {
        self.inner.symbol.to_string()
    }
    #[getter]
    pub(crate) fn side(&self) -> &'static str {
        side_str(self.inner.side)
    }
    #[getter]
    pub(crate) fn units(&self) -> f64 {
        self.inner.units
    }
    /// The per-unit price this order filled at.
    #[getter]
    pub(crate) fn price(&self) -> f64 {
        self.inner.price
    }
    /// What produced this fill: `"market"`, `"stop"`, or `"take_profit"`.
    #[getter]
    pub(crate) fn kind(&self) -> &'static str {
        kind_str(self.inner.kind)
    }
    /// The id of the submission this fill belongs to — pass it to
    /// `PaperWallet.cancel(id)` to cancel a still-working order.
    #[getter]
    pub(crate) fn id(&self) -> u64 {
        self.inner.id.0
    }
    /// Commission paid on this fill, in reference currency. Zero on a wallet
    /// built with `PaperWallet(funds)`; populated when the wallet was built with
    /// a non-trivial `commission` leg in its `TradingCostsConfig`.
    #[getter]
    pub(crate) fn commission(&self) -> f64 {
        self.inner.commission
    }
    /// How many units this order **would** have traded had the wallet not
    /// fitted it to the account — always `>= units`, and exactly `units` on a
    /// fill taken at face value.
    ///
    /// A fractional sizing (`Size.value_frac` / `Size.funds_frac`) is fitted to
    /// what the account can carry rather than refused: an all-in has to shed a
    /// sliver to leave room for commission, and a `sizing:` above what the
    /// wallet's `max_gross` allows is scaled back to it. Both used to be
    /// invisible. Compare against `units`, or read `fill_ratio`, and decide for
    /// yourself which gaps are material.
    #[getter]
    pub(crate) fn requested_units(&self) -> f64 {
        self.inner.requested_units
    }
    /// `units / requested_units`, in `(0, 1]` — `1.0` when nothing was fitted.
    #[getter]
    pub(crate) fn fill_ratio(&self) -> f64 {
        self.inner.fill_ratio()
    }
    /// `+units` for a buy, `-units` for a sell.
    pub(crate) fn __reduce__(&self, py: Python<'_>) -> PyResult<Py<PyAny>> {
        crate::classes::reduce_with(
            py,
            py.import("fugazi")?.getattr("_rebuild_order")?,
            (
                self.symbol(),
                self.side(),
                self.units(),
                self.price(),
                self.kind(),
                self.id(),
                self.commission(),
                self.requested_units(),
            ),
        )
    }

    #[getter]
    pub(crate) fn signed_units(&self) -> f64 {
        self.inner.signed_units()
    }
    pub(crate) fn __repr__(&self) -> String {
        // `commission` is elided when zero — the common case — so the repr of an
        // uncosted fill stays short, and the costed one stays `eval`-able.
        let commission = if self.inner.commission == 0.0 {
            String::new()
        } else {
            format!(", commission={}", self.inner.commission)
        };
        // Elided when it matches `units` — the un-fitted case, and the common
        // one — for the same reason: an ordinary fill's repr stays short, and
        // the interesting one stays `eval`-able.
        let requested = if self.inner.requested_units == self.inner.units {
            String::new()
        } else {
            format!(", requested_units={}", self.inner.requested_units)
        };
        format!(
            "Order(symbol='{}', side='{}', units={}, price={}, kind='{}', id={}{}{})",
            self.inner.symbol,
            self.side(),
            self.inner.units,
            self.inner.price,
            self.kind(),
            self.inner.id.0,
            commission,
            requested,
        )
    }
}

/// A paper-trading wallet a strategy trades into: funds, per-symbol positions,
/// the prices fed to it, and a blotter of executed orders. (The live-broker
/// counterpart would be a separate wallet type implementing the same interface.)
///
/// Feed each symbol's bar every tick with `update(symbol, candle_or_price)`,
/// which returns the orders that filled on it (the fill stream); the wallet is
/// otherwise market-agnostic. `set(symbol, side, size)` targets an absolute
/// position (an opposite-side `set` reverses; `Size.value_frac(1.0)` is all-in),
/// `set_position(symbol, target)` drives to an absolute unit count, and `close`
/// flattens. These are **market orders**: they queue and fill on the next
/// `update`, at that bar's `open` (so a backtest never fills on the same bar whose
/// `close` triggered the signal), returning `None` — the filled `Order` shows up
/// in that `update`'s return (and in `orders()`). Protective exits are **resting
/// orders**: `set_stop(symbol, trigger)` and `set_take_profit(symbol, trigger)`
/// register a level (idempotent, latest-wins per symbol; re-submit to trail) that
/// the wallet triggers and prices itself — filling at the level, or the bar's
/// `open` on a gap — and `cancel_protective(symbol)` drops both legs. Each `Order`
/// carries a `kind` of `"market"`, `"stop"`, or `"take_profit"`.
#[pyclass(name = "PaperWallet", module = "fugazi")]
pub(crate) struct PyWallet {
    pub(crate) inner: PaperWallet<Symbol>,
}

#[pymethods]
impl PyWallet {
    /// A wallet seeded with `funds` of cash and no positions.
    ///
    /// `quote_ccy` labels the currency that cash is in (`"USD"`, `"EUR"`,
    /// `"USDT"`), readable back off `.quote_ccy`. Purely descriptive — nothing
    /// converts, and a labelled wallet trades identically to an unlabelled one;
    /// it is there so a simulation can carry the fact a live wallet reports from
    /// its venue instead of leaving a caller to assume dollars.
    ///
    /// `max_gross` is the most gross notional the account may hold, as a
    /// multiple of equity — `1.0`, unlevered, by default, and readable back off
    /// `leverage(symbol)`. It is the one bound both sides of the book share: a
    /// buy is limited by the cash it spends, but a short *credits* cash, so
    /// without it a `sizing: 3.0` document took 1x long and 3x short under one
    /// spec value. Set it to the leverage of the live account you are modelling
    /// so the two curves measure the same strategy.
    ///
    /// # Raises
    ///
    /// `ValueError` if `max_gross` is not finite and strictly positive.
    #[new]
    #[pyo3(signature = (funds, *, quote_ccy=None, max_gross=1.0))]
    pub(crate) fn new(funds: f64, quote_ccy: Option<String>, max_gross: f64) -> PyResult<Self> {
        if !(max_gross > 0.0 && max_gross.is_finite()) {
            return Err(PyValueError::new_err(format!(
                "max_gross must be finite and > 0, got {max_gross}"
            )));
        }
        let inner = PaperWallet::new(funds).with_max_gross(max_gross);
        Ok(PyWallet {
            inner: match quote_ccy {
                Some(ccy) => inner.with_quote_ccy(ccy),
                None => inner,
            },
        })
    }

    /// The available cash balance.
    #[getter]
    pub(crate) fn funds(&self) -> f64 {
        self.inner.funds().0
    }

    /// The signed position in `symbol` (positive long, negative short).
    pub(crate) fn position(&self, symbol: &str) -> f64 {
        self.inner.position(&intern(symbol)).amount
    }

    /// The last price fed for `symbol`, or `None` if never fed.
    pub(crate) fn price(&self, symbol: &str) -> Option<f64> {
        self.inner.price(&intern(symbol)).map(|p| p.0)
    }

    /// The held positions as a `{symbol: quantity}` dict.
    pub(crate) fn positions<'py>(&self, py: Python<'py>) -> PyResult<Bound<'py, PyDict>> {
        let dict = PyDict::new(py);
        for position in self.inner.positions() {
            dict.set_item(position.symbol.as_ref(), position.amount)?;
        }
        Ok(dict)
    }

    /// Whether this wallet can carry a short (negative) position — `True` here:
    /// a paper sell credits cash, so a position may go as negative as the
    /// strategy asks. The live wallets answer for their venue (`OkxWallet`
    /// trades swaps and says `True`; the spot `CoinbaseWallet` says `False`), so
    /// a caller can pick a long-only path before trading rather than after a
    /// clamped order.
    #[getter]
    pub(crate) fn can_short(&self) -> bool {
        self.inner.can_short()
    }

    /// The currency `funds` and `equity` are counted in, or `None` when nobody
    /// said — which is the default for a paper wallet, since simulated money has
    /// no venue to ask. `None` means "unlabelled", never "no currency": the
    /// numbers are always in *some* unit. Pass `quote_ccy=` to the constructor
    /// to set it.
    /// The market-data providers that quote what this account trades, named as
    /// a `fugazi get` spec names them (`"okx"`, `"coinbase"`, …). Empty here:
    /// simulated money has no venue whose prices are the *right* ones, and a
    /// paper run is fed by whoever ran it. Empty means "does not say", never
    /// "nothing quotes this market" — the same reading `quote_ccy=None` asks
    /// for. The live wallets each name their venue.
    ///
    /// Introspection, not fetching: answering binds nothing, it lets a caller
    /// check the pairing it was about to make.
    #[getter]
    pub(crate) fn data_sources(&self) -> Vec<&'static str> {
        self.inner.data_sources().to_vec()
    }

    /// The gross-exposure multiple this wallet enforces — the `max_gross` it was
    /// built with, the same for every symbol, so `symbol` is accepted and
    /// ignored.
    ///
    /// Unlike `quote_ccy`, which a paper wallet can only answer if it was told,
    /// this one it *knows*: the number is a rule it applies to every fill.
    /// `1.0` from a default wallet is a claim about behaviour — an unlevered
    /// book — not the "does not say" that a live wallet's `None` means.
    ///
    /// Which is the point of answering: a live account's leverage is set out of
    /// band and readable only from the venue, so the paper side has to be able
    /// to state its own for the two to be compared.
    pub(crate) fn leverage(&self, symbol: &str) -> Option<f64> {
        self.inner.leverage(&intern(symbol))
    }

    #[getter]
    pub(crate) fn quote_ccy(&self) -> Option<&str> {
        self.inner.quote_ccy()
    }

    /// How many blotter / rejection entries this wallet keeps, or `None` if it
    /// keeps every one.
    ///
    /// Both logs are reporting artifacts that nothing in the fill, pricing or
    /// resume path reads, so they are bounded by default rather than growing
    /// forever in a long-lived run. Set to `None` to opt out and retain the full
    /// in-process history.
    #[getter]
    pub(crate) fn retention(&self) -> Option<usize> {
        self.inner.retention()
    }

    #[setter]
    pub(crate) fn set_retention(&mut self, entries: Option<usize>) {
        self.inner.set_retention(entries);
    }

    /// The most recent executed orders, oldest first (the trade blotter).
    ///
    /// Bounded by `.retention`, and not carried across a run-resume — after
    /// resuming, this reports the resumed chunk. It is for reporting; durable
    /// trade history is the caller's to keep.
    pub(crate) fn orders(&self) -> Vec<PyOrder> {
        self.inner
            .orders()
            .iter()
            .cloned()
            .map(|inner| PyOrder { inner })
            .collect()
    }

    /// Install the trading costs applied to `symbol`'s fills — commission,
    /// spread and slippage. Without this a wallet is frictionless, which
    /// flatters every backtest run through it.
    ///
    /// `costs` is a `TradingCostsConfig` or the equivalent dict (the same shape
    /// `optimize(costs=...)` and the CLI's YAML take). `freq` is the bar cadence
    /// as a token (`"1d"`, `"4h"`, …) or a `Frequency`, needed only by
    /// cadence-dependent models such as funding rates; leave it `None`
    /// otherwise. Call once per symbol before driving — the resolution honours
    /// the config's `by_symbol` scoping, so the same config can be installed on
    /// every leg and still give each its own bundle:
    ///
    /// ```python
    /// wallet = ta.PaperWallet(10_000.0)
    /// wallet.set_costs_for("BTC", {"commission": {"percentage": {"rate": 0.001}}})
    /// report = strat.run(wallet, candles)
    /// report.fills[0].order.commission     # what that fill actually paid
    /// ```
    #[pyo3(signature = (symbol, costs, freq = None))]
    pub(crate) fn set_costs_for(
        &mut self,
        symbol: &str,
        costs: &Bound<'_, PyAny>,
        freq: Option<&Bound<'_, PyAny>>,
    ) -> PyResult<()> {
        let config = coerce_cost_config(Some(costs))?;
        let freq = freq
            .filter(|f| !f.is_none())
            .map(coerce_frequency)
            .transpose()?;
        let resolved = config.resolve(symbol, freq);
        self.inner
            .set_costs_for(intern(symbol), resolved)
            .map_err(|e| PyWalletError::new_err(e.to_string()))
    }

    /// [`set_costs_for`](Self::set_costs_for) over a whole universe — the same
    /// `costs` resolved **separately per symbol**, in one call.
    ///
    /// ```python
    /// wallet.set_costs_for_all(["BTC", "ETH", "SOL"], config)
    /// ```
    ///
    /// This is a loop, deliberately, and not a mirror of Rust's
    /// `PaperWallet::with_costs(funds, costs)`. That form installs *one*
    /// pre-resolved bundle on every symbol, which cannot be built here: resolving
    /// a `CostConfig` needs a symbol to resolve against, so a whole-wallet
    /// version would have to pick a placeholder and silently take the config's
    /// `default:` leg — quietly wrong for any config using `by_symbol`. Looping
    /// gives each symbol its own resolution, which is what the scoping is for.
    ///
    /// Rejects a duplicate symbol rather than installing twice: in a call whose
    /// whole purpose is a universe, a repeat is a typo in the list.
    #[pyo3(signature = (symbols, costs, freq = None))]
    pub(crate) fn set_costs_for_all(
        &mut self,
        symbols: Vec<String>,
        costs: &Bound<'_, PyAny>,
        freq: Option<&Bound<'_, PyAny>>,
    ) -> PyResult<()> {
        let mut seen = std::collections::HashSet::with_capacity(symbols.len());
        for symbol in &symbols {
            if !seen.insert(symbol.as_str()) {
                return Err(PyValueError::new_err(format!(
                    "set_costs_for_all: symbol {symbol:?} appears more than once"
                )));
            }
        }
        for symbol in &symbols {
            self.set_costs_for(symbol, costs, freq)?;
        }
        Ok(())
    }

    /// Restore the wallet to its freshly-constructed state — the seed funds it
    /// was built with, no positions, no fed prices, no pending or resting
    /// orders, and an empty blotter.
    pub(crate) fn reset(&mut self) {
        self.inner.reset();
    }

    /// Mark-to-market equity: funds plus each position valued at its fed price.
    #[getter]
    pub(crate) fn equity(&self) -> f64 {
        self.inner.equity().0
    }

    /// Feed `symbol`'s current bar and return the orders that filled on it (the
    /// fill stream — a queued market order at this bar's `open`, and any resting
    /// stop / take-profit this bar triggers). Accepts a `Candle` (whose `close`
    /// marks to market and whose `[low, high]` bounds fills) or a bare price
    /// `float` (a flat bar `open = high = low = close`). Call this each tick before
    /// trading or reading `equity`.
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

    /// Queue a market order driving `symbol` to `target` signed units; it fills on
    /// the next `update`, at that bar's `open`. Returns `None` (working — the fill
    /// shows up in that `update`'s return, not here).
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

    /// Queue a market order targeting `side` `size` of `symbol`; it fills on the
    /// next `update`, at that bar's `open` (where the `size` is resolved, so an
    /// all-in stays exact). Returns `None` — working.
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

    /// Queue a market order flattening `symbol`; it fills on the next `update`, at
    /// that bar's `open`. Returns `None` — working.
    pub(crate) fn close(&mut self, symbol: String) -> PyResult<Option<PyOrder>> {
        wrap_ack(self.inner.close(intern(symbol)))
    }

    /// Rest a stop-loss on `symbol` at `trigger` — an adverse level the wallet
    /// fills when a bar trades through it (the side is read from the current
    /// position). Idempotent, latest-wins per symbol; re-submit to trail. Returns
    /// `None` (the resting order is working until it triggers in some `update`).
    ///
    /// `size` is how much of the position the leg takes off, defaulting to all
    /// of it. It is **reduce-only**: resolved at the fill price and clamped to
    /// the position's magnitude, so a leg can flatten but never flip. An
    /// explicit `Size.units(n)` is a *partial* exit — what lets several owners
    /// rest their own share against one position.
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

    /// Rest a take-profit on `symbol` at `trigger` — the favourable twin of
    /// `set_stop`, with the same reduce-only `size` semantics. Idempotent,
    /// latest-wins per symbol. Returns `None` (working).
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

    /// Credit (positive) or debit (negative) the cash balance with no order
    /// flow — an external funding event, or the cash leg of a rebalance.
    /// Raises `ValueError` if a debit would take the balance negative.
    pub(crate) fn adjust_funds(&mut self, delta: f64) -> PyResult<()> {
        self.inner
            .adjust_funds(delta)
            .map_err(|e| PyWalletError::new_err(e.to_string()))
    }

    /// Rest a limit order on `symbol`: drive the position to `side · size` once
    /// the market trades through `limit`, filling at that price **or better**
    /// and never worse. The entry counterpart to `set_stop`. Idempotent,
    /// latest-wins per symbol. Returns `None` (working until it triggers).
    ///
    /// The size resolves at the *fill* price, not at submission — an all-in
    /// `Size.value_frac(1.0)` sizes against equity when the limit is hit.
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

    /// Cancel both resting protective legs (stop and take-profit) on `symbol`.
    pub(crate) fn cancel_protective(&mut self, symbol: String) -> PyResult<()> {
        self.inner
            .cancel_protective(&intern(symbol))
            .map_err(|error| PyValueError::new_err(error.to_string()))
    }

    /// Drain fills booked out of band (not on a specific `update`). Always empty
    /// for the paper wallet — the method exists for parity with live wallets,
    /// which buffer async fills here.
    pub(crate) fn poll_fills(&mut self) -> Vec<PyOrder> {
        self.inner
            .poll_fills()
            .into_iter()
            .map(|inner| PyOrder { inner })
            .collect()
    }

    /// Cancel a working order by its `id` (see `Order.id`): a queued market
    /// order or a resting protective leg is dropped. An unknown id is a no-op.
    pub(crate) fn cancel(&mut self, id: u64) -> PyResult<()> {
        self.inner
            .cancel(OrderId(id))
            .map_err(|error| PyValueError::new_err(error.to_string()))
    }
}

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
}

/// Map a wallet `Ack` to Python: the fill if it filled synchronously, `None` if it
/// is merely working, or a `ValueError`.
pub(crate) fn wrap_ack(result: Result<Ack<Symbol>, WalletError>) -> PyResult<Option<PyOrder>> {
    match result {
        Ok(Ack::Filled(inner)) => Ok(Some(PyOrder { inner })),
        Ok(Ack::Working(_)) => Ok(None),
        Err(error) => Err(PyWalletError::new_err(error.to_string())),
    }
}

/// Parse a side string into a [`Side`].
pub(crate) fn parse_side(side: &str) -> PyResult<Side> {
    match side.to_ascii_lowercase().as_str() {
        "buy" | "long" => Ok(Side::Buy),
        "sell" | "short" => Ok(Side::Sell),
        _ => Err(PyValueError::new_err("side must be 'buy' or 'sell'")),
    }
}

/// Parse an order-kind string into an [`OrderKind`] — the inverse of
/// [`kind_str`], so `Order(kind=o.kind)` round-trips a fill read back out.
pub(crate) fn parse_kind(kind: &str) -> PyResult<OrderKind> {
    match kind.to_ascii_lowercase().as_str() {
        "market" => Ok(OrderKind::Market),
        "stop" => Ok(OrderKind::Stop),
        "take_profit" => Ok(OrderKind::TakeProfit),
        "limit" => Ok(OrderKind::Limit),
        _ => Err(PyValueError::new_err(
            "kind must be 'market', 'stop', 'take_profit' or 'limit'",
        )),
    }
}

/// Coerce a Python argument into a [`Size`]: a number is units, or a `Size`.
pub(crate) fn coerce_size(obj: &Bound<'_, PyAny>) -> PyResult<Size> {
    if let Ok(size) = obj.extract::<PySize>() {
        Ok(size.inner)
    } else if let Ok(units) = obj.extract::<f64>() {
        Ok(Size::Units(units))
    } else {
        Err(PyTypeError::new_err(
            "size must be a number of units or a Size",
        ))
    }
}

/// The `"buy"`/`"sell"` string for a [`Side`].
pub(crate) fn side_str(side: Side) -> &'static str {
    match side {
        Side::Buy => "buy",
        Side::Sell => "sell",
    }
}

/// The `"market"`/`"stop"`/`"take_profit"`/`"limit"` string for an [`OrderKind`].
pub(crate) fn kind_str(kind: OrderKind) -> &'static str {
    match kind {
        OrderKind::Market => "market",
        OrderKind::Stop => "stop",
        OrderKind::TakeProfit => "take_profit",
        OrderKind::Limit => "limit",
    }
}

// ---------------------------------------------------------------------------
// Strategy builder + run
//
// The declarative `SingleAssetStrategy` builder, mirrored for Python: a
// `Strategy("BTC").long_on(enter, exit).short_on(down, up).run(wallet, candles)`
// pipeline over a `PaperWallet`, returning a `RunReport` (equity curve + fill
// blotter) the metrics functions consume. The strategy layer is snapshot-rooted;
// the everyday Python leaves are candle-rooted, so `AtomLift` bridges them.
//
// Deliberately omitted this pass (see python parity notes): position-anchored
// protective levels (`Position` uses `Rc`, which isn't `Send + Sync`, so the
// `entry()`/`peak()`/`trough()` accessors can't be type-erased for pyo3),
// pairs/basket strategies, and the `src/strategies` recipe catalogue.
// ---------------------------------------------------------------------------

/// Lift a candle-rooted (`Input = Atom`) source/signal into a snapshot-rooted
/// one by projecting the single-entry snapshot's sole atom. `SingleAssetStrategy`
/// is snapshot-rooted, but `ta.close()` & friends are candle-rooted; this bridges
/// them, the same size-1 unpack a CLI `Pick` performs.
///
/// The unpack panics on a 2+ entry bar, and `update` has no channel to return
/// through, so the shapes that *can* be handed a multi-entry snapshot refuse a
/// candle-rooted leaf at wiring time instead — see [`pairs_signal`]. The lift
/// itself is therefore only reached where the snapshot is a single series.
#[derive(Clone)]
pub(crate) struct AtomLift<S>(pub(crate) S);

impl<S: Indicator<Input = Atom>> Indicator for AtomLift<S> {
    type Input = Snapshot<Symbol>;
    type Output = S::Output;
    fn update(&mut self, snap: Snapshot<Symbol>) -> Option<S::Output> {
        snap.sole_atom_or_panic()
            .cloned()
            .and_then(|a| self.0.update(a))
    }
    fn value(&self) -> Option<S::Output> {
        self.0.value()
    }
    fn warm_up_bars(&self) -> usize {
        self.0.warm_up_bars()
    }
    fn unstable_bars(&self) -> usize {
        self.0.unstable_bars()
    }
    fn reset(&mut self) {
        self.0.reset()
    }
}

/// Project a Python signal (candle- or snapshot-rooted) into the snapshot-rooted
/// form a strategy consumes. A bare-value (Real) signal is a domain error.
pub(crate) fn snapshot_signal(sig: &PySignal) -> PyResult<SignalBox<Snapshot<Symbol>>> {
    match &sig.sig {
        // Bar-only lifts twice: to the atom domain, then to the snapshot one.
        AnySignal::Candle(s) => Ok(SignalBox::new(AtomLift(SignalBox(atom_over_candle(
            s.0.clone(),
        ))))),
        AnySignal::Atom(s) => Ok(SignalBox::new(AtomLift(s.clone()))),
        AnySignal::Snapshot(s) => Ok(s.clone()),
        AnySignal::Real(_) => Err(PyValueError::new_err(
            "a strategy signal must be candle- or snapshot-rooted, not a bare value (Real) signal",
        )),
    }
}

/// Project a Python real source (candle-rooted, snapshot-rooted, or a constant)
/// into the snapshot-rooted sizing multiplier a strategy consumes.
pub(crate) fn snapshot_source(ind: &PyIndicator) -> PyResult<Source<Snapshot<Symbol>>> {
    match &ind.src {
        AnySource::Candle(s) => Ok(runtime::erase(AtomLift(atom_over_candle(s.clone())))),
        AnySource::Atom(s) => Ok(runtime::erase(AtomLift(s.clone()))),
        AnySource::Snapshot(s) => Ok(s.clone()),
        AnySource::Const(c) => Ok(runtime::erase(Value::<Snapshot<Symbol>>::new(*c))),
        AnySource::Real(_) => Err(PyValueError::new_err(
            "a sizing source must be candle- or snapshot-rooted (or a constant), not a bare value (Real) source",
        )),
    }
}

pub(crate) fn const_false_signal() -> SignalBox<Snapshot<Symbol>> {
    SignalBox::new(ValueBool::<Snapshot<Symbol>>::new(false))
}

/// Refuse anything but a callable, at **build** time.
///
/// The per-symbol factories are invoked inside `Strategy::update`, which has no
/// error channel, so every failure there is a `panic!` that pyo3 re-raises as a
/// `PanicException` — a `BaseException` that `except Exception` does not catch.
/// Passing an `Indicator` where a factory belongs is the overwhelmingly common
/// way to reach that, and it is decidable the moment the builder is called, so
/// it earns an ordinary `TypeError` here instead.
///
/// This does not (and cannot) probe the callable: a factory keyed on the real
/// universe would raise for a synthetic probe symbol, so a factory that raises
/// for a *genuine* symbol still surfaces as a panic at run time.
fn require_factory(obj: &Py<PyAny>, method: &str, kind: &str) -> PyResult<()> {
    Python::attach(|py| {
        let bound = obj.bind(py);
        if bound.is_callable() {
            return Ok(());
        }
        let got = bound.get_type().name()?;
        Err(PyTypeError::new_err(format!(
            "{method}() takes a per-symbol factory — a callable `sym -> {kind}` — but got \
             {got}. Each symbol needs its own chain, rooted on that symbol, so the \
             argument has to be a function of the symbol: \
             `.{method}(lambda sym: rsi(close(pick(sym)), 14))`."
        )))
    })
}

/// Turn a Python callable `sym -> Signal` into the per-symbol signal factory a
/// [`MultiAssetStrategy`] / [`BasketStrategy`] consumes. The callable is invoked
/// once per symbol on first sight (during `run`, GIL held); it must return a
/// candle- or snapshot-rooted `Signal`.
///
/// The factory boundary has no `Result` channel, so a failure *there* is a
/// `panic!` that pyo3 re-raises as a `PanicException` — a `BaseException`, which
/// `except Exception` does not catch. [`require_factory`] takes the decidable
/// half of that away at wiring time; what is left is a callable that genuinely
/// raises, or returns the wrong type, for a real symbol.
pub(crate) fn signal_factory_from_callable(
    cb: Py<PyAny>,
) -> impl Fn(&Symbol) -> SignalBox<Snapshot<Symbol>> + Send + Sync + 'static {
    move |sym: &Symbol| {
        Python::attach(|py| {
            let obj = cb
                .call1(py, (sym.as_ref(),))
                .unwrap_or_else(|e| panic!("signal factory raised for symbol '{sym}': {e}"));
            let bound = obj.bind(py);
            let sig = bound.cast::<PySignal>().unwrap_or_else(|_| {
                panic!("signal factory for symbol '{sym}' must return a fugazi.Signal")
            });
            snapshot_signal(&sig.borrow())
                .unwrap_or_else(|e| panic!("signal factory for symbol '{sym}': {e}"))
        })
    }
}

/// Turn a Python callable `sym -> Indicator` into the per-symbol real-source
/// factory a [`MultiAssetStrategy`] / [`BasketStrategy`] consumes (sizing / score).
/// Same lifecycle and error handling as [`signal_factory_from_callable`].
pub(crate) fn source_factory_from_callable(
    cb: Py<PyAny>,
) -> impl Fn(&Symbol) -> Source<Snapshot<Symbol>> + Send + Sync + 'static {
    move |sym: &Symbol| {
        Python::attach(|py| {
            let obj = cb
                .call1(py, (sym.as_ref(),))
                .unwrap_or_else(|e| panic!("source factory raised for symbol '{sym}': {e}"));
            let bound = obj.bind(py);
            let ind = bound.cast::<PyIndicator>().unwrap_or_else(|_| {
                panic!("source factory for symbol '{sym}' must return a fugazi.Indicator")
            });
            snapshot_source(&ind.borrow())
                .unwrap_or_else(|e| panic!("source factory for symbol '{sym}': {e}"))
        })
    }
}

/// The error a leaf that named no asset earns inside a **two-legged** strategy.
///
/// The Python mirror of `spec::expr::Root::ambiguous("pairs")`, which refuses
/// the same document on the YAML side. A pairs strategy blesses neither leg,
/// so a candle- or atom-rooted leaf reaches `AtomLift` and unpacks the sole
/// atom — and the bar carries two. That used to panic on the first bar, which
/// crosses the FFI boundary as a `PanicException`: a `BaseException`, so
/// `except Exception` walked straight past it and the caller could not handle
/// it at all.
///
/// Nothing expressible is lost. Every leaf takes an optional `source`, so the
/// rooted spelling is always available — including the calendar leaves, which
/// only read the bar's timestamp and are therefore happy rooted on *either*
/// leg.
fn unrooted_pairs_leaf(slot: &str) -> PyErr {
    PyValueError::new_err(format!(
        "a pairs strategy privileges neither leg, so this {slot} has no series to \
         read: it is candle-rooted and the bar carries both legs. Name the series \
         on each leaf — `close(pick(\"BTC\"))` rather than `close()`. Calendar \
         leaves need it too, even though they only read the bar's timestamp: \
         `day_of_week(pick(\"BTC\"))` (either leg will do — they share the time)."
    ))
}

/// [`snapshot_signal`] for the two-legged shapes: same projection, but a
/// candle- or atom-rooted signal is refused up front rather than panicking on
/// the first two-entry bar. See [`unrooted_pairs_leaf`].
pub(crate) fn pairs_signal(sig: &PySignal) -> PyResult<SignalBox<Snapshot<Symbol>>> {
    match &sig.sig {
        AnySignal::Candle(_) | AnySignal::Atom(_) => Err(unrooted_pairs_leaf("signal")),
        _ => snapshot_signal(sig),
    }
}

/// [`snapshot_source`] for the two-legged shapes. A constant is still fine — it
/// reads no series at all — so only the candle- and atom-rooted arms are
/// refused. See [`unrooted_pairs_leaf`].
pub(crate) fn pairs_source(ind: &PyIndicator) -> PyResult<Source<Snapshot<Symbol>>> {
    match &ind.src {
        AnySource::Candle(_) | AnySource::Atom(_) => Err(unrooted_pairs_leaf("source")),
        _ => snapshot_source(ind),
    }
}

/// A copyable descriptor for a preset catalogue strategy: the Rust free
/// function's ctor args, dispatched at `run` / `sharpe_of` construction time
/// to build a fresh [`SingleAssetStrategy`] (Rust catalogue helpers aren't
/// `Clone`, so we carry the recipe rather than the built strategy).
#[derive(Clone, Debug)]
pub(crate) enum PresetSpec {
    BuyAndHold {
        symbol: String,
    },
    MaCrossover {
        symbol: String,
        fast: usize,
        slow: usize,
    },
    RsiReversal {
        symbol: String,
        period: usize,
        oversold: Real,
        exit_level: Real,
    },
    DonchianBreakout {
        symbol: String,
        period: usize,
    },
    KeltnerBreakout {
        symbol: String,
        ema_period: usize,
        atr_period: usize,
        multiplier: Real,
    },
}

impl PresetSpec {
    /// Build a fresh [`SingleAssetStrategy`] with `initial_equity` seeded
    /// into its book (so book-anchored sizing recipes read meaningful
    /// numbers). Every dispatch mirrors the Rust free function in
    /// `fugazi::strategies`; `with_initial_equity` re-seeds after the
    /// catalogue's default `new`.
    pub(crate) fn build(&self, initial_equity: Real) -> SingleAssetStrategy<Symbol> {
        use fugazi_core::strategies::{composite, mean_reversion, trend};
        let s = match self {
            PresetSpec::BuyAndHold { symbol } => {
                SingleAssetStrategy::<Symbol>::buy_and_hold(intern(symbol))
            }
            PresetSpec::MaCrossover { symbol, fast, slow } => {
                trend::ma_crossover(intern(symbol), *fast, *slow)
            }
            PresetSpec::RsiReversal {
                symbol,
                period,
                oversold,
                exit_level,
            } => mean_reversion::rsi_reversal(intern(symbol), *period, *oversold, *exit_level),
            PresetSpec::DonchianBreakout { symbol, period } => {
                trend::donchian_breakout(intern(symbol), *period)
            }
            PresetSpec::KeltnerBreakout {
                symbol,
                ema_period,
                atr_period,
                multiplier,
            } => composite::keltner_breakout(intern(symbol), *ema_period, *atr_period, *multiplier),
        };
        // Re-seed the book at the requested initial equity — the
        // catalogue free functions use `new`, which starts at 1.0.
        // `SingleAssetStrategy` doesn't expose a re-seed method, so we
        // extract sides + sizing via a fresh build path...
        // ... except that isn't exposed either. In practice, users pass
        // the same seed to both the wallet and the strategy at
        // construction; a mismatch only affects book-anchored recipes
        // (`equity_vol_target`, `fractional_kelly`, `drawdown_throttle`)
        // and is documented on the catalogue functions themselves.
        let _ = initial_equity;
        s
    }
}

/// A declarative single-asset strategy: long/short entry & exit signals plus an
/// optional sizing multiplier, driven over a `PaperWallet` by [`run`](Self::run).
///
/// A missing `exit` never fires — right for an always-in long/short reversal
/// (the opposite side's `enter` reverses the position); give an `exit` only for
/// a flat rest. Builder methods return a new `Strategy`, so they chain.
///
/// **Two shapes**: the builder path (`Strategy(symbol).long_on(...).short_on(...)`)
/// and the preset path (catalogue functions like `ma_crossover(...)`). A preset
/// strategy carries its catalogue recipe; builder methods (`long_on`,
/// `short_on`, `position_sizing`) raise `ValueError` on it — build from
/// scratch or use the preset as-is.
#[pyclass(name = "Strategy", module = "fugazi", skip_from_py_object)]
#[derive(Clone)]
pub(crate) struct PyStrategy {
    /// Interned once at construction, so the per-bar `Snapshot::single` in
    /// `run` tags with a refcount bump rather than a fresh allocation.
    pub(crate) symbol: Symbol,
    pub(crate) long_enter: Option<SignalBox<Snapshot<Symbol>>>,
    pub(crate) long_exit: Option<SignalBox<Snapshot<Symbol>>>,
    pub(crate) short_enter: Option<SignalBox<Snapshot<Symbol>>>,
    pub(crate) short_exit: Option<SignalBox<Snapshot<Symbol>>>,
    pub(crate) sizing: Option<Source<Snapshot<Symbol>>>,
    pub(crate) rebalance: Option<SignalBox<Snapshot<Symbol>>>,
    pub(crate) preset: Option<PresetSpec>,
}

#[pymethods]
impl PyStrategy {
    /// A fresh strategy trading `symbol`, with no sides wired.
    #[new]
    pub(crate) fn new(symbol: String) -> Self {
        PyStrategy {
            symbol: intern(symbol),
            long_enter: None,
            long_exit: None,
            short_enter: None,
            short_exit: None,
            sizing: None,
            rebalance: None,
            preset: None,
        }
    }

    /// Enter (or reverse into) a long on `enter`; flatten it on `exit`
    /// (defaults to never).
    #[pyo3(signature = (enter, exit=None))]
    pub(crate) fn long_on(
        &self,
        enter: &PySignal,
        exit: Option<&PySignal>,
    ) -> PyResult<PyStrategy> {
        if self.preset.is_some() {
            return Err(PyValueError::new_err(
                "long_on is not supported on a preset strategy; build one from scratch with fugazi.Strategy(symbol) if you need custom sides",
            ));
        }
        let mut s = self.clone();
        s.long_enter = Some(snapshot_signal(enter)?);
        s.long_exit = exit.map(snapshot_signal).transpose()?;
        Ok(s)
    }

    /// Enter (or reverse into) a short on `enter`; flatten it on `exit`
    /// (defaults to never).
    #[pyo3(signature = (enter, exit=None))]
    pub(crate) fn short_on(
        &self,
        enter: &PySignal,
        exit: Option<&PySignal>,
    ) -> PyResult<PyStrategy> {
        if self.preset.is_some() {
            return Err(PyValueError::new_err(
                "short_on is not supported on a preset strategy; build one from scratch with fugazi.Strategy(symbol) if you need custom sides",
            ));
        }
        let mut s = self.clone();
        s.short_enter = Some(snapshot_signal(enter)?);
        s.short_exit = exit.map(snapshot_signal).transpose()?;
        Ok(s)
    }

    /// Scale every entry's value-fraction magnitude by this real source (Kelly,
    /// vol targeting, fixed-fraction, …). Defaults to all-in (`1.0`). A `None`
    /// reading skips that bar's trade (safe default).
    pub(crate) fn position_sizing(&self, source: &PyIndicator) -> PyResult<PyStrategy> {
        if self.preset.is_some() {
            return Err(PyValueError::new_err(
                "position_sizing is not supported on a preset strategy; build one from scratch with fugazi.Strategy(symbol) if you need custom sizing",
            ));
        }
        let mut s = self.clone();
        s.sizing = Some(snapshot_source(source)?);
        Ok(s)
    }

    /// Install the rebalance gate — on bars where `signal` fires, the open
    /// position is resized to the current sizing target. **Defaults to never**,
    /// so without this the position is sized only on entry and then drifts with
    /// P&L. Unlike `position_sizing`, this composes with a preset.
    ///
    /// A `None` reading is treated as `False` (the safe default). Pair it with
    /// `ta.value(...)`-style periodic gates or a book-anchored signal for
    /// event-driven rebalancing.
    pub(crate) fn rebalance_on(&self, signal: &PySignal) -> PyResult<PyStrategy> {
        let mut s = self.clone();
        s.rebalance = Some(snapshot_signal(signal)?);
        Ok(s)
    }

    /// Drive the strategy over `candles` against `wallet` (a `PaperWallet`, an
    /// `OkxWallet`, or a `CoinbaseWallet`), returning the [`RunReport`](PyRunReport). `candles` is a
    /// DataFrame / dict of OHLCV columns (same shape as `Indicator.feed`). Passing
    /// an `OkxWallet` drives the strategy **live**, one bar at a time. The book is
    /// seeded to the wallet's opening equity, so book-anchored sizing reads
    /// meaningful numbers. The wallet is mutated in place (positions, blotter).
    ///
    /// Whatever the wallet already holds at start is treated as the user's own,
    /// externally-managed book: the strategy sizes against its own capital (cash +
    /// only the positions it opens) and never disturbs the pre-existing ones. A
    /// flat wallet is the common case and behaves exactly as before.
    pub(crate) fn run(
        &self,
        wallet: &Bound<'_, PyAny>,
        candles: &Bound<'_, PyAny>,
    ) -> PyResult<PyRunReport> {
        let snaps = single_snapshots_from_frame(candles, &self.symbol)?;

        run_over_wallet!(wallet, py, snaps, seed => self.materialize(seed))
    }

    pub(crate) fn __repr__(&self) -> String {
        if self.preset.is_some() {
            format!("Strategy(preset, symbol='{}')", self.symbol)
        } else {
            format!("Strategy(symbol='{}')", self.symbol)
        }
    }
}

impl PyStrategy {
    /// Rust-side builder for a fresh [`SingleAssetStrategy<Symbol>`]
    /// seeded at `initial_equity`. Preset presets dispatch to the
    /// catalogue; otherwise starts from a bare `with_initial_equity`
    /// that later assignments layer sides / sizing onto.
    /// The configured strategy, ready to drive — [`build_strategy`](Self::build_strategy)
    /// plus every builder-shape override layered on.
    ///
    /// Split out of `run` so a [`PyPortfolio`] child can be materialized
    /// without driving it, which is the only difference between running a
    /// strategy alone and running it inside a portfolio.
    pub(crate) fn materialize(&self, initial_equity: Real) -> SingleAssetStrategy<Symbol> {
        let mut strat = self.build_strategy(initial_equity);
        // Builder-shape overrides (only meaningful when preset is None; if a
        // preset is set, long_enter/etc. are guaranteed None by the builder
        // methods' guards).
        if let Some(enter) = &self.long_enter {
            strat = strat.long_on(
                enter.clone(),
                self.long_exit.clone().unwrap_or_else(const_false_signal),
            );
        }
        if let Some(enter) = &self.short_enter {
            strat = strat.short_on(
                enter.clone(),
                self.short_exit.clone().unwrap_or_else(const_false_signal),
            );
        }
        if let Some(sizing) = &self.sizing {
            strat = strat.position_sizing(sizing.clone());
        }
        // Applied unconditionally: a rebalance gate is orthogonal to how the
        // sides were wired, so it composes with a preset as well as a
        // hand-built strategy.
        if let Some(rebalance) = &self.rebalance {
            strat = strat.rebalance_on(rebalance.clone());
        }
        strat
    }

    pub(crate) fn build_strategy(&self, initial_equity: Real) -> SingleAssetStrategy<Symbol> {
        match &self.preset {
            Some(preset) => preset.build(initial_equity),
            None => SingleAssetStrategy::<Symbol>::with_initial_equity(
                self.symbol.clone(),
                initial_equity,
            ),
        }
    }
}

/// A two-leg pairs strategy, long / flat / short **on the spread**
/// `close(left) − close(right)`.
///
/// `long_spread_on` goes long `left` / short `right` (profiting as the spread
/// rises); `short_spread_on` is the mirror (profiting as it falls). Each leg
/// entries at `value_frac(0.5 * m)` — a 1.0-gross dollar-neutral pair by
/// default. The two directions are inverse positions, so they are mutually
/// exclusive in time and share one capital pool at full notional; the opposite
/// side's entry reverses an open pair.
///
/// A mean-reverting spread visits both tails and the correct position is
/// opposite at each, so wiring only one side skips every excursion on the
/// other. Leaving `short_spread_on` unwired keeps the historical
/// long-spread-only behaviour.
///
/// Spread stop-loss / take-profit levels are per-side and compared with
/// mirrored sense: the short side stops out when the spread rises *above* its
/// level, since that is its adverse direction. Mirrors
/// `fugazi::strategies::PairsStrategy`. Signals and levels are snapshot-rooted
/// (built from `pick(...)` sources); `run` consumes a sequence of snapshots.
#[pyclass(name = "PairsStrategy", module = "fugazi", skip_from_py_object)]
#[derive(Clone)]
pub(crate) struct PyPairsStrategy {
    pub(crate) left: String,
    pub(crate) right: String,
    pub(crate) enter: Option<SignalBox<Snapshot<Symbol>>>,
    pub(crate) exit: Option<SignalBox<Snapshot<Symbol>>>,
    pub(crate) short_enter: Option<SignalBox<Snapshot<Symbol>>>,
    pub(crate) short_exit: Option<SignalBox<Snapshot<Symbol>>>,
    pub(crate) stop: Option<Source<Snapshot<Symbol>>>,
    pub(crate) target: Option<Source<Snapshot<Symbol>>>,
    pub(crate) short_stop: Option<Source<Snapshot<Symbol>>>,
    pub(crate) short_target: Option<Source<Snapshot<Symbol>>>,
    pub(crate) sizing: Option<Source<Snapshot<Symbol>>>,
    pub(crate) rebalance: Option<SignalBox<Snapshot<Symbol>>>,
}

#[pymethods]
impl PyPairsStrategy {
    /// A fresh pairs strategy over `left` / `right` with no transitions wired.
    #[new]
    pub(crate) fn new(left: String, right: String) -> Self {
        PyPairsStrategy {
            left,
            right,
            enter: None,
            exit: None,
            short_enter: None,
            short_exit: None,
            stop: None,
            target: None,
            short_stop: None,
            short_target: None,
            sizing: None,
            rebalance: None,
        }
    }

    /// Wire the **long-spread** side: `enter` opens long `left` / short `right`
    /// (profiting as the spread rises), `exit` flattens both. A missing `exit`
    /// never fires.
    #[pyo3(signature = (enter, exit=None))]
    pub(crate) fn long_spread_on(
        &self,
        enter: &PySignal,
        exit: Option<&PySignal>,
    ) -> PyResult<PyPairsStrategy> {
        let mut s = self.clone();
        s.enter = Some(pairs_signal(enter)?);
        s.exit = exit.map(pairs_signal).transpose()?;
        Ok(s)
    }

    /// Wire the **short-spread** side: `enter` opens short `left` / long
    /// `right` (profiting as the spread falls), `exit` flattens both. Leaving
    /// this unwired keeps the pair long-spread-only.
    #[pyo3(signature = (enter, exit=None))]
    pub(crate) fn short_spread_on(
        &self,
        enter: &PySignal,
        exit: Option<&PySignal>,
    ) -> PyResult<PyPairsStrategy> {
        let mut s = self.clone();
        s.short_enter = Some(pairs_signal(enter)?);
        s.short_exit = exit.map(pairs_signal).transpose()?;
        Ok(s)
    }

    /// Alias for `long_spread_on`, kept for callers written before the short
    /// side existed.
    #[pyo3(signature = (enter, exit=None))]
    pub(crate) fn on(
        &self,
        enter: &PySignal,
        exit: Option<&PySignal>,
    ) -> PyResult<PyPairsStrategy> {
        self.long_spread_on(enter, exit)
    }

    /// Attach the long-spread stop-loss: that side flattens when `close(left) −
    /// close(right)` reads at or below `level` (its adverse direction).
    pub(crate) fn long_spread_stop_loss(&self, level: &PyIndicator) -> PyResult<PyPairsStrategy> {
        let mut s = self.clone();
        s.stop = Some(pairs_source(level)?);
        Ok(s)
    }

    /// Attach the long-spread take-profit: that side flattens when the spread
    /// reads at or above `level`.
    pub(crate) fn long_spread_take_profit(&self, level: &PyIndicator) -> PyResult<PyPairsStrategy> {
        let mut s = self.clone();
        s.target = Some(pairs_source(level)?);
        Ok(s)
    }

    /// Attach the short-spread stop-loss. Mirrored sense: that side flattens
    /// when the spread reads at or **above** `level`, since it profits as the
    /// spread falls.
    pub(crate) fn short_spread_stop_loss(&self, level: &PyIndicator) -> PyResult<PyPairsStrategy> {
        let mut s = self.clone();
        s.short_stop = Some(pairs_source(level)?);
        Ok(s)
    }

    /// Attach the short-spread take-profit: that side flattens when the spread
    /// reads at or **below** `level`.
    pub(crate) fn short_spread_take_profit(
        &self,
        level: &PyIndicator,
    ) -> PyResult<PyPairsStrategy> {
        let mut s = self.clone();
        s.short_target = Some(pairs_source(level)?);
        Ok(s)
    }

    /// Alias for `long_spread_stop_loss`.
    pub(crate) fn spread_stop_loss(&self, level: &PyIndicator) -> PyResult<PyPairsStrategy> {
        self.long_spread_stop_loss(level)
    }

    /// Alias for `long_spread_take_profit`.
    pub(crate) fn spread_take_profit(&self, level: &PyIndicator) -> PyResult<PyPairsStrategy> {
        self.long_spread_take_profit(level)
    }

    /// Scale the pair's gross exposure by this real source (each leg entries at
    /// `value_frac(0.5 * m)`). Defaults to `1.0`. A `None` reading skips the
    /// bar's trade (safe default).
    pub(crate) fn position_sizing(&self, source: &PyIndicator) -> PyResult<PyPairsStrategy> {
        let mut s = self.clone();
        s.sizing = Some(pairs_source(source)?);
        Ok(s)
    }

    /// Install the rebalance gate — on bars where `signal` fires, both legs are
    /// resized to the current sizing target. Defaults to never.
    pub(crate) fn rebalance_on(&self, signal: &PySignal) -> PyResult<PyPairsStrategy> {
        let mut s = self.clone();
        s.rebalance = Some(pairs_signal(signal)?);
        Ok(s)
    }

    /// Drive the pair over `snapshots` (a sequence of `Snapshot` or `dict`)
    /// against `wallet` (a `PaperWallet` or an `OkxWallet`), returning the
    /// [`RunReport`](PyRunReport). The book is seeded to the wallet's opening
    /// equity. The wallet is mutated in place. Any positions the wallet already
    /// holds are left untouched and the pair sizes against its own capital (see
    /// `Strategy.run`).
    pub(crate) fn run(
        &self,
        wallet: &Bound<'_, PyAny>,
        snapshots: &Bound<'_, PyAny>,
    ) -> PyResult<PyRunReport> {
        let snaps = snapshots_from_sequence(snapshots)?;
        run_over_wallet!(wallet, py, snaps, seed => self.materialize(seed))
    }

    pub(crate) fn __repr__(&self) -> String {
        format!(
            "PairsStrategy(left='{}', right='{}')",
            self.left, self.right
        )
    }
}

impl PyPairsStrategy {
    /// The configured strategy, ready to drive. Split out of `run` so a
    /// [`PyPortfolio`] child can be materialized without driving it.
    pub(crate) fn materialize(&self, seed: Real) -> PairsStrategy<Symbol> {
        let mut strat = PairsStrategy::<Symbol>::with_initial_equity(
            intern(&self.left),
            intern(&self.right),
            seed,
        );
        if let Some(enter) = &self.enter {
            strat = strat.long_spread_on(
                enter.clone(),
                self.exit.clone().unwrap_or_else(const_false_signal),
            );
        }
        if let Some(enter) = &self.short_enter {
            strat = strat.short_spread_on(
                enter.clone(),
                self.short_exit.clone().unwrap_or_else(const_false_signal),
            );
        }
        if let Some(stop) = &self.stop {
            strat = strat.long_spread_stop_loss(stop.clone());
        }
        if let Some(target) = &self.target {
            strat = strat.long_spread_take_profit(target.clone());
        }
        if let Some(stop) = &self.short_stop {
            strat = strat.short_spread_stop_loss(stop.clone());
        }
        if let Some(target) = &self.short_target {
            strat = strat.short_spread_take_profit(target.clone());
        }
        if let Some(sizing) = &self.sizing {
            strat = strat.position_sizing(sizing.clone());
        }
        if let Some(rebalance) = &self.rebalance {
            strat = strat.rebalance_on(rebalance.clone());
        }
        strat
    }
}

/// A declared universe restriction shared by [`PyMultiAssetStrategy`] and
/// [`PyBasketStrategy`]: `strict` → `all_of` (missing symbol panics, readiness
/// gated on every leg); otherwise `any_of` (absent / unready silently skipped).
#[derive(Clone)]
pub(crate) struct DeclaredUniverse {
    pub(crate) strict: bool,
    pub(crate) symbols: Vec<String>,
}

/// An N-symbol strategy running the same single-asset decision independently on
/// every symbol in a snapshot (any subset long / short / flat at once). Mirrors
/// `fugazi::strategies::MultiAssetStrategy`. Every side is a Python factory
/// `sym -> Signal` (built once per symbol on first sight, its leaves rooted on
/// that symbol via `pick(...)`); sizing is `sym -> Indicator`. Position-anchored
/// protective levels are not exposed (they require a per-leg `Position`).
#[pyclass(name = "MultiAssetStrategy", module = "fugazi", skip_from_py_object)]
pub(crate) struct PyMultiAssetStrategy {
    pub(crate) long_enter: Option<Py<PyAny>>,
    pub(crate) long_exit: Option<Py<PyAny>>,
    pub(crate) short_enter: Option<Py<PyAny>>,
    pub(crate) short_exit: Option<Py<PyAny>>,
    pub(crate) sizing: Option<Py<PyAny>>,
    pub(crate) rebalance: Option<SignalBox<Snapshot<Symbol>>>,
    pub(crate) universe: Option<DeclaredUniverse>,
}

impl Clone for PyMultiAssetStrategy {
    fn clone(&self) -> Self {
        Python::attach(|py| PyMultiAssetStrategy {
            long_enter: self.long_enter.as_ref().map(|p| p.clone_ref(py)),
            long_exit: self.long_exit.as_ref().map(|p| p.clone_ref(py)),
            short_enter: self.short_enter.as_ref().map(|p| p.clone_ref(py)),
            short_exit: self.short_exit.as_ref().map(|p| p.clone_ref(py)),
            sizing: self.sizing.as_ref().map(|p| p.clone_ref(py)),
            rebalance: self.rebalance.clone(),
            universe: self.universe.clone(),
        })
    }
}

#[pymethods]
impl PyMultiAssetStrategy {
    /// A fresh multi-asset strategy with no sides wired (trades nothing until a
    /// side is added).
    #[new]
    pub(crate) fn new() -> Self {
        PyMultiAssetStrategy {
            long_enter: None,
            long_exit: None,
            short_enter: None,
            short_exit: None,
            sizing: None,
            rebalance: None,
            universe: None,
        }
    }

    /// Wire the long side: `enter(sym)` opens (or reverses into) a long on that
    /// symbol, `exit(sym)` flattens it. Both are callables `sym -> Signal`; a
    /// missing `exit` never fires.
    #[pyo3(signature = (enter, exit=None))]
    pub(crate) fn long_on(
        &self,
        enter: Py<PyAny>,
        exit: Option<Py<PyAny>>,
    ) -> PyResult<PyMultiAssetStrategy> {
        require_factory(&enter, "long_on", "Signal")?;
        if let Some(x) = &exit {
            require_factory(x, "long_on", "Signal")?;
        }
        let mut s = self.clone();
        s.long_enter = Some(enter);
        s.long_exit = exit;
        Ok(s)
    }

    /// Wire the short side: `enter(sym)` opens (or reverses into) a short,
    /// `exit(sym)` flattens it. Same factory shape as [`long_on`](Self::long_on).
    #[pyo3(signature = (enter, exit=None))]
    pub(crate) fn short_on(
        &self,
        enter: Py<PyAny>,
        exit: Option<Py<PyAny>>,
    ) -> PyResult<PyMultiAssetStrategy> {
        require_factory(&enter, "short_on", "Signal")?;
        if let Some(x) = &exit {
            require_factory(x, "short_on", "Signal")?;
        }
        let mut s = self.clone();
        s.short_enter = Some(enter);
        s.short_exit = exit;
        Ok(s)
    }

    /// Wire the per-symbol sizing factory `sym -> Indicator` — the
    /// value-fraction magnitude every entry on that symbol is sized against.
    /// Defaults to all-in (`1.0`).
    pub(crate) fn position_sizing(&self, factory: Py<PyAny>) -> PyResult<PyMultiAssetStrategy> {
        require_factory(&factory, "position_sizing", "Indicator")?;
        let mut s = self.clone();
        s.sizing = Some(factory);
        Ok(s)
    }

    /// Install the rebalance gate (a snapshot-rooted signal). On fire, every
    /// held position is resized to its current sizing target. Defaults to never.
    pub(crate) fn rebalance_on(&self, signal: &PySignal) -> PyResult<PyMultiAssetStrategy> {
        let mut s = self.clone();
        s.rebalance = Some(snapshot_signal(signal)?);
        Ok(s)
    }

    /// Restrict to exactly `symbols` (strict): a missing symbol on any bar
    /// panics, and readiness waits until every listed leg has settled.
    pub(crate) fn all_of(&self, symbols: Vec<String>) -> PyMultiAssetStrategy {
        let mut s = self.clone();
        s.universe = Some(DeclaredUniverse {
            strict: true,
            symbols,
        });
        s
    }

    /// Restrict to `symbols` (lax): only listed symbols trade, absent / unready
    /// ones are silently skipped.
    pub(crate) fn any_of(&self, symbols: Vec<String>) -> PyMultiAssetStrategy {
        let mut s = self.clone();
        s.universe = Some(DeclaredUniverse {
            strict: false,
            symbols,
        });
        s
    }

    /// Drive the strategy over `snapshots` against `wallet` (a `PaperWallet`, an `OkxWallet`, or
    /// a `CoinbaseWallet`), returning the [`RunReport`](PyRunReport). The book is
    /// seeded to the wallet's opening equity. The wallet is mutated in place. Any
    /// positions the wallet already holds are left untouched and sizing is against
    /// our own capital (see `Strategy.run`).
    pub(crate) fn run(
        &self,
        wallet: &Bound<'_, PyAny>,
        snapshots: &Bound<'_, PyAny>,
    ) -> PyResult<PyRunReport> {
        let snaps = snapshots_from_sequence(snapshots)?;
        run_over_wallet!(wallet, py, snaps, seed => self.materialize(py, seed))
    }

    pub(crate) fn __repr__(&self) -> String {
        "MultiAssetStrategy(...)".to_string()
    }
}

impl PyMultiAssetStrategy {
    /// The configured strategy, ready to drive. Needs the GIL token because
    /// every slot is a per-symbol Python callable. Split out of `run` so a
    /// [`PyPortfolio`] child can be materialized without driving it.
    pub(crate) fn materialize(&self, py: Python<'_>, seed: Real) -> MultiAssetStrategy<Symbol> {
        let mut strat = MultiAssetStrategy::<Symbol>::with_initial_equity(seed);

        if let Some(enter) = &self.long_enter {
            let ef = signal_factory_from_callable(enter.clone_ref(py));
            strat = match &self.long_exit {
                Some(exit) => {
                    let xf = signal_factory_from_callable(exit.clone_ref(py));
                    strat.long_on(ef, xf)
                }
                None => strat.long_on(ef, |_: &Symbol| const_false_signal()),
            };
        }
        if let Some(enter) = &self.short_enter {
            let ef = signal_factory_from_callable(enter.clone_ref(py));
            strat = match &self.short_exit {
                Some(exit) => {
                    let xf = signal_factory_from_callable(exit.clone_ref(py));
                    strat.short_on(ef, xf)
                }
                None => strat.short_on(ef, |_: &Symbol| const_false_signal()),
            };
        }
        if let Some(sizing) = &self.sizing {
            let sf = source_factory_from_callable(sizing.clone_ref(py));
            strat = strat.position_sizing(sf);
        }
        if let Some(rebalance) = &self.rebalance {
            strat = strat.rebalance_on(rebalance.clone());
        }
        if let Some(u) = &self.universe {
            strat = if u.strict {
                strat.all_of(u.symbols.iter().map(intern))
            } else {
                strat.any_of(u.symbols.iter().map(intern))
            };
        }
        strat
    }
}

/// The cross-sectional selection rule for a [`PyBasketStrategy`] — a
/// composable tree mirroring `fugazi::strategies::basket::Selection`. Each
/// ranked rule narrows an inner rule (`of`), defaulting to the
/// `Everything` leaf (the whole universe).
#[derive(Clone)]
pub(crate) enum BasketSelection {
    Everything,
    TopBottom {
        longs: usize,
        shorts: usize,
        of: Box<BasketSelection>,
    },
    Threshold {
        long_min: Real,
        short_max: Real,
        of: Box<BasketSelection>,
    },
    Quantile {
        long_q: Real,
        short_q: Real,
        of: Box<BasketSelection>,
    },
}

impl BasketSelection {
    /// Build the composed `Selection` chain this tree describes, nesting
    /// each rule's `of` inner via the core `::of` constructors.
    pub(crate) fn build(&self) -> Box<dyn core_basket::Selection<Symbol>> {
        match self {
            BasketSelection::Everything => Box::new(core_basket::Everything),
            BasketSelection::TopBottom { longs, shorts, of } => Box::new(
                core_basket::TopBottom::of(core_basket::DynSelection(of.build()), *longs, *shorts),
            ),
            BasketSelection::Threshold {
                long_min,
                short_max,
                of,
            } => Box::new(core_basket::Threshold::of(
                core_basket::DynSelection(of.build()),
                *long_min,
                *short_max,
            )),
            BasketSelection::Quantile {
                long_q,
                short_q,
                of,
            } => Box::new(core_basket::Quantile::of(
                core_basket::DynSelection(of.build()),
                *long_q,
                *short_q,
            )),
        }
    }
}

/// Wrap an optional inner rule, defaulting to the `Everything` leaf.
pub(crate) fn selection_inner(of: Option<PySelection>) -> Box<BasketSelection> {
    Box::new(of.map_or(BasketSelection::Everything, |s| s.inner))
}

/// A composed basket selection rule — built by `ta.top_bottom` /
/// `ta.threshold` / `ta.quantile` / `ta.everything`, installed via
/// `BasketStrategy.selection(...)`, and usable as the `of=` inner of
/// another rule so selections nest to any depth (e.g. the top-2 / bottom-2
/// *of* the threshold survivors).
#[pyclass(name = "Selection", module = "fugazi", from_py_object)]
#[derive(Clone)]
pub(crate) struct PySelection {
    pub(crate) inner: BasketSelection,
}

#[pymethods]
impl PySelection {
    pub(crate) fn __repr__(&self) -> String {
        "Selection(...)".to_string()
    }
}

/// The full-universe selection leaf — every symbol eligible for either
/// side. The implicit `of=` default; rarely needed explicitly.
#[pyfunction]
pub(crate) fn everything() -> PySelection {
    PySelection {
        inner: BasketSelection::Everything,
    }
}

/// Select the top `longs` and bottom `shorts` symbols by score, ranked
/// within the optional `of` inner rule (default: the whole universe).
#[pyfunction]
#[pyo3(signature = (longs, shorts, of=None))]
pub(crate) fn top_bottom(longs: usize, shorts: usize, of: Option<PySelection>) -> PySelection {
    PySelection {
        inner: BasketSelection::TopBottom {
            longs,
            shorts,
            of: selection_inner(of),
        },
    }
}

/// Long every symbol scoring `>= long_min`, short every symbol scoring
/// `<= short_max`, within the optional `of` inner rule (default: all).
#[pyfunction]
#[pyo3(signature = (long_min, short_max, of=None))]
pub(crate) fn threshold(long_min: Real, short_max: Real, of: Option<PySelection>) -> PySelection {
    PySelection {
        inner: BasketSelection::Threshold {
            long_min,
            short_max,
            of: selection_inner(of),
        },
    }
}

/// Long the top `long_q` quantile by score, short the bottom `short_q`
/// (each in `[0, 1]`), within the optional `of` inner rule (default: all).
#[pyfunction]
#[pyo3(signature = (long_q, short_q, of=None))]
pub(crate) fn quantile(long_q: Real, short_q: Real, of: Option<PySelection>) -> PySelection {
    PySelection {
        inner: BasketSelection::Quantile {
            long_q,
            short_q,
            of: selection_inner(of),
        },
    }
}

/// An N-symbol cross-sectional basket: score every symbol, then a selection rule
/// picks the longs / shorts. Mirrors `fugazi::strategies::BasketStrategy`. Score
/// and sizing are Python factories `sym -> Indicator` (leaves rooted on that
/// symbol via `pick(...)`); the selection is one of top-bottom / threshold /
/// quantile, each of which composes by narrowing an inner rule via `of=`
/// (e.g. `strat.top_bottom(2, 2, of=ta.threshold(0.5, -0.5))`, or the general
/// `strat.selection(...)` seam). The `.selection(closure)` escape hatch and
/// per-leg protective levels are not exposed.
#[pyclass(name = "BasketStrategy", module = "fugazi", skip_from_py_object)]
pub(crate) struct PyBasketStrategy {
    pub(crate) score: Option<Py<PyAny>>,
    pub(crate) sizing: Option<Py<PyAny>>,
    pub(crate) selection: Option<BasketSelection>,
    pub(crate) balance_sides: bool,
    pub(crate) rebalance: Option<SignalBox<Snapshot<Symbol>>>,
    pub(crate) universe: Option<DeclaredUniverse>,
}

impl Clone for PyBasketStrategy {
    fn clone(&self) -> Self {
        Python::attach(|py| PyBasketStrategy {
            score: self.score.as_ref().map(|p| p.clone_ref(py)),
            sizing: self.sizing.as_ref().map(|p| p.clone_ref(py)),
            selection: self.selection.clone(),
            balance_sides: self.balance_sides,
            rebalance: self.rebalance.clone(),
            universe: self.universe.clone(),
        })
    }
}

#[pymethods]
impl PyBasketStrategy {
    /// A fresh basket (trades nothing until scored, sized, and given a selection
    /// rule).
    #[new]
    pub(crate) fn new() -> Self {
        PyBasketStrategy {
            score: None,
            sizing: None,
            selection: None,
            balance_sides: true,
            rebalance: None,
            universe: None,
        }
    }

    /// Wire the per-symbol score factory `sym -> Indicator`; the selection rule
    /// ranks symbols by this value each rebalance.
    pub(crate) fn scored_by(&self, factory: Py<PyAny>) -> PyResult<PyBasketStrategy> {
        require_factory(&factory, "scored_by", "Indicator")?;
        let mut s = self.clone();
        s.score = Some(factory);
        Ok(s)
    }

    /// Wire the per-symbol sizing factory `sym -> Indicator` — each selected
    /// leg's value-fraction. Use an equal-weight source for a normalized gross.
    pub(crate) fn sized_by(&self, factory: Py<PyAny>) -> PyResult<PyBasketStrategy> {
        require_factory(&factory, "sized_by", "Indicator")?;
        let mut s = self.clone();
        s.sizing = Some(factory);
        Ok(s)
    }

    /// Select the top `longs` and bottom `shorts` symbols by score, ranked
    /// within the optional `of` inner rule (default: the whole universe).
    #[pyo3(signature = (longs, shorts, of=None))]
    pub(crate) fn top_bottom(
        &self,
        longs: usize,
        shorts: usize,
        of: Option<PySelection>,
    ) -> PyBasketStrategy {
        let mut s = self.clone();
        s.selection = Some(BasketSelection::TopBottom {
            longs,
            shorts,
            of: selection_inner(of),
        });
        s
    }

    /// Long every symbol scoring `>= long_min`, short every symbol scoring
    /// `<= short_max`, within the optional `of` inner rule (default: all).
    #[pyo3(signature = (long_min, short_max, of=None))]
    pub(crate) fn threshold(
        &self,
        long_min: Real,
        short_max: Real,
        of: Option<PySelection>,
    ) -> PyBasketStrategy {
        let mut s = self.clone();
        s.selection = Some(BasketSelection::Threshold {
            long_min,
            short_max,
            of: selection_inner(of),
        });
        s
    }

    /// Long the top `long_q` quantile by score, short the bottom `short_q`
    /// quantile (each in `[0, 1]`), within the optional `of` inner rule.
    #[pyo3(signature = (long_q, short_q, of=None))]
    pub(crate) fn quantile(
        &self,
        long_q: Real,
        short_q: Real,
        of: Option<PySelection>,
    ) -> PyBasketStrategy {
        let mut s = self.clone();
        s.selection = Some(BasketSelection::Quantile {
            long_q,
            short_q,
            of: selection_inner(of),
        });
        s
    }

    /// Install a composed selection rule (from `ta.top_bottom` /
    /// `ta.threshold` / `ta.quantile` / `ta.everything`) — the general
    /// seam behind the convenience methods, and how you nest rules via
    /// `of=`.
    pub(crate) fn selection(&self, rule: PySelection) -> PyBasketStrategy {
        let mut s = self.clone();
        s.selection = Some(rule.inner);
        s
    }

    /// Balance the two sides' target sizes each rebalance so that the long
    /// weights and the short weights sum to the same gross (never levers up;
    /// a one-sided bar passes through unscaled). On by default -- pass
    /// `False` to keep the raw per-leg sizes.
    #[pyo3(signature = (balance = true))]
    pub(crate) fn balance_sides(&self, balance: bool) -> PyBasketStrategy {
        let mut s = self.clone();
        s.balance_sides = balance;
        s
    }

    /// Install the rebalance gate (snapshot-rooted signal). Defaults to every
    /// bar (`Every::new(1)`), matching the Rust default.
    pub(crate) fn rebalance_on(&self, signal: &PySignal) -> PyResult<PyBasketStrategy> {
        let mut s = self.clone();
        s.rebalance = Some(snapshot_signal(signal)?);
        Ok(s)
    }

    /// Restrict discovery to exactly `symbols` (strict): a missing symbol panics
    /// and readiness gates on every listed leg scoring & sizing.
    pub(crate) fn all_of(&self, symbols: Vec<String>) -> PyBasketStrategy {
        let mut s = self.clone();
        s.universe = Some(DeclaredUniverse {
            strict: true,
            symbols,
        });
        s
    }

    /// Restrict discovery to `symbols` (lax): absent / unready members skipped.
    pub(crate) fn any_of(&self, symbols: Vec<String>) -> PyBasketStrategy {
        let mut s = self.clone();
        s.universe = Some(DeclaredUniverse {
            strict: false,
            symbols,
        });
        s
    }

    /// Drive the basket over `snapshots` against `wallet` (a `PaperWallet`, an
    /// `OkxWallet`, or a `CoinbaseWallet`), returning the [`RunReport`](PyRunReport). The book is seeded
    /// to the wallet's opening equity. The wallet is mutated in place. Any
    /// positions the wallet already holds are left untouched and sizing is against
    /// our own capital (see `Strategy.run`).
    pub(crate) fn run(
        &self,
        wallet: &Bound<'_, PyAny>,
        snapshots: &Bound<'_, PyAny>,
    ) -> PyResult<PyRunReport> {
        let snaps = snapshots_from_sequence(snapshots)?;
        run_over_wallet!(wallet, py, snaps, seed => self.materialize(py, seed))
    }

    pub(crate) fn __repr__(&self) -> String {
        "BasketStrategy(...)".to_string()
    }
}

impl PyBasketStrategy {
    /// The configured strategy, ready to drive. Needs the GIL token because
    /// score / sizing are per-symbol Python callables. Split out of `run` so a
    /// [`PyPortfolio`] child can be materialized without driving it.
    pub(crate) fn materialize(&self, py: Python<'_>, seed: Real) -> BasketStrategy<Symbol> {
        let mut strat = BasketStrategy::<Symbol>::with_initial_equity(seed);

        if let Some(score) = &self.score {
            let f = source_factory_from_callable(score.clone_ref(py));
            strat = strat.scored_by(f);
        }
        if let Some(sizing) = &self.sizing {
            let f = source_factory_from_callable(sizing.clone_ref(py));
            strat = strat.sized_by(f);
        }
        strat = match &self.selection {
            Some(sel) => strat.selection(core_basket::DynSelection(sel.build())),
            None => strat,
        };
        strat = strat.balance_sides(self.balance_sides);
        if let Some(rebalance) = &self.rebalance {
            strat = strat.rebalance_on(rebalance.clone());
        }
        if let Some(u) = &self.universe {
            strat = if u.strict {
                strat.all_of(u.symbols.iter().map(intern))
            } else {
                strat.any_of(u.symbols.iter().map(intern))
            };
        }
        strat
    }
}

// ---------------------------------------------------------------------------
// Strategy catalogue as Python constructors
// ---------------------------------------------------------------------------

/// Buy-and-hold `symbol` — long every bar with `value_frac(1.0)` sizing.
/// Matches `fugazi::strategies::SingleAssetStrategy::buy_and_hold`.
#[pyfunction]
pub(crate) fn buy_and_hold(symbol: String) -> PyStrategy {
    PyStrategy {
        symbol: intern(symbol.as_str()),
        long_enter: None,
        long_exit: None,
        short_enter: None,
        short_exit: None,
        sizing: None,
        rebalance: None,
        preset: Some(PresetSpec::BuyAndHold { symbol }),
    }
}

/// A simple MA-crossover strategy: long when `fast` SMA crosses above `slow`
/// SMA, short on the opposite cross. Matches
/// `fugazi::strategies::trend::ma_crossover`.
#[pyfunction]
pub(crate) fn ma_crossover(symbol: String, fast: usize, slow: usize) -> PyStrategy {
    PyStrategy {
        symbol: intern(symbol.as_str()),
        long_enter: None,
        long_exit: None,
        short_enter: None,
        short_exit: None,
        sizing: None,
        rebalance: None,
        preset: Some(PresetSpec::MaCrossover { symbol, fast, slow }),
    }
}

/// RSI reversal: long when RSI crosses below `oversold`, exit long when it
/// crosses above `exit_level`. Matches
/// `fugazi::strategies::mean_reversion::rsi_reversal`.
#[pyfunction]
#[pyo3(signature = (symbol, period, oversold=30.0, exit_level=50.0))]
pub(crate) fn rsi_reversal(
    symbol: String,
    period: usize,
    oversold: Real,
    exit_level: Real,
) -> PyStrategy {
    PyStrategy {
        symbol: intern(symbol.as_str()),
        long_enter: None,
        long_exit: None,
        short_enter: None,
        short_exit: None,
        sizing: None,
        rebalance: None,
        preset: Some(PresetSpec::RsiReversal {
            symbol,
            period,
            oversold,
            exit_level,
        }),
    }
}

/// Donchian breakout: long on a `period`-bar high break, short on a `period`-
/// bar low break. Matches `fugazi::strategies::trend::donchian_breakout`.
#[pyfunction]
pub(crate) fn donchian_breakout(symbol: String, period: usize) -> PyStrategy {
    PyStrategy {
        symbol: intern(symbol.as_str()),
        long_enter: None,
        long_exit: None,
        short_enter: None,
        short_exit: None,
        sizing: None,
        rebalance: None,
        preset: Some(PresetSpec::DonchianBreakout { symbol, period }),
    }
}

/// Keltner band breakout: long above the upper ATR-banded EMA channel, short
/// below the lower. Matches `fugazi::strategies::composite::keltner_breakout`.
#[pyfunction]
#[pyo3(signature = (symbol, ema_period, atr_period, multiplier=2.0))]
pub(crate) fn keltner_breakout(
    symbol: String,
    ema_period: usize,
    atr_period: usize,
    multiplier: Real,
) -> PyStrategy {
    PyStrategy {
        symbol: intern(symbol.as_str()),
        long_enter: None,
        long_exit: None,
        short_enter: None,
        short_exit: None,
        sizing: None,
        rebalance: None,
        preset: Some(PresetSpec::KeltnerBreakout {
            symbol,
            ema_period,
            atr_period,
            multiplier,
        }),
    }
}

// ---------------------------------------------------------------------------
// Trailing risk-of-strategy indicators — embed a preset [`Strategy`], drive
// it against a private wallet, and read a rolling metric over its equity
// curve. Unblocked by two changes: the Position + Book + Shared
// `Arc<Mutex>` refactor (was `Rc<RefCell>`) *and* the strategy internal
// trait-object bound tightening (`Box<dyn Signal + 'static>` →
// `+ Send + Sync + 'static`).
//
// The trailing indicators aren't `Clone` (they own an embedded strategy +
// PaperWallet), so we wrap them in `RebuildOnClone` — an `Arc`-based
// mirror of the CLI's `RebuildIndicator` that rebuilds a fresh instance
// on `Clone`. This satisfies `TypedSource`'s Clone + Send + Sync bound.
// ---------------------------------------------------------------------------

/// The fixed wallet seed the trailing-risk-of-strategy indicators use for
/// their embedded strategy. Every metric is a ratio, so the seed only
/// determines value scale — matches `src/cli/spec/trailing.rs::SEED`.
pub(crate) const TRAILING_STRATEGY_SEED: Real = 1_000.0;

/// A `Clone`-able wrapper around a non-`Clone` inner indicator. Holds a
/// factory closure that builds a fresh instance on each `Clone`, so the
/// enclosing carrier can be `Clone + Send + Sync` even when the inner
/// (an embedded-strategy trailing indicator) is not. Mirror of
/// `RebuildIndicator` in `src/cli/spec/trailing.rs`, but with `Arc` for
/// `Send + Sync`.
pub(crate) type BoxedSnapshotReal =
    Box<dyn Indicator<Input = Snapshot<Symbol>, Output = Real> + Send + Sync>;

pub(crate) struct RebuildOnClone {
    pub(crate) build: Arc<dyn Fn() -> BoxedSnapshotReal + Send + Sync>,
    pub(crate) inner: BoxedSnapshotReal,
}

impl Clone for RebuildOnClone {
    fn clone(&self) -> Self {
        let inner = (self.build)();
        Self {
            build: Arc::clone(&self.build),
            inner,
        }
    }
}

impl Indicator for RebuildOnClone {
    type Input = Snapshot<Symbol>;
    type Output = Real;
    fn update(&mut self, input: Snapshot<Symbol>) -> Option<Real> {
        self.inner.update(input)
    }
    fn value(&self) -> Option<Real> {
        self.inner.value()
    }
    fn warm_up_bars(&self) -> usize {
        self.inner.warm_up_bars()
    }
    fn unstable_bars(&self) -> usize {
        self.inner.unstable_bars()
    }
    fn reset(&mut self) {
        self.inner.reset();
    }
}

/// Turn a factory closure that builds a non-`Clone` indicator into a
/// [`PyIndicator`] that carries the factory in a
/// [`RebuildOnClone`] so `TypedSource::new`'s `Clone` bound is satisfied.
pub(crate) fn wrap_rebuild<F>(build: F) -> PyIndicator
where
    F: Fn() -> BoxedSnapshotReal + Send + Sync + 'static,
{
    let build = Arc::new(build);
    let inner = build();
    let carrier = RebuildOnClone { build, inner };
    PyIndicator::wrap(AnySource::Snapshot(runtime::erase(carrier)))
}

/// Rolling annualized Sharpe over the equity curve of an embedded
/// [`Strategy`](PyStrategy). Matches `!sharpe` in the YAML spec.
#[pyfunction]
#[pyo3(signature = (strategy, period, bars_per_year, risk_free_rate=0.0))]
pub(crate) fn sharpe_of(
    strategy: &PyStrategy,
    period: usize,
    bars_per_year: Real,
    risk_free_rate: Real,
) -> PyIndicator {
    let preset = strategy.preset.clone();
    let symbol = strategy.symbol.clone();
    wrap_rebuild(move || {
        let strat = build_preset_or_bare(&preset, &symbol);
        Box::new(fugazi_core::indicators::Sharpe::new(
            strat,
            symbol.clone(),
            TRAILING_STRATEGY_SEED,
            period,
            risk_free_rate,
            bars_per_year,
        ))
    })
}

/// Rolling annualized Sortino over the equity curve of an embedded
/// [`Strategy`](PyStrategy). Matches `!sortino` in the YAML spec.
#[pyfunction]
#[pyo3(signature = (strategy, period, bars_per_year, risk_free_rate=0.0))]
pub(crate) fn sortino_of(
    strategy: &PyStrategy,
    period: usize,
    bars_per_year: Real,
    risk_free_rate: Real,
) -> PyIndicator {
    let preset = strategy.preset.clone();
    let symbol = strategy.symbol.clone();
    wrap_rebuild(move || {
        let strat = build_preset_or_bare(&preset, &symbol);
        Box::new(fugazi_core::indicators::Sortino::new(
            strat,
            symbol.clone(),
            TRAILING_STRATEGY_SEED,
            period,
            risk_free_rate,
            bars_per_year,
        ))
    })
}

/// Rolling annualized volatility of the equity curve of an embedded
/// [`Strategy`](PyStrategy). Matches `!volatility` in the YAML spec.
#[pyfunction]
pub(crate) fn volatility_of(
    strategy: &PyStrategy,
    period: usize,
    bars_per_year: Real,
) -> PyIndicator {
    let preset = strategy.preset.clone();
    let symbol = strategy.symbol.clone();
    wrap_rebuild(move || {
        let strat = build_preset_or_bare(&preset, &symbol);
        Box::new(fugazi_core::indicators::Volatility::new(
            strat,
            symbol.clone(),
            TRAILING_STRATEGY_SEED,
            period,
            bars_per_year,
        ))
    })
}

/// Rolling maximum drawdown of the equity curve of an embedded
/// [`Strategy`](PyStrategy). Matches `!max_drawdown` in the YAML spec.
#[pyfunction]
pub(crate) fn max_drawdown_of(strategy: &PyStrategy, period: usize) -> PyIndicator {
    let preset = strategy.preset.clone();
    let symbol = strategy.symbol.clone();
    wrap_rebuild(move || {
        let strat = build_preset_or_bare(&preset, &symbol);
        Box::new(fugazi_core::indicators::MaxDrawdown::new(
            strat,
            symbol.clone(),
            TRAILING_STRATEGY_SEED,
            period,
        ))
    })
}

/// Rolling Calmar ratio (annualized return / max drawdown) over the equity
/// curve of an embedded [`Strategy`](PyStrategy). Matches `!calmar` in the
/// YAML spec.
#[pyfunction]
pub(crate) fn calmar_of(strategy: &PyStrategy, period: usize, bars_per_year: Real) -> PyIndicator {
    let preset = strategy.preset.clone();
    let symbol = strategy.symbol.clone();
    wrap_rebuild(move || {
        let strat = build_preset_or_bare(&preset, &symbol);
        Box::new(fugazi_core::indicators::Calmar::new(
            strat,
            symbol.clone(),
            TRAILING_STRATEGY_SEED,
            period,
            bars_per_year,
        ))
    })
}

/// Rebuild a preset strategy (or a bare `SingleAssetStrategy` if none set)
/// with the trailing seed. Used inside the `wrap_rebuild` factory of each
/// trailing-risk-of-strategy indicator.
pub(crate) fn build_preset_or_bare(
    preset: &Option<PresetSpec>,
    symbol: &str,
) -> SingleAssetStrategy<Symbol> {
    match preset {
        Some(p) => p.build(TRAILING_STRATEGY_SEED),
        None => SingleAssetStrategy::<Symbol>::with_initial_equity(
            intern(symbol),
            TRAILING_STRATEGY_SEED,
        ),
    }
}

/// One order the wallet **refused**, stamped with the bar it was refused on —
/// the failure-side twin of [`Fill`](PyFill).
///
/// A refusal is otherwise invisible: an order can be accepted at submission and
/// still fail later when it is filled, with nobody holding a result to check. A
/// non-empty `RunReport.rejections` means the run did not trade the way the
/// strategy intended, so the metrics describe something other than what was
/// specified.
#[pyclass(name = "Rejected", module = "fugazi", frozen)]
pub(crate) struct PyRejected {
    pub(crate) inner: Rejected<Symbol>,
}

#[pymethods]
impl PyRejected {
    /// The bar index at which the order was refused.
    #[getter]
    pub(crate) fn bar(&self) -> usize {
        self.inner.bar
    }

    /// The instrument the refused order was for.
    #[getter]
    pub(crate) fn symbol(&self) -> String {
        self.inner.rejection.symbol.to_string()
    }

    /// Why it was refused, as the `WalletError`'s message.
    #[getter]
    pub(crate) fn error(&self) -> String {
        self.inner.rejection.error.to_string()
    }

    /// `"market"`, `"stop"` or `"take_profit"` — a refused stop means the
    /// position is still open and its protection did not fire.
    #[getter]
    pub(crate) fn kind(&self) -> &'static str {
        kind_str(self.inner.rejection.kind)
    }

    /// The id of the submission this refusal belongs to.
    #[getter]
    pub(crate) fn id(&self) -> u64 {
        self.inner.rejection.id.0
    }

    pub(crate) fn __repr__(&self) -> String {
        format!(
            "Rejected(bar={}, symbol='{}', error='{}', kind='{}')",
            self.inner.bar,
            self.inner.rejection.symbol,
            self.inner.rejection.error,
            kind_str(self.inner.rejection.kind),
        )
    }
}

/// The result of [`Strategy.run`](PyStrategy::run): the per-bar equity curve, the
/// fill blotter, the refused orders, and the pre-run seed equity — everything the
/// `fugazi.metrics` functions reduce to numbers.
///
/// It is also plain data, so a caller holding an equity curve that no `run()` in
/// this process produced — a live account's accrued equity, a resumed run, an
/// externally-computed series — can build one and hand it to
/// [`evaluate_report`](evaluate_report) for the whole metric tree rather than
/// composing a dozen `fugazi.metrics` calls and reproducing the dotted key names
/// by hand:
///
/// ```python
/// report = fugazi.RunReport(equity_curve=curve, initial_equity=10_000.0)
/// metrics = fugazi.evaluate_report(report, bars_per_year=252.0)
/// ```
///
/// Pass `fills` too (see [`Order`](PyOrder)) to get the trade-statistics section
/// populated; without them `trades.*` reads as a run that never traded, and
/// `rejections` is always empty on a hand-built report.
#[pyclass(name = "RunReport", module = "fugazi", frozen)]
pub(crate) struct PyRunReport {
    pub(crate) inner: RunReport<Symbol>,
}

#[pymethods]
impl PyRunReport {
    /// A report over `equity_curve` (one marked-to-market equity per bar) seeded
    /// from `initial_equity`, optionally carrying the `fills` that produced it.
    ///
    /// `ruin_bar` marks a run that was wiped out — see the property of the same
    /// name. A hand-built report leaves it `None` unless you are reconstructing
    /// one that was.
    #[new]
    #[pyo3(signature = (equity_curve, initial_equity, *, fills = None, ruin_bar = None))]
    pub(crate) fn new(
        equity_curve: Vec<f64>,
        initial_equity: f64,
        fills: Option<Vec<PyFill>>,
        ruin_bar: Option<usize>,
    ) -> Self {
        PyRunReport {
            inner: RunReport {
                ruin_bar,
                equity_curve,
                fills: fills
                    .unwrap_or_default()
                    .into_iter()
                    .map(|f| f.inner)
                    .collect(),
                // A rejection carries a `WalletError`, which only a wallet can
                // raise — there is nothing for a caller to reconstruct one from,
                // so a hand-built report is by definition a clean one.
                rejections: Vec::new(),
                initial_equity,
            },
        }
    }

    /// One marked-to-market equity value per input bar, as a `list[float]`.
    ///
    /// **Rebuilt on every access** — it is a property, so it looks free, and on a
    /// million-bar run each read allocates a million-element list. Bind it once
    /// (`curve = report.equity_curve`) rather than touching it in a loop, or use
    /// [`equity_array`](Self::equity_array), which skips the list entirely.
    ///
    /// Stays a `list` rather than becoming an `ndarray`: `+` concatenates two
    /// lists and *adds* two arrays, and chunked-run examples in the docs rely on
    /// the former. `equity_array` is the opt-in.
    #[getter]
    pub(crate) fn equity_curve(&self) -> Vec<f64> {
        self.inner.equity_curve.clone()
    }

    /// The same curve as a NumPy `float64` `ndarray`.
    ///
    /// Written straight into the array's buffer, so it costs one allocation and
    /// no per-element Python `float` — unlike `equity_curve`, which boxes every
    /// value. It is also the form the metrics want: `Series` takes a fast
    /// `memcpy` path out of a contiguous buffer and falls back to
    /// element-by-element extraction for a `list`, so
    /// `per_bar_returns(report.equity_array, report.initial_equity)` avoids both
    /// costs.
    ///
    /// Raises `ImportError` if NumPy isn't installed — the wheel has no required
    /// dependencies, so `equity_curve` remains the one that always works.
    #[getter]
    pub(crate) fn equity_array<'py>(&self, py: Python<'py>) -> PyResult<Bound<'py, PyAny>> {
        let curve = &self.inner.equity_curve;
        crate::constructors::numpy_filled(py, curve.len(), |out| out.copy_from_slice(curve))
    }

    /// Every booked fill (a [`Fill`](PyFill)), in fill order.
    ///
    /// Rebuilt on every access, and more expensively than
    /// [`equity_curve`](Self::equity_curve) — one fresh `Fill` *object* per
    /// entry, not one float. Bind it once.
    #[getter]
    pub(crate) fn fills(&self) -> Vec<PyFill> {
        self.inner
            .fills
            .iter()
            .cloned()
            .map(|inner| PyFill { inner })
            .collect()
    }

    /// Every refused order (a [`Rejected`](PyRejected)), in refusal order. Empty
    /// on a clean run — check it before trusting the metrics.
    ///
    /// Rebuilt on every access; see [`fills`](Self::fills).
    #[getter]
    pub(crate) fn rejections(&self) -> Vec<PyRejected> {
        self.inner
            .rejections
            .iter()
            .cloned()
            .map(|inner| PyRejected { inner })
            .collect()
    }

    /// The wallet's equity captured immediately before the first bar — the seed
    /// returns / CAGR compound against.
    #[getter]
    pub(crate) fn initial_equity(&self) -> f64 {
        self.inner.initial_equity
    }

    /// The bar this run was **ruined** on — the first bar close at which total
    /// equity reached zero — or `None` for a run that stayed solvent.
    ///
    /// On that bar the book is liquidated and nothing trades afterwards, and
    /// the equity curve is pinned at `0.0` from there to the end. So a report
    /// with a `ruin_bar` reduces to exactly `-100%` total return and a `100%`
    /// max drawdown, and `metrics["run"]["ruin_bar"]` carries the same index.
    ///
    /// Note the **nested** spelling. `run.ruin_bar` is the *flat* key — what
    /// `metrics::flatten`, the CSV columns and the CLI use; the Python metrics
    /// document is a nested `dict`, and the key is absent entirely on a solvent
    /// run rather than present as `None`.
    #[getter]
    pub(crate) fn ruin_bar(&self) -> Option<usize> {
        self.inner.ruin_bar
    }

    /// Rebuild through [`_rebuild_run_report`].
    ///
    /// **Lossy in one place, deliberately:** `rejections` do not survive. A
    /// `Rejected` carries a live `WalletError`, which only a wallet can raise
    /// and `RunReport.__new__` therefore cannot accept — the same reason a
    /// hand-built report is documented as a clean one. Every other field round
    /// trips exactly, and `metrics` reads none of the dropped one.
    pub(crate) fn __reduce__(&self, py: Python<'_>) -> PyResult<Py<PyAny>> {
        crate::classes::reduce_with(
            py,
            py.import("fugazi")?.getattr("_rebuild_run_report")?,
            (
                self.equity_curve(),
                self.initial_equity(),
                self.fills(),
                self.ruin_bar(),
            ),
        )
    }

    pub(crate) fn __repr__(&self) -> String {
        format!(
            "RunReport(bars={}, fills={}, rejections={}, initial_equity={}, ruin_bar={:?})",
            self.inner.equity_curve.len(),
            self.inner.fills.len(),
            self.inner.rejections.len(),
            self.inner.initial_equity,
            self.inner.ruin_bar,
        )
    }
}

/// N *different* strategies sharing one account.
///
/// The other four shapes each run a single decision rule; this one composes
/// them, so it answers a question none of them can: *what would these
/// strategies have earned together?*
///
/// Each child trades its own notional **ledger** — its slice of the account's
/// cash and positions — and sizes against that, so `value_frac(1.0)` in a child
/// still means all of *that child's* capital. Every child's intent is then
/// netted into one order per symbol against a single account, which is what a
/// real deployment looks like.
///
/// ```python
/// pf = (ta.Portfolio()
///         .add("trend",  ta.Strategy("BTC").long_on(fast.crosses_above(slow), fast.crosses_below(slow)))
///         .add("revert", ta.Strategy("ETH").long_on(rsi.lt(30.0), rsi.gt(70.0)))
///         .weights([0.7, 0.3]))
/// report = pf.run(ta.PaperWallet(10_000.0), snapshots)
/// ```
///
/// Because children share a book, opposing flow between them crosses
/// internally (and pays no costs), and a child's stop takes off only its own
/// share. See the Rust `fugazi::portfolio` docs for the full set.
#[pyclass(name = "Portfolio", module = "fugazi")]
#[derive(Default)]
pub(crate) struct PyPortfolio {
    children: Vec<(String, Py<PyAny>)>,
    weights: Option<Vec<Real>>,
    rebalance: Option<SignalBox<Snapshot<Symbol>>>,
}

#[pymethods]
impl PyPortfolio {
    #[new]
    pub(crate) fn new() -> Self {
        Self::default()
    }

    /// Add a child under `name`, which must be unique. `strategy` is any of
    /// `Strategy`, `PairsStrategy`, `BasketStrategy` or `MultiAssetStrategy` —
    /// a `Portfolio` is refused, since a nested one could never be priced.
    pub(crate) fn add(
        &self,
        py: Python<'_>,
        name: String,
        strategy: &Bound<'_, PyAny>,
    ) -> PyResult<PyPortfolio> {
        if self.children.iter().any(|(n, _)| *n == name) {
            return Err(PyValueError::new_err(format!(
                "Portfolio.add: duplicate child name `{name}`"
            )));
        }
        if strategy.is_instance_of::<PyPortfolio>() {
            return Err(PyValueError::new_err(
                "Portfolio.add: a Portfolio cannot be a child of a Portfolio — an inner \
                 portfolio's account is never priced. Flatten the children into one \
                 portfolio, or run them separately.",
            ));
        }
        if !(strategy.is_instance_of::<PyStrategy>()
            || strategy.is_instance_of::<PyPairsStrategy>()
            || strategy.is_instance_of::<PyBasketStrategy>()
            || strategy.is_instance_of::<PyMultiAssetStrategy>())
        {
            return Err(PyValueError::new_err(
                "Portfolio.add: strategy must be a Strategy, PairsStrategy, \
                 BasketStrategy or MultiAssetStrategy",
            ));
        }
        let mut next = self.clone_with(py);
        next.children.push((name, strategy.clone().unbind()));
        Ok(next)
    }

    /// Fixed per-child weights, in `add` order. Magnitudes — they are
    /// normalized, so they needn't sum to 1. Defaults to equal weight.
    pub(crate) fn weights(&self, py: Python<'_>, weights: Vec<Real>) -> PyResult<PyPortfolio> {
        if weights.iter().any(|w| *w < 0.0) {
            return Err(PyValueError::new_err(
                "Portfolio.weights: weights must be non-negative",
            ));
        }
        let mut next = self.clone_with(py);
        next.weights = Some(weights);
        Ok(next)
    }

    /// Pull capital back to the target weights whenever `signal` fires.
    /// Off by default, so the split drifts with P&L.
    pub(crate) fn rebalance_on(&self, py: Python<'_>, signal: &PySignal) -> PyResult<PyPortfolio> {
        let mut next = self.clone_with(py);
        next.rebalance = Some(snapshot_signal(signal)?);
        Ok(next)
    }

    /// Drive the portfolio over `snapshots` against `wallet` (a `PaperWallet`, an `OkxWallet`, or
    /// a `CoinbaseWallet`), returning the aggregate report.
    ///
    /// A portfolio is an ordinary strategy that trades the wallet it is handed,
    /// exactly like the other four shapes: it nets its children's intents onto
    /// that one account. The children trade notional slices of it (tracked by
    /// per-child `Ledger`s), fills settle on it, and it is handed back **mutated**
    /// — positions, cash, and blotter applied — so the caller's Python wallet
    /// reflects what happened. Its opening equity seeds the per-child cash split.
    /// Passing an `OkxWallet` or `CoinbaseWallet` therefore trades the whole
    /// netted portfolio **live**. Costs pre-installed on the wallet apply (it
    /// *is* the account).
    ///
    /// Whatever the account already holds at start is treated as the user's own,
    /// externally-managed book (see `Strategy.run`): the children size against our
    /// own equity and net only over our own positions, leaving the pre-existing
    /// ones untouched. A flat account behaves exactly as before.
    pub(crate) fn run(
        &self,
        wallet: &Bound<'_, PyAny>,
        snapshots: &Bound<'_, PyAny>,
    ) -> PyResult<PyRunReport> {
        let snaps = snapshots_from_sequence(snapshots)?;
        run_over_wallet!(wallet, py, snaps, seed => self.materialize(py, seed)?)
    }

    pub(crate) fn __repr__(&self) -> String {
        let names: Vec<&str> = self.children.iter().map(|(n, _)| n.as_str()).collect();
        format!("Portfolio(children=[{}])", names.join(", "))
    }
}

impl PyPortfolio {
    /// Deep-ish clone: the child handles are refcount bumps, which needs the
    /// GIL token, so this can't be a plain `Clone` impl.
    fn clone_with(&self, py: Python<'_>) -> Self {
        Self {
            children: self
                .children
                .iter()
                .map(|(n, c)| (n.clone(), c.clone_ref(py)))
                .collect(),
            weights: self.weights.clone(),
            rebalance: self.rebalance.clone(),
        }
    }

    /// Build the composite, materializing each child at its share of `seed`.
    fn materialize(
        &self,
        py: Python<'_>,
        seed: Real,
    ) -> PyResult<fugazi_core::portfolio::Portfolio<Symbol>> {
        if self.children.is_empty() {
            return Err(PyValueError::new_err(
                "Portfolio.run: add at least one child strategy first",
            ));
        }
        if let Some(w) = &self.weights
            && w.len() != self.children.len()
        {
            return Err(PyValueError::new_err(format!(
                "Portfolio.weights: {} weights for {} children",
                w.len(),
                self.children.len()
            )));
        }

        // Each child is seeded with its own share, so a child sizing against
        // `value_frac(1.0)` commits its slice rather than the whole book —
        // matching how the builder splits cash on the Rust side.
        let shares: Vec<Real> = match &self.weights {
            Some(w) => {
                let total: Real = w.iter().sum();
                if total <= 0.0 {
                    return Err(PyValueError::new_err(
                        "Portfolio.weights: weights must not sum to zero",
                    ));
                }
                w.iter().map(|x| seed * x / total).collect()
            }
            None => vec![seed / self.children.len() as Real; self.children.len()],
        };

        let mut builder =
            fugazi_core::portfolio::Portfolio::<Symbol>::builder().with_initial_equity(seed);
        for ((name, child), share) in self.children.iter().zip(&shares) {
            let bound = child.bind(py);
            builder = if let Ok(s) = bound.cast::<PyStrategy>() {
                builder.add(name.clone(), s.borrow().materialize(*share))
            } else if let Ok(s) = bound.cast::<PyPairsStrategy>() {
                builder.add(name.clone(), s.borrow().materialize(*share))
            } else if let Ok(s) = bound.cast::<PyBasketStrategy>() {
                builder.add(name.clone(), s.borrow().materialize(py, *share))
            } else if let Ok(s) = bound.cast::<PyMultiAssetStrategy>() {
                builder.add(name.clone(), s.borrow().materialize(py, *share))
            } else {
                // `add` already vetted the type, so this is unreachable.
                return Err(PyValueError::new_err(
                    "Portfolio: unsupported child strategy type",
                ));
            };
        }
        if let Some(w) = &self.weights {
            builder = builder.weights(fugazi_core::portfolio::policy::Fixed::new(w.clone()));
        } else {
            builder = builder.weights(fugazi_core::portfolio::policy::EqualWeight);
        }
        if let Some(rebalance) = &self.rebalance {
            builder = builder.rebalance_on(rebalance.clone());
        }
        Ok(builder.build())
    }
}

// ---------------------------------------------------------------------------
// Unpickling entry points — see the note in `classes.rs`.
//
// These three exist because their public constructors take keyword-only
// arguments (`Order`'s `kind`/`id`/`commission`, `RunReport`'s `fills`/
// `ruin_bar`) or none at all (`Size`, which is built by four static methods).
// `__reduce__` can only hand pickle a positional tuple, so it hands it one of
// these instead of the class.
// ---------------------------------------------------------------------------

/// Rebuild a [`Size`](PySize) from `kind` + `value` — the inverse of the
/// getters of the same names.
#[pyfunction]
pub(crate) fn _rebuild_size(kind: &str, value: f64) -> PyResult<PySize> {
    match kind {
        "units" => Ok(PySize::units(value)),
        "funds_frac" => Ok(PySize::funds_frac(value)),
        "value_frac" => Ok(PySize::value_frac(value)),
        "position_frac" => Ok(PySize::position_frac(value)),
        other => Err(PyValueError::new_err(format!(
            "_rebuild_size: unknown kind {other:?}"
        ))),
    }
}

/// Rebuild an [`Order`](PyOrder) with every field positional.
#[pyfunction]
#[allow(clippy::too_many_arguments)]
pub(crate) fn _rebuild_order(
    symbol: String,
    side: &str,
    units: f64,
    price: f64,
    kind: &str,
    id: u64,
    commission: f64,
    requested_units: f64,
) -> PyResult<PyOrder> {
    PyOrder::new(
        symbol,
        side,
        units,
        price,
        kind,
        id,
        commission,
        Some(requested_units),
    )
}

/// Rebuild a [`RunReport`](PyRunReport) with every field positional.
#[pyfunction]
pub(crate) fn _rebuild_run_report(
    equity_curve: Vec<f64>,
    initial_equity: f64,
    fills: Option<Vec<PyFill>>,
    ruin_bar: Option<usize>,
) -> PyRunReport {
    PyRunReport::new(equity_curve, initial_equity, fills, ruin_bar)
}

// ---------------------------------------------------------------------------
// `fugazi.Wallet` — the classification the three concrete wallets lacked
// ---------------------------------------------------------------------------

/// The methods every wallet answers, whatever it is trading against.
///
/// Mirrors the Rust `Wallet` trait's core, minus the paper-only conveniences
/// (`orders`, `reset`, `set_costs_for`, `retention`, `adjust_funds`) that a live
/// venue documents as unbound — see the per-wallet ledgers in
/// `python/tests/test_parity.py`.
const WALLET_SURFACE: &[&str] = &[
    "position",
    "price",
    "equity",
    "funds",
    "can_short",
    "quote_ccy",
    "data_sources",
    "leverage",
    "update",
    "set",
    "set_position",
    "close",
    "set_stop",
    "set_take_profit",
    "set_limit",
    "cancel",
    "cancel_limit",
    "cancel_protective",
    "poll_fills",
];

/// Register `fugazi.Wallet`: an [`abc.ABCMeta`] class with the three concrete
/// wallets recorded as virtual subclasses.
///
/// Rust has a `Wallet` trait; Python had three unrelated classes reimplementing
/// the same twenty methods, so there was no `isinstance(w, Wallet)`, no way to
/// annotate "any wallet", and `test_parity.py` carried three near-identical
/// surface tests to compensate.
///
/// `register()` rather than real inheritance on purpose. Making the three
/// `#[pyclass(extends = ...)]` would put a shared base in the MRO for no gain —
/// there is no shared *implementation* to inherit, each one bridges a different
/// Rust type — while `register` gives `isinstance` and `issubclass` exactly what
/// they should say and touches none of them.
///
/// **Not an extension point.** Subclassing this in Python produces something
/// `Strategy.run` will refuse: the wallet argument resolves to one of the three
/// concrete pyclasses (see `over_any_wallet!`) because the run is generic over
/// the Rust trait and monomorphises per arm. Hence no `@abstractmethod`s — they
/// would advertise a contract that implementing gets you nothing. This is a
/// classification, and `WALLET_SURFACE` is the doc of what it classifies.
pub(crate) fn register_wallet_protocol(m: &Bound<'_, PyModule>) -> PyResult<()> {
    let py = m.py();
    let namespace = PyDict::new(py);
    namespace.set_item("__module__", "fugazi")?;
    namespace.set_item(
        "__doc__",
        format!(
            "Anything `Strategy.run` / `StrategySpec.run` will trade into.\n\n\
             `PaperWallet`, `OkxWallet` and `CoinbaseWallet` are registered as \
             virtual subclasses, so `isinstance(w, fugazi.Wallet)` is the way to \
             ask. Mirrors the Rust `Wallet` trait.\n\n\
             Common surface: {}.\n\n\
             This is a classification, not a base class to extend: a Python \
             subclass is not one of the three concrete wallets and `run` will \
             refuse it.",
            WALLET_SURFACE.join(", ")
        ),
    )?;
    let wallet = py.import("abc")?.getattr("ABCMeta")?.call1((
        "Wallet",
        pyo3::types::PyTuple::empty(py),
        namespace,
    ))?;
    for ty in [
        py.get_type::<PyWallet>(),
        py.get_type::<PyOkxWallet>(),
        py.get_type::<PyCoinbaseWallet>(),
    ] {
        wallet.call_method1("register", (ty,))?;
    }
    m.add("Wallet", wallet)
}
