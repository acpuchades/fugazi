//! [`MultiAssetStrategy`]: run the same
//! [`SingleAssetStrategy`](crate::strategies::SingleAssetStrategy)-shaped
//! decision independently on every symbol in a snapshot.
//!
//! Where [`BasketStrategy`](crate::strategies::BasketStrategy) is
//! *cross-sectional* — its selection closure compares symbols against
//! each other and picks a subset to trade — `MultiAssetStrategy` is
//! *independent*: each symbol runs the same signals, protective levels,
//! and sizing rule in isolation, and any subset of them can be long,
//! short, or flat concurrently without competing for a slot.
//!
//! The primitive shape mirrors
//! [`SingleAssetStrategy`](crate::strategies::SingleAssetStrategy): four
//! boolean signal slots (open long / close long / open short / close
//! short), four optional protective levels (long / short stop-loss and
//! take-profit), a sizing multiplier, and the same
//! [`value_frac(m)`](crate::Size::value_frac) entry semantics. The
//! difference is that every slot is a **factory closure** that's built
//! once per symbol on first sight — every leaf inside is expected to
//! root itself on the current symbol via
//! [`Pick::matching(Selector::by_symbol(sym.clone()))`](crate::indicators::Pick),
//! and protective-level factories additionally receive the per-symbol
//! [`Position`] so `position.entry()` / `.peak()` / `.trough()` track the
//! actual entry per leg.
//!
//! Uses the same [`Universe`] trait knob as
//! [`BasketStrategy`](crate::strategies::BasketStrategy) — declare
//! [`all_of`](MultiAssetStrategy::all_of) (strict),
//! [`any_of`](MultiAssetStrategy::any_of) (lax), or
//! [`universe(custom)`](MultiAssetStrategy::universe) to plug in an arbitrary
//! [`Universe`] impl. Leaving the default [`Floating`] picks up every
//! symbol the snapshot carries.

use std::collections::HashMap;
use std::hash::Hash;

use super::{Chain, LevelFactory};
use crate::hash::SymMap;
use crate::indicators::{Book, Position, Value, ValueBool};
use crate::prelude::*;
use crate::strategies::universe::{AllOf, AnyOf, Floating, Universe};
use crate::types::Snapshot;

// ---------------------------------------------------------------------------
// Chain type aliases
// ---------------------------------------------------------------------------

/// A per-symbol boolean chain — one of the four signal slots.
type SignalChain<Sym> = Box<dyn Indicator<Input = Snapshot<Sym>, Output = bool> + Send + Sync>;

/// A per-symbol signal factory: `Fn(&Sym) -> SignalChain<Sym>`.
type SignalFactory<Sym> = Box<dyn Fn(&Sym) -> SignalChain<Sym> + Send + Sync>;

/// A per-symbol sizing factory: `Fn(&Sym) -> Chain<Sym>`. The sizing
/// slot doesn't take a [`Position`] because a size that reads back the
/// entry price for its own leg is unusual — most sizing recipes are
/// symbol-agnostic magnitudes (equal weight, ATR risk, drawdown throttle
/// on the shared [`Book`]).
type SizingFactory<Sym> = Box<dyn Fn(&Sym) -> Chain<Sym> + Send + Sync>;

/// The **rebalance gate** — a boolean signal decided on the whole
/// snapshot (not per symbol). On bars where it reads `true`,
/// [`MultiAssetStrategy::trade`] resizes every held per-symbol position
/// to its current sizing target.
type RebalanceSignal<Sym> = Box<dyn Indicator<Input = Snapshot<Sym>, Output = bool> + Send + Sync>;

// ---------------------------------------------------------------------------
// Per-symbol state
// ---------------------------------------------------------------------------

/// The signals, protective levels, sizing, position, and warm-up counter
/// held per symbol. Built lazily by [`MultiAssetStrategy::update`] the
/// first bar a symbol appears in the snapshot (and passes the universe
/// admittance check).
struct PerAssetState<Sym> {
    long: SignalChain<Sym>,
    close_long: SignalChain<Sym>,
    short: SignalChain<Sym>,
    close_short: SignalChain<Sym>,
    long_stop: Option<Chain<Sym>>,
    long_target: Option<Chain<Sym>>,
    short_stop: Option<Chain<Sym>>,
    short_target: Option<Chain<Sym>>,
    sizing: Chain<Sym>,
    position: Position,
    bars_seen: usize,
    /// This leg's [`stable_bars`](Self::stable_bars), computed once when the
    /// leg is built.
    ///
    /// `is_ready` is consulted per symbol per bar inside
    /// [`MultiAssetStrategy::trade`], and `stable_bars()` is a walk of this
    /// leg's whole chain set whose visit count grows exponentially with
    /// expression depth (`Combine::unstable_bars` asks both children for
    /// `stable_bars()` and then asks itself for `warm_up_bars()`, walking them
    /// again). An N-symbol universe paid that N times a bar.
    ///
    /// A plain field rather than a `OnceLock`: unlike the single-asset and
    /// pairs shapes there are no builders here — a leg is constructed whole by
    /// [`MultiAssetStrategy::build_state`] and its chains are never replaced —
    /// so the value is known at construction and can never go stale.
    stable_bars: usize,
}

impl<Sym> PerAssetState<Sym> {
    /// Largest `stable_bars()` across this symbol's four signals, four
    /// (optional) protective levels, and sizing — same aggregation as
    /// [`SingleAssetStrategy::stable_bars`](crate::strategies::SingleAssetStrategy::stable_bars),
    /// applied per leg.
    ///
    /// Recomputes the threshold from the chains. Called once,
    /// at construction, into the [`stable_bars`](Self::stable_bars) field —
    /// read that instead on any hot path.
    fn compute_stable_bars(&self) -> usize {
        let mut n = self.long.stable_bars();
        n = n.max(self.close_long.stable_bars());
        n = n.max(self.short.stable_bars());
        n = n.max(self.close_short.stable_bars());
        for level in [
            &self.long_stop,
            &self.long_target,
            &self.short_stop,
            &self.short_target,
        ]
        .into_iter()
        .flatten()
        {
            n = n.max(level.stable_bars());
        }
        n.max(self.sizing.stable_bars())
    }

    /// Largest `warm_up_bars()` across this symbol's four signals, four
    /// (optional) protective levels, and sizing — the warm-up-only twin of
    /// [`stable_bars`](Self::stable_bars), ignoring IIR unstable
    /// settling.
    fn warm_up_bars(&self) -> usize {
        let mut n = self.long.warm_up_bars();
        n = n.max(self.close_long.warm_up_bars());
        n = n.max(self.short.warm_up_bars());
        n = n.max(self.close_short.warm_up_bars());
        for level in [
            &self.long_stop,
            &self.long_target,
            &self.short_stop,
            &self.short_target,
        ]
        .into_iter()
        .flatten()
        {
            n = n.max(level.warm_up_bars());
        }
        n.max(self.sizing.warm_up_bars())
    }

    /// Whether this leg has seen enough bars for its own decision to be
    /// safe to act on. Consulted at trade time; also folded into the
    /// [`MultiAssetStrategy::is_ready`] gate under a strict
    /// [`Universe`](crate::strategies::universe::Universe) impl (e.g.
    /// [`AllOf`](crate::strategies::universe::AllOf)).
    fn is_ready(&self) -> bool {
        self.bars_seen >= self.stable_bars
    }
}

// ---------------------------------------------------------------------------
// MultiAssetStrategy
// ---------------------------------------------------------------------------

/// An independent, per-symbol strategy driven by the same signals,
/// protective levels, and sizing rule as
/// [`SingleAssetStrategy`](crate::strategies::SingleAssetStrategy),
/// replicated across every symbol the snapshot carries (or a declared
/// [`Universe`] subset).
///
/// Each bar `MultiAssetStrategy` advances every symbol's chains against
/// the whole [`Snapshot<Sym>`](crate::types::Snapshot), folds each
/// symbol's atom into its own [`Position`], and — for each symbol whose
/// state is past its own warm-up — runs the identical trade logic as
/// [`SingleAssetStrategy`](crate::strategies::SingleAssetStrategy): sizing skip on `None`, entry / reversal,
/// signal-driven flatten, then rest the active side's protective level.
///
/// ## Independent, not cross-sectional
///
/// A leg's decision is made **only from that leg's own signals** — there
/// is no ranking, no picking winners and losers across symbols. Any
/// subset of symbols can be long, short, or flat at the same time.
/// Reach for [`BasketStrategy`](crate::strategies::BasketStrategy) when
/// you want a symbol's fate to depend on how it scores relative to the
/// rest of the universe; reach for `MultiAssetStrategy` when you want
/// the same signal set applied independently across a portfolio.
///
/// ## Symbol discovery
///
/// The default universe is *floating*: symbols are discovered from the
/// incoming snapshot on first sight, and the per-symbol chains are spun
/// up lazily by the caller-supplied factories. Every leaf inside is
/// expected to root itself on the current symbol via
/// [`Pick::matching(Selector::by_symbol(sym.clone()))`](crate::indicators::Pick).
/// Protective-level factories additionally receive the per-symbol
/// [`Position`] (see [`long_stop_loss`](Self::long_stop_loss) et al.),
/// so `position.entry()` etc. compose as they do on
/// [`SingleAssetStrategy`](crate::strategies::SingleAssetStrategy).
///
/// A caller who wants a *declared* universe uses [`all_of`](Self::all_of)
/// (strict — panics on absence, gates
/// [`is_ready`](Strategy::is_ready) on every listed symbol being past
/// its warm-up) or [`any_of`](Self::any_of) (lax — restricts to the
/// listed subset but silently ignores absent / unready members).
///
/// ## Readiness
///
/// [`is_ready`](Strategy::is_ready) mirrors
/// [`BasketStrategy`](crate::strategies::BasketStrategy)'s convention:
/// under `Floating` / `any_of` it returns `true` unconditionally and the
/// per-symbol warm-up is enforced inside
/// [`trade`](Strategy::trade) (a symbol whose own state hasn't settled
/// simply doesn't trade this bar); under `all_of` it stays `false` until
/// every listed symbol has passed its own
/// `stable_bars` so the driver skips
/// [`trade`](Strategy::trade) entirely while the declared universe warms.
///
/// ## Book anchor
///
/// The strategy owns a shared [`Book`] that tracks aggregate cash /
/// per-leg units / marked-to-market equity across every symbol — one
/// trade in the book's sense is one open-to-flat cycle across the whole
/// portfolio (matching how
/// [`BasketStrategy`](crate::strategies::BasketStrategy) and
/// [`PairsStrategy`](crate::strategies::PairsStrategy) aggregate). Seed
/// via [`with_initial_equity`](Self::with_initial_equity) to match the
/// wallet's starting cash for book-anchored sizing recipes to read
/// meaningful numbers.
///
/// ## Costs
///
/// Costs live on the [`Wallet`], not on the strategy: install per-symbol
/// trading costs via
/// [`PaperWallet::set_costs_for`](crate::PaperWallet::set_costs_for) and
/// they apply transparently to every fill on that symbol.
///
/// ## Example
///
/// A short-term-reversal portfolio: on each symbol, go long when its
/// short SMA crosses above the long SMA, flatten when it crosses back;
/// go short on the opposite cross, flatten symmetrically. Equal-weight
/// sizing at 25% per leg (4 legs = 100% gross).
///
/// ```
/// use fugazi::prelude::*;
/// use fugazi::indicators::sizing::equal_weight;
/// use fugazi::indicators::{Close, Pick, Sma};
/// use fugazi::strategies::MultiAssetStrategy;
/// use fugazi::types::Selector;
///
/// fn close_of(sym: &String) -> Close<Pick<String>> {
///     Close::of(Pick::matching(Selector::by_symbol(sym.clone())))
/// }
/// let strat: MultiAssetStrategy<String> =
///     MultiAssetStrategy::with_initial_equity(100_000.0)
///         .long_on(
///             |sym: &String| Sma::new(close_of(sym), 5).crosses_above(Sma::new(close_of(sym), 20)),
///             |sym: &String| Sma::new(close_of(sym), 5).crosses_below(Sma::new(close_of(sym), 20)),
///         )
///         .short_on(
///             |sym: &String| Sma::new(close_of(sym), 5).crosses_below(Sma::new(close_of(sym), 20)),
///             |sym: &String| Sma::new(close_of(sym), 5).crosses_above(Sma::new(close_of(sym), 20)),
///         )
///         .position_sizing(|_sym: &String| equal_weight::<String>(4))
///         .all_of(["BTC".to_string(), "ETH".to_string(), "SOL".to_string(), "ADA".to_string()]);
/// # let _ = strat;
/// ```
pub struct MultiAssetStrategy<Sym> {
    long_factory: SignalFactory<Sym>,
    close_long_factory: SignalFactory<Sym>,
    short_factory: SignalFactory<Sym>,
    close_short_factory: SignalFactory<Sym>,
    long_stop_factory: Option<LevelFactory<Sym>>,
    long_target_factory: Option<LevelFactory<Sym>>,
    short_stop_factory: Option<LevelFactory<Sym>>,
    short_target_factory: Option<LevelFactory<Sym>>,
    sizing_factory: SizingFactory<Sym>,
    /// Symbol-keyed, so it uses the crate's FxHash like `bar_candles` below —
    /// see `src/hash.rs`. The discovery filter in `update` does a
    /// `contains_key` here **once per symbol per bar**, which was the largest
    /// SipHash consumer in a multi-asset profile (docs/PERFORMANCE.md,
    /// Phase 13).
    states: SymMap<Sym, PerAssetState<Sym>>,
    /// The rebalance gate: on bars where it fires, `trade` resizes every
    /// held per-symbol position to its current sizing target. Default is
    /// `ValueBool::new(false)` — never rebalance — so a strategy that
    /// doesn't wire `.rebalance_on(...)` behaves exactly as before (sizing
    /// only read on transitions).
    rebalance: RebalanceSignal<Sym>,
    universe: Box<dyn Universe<Sym>>,
    book: Book<Sym>,
    /// This bar's `(symbol, candle)` pairs, rebuilt at the top of each
    /// [`update`](Strategy::update) and consumed by both the per-leg position
    /// fold and the book mark.
    ///
    /// A reused scratch buffer, not state: it is cleared and refilled every bar,
    /// and nothing reads it between bars. Held on the struct purely so the
    /// allocation is amortised — the previous code built a fresh `Vec` for the
    /// book marks on every bar, on top of rescanning the snapshot once per
    /// symbol.
    ///
    /// A map rather than a `Vec` because the per-leg loop *looks each symbol
    /// up*: with a `Vec` that is a linear scan done once per symbol, which is
    /// the same O(N^2) per bar the snapshot rescan had. Keyed lookup is what
    /// makes the per-symbol-bar cost flat in universe size.
    bar_candles: SymMap<Sym, Candle>,
}

impl<Sym: Clone + PartialEq + Hash + Eq + 'static + Send + Sync> MultiAssetStrategy<Sym> {
    /// A fresh multi-asset strategy with every signal slot a
    /// constant-`false`, no protective levels, a constant-`1.0` sizing,
    /// and a seed-1.0 [`Book`]. Add sides with [`long_on`](Self::long_on)
    /// / [`short_on`](Self::short_on); attach protective levels with
    /// [`long_stop_loss`](Self::long_stop_loss) et al.
    ///
    /// The seed-1.0 book is fine for unit-scale tests; for a real
    /// backtest use [`with_initial_equity`](Self::with_initial_equity) to
    /// match the wallet's starting cash so the book-anchored sizing
    /// recipes read meaningful numbers.
    pub fn new() -> Self {
        Self::with_initial_equity(1.0)
    }

    /// A fresh multi-asset strategy whose shared [`Book`] is seeded at
    /// `initial_equity` — the assumed starting capital, which should
    /// match the wallet's starting cash for aggregate equity / drawdown
    /// numbers to be meaningful.
    ///
    /// # Panics
    /// Panics if `initial_equity` is not strictly positive.
    pub fn with_initial_equity(initial_equity: Real) -> Self {
        Self {
            long_factory: Box::new(|_sym: &Sym| {
                let s: SignalChain<Sym> = Box::new(ValueBool::<Snapshot<Sym>>::new(false));
                s
            }),
            close_long_factory: Box::new(|_sym: &Sym| {
                let s: SignalChain<Sym> = Box::new(ValueBool::<Snapshot<Sym>>::new(false));
                s
            }),
            short_factory: Box::new(|_sym: &Sym| {
                let s: SignalChain<Sym> = Box::new(ValueBool::<Snapshot<Sym>>::new(false));
                s
            }),
            close_short_factory: Box::new(|_sym: &Sym| {
                let s: SignalChain<Sym> = Box::new(ValueBool::<Snapshot<Sym>>::new(false));
                s
            }),
            long_stop_factory: None,
            long_target_factory: None,
            short_stop_factory: None,
            short_target_factory: None,
            sizing_factory: Box::new(|_sym: &Sym| {
                let s: Chain<Sym> = Box::new(Value::<Snapshot<Sym>>::new(1.0));
                s
            }),
            states: SymMap::default(),
            rebalance: Box::new(ValueBool::<Snapshot<Sym>>::new(false)),
            universe: Box::new(Floating),
            book: Book::new(initial_equity),
            bar_candles: SymMap::default(),
        }
    }

    /// Install the **rebalance gate** — a boolean signal that decides,
    /// on each bar, whether [`trade`](Strategy::trade) resizes every
    /// held per-symbol position to its current sizing target. Defaults
    /// to a constant `false` (never rebalance — matches the pre-refactor
    /// behavior where sizing is only read on transitions).
    ///
    /// A common non-default: `Every::new(20)` for a ~monthly rebalance
    /// on a daily strategy, or an equity-drawdown signal for
    /// drawdown-triggered de-risking. On bars where the gate is `true`,
    /// the strategy issues `wallet.set(sym, held_side, value_frac(size))`
    /// on each open leg — a no-op when the target size matches current,
    /// a market resize otherwise. Entry / exit signals still fire every
    /// bar independently of the gate.
    ///
    /// A `None` reading is treated as `false` — the safe default.
    pub fn rebalance_on<S>(mut self, signal: S) -> Self
    where
        S: Indicator<Input = Snapshot<Sym>, Output = bool> + 'static + Send + Sync,
    {
        self.rebalance = Box::new(signal);
        self
    }

    /// Wire the **long side**: `enter` opens (or reverses into) a long,
    /// `exit` flattens the long. Both are factories called once per
    /// symbol on first sight — every atom-input leaf inside is expected
    /// to root itself on the current symbol via
    /// [`Pick::matching(Selector::by_symbol(sym.clone()))`](crate::indicators::Pick).
    ///
    /// Chainable with [`short_on`](Self::short_on) for a per-symbol
    /// long/short strategy; because opening the short closes an open
    /// long (and vice versa), an always-in per-symbol reversal reads as
    /// `.long_on(up, down).short_on(down, up)`.
    pub fn long_on<E, X, FE, FX>(mut self, enter: FE, exit: FX) -> Self
    where
        FE: Fn(&Sym) -> E + 'static + Send + Sync,
        FX: Fn(&Sym) -> X + 'static + Send + Sync,
        E: Indicator<Input = Snapshot<Sym>, Output = bool> + 'static + Send + Sync,
        X: Indicator<Input = Snapshot<Sym>, Output = bool> + 'static + Send + Sync,
    {
        self.long_factory = Box::new(move |sym: &Sym| {
            let s: SignalChain<Sym> = Box::new(enter(sym));
            s
        });
        self.close_long_factory = Box::new(move |sym: &Sym| {
            let s: SignalChain<Sym> = Box::new(exit(sym));
            s
        });
        self
    }

    /// Wire the **short side**: `enter` opens (or reverses into) a
    /// short, `exit` flattens the short. Same factory shape as
    /// [`long_on`](Self::long_on); opening the short closes any open
    /// long on that symbol.
    pub fn short_on<E, X, FE, FX>(mut self, enter: FE, exit: FX) -> Self
    where
        FE: Fn(&Sym) -> E + 'static + Send + Sync,
        FX: Fn(&Sym) -> X + 'static + Send + Sync,
        E: Indicator<Input = Snapshot<Sym>, Output = bool> + 'static + Send + Sync,
        X: Indicator<Input = Snapshot<Sym>, Output = bool> + 'static + Send + Sync,
    {
        self.short_factory = Box::new(move |sym: &Sym| {
            let s: SignalChain<Sym> = Box::new(enter(sym));
            s
        });
        self.close_short_factory = Box::new(move |sym: &Sym| {
            let s: SignalChain<Sym> = Box::new(exit(sym));
            s
        });
        self
    }

    /// Attach a **long stop-loss** level factory: called once per symbol
    /// on first sight with `(sym, position)`, where `position` is that
    /// symbol's tracked [`Position`]. Compose the level from
    /// `position.entry()` (fixed) / `position.peak()` (trailing) etc.,
    /// same as
    /// [`SingleAssetStrategy::long_stop_loss`](crate::strategies::SingleAssetStrategy::long_stop_loss).
    pub fn long_stop_loss<F, L>(mut self, factory: F) -> Self
    where
        F: Fn(&Sym, &Position) -> L + 'static + Send + Sync,
        L: Indicator<Input = Snapshot<Sym>, Output = Real> + 'static + Send + Sync,
    {
        self.long_stop_factory = Some(super::level_factory(factory));
        self
    }

    /// Attach a **long take-profit** level factory. Shape mirrors
    /// [`long_stop_loss`](Self::long_stop_loss).
    pub fn long_take_profit<F, L>(mut self, factory: F) -> Self
    where
        F: Fn(&Sym, &Position) -> L + 'static + Send + Sync,
        L: Indicator<Input = Snapshot<Sym>, Output = Real> + 'static + Send + Sync,
    {
        self.long_target_factory = Some(super::level_factory(factory));
        self
    }

    /// Attach a **short stop-loss** level factory. Shape mirrors
    /// [`long_stop_loss`](Self::long_stop_loss); a trailing short stop
    /// composes from `position.trough()`.
    pub fn short_stop_loss<F, L>(mut self, factory: F) -> Self
    where
        F: Fn(&Sym, &Position) -> L + 'static + Send + Sync,
        L: Indicator<Input = Snapshot<Sym>, Output = Real> + 'static + Send + Sync,
    {
        self.short_stop_factory = Some(super::level_factory(factory));
        self
    }

    /// Attach a **short take-profit** level factory. Shape mirrors
    /// [`long_stop_loss`](Self::long_stop_loss).
    pub fn short_take_profit<F, L>(mut self, factory: F) -> Self
    where
        F: Fn(&Sym, &Position) -> L + 'static + Send + Sync,
        L: Indicator<Input = Snapshot<Sym>, Output = Real> + 'static + Send + Sync,
    {
        self.short_target_factory = Some(super::level_factory(factory));
        self
    }

    /// Wire the **per-symbol sizing** factory — the
    /// [`ValueFraction`](crate::Size::ValueFraction) magnitude every
    /// entry / reversal on that symbol is sized against, same semantics
    /// as
    /// [`SingleAssetStrategy::position_sizing`](crate::strategies::SingleAssetStrategy::position_sizing).
    ///
    /// Defaults to a constant `1.0` (all-in per leg). For an N-symbol
    /// equal-weight portfolio at 100% gross, use
    /// `.position_sizing(|_| equal_weight(N))`
    /// ([`sizing::equal_weight`](crate::indicators::sizing::equal_weight)).
    pub fn position_sizing<F, S>(mut self, factory: F) -> Self
    where
        F: Fn(&Sym) -> S + 'static + Send + Sync,
        S: Indicator<Input = Snapshot<Sym>, Output = Real> + 'static + Send + Sync,
    {
        self.sizing_factory = Box::new(move |sym: &Sym| {
            let l: Chain<Sym> = Box::new(factory(sym));
            l
        });
        self
    }

    /// Restrict this strategy to the exact set `symbols` under a
    /// **strict** contract: every listed symbol must appear on every bar
    /// (an absent symbol panics from [`update`](Strategy::update)), and
    /// [`is_ready`](Strategy::is_ready) stays `false` until every listed
    /// symbol has passed its own
    /// `stable_bars`. Non-listed
    /// symbols are filtered out at discovery — no per-symbol state is
    /// built for them.
    ///
    /// Use this when the universe list is authoritative and a missing
    /// symbol means the data feed is broken. For silent skipping, use
    /// [`any_of`](Self::any_of).
    pub fn all_of<I>(self, symbols: I) -> Self
    where
        I: IntoIterator<Item = Sym>,
    {
        self.universe(AllOf(symbols.into_iter().collect()))
    }

    /// Restrict this strategy to the set `symbols` under a **lax**
    /// contract: only listed symbols enter the portfolio, but absent or
    /// still-unready members are silently skipped — same per-bar
    /// filtering the floating universe does, just narrowed to a fixed
    /// list.
    pub fn any_of<I>(self, symbols: I) -> Self
    where
        I: IntoIterator<Item = Sym>,
    {
        self.universe(AnyOf(symbols.into_iter().collect()))
    }

    /// Install a custom [`Universe`] impl — the general seam behind
    /// [`all_of`](Self::all_of) / [`any_of`](Self::any_of). See
    /// [`BasketStrategy::universe`](crate::strategies::BasketStrategy::universe).
    pub fn universe<U>(mut self, universe: U) -> Self
    where
        U: Universe<Sym> + 'static,
    {
        self.universe = Box::new(universe);
        self
    }

    /// A clone of the [`Position`] tracker for `symbol`, if it has been
    /// discovered. Available for read-only inspection — protective-level
    /// factories receive their own `&Position` directly.
    pub fn position(&self, symbol: &Sym) -> Option<Position> {
        self.states.get(symbol).map(|s| s.position.clone())
    }

    /// A clone of the shared [`Book`], for composing book-anchored
    /// sizing against the portfolio's aggregate equity curve.
    pub fn book(&self) -> Book<Sym> {
        self.book.clone()
    }

    /// The largest `stable_bars()` across every currently-discovered
    /// symbol's per-asset chains and the rebalance gate — the number of
    /// bars the driver waits before treating the strategy as ready.
    ///
    /// **Lazy readiness contract.** A multi-asset strategy's per-symbol
    /// chains are built on first sight (see
    /// [`update`](Strategy::update)) — a freshly-constructed strategy
    /// that hasn't seen any snapshot yet has no chains, and this method
    /// reports `0` (only the rebalance signal contributes). To probe
    /// grid-wide readiness (for `optimize --walkforward`'s prefix skip,
    /// or any caller that wants the "worst case across every symbol"
    /// number), feed the strategy one representative snapshot with
    /// [`update`](Strategy::update) first so the per-symbol chains exist,
    /// then read `stable_bars()`.
    pub fn stable_bars(&self) -> usize {
        let mut n = self.rebalance.stable_bars();
        for state in self.states.values() {
            n = n.max(state.stable_bars);
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
        for state in self.states.values() {
            n = n.max(state.warm_up_bars());
        }
        n
    }
}

impl<Sym: Clone + PartialEq + Hash + Eq + 'static + Send + Sync> Default
    for MultiAssetStrategy<Sym>
{
    fn default() -> Self {
        Self::new()
    }
}

// Consumed only by the `spec`-gated `DynMultiAssetStrategy` wrapper.
#[cfg_attr(not(feature = "spec"), allow(dead_code))]
impl<Sym> MultiAssetStrategy<Sym>
where
    Sym: Clone + Hash + Eq + 'static + Send + Sync + serde::Serialize + serde::de::DeserializeOwned,
{
    /// Serialize the portfolio's runtime state for run resuming — the shared
    /// [`Book`] plus each seen symbol's full per-asset chain state, protective
    /// levels, [`Position`], and bar counter.
    pub(crate) fn save_state(&self) -> serde_json::Value {
        let mut symbols: HashMap<Sym, serde_json::Value> = HashMap::new();
        for (sym, st) in self.states.iter() {
            let mut entry = serde_json::Map::new();
            entry.insert("long".into(), st.long.save_state());
            entry.insert("close_long".into(), st.close_long.save_state());
            entry.insert("short".into(), st.short.save_state());
            entry.insert("close_short".into(), st.close_short.save_state());
            entry.insert("sizing".into(), st.sizing.save_state());
            entry.insert("position".into(), st.position.snapshot());
            entry.insert("bars_seen".into(), serde_json::json!(st.bars_seen));
            for (level, key) in [
                (&st.long_stop, "long_stop"),
                (&st.long_target, "long_target"),
                (&st.short_stop, "short_stop"),
                (&st.short_target, "short_target"),
            ] {
                if let Some(c) = level {
                    entry.insert(key.into(), c.save_state());
                }
            }
            symbols.insert(sym.clone(), serde_json::Value::Object(entry));
        }
        serde_json::json!({
            "book": self.book.snapshot_state(),
            // The gate carries a bar counter (`Every`) or arbitrary chain
            // state; without it a resumed run restarts the cadence's phase
            // mid-run and rebalances on different bars than an uninterrupted
            // one would.
            "rebalance": self.rebalance.save_state(),
            "symbols": serde_json::to_value(&symbols).unwrap_or(serde_json::Value::Null),
        })
    }

    /// Restore state produced by [`save_state`](Self::save_state).
    ///
    /// **Eagerly**: every symbol in the blob has its `PerAssetState` built here
    /// and its state loaded here, rather than waiting for the symbol to be seen
    /// again. Three things go wrong if this is deferred to
    /// [`update`](Strategy::update), and all three are silent:
    ///
    /// 1. `backtest::run` routes the wallet's fills through
    ///    [`on_fill`](Strategy::on_fill) *before* `update` on every bar. A
    ///    resumed run's first bar is exactly where the previous run's queued
    ///    order fills — and with no `PerAssetState` built yet, that fill lands
    ///    on the shared [`Book`] but not on the symbol's [`Position`], which
    ///    then disagrees with the book for the rest of the run.
    /// 2. [`save_state`](Self::save_state) can only serialize symbols it holds
    ///    state for, so a symbol that doesn't quote during a resumed chunk
    ///    would have its state dropped at that chunk's save rather than carried.
    /// 3. `restore_state` is the only place with an error path to report a
    ///    malformed blob through; a deferred load can only swallow it.
    ///
    /// Building here is safe for the same reason the lazy path was: a
    /// per-symbol template that builds for one symbol builds for all, which the
    /// spec layer probes once at build time (`spec::multi_asset::probe_signal`).
    pub(crate) fn restore_state(&mut self, state: &serde_json::Value) -> Result<(), String> {
        let obj = state
            .as_object()
            .ok_or_else(|| format!("multi: expected a state object, got {state}"))?;
        if let Some(v) = obj.get("book") {
            self.book
                .restore_state(v)
                .map_err(|e| format!("book > {e}"))?;
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
                if !self.states.contains_key(&sym) {
                    let st = self.build_state(&sym);
                    self.states.insert(sym.clone(), st);
                }
                self.restore_symbol(&sym, &entry)?;
            }
        }
        Ok(())
    }

    /// Load one symbol's saved entry into its already-built `PerAssetState`.
    /// Every field propagates its error with a `symbols[sym] > ` breadcrumb —
    /// a shape mismatch here means the document and the blob disagree, which is
    /// bad input, not something to resume through.
    fn restore_symbol(&mut self, sym: &Sym, entry: &serde_json::Value) -> Result<(), String> {
        let label = serde_json::to_string(sym).unwrap_or_else(|_| "?".into());
        let obj = entry
            .as_object()
            .ok_or_else(|| format!("symbols[{label}]: expected an object, got {entry}"))?;
        let st = self
            .states
            .get_mut(sym)
            .expect("state built immediately above");
        let null = serde_json::Value::Null;
        let at = |field: &str, e: String| format!("symbols[{label}] > {field} > {e}");

        for (chain, key) in [
            (&mut st.long, "long"),
            (&mut st.close_long, "close_long"),
            (&mut st.short, "short"),
            (&mut st.close_short, "close_short"),
        ] {
            chain
                .load_state(obj.get(key).unwrap_or(&null))
                .map_err(|e| at(key, e))?;
        }
        st.sizing
            .load_state(obj.get("sizing").unwrap_or(&null))
            .map_err(|e| at("sizing", e))?;
        for (level, key) in [
            (&mut st.long_stop, "long_stop"),
            (&mut st.long_target, "long_target"),
            (&mut st.short_stop, "short_stop"),
            (&mut st.short_target, "short_target"),
        ] {
            if let (Some(c), Some(v)) = (level.as_mut(), obj.get(key)) {
                c.load_state(v).map_err(|e| at(key, e))?;
            }
        }
        if let Some(v) = obj.get("position") {
            st.position.restore(v).map_err(|e| at("position", e))?;
        }
        st.bars_seen = obj
            .get("bars_seen")
            .and_then(|v| v.as_u64())
            .unwrap_or(st.bars_seen as u64) as usize;
        Ok(())
    }
}

impl<Sym: Clone + Hash + Eq + 'static + Send + Sync> MultiAssetStrategy<Sym> {
    /// Spin up one symbol's chains from the per-symbol factories.
    ///
    /// The single place a `PerAssetState` is born — reached from
    /// [`update`](Strategy::update) on first sight of a symbol, and from
    /// [`restore_state`](Self::restore_state) for every symbol a resumed blob
    /// carries. The protective-level factories take the brand-new
    /// [`Position`] so `position.entry()` / `.peak()` inside a level compose
    /// against the right anchor.
    fn build_state(&self, sym: &Sym) -> PerAssetState<Sym> {
        let position = Position::new();
        let level = |f: &Option<LevelFactory<Sym>>| f.as_ref().map(|f| f(sym, &position));
        let mut state = PerAssetState {
            long: (self.long_factory)(sym),
            close_long: (self.close_long_factory)(sym),
            short: (self.short_factory)(sym),
            close_short: (self.close_short_factory)(sym),
            long_stop: level(&self.long_stop_factory),
            long_target: level(&self.long_target_factory),
            short_stop: level(&self.short_stop_factory),
            short_target: level(&self.short_target_factory),
            sizing: (self.sizing_factory)(sym),
            position,
            bars_seen: 0,
            // Filled in below: the threshold is derived from the chains, so it
            // cannot be computed until they are in place.
            stable_bars: 0,
        };
        state.stable_bars = state.compute_stable_bars();
        state
    }
}

impl<Sym: Clone + PartialEq + Hash + Eq + 'static + Send + Sync> Strategy
    for MultiAssetStrategy<Sym>
{
    type Input = Snapshot<Sym>;
    type Symbol = Sym;

    fn update(&mut self, snap: Snapshot<Sym>) {
        // 0. Universe: strict impls (e.g. `AllOf`) require every listed
        // symbol on every bar. Absence panics — the point of a strict
        // universe is to catch feed gaps and typos loudly. Lax /
        // floating impls report an empty `required()` and this loop
        // is a no-op.
        for sym in self.universe.required() {
            let present = snap.iter().any(|(s, _, _)| s == Some(sym));
            if !present {
                panic!(
                    "MultiAssetStrategy: the installed strict universe \
                     requires every listed symbol to be present in every \
                     snapshot, but at least one is missing this bar. Either \
                     fix the data feed or install a lax universe (`any_of` \
                     / `Floating`) if silent skipping is what you want."
                );
            }
        }

        // 1. Discover new symbols admissible under the universe, build
        //    their per-symbol state lazily. Symbols outside the universe
        //    are silently dropped at discovery — they never get chains.
        let new_syms: Vec<Sym> = snap
            .iter()
            .filter_map(|(sym_opt, _freq, _atom)| {
                sym_opt
                    .filter(|s| self.universe.admits(s))
                    .filter(|s| !self.states.contains_key(s))
                    .cloned()
            })
            .collect();
        for sym in new_syms {
            let state = self.build_state(&sym);
            self.states.insert(sym, state);
        }

        // 2. Advance every known symbol's chains, fold its atom into its
        //    Position, and count the bar.
        //
        // The snapshot's candles are read into a lookup keyed by symbol *once*
        // per bar, before the loop. Finding each leg's own bar with a
        // `snap.iter().find_map(...)` *inside* the loop made the per-bar cost
        // O(N^2) in the universe size — and it cloned the whole `Atom` to read
        // one `Copy` candle out of it. Both showed up as the per-symbol-bar
        // cost climbing with N, which for an independent-legs strategy should
        // be flat.
        self.bar_candles.clear();
        for (s, _, a) in snap.iter() {
            if let (Some(s), Some(c)) = (s, a.candle) {
                self.bar_candles.insert(s.clone(), c);
            }
        }

        let bar_candles = &self.bar_candles;
        for (sym, state) in self.states.iter_mut() {
            if let Some(&candle) = bar_candles.get(sym) {
                state.position.update(candle);
            }

            state.long.update(snap.clone());
            state.close_long.update(snap.clone());
            state.short.update(snap.clone());
            state.close_short.update(snap.clone());
            if let Some(l) = state.long_stop.as_mut() {
                l.update(snap.clone());
            }
            if let Some(l) = state.long_target.as_mut() {
                l.update(snap.clone());
            }
            if let Some(l) = state.short_stop.as_mut() {
                l.update(snap.clone());
            }
            if let Some(l) = state.short_target.as_mut() {
                l.update(snap.clone());
            }
            state.sizing.update(snap.clone());
            state.bars_seen = state.bars_seen.saturating_add(1);
        }

        // 3. Mark the shared Book to market with every tagged symbol's
        //    close in the snapshot. Non-universe symbols contribute a
        //    price that Book::update no-ops on (their leg was never
        //    registered via apply_fill), so it's cheap.
        // Reuses the same per-bar lookup built above — it already holds exactly
        // the tagged, priceable entries (`a.candle` is `None` for an
        // overlay-only series, which is not a price and has nothing to mark).
        if !self.bar_candles.is_empty() {
            self.book
                .update(self.bar_candles.iter().map(|(s, &c)| (s.clone(), c)));
        }

        // 4. Advance the rebalance gate. Reads the same snapshot as the
        // per-symbol chains but only consulted in `trade()`.
        self.rebalance.update(snap);
    }

    fn on_fill(&mut self, order: &Order<Sym>) {
        if let Some(state) = self.states.get(&order.symbol) {
            state.position.apply(order.side, order.units, order.price);
        }
        self.book
            .apply_fill(&order.symbol, order.side, order.units, order.price);
    }

    fn is_ready(&self) -> bool {
        // Floating / any_of: per-symbol readiness is enforced inside
        // trade() by skipping symbols whose own state hasn't settled, so
        // the strategy is always ready to try.
        //
        // all_of: strict — the driver skips trade() until every listed
        // symbol has been discovered and is past its own stable_bars,
        // so the whole portfolio sits through warm-up rather than trading
        // a partial universe.
        self.universe
            .required()
            .iter()
            .all(|s| self.states.get(s).map(|st| st.is_ready()).unwrap_or(false))
    }

    fn trade(&self, wallet: &mut dyn Wallet<Sym>) {
        // The rebalance gate is read once per bar and applied per symbol below.
        let rebalancing = self.rebalance.value().unwrap_or(false);
        for (sym, state) in self.states.iter() {
            // Per-symbol readiness gate — a leg whose own chains haven't
            // settled sits out this bar even under a floating universe.
            if !state.is_ready() {
                continue;
            }
            // A `None` sizing skips this symbol for this bar (safe default).
            let Some(size) = state.sizing.value() else {
                continue;
            };
            super::trade_leg(
                &super::Leg {
                    symbol: sym,
                    size,
                    is_long: state.position.is_long(),
                    is_short: state.position.is_short(),
                    enter_long: state.long.value().unwrap_or(false),
                    enter_short: state.short.value().unwrap_or(false),
                    close_long: state.close_long.value().unwrap_or(false),
                    close_short: state.close_short.value().unwrap_or(false),
                    rebalancing,
                    long_stop: state.long_stop.as_ref().and_then(|l| l.value()),
                    long_target: state.long_target.as_ref().and_then(|l| l.value()),
                    short_stop: state.short_stop.as_ref().and_then(|l| l.value()),
                    short_target: state.short_target.as_ref().and_then(|l| l.value()),
                },
                wallet,
            );
        }
    }

    fn reset(&mut self) {
        self.states.clear();
        self.rebalance.reset();
        self.book.reset();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::indicators::sizing::equal_weight;
    use crate::indicators::{Close, IndicatorExt, Pick, Sma, Value};
    use crate::types::{Atom, Selector};
    use crate::wallet::PaperWallet;

    fn snap(entries: &[(&'static str, Real)]) -> Snapshot<&'static str> {
        let mut s = Snapshot::new();
        for &(sym, close) in entries {
            let atom = Atom::new(Candle::new(close, close, close, close, 0.0));
            s.push(Some(sym), None, atom);
        }
        s
    }

    /// A per-symbol close leaf, rooted through `Pick::matching(by_symbol)`.
    fn close_of(sym: &&'static str) -> Close<Pick<&'static str>> {
        Close::of(Pick::matching(Selector::by_symbol(*sym)))
    }

    /// Drive a strategy + wallet over a bar for the given per-symbol closes.
    fn tick(
        strat: &mut MultiAssetStrategy<&'static str>,
        wallet: &mut PaperWallet<&'static str>,
        entries: &[(&'static str, Real)],
    ) {
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
    }

    // ---------------- Lifecycle -----------------------------------------

    #[test]
    fn lazy_instantiation_on_first_sight() {
        let mut strat: MultiAssetStrategy<&'static str> =
            MultiAssetStrategy::with_initial_equity(1_000.0);
        assert!(strat.position(&"A").is_none());
        strat.update(snap(&[("A", 100.0), ("B", 50.0)]));
        assert!(strat.position(&"A").is_some());
        assert!(strat.position(&"B").is_some());
        // A new symbol later is also lazily built.
        strat.update(snap(&[("A", 101.0), ("B", 51.0), ("C", 200.0)]));
        assert!(strat.position(&"C").is_some());
    }

    // ---------------- Independent per-symbol decision -------------------

    #[test]
    fn each_symbol_decides_from_its_own_close_signal() {
        // Long-only per symbol: enter when close > 50, exit when close < 30.
        // Two symbols priced independently — one enters, the other stays
        // flat, in the same bar.
        let mut strat: MultiAssetStrategy<&'static str> =
            MultiAssetStrategy::with_initial_equity(1_000.0)
                .long_on(
                    |sym: &&'static str| close_of(sym).gt(Value::new(50.0)),
                    |sym: &&'static str| close_of(sym).lt(Value::new(30.0)),
                )
                .position_sizing(|_| equal_weight::<&'static str>(2));
        let mut wallet: PaperWallet<&'static str> = PaperWallet::new(1_000.0);
        // Bar 1: prime. A=100 (long condition true), B=20 (long condition false).
        tick(&mut strat, &mut wallet, &[("A", 100.0), ("B", 20.0)]);
        // Bar 2: fills at open.
        tick(&mut strat, &mut wallet, &[("A", 100.0), ("B", 20.0)]);
        assert!(wallet.position(&"A").amount > 0.0, "A long");
        assert!(
            wallet.position(&"B").amount.abs() < 1e-9,
            "B stays flat — B's own signal didn't fire"
        );
    }

    // ---------------- Universe: all_of / any_of / floating --------------

    #[test]
    fn all_of_restricts_discovery_to_listed_symbols() {
        let mut strat: MultiAssetStrategy<&'static str> =
            MultiAssetStrategy::with_initial_equity(1_000.0)
                .long_on(
                    |sym: &&'static str| close_of(sym).gt(Value::new(0.0)),
                    |sym: &&'static str| close_of(sym).lt(Value::new(0.0)),
                )
                .all_of(["A", "B"]);
        strat.update(snap(&[("A", 100.0), ("B", 50.0), ("C", 200.0)]));
        assert!(strat.position(&"A").is_some());
        assert!(strat.position(&"B").is_some());
        assert!(
            strat.position(&"C").is_none(),
            "C is outside the declared universe"
        );
    }

    #[test]
    #[should_panic(expected = "strict universe requires")]
    fn all_of_panics_when_listed_symbol_absent() {
        let mut strat: MultiAssetStrategy<&'static str> =
            MultiAssetStrategy::with_initial_equity(1_000.0)
                .long_on(
                    |sym: &&'static str| close_of(sym).gt(Value::new(0.0)),
                    |sym: &&'static str| close_of(sym).lt(Value::new(0.0)),
                )
                .all_of(["A", "B"]);
        strat.update(snap(&[("A", 100.0)])); // B missing → panic
    }

    #[test]
    fn all_of_is_ready_gates_on_every_listed_symbol_past_stable_bars() {
        // SMA-3 on close: first two bars unready per symbol. Under all_of,
        // is_ready waits until every listed symbol has passed its own
        // stable_bars.
        let mut strat: MultiAssetStrategy<&'static str> =
            MultiAssetStrategy::with_initial_equity(1_000.0)
                .long_on(
                    |sym: &&'static str| Sma::new(close_of(sym), 3).gt(Value::new(0.0)),
                    |sym: &&'static str| Sma::new(close_of(sym), 3).lt(Value::new(0.0)),
                )
                .all_of(["A", "B"]);
        assert!(!strat.is_ready(), "empty portfolio: not ready under all_of");
        strat.update(snap(&[("A", 100.0), ("B", 50.0)]));
        assert!(!strat.is_ready());
        strat.update(snap(&[("A", 101.0), ("B", 51.0)]));
        assert!(!strat.is_ready());
        strat.update(snap(&[("A", 102.0), ("B", 52.0)]));
        assert!(strat.is_ready(), "both listed have hit their stable_bars");
    }

    #[test]
    fn any_of_ignores_missing_symbols() {
        let mut strat: MultiAssetStrategy<&'static str> =
            MultiAssetStrategy::with_initial_equity(1_000.0)
                .long_on(
                    |sym: &&'static str| close_of(sym).gt(Value::new(0.0)),
                    |sym: &&'static str| close_of(sym).lt(Value::new(0.0)),
                )
                .any_of(["A", "B"]);
        strat.update(snap(&[("A", 100.0)])); // no panic
        assert!(strat.position(&"A").is_some());
        assert!(strat.position(&"B").is_none()); // not seen yet
        assert!(strat.is_ready());
    }

    // ---------------- Protective stop per symbol ------------------------

    #[test]
    fn per_symbol_long_stop_fills_at_the_level() {
        // Buy-and-hold-per-symbol with a 10% fixed stop off entry.
        let mut strat: MultiAssetStrategy<&'static str> =
            MultiAssetStrategy::with_initial_equity(1_000.0)
                .long_on(
                    |_sym: &&'static str| {
                        crate::indicators::ValueBool::<Snapshot<&'static str>>::new(true)
                    },
                    |_sym: &&'static str| {
                        crate::indicators::ValueBool::<Snapshot<&'static str>>::new(false)
                    },
                )
                .position_sizing(|_| Value::<Snapshot<&'static str>>::new(0.5))
                .long_stop_loss(|_sym: &&'static str, pos: &Position| {
                    pos.entry().mul(Value::new(0.90))
                });
        let mut wallet: PaperWallet<&'static str> = PaperWallet::new(1_000.0);
        // Bar 1: signal / queue entry. Bar 2: entry fills at open=100; stop = 90.
        tick(&mut strat, &mut wallet, &[("A", 100.0)]);
        tick(&mut strat, &mut wallet, &[("A", 100.0)]);
        assert!(wallet.position(&"A").amount > 0.0, "A long after fill");
        // Bar 3: crosses through 90 (opens above, low 88).
        let s = snap(&[]);
        let mut s = s;
        s.push(
            Some("A"),
            None,
            Atom::new(Candle::new(95.0, 96.0, 88.0, 89.0, 0.0)),
        );
        for (sym_opt, _f, atom) in s.iter() {
            let sym = sym_opt.copied().unwrap();
            let Some(candle) = atom.candle else { continue };
            for fill in wallet.update(sym, candle) {
                strat.on_fill(&fill);
            }
        }
        strat.update(s);
        strat.trade(&mut wallet);
        // The stop should have fired at 90.
        let last = wallet.orders().last().unwrap();
        assert_eq!(last.side, Side::Sell);
        assert_eq!(last.price, 90.0);
        assert!(wallet.position(&"A").amount.abs() < 1e-9);
    }

    // ---------------- Book tracks aggregate equity ----------------------

    #[test]
    fn book_tracks_aggregate_equity_across_symbols() {
        // Two-symbol always-long portfolio at 25% each = 50% gross.
        let mut strat: MultiAssetStrategy<&'static str> =
            MultiAssetStrategy::with_initial_equity(10_000.0)
                .long_on(
                    |_sym: &&'static str| {
                        crate::indicators::ValueBool::<Snapshot<&'static str>>::new(true)
                    },
                    |_sym: &&'static str| {
                        crate::indicators::ValueBool::<Snapshot<&'static str>>::new(false)
                    },
                )
                .position_sizing(|_| Value::<Snapshot<&'static str>>::new(0.25));
        let book = strat.book();
        let mut wallet: PaperWallet<&'static str> = PaperWallet::new(10_000.0);
        // Bar 1: prime. Bar 2: fill. A@100 → 25 units, B@50 → 50 units.
        tick(&mut strat, &mut wallet, &[("A", 100.0), ("B", 50.0)]);
        tick(&mut strat, &mut wallet, &[("A", 100.0), ("B", 50.0)]);
        // Same-close bar after fill: equity ≈ initial capital.
        assert!(
            (book.equity_value() - 10_000.0).abs() < 1e-6,
            "book equity at fill: {}",
            book.equity_value()
        );
        // Bar 3: A rises to 110, B holds. PnL = 25 * (110 - 100) = 250.
        tick(&mut strat, &mut wallet, &[("A", 110.0), ("B", 50.0)]);
        assert!(
            (book.equity_value() - 10_250.0).abs() < 1e-6,
            "book equity after A gain: {}",
            book.equity_value()
        );
    }

    // ---------------- Reset ----------------------------------------------

    #[test]
    fn reset_clears_symbol_state_but_keeps_universe() {
        let mut strat: MultiAssetStrategy<&'static str> =
            MultiAssetStrategy::with_initial_equity(1_000.0)
                .long_on(
                    |sym: &&'static str| close_of(sym).gt(Value::new(0.0)),
                    |sym: &&'static str| close_of(sym).lt(Value::new(0.0)),
                )
                .all_of(["A", "B"]);
        strat.update(snap(&[("A", 100.0), ("B", 50.0)]));
        assert!(strat.position(&"A").is_some());
        strat.reset();
        assert!(strat.position(&"A").is_none());
        assert_eq!(strat.book().equity_value(), 1_000.0);
        // Universe survives — the strict check still fires on missing B.
        // (Sanity: feeding an incomplete snap now still panics.)
        let panic_result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            strat.update(snap(&[("A", 100.0)]));
        }));
        assert!(
            panic_result.is_err(),
            "universe should survive reset — all_of([A, B]) still expects B"
        );
    }

    // ---------------- Rebalance gate ------------------------------------

    #[test]
    fn default_rebalance_never_resizes_held_positions() {
        // Sizing target drifts over time, but with the default
        // never-rebalance gate, only the entry's size is used and no
        // resize orders fire. Verifies pre-refactor behavior is
        // preserved when `.rebalance_on(...)` isn't called.
        use crate::indicators::ValueBool;
        let mut strat: MultiAssetStrategy<&'static str> =
            MultiAssetStrategy::with_initial_equity(10_000.0)
                .long_on(
                    |_sym: &&'static str| ValueBool::<Snapshot<&'static str>>::new(true),
                    |_sym: &&'static str| ValueBool::<Snapshot<&'static str>>::new(false),
                )
                .position_sizing(|_| Value::<Snapshot<&'static str>>::new(0.25));
        let mut wallet: PaperWallet<&'static str> = PaperWallet::new(10_000.0);
        // Bar 1 signal, Bar 2 fill — entry sized at 0.25.
        tick(&mut strat, &mut wallet, &[("A", 100.0)]);
        tick(&mut strat, &mut wallet, &[("A", 100.0)]);
        assert!(wallet.position(&"A").amount > 0.0);
        let orders_after_entry = wallet.orders().len();
        // Bars 3-5: no rebalance signal → no new orders.
        for _ in 0..3 {
            tick(&mut strat, &mut wallet, &[("A", 100.0)]);
        }
        assert_eq!(
            wallet.orders().len(),
            orders_after_entry,
            "default (!never) rebalance: no mid-position resize"
        );
    }

    #[test]
    fn rebalance_every_bar_holds_position_when_target_size_unchanged() {
        // With `rebalance_on(Every::new(1))` and a constant sizing at
        // steady prices, the resize is idempotent — wallet.set at the
        // same target size / same side just re-affirms the target
        // without changing units.
        use crate::indicators::{Every, ValueBool};
        let mut strat: MultiAssetStrategy<&'static str> =
            MultiAssetStrategy::with_initial_equity(10_000.0)
                .long_on(
                    |_sym: &&'static str| ValueBool::<Snapshot<&'static str>>::new(true),
                    |_sym: &&'static str| ValueBool::<Snapshot<&'static str>>::new(false),
                )
                .position_sizing(|_| Value::<Snapshot<&'static str>>::new(0.5))
                .rebalance_on(Every::<Snapshot<&'static str>>::new(1));
        let mut wallet: PaperWallet<&'static str> = PaperWallet::new(10_000.0);
        tick(&mut strat, &mut wallet, &[("A", 100.0)]);
        tick(&mut strat, &mut wallet, &[("A", 100.0)]);
        let after_entry = wallet.position(&"A").amount;
        assert!(after_entry > 0.0);
        // Several more bars: idempotent resize, no change in units.
        for _ in 0..3 {
            tick(&mut strat, &mut wallet, &[("A", 100.0)]);
        }
        assert!(
            (wallet.position(&"A").amount - after_entry).abs() < 1e-6,
            "same-target resize doesn't move units"
        );
    }

    #[test]
    fn entry_and_exit_signals_still_fire_between_rebalances() {
        // Verify the rebalance gate is orthogonal to the entry / exit
        // signals: even with `rebalance_on(!never)`, an exit signal
        // still flattens the position.
        use crate::indicators::ValueBool;
        let mut strat: MultiAssetStrategy<&'static str> =
            MultiAssetStrategy::with_initial_equity(10_000.0)
                .long_on(
                    |sym: &&'static str| close_of(sym).gt(Value::new(50.0)),
                    |sym: &&'static str| close_of(sym).lt(Value::new(30.0)),
                )
                .position_sizing(|_| Value::<Snapshot<&'static str>>::new(0.5))
                .rebalance_on(ValueBool::<Snapshot<&'static str>>::new(false));
        let mut wallet: PaperWallet<&'static str> = PaperWallet::new(10_000.0);
        tick(&mut strat, &mut wallet, &[("A", 100.0)]);
        tick(&mut strat, &mut wallet, &[("A", 100.0)]);
        assert!(wallet.position(&"A").amount > 0.0, "A long after entry");
        // Price drops through the exit threshold — flatten fires.
        tick(&mut strat, &mut wallet, &[("A", 20.0)]);
        tick(&mut strat, &mut wallet, &[("A", 20.0)]);
        assert!(
            wallet.position(&"A").amount.abs() < 1e-9,
            "A flat after exit signal (unaffected by never-rebalance)"
        );
    }
}
