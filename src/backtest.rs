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
//! Bars enter as [`Snapshot<Sym>`](crate::types::Snapshot)s — a per-bar
//! keyed collection of tagged [`Atom`]s. Single-series callers can lift a
//! `Vec<Candle>` or `Vec<Atom>` via the shorthand
//! [`Snapshot::of_atom`](crate::types::Snapshot::of_atom) (or the
//! [`Atom → Snapshot<Sym>`](crate::types::Snapshot#impl-From%3CAtom%3E-for-Snapshot%3CSym%3E)
//! `From` — untagged, size-1). Per bar, in order: extract the strategy's own
//! asset out of the snapshot for the wallet mark-to-market, feed the wallet
//! that candle (the fill stream it returns is routed to
//! [`Strategy::on_fill`] and collected into the blotter),
//! [`Strategy::update`] the strategy with the whole snapshot, then
//! [`Strategy::trade`] it (queuing this bar's market orders —
//! [`PaperWallet`](crate::PaperWallet) fills them at the next bar's `open`).
//! The bar's mark-to-market equity is appended last.

use crate::types::{Selector, Snapshot};
use crate::{Order, Real, Strategy, Wallet};

/// One booked order stamped with the bar index it filled on.
///
/// Held in [`RunReport::fills`] in fill order — the same order the wallet
/// booked them. `bar` is the zero-based position in the input snapshot stream
/// at which the fill occurred (which, for [`PaperWallet`](crate::PaperWallet),
/// is the bar whose `open` the fill traded at, i.e. one bar after the signal).
#[derive(Debug, Clone)]
pub struct Fill<Sym> {
    /// Zero-based index into the input snapshot stream.
    pub bar: usize,
    /// The order that filled, as booked by the wallet (side, units, price, kind,
    /// id — see [`Order`]).
    pub order: Order<Sym>,
}

/// Everything a post-run analytic needs to reduce one run to numbers.
///
/// - [`equity_curve`](Self::equity_curve) holds one mark-to-market equity value
///   per input snapshot, in bar order.
/// - [`fills`](Self::fills) holds every order the wallet booked over the run,
///   in fill order, each tagged with the bar index it filled on.
/// - [`initial_equity`](Self::initial_equity) is the wallet's total equity
///   captured **before the first bar** — the seed value returns / CAGR compound
///   against.
#[derive(Debug, Clone)]
pub struct RunReport<Sym> {
    /// One entry per input snapshot, in bar order (post mark-to-market).
    pub equity_curve: Vec<Real>,
    /// Every booked fill, in the order the wallet produced them.
    pub fills: Vec<Fill<Sym>>,
    /// Total wallet equity captured immediately before the first bar.
    pub initial_equity: Real,
}

/// Drive `strategy` over `snapshots`, executing against `wallet` (which is fed
/// one `(symbol, candle)` pair per bar, extracted from each snapshot), and
/// return the [`RunReport`].
///
/// The reported [`equity_curve`](RunReport::equity_curve) has one entry per
/// bar (post mark-to-market for that bar). The reported
/// [`fills`](RunReport::fills) are the wallet's fill stream: for
/// [`PaperWallet`](crate::PaperWallet), the previous bar's queued market orders
/// filling at this bar's `open`, plus any resting protective legs this bar
/// triggered.
///
/// Per bar, the strategy's own asset is located inside the incoming
/// [`Snapshot`] via `snap.find(&Selector::by_symbol(symbol.clone()))`; when
/// no tag matches (the single-series driver hot path — untagged size-1
/// snapshot via [`Snapshot::of_atom`]), it falls back to
/// [`Snapshot::sole_atom`]. That atom's candle is what the wallet marks to
/// market; the whole snapshot is what the strategy sees, so its leaves can
/// [`Pick`](crate::indicators::Pick) any asset out of the frame.
///
/// The wallet is passed in so the caller controls initial cash, wallet
/// implementation (paper vs. downstream broker-backed), and any pre-warming.
/// Pass the intended trading symbol as `symbol`; it's cloned once per bar to
/// feed [`Wallet::update`]. `snapshots` is any iterable over anything
/// convertible to [`Snapshot<S::Symbol>`] — pass `Vec<Snapshot<Sym>>`
/// directly, or `Vec<Candle>` / `Vec<Atom>` and each entry lifts into an
/// untagged size-1 snapshot via [`Atom::from`]. The size hint (when
/// available) pre-sizes the equity curve.
pub fn run<Sym, S, W, I, A>(
    strategy: &mut S,
    wallet: &mut W,
    symbol: Sym,
    snapshots: I,
) -> RunReport<Sym>
where
    Sym: Clone + PartialEq,
    S: Strategy<Symbol = Sym, Input = Snapshot<Sym>>,
    W: Wallet<Sym>,
    I: IntoIterator<Item = A>,
    A: Into<Snapshot<Sym>>,
{
    let initial_equity = wallet.equity().0;
    let iter = snapshots.into_iter();
    let (lower, _) = iter.size_hint();
    let mut equity_curve = Vec::with_capacity(lower);
    let mut fills: Vec<Fill<Sym>> = Vec::new();

    let query = Selector::by_symbol(symbol.clone());
    for (bar, snap) in iter.enumerate() {
        let snap: Snapshot<S::Symbol> = snap.into();
        // Route the strategy's own asset out of the snapshot for the wallet's
        // mark-to-market. Prefer the symbol-matching entry (single- and
        // cross-asset); fall back to the sole atom for the untagged
        // single-series shortcut (`Snapshot::of_atom(atom)`).
        let self_atom = snap.find(&query).or_else(|| snap.sole_atom());
        if let Some(atom) = self_atom.cloned() {
            // The wallet's fill stream: any queued market order filling this
            // bar at its open, plus any resting protective leg triggered.
            // Route each fill through the strategy first (so its on_fill can
            // update internal state), then record it.
            for fill in wallet.update(symbol.clone(), atom.candle) {
                strategy.on_fill(&fill);
                fills.push(Fill { bar, order: fill });
            }
        }
        strategy.update(snap);
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
