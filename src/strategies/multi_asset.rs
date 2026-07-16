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
//! Uses the same [`Universe`] knob as
//! [`BasketStrategy`](crate::strategies::BasketStrategy) — declare
//! [`all_of`](Self::all_of) (strict) or [`any_of`](Self::any_of) (lax),
//! or leave the default [`Universe::Floating`] to pick up every symbol
//! the snapshot carries.

use std::collections::HashMap;
use std::hash::Hash;

use crate::indicators::{Book, Const, Position, Value};
use crate::prelude::*;
use crate::strategies::basket::Universe;
use crate::types::Snapshot;

// ---------------------------------------------------------------------------
// Chain type aliases
// ---------------------------------------------------------------------------

/// A per-symbol boolean chain — one of the four signal slots.
type SignalChain<Sym> = Box<dyn Indicator<Input = Snapshot<Sym>, Output = bool>>;

/// A per-symbol real chain — the sizing multiplier and each of the four
/// protective levels.
type LevelChain<Sym> = Box<dyn Indicator<Input = Snapshot<Sym>, Output = Real>>;

/// A per-symbol signal factory: `Fn(&Sym) -> SignalChain<Sym>`.
type SignalFactory<Sym> = Box<dyn Fn(&Sym) -> SignalChain<Sym>>;

/// A per-symbol level factory that receives the per-symbol
/// [`Position`] so `position.entry()` / `.peak()` / `.trough()` inside
/// the chain resolves against the strategy's actual entry for that
/// symbol.
type LevelFactory<Sym> = Box<dyn Fn(&Sym, &Position) -> LevelChain<Sym>>;

/// A per-symbol sizing factory: `Fn(&Sym) -> LevelChain<Sym>`. The sizing
/// slot doesn't take a [`Position`] because a size that reads back the
/// entry price for its own leg is unusual — most sizing recipes are
/// symbol-agnostic magnitudes (equal weight, ATR risk, drawdown throttle
/// on the shared [`Book`]).
type SizingFactory<Sym> = Box<dyn Fn(&Sym) -> LevelChain<Sym>>;

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
    long_stop: Option<LevelChain<Sym>>,
    long_target: Option<LevelChain<Sym>>,
    short_stop: Option<LevelChain<Sym>>,
    short_target: Option<LevelChain<Sym>>,
    sizing: LevelChain<Sym>,
    position: Position,
    bars_seen: usize,
}

impl<Sym> PerAssetState<Sym> {
    /// Largest `stable_period()` across this symbol's four signals, four
    /// (optional) protective levels, and sizing — same aggregation as
    /// [`SingleAssetStrategy::stable_period`](crate::strategies::SingleAssetStrategy::stable_period),
    /// applied per leg.
    fn stable_period(&self) -> usize {
        let mut n = self.long.stable_period();
        n = n.max(self.close_long.stable_period());
        n = n.max(self.short.stable_period());
        n = n.max(self.close_short.stable_period());
        for level in [
            &self.long_stop,
            &self.long_target,
            &self.short_stop,
            &self.short_target,
        ]
        .into_iter()
        .flatten()
        {
            n = n.max(level.stable_period());
        }
        n.max(self.sizing.stable_period())
    }

    /// Whether this leg has seen enough bars for its own decision to be
    /// safe to act on. Consulted at trade time; also folded into the
    /// [`MultiAssetStrategy::is_ready`] gate under
    /// [`Universe::AllOf`](crate::strategies::basket::Universe::AllOf).
    fn is_ready(&self) -> bool {
        self.bars_seen >= self.stable_period()
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
/// [`SingleAssetStrategy`]: sizing skip on `None`, entry / reversal,
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
/// [`SingleAssetStrategy`].
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
/// [`stable_period`](PerAssetState::stable_period) so the driver skips
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
    states: HashMap<Sym, PerAssetState<Sym>>,
    universe: Universe<Sym>,
    book: Book<Sym>,
}

impl<Sym: Clone + PartialEq + Hash + Eq + 'static> MultiAssetStrategy<Sym> {
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
                let s: SignalChain<Sym> = Box::new(Const::<Snapshot<Sym>>::new(false));
                s
            }),
            close_long_factory: Box::new(|_sym: &Sym| {
                let s: SignalChain<Sym> = Box::new(Const::<Snapshot<Sym>>::new(false));
                s
            }),
            short_factory: Box::new(|_sym: &Sym| {
                let s: SignalChain<Sym> = Box::new(Const::<Snapshot<Sym>>::new(false));
                s
            }),
            close_short_factory: Box::new(|_sym: &Sym| {
                let s: SignalChain<Sym> = Box::new(Const::<Snapshot<Sym>>::new(false));
                s
            }),
            long_stop_factory: None,
            long_target_factory: None,
            short_stop_factory: None,
            short_target_factory: None,
            sizing_factory: Box::new(|_sym: &Sym| {
                let s: LevelChain<Sym> = Box::new(Value::<Snapshot<Sym>>::new(1.0));
                s
            }),
            states: HashMap::new(),
            universe: Universe::Floating,
            book: Book::new(initial_equity),
        }
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
        FE: Fn(&Sym) -> E + 'static,
        FX: Fn(&Sym) -> X + 'static,
        E: Indicator<Input = Snapshot<Sym>, Output = bool> + 'static,
        X: Indicator<Input = Snapshot<Sym>, Output = bool> + 'static,
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
        FE: Fn(&Sym) -> E + 'static,
        FX: Fn(&Sym) -> X + 'static,
        E: Indicator<Input = Snapshot<Sym>, Output = bool> + 'static,
        X: Indicator<Input = Snapshot<Sym>, Output = bool> + 'static,
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
        F: Fn(&Sym, &Position) -> L + 'static,
        L: Indicator<Input = Snapshot<Sym>, Output = Real> + 'static,
    {
        self.long_stop_factory = Some(Box::new(move |sym: &Sym, pos: &Position| {
            let l: LevelChain<Sym> = Box::new(factory(sym, pos));
            l
        }));
        self
    }

    /// Attach a **long take-profit** level factory. Shape mirrors
    /// [`long_stop_loss`](Self::long_stop_loss).
    pub fn long_take_profit<F, L>(mut self, factory: F) -> Self
    where
        F: Fn(&Sym, &Position) -> L + 'static,
        L: Indicator<Input = Snapshot<Sym>, Output = Real> + 'static,
    {
        self.long_target_factory = Some(Box::new(move |sym: &Sym, pos: &Position| {
            let l: LevelChain<Sym> = Box::new(factory(sym, pos));
            l
        }));
        self
    }

    /// Attach a **short stop-loss** level factory. Shape mirrors
    /// [`long_stop_loss`](Self::long_stop_loss); a trailing short stop
    /// composes from `position.trough()`.
    pub fn short_stop_loss<F, L>(mut self, factory: F) -> Self
    where
        F: Fn(&Sym, &Position) -> L + 'static,
        L: Indicator<Input = Snapshot<Sym>, Output = Real> + 'static,
    {
        self.short_stop_factory = Some(Box::new(move |sym: &Sym, pos: &Position| {
            let l: LevelChain<Sym> = Box::new(factory(sym, pos));
            l
        }));
        self
    }

    /// Attach a **short take-profit** level factory. Shape mirrors
    /// [`long_stop_loss`](Self::long_stop_loss).
    pub fn short_take_profit<F, L>(mut self, factory: F) -> Self
    where
        F: Fn(&Sym, &Position) -> L + 'static,
        L: Indicator<Input = Snapshot<Sym>, Output = Real> + 'static,
    {
        self.short_target_factory = Some(Box::new(move |sym: &Sym, pos: &Position| {
            let l: LevelChain<Sym> = Box::new(factory(sym, pos));
            l
        }));
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
        F: Fn(&Sym) -> S + 'static,
        S: Indicator<Input = Snapshot<Sym>, Output = Real> + 'static,
    {
        self.sizing_factory = Box::new(move |sym: &Sym| {
            let l: LevelChain<Sym> = Box::new(factory(sym));
            l
        });
        self
    }

    /// Restrict this strategy to the exact set `symbols` under a
    /// **strict** contract: every listed symbol must appear on every bar
    /// (an absent symbol panics from [`update`](Strategy::update)), and
    /// [`is_ready`](Strategy::is_ready) stays `false` until every listed
    /// symbol has passed its own
    /// [`stable_period`](PerAssetState::stable_period). Non-listed
    /// symbols are filtered out at discovery — no per-symbol state is
    /// built for them.
    ///
    /// Use this when the universe list is authoritative and a missing
    /// symbol means the data feed is broken. For silent skipping, use
    /// [`any_of`](Self::any_of).
    pub fn all_of<I>(mut self, symbols: I) -> Self
    where
        I: IntoIterator<Item = Sym>,
    {
        self.universe = Universe::AllOf(symbols.into_iter().collect());
        self
    }

    /// Restrict this strategy to the set `symbols` under a **lax**
    /// contract: only listed symbols enter the portfolio, but absent or
    /// still-unready members are silently skipped — same per-bar
    /// filtering the floating universe does, just narrowed to a fixed
    /// list.
    pub fn any_of<I>(mut self, symbols: I) -> Self
    where
        I: IntoIterator<Item = Sym>,
    {
        self.universe = Universe::AnyOf(symbols.into_iter().collect());
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
}

impl<Sym: Clone + PartialEq + Hash + Eq + 'static> Default for MultiAssetStrategy<Sym> {
    fn default() -> Self {
        Self::new()
    }
}

impl<Sym: Clone + PartialEq + Hash + Eq + 'static> Strategy for MultiAssetStrategy<Sym> {
    type Input = Snapshot<Sym>;
    type Symbol = Sym;

    fn update(&mut self, snap: Snapshot<Sym>) {
        // 0. Universe: !all_of requires every listed symbol on every bar.
        if let Some(required) = self.universe.required() {
            for sym in required {
                let present = snap.iter().any(|(s, _, _)| s == Some(sym));
                if !present {
                    panic!(
                        "MultiAssetStrategy: `all_of` universe requires every \
                         listed symbol to be present in every snapshot, but \
                         at least one is missing this bar. Either fix the \
                         data feed or switch to `any_of` if silent skipping \
                         is what you want."
                    );
                }
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
            let position = Position::new();
            let long = (self.long_factory)(&sym);
            let close_long = (self.close_long_factory)(&sym);
            let short = (self.short_factory)(&sym);
            let close_short = (self.close_short_factory)(&sym);
            let long_stop = self
                .long_stop_factory
                .as_ref()
                .map(|f| f(&sym, &position));
            let long_target = self
                .long_target_factory
                .as_ref()
                .map(|f| f(&sym, &position));
            let short_stop = self
                .short_stop_factory
                .as_ref()
                .map(|f| f(&sym, &position));
            let short_target = self
                .short_target_factory
                .as_ref()
                .map(|f| f(&sym, &position));
            let sizing = (self.sizing_factory)(&sym);
            self.states.insert(
                sym,
                PerAssetState {
                    long,
                    close_long,
                    short,
                    close_short,
                    long_stop,
                    long_target,
                    short_stop,
                    short_target,
                    sizing,
                    position,
                    bars_seen: 0,
                },
            );
        }

        // 2. Advance every known symbol's chains, fold its atom into its
        //    Position, and count the bar.
        for (sym, state) in self.states.iter_mut() {
            let self_atom = snap.iter().find_map(|(s, _, a)| {
                if s == Some(sym) { Some(a.clone()) } else { None }
            });
            if let Some(atom) = self_atom {
                state.position.update(atom.candle);
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
        let marks: Vec<(Sym, Candle)> = snap
            .iter()
            .filter_map(|(s, _, a)| s.cloned().map(|sym| (sym, a.candle)))
            .collect();
        if !marks.is_empty() {
            self.book.update(marks);
        }
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
        // symbol has been discovered and is past its own stable_period,
        // so the whole portfolio sits through warm-up rather than trading
        // a partial universe.
        match &self.universe {
            Universe::AllOf(required) => required.iter().all(|s| {
                self.states.get(s).map(|st| st.is_ready()).unwrap_or(false)
            }),
            _ => true,
        }
    }

    fn trade(&self, wallet: &mut dyn Wallet<Sym>) {
        for (sym, state) in self.states.iter() {
            // Per-symbol readiness gate — a leg whose own chains haven't
            // settled sits out this bar even under a floating universe.
            if !state.is_ready() {
                continue;
            }
            // Sizing is read once per bar per symbol; a None reading
            // skips this symbol's trade this bar (safe default).
            let Some(size) = state.sizing.value() else {
                continue;
            };
            let long = state.position.is_long();
            let short = state.position.is_short();

            // Entries first (magnitude = sizing, reversal-capable). Cancel
            // any resting bracket on entry / reversal.
            if state.long.value().unwrap_or(false) && !long {
                let _ = wallet.set(sym.clone(), Side::Buy, Size::value_frac(size));
                let _ = wallet.cancel_protective(sym);
                continue;
            }
            if state.short.value().unwrap_or(false) && !short {
                let _ = wallet.set(sym.clone(), Side::Sell, Size::value_frac(size));
                let _ = wallet.cancel_protective(sym);
                continue;
            }
            // Signal-driven flatten-to-flat exits.
            let close_long = state.close_long.value().unwrap_or(false) && long;
            let close_short = state.close_short.value().unwrap_or(false) && short;
            if close_long || close_short {
                let _ = wallet.close(sym.clone());
                let _ = wallet.cancel_protective(sym);
                continue;
            }
            // Rest the active side's protective levels — re-submitted
            // every bar so a trailing level cancel/replaces.
            if long {
                if let Some(level) = state.long_stop.as_ref().and_then(|l| l.value()) {
                    let _ = wallet.set_stop(sym.clone(), Reference(level));
                }
                if let Some(level) = state.long_target.as_ref().and_then(|l| l.value()) {
                    let _ = wallet.set_take_profit(sym.clone(), Reference(level));
                }
            } else if short {
                if let Some(level) = state.short_stop.as_ref().and_then(|l| l.value()) {
                    let _ = wallet.set_stop(sym.clone(), Reference(level));
                }
                if let Some(level) = state.short_target.as_ref().and_then(|l| l.value()) {
                    let _ = wallet.set_take_profit(sym.clone(), Reference(level));
                }
            }
        }
    }

    fn reset(&mut self) {
        self.states.clear();
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
            for fill in wallet.update(sym, atom.candle) {
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
    #[should_panic(expected = "`all_of` universe requires")]
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
    fn all_of_is_ready_gates_on_every_listed_symbol_past_stable_period() {
        // SMA-3 on close: first two bars unready per symbol. Under all_of,
        // is_ready waits until every listed symbol has passed its own
        // stable_period.
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
        assert!(strat.is_ready(), "both listed have hit their stable_period");
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
                    |_sym: &&'static str| crate::indicators::Const::<Snapshot<&'static str>>::new(true),
                    |_sym: &&'static str| crate::indicators::Const::<Snapshot<&'static str>>::new(false),
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
        s.push(Some("A"), None, Atom::new(Candle::new(95.0, 96.0, 88.0, 89.0, 0.0)));
        for (sym_opt, _f, atom) in s.iter() {
            let sym = sym_opt.copied().unwrap();
            for fill in wallet.update(sym, atom.candle) {
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
                    |_sym: &&'static str| crate::indicators::Const::<Snapshot<&'static str>>::new(true),
                    |_sym: &&'static str| crate::indicators::Const::<Snapshot<&'static str>>::new(false),
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
}
