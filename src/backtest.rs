//! Drive a [`Strategy`] over a bar series through a [`Wallet`], recording the
//! two artefacts every post-run analytic reduces to: the **equity curve** (one
//! mark-to-market point per bar) and the **fill blotter** (each booked order,
//! tagged with the bar it filled on).
//!
//! This is the pure primitive. It does no I/O, no formatting, and takes no
//! opinion on what to do with the report — a CLI backtester turns it into
//! `trades.csv` / `returns.csv` / `metrics.yml`, an optimizer runs it once per
//! parameter combination, a Python binding hands it to a notebook. The
//! [`Wallet`] is generic (taken as `&mut impl Wallet<Sym>`) so the same
//! primitive drives a [`PaperWallet`](crate::PaperWallet) backtest or a live
//! broker-backed impl unchanged — it's not backtest-only, hence the neutral
//! [`run`] name.
//!
//! Bars enter as [`Atom`]s — an OHLCV [`Candle`] plus optional overlay data. A
//! `Vec<Candle>` still works: [`Atom: From<Candle>`](crate::Atom) lifts each
//! candle to an atom with no overlays. Per bar, in order: feed the wallet the
//! candle (the fill stream it returns is routed to [`Strategy::on_fill`] and
//! collected into the blotter), [`Strategy::update`] the strategy with the
//! atom, then [`Strategy::trade`] it (queuing this bar's market orders —
//! [`PaperWallet`](crate::PaperWallet) fills them at the next bar's `open`).
//! The bar's mark-to-market equity is appended last.
//!
//! ```no_run
//! use fugazi::prelude::*;
//! use fugazi::backtest::run;
//!
//! # struct MyStrategy;
//! # impl Strategy for MyStrategy {
//! #     type Input = Atom;
//! #     type Symbol = String;
//! #     fn update(&mut self, _: Atom) {}
//! #     fn trade(&self, _: &mut dyn Wallet<String>) {}
//! #     fn reset(&mut self) {}
//! # }
//! # let mut strategy = MyStrategy;
//! # let candles: Vec<Candle> = vec![];
//! let mut wallet = PaperWallet::new(10_000.0);
//! let report = run(&mut strategy, &mut wallet, "BTC".to_string(), candles);
//! let bars = report.equity_curve.len();
//! let filled = report.fills.len();
//! # let _ = (bars, filled);
//! ```

use crate::{Atom, Order, Real, Strategy, Wallet};

/// One booked order stamped with the bar index it filled on.
///
/// Held in [`RunReport::fills`] in fill order — the same order the wallet
/// booked them. `bar` is the zero-based position in the input candle stream at
/// which the fill occurred (which, for [`PaperWallet`](crate::PaperWallet), is
/// the bar whose `open` the fill traded at, i.e. one bar after the signal).
#[derive(Debug, Clone)]
pub struct Fill<Sym> {
    /// Zero-based index into the input candle stream.
    pub bar: usize,
    /// The order that filled, as booked by the wallet (side, units, price, kind,
    /// id — see [`Order`]).
    pub order: Order<Sym>,
}

/// Everything a post-run analytic needs to reduce one run to numbers.
///
/// - [`equity_curve`](Self::equity_curve) holds one mark-to-market equity value
///   per input candle, in bar order.
/// - [`fills`](Self::fills) holds every order the wallet booked over the run,
///   in fill order, each tagged with the bar index it filled on.
/// - [`initial_equity`](Self::initial_equity) is the wallet's total equity
///   captured **before the first bar** — the seed value returns / CAGR compound
///   against.
#[derive(Debug, Clone)]
pub struct RunReport<Sym> {
    /// One entry per input candle, in bar order (post mark-to-market).
    pub equity_curve: Vec<Real>,
    /// Every booked fill, in the order the wallet produced them.
    pub fills: Vec<Fill<Sym>>,
    /// Total wallet equity captured immediately before the first bar.
    pub initial_equity: Real,
}

/// Drive `strategy` over `atoms`, executing against `wallet` (which is fed one
/// `(symbol, candle)` pair per bar), and return the [`RunReport`].
///
/// The reported [`equity_curve`](RunReport::equity_curve) has one entry per
/// bar (post mark-to-market for that bar). The reported
/// [`fills`](RunReport::fills) are the wallet's fill stream: for
/// [`PaperWallet`](crate::PaperWallet), the previous bar's queued market orders
/// filling at this bar's `open`, plus any resting protective legs this bar
/// triggered.
///
/// The wallet is passed in so the caller controls initial cash, wallet
/// implementation (paper vs. downstream broker-backed), and any pre-warming.
/// Pass the intended trading symbol as `symbol`; it is cloned once per bar to
/// feed [`Wallet::update`]. `atoms` is any iterable over anything convertible
/// to [`Atom`] — pass `Vec<Atom>` directly, or `Vec<Candle>` for the no-overlay
/// case (the [`From<Candle>`] lift is free). The size hint (when available)
/// pre-sizes the equity curve.
pub fn run<S, W, I, A>(
    strategy: &mut S,
    wallet: &mut W,
    symbol: S::Symbol,
    atoms: I,
) -> RunReport<S::Symbol>
where
    S: Strategy<Input = Atom>,
    W: Wallet<S::Symbol>,
    S::Symbol: Clone,
    I: IntoIterator<Item = A>,
    A: Into<Atom>,
{
    let initial_equity = wallet.equity().0;
    let iter = atoms.into_iter();
    let (lower, _) = iter.size_hint();
    let mut equity_curve = Vec::with_capacity(lower);
    let mut fills: Vec<Fill<S::Symbol>> = Vec::new();

    for (bar, atom) in iter.enumerate() {
        let atom: Atom = atom.into();
        // The wallet's fill stream: any queued market order filling this bar
        // at its open, plus any resting protective leg triggered. Route each
        // fill through the strategy first (so its on_fill can update internal
        // state), then record it.
        for fill in wallet.update(symbol.clone(), atom.candle) {
            strategy.on_fill(&fill);
            fills.push(Fill { bar, order: fill });
        }
        strategy.update(atom);
        // update()/on_fill() always run so warm-up progresses; trade() only
        // runs once the strategy reports ready. is_ready() defaults to true,
        // so this is a no-op for strategies that don't override it.
        if strategy.is_ready() {
            strategy.trade(wallet);
        }
        equity_curve.push(wallet.equity().0);
    }

    RunReport {
        equity_curve,
        fills,
        initial_equity,
    }
}
