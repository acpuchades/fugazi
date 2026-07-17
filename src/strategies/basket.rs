//! [`BasketStrategy`]: a cross-sectional, multi-symbol ranker with a
//! caller-declared or floating universe.
//!
//! Where [`SingleAssetStrategy`](crate::strategies::SingleAssetStrategy) drives
//! one asset from boolean signals and [`PairsStrategy`](crate::strategies::PairsStrategy)
//! drives two symbols as a spread, `BasketStrategy` reads the whole
//! [`Snapshot<Sym>`](crate::types::Snapshot) each bar: it scores every symbol
//! present with a per-symbol *scoring* source, applies a **selection closure**
//! (mapping the score map to per-symbol [`Side`]s), and drives each selection
//! long / short / flat. The default universe is *floating*: symbols are
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
//! The crate ships three built-in selection functions — [`top_bottom`],
//! [`threshold`], and [`quantile`] — plus matching builder methods on
//! [`BasketStrategy`] that wrap them. A caller who needs something the
//! built-ins don't cover installs their own closure via
//! [`BasketStrategy::selection`].

use std::collections::HashMap;
use std::hash::Hash;

use crate::indicators::{Book, Every, Position, Value};
use crate::prelude::*;
use crate::types::Snapshot;

/// A per-symbol score / sizing chain: a boxed real-valued indicator over the
/// basket's [`Snapshot<Sym>`](crate::types::Snapshot). One instance is built
/// per symbol on first sight, so every leaf inside is free to root itself on
/// the symbol via [`Pick`](crate::indicators::Pick).
type Chain<Sym> = Box<dyn Indicator<Input = Snapshot<Sym>, Output = Real>>;

/// A per-symbol factory: builds a fresh [`Chain`] for the given symbol. Called
/// exactly once per symbol the first time it appears in a snapshot.
type Factory<Sym> = Box<dyn Fn(&Sym) -> Chain<Sym>>;

// ---------------------------------------------------------------------------
// Selection functions — each rule is a standalone `pub fn` that ranks a
// score map into a per-symbol side, so a caller who knows which rule they
// want can call it directly (`basket::top_bottom(&scores, 3, 3)`) without
// going through [`SelectionRule::pick`]. The enum is a data-carrier for
// [`BasketStrategy`]'s storage and the YAML/spec discriminator; `pick`
// dispatches to whichever of these three functions matches its variant.
// ---------------------------------------------------------------------------

/// Rank `scores` and return the `longs` highest-scoring symbols as
/// [`Side::Buy`] and the `shorts` lowest-scoring as [`Side::Sell`].
///
/// The two sides never overlap: when the pool is smaller than
/// `longs + shorts`, longs are taken first (highest scores) and shorts
/// drawn from what remains. Ties are broken by `HashMap` iteration order
/// and are not stable — a caller who needs deterministic tie-breaking
/// should score unique values (or add a tie-breaker term to the score).
///
/// Symbols not in the returned map are not selected. Called by
/// [`SelectionRule::TopBottom`] and (as a post-count) by [`quantile`].
pub fn top_bottom<Sym: Clone + Hash + Eq>(
    scores: &HashMap<Sym, Real>,
    longs: usize,
    shorts: usize,
) -> HashMap<Sym, Side> {
    let mut sorted: Vec<(&Sym, Real)> =
        scores.iter().map(|(s, &v)| (s, v)).collect();
    // Descending by score. NaN sorts to the end (Ordering::Equal).
    sorted.sort_by(|a, b| {
        b.1.partial_cmp(&a.1).unwrap_or(std::cmp::Ordering::Equal)
    });
    let mut result = HashMap::new();
    let long_count = longs.min(sorted.len());
    for (sym, _) in sorted.iter().take(long_count) {
        result.insert((*sym).clone(), Side::Buy);
    }
    let short_count = shorts.min(sorted.len() - long_count);
    for (sym, _) in sorted.iter().rev().take(short_count) {
        result.insert((*sym).clone(), Side::Sell);
    }
    result
}

/// Long every symbol whose score reads at or above `long_min`; short every
/// symbol whose score reads at or below `short_max`. Symbols in the gap
/// (or missing from `scores`) are not selected.
///
/// When both cutoffs apply to the same score (mis-ordered thresholds with
/// `long_min <= short_max`), **long wins** — the strategy will not put a
/// symbol on both sides at once.
///
/// Called by [`SelectionRule::Threshold`].
pub fn threshold<Sym: Clone + Hash + Eq>(
    scores: &HashMap<Sym, Real>,
    long_min: Real,
    short_max: Real,
) -> HashMap<Sym, Side> {
    let mut result = HashMap::new();
    for (sym, &v) in scores {
        if v >= long_min {
            result.insert(sym.clone(), Side::Buy);
        } else if v <= short_max {
            result.insert(sym.clone(), Side::Sell);
        }
    }
    result
}

/// Long the top `long_q` fraction of the score distribution, short the
/// bottom `short_q`. Counts are `ceil(q * n)` clamped to `[0, n]`.
///
/// The two sides never overlap: longs are drawn first, then shorts from
/// what remains, so `long_q + short_q > 1.0` truncates the shorts.
/// Zero-quantile sides are legal (a top-decile long-only basket is
/// `quantile(scores, 0.1, 0.0)`).
///
/// Called by [`SelectionRule::Quantile`]. Delegates the actual rank to
/// [`top_bottom`] once the two counts are resolved.
pub fn quantile<Sym: Clone + Hash + Eq>(
    scores: &HashMap<Sym, Real>,
    long_q: Real,
    short_q: Real,
) -> HashMap<Sym, Side> {
    let n = scores.len();
    if n == 0 {
        return HashMap::new();
    }
    let long_count = quantile_count(long_q, n).min(n);
    let short_count = quantile_count(short_q, n).min(n - long_count);
    top_bottom(scores, long_count, short_count)
}

/// `ceil(q * n)` clamped to `[0, n]`. Converts a fractional cutoff into a
/// count for [`quantile`].
fn quantile_count(q: Real, n: usize) -> usize {
    if q <= 0.0 {
        0
    } else {
        (q * n as Real).ceil() as usize
    }
}

// ---------------------------------------------------------------------------
// Universe — declared vs. floating symbol scope.
// ---------------------------------------------------------------------------

/// The set of symbols a [`BasketStrategy`] is willing to trade.
///
/// - [`Floating`](Self::Floating) — the default. Symbols are discovered
///   from the incoming snapshot on first sight; nothing is required, nothing
///   is filtered.
/// - [`AllOf`](Self::AllOf) — a strict, declared universe. Only listed
///   symbols enter the basket, every listed symbol *must* appear in every
///   snapshot (an absent symbol **panics** from
///   [`Strategy::update`](crate::Strategy::update)), and
///   [`is_ready`](crate::Strategy::is_ready) stays `false` until every
///   listed symbol has produced a score *and* a size this bar. Catches
///   typos and feed gaps loud instead of silently building an
///   under-populated basket.
/// - [`AnyOf`](Self::AnyOf) — a lax, declared universe. Restricts to the
///   listed subset but silently ignores absent or still-unready members —
///   same per-bar filter the floating universe does, just narrowed to a
///   fixed list.
///
/// Installed via [`BasketStrategy::all_of`] / [`BasketStrategy::any_of`];
/// the floating default matches `Universe::Floating`.
#[derive(Debug, Clone)]
pub enum Universe<Sym> {
    /// No declared scope — every symbol seen in the snapshot enters the
    /// basket lazily.
    Floating,
    /// Strict declared scope: exactly these symbols, every bar. Absence
    /// panics; readiness gates on every listed symbol scoring `Some`.
    AllOf(Vec<Sym>),
    /// Lax declared scope: these symbols only, silently skip absent /
    /// unready members.
    AnyOf(Vec<Sym>),
}

impl<Sym: PartialEq> Universe<Sym> {
    /// Whether `sym` is allowed into the basket under this universe.
    /// [`Floating`](Self::Floating) always accepts; declared universes
    /// accept only listed members. Used at symbol discovery to filter the
    /// incoming snapshot down to admissible names.
    pub fn admits(&self, sym: &Sym) -> bool {
        match self {
            Universe::Floating => true,
            Universe::AllOf(v) | Universe::AnyOf(v) => v.contains(sym),
        }
    }

    /// The symbols this universe *requires* on every bar, if any. Only
    /// [`AllOf`](Self::AllOf) returns `Some`; the strict-erroring convention
    /// panics from `update` when a required symbol is absent from a
    /// snapshot.
    pub fn required(&self) -> Option<&[Sym]> {
        match self {
            Universe::AllOf(v) => Some(v.as_slice()),
            _ => None,
        }
    }
}

/// The selection closure a [`BasketStrategy`] holds: takes a
/// per-symbol score map and returns which symbols to trade on which side.
/// Symbols not in the returned map are not selected (an open position on
/// such a symbol is flattened).
///
/// The three built-in rules — [`top_bottom`] / [`threshold`] /
/// [`quantile`] — all match this signature and are the natural
/// implementations of `.top_bottom(...)` / `.threshold(...)` /
/// `.quantile(...)` on [`BasketStrategy`]. A caller with a custom rule
/// installs an arbitrary closure via [`BasketStrategy::selection`].
type Selection<Sym> = Box<dyn Fn(&HashMap<Sym, Real>) -> HashMap<Sym, Side>>;

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
type RebalanceSignal<Sym> = Box<dyn Indicator<Input = Snapshot<Sym>, Output = bool>>;

pub struct BasketStrategy<Sym> {
    score_factory: Factory<Sym>,
    sizing_factory: Factory<Sym>,
    scores: HashMap<Sym, Chain<Sym>>,
    sizes: HashMap<Sym, Chain<Sym>>,
    positions: HashMap<Sym, Position>,
    latest_score: HashMap<Sym, Real>,
    latest_size: HashMap<Sym, Real>,
    selection: Selection<Sym>,
    /// The **rebalance gate**: on each bar `trade()` runs the selection
    /// and issues resize orders only when this signal reads `true`.
    /// Default is `Every::new(1)` — fires every bar, matching the
    /// pre-`rebalance_on` "re-rank every bar" behavior. Set with
    /// [`rebalance_on`](Self::rebalance_on).
    rebalance: RebalanceSignal<Sym>,
    universe: Universe<Sym>,
    book: Book<Sym>,
}

impl<Sym: Clone + PartialEq + Hash + Eq + 'static> BasketStrategy<Sym> {
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
            scores: HashMap::new(),
            sizes: HashMap::new(),
            positions: HashMap::new(),
            latest_score: HashMap::new(),
            latest_size: HashMap::new(),
            selection: Box::new(|_scores: &HashMap<Sym, Real>| HashMap::new()),
            rebalance: Box::new(Every::<Snapshot<Sym>>::new(1)),
            universe: Universe::Floating,
            book: Book::new(initial_equity),
        }
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
        S: Indicator<Input = Snapshot<Sym>, Output = bool> + 'static,
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
        F: Fn(&Sym) -> I + 'static,
        I: Indicator<Input = Snapshot<Sym>, Output = Real> + 'static,
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
        F: Fn(&Sym) -> I + 'static,
        I: Indicator<Input = Snapshot<Sym>, Output = Real> + 'static,
    {
        self.sizing_factory = Box::new(move |sym: &Sym| {
            let ind: Chain<Sym> = Box::new(factory(sym));
            ind
        });
        self
    }

    /// Take the top `longs` and bottom `shorts` symbols by score. Wraps
    /// [`top_bottom`].
    pub fn top_bottom(self, longs: usize, shorts: usize) -> Self {
        self.selection(move |scores| top_bottom(scores, longs, shorts))
    }

    /// Long every symbol scoring at/above `long_min`; short at/below
    /// `short_max`. Wraps [`threshold`].
    pub fn threshold(self, long_min: Real, short_max: Real) -> Self {
        self.selection(move |scores| threshold(scores, long_min, short_max))
    }

    /// Long the top `long_q` fraction, short the bottom `short_q` fraction
    /// of the score distribution. Wraps [`quantile`].
    pub fn quantile(self, long_q: Real, short_q: Real) -> Self {
        self.selection(move |scores| quantile(scores, long_q, short_q))
    }

    /// Install an **arbitrary selection closure** — the escape hatch for
    /// custom logic the three built-in rules don't cover (e.g. a signal
    /// gate on top of a rank, or a stateful selector that reads the
    /// strategy's [`Book`](Self::book)).
    ///
    /// The closure takes the current bar's score map (one entry per symbol
    /// whose score chain produced a `Some` this bar) and returns the
    /// symbols to trade tagged with a [`Side`]. Symbols not in the
    /// returned map are not selected (an open position on such a symbol
    /// gets flattened). The three sugar builders
    /// ([`top_bottom`](Self::top_bottom) / [`threshold`](Self::threshold) /
    /// [`quantile`](Self::quantile)) delegate here.
    pub fn selection<F>(mut self, f: F) -> Self
    where
        F: Fn(&HashMap<Sym, Real>) -> HashMap<Sym, Side> + 'static,
    {
        self.selection = Box::new(f);
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
    pub fn all_of<I>(mut self, symbols: I) -> Self
    where
        I: IntoIterator<Item = Sym>,
    {
        self.universe = Universe::AllOf(symbols.into_iter().collect());
        self
    }

    /// Restrict this basket to the set `symbols` under a **lax** contract:
    /// only listed symbols enter the basket, but absent or still-unready
    /// members are silently skipped — same per-bar filtering the floating
    /// universe does, just narrowed to a fixed list.
    pub fn any_of<I>(mut self, symbols: I) -> Self
    where
        I: IntoIterator<Item = Sym>,
    {
        self.universe = Universe::AnyOf(symbols.into_iter().collect());
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

    /// The largest `stable_period()` across every currently-built score /
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
    /// `stable_period()`.
    pub fn stable_period(&self) -> usize {
        let mut n = self.rebalance.stable_period();
        for score in self.scores.values() {
            n = n.max(score.stable_period());
        }
        for size in self.sizes.values() {
            n = n.max(size.stable_period());
        }
        n
    }

    /// The warm-up-only twin of [`stable_period`](Self::stable_period) —
    /// ignores IIR unstable settling. Used by
    /// `optimize --walkforward --keep-unstable`.
    ///
    /// Same lazy-readiness caveat: feed one snapshot before probing so
    /// per-symbol chains exist.
    pub fn warm_up_period(&self) -> usize {
        let mut n = self.rebalance.warm_up_period();
        for score in self.scores.values() {
            n = n.max(score.warm_up_period());
        }
        for size in self.sizes.values() {
            n = n.max(size.warm_up_period());
        }
        n
    }
}

impl<Sym: Clone + PartialEq + Hash + Eq + 'static> Default for BasketStrategy<Sym> {
    fn default() -> Self {
        Self::new()
    }
}

impl<Sym: Clone + PartialEq + Hash + Eq + 'static> Strategy for BasketStrategy<Sym> {
    type Input = Snapshot<Sym>;
    type Symbol = Sym;

    fn update(&mut self, snap: Snapshot<Sym>) {
        // 0. Universe: !all_of requires every listed symbol on every bar.
        // Absence is a hard panic — the point of a strict universe is to
        // catch feed gaps and typos at the first bar, not silently trade a
        // smaller basket.
        if let Some(required) = self.universe.required() {
            for sym in required {
                let present = snap.iter().any(|(s, _, _)| s == Some(sym));
                if !present {
                    panic!(
                        "BasketStrategy: `all_of` universe requires every \
                         listed symbol to be present in every snapshot, but \
                         at least one is missing this bar. Either fix the \
                         data feed or switch to `any_of` if silent skipping \
                         is what you want."
                    );
                }
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
            let score = (self.score_factory)(&sym);
            let size = (self.sizing_factory)(&sym);
            self.scores.insert(sym.clone(), score);
            self.sizes.insert(sym.clone(), size);
            self.positions.insert(sym, Position::new());
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

        // 3. Fold the in-progress bar into each held symbol's Position.
        // 4. Mark the Book to market with per-leg closes, all in one pass.
        let mut marks: Vec<(Sym, Candle)> = Vec::new();
        for (sym_opt, _freq, atom) in snap.iter() {
            if let Some(sym) = sym_opt {
                if let Some(pos) = self.positions.get(sym) {
                    pos.update(atom.candle);
                }
                marks.push((sym.clone(), atom.candle));
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
        let selection = (self.selection)(&self.latest_score);
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
                    let is_long = position.map(|p| p.is_long()).unwrap_or(false);
                    if !is_long {
                        let _ =
                            wallet.set(sym.clone(), Side::Buy, Size::value_frac(size));
                        let _ = wallet.cancel_protective(sym);
                    }
                }
                Some(Side::Sell) => {
                    let Some(&size) = self.latest_size.get(sym) else {
                        continue;
                    };
                    let is_short = position.map(|p| p.is_short()).unwrap_or(false);
                    if !is_short {
                        let _ =
                            wallet.set(sym.clone(), Side::Sell, Size::value_frac(size));
                        let _ = wallet.cancel_protective(sym);
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
        // Floating / any_of: per-symbol readiness is enforced inside
        // `trade` by only considering symbols that scored `Some` this bar,
        // so the strategy is always ready to *try*.
        //
        // all_of: strict — the driver skips `trade` until every listed
        // symbol has both scored and sized `Some`, so the basket sits
        // through warm-up rather than picking from a partial universe.
        match &self.universe {
            Universe::AllOf(required) => required.iter().all(|s| {
                self.latest_score.contains_key(s) && self.latest_size.contains_key(s)
            }),
            _ => true,
        }
    }

    fn reset(&mut self) {
        self.scores.clear();
        self.sizes.clear();
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
    use crate::indicators::{Close, IndicatorExt, Pick};
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

    // ---------------- Selection functions --------------------------------

    #[test]
    fn top_bottom_ranks_and_splits_by_score() {
        let mut scores = HashMap::new();
        scores.insert("A", 5.0);
        scores.insert("B", 4.0);
        scores.insert("C", 3.0);
        scores.insert("D", 2.0);
        scores.insert("E", 1.0);
        let picked = top_bottom(&scores, 2, 2);
        // A, B → long; D, E → short; C rests.
        assert_eq!(picked.get("A"), Some(&Side::Buy));
        assert_eq!(picked.get("B"), Some(&Side::Buy));
        assert_eq!(picked.get("D"), Some(&Side::Sell));
        assert_eq!(picked.get("E"), Some(&Side::Sell));
        assert_eq!(picked.get("C"), None);
    }

    #[test]
    fn top_bottom_never_overlaps_when_pool_is_small() {
        let mut scores = HashMap::new();
        scores.insert("A", 3.0);
        scores.insert("B", 2.0);
        scores.insert("C", 1.0);
        // 5 longs and 5 shorts on 3 candidates: longs take all 3, shorts
        // get nothing (no overlap).
        let picked = top_bottom(&scores, 5, 5);
        assert_eq!(picked.len(), 3);
        assert!(picked.values().all(|s| *s == Side::Buy));
    }

    #[test]
    fn threshold_selects_by_cutoffs() {
        let mut scores = HashMap::new();
        scores.insert("A", 1.2);
        scores.insert("B", 0.5);
        scores.insert("C", 0.0);
        scores.insert("D", -0.5);
        scores.insert("E", -1.5);
        let picked = threshold(&scores, 1.0, -1.0);
        assert_eq!(picked.get("A"), Some(&Side::Buy));
        assert_eq!(picked.get("E"), Some(&Side::Sell));
        assert_eq!(picked.get("B"), None);
        assert_eq!(picked.get("C"), None);
        assert_eq!(picked.get("D"), None);
    }

    #[test]
    fn quantile_uses_ceil_and_avoids_overlap() {
        // 10 candidates, top-decile long, bottom-decile short → 1 each.
        let mut scores = HashMap::new();
        for i in 0..10 {
            scores.insert(format!("S{i:02}"), i as Real);
        }
        let picked = quantile(&scores, 0.1, 0.1);
        assert_eq!(picked.get("S09"), Some(&Side::Buy));
        assert_eq!(picked.get("S00"), Some(&Side::Sell));
        assert_eq!(picked.len(), 2);
    }

    #[test]
    fn quantile_truncates_shorts_on_overflow() {
        // long_q + short_q > 1: shorts get whatever is left after longs.
        let mut scores = HashMap::new();
        scores.insert("A", 4.0);
        scores.insert("B", 3.0);
        scores.insert("C", 2.0);
        scores.insert("D", 1.0);
        // long_count = ceil(0.8 * 4) = 4 → all long, no shorts.
        let picked = quantile(&scores, 0.8, 0.5);
        assert_eq!(picked.len(), 4);
        assert!(picked.values().all(|s| *s == Side::Buy));
    }

    #[test]
    fn custom_selection_closure_is_installed_verbatim() {
        // A whimsical rule: long any symbol whose name starts with 'A'.
        let mut strat: BasketStrategy<&'static str> =
            BasketStrategy::with_initial_equity(1_000.0)
                .scored_by(|sym: &&'static str| {
                    Close::of(Pick::matching(Selector::by_symbol(*sym)))
                })
                .sized_by(|_| equal_weight::<&'static str>(2))
                .selection(|scores| {
                    let mut out = HashMap::new();
                    for sym in scores.keys() {
                        if sym.starts_with('A') {
                            out.insert(*sym, Side::Buy);
                        }
                    }
                    out
                });
        let mut wallet: PaperWallet<&'static str> = PaperWallet::new(1_000.0);
        let tick = |strat: &mut BasketStrategy<&'static str>,
                    wallet: &mut PaperWallet<&'static str>,
                    entries: &[(&'static str, Real)]| {
            let s = snap(entries);
            for (sym_opt, _f, atom) in s.iter() {
                let sym = sym_opt.copied().unwrap();
                for fill in wallet.update(sym, atom.candle) {
                    strat.on_fill(&fill);
                }
            }
            strat.update(s);
            strat.trade(wallet);
        };
        tick(&mut strat, &mut wallet, &[("AAPL", 100.0), ("BTC", 50.0)]);
        tick(&mut strat, &mut wallet, &[("AAPL", 100.0), ("BTC", 50.0)]);
        assert!(wallet.position(&"AAPL").amount > 0.0, "AAPL long via custom rule");
        assert!(
            wallet.position(&"BTC").amount.abs() < 1e-9,
            "BTC not picked, so flat"
        );
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
            for fill in wallet.update(sym, atom.candle) {
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
            for fill in wallet.update(sym, atom.candle) {
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
                for fill in wallet.update(sym, atom.candle) {
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
                for fill in wallet.update(sym, atom.candle) {
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
                for fill in wallet.update(sym, atom.candle) {
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
            for fill in wallet.update(sym, atom.candle) {
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
                for fill in wallet.update(sym, atom.candle) {
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

    // Silence "unused" for helpers only reached from IndicatorExt-derived
    // Roc chains in the doctest / above.
    #[allow(dead_code)]
    fn _touch_indicator_ext() {
        let _ = Close::<Pick<&'static str>>::of(Pick::<&'static str>::new()).roc(5);
    }

    // ---------------- Universe (all_of / any_of) -------------------------

    #[test]
    fn all_of_restricts_discovery_to_listed_symbols() {
        // Universe = {A, B}. Snapshot carries A, B, C — C should never get
        // a chain built.
        let mut strat: BasketStrategy<&'static str> =
            BasketStrategy::with_initial_equity(1_000.0)
                .scored_by(|sym: &&'static str| {
                    Close::of(Pick::matching(Selector::by_symbol(*sym)))
                })
                .sized_by(|_| equal_weight::<&'static str>(2))
                .top_bottom(1, 1)
                .all_of(["A", "B"]);
        strat.update(snap(&[("A", 100.0), ("B", 50.0), ("C", 200.0)]));
        assert!(strat.position(&"A").is_some());
        assert!(strat.position(&"B").is_some());
        assert!(
            strat.position(&"C").is_none(),
            "C is not in the declared universe; no chain / position should be built for it"
        );
    }

    #[test]
    #[should_panic(expected = "`all_of` universe requires")]
    fn all_of_panics_when_listed_symbol_absent() {
        let mut strat: BasketStrategy<&'static str> =
            BasketStrategy::with_initial_equity(1_000.0)
                .scored_by(|sym: &&'static str| {
                    Close::of(Pick::matching(Selector::by_symbol(*sym)))
                })
                .sized_by(|_| equal_weight::<&'static str>(2))
                .top_bottom(1, 1)
                .all_of(["A", "B"]);
        // B is missing from the snapshot — strict-erroring convention.
        strat.update(snap(&[("A", 100.0)]));
    }

    #[test]
    fn all_of_is_ready_gates_on_every_listed_symbol_scoring() {
        // Score = SMA-3 so the first two bars score None for every symbol.
        // Under !all_of, is_ready must stay false until every listed
        // symbol has settled.
        let mut strat: BasketStrategy<&'static str> =
            BasketStrategy::with_initial_equity(1_000.0)
                .scored_by(|sym: &&'static str| {
                    crate::indicators::Sma::new(
                        Close::of(Pick::matching(Selector::by_symbol(*sym))),
                        3,
                    )
                })
                .sized_by(|_| equal_weight::<&'static str>(2))
                .top_bottom(1, 1)
                .all_of(["A", "B"]);
        assert!(!strat.is_ready(), "empty basket cannot be ready under all_of");
        strat.update(snap(&[("A", 100.0), ("B", 50.0)]));
        assert!(!strat.is_ready(), "first bar: SMA-3 not warmed for either");
        strat.update(snap(&[("A", 101.0), ("B", 51.0)]));
        assert!(!strat.is_ready(), "second bar: SMA-3 still not warmed");
        strat.update(snap(&[("A", 102.0), ("B", 52.0)]));
        assert!(
            strat.is_ready(),
            "third bar: both listed symbols have scored — ready"
        );
    }

    #[test]
    fn any_of_ignores_absent_symbols() {
        // Universe = {A, B} lax. B is missing on this bar — must not panic.
        let mut strat: BasketStrategy<&'static str> =
            BasketStrategy::with_initial_equity(1_000.0)
                .scored_by(|sym: &&'static str| {
                    Close::of(Pick::matching(Selector::by_symbol(*sym)))
                })
                .sized_by(|_| equal_weight::<&'static str>(2))
                .top_bottom(1, 1)
                .any_of(["A", "B"]);
        strat.update(snap(&[("A", 100.0)]));
        assert!(strat.position(&"A").is_some());
        // No B in the snapshot: no chain built yet (it hasn't been seen).
        assert!(strat.position(&"B").is_none());
        // any_of doesn't gate readiness on absence.
        assert!(strat.is_ready());
    }

    #[test]
    fn any_of_restricts_discovery_to_listed_symbols() {
        // Same shape as the all_of restriction test, but without the
        // presence-required panic. C in the snapshot must still be
        // filtered out at discovery.
        let mut strat: BasketStrategy<&'static str> =
            BasketStrategy::with_initial_equity(1_000.0)
                .scored_by(|sym: &&'static str| {
                    Close::of(Pick::matching(Selector::by_symbol(*sym)))
                })
                .sized_by(|_| equal_weight::<&'static str>(2))
                .top_bottom(1, 1)
                .any_of(["A", "B"]);
        strat.update(snap(&[("A", 100.0), ("B", 50.0), ("C", 200.0)]));
        assert!(strat.position(&"A").is_some());
        assert!(strat.position(&"B").is_some());
        assert!(strat.position(&"C").is_none());
    }

    #[test]
    fn floating_universe_is_ready_by_default() {
        // Sanity: the default (no all_of / no any_of) leaves is_ready as
        // the trait default.
        let strat: BasketStrategy<&'static str> =
            BasketStrategy::with_initial_equity(1_000.0);
        assert!(strat.is_ready());
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
                for fill in wallet.update(sym, atom.candle) {
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
                for fill in wallet.update(sym, atom.candle) {
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
        // With `rebalance_on(Const::new(false))`, the basket never runs
        // selection. No orders at all.
        use crate::indicators::Const;
        let mut strat: BasketStrategy<&'static str> =
            BasketStrategy::with_initial_equity(10_000.0)
                .scored_by(|sym: &&'static str| {
                    Close::of(Pick::matching(Selector::by_symbol(*sym)))
                })
                .sized_by(|_| equal_weight::<&'static str>(2))
                .top_bottom(1, 1)
                .rebalance_on(Const::<Snapshot<&'static str>>::new(false));
        let mut wallet: PaperWallet<&'static str> = PaperWallet::new(10_000.0);
        for _ in 0..5 {
            let s = snap(&[("A", 100.0), ("B", 50.0)]);
            for (sym_opt, _f, atom) in s.iter() {
                let sym = sym_opt.copied().unwrap();
                for fill in wallet.update(sym, atom.candle) {
                    strat.on_fill(&fill);
                }
            }
            strat.update(s);
            strat.trade(&mut wallet);
        }
        assert!(wallet.orders().is_empty(), "never-rebalance basket must not trade");
    }
}
