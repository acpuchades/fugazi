//! [`BasketStrategy`]: a cross-sectional, multi-symbol ranker with a
//! caller-declared or floating universe.
//!
//! Where [`SingleAssetStrategy`](crate::strategies::SingleAssetStrategy) drives
//! one asset from boolean signals and [`PairsStrategy`](crate::strategies::PairsStrategy)
//! drives two symbols as a spread, `BasketStrategy` reads the whole
//! [`Snapshot<Sym>`](crate::types::Snapshot) each bar: it scores every symbol
//! present with a per-symbol *scoring* source, applies a
//! [`Selection`] impl (mapping the score map to per-symbol [`Side`]s),
//! and drives each selection long / short / flat. The default universe
//! is *floating*: symbols are
//! discovered from the incoming snapshot on first sight, and the per-symbol
//! score / sizing chains are spun up lazily by user-supplied factories — no
//! upfront universe list, no reject on a new listing.
//!
//! A caller who wants to catch feed gaps or typos declares an explicit
//! [`Universe`] via [`BasketStrategy::all_of`] (strict — every listed symbol
//! must be present in every snapshot, panics on absence, readiness waits
//! until all are ready) or [`BasketStrategy::any_of`] (lax — restricts to
//! the listed subset but silently ignores absent or still-unready members).
//!
//! The crate ships three built-in [`Selection`] impls —
//! [`TopBottom`], [`Threshold`], and [`Quantile`] — plus matching
//! builder shortcuts on [`BasketStrategy`] that install them
//! ([`top_bottom`](BasketStrategy::top_bottom) /
//! [`threshold`](BasketStrategy::threshold) /
//! [`quantile`](BasketStrategy::quantile)). A caller who needs
//! something the built-ins don't cover installs a custom impl (or a
//! closure, via the [`Selection`] blanket impl) via
//! [`BasketStrategy::selection`].

use std::collections::HashMap;
use std::hash::Hash;

use crate::indicators::{Book, Every, Position, Value};
use crate::prelude::*;
use crate::types::Snapshot;
use super::{Chain, LevelFactory};


/// A per-symbol factory: builds a fresh [`Chain`] for the given symbol. Called
/// exactly once per symbol the first time it appears in a snapshot.
type Factory<Sym> = Box<dyn Fn(&Sym) -> Chain<Sym> + Send + Sync>;


// The `Selection` algebra and the `Universe` trait moved to sibling
// modules — they are independent of this shape (multi-asset uses the
// latter too). Re-exported so `strategies::basket::{Selection, AllOf, …}`
// paths keep resolving.
pub use super::selection::{
    DynSelection, Everything, Quantile, Selection, Sides, Threshold, TopBottom, quantile,
    threshold, top_bottom,
};
pub use super::universe::{AllOf, AnyOf, Floating, Universe};



/// A cross-sectional, ranking basket strategy over a floating universe.
///
/// Each bar `BasketStrategy` scores every symbol present in the incoming
/// [`Snapshot<Sym>`](crate::types::Snapshot) using the caller-supplied
/// **score factory**, calls its **selection closure** on the score map to
/// decide who goes long / short, reads each selected symbol's per-leg
/// [`ValueFraction`](crate::Size::ValueFraction) from the **sizing
/// factory**, and drives the wallet accordingly. The selection is
/// installed via one of the sugar builders
/// ([`top_bottom`](Self::top_bottom) / [`threshold`](Self::threshold) /
/// [`quantile`](Self::quantile), each wrapping the matching free
/// function) or via [`selection`](Self::selection) with an arbitrary
/// closure.
///
/// ## Universe
///
/// By default the universe is **floating**: symbols aren't declared
/// upfront — the strategy owns two factories, `Fn(&Sym) -> impl Indicator`,
/// and calls each factory once on the first bar a new symbol appears in
/// the snapshot. Every leaf inside is expected to root itself on the
/// current symbol via [`Pick`](crate::indicators::Pick) /
/// [`Selector::by_symbol`](crate::types::Selector::by_symbol), so the
/// same factory closure produces a per-symbol chain for every symbol it's
/// asked about. A symbol that stops appearing keeps its chain (in case it
/// comes back) but rolls off the score / sizing maps as its chain's
/// `Pick` reads `None` — so the ranker only sees the currently-live symbols.
///
/// A caller who wants to *catch* feed gaps or typos declares an explicit
/// [`Universe`] via [`all_of`](Self::all_of) (strict: every listed symbol
/// must appear on every bar — panics otherwise — and
/// [`is_ready`](Strategy::is_ready) waits until all listed symbols score
/// `Some`) or [`any_of`](Self::any_of) (lax: restricts to the listed
/// subset but silently ignores absent / unready members). Either way
/// non-listed symbols are filtered out at discovery — no chain gets built
/// for a symbol outside the declared universe.
///
/// ## Sizing
///
/// The sizing factory is a *per-leg* [`ValueFraction`](crate::Size::ValueFraction)
/// magnitude — same semantics as
/// [`SingleAssetStrategy::position_sizing`](crate::strategies::SingleAssetStrategy::position_sizing).
/// No auto-normalization: `sized_by(|_| equal_weight(N)).top_bottom(N/2, N/2)`
/// yields 100% gross exposure. Use
/// [`sizing::equal_weight`](crate::indicators::sizing::equal_weight) for the
/// common case. For a per-symbol *vol-target* or *ATR-risk* chain, reach
/// for the source-generic
/// [`vol_target_of`](crate::indicators::sizing::vol_target_of) /
/// [`atr_risk_of`](crate::indicators::sizing::atr_risk_of) recipes and
/// hand them a per-leg `Pick::matching(Selector::by_symbol(sym.clone()))`
/// — the no-source
/// [`vol_target`](crate::indicators::sizing::vol_target) /
/// [`atr_risk`](crate::indicators::sizing::atr_risk) shortcuts default to
/// the empty-selector `Pick::new()`, which panics on a multi-symbol
/// snapshot. A symbol whose sizing reads `None` this bar is skipped for
/// entry (safe default; opt out with a fallback in the sizing closure).
///
/// ## Readiness
///
/// [`is_ready`](Strategy::is_ready) returns `true`; per-symbol readiness
/// is enforced inside [`trade`](Strategy::trade) by only ranking symbols
/// whose score chain has produced a `Some` reading this bar. A symbol
/// whose score is still `None` (still warming, or missing from the
/// snapshot) is not selected, so it never trades — same "unsettled data
/// ⇒ wait" convention as the rest of the crate, applied per-symbol rather
/// than gate-the-whole-strategy.
///
/// ## Costs
///
/// Costs live on the [`Wallet`], not on the strategy: per-symbol trading
/// costs installed via
/// [`PaperWallet::set_costs_for`](crate::PaperWallet::set_costs_for) apply
/// transparently to every fill on that symbol, whichever leg it lands on.
/// A caller wiring a 20-symbol basket loops
/// `wallet.set_costs_for(sym, ...)` once per symbol at setup and never
/// mentions costs again on the strategy.
///
/// ## Book anchor
///
/// The strategy owns a shared [`Book`] tracking aggregate cash / positions
/// / equity across every leg — one trade is one open-to-flat cycle across
/// the whole basket, matching how
/// [`PairsStrategy`](crate::strategies::PairsStrategy) accounts for its
/// two-leg book. Access it via [`book`](Self::book); seed it via
/// [`with_initial_equity`](Self::with_initial_equity) to match the
/// wallet's starting cash for the book-anchored sizing recipes
/// ([`drawdown_throttle`](crate::indicators::sizing::drawdown_throttle),
/// [`equity_vol_target`](crate::indicators::sizing::equity_vol_target),
/// [`fractional_kelly`](crate::indicators::sizing::fractional_kelly)) to
/// read meaningful numbers.
///
/// ## Example
///
/// A 4-symbol momentum basket: score every symbol by its 20-bar rate of
/// change, take the top-2 long and the bottom-2 short at equal weight so
/// gross exposure is 100%.
///
/// ```
/// use fugazi::prelude::*;
/// use fugazi::indicators::sizing::equal_weight;
/// use fugazi::indicators::{Close, Pick, Roc};
/// use fugazi::strategies::BasketStrategy;
/// use fugazi::types::Selector;
///
/// let strat: BasketStrategy<String> =
///     BasketStrategy::with_initial_equity(100_000.0)
///         .scored_by(|sym: &String| {
///             Roc::new(
///                 Close::of(Pick::matching(Selector::by_symbol(sym.clone()))),
///                 20,
///             )
///         })
///         .sized_by(|_sym: &String| equal_weight::<String>(4))
///         .top_bottom(2, 2);
/// # let _ = strat;
/// ```
/// A boolean chain over the basket's `Snapshot<Sym>` — the shape used
/// by the [`rebalance`](BasketStrategy::rebalance_on) gate signal.
type RebalanceSignal<Sym> = Box<dyn Indicator<Input = Snapshot<Sym>, Output = bool> + Send + Sync>;

pub struct BasketStrategy<Sym> {
    score_factory: Factory<Sym>,
    sizing_factory: Factory<Sym>,
    /// Per-symbol protective-level factories. When present, each
    /// scored symbol also builds a stop / take-profit chain that reads
    /// against that symbol's [`Position`] (via `position.entry()` etc.);
    /// `trade` re-submits the resting level after entries, and closes
    /// the bracket on flatten.
    long_stop_factory: Option<LevelFactory<Sym>>,
    long_target_factory: Option<LevelFactory<Sym>>,
    short_stop_factory: Option<LevelFactory<Sym>>,
    short_target_factory: Option<LevelFactory<Sym>>,
    scores: HashMap<Sym, Chain<Sym>>,
    sizes: HashMap<Sym, Chain<Sym>>,
    /// Per-symbol protective-level chains built lazily on first sight
    /// (mirrors [`scores`](Self::scores) / [`sizes`](Self::sizes)).
    long_stops: HashMap<Sym, Chain<Sym>>,
    long_targets: HashMap<Sym, Chain<Sym>>,
    short_stops: HashMap<Sym, Chain<Sym>>,
    short_targets: HashMap<Sym, Chain<Sym>>,
    positions: HashMap<Sym, Position>,
    latest_score: HashMap<Sym, Real>,
    latest_size: HashMap<Sym, Real>,
    selection: Box<dyn Selection<Sym>>,
    /// The **rebalance gate**: on each bar `trade()` runs the selection
    /// and issues resize orders only when this signal reads `true`.
    /// Default is `Every::new(1)` — fires every bar, matching the
    /// pre-`rebalance_on` "re-rank every bar" behavior. Set with
    /// [`rebalance_on`](Self::rebalance_on).
    rebalance: RebalanceSignal<Sym>,
    universe: Box<dyn Universe<Sym>>,
    book: Book<Sym>,
    /// If `true`, per-symbol sizes are scaled at each rebalance so
    /// `Σ long_sizes == Σ short_sizes` (dollar-neutral). Set via
    /// [`dollar_neutral`](Self::dollar_neutral); defaults to `false`.
    dollar_neutral: bool,
}

impl<Sym: Clone + PartialEq + Hash + Eq + 'static + Send + Sync> BasketStrategy<Sym> {
    /// A fresh basket with a seed-1.0 [`Book`], the default zero score /
    /// zero sizing factories, and a no-op selection (empty map, so nothing
    /// is picked). All three defaults trade nothing — a basket only comes
    /// alive once you call [`scored_by`](Self::scored_by),
    /// [`sized_by`](Self::sized_by), and one of the selection builders
    /// ([`top_bottom`](Self::top_bottom) / [`threshold`](Self::threshold) /
    /// [`quantile`](Self::quantile) / [`selection`](Self::selection)).
    ///
    /// See [`with_initial_equity`](Self::with_initial_equity) for the
    /// real-money constructor — the seed-1.0 book here is fine for
    /// unit-scale tests but book-anchored sizing recipes need the book
    /// seed to match the wallet's starting cash.
    pub fn new() -> Self {
        Self::with_initial_equity(1.0)
    }

    /// A fresh basket whose [`Book`] is seeded at `initial_equity` — the
    /// assumed starting capital. Match the wallet's seed for aggregate
    /// equity / drawdown numbers to be meaningful.
    ///
    /// # Panics
    /// Panics if `initial_equity` is not strictly positive.
    pub fn with_initial_equity(initial_equity: Real) -> Self {
        Self {
            score_factory: Box::new(|_sym: &Sym| {
                let ind: Chain<Sym> = Box::new(Value::<Snapshot<Sym>>::new(0.0));
                ind
            }),
            sizing_factory: Box::new(|_sym: &Sym| {
                let ind: Chain<Sym> = Box::new(Value::<Snapshot<Sym>>::new(0.0));
                ind
            }),
            long_stop_factory: None,
            long_target_factory: None,
            short_stop_factory: None,
            short_target_factory: None,
            scores: HashMap::new(),
            sizes: HashMap::new(),
            long_stops: HashMap::new(),
            long_targets: HashMap::new(),
            short_stops: HashMap::new(),
            short_targets: HashMap::new(),
            positions: HashMap::new(),
            latest_score: HashMap::new(),
            latest_size: HashMap::new(),
            selection: Box::new(|_scores: &HashMap<Sym, Real>| HashMap::new()),
            rebalance: Box::new(Every::<Snapshot<Sym>>::new(1)),
            universe: Box::new(Floating),
            book: Book::new(initial_equity),
            dollar_neutral: false,
        }
    }

    /// Attach a per-leg **long stop-loss** factory. Called once per new
    /// symbol on first sight, receiving both the symbol and its
    /// [`Position`] so `position.entry()` / `.peak()` levels compose
    /// exactly as on [`SingleAssetStrategy`](crate::strategies::SingleAssetStrategy).
    /// The returned chain reads a `Real` level per bar; the basket
    /// re-submits the resting stop after every long entry, and cancels
    /// the bracket on flatten.
    pub fn long_stop_loss<F, L>(mut self, factory: F) -> Self
    where
        F: Fn(&Sym, &Position) -> L + 'static + Send + Sync,
        L: Indicator<Input = Snapshot<Sym>, Output = Real> + 'static + Send + Sync,
    {
        self.long_stop_factory = Some(super::level_factory(factory));
        self
    }

    /// Attach a per-leg **long take-profit** factory. See
    /// [`long_stop_loss`](Self::long_stop_loss) for the factory shape.
    pub fn long_take_profit<F, L>(mut self, factory: F) -> Self
    where
        F: Fn(&Sym, &Position) -> L + 'static + Send + Sync,
        L: Indicator<Input = Snapshot<Sym>, Output = Real> + 'static + Send + Sync,
    {
        self.long_target_factory = Some(super::level_factory(factory));
        self
    }

    /// Attach a per-leg **short stop-loss** factory. See
    /// [`long_stop_loss`](Self::long_stop_loss) for the factory shape.
    pub fn short_stop_loss<F, L>(mut self, factory: F) -> Self
    where
        F: Fn(&Sym, &Position) -> L + 'static + Send + Sync,
        L: Indicator<Input = Snapshot<Sym>, Output = Real> + 'static + Send + Sync,
    {
        self.short_stop_factory = Some(super::level_factory(factory));
        self
    }

    /// Attach a per-leg **short take-profit** factory. See
    /// [`long_stop_loss`](Self::long_stop_loss) for the factory shape.
    pub fn short_take_profit<F, L>(mut self, factory: F) -> Self
    where
        F: Fn(&Sym, &Position) -> L + 'static + Send + Sync,
        L: Indicator<Input = Snapshot<Sym>, Output = Real> + 'static + Send + Sync,
    {
        self.short_target_factory = Some(super::level_factory(factory));
        self
    }

    /// Enforce **dollar-neutrality**: at each rebalance, scale per-symbol
    /// sizes so that the sum of long weights equals the sum of short
    /// weights. Concretely, the smaller of the two per-side sums is
    /// taken as the target gross-per-side (never levers up), and each
    /// side's sizes are rescaled by `target / side_sum`.
    ///
    /// If the selection is one-sided on a given fire bar (no longs, or
    /// no shorts), the basket **skips that rebalance** — dollar-neutral
    /// with no hedgeable counter-side is undefined; the safe default is
    /// to sit rather than run an accidental net-long or net-short leg.
    ///
    /// Off by default. Compose with any selection rule
    /// ([`top_bottom`](Self::top_bottom) / [`threshold`](Self::threshold) /
    /// [`quantile`](Self::quantile) / [`selection`](Self::selection)).
    pub fn dollar_neutral(mut self) -> Self {
        self.dollar_neutral = true;
        self
    }

    /// Install the **rebalance gate** — a boolean signal that decides,
    /// on each bar, whether [`trade`](Strategy::trade) re-runs the
    /// selection and issues resize orders. Defaults to
    /// [`Every::new(1)`](crate::indicators::Every) (fires every bar,
    /// preserving the pre-`rebalance_on` behavior).
    ///
    /// A less-frequent rebalance both reduces turnover (churn on noisy
    /// scores) and lets the basket hold "stale" picks between rebalance
    /// events. That's usually the desired trade-off for
    /// weekly/monthly-rebalanced strategies; compose with a
    /// drawdown-triggered signal (`!or [!every 20, !drawdown_exceeds 0.1]`
    /// in YAML) if you want drift protection between rebalances too.
    ///
    /// A `None` reading from the gate is treated as `false` (safe
    /// default — don't rebalance during warm-up), same as elsewhere in
    /// the crate.
    pub fn rebalance_on<S>(mut self, signal: S) -> Self
    where
        S: Indicator<Input = Snapshot<Sym>, Output = bool> + 'static + Send + Sync,
    {
        self.rebalance = Box::new(signal);
        self
    }

    /// Wire the **score factory**: a closure that builds a fresh real-valued
    /// chain for one symbol. Called once per symbol the first time it
    /// appears in a snapshot. Every leaf in the returned chain is expected
    /// to root itself on the current symbol via
    /// [`Pick::matching(Selector::by_symbol(sym.clone()))`](crate::indicators::Pick::matching) —
    /// otherwise the same asset feeds every symbol's score, defeating the
    /// point of the ranker.
    pub fn scored_by<F, I>(mut self, factory: F) -> Self
    where
        F: Fn(&Sym) -> I + 'static + Send + Sync,
        I: Indicator<Input = Snapshot<Sym>, Output = Real> + 'static + Send + Sync,
    {
        self.score_factory = Box::new(move |sym: &Sym| {
            let ind: Chain<Sym> = Box::new(factory(sym));
            ind
        });
        self
    }

    /// Wire the **sizing factory** — the per-symbol
    /// [`ValueFraction`](crate::Size::ValueFraction) magnitude every
    /// selected leg is entered at. Same shape as
    /// [`scored_by`](Self::scored_by): the closure is invoked once per
    /// symbol on first sight. Defaults to a constant `0.0` so an
    /// unconfigured basket trades no notional; the crate never
    /// auto-normalizes, so a caller wanting 100% gross across an N-symbol
    /// basket calls
    /// [`sized_by(|_| equal_weight(N))`](crate::indicators::sizing::equal_weight).
    pub fn sized_by<F, I>(mut self, factory: F) -> Self
    where
        F: Fn(&Sym) -> I + 'static + Send + Sync,
        I: Indicator<Input = Snapshot<Sym>, Output = Real> + 'static + Send + Sync,
    {
        self.sizing_factory = Box::new(move |sym: &Sym| {
            let ind: Chain<Sym> = Box::new(factory(sym));
            ind
        });
        self
    }

    /// Take the top `longs` and bottom `shorts` symbols by score.
    /// Installs the [`TopBottom`] [`Selection`] impl.
    pub fn top_bottom(self, longs: usize, shorts: usize) -> Self {
        self.selection(TopBottom::new(longs, shorts))
    }

    /// Long every symbol scoring at/above `long_min`; short at/below
    /// `short_max`. Installs the [`Threshold`] [`Selection`] impl.
    pub fn threshold(self, long_min: Real, short_max: Real) -> Self {
        self.selection(Threshold::new(long_min, short_max))
    }

    /// Long the top `long_q` fraction, short the bottom `short_q` fraction
    /// of the score distribution. Installs the [`Quantile`] [`Selection`]
    /// impl.
    pub fn quantile(self, long_q: Real, short_q: Real) -> Self {
        self.selection(Quantile::new(long_q, short_q))
    }

    /// Install any [`Selection`] impl — the general seam behind the
    /// [`top_bottom`](Self::top_bottom) / [`threshold`](Self::threshold)
    /// / [`quantile`](Self::quantile) shortcuts, and the escape hatch
    /// for custom rules the built-ins don't cover (a signal gate on top
    /// of a rank, a stateful selector that reads
    /// [`book`](Self::book), a per-sector picker, a machine-learned
    /// classifier).
    ///
    /// Because the trait carries a blanket impl for any
    /// `Fn(&HashMap<Sym, Real>) -> HashMap<Sym, Side>` closure, you can
    /// still pass a closure directly for one-off logic:
    ///
    /// ```ignore
    /// basket.selection(|scores: &HashMap<Sym, Real>| { ... })
    /// ```
    ///
    /// A returned map's absent symbols are not selected (any open
    /// position on such a symbol gets flattened).
    pub fn selection<S>(mut self, s: S) -> Self
    where
        S: Selection<Sym> + 'static,
    {
        self.selection = Box::new(s);
        self
    }

    /// Restrict this basket to the exact set `symbols` under a **strict**
    /// contract: every listed symbol must appear on every bar (an absent
    /// symbol panics from [`update`](Strategy::update)), and
    /// [`is_ready`](Strategy::is_ready) stays `false` until every listed
    /// symbol has scored *and* sized `Some`. Non-listed symbols are
    /// filtered out at discovery — no chain is built for them.
    ///
    /// Use this when the universe list is authoritative and a missing
    /// symbol means the data feed is broken. If silent skipping is what
    /// you want, use [`any_of`](Self::any_of) instead.
    pub fn all_of<I>(self, symbols: I) -> Self
    where
        I: IntoIterator<Item = Sym>,
    {
        self.universe(AllOf(symbols.into_iter().collect()))
    }

    /// Restrict this basket to the set `symbols` under a **lax** contract:
    /// only listed symbols enter the basket, but absent or still-unready
    /// members are silently skipped — same per-bar filtering the floating
    /// universe does, just narrowed to a fixed list.
    pub fn any_of<I>(self, symbols: I) -> Self
    where
        I: IntoIterator<Item = Sym>,
    {
        self.universe(AnyOf(symbols.into_iter().collect()))
    }

    /// Install a custom [`Universe`] impl. The
    /// [`all_of`](Self::all_of) / [`any_of`](Self::any_of) shortcuts
    /// wrap the built-in impls, but a caller with a bespoke scoping
    /// rule (a sector filter, a dynamic membership indicator, an
    /// intersection of two lists) can construct their own
    /// `Box<dyn Universe<Sym>>` and hand it in here — nothing else in
    /// the strategy needs to change.
    pub fn universe<U>(mut self, universe: U) -> Self
    where
        U: Universe<Sym> + 'static,
    {
        self.universe = Box::new(universe);
        self
    }

    /// A clone of the [`Position`] tracker for `symbol`, if it has been
    /// seen. Available for building per-symbol protective levels off the
    /// tracked entry / peak / trough (not wired into `trade` in this
    /// pass — protective stops on a basket are a follow-up).
    pub fn position(&self, symbol: &Sym) -> Option<Position> {
        self.positions.get(symbol).cloned()
    }

    /// A clone of the shared [`Book`], for composing book-anchored sizing
    /// against the basket's aggregate equity curve.
    pub fn book(&self) -> Book<Sym> {
        self.book.clone()
    }

    /// The largest `stable_bars()` across every currently-built score /
    /// sizing chain and the rebalance gate — the number of bars the driver
    /// waits before treating the strategy as ready.
    ///
    /// **Lazy readiness contract.** A basket's per-symbol score / sizing
    /// chains are built on first sight (see
    /// [`update`](Strategy::update)) — a freshly-constructed strategy that
    /// hasn't seen any snapshot yet has no chains, and this method reports
    /// `0` (only the rebalance signal contributes). To probe grid-wide
    /// readiness (for `optimize --walkforward`'s prefix skip, or any
    /// caller that wants the "worst case across every symbol" number),
    /// feed the strategy one representative snapshot with
    /// [`update`](Strategy::update) first so the chains exist, then read
    /// `stable_bars()`.
    pub fn stable_bars(&self) -> usize {
        let mut n = self.rebalance.stable_bars();
        for score in self.scores.values() {
            n = n.max(score.stable_bars());
        }
        for size in self.sizes.values() {
            n = n.max(size.stable_bars());
        }
        for level in self
            .long_stops
            .values()
            .chain(self.long_targets.values())
            .chain(self.short_stops.values())
            .chain(self.short_targets.values())
        {
            n = n.max(level.stable_bars());
        }
        n
    }

    /// The warm-up-only twin of [`stable_bars`](Self::stable_bars) —
    /// ignores IIR unstable settling. Used by
    /// `optimize --walkforward --keep-unstable`.
    ///
    /// Same lazy-readiness caveat: feed one snapshot before probing so
    /// per-symbol chains exist.
    pub fn warm_up_bars(&self) -> usize {
        let mut n = self.rebalance.warm_up_bars();
        for score in self.scores.values() {
            n = n.max(score.warm_up_bars());
        }
        for size in self.sizes.values() {
            n = n.max(size.warm_up_bars());
        }
        for level in self
            .long_stops
            .values()
            .chain(self.long_targets.values())
            .chain(self.short_stops.values())
            .chain(self.short_targets.values())
        {
            n = n.max(level.warm_up_bars());
        }
        n
    }
}

impl<Sym: Clone + PartialEq + Hash + Eq + 'static + Send + Sync> Default for BasketStrategy<Sym> {
    fn default() -> Self {
        Self::new()
    }
}

// Consumed only by the `spec`-gated `DynBasketStrategy` wrapper.
#[cfg_attr(not(feature = "spec"), allow(dead_code))]
impl<Sym> BasketStrategy<Sym>
where
    Sym: Clone + Hash + Eq + 'static + Send + Sync + serde::Serialize + serde::de::DeserializeOwned,
{
    /// Serialize the basket's runtime state for run resuming — the shared
    /// [`Book`] and rebalance gate, plus each known symbol's score / sizing /
    /// protective chains and its [`Position`]. Symbols never seen (and never
    /// restored) carry no state: they build fresh on first sight.
    pub(crate) fn save_state(&self) -> serde_json::Value {
        let mut symbols: HashMap<Sym, serde_json::Value> = HashMap::new();
        for sym in self.scores.keys() {
            let mut entry = serde_json::Map::new();
            if let Some(c) = self.scores.get(sym) {
                entry.insert("score".into(), c.save_state());
            }
            if let Some(c) = self.sizes.get(sym) {
                entry.insert("size".into(), c.save_state());
            }
            if let Some(p) = self.positions.get(sym) {
                entry.insert("position".into(), p.snapshot());
            }
            for (map, key) in [
                (&self.long_stops, "long_stop"),
                (&self.long_targets, "long_target"),
                (&self.short_stops, "short_stop"),
                (&self.short_targets, "short_target"),
            ] {
                if let Some(c) = map.get(sym) {
                    entry.insert(key.into(), c.save_state());
                }
            }
            symbols.insert(sym.clone(), serde_json::Value::Object(entry));
        }
        serde_json::json!({
            "book": self.book.snapshot_state(),
            // The gate is state, not config: `Every` carries a bar counter, so
            // a resumed run that restarts it rebalances on different bars than
            // an uninterrupted one.
            "rebalance": self.rebalance.save_state(),
            "symbols": serde_json::to_value(&symbols).unwrap_or(serde_json::Value::Null),
        })
    }

    /// Restore state produced by [`save_state`](Self::save_state).
    ///
    /// **Eagerly**: every symbol in the blob has its chains built and loaded
    /// here rather than on its next sighting. See
    /// [`MultiAssetStrategy::restore_state`](crate::strategies::MultiAssetStrategy)
    /// for the three failures a deferred restore causes — chief among them that
    /// `backtest::run` routes fills through [`on_fill`](Strategy::on_fill)
    /// *before* `update`, so a resumed run's first-bar fill would land on the
    /// shared [`Book`] with no [`Position`] built to receive it.
    pub(crate) fn restore_state(&mut self, state: &serde_json::Value) -> Result<(), String> {
        let obj = state
            .as_object()
            .ok_or_else(|| format!("basket: expected a state object, got {state}"))?;
        if let Some(v) = obj.get("book") {
            self.book.restore_state(v).map_err(|e| format!("book > {e}"))?;
        }
        if let Some(v) = obj.get("rebalance") {
            self.rebalance
                .load_state(v)
                .map_err(|e| format!("rebalance > {e}"))?;
        }
        if let Some(v) = obj.get("symbols") {
            let saved: HashMap<Sym, serde_json::Value> =
                serde_json::from_value(v.clone()).map_err(|e| format!("symbols: {e}"))?;
            for (sym, entry) in saved {
                // A universe narrowed since the state was written drops the
                // symbol the same way a live discovery would.
                if !self.universe.admits(&sym) {
                    continue;
                }
                self.discover(sym.clone());
                self.restore_symbol(&sym, &entry)?;
            }
        }
        Ok(())
    }

    /// Load one symbol's saved entry into its already-built chains. Every field
    /// propagates with a `symbols[sym] > ` breadcrumb: a shape mismatch means
    /// the document and the blob disagree, which is bad input to report, not
    /// something to silently resume through.
    fn restore_symbol(&mut self, sym: &Sym, entry: &serde_json::Value) -> Result<(), String> {
        let label = serde_json::to_string(sym).unwrap_or_else(|_| "?".into());
        let obj = entry
            .as_object()
            .ok_or_else(|| format!("symbols[{label}]: expected an object, got {entry}"))?;
        let at = |field: &str, e: String| format!("symbols[{label}] > {field} > {e}");

        for (map, key) in [
            (&mut self.scores, "score"),
            (&mut self.sizes, "size"),
            (&mut self.long_stops, "long_stop"),
            (&mut self.long_targets, "long_target"),
            (&mut self.short_stops, "short_stop"),
            (&mut self.short_targets, "short_target"),
        ] {
            if let (Some(c), Some(v)) = (map.get_mut(sym), obj.get(key)) {
                c.load_state(v).map_err(|e| at(key, e))?;
            }
        }
        if let (Some(p), Some(v)) = (self.positions.get(sym), obj.get("position")) {
            p.restore(v).map_err(|e| at("position", e))?;
        }
        Ok(())
    }
}

impl<Sym: Clone + Hash + Eq + 'static + Send + Sync> BasketStrategy<Sym> {
    /// Spin up one symbol's chains from the per-symbol factories and register
    /// it across the seven maps. Idempotent — a symbol already known is left
    /// alone, so a restore followed by a live sighting doesn't rebuild it.
    ///
    /// The single place a basket leg is born: reached from
    /// [`update`](Strategy::update) on first sight and from
    /// [`restore_state`](Self::restore_state) for every symbol a resumed blob
    /// carries. Protective factories take the brand-new [`Position`] so
    /// `position.entry()` / `.peak()` / `.trough()` anchor correctly.
    fn discover(&mut self, sym: Sym) {
        if self.scores.contains_key(&sym) {
            return;
        }
        let position = Position::new();
        if let Some(f) = &self.long_stop_factory {
            self.long_stops.insert(sym.clone(), f(&sym, &position));
        }
        if let Some(f) = &self.long_target_factory {
            self.long_targets.insert(sym.clone(), f(&sym, &position));
        }
        if let Some(f) = &self.short_stop_factory {
            self.short_stops.insert(sym.clone(), f(&sym, &position));
        }
        if let Some(f) = &self.short_target_factory {
            self.short_targets.insert(sym.clone(), f(&sym, &position));
        }
        self.scores.insert(sym.clone(), (self.score_factory)(&sym));
        self.sizes.insert(sym.clone(), (self.sizing_factory)(&sym));
        self.positions.insert(sym, position);
    }
}

impl<Sym: Clone + PartialEq + Hash + Eq + 'static + Send + Sync> Strategy for BasketStrategy<Sym> {
    type Input = Snapshot<Sym>;
    type Symbol = Sym;

    fn update(&mut self, snap: Snapshot<Sym>) {
        // 0. Universe: strict impls (e.g. `AllOf`) require every listed
        // symbol on every bar. Absence is a hard panic — the point of a
        // strict universe is to catch feed gaps and typos at the first
        // bar, not silently trade a smaller basket. Lax and floating
        // impls return an empty slice here and the loop is a no-op.
        for sym in self.universe.required() {
            let present = snap.iter().any(|(s, _, _)| s == Some(sym));
            if !present {
                panic!(
                    "BasketStrategy: the installed strict universe requires \
                     every listed symbol to be present in every snapshot, \
                     but at least one is missing this bar. Either fix the \
                     data feed or install a lax universe (`any_of` / \
                     `Floating`) if silent skipping is what you want."
                );
            }
        }

        // 1. Discover symbols on first sight and spin up their chains,
        // filtered by the declared universe (floating admits all). We
        // collect the new symbols first so the borrow of `snap` ends before
        // we mutate `self`.
        let new_syms: Vec<Sym> = snap
            .iter()
            .filter_map(|(sym_opt, _freq, _atom)| {
                sym_opt
                    .filter(|s| self.universe.admits(s))
                    .filter(|s| !self.scores.contains_key(s))
                    .cloned()
            })
            .collect();
        for sym in new_syms {
            self.discover(sym);
        }

        // 2. Advance every known chain against the whole snapshot; the
        // internal Pick per symbol filters to its own atom. A None reading
        // rolls the symbol off the latest_* maps so it's not considered
        // for selection this bar.
        for (sym, chain) in self.scores.iter_mut() {
            match chain.update(snap.clone()) {
                Some(v) => {
                    self.latest_score.insert(sym.clone(), v);
                }
                None => {
                    self.latest_score.remove(sym);
                }
            }
        }
        for (sym, chain) in self.sizes.iter_mut() {
            match chain.update(snap.clone()) {
                Some(v) => {
                    self.latest_size.insert(sym.clone(), v);
                }
                None => {
                    self.latest_size.remove(sym);
                }
            }
        }
        // Advance any protective-level chains against the same snapshot.
        // We drop their return values on the floor here; `trade` reads
        // `.value()` when it needs to re-submit the resting order.
        for chain in self.long_stops.values_mut() {
            let _ = chain.update(snap.clone());
        }
        for chain in self.long_targets.values_mut() {
            let _ = chain.update(snap.clone());
        }
        for chain in self.short_stops.values_mut() {
            let _ = chain.update(snap.clone());
        }
        for chain in self.short_targets.values_mut() {
            let _ = chain.update(snap.clone());
        }

        // 3. Fold the in-progress bar into each held symbol's Position.
        // 4. Mark the Book to market with per-leg closes, all in one pass.
        let mut marks: Vec<(Sym, Candle)> = Vec::new();
        for (sym_opt, _freq, atom) in snap.iter() {
            if let Some(sym) = sym_opt {
                // An overlay-only series stacked into the snapshot carries no
                // price, so it neither advances a position nor marks a leg.
                let Some(candle) = atom.candle else { continue };
                if let Some(pos) = self.positions.get(sym) {
                    pos.update(candle);
                }
                marks.push((sym.clone(), candle));
            }
        }
        if !marks.is_empty() {
            self.book.update(marks);
        }

        // 5. Advance the rebalance gate — a signal over the whole
        // snapshot. Reads on the same bar as scoring, but only consulted
        // in `trade()`.
        self.rebalance.update(snap);
    }

    fn on_fill(&mut self, order: &Order<Sym>) {
        if let Some(pos) = self.positions.get(&order.symbol) {
            pos.apply(order.side, order.units, order.price);
        }
        self.book
            .apply_fill(&order.symbol, order.side, order.units, order.price);
    }

    fn trade(&self, wallet: &mut dyn Wallet<Sym>) {
        // Rebalance gate: skip the whole selection + resize step on bars
        // where the gate signal doesn't fire (None reads as false — the
        // "unsettled data ⇒ wait" convention). Default gate is
        // `Every::new(1)` so this is a no-op unless the caller wired a
        // less-frequent cadence.
        if !self.rebalance.value().unwrap_or(false) {
            return;
        }
        let selection = self.selection.pick(&self.latest_score);

        // Dollar-neutrality: scale per-side sizes so Σ long_sizes ==
        // Σ short_sizes. The smaller side's sum becomes the target
        // gross-per-side (never levers up). A one-sided selection skips
        // the whole rebalance — running only one leg would break the
        // hedge intent.
        let (long_scale, short_scale) = if self.dollar_neutral {
            let long_sum: Real = selection
                .iter()
                .filter(|(_, s)| **s == Side::Buy)
                .map(|(sym, _)| self.latest_size.get(sym).copied().unwrap_or(0.0))
                .sum();
            let short_sum: Real = selection
                .iter()
                .filter(|(_, s)| **s == Side::Sell)
                .map(|(sym, _)| self.latest_size.get(sym).copied().unwrap_or(0.0))
                .sum();
            if long_sum <= 0.0 || short_sum <= 0.0 {
                return;
            }
            let target = long_sum.min(short_sum);
            (target / long_sum, target / short_sum)
        } else {
            (1.0, 1.0)
        };

        for sym in self.scores.keys() {
            let position = self.positions.get(sym);
            match selection.get(sym) {
                Some(Side::Buy) => {
                    // Sizing must be available to open a leg; skip this
                    // symbol otherwise (safe default per the crate's
                    // "unsettled data ⇒ wait" convention).
                    let Some(&size) = self.latest_size.get(sym) else {
                        continue;
                    };
                    let scaled = size * long_scale;
                    let is_long = position.map(|p| p.is_long()).unwrap_or(false);
                    if !is_long {
                        let _ =
                            wallet.set(sym.clone(), Side::Buy, Size::value_frac(scaled));
                        let _ = wallet.cancel_protective(sym);
                    }
                    // Re-submit long-side resting orders every fire —
                    // idempotent (`set_stop` / `set_take_profit` are
                    // latest-wins on `PaperWallet`) so trailing levels
                    // that move with the bar update naturally.
                    if let Some(level) = self.long_stops.get(sym).and_then(|c| c.value()) {
                        let _ = wallet.set_stop(sym.clone(), Reference(level), Size::position_frac(1.0));
                    }
                    if let Some(level) = self.long_targets.get(sym).and_then(|c| c.value()) {
                        let _ = wallet.set_take_profit(sym.clone(), Reference(level), Size::position_frac(1.0));
                    }
                }
                Some(Side::Sell) => {
                    let Some(&size) = self.latest_size.get(sym) else {
                        continue;
                    };
                    let scaled = size * short_scale;
                    let is_short = position.map(|p| p.is_short()).unwrap_or(false);
                    if !is_short {
                        let _ =
                            wallet.set(sym.clone(), Side::Sell, Size::value_frac(scaled));
                        let _ = wallet.cancel_protective(sym);
                    }
                    if let Some(level) = self.short_stops.get(sym).and_then(|c| c.value()) {
                        let _ = wallet.set_stop(sym.clone(), Reference(level), Size::position_frac(1.0));
                    }
                    if let Some(level) = self.short_targets.get(sym).and_then(|c| c.value()) {
                        let _ = wallet.set_take_profit(sym.clone(), Reference(level), Size::position_frac(1.0));
                    }
                }
                None => {
                    let is_open = position.map(|p| !p.is_flat()).unwrap_or(false);
                    if is_open {
                        let _ = wallet.close(sym.clone());
                        let _ = wallet.cancel_protective(sym);
                    }
                }
            }
        }
    }

    fn is_ready(&self) -> bool {
        // Lax / floating universes report no required symbols → this
        // loop is a no-op → always ready to *try*. Per-symbol readiness
        // is enforced inside `trade` by only considering symbols that
        // scored `Some` this bar.
        //
        // Strict universes report every listed symbol as required; we
        // gate on each one having both scored and sized this bar, so
        // the basket sits through warm-up rather than picking from a
        // partial universe.
        self.universe
            .required()
            .iter()
            .all(|s| self.latest_score.contains_key(s) && self.latest_size.contains_key(s))
    }

    fn reset(&mut self) {
        self.scores.clear();
        self.sizes.clear();
        self.long_stops.clear();
        self.long_targets.clear();
        self.short_stops.clear();
        self.short_targets.clear();
        self.positions.clear();
        self.latest_score.clear();
        self.latest_size.clear();
        self.rebalance.reset();
        self.book.reset();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::indicators::sizing::equal_weight;
    use crate::indicators::{Close, Pick};
    use crate::wallet::PaperWallet;
    use crate::types::{Atom, Selector};

    /// Build a snapshot from per-symbol closes. Insertion order is the
    /// caller's argument order.
    fn snap(entries: &[(&'static str, Real)]) -> Snapshot<&'static str> {
        let mut s = Snapshot::new();
        for &(sym, close) in entries {
            let atom = Atom::new(Candle::new(close, close, close, close, 0.0));
            s.push(Some(sym), None, atom);
        }
        s
    }
    // ---------------- BasketStrategy lifecycle ---------------------------

    // (No shared "score = close" helper: passing a free `fn` returning
    // `impl Indicator + 'static` into `scored_by` runs into higher-ranked
    // lifetime unification. Each test that needs it wires the closure
    // inline — a couple more lines per test, no lifetime headache.)

    #[test]
    fn lazy_instantiation_on_first_sight() {
        let mut strat: BasketStrategy<&'static str> =
            BasketStrategy::with_initial_equity(1_000.0)
                .scored_by(|sym: &&'static str| {
                    Close::of(Pick::matching(Selector::by_symbol(*sym)))
                })
                .sized_by(|_| equal_weight::<&'static str>(2))
                .top_bottom(1, 1);
        // Before any bar: no symbols known.
        assert!(strat.position(&"A").is_none());
        // First bar with A, B → chains built lazily.
        strat.update(snap(&[("A", 100.0), ("B", 50.0)]));
        assert!(strat.position(&"A").is_some());
        assert!(strat.position(&"B").is_some());
        // A new symbol appearing later: also lazily built.
        strat.update(snap(&[("A", 101.0), ("B", 51.0), ("C", 200.0)]));
        assert!(strat.position(&"C").is_some());
    }

    #[test]
    fn top_bottom_drives_wallet_into_long_and_short() {
        // 3-symbol basket, top-1 long, bottom-1 short. Scores = the raw
        // close, so the highest-priced symbol goes long and the lowest
        // goes short. Sizing: equal-weight over 2 legs (50% each = 100%
        // gross).
        let mut strat: BasketStrategy<&'static str> =
            BasketStrategy::with_initial_equity(1_000.0)
                .scored_by(|sym: &&'static str| {
                    Close::of(Pick::matching(Selector::by_symbol(*sym)))
                })
                .sized_by(|_| equal_weight::<&'static str>(2))
                .top_bottom(1, 1);
        let mut wallet: PaperWallet<&'static str> = PaperWallet::new(1_000.0);

        // Bar 1: prime the wallet + strategy.
        let bar1 = snap(&[("A", 100.0), ("B", 50.0), ("C", 25.0)]);
        for (sym_opt, _f, atom) in bar1.iter() {
            let sym = sym_opt.copied().unwrap();
            let Some(candle) = atom.candle else { continue };
            for fill in wallet.update(sym, candle) {
                strat.on_fill(&fill);
            }
        }
        strat.update(bar1);
        strat.trade(&mut wallet);
        // Only market queues here — no fills yet.
        assert!(wallet.orders().is_empty());

        // Bar 2: same prices; queued orders now fill at each symbol's open.
        let bar2 = snap(&[("A", 100.0), ("B", 50.0), ("C", 25.0)]);
        for (sym_opt, _f, atom) in bar2.iter() {
            let sym = sym_opt.copied().unwrap();
            let Some(candle) = atom.candle else { continue };
            for fill in wallet.update(sym, candle) {
                strat.on_fill(&fill);
            }
        }
        strat.update(bar2);
        strat.trade(&mut wallet);
        // A should be long (top score), C short (bottom score), B flat.
        assert!(wallet.position(&"A").amount > 0.0, "A long");
        assert!(wallet.position(&"C").amount < 0.0, "C short");
        assert!(
            wallet.position(&"B").amount.abs() < 1e-9,
            "B flat, got {}",
            wallet.position(&"B").amount
        );
    }

    #[test]
    fn selection_change_rebalances() {
        // Same setup as above, but on bar 3 the scores flip: A now scores
        // lowest, C highest. The basket should close A, open C long, and
        // reverse the short from C into A.
        let mut strat: BasketStrategy<&'static str> =
            BasketStrategy::with_initial_equity(10_000.0)
                .scored_by(|sym: &&'static str| {
                    Close::of(Pick::matching(Selector::by_symbol(*sym)))
                })
                .sized_by(|_| equal_weight::<&'static str>(2))
                .top_bottom(1, 1);
        let mut wallet: PaperWallet<&'static str> = PaperWallet::new(10_000.0);

        // Helper: mark, deliver fills, update, trade. `close_by_symbol` is a
        // per-symbol close override so we can shift ranks bar-to-bar.
        let tick = |strat: &mut BasketStrategy<&'static str>,
                    wallet: &mut PaperWallet<&'static str>,
                    symbols: &[(&'static str, Real)]| {
            let s = snap(symbols);
            for (sym_opt, _f, atom) in s.iter() {
                let sym = sym_opt.copied().unwrap();
                let Some(candle) = atom.candle else { continue };
                for fill in wallet.update(sym, candle) {
                    strat.on_fill(&fill);
                }
            }
            strat.update(s);
            strat.trade(wallet);
        };

        // Bar 1: prime; Bar 2: fill first selection (A long, C short).
        // Prices deliberately close so the flip stays within the paper
        // wallet's cash — a bigger move would leave the short leg's mark
        // to market > equity, and the queued reversal would fail with
        // `InsufficientFunds` (silently, per the strategy's `let _`).
        tick(&mut strat, &mut wallet, &[("A", 100.0), ("B", 90.0), ("C", 80.0)]);
        tick(&mut strat, &mut wallet, &[("A", 100.0), ("B", 90.0), ("C", 80.0)]);
        assert!(wallet.position(&"A").amount > 0.0, "A long after first fill");
        assert!(wallet.position(&"C").amount < 0.0, "C short after first fill");

        // Bar 3: flip scores — A drops to 80, C climbs to 100. New pick:
        // C long, A short. Queues open on this bar.
        tick(&mut strat, &mut wallet, &[("A", 80.0), ("B", 90.0), ("C", 100.0)]);
        // Bar 4: queued rebalance fills at the open.
        tick(&mut strat, &mut wallet, &[("A", 80.0), ("B", 90.0), ("C", 100.0)]);
        assert!(wallet.position(&"C").amount > 0.0, "C long after flip");
        assert!(wallet.position(&"A").amount < 0.0, "A short after flip");
    }

    #[test]
    fn no_trade_while_score_reads_none() {
        // Use an Sma-5 as the scoring source. For the first 4 bars, every
        // symbol's score is None — the basket must select nothing.
        let mut strat: BasketStrategy<&'static str> =
            BasketStrategy::with_initial_equity(1_000.0)
                .scored_by(|sym: &&'static str| {
                    crate::indicators::Sma::new(
                        Close::of(Pick::matching(Selector::by_symbol(*sym))),
                        5,
                    )
                })
                .sized_by(|_| equal_weight::<&'static str>(2))
                .top_bottom(1, 1);
        let mut wallet: PaperWallet<&'static str> = PaperWallet::new(1_000.0);

        for _ in 0..4 {
            let s = snap(&[("A", 100.0), ("B", 50.0)]);
            for (sym_opt, _f, atom) in s.iter() {
                let sym = sym_opt.copied().unwrap();
                let Some(candle) = atom.candle else { continue };
                for fill in wallet.update(sym, candle) {
                    strat.on_fill(&fill);
                }
            }
            strat.update(s);
            strat.trade(&mut wallet);
        }
        // 4 bars fed; SMA-5 hasn't warmed, so no queued entry has resolved,
        // and none should even have been queued.
        assert!(wallet.orders().is_empty(), "expected zero fills during warm-up");
    }

    #[test]
    fn missing_symbol_causes_close() {
        // Establish a top-1/bottom-1 selection on A and B; then B stops
        // appearing. B's score chain returns None on its own symbol via
        // Pick, so B rolls off the ranking and the strategy closes it.
        let mut strat: BasketStrategy<&'static str> =
            BasketStrategy::with_initial_equity(1_000.0)
                .scored_by(|sym: &&'static str| {
                    Close::of(Pick::matching(Selector::by_symbol(*sym)))
                })
                .sized_by(|_| equal_weight::<&'static str>(2))
                .top_bottom(1, 1);
        let mut wallet: PaperWallet<&'static str> = PaperWallet::new(1_000.0);
        let tick = |strat: &mut BasketStrategy<&'static str>,
                    wallet: &mut PaperWallet<&'static str>,
                    entries: &[(&'static str, Real)]| {
            let s = snap(entries);
            for (sym_opt, _f, atom) in s.iter() {
                let sym = sym_opt.copied().unwrap();
                let Some(candle) = atom.candle else { continue };
                for fill in wallet.update(sym, candle) {
                    strat.on_fill(&fill);
                }
            }
            strat.update(s);
            strat.trade(wallet);
        };
        tick(&mut strat, &mut wallet, &[("A", 100.0), ("B", 50.0)]);
        tick(&mut strat, &mut wallet, &[("A", 100.0), ("B", 50.0)]);
        assert!(wallet.position(&"A").amount > 0.0);
        assert!(wallet.position(&"B").amount < 0.0);
        // Bar 3: B disappears. Its score rolls off; A is still ranked
        // (top-1 in a 1-symbol pool). Queue a close on B.
        tick(&mut strat, &mut wallet, &[("A", 100.0)]);
        // Bar 4: B is still absent — the queued close on B doesn't fill
        // because the wallet hasn't seen a new bar for B; re-queue and
        // eventually a B bar prices the exit. Feed one final B bar so the
        // close can fill.
        let bar4 = snap(&[("A", 100.0), ("B", 50.0)]);
        for (sym_opt, _f, atom) in bar4.iter() {
            let sym = sym_opt.copied().unwrap();
            let Some(candle) = atom.candle else { continue };
            for fill in wallet.update(sym, candle) {
                strat.on_fill(&fill);
            }
        }
        // We do NOT run strat.update / trade on bar 4 — the point is that a
        // close queued on bar 3 fills when B's next bar arrives.
        assert!(
            wallet.position(&"B").amount.abs() < 1e-9,
            "B should be flat after the queued close fills, got {}",
            wallet.position(&"B").amount
        );
    }

    #[test]
    fn book_tracks_aggregate_equity() {
        // 2-leg basket, top-1 long + bottom-1 short. After the entry fills,
        // move both legs to book a small P&L and confirm Book equity
        // reflects it.
        let mut strat: BasketStrategy<&'static str> =
            BasketStrategy::with_initial_equity(10_000.0)
                .scored_by(|sym: &&'static str| {
                    Close::of(Pick::matching(Selector::by_symbol(*sym)))
                })
                .sized_by(|_| equal_weight::<&'static str>(2))
                .top_bottom(1, 1);
        let book = strat.book();
        let mut wallet: PaperWallet<&'static str> = PaperWallet::new(10_000.0);
        let tick = |strat: &mut BasketStrategy<&'static str>,
                    wallet: &mut PaperWallet<&'static str>,
                    entries: &[(&'static str, Real)]| {
            let s = snap(entries);
            for (sym_opt, _f, atom) in s.iter() {
                let sym = sym_opt.copied().unwrap();
                let Some(candle) = atom.candle else { continue };
                for fill in wallet.update(sym, candle) {
                    strat.on_fill(&fill);
                }
            }
            strat.update(s);
            strat.trade(wallet);
        };
        // Bar 1: prime. Bar 2: fill. Sizing 0.5, seed 10_000 → 5000/price
        // units per leg. A @100 → 50 long units; B @50 → 100 short units.
        tick(&mut strat, &mut wallet, &[("A", 100.0), ("B", 50.0)]);
        tick(&mut strat, &mut wallet, &[("A", 100.0), ("B", 50.0)]);
        // After the fill bar, book equity = 10_000 (dollar-neutral fills
        // don't move cash or MTM valuation on the same-close bar).
        assert!(
            (book.equity_value() - 10_000.0).abs() < 1e-6,
            "book equity {}",
            book.equity_value()
        );
        // Bar 3: A rises to 110, B holds at 50. P&L: +50 * (110-100) = +500.
        tick(&mut strat, &mut wallet, &[("A", 110.0), ("B", 50.0)]);
        assert!(
            (book.equity_value() - 10_500.0).abs() < 1e-6,
            "book equity after gain: {}",
            book.equity_value()
        );
    }

    #[test]
    fn reset_clears_everything() {
        let mut strat: BasketStrategy<&'static str> =
            BasketStrategy::with_initial_equity(1_000.0)
                .scored_by(|sym: &&'static str| {
                    Close::of(Pick::matching(Selector::by_symbol(*sym)))
                })
                .sized_by(|_| equal_weight::<&'static str>(2));
        strat.update(snap(&[("A", 100.0), ("B", 50.0)]));
        assert!(strat.position(&"A").is_some());
        strat.reset();
        assert!(strat.position(&"A").is_none());
        assert_eq!(strat.book().equity_value(), 1_000.0);
    }

    // Cross-check the doctested constructor path shape here too, so a
    // Roc-based factory compiles in the test binary without needing the
    // doc example to expand.
    #[test]
    fn roc_scored_basket_compiles() {
        use crate::indicators::Roc;
        let _strat: BasketStrategy<String> =
            BasketStrategy::with_initial_equity(1_000.0)
                .scored_by(|sym: &String| {
                    Roc::new(
                        Close::of(Pick::matching(Selector::by_symbol(sym.clone()))),
                        5,
                    )
                })
                .sized_by(|_sym: &String| equal_weight::<String>(4))
                .top_bottom(2, 2);
    }
    // ---------------- Rebalance gate ------------------------------------

    #[test]
    fn default_rebalance_fires_every_bar() {
        // No `.rebalance_on(...)` set — default `Every::new(1)` gate
        // rebalances on every bar (matches the pre-`rebalance_on`
        // behavior). A top-1 long / bottom-1 short basket enters on bar 2.
        let mut strat: BasketStrategy<&'static str> =
            BasketStrategy::with_initial_equity(10_000.0)
                .scored_by(|sym: &&'static str| {
                    Close::of(Pick::matching(Selector::by_symbol(*sym)))
                })
                .sized_by(|_| equal_weight::<&'static str>(2))
                .top_bottom(1, 1);
        let mut wallet: PaperWallet<&'static str> = PaperWallet::new(10_000.0);
        let tick = |strat: &mut BasketStrategy<&'static str>,
                    wallet: &mut PaperWallet<&'static str>,
                    entries: &[(&'static str, Real)]| {
            let s = snap(entries);
            for (sym_opt, _f, atom) in s.iter() {
                let sym = sym_opt.copied().unwrap();
                let Some(candle) = atom.candle else { continue };
                for fill in wallet.update(sym, candle) {
                    strat.on_fill(&fill);
                }
            }
            strat.update(s);
            strat.trade(wallet);
        };
        tick(&mut strat, &mut wallet, &[("A", 100.0), ("B", 50.0)]);
        tick(&mut strat, &mut wallet, &[("A", 100.0), ("B", 50.0)]);
        assert!(wallet.position(&"A").amount > 0.0);
        assert!(wallet.position(&"B").amount < 0.0);
    }

    #[test]
    fn rebalance_every_3_only_re_ranks_periodically() {
        // Score = close. Every 3 bars, top-1 long / bottom-1 short.
        // On bar 3 (the first fire of `Every::new(3)`), a queued order
        // enters positions. Between rebalance bars the basket should NOT
        // issue new orders even if the ranking changed.
        use crate::indicators::Every;
        let mut strat: BasketStrategy<&'static str> =
            BasketStrategy::with_initial_equity(10_000.0)
                .scored_by(|sym: &&'static str| {
                    Close::of(Pick::matching(Selector::by_symbol(*sym)))
                })
                .sized_by(|_| equal_weight::<&'static str>(2))
                .top_bottom(1, 1)
                .rebalance_on(Every::<Snapshot<&'static str>>::new(3));
        let mut wallet: PaperWallet<&'static str> = PaperWallet::new(10_000.0);
        let tick = |strat: &mut BasketStrategy<&'static str>,
                    wallet: &mut PaperWallet<&'static str>,
                    entries: &[(&'static str, Real)]| {
            let s = snap(entries);
            for (sym_opt, _f, atom) in s.iter() {
                let sym = sym_opt.copied().unwrap();
                let Some(candle) = atom.candle else { continue };
                for fill in wallet.update(sym, candle) {
                    strat.on_fill(&fill);
                }
            }
            strat.update(s);
            strat.trade(wallet);
        };
        // Bars 1 and 2: gate is false, no orders queued.
        tick(&mut strat, &mut wallet, &[("A", 100.0), ("B", 50.0)]);
        assert!(wallet.orders().is_empty(), "bar 1: no rebalance");
        tick(&mut strat, &mut wallet, &[("A", 100.0), ("B", 50.0)]);
        assert!(wallet.orders().is_empty(), "bar 2: no rebalance");
        // Bar 3: gate fires — selection runs, orders queued.
        tick(&mut strat, &mut wallet, &[("A", 100.0), ("B", 50.0)]);
        // Bar 4: fills at open.
        tick(&mut strat, &mut wallet, &[("A", 100.0), ("B", 50.0)]);
        assert!(wallet.position(&"A").amount > 0.0, "A long after first rebalance");
        assert!(wallet.position(&"B").amount < 0.0, "B short after first rebalance");
        let n_after_first = wallet.orders().len();
        // Bar 5: ranking flip — A drops to 40, B rises to 100. Under
        // `rebalance_on: !every 3`, the basket must NOT re-rank on this
        // off-cycle bar. Positions should hold.
        tick(&mut strat, &mut wallet, &[("A", 40.0), ("B", 100.0)]);
        assert_eq!(
            wallet.orders().len(),
            n_after_first,
            "bar 5 is off-cycle: no new orders"
        );
        assert!(wallet.position(&"A").amount > 0.0, "A stays long between rebalances");
        assert!(wallet.position(&"B").amount < 0.0, "B stays short between rebalances");
    }

    #[test]
    fn rebalance_on_never_freezes_the_basket() {
        // With `rebalance_on(ValueBool::new(false))`, the basket never runs
        // selection. No orders at all.
        use crate::indicators::ValueBool;
        let mut strat: BasketStrategy<&'static str> =
            BasketStrategy::with_initial_equity(10_000.0)
                .scored_by(|sym: &&'static str| {
                    Close::of(Pick::matching(Selector::by_symbol(*sym)))
                })
                .sized_by(|_| equal_weight::<&'static str>(2))
                .top_bottom(1, 1)
                .rebalance_on(ValueBool::<Snapshot<&'static str>>::new(false));
        let mut wallet: PaperWallet<&'static str> = PaperWallet::new(10_000.0);
        for _ in 0..5 {
            let s = snap(&[("A", 100.0), ("B", 50.0)]);
            for (sym_opt, _f, atom) in s.iter() {
                let sym = sym_opt.copied().unwrap();
                let Some(candle) = atom.candle else { continue };
                for fill in wallet.update(sym, candle) {
                    strat.on_fill(&fill);
                }
            }
            strat.update(s);
            strat.trade(&mut wallet);
        }
        assert!(wallet.orders().is_empty(), "never-rebalance basket must not trade");
    }

    // ---------------- Dollar-neutral ------------------------------------

    #[test]
    fn dollar_neutral_rescales_sides_to_min_gross() {
        // Three symbols. Uniform sizing 0.5 per leg. Top-2 long / bottom-1
        // short: long side sums to 1.0, short side sums to 0.5. Dollar-
        // neutral should downscale longs from 0.5 each to 0.25 each (so
        // Σ longs == Σ shorts == 0.5).
        //
        // Verify via wallet notional (units × price) after fills land.
        let mut strat: BasketStrategy<&'static str> =
            BasketStrategy::with_initial_equity(10_000.0)
                .scored_by(|sym: &&'static str| {
                    Close::of(Pick::matching(Selector::by_symbol(*sym)))
                })
                .sized_by(|_| crate::indicators::Value::<Snapshot<&'static str>>::new(0.5))
                .top_bottom(2, 1)
                .dollar_neutral();
        let mut wallet: PaperWallet<&'static str> = PaperWallet::new(10_000.0);
        let tick = |strat: &mut BasketStrategy<&'static str>,
                    wallet: &mut PaperWallet<&'static str>,
                    entries: &[(&'static str, Real)]| {
            let s = snap(entries);
            for (sym_opt, _f, atom) in s.iter() {
                let sym = sym_opt.copied().unwrap();
                let Some(candle) = atom.candle else { continue };
                for fill in wallet.update(sym, candle) {
                    strat.on_fill(&fill);
                }
            }
            strat.update(s);
            strat.trade(wallet);
        };
        // A = 300 (top), B = 200 (mid, long), C = 100 (bottom, short).
        tick(&mut strat, &mut wallet, &[("A", 300.0), ("B", 200.0), ("C", 100.0)]);
        tick(&mut strat, &mut wallet, &[("A", 300.0), ("B", 200.0), ("C", 100.0)]);

        // Per-symbol notionals: |units × price|. Longs summed vs shorts summed
        // should be equal (dollar-neutral) and each side ≈ 0.5 × equity ≈ 5000.
        let notional = |sym: &'static str, price: Real| -> Real {
            wallet.position(&sym).amount.abs() * price
        };
        let long_gross = notional("A", 300.0) + notional("B", 200.0);
        let short_gross = notional("C", 100.0);
        // Same tolerance we use elsewhere for equity math with rounded
        // unit counts.
        assert!(
            (long_gross - short_gross).abs() < 50.0,
            "dollar-neutral: longs={long_gross}, shorts={short_gross}",
        );
        // And the target-per-side is the smaller (short) sum ≈ 5000
        // (before any drift for prices set at close = open).
        assert!(
            short_gross > 4_000.0 && short_gross < 6_000.0,
            "dollar-neutral gross-per-side should be ≈ 5000; got {short_gross}",
        );
    }

    #[test]
    fn dollar_neutral_skips_one_sided_rebalance() {
        // Only long side selected → dollar-neutral has no counter-side to
        // hedge, so the whole rebalance skips (no orders queued).
        let mut strat: BasketStrategy<&'static str> =
            BasketStrategy::with_initial_equity(10_000.0)
                .scored_by(|sym: &&'static str| {
                    Close::of(Pick::matching(Selector::by_symbol(*sym)))
                })
                .sized_by(|_| crate::indicators::Value::<Snapshot<&'static str>>::new(0.5))
                .top_bottom(2, 0) // longs only
                .dollar_neutral();
        let mut wallet: PaperWallet<&'static str> = PaperWallet::new(10_000.0);
        for _ in 0..3 {
            let s = snap(&[("A", 300.0), ("B", 200.0), ("C", 100.0)]);
            for (sym_opt, _f, atom) in s.iter() {
                let sym = sym_opt.copied().unwrap();
                let Some(candle) = atom.candle else { continue };
                for fill in wallet.update(sym, candle) {
                    strat.on_fill(&fill);
                }
            }
            strat.update(s);
            strat.trade(&mut wallet);
        }
        assert!(
            wallet.orders().is_empty(),
            "one-sided dollar-neutral basket must not trade; got {} orders",
            wallet.orders().len(),
        );
    }
}
