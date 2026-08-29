//! A catalogue of **classical single-asset strategies**, each ready to trade
//! into a [`Wallet`](crate::Wallet).
//!
//! Almost every classical single-asset strategy has the same shape — a long /
//! flat / short position driven by a handful of boolean conditions, sized all-in
//! — so the catalogue factors that shape into one generic type,
//! [`SingleAssetStrategy`], and expresses each named strategy as a thin
//! specialisation that builds its particular entry/exit [`Signal`](crate::Signal)s.
//! (`SingleAssetStrategy` is itself just "the user's own type implementing the
//! trait", parameterised over its signals; a strategy that does not fit its
//! long/flat/short, all-in mould — like [`ZScoreReversion`](mean_reversion::ZScoreReversion)'s
//! bespoke sizing — still spells out its own [`Strategy`](crate::Strategy) impl.)
//!
//! Every strategy:
//!
//! * is generic over the symbol type `Sym: Clone + PartialEq + std::hash::Hash + Eq + 'static` and
//!   takes `Input = Snapshot<Sym>` (the multi-asset input frame). The
//!   catalogue's specialisations wire their leaves through
//!   [`Pick::<Sym>::new()`](crate::indicators::Pick::new) — the empty-selector
//!   single-entry unpack — so a single-series driver feeding size-1
//!   snapshots gets the same behaviour as a raw atom stream, and a
//!   cross-asset driver can layer explicit
//!   [`Pick::matching(Selector::by_symbol(...))`](crate::indicators::Pick::matching)
//!   composition on top.
//! * in [`update`](crate::Strategy::update) advances **all** of its
//!   signals/indicators every bar (never short-circuiting, or a skipped source
//!   desyncs from the price stream), then decides in [`trade`](crate::Strategy::trade);
//! * sizes positions all-in via [`Size::value_frac(1.0)`](crate::Size). Two
//!   flavours of position management appear:
//!   - **long/flat** — go all-in long on an entry edge, [`close`](crate::Wallet::close)
//!     on an exit edge ([`SingleAssetStrategy::long_on`]);
//!   - **long/short** (always-in) — flip with a single
//!     [`set`](crate::Wallet::set) to the other side ([`SingleAssetStrategy::long_on`] +
//!     [`short_on`](SingleAssetStrategy::short_on)).
//!     Because `value_frac` resolves against equity (which survives a reversal,
//!     unlike cash), one `set` reverses and re-sizes all-in exactly — no
//!     flatten-then-reopen.
//!
//! The families:
//!
//! * [`trend`] — crossover / breakout trend-following.
//! * [`mean_reversion`] — oscillator and band reversion.
//! * [`momentum`] — rate-of-change / oscillator-vs-midline.
//! * [`volume`] — volume- and flow-based.
//! * [`composite`] — multi-condition (trend gated by strength, dip-in-uptrend).

pub mod basket;
pub mod composite;
pub mod mean_reversion;
pub mod momentum;
pub mod multi_asset;
pub mod pairs;
pub mod selection;
pub mod single_asset;
pub mod trend;
pub mod universe;
pub mod volume;

pub use basket::BasketStrategy;
pub use multi_asset::MultiAssetStrategy;
pub use pairs::PairsStrategy;
pub use single_asset::SingleAssetStrategy;

use crate::Indicator;
use crate::indicators::{Close, CurrentBar, High, Low, Pick, Position};
use crate::types::{Real, Snapshot};

/// A boxed real-valued chain over a per-bar snapshot — what a per-symbol
/// factory produces.
///
/// `basket.rs` called this `Chain` and `multi_asset.rs` called it
/// `LevelChain`; they were the same type, written twice.
pub(crate) type Chain<Sym> = Box<dyn Indicator<Input = Snapshot<Sym>, Output = Real> + Send + Sync>;

/// A per-symbol, position-aware factory for a protective level.
pub(crate) type LevelFactory<Sym> = Box<dyn Fn(&Sym, &Position) -> Chain<Sym> + Send + Sync>;

/// Box a user-supplied protective-level factory into a [`LevelFactory`].
///
/// The four builders (`long_stop_loss`, `long_take_profit`,
/// `short_stop_loss`, `short_take_profit`) existed on both the basket and
/// multi-asset shapes with byte-identical bodies — eight copies of the same
/// five lines, differing only in a local binding name and which of the two
/// spellings of `Chain` they named.
pub(crate) fn level_factory<Sym, F, L>(factory: F) -> LevelFactory<Sym>
where
    F: Fn(&Sym, &Position) -> L + 'static + Send + Sync,
    L: Indicator<Input = Snapshot<Sym>, Output = Real> + 'static + Send + Sync,
{
    Box::new(move |sym: &Sym, pos: &Position| {
        let chain: Chain<Sym> = Box::new(factory(sym, pos));
        chain
    })
}

/// Shorthand for `Close::of(Pick::<Sym>::new())` — read the strategy's own
/// asset's close out of the incoming [`Snapshot`](crate::types::Snapshot).
/// The empty-selector [`Pick`] unpacks a size-1 snapshot on the single-series
/// hot path and matches by symbol at the strategy layer otherwise.
pub(crate) fn self_close<Sym: Clone + PartialEq + std::hash::Hash + Eq + 'static + Send + Sync>()
-> Close<Pick<Sym>> {
    Close::of(Pick::<Sym>::new())
}

/// Shorthand for `High::of(Pick::<Sym>::new())` — see [`self_close`].
pub(crate) fn self_high<Sym: Clone + PartialEq + std::hash::Hash + Eq + 'static + Send + Sync>()
-> High<Pick<Sym>> {
    High::of(Pick::<Sym>::new())
}

/// Shorthand for `Low::of(Pick::<Sym>::new())` — see [`self_close`].
pub(crate) fn self_low<Sym: Clone + PartialEq + std::hash::Hash + Eq + 'static + Send + Sync>()
-> Low<Pick<Sym>> {
    Low::of(Pick::<Sym>::new())
}

/// Shorthand for `CurrentBar::of(Pick::<Sym>::new())` — read the strategy's
/// own asset's whole [`Candle`](crate::types::Candle) out of the snapshot;
/// used to root the bar indicators (`Atr`, `Adx`, `Obv`, …).
pub(crate) fn self_bar<Sym: Clone + PartialEq + std::hash::Hash + Eq + 'static + Send + Sync>()
-> CurrentBar<Pick<Sym>> {
    CurrentBar::of(Pick::<Sym>::new())
}

// ---------------------------------------------------------------------------
// The shared per-leg decision
// ---------------------------------------------------------------------------

/// One leg's resolved inputs for [`trade_leg`] — every slot already read to a
/// plain value, so the caller owns *how* it reads them and this owns *what to
/// do* with them.
///
/// That split is what lets the single-asset and multi-asset shapes share the
/// decision: the former reads `Signal::is_true()` on its own fields, the latter
/// `.value().unwrap_or(false)` on a per-symbol state, and those are the same
/// thing (`is_true` is defined as exactly that) once resolved.
pub(crate) struct Leg<'a, Sym> {
    pub symbol: &'a Sym,
    /// The `value_frac` magnitude for entries and rebalance resizes.
    pub size: Real,
    pub is_long: bool,
    pub is_short: bool,
    pub enter_long: bool,
    pub enter_short: bool,
    pub close_long: bool,
    pub close_short: bool,
    /// Whether this bar's rebalance gate fired.
    pub rebalancing: bool,
    pub long_stop: Option<Real>,
    pub long_target: Option<Real>,
    pub short_stop: Option<Real>,
    pub short_target: Option<Real>,
}

// ---------------------------------------------------------------------------
// The one-shot rebalance override
// ---------------------------------------------------------------------------

/// The latch behind [`Strategy::force_rebalance`](crate::Strategy::force_rebalance),
/// held by every shape that owns a rebalance gate.
///
/// `None` is the resting state. `Some(hold)` means *the next `trade` runs as
/// though the gate fired*, leaving the symbols in `hold` alone — and `hold` is
/// very often empty, which is why this is an `Option<Vec<_>>` rather than a
/// `Vec<_>` plus a `bool` that could disagree with it.
pub(crate) type ForcedRebalance<Sym> = Option<Vec<Sym>>;

/// Arm (`Some`) or clear (`None`) a [`ForcedRebalance`] latch — the whole body
/// of every shape's `force_rebalance`, so the five cannot drift.
pub(crate) fn arm_rebalance<Sym: Clone>(latch: &mut ForcedRebalance<Sym>, hold: Option<&[Sym]>) {
    *latch = hold.map(<[Sym]>::to_vec);
}

/// Whether an armed latch wants `symbol` rebalanced this bar — armed, and not
/// named in the hold list.
pub(crate) fn forced_for<Sym: PartialEq>(latch: &ForcedRebalance<Sym>, symbol: &Sym) -> bool {
    latch.as_ref().is_some_and(|hold| !hold.contains(symbol))
}

/// Whether an armed latch is holding `symbol` back — the caller named it in
/// `hold`, so this bar's override must produce no order flow for it at all.
///
/// The narrower instruction wins: `hold` states an absolute target the driver
/// settles once the bar is over, and a rebalance that also moved the symbol
/// would be undone by it — silently on a
/// [`PaperWallet`](crate::PaperWallet), whose `settle_position` drops the
/// queued move, and at the cost of a real round trip on a live venue, where the
/// order has already reached the broker.
pub(crate) fn held_back<Sym: PartialEq>(latch: &ForcedRebalance<Sym>, symbol: &Sym) -> bool {
    latch.as_ref().is_some_and(|hold| hold.contains(symbol))
}

/// Drive one leg's wallet interaction for one bar.
///
/// `SingleAssetStrategy::trade` and `MultiAssetStrategy::trade` ran this
/// algorithm as two independent copies — the same order, the same comments, the
/// same `set` / `cancel_protective` / `close` / rebalance-gate / rest-protective
/// sequence, differing only in `self.x` vs `state.x`, a fixed symbol vs a loop
/// variable, and `return` vs `continue`. ARCHITECTURE.md describes multi-asset
/// as "every symbol runs the same `SingleAssetStrategy`-shaped decision in
/// isolation"; the code had forked, with nothing to keep the two aligned.
///
/// The order matters and is load-bearing:
///
/// 1. **Entries first**, magnitude from `size`, reversal-capable. The fill lands
///    next bar at the open, so any resting bracket is cancelled now — a reversal
///    voids the old one.
/// 2. **Signal-driven exits** to flat, also filling next bar at the open.
/// 3. **The rebalance gate**: resize a held position to the current sizing
///    target. `set` at the side already held is idempotent when the target
///    matches, so an unchanged target queues no fill. Protective levels survive,
///    because the position stays on the same side and its anchor / peak / trough
///    carry through the `Position::apply` merge.
/// 4. **Rest the active side's protective levels**, re-submitted every bar so a
///    trailing level cancel/replaces. The wallet reads the side from the
///    position, so a stop is always the adverse level and a take-profit the
///    favourable one.
///
/// Steps 1 and 2 are terminal for the leg — they return early, exactly as the
/// two originals did.
pub(crate) fn trade_leg<Sym: Clone>(leg: &Leg<'_, Sym>, wallet: &mut dyn crate::Wallet<Sym>) {
    use crate::wallet::{Reference, Side, Size};

    if leg.enter_long && !leg.is_long {
        let _ = wallet.set(leg.symbol.clone(), Side::Buy, Size::value_frac(leg.size));
        let _ = wallet.cancel_protective(leg.symbol);
        return;
    }
    if leg.enter_short && !leg.is_short {
        let _ = wallet.set(leg.symbol.clone(), Side::Sell, Size::value_frac(leg.size));
        let _ = wallet.cancel_protective(leg.symbol);
        return;
    }
    if (leg.close_long && leg.is_long) || (leg.close_short && leg.is_short) {
        let _ = wallet.close(leg.symbol.clone());
        let _ = wallet.cancel_protective(leg.symbol);
        return;
    }
    if leg.rebalancing {
        if leg.is_long {
            let _ = wallet.set(leg.symbol.clone(), Side::Buy, Size::value_frac(leg.size));
        } else if leg.is_short {
            let _ = wallet.set(leg.symbol.clone(), Side::Sell, Size::value_frac(leg.size));
        }
    }
    let (stop, target) = if leg.is_long {
        (leg.long_stop, leg.long_target)
    } else if leg.is_short {
        (leg.short_stop, leg.short_target)
    } else {
        (None, None)
    };
    if let Some(level) = stop {
        let _ = wallet.set_stop(
            leg.symbol.clone(),
            Reference(level),
            Size::position_frac(1.0),
        );
    }
    if let Some(level) = target {
        let _ = wallet.set_take_profit(
            leg.symbol.clone(),
            Reference(level),
            Size::position_frac(1.0),
        );
    }
}
