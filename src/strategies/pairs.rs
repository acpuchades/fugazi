//! [`PairsStrategy`]: a two-leg, spread-driven pair-trading strategy.
//!
//! Same shape as [`SingleAssetStrategy`](crate::strategies::SingleAssetStrategy)
//! at the trait boundary (`Input = Snapshot<Sym>`, boolean signal fields,
//! `Position`-anchored levels) but with two symbols and two signals: **enter**
//! opens a dollar-neutral long-left / short-right pair, **exit** flattens both
//! legs. Positions are sized against equity via
//! [`Size::value_frac(0.5)`](crate::Size) on each leg — a `1.0` gross exposure,
//! half of it on each side.
//!
//! Because the pair's P&L tracks a spread rather than a single instrument's
//! price, protective levels are compared against the *running spread*
//! (`close_left − close_right`) rather than a per-leg price. `stop_loss` fires
//! (flattens both legs) when the spread trades **at or below** its level;
//! `take_profit` fires when the spread trades **at or above** its level.
//! Levels are ordinary indicator expressions built off the strategy's
//! [`Position`] anchor, exactly like [`SingleAssetStrategy`]'s per-leg levels.

use crate::indicators::{Book, Close, ValueBool, Pick, Position, Value};

/// The rebalance-gate signal type — a boolean over the pair's snapshot.
type RebalanceSignal<Sym> = Box<dyn Indicator<Input = Snapshot<Sym>, Output = bool> + Send + Sync>;
use crate::prelude::*;
use crate::types::{Selector, Snapshot};

/// Spread-level source (a real-valued indicator over the pair's `Snapshot`).
type Level<Sym> = Box<dyn Indicator<Input = Snapshot<Sym>, Output = Real> + Send + Sync>;

/// The strategy's internal spread indicator: `close(left) - close(right)`.
type Spread<Sym> = Box<dyn Indicator<Input = Snapshot<Sym>, Output = Real> + Send + Sync>;

/// Fetch a matching atom out of a per-bar [`Snapshot`], for the [`Position`]
/// tracker's own view. Returns `None` on a miss (the pair leg is absent on this
/// bar); the caller then simply skips the position fold for that side.
fn find_atom<Sym: PartialEq + Clone>(snap: &Snapshot<Sym>, symbol: &Sym) -> Option<Atom> {
    let query = Selector::by_symbol(symbol.clone());
    snap.find(&query).cloned()
}

/// The latest value of an optional level, if it is present and warmed up.
fn level_value<Sym>(level: &Option<Level<Sym>>) -> Option<Real> {
    level.as_ref().and_then(|l| l.value())
}

/// A two-symbol, spread-driven pair-trading strategy, long / flat / short **on
/// the spread**.
///
/// The traded instrument is `spread = close(left) − close(right)`, and the two
/// directions are:
///
/// | Direction | Legs | Profits when |
/// | --- | --- | --- |
/// | **long spread** | long `left`, short `right` | the spread rises |
/// | **short spread** | short `left`, long `right` | the spread falls |
///
/// Each leg entries at `value_frac(0.5 * m)` — a 1.0-gross, dollar-neutral pair
/// by default (`m = 1.0`), scaled by the **position-sizing indicator** `m`. An
/// `exit` flattens both legs, as does either **spread** level on the active
/// side.
///
/// ## Why both directions
///
/// A mean-reverting spread visits both tails, and the correct position is
/// *opposite* at each: at a rich spread you short it, at a cheap spread you long
/// it. Which leg does the converging is irrelevant — that is what the hedge
/// removes — but the *sign* is not optional. A one-directional pair therefore
/// skips every excursion on the wrong side of the mean, silently, showing up
/// only as a lower trade count.
///
/// Wiring [`short_spread_on`](Self::short_spread_on) is what enables the second
/// direction; a strategy with only [`long_spread_on`](Self::long_spread_on)
/// wired behaves exactly as before. The two directions are mutually exclusive
/// in time (they are inverse positions — held together they would net flat), so
/// they share one capital pool at full notional rather than splitting it.
///
/// Each side needs its own entry / exit conditions: a signal is an opaque
/// `bool`, so the engine sees `true`, not "the z-score is −2.3", and cannot
/// derive one side's conditions by negating the other's.
///
/// Same shape as [`SingleAssetStrategy`](crate::strategies::SingleAssetStrategy)
/// at the trait boundary: `Input = Snapshot<Sym>`, `Symbol = Sym`, updated one
/// snapshot at a time, levels compose off a shared [`Position`] anchor, and the
/// opposite side's entry reverses an open position.
///
/// ## Position sizing
///
/// The gross exposure of the pair is scaled by a caller-supplied real-valued
/// indicator, set via [`position_sizing`](Self::position_sizing) and defaulting
/// to `Value::new(1.0)` (the classical 1.0-gross, 0.5-per-leg dollar-neutral
/// pair). Each leg entries at `value_frac(0.5 * m)`. `m` is a *magnitude only*
/// (the direction comes from which side's entry fired), and a `None` reading — while
/// the sizing indicator is warming, or on a division by zero — causes the
/// whole [`trade`](Strategy::trade) call to be skipped for that bar (safe
/// default; opt out by composing a well-defined fallback into the sizing
/// expression). The multiplier is read on transitions only; a mid-position
/// change doesn't resize.
///
/// ## Book anchor
///
/// Alongside its two [`Position`] anchors (per-leg) the pair also owns a
/// [`Book`] tracking the *aggregate* equity curve: both legs' fills feed one
/// cash balance, both legs' closes mark the book to market each bar, and a
/// trade closes only when both legs are back to flat. `strat.book()` returns
/// the shared handle so the position-dependent sizing recipes — `drawdown_throttle`,
/// `equity_vol_target`, `fractional_kelly` — work on a pair the same as on a
/// single-asset strategy. Seed the book with
/// [`with_initial_equity`](Self::with_initial_equity) to match the wallet's
/// starting cash for meaningful drawdown/return numbers.
///
/// ## Readiness
///
/// [`is_ready`](Strategy::is_ready) returns `true` once `bars_seen` reaches the
/// largest `stable_period()` across every wired signal, any attached spread
/// level, and the sizing indicator. Because the internal spread is a raw
/// close-of-left − close-of-right expression, its own `stable_period()` is `0`
/// and it never dominates.
///
/// ```
/// use fugazi::prelude::*;
/// use fugazi::indicators::{Close, Pick, Sma, Value};
/// use fugazi::strategies::PairsStrategy;
/// use fugazi::types::Selector;
///
/// // The spread and its 20-bar mean, reused by both directions.
/// let spread = || Close::of(Pick::matching(Selector::by_symbol("BTC")))
///     .sub(Close::of(Pick::matching(Selector::by_symbol("ETH"))));
/// let gap = || spread().sub(Sma::new(spread(), 20));
///
/// let _strat = PairsStrategy::new("BTC", "ETH")
///     // Spread cheap -> long it; close on reversion through the mean.
///     .long_spread_on(gap().below(-1.0), gap().above(0.0))
///     // Spread rich -> short it; close on reversion the other way.
///     .short_spread_on(gap().above(1.0), gap().below(0.0));
/// ```
/// Which direction the pair is currently holding.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Direction {
    /// Long `left`, short `right` — profits as the spread rises.
    LongSpread,
    /// Short `left`, long `right` — profits as the spread falls.
    ShortSpread,
}

impl Direction {
    /// The order sides for `(left, right)` in this direction.
    fn legs(self) -> (Side, Side) {
        match self {
            Direction::LongSpread => (Side::Buy, Side::Sell),
            Direction::ShortSpread => (Side::Sell, Side::Buy),
        }
    }

    fn opposite(self) -> Self {
        match self {
            Direction::LongSpread => Direction::ShortSpread,
            Direction::ShortSpread => Direction::LongSpread,
        }
    }
}

pub struct PairsStrategy<Sym> {
    left: Sym,
    right: Sym,
    long_enter: Box<dyn Signal<Snapshot<Sym>> + Send + Sync>,
    long_exit: Box<dyn Signal<Snapshot<Sym>> + Send + Sync>,
    short_enter: Box<dyn Signal<Snapshot<Sym>> + Send + Sync>,
    short_exit: Box<dyn Signal<Snapshot<Sym>> + Send + Sync>,
    spread: Spread<Sym>,
    long_stop: Option<Level<Sym>>,
    long_target: Option<Level<Sym>>,
    short_stop: Option<Level<Sym>>,
    short_target: Option<Level<Sym>>,
    sizing: Level<Sym>,
    /// Rebalance gate — on `true`, resize both legs to the current
    /// sizing target. Default is `ValueBool::new(false)` (never rebalance),
    /// preserving pre-refactor behavior.
    rebalance: RebalanceSignal<Sym>,
    left_position: Position,
    right_position: Position,
    book: Book<Sym>,
    bars_seen: usize,
}

impl<Sym: Clone + PartialEq + std::hash::Hash + Eq + 'static + Send + Sync> PairsStrategy<Sym> {
    /// A pairs strategy over `left`/`right` with no transitions wired — both
    /// signal slots are constant-`false` and neither spread level is set. Add
    /// the signals with [`on`](Self::on); attach a level with
    /// [`spread_stop_loss`](Self::spread_stop_loss) /
    /// [`spread_take_profit`](Self::spread_take_profit).
    pub fn new(left: Sym, right: Sym) -> Self {
        Self::with_initial_equity(left, right, 1.0)
    }

    /// A pairs strategy seeded with a specific `initial_equity` for its
    /// [`Book`] anchor — match the wallet's starting cash for the
    /// book-anchored sizing recipes (`drawdown_throttle`,
    /// `equity_vol_target`, `fractional_kelly`) to read meaningful
    /// numbers. Otherwise identical to [`new`](Self::new).
    ///
    /// # Panics
    /// Panics if `initial_equity` is not strictly positive.
    pub fn with_initial_equity(left: Sym, right: Sym, initial_equity: Real) -> Self {
        let spread: Spread<Sym> = Box::new(
            Close::of(Pick::matching(Selector::by_symbol(left.clone())))
                .sub(Close::of(Pick::matching(Selector::by_symbol(right.clone())))),
        );
        Self {
            left,
            right,
            long_enter: Box::new(ValueBool::<Snapshot<Sym>>::new(false)),
            long_exit: Box::new(ValueBool::<Snapshot<Sym>>::new(false)),
            short_enter: Box::new(ValueBool::<Snapshot<Sym>>::new(false)),
            short_exit: Box::new(ValueBool::<Snapshot<Sym>>::new(false)),
            spread,
            long_stop: None,
            long_target: None,
            short_stop: None,
            short_target: None,
            sizing: Box::new(Value::<Snapshot<Sym>>::new(1.0)),
            rebalance: Box::new(ValueBool::<Snapshot<Sym>>::new(false)),
            left_position: Position::new(),
            right_position: Position::new(),
            book: Book::new(initial_equity),
            bars_seen: 0,
        }
    }

    /// Install the **rebalance gate** — a boolean signal that decides,
    /// on each bar, whether both legs are resized to the current
    /// sizing target. Defaults to a constant `false` (never rebalance,
    /// matches pre-refactor behavior where sizing only reads on entry).
    ///
    /// A `None` reading is treated as `false` — the safe default.
    pub fn rebalance_on(
        mut self,
        signal: impl Indicator<Input = Snapshot<Sym>, Output = bool> + 'static + Send + Sync,
    ) -> Self {
        self.rebalance = Box::new(signal);
        self
    }

    /// Wire the **long-spread** side: `enter` opens long `left` / short `right`
    /// (profiting as the spread rises), `exit` flattens both legs. Both fire
    /// idempotently — a held `enter` is a no-op while that side is open, and an
    /// `exit` on a flat book is silent.
    ///
    /// If the short-spread side is also wired, its `enter` firing while this
    /// side is open **reverses** the pair, matching
    /// [`SingleAssetStrategy`](crate::strategies::SingleAssetStrategy).
    pub fn long_spread_on(
        mut self,
        enter: impl Signal<Snapshot<Sym>> + 'static + Send + Sync,
        exit: impl Signal<Snapshot<Sym>> + 'static + Send + Sync,
    ) -> Self {
        self.long_enter = Box::new(enter);
        self.long_exit = Box::new(exit);
        self
    }

    /// Wire the **short-spread** side: `enter` opens short `left` / long
    /// `right` (profiting as the spread falls), `exit` flattens both legs.
    ///
    /// Leaving this unwired keeps the strategy long-spread-only, which is the
    /// historical behaviour.
    pub fn short_spread_on(
        mut self,
        enter: impl Signal<Snapshot<Sym>> + 'static + Send + Sync,
        exit: impl Signal<Snapshot<Sym>> + 'static + Send + Sync,
    ) -> Self {
        self.short_enter = Box::new(enter);
        self.short_exit = Box::new(exit);
        self
    }

    /// The long-spread side's alias for [`long_spread_on`](Self::long_spread_on),
    /// kept for callers written before the short side existed.
    pub fn on(
        self,
        enter: impl Signal<Snapshot<Sym>> + 'static + Send + Sync,
        exit: impl Signal<Snapshot<Sym>> + 'static + Send + Sync,
    ) -> Self {
        self.long_spread_on(enter, exit)
    }

    /// Attach the **long-spread stop-loss**: that side flattens when the running
    /// `close(left) − close(right)` reads **at or below** this level. The long
    /// side profits as the spread rises, so falling is its adverse direction.
    pub fn long_spread_stop_loss(
        mut self,
        level: impl Indicator<Input = Snapshot<Sym>, Output = Real> + 'static + Send + Sync,
    ) -> Self {
        self.long_stop = Some(Box::new(level));
        self
    }

    /// Attach the **long-spread take-profit**: that side flattens when the
    /// spread reads **at or above** this level (its favourable direction).
    pub fn long_spread_take_profit(
        mut self,
        level: impl Indicator<Input = Snapshot<Sym>, Output = Real> + 'static + Send + Sync,
    ) -> Self {
        self.long_target = Some(Box::new(level));
        self
    }

    /// Attach the **short-spread stop-loss**. The comparison is mirrored: the
    /// short side profits as the spread falls, so it stops out when the spread
    /// reads **at or above** this level.
    pub fn short_spread_stop_loss(
        mut self,
        level: impl Indicator<Input = Snapshot<Sym>, Output = Real> + 'static + Send + Sync,
    ) -> Self {
        self.short_stop = Some(Box::new(level));
        self
    }

    /// Attach the **short-spread take-profit**: that side flattens when the
    /// spread reads **at or below** this level (its favourable direction).
    pub fn short_spread_take_profit(
        mut self,
        level: impl Indicator<Input = Snapshot<Sym>, Output = Real> + 'static + Send + Sync,
    ) -> Self {
        self.short_target = Some(Box::new(level));
        self
    }

    /// Alias for [`long_spread_stop_loss`](Self::long_spread_stop_loss).
    pub fn spread_stop_loss(
        self,
        level: impl Indicator<Input = Snapshot<Sym>, Output = Real> + 'static + Send + Sync,
    ) -> Self {
        self.long_spread_stop_loss(level)
    }

    /// Alias for [`long_spread_take_profit`](Self::long_spread_take_profit).
    pub fn spread_take_profit(
        self,
        level: impl Indicator<Input = Snapshot<Sym>, Output = Real> + 'static + Send + Sync,
    ) -> Self {
        self.long_spread_take_profit(level)
    }

    /// Set the **position-sizing multiplier** — a real-valued source read on
    /// every entry and scaled into both legs'
    /// [`value_frac`](crate::Size::value_frac) magnitude (each leg entries at
    /// `value_frac(0.5 * m)` for a gross of `m`). The default is a constant
    /// `1.0` (1.0 gross, dollar-neutral). A `None` reading — still warming, or
    /// on a division by zero — causes [`trade`](Strategy::trade) to skip that
    /// bar entirely; wrap with a well-defined fallback at the composition
    /// level to opt out. The multiplier is a magnitude only; direction is
    /// fixed (long left / short right — swap `left` and `right` at construction
    /// for the other side).
    ///
    /// Typical usage: `Value::new(0.5)` for a 0.5-gross pair,
    /// `target_vol.div(spread_realized_vol)` for spread-vol targeting.
    pub fn position_sizing(
        mut self,
        sizing: impl Indicator<Input = Snapshot<Sym>, Output = Real> + 'static + Send + Sync,
    ) -> Self {
        self.sizing = Box::new(sizing);
        self
    }

    /// A clone of the left leg's [`Position`], for building a custom spread
    /// level off its entry price / peak / trough. Note the level is compared
    /// against the *spread*, not the leg's own price.
    pub fn left_position(&self) -> Position {
        self.left_position.clone()
    }

    /// A clone of the right leg's [`Position`], analogously.
    pub fn right_position(&self) -> Position {
        self.right_position.clone()
    }

    /// A clone of this strategy's [`Book`], for building book-anchored
    /// sizing expressions against the pair's *aggregate* equity curve.
    /// The book tracks both legs' fills and marks each leg to market
    /// per bar; per-trade P&L is summed across both legs at pair
    /// close.
    pub fn book(&self) -> Book<Sym> {
        self.book.clone()
    }

    /// The number of bars that must be fed before [`is_ready`](Strategy::is_ready)
    /// reports `true`: the largest `stable_period()` across `enter`, `exit`, any
    /// attached spread level, and the sizing indicator. Same aggregation shape as
    /// [`SingleAssetStrategy::stable_period`](crate::strategies::SingleAssetStrategy::stable_period).
    pub fn stable_period(&self) -> usize {
        let mut needed = self
            .long_enter
            .stable_period()
            .max(self.long_exit.stable_period())
            .max(self.short_enter.stable_period())
            .max(self.short_exit.stable_period());
        for level in [
            &self.long_stop,
            &self.long_target,
            &self.short_stop,
            &self.short_target,
        ]
        .into_iter()
        .flatten()
        {
            needed = needed.max(level.stable_period());
        }
        needed = needed.max(self.sizing.stable_period());
        needed
    }

    /// The largest `warm_up_period()` across every wired signal, attached
    /// spread level, and the sizing indicator — the readiness threshold
    /// *ignoring* IIR settling (matching `optimize --walkforward
    /// --keep-unstable`). Same aggregation shape as
    /// [`stable_period`](Self::stable_period).
    pub fn warm_up_period(&self) -> usize {
        let mut needed = self
            .long_enter
            .warm_up_period()
            .max(self.long_exit.warm_up_period())
            .max(self.short_enter.warm_up_period())
            .max(self.short_exit.warm_up_period());
        for level in [
            &self.long_stop,
            &self.long_target,
            &self.short_stop,
            &self.short_target,
        ]
        .into_iter()
        .flatten()
        {
            needed = needed.max(level.warm_up_period());
        }
        needed = needed.max(self.sizing.warm_up_period());
        needed
    }

    /// Which direction the pair is currently holding, if any. Anything other
    /// than a cleanly-opposed pair of legs (one flat, both same side, a partial
    /// fill) reads as `None` — flat — so the entry path can re-establish it.
    fn open_side(&self) -> Option<Direction> {
        if self.left_position.is_long() && self.right_position.is_short() {
            Some(Direction::LongSpread)
        } else if self.left_position.is_short() && self.right_position.is_long() {
            Some(Direction::ShortSpread)
        } else {
            None
        }
    }

    /// That side's `(enter, exit)` signals.
    fn signals(&self, side: Direction) -> (&dyn Signal<Snapshot<Sym>>, &dyn Signal<Snapshot<Sym>>) {
        match side {
            Direction::LongSpread => (&*self.long_enter, &*self.long_exit),
            Direction::ShortSpread => (&*self.short_enter, &*self.short_exit),
        }
    }

    /// Whether an open position on `side` should flatten on its spread levels.
    ///
    /// The comparisons are mirrored per direction, because "adverse" is a
    /// different sign on each: the long-spread side loses as the spread falls
    /// and profits as it rises, and the short-spread side is the exact
    /// opposite.
    fn level_exit_hit(&self, side: Direction) -> bool {
        let Some(spread) = self.spread.value() else {
            return false;
        };
        let (stop, target) = match side {
            Direction::LongSpread => (&self.long_stop, &self.long_target),
            Direction::ShortSpread => (&self.short_stop, &self.short_target),
        };
        let stop_hit = match (level_value(stop), side) {
            (Some(level), Direction::LongSpread) => spread <= level,
            (Some(level), Direction::ShortSpread) => spread >= level,
            (None, _) => false,
        };
        let target_hit = match (level_value(target), side) {
            (Some(level), Direction::LongSpread) => spread >= level,
            (Some(level), Direction::ShortSpread) => spread <= level,
            (None, _) => false,
        };
        stop_hit || target_hit
    }
}

impl<Sym: Clone + PartialEq + std::hash::Hash + Eq + 'static + Send + Sync> Strategy for PairsStrategy<Sym> {
    type Input = Snapshot<Sym>;
    type Symbol = Sym;

    fn update(&mut self, snap: Snapshot<Sym>) {
        // Collect per-leg atoms for both positions AND the book.
        let left_atom = find_atom(&snap, &self.left);
        let right_atom = find_atom(&snap, &self.right);
        if let Some(candle) = left_atom.as_ref().and_then(|a| a.candle) {
            self.left_position.update(candle);
        }
        if let Some(candle) = right_atom.as_ref().and_then(|a| a.candle) {
            self.right_position.update(candle);
        }
        // Feed the book both legs' closes in one call so aggregate
        // mark-to-market and per-bar return are computed correctly.
        let marks: Vec<(Sym, Candle)> = [
            left_atom.as_ref().and_then(|a| a.candle).map(|c| (self.left.clone(), c)),
            right_atom.as_ref().and_then(|a| a.candle).map(|c| (self.right.clone(), c)),
        ]
        .into_iter()
        .flatten()
        .collect();
        if !marks.is_empty() {
            self.book.update(marks);
        }

        // Every signal and level is advanced each bar regardless of which side
        // is open, so an idle side keeps its own warm-up progressing and reads
        // a properly-settled value the moment it becomes relevant.
        self.long_enter.update(snap.clone());
        self.long_exit.update(snap.clone());
        self.short_enter.update(snap.clone());
        self.short_exit.update(snap.clone());
        self.spread.update(snap.clone());
        for level in [
            self.long_stop.as_mut(),
            self.long_target.as_mut(),
            self.short_stop.as_mut(),
            self.short_target.as_mut(),
        ]
        .into_iter()
        .flatten()
        {
            level.update(snap.clone());
        }
        self.sizing.update(snap.clone());
        self.rebalance.update(snap);
        self.bars_seen = self.bars_seen.saturating_add(1);
    }

    fn is_ready(&self) -> bool {
        self.bars_seen >= self.stable_period()
    }

    fn on_fill(&mut self, order: &Order<Sym>) {
        // Track each leg's position + the aggregate book from the fill stream.
        if order.symbol == self.left {
            self.left_position
                .apply(order.side, order.units, order.price);
            self.book
                .apply_fill(&self.left, order.side, order.units, order.price);
        } else if order.symbol == self.right {
            self.right_position
                .apply(order.side, order.units, order.price);
            self.book
                .apply_fill(&self.right, order.side, order.units, order.price);
        }
    }

    fn trade(&self, wallet: &mut dyn Wallet<Sym>) {
        // Sizing is read once per bar and skips the whole `trade` call when
        // the indicator can't produce a number (safe default per the
        // "unsettled data ⇒ wait" convention).
        let Some(size) = self.sizing.value() else {
            return;
        };
        let open = self.open_side();
        let leg_frac = 0.5 * size;
        // Open both legs on `side` at `value_frac(0.5 * m)` for a
        // dollar-neutral pair with gross `m`. Each leg's size resolves against
        // the same shared equity at the *next* bar's open (each leg is a queued
        // market order in `PaperWallet`), so the two legs' notionals match at
        // the fill. A `set` at the side already held is idempotent; a `set` at
        // the opposite side reverses in one order.
        let open_pair = |wallet: &mut dyn Wallet<Sym>, side: Direction| {
            let (left_side, right_side) = side.legs();
            let _ = wallet.set(self.left.clone(), left_side, Size::value_frac(leg_frac));
            let _ = wallet.set(self.right.clone(), right_side, Size::value_frac(leg_frac));
        };

        if let Some(side) = open {
            // Signal-driven exit takes precedence: an exit fires even on the
            // same bar as an entry re-fires (idempotent open → close → open
            // would be wasteful; the trade layer never opens what the exit just
            // closed).
            let (_, exit) = self.signals(side);
            if exit.is_true() {
                let _ = wallet.close(self.left.clone());
                let _ = wallet.close(self.right.clone());
                return;
            }
            // Spread-level exits on the active side (sign-aware, see
            // `level_exit_hit`).
            if self.level_exit_hit(side) {
                let _ = wallet.close(self.left.clone());
                let _ = wallet.close(self.right.clone());
                return;
            }
            // Reversal: the opposite side's entry flips the pair directly,
            // matching `SingleAssetStrategy`. One `set` per leg does it, since
            // sizing is against equity.
            let (opposite_enter, _) = self.signals(side.opposite());
            if opposite_enter.is_true() {
                open_pair(wallet, side.opposite());
                return;
            }
            // Rebalance gate: on `true`, resize both legs to the current
            // sizing target. `wallet.set` at the current side is idempotent
            // when the target already matches, so no spurious fills.
            if self.rebalance.value().unwrap_or(false) {
                open_pair(wallet, side);
            }
            return;
        }

        // Flat: whichever side's entry fires opens the pair. Long wins if both
        // fire on the same bar — the same tie-break `basket::Selection::pick`
        // uses, and the two are inverse positions so one must be chosen.
        if self.long_enter.is_true() {
            open_pair(wallet, Direction::LongSpread);
        } else if self.short_enter.is_true() {
            open_pair(wallet, Direction::ShortSpread);
        }
    }

    fn reset(&mut self) {
        self.long_enter.reset();
        self.long_exit.reset();
        self.short_enter.reset();
        self.short_exit.reset();
        self.spread.reset();
        for level in [
            self.long_stop.as_mut(),
            self.long_target.as_mut(),
            self.short_stop.as_mut(),
            self.short_target.as_mut(),
        ]
        .into_iter()
        .flatten()
        {
            level.reset();
        }
        self.sizing.reset();
        self.rebalance.reset();
        self.left_position.reset();
        self.right_position.reset();
        self.book.reset();
        self.bars_seen = 0;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::indicators::Value;
    use crate::wallet::PaperWallet;
    use crate::types::Snapshot;

    /// Build a size-2 snapshot with left/right atoms tagged by symbol.
    fn snap(l_price: Real, r_price: Real) -> Snapshot<&'static str> {
        let l = Atom::new(Candle::new(l_price, l_price, l_price, l_price, 0.0));
        let r = Atom::new(Candle::new(r_price, r_price, r_price, r_price, 0.0));
        let mut s = Snapshot::new();
        s.push(Some("L"), None, l);
        s.push(Some("R"), None, r);
        s
    }

    /// Drive `strat` over a pair-price series, feeding each symbol's bar to
    /// the wallet and delivering its fills to the strategy before it updates
    /// and trades.
    fn run(
        strat: &mut PairsStrategy<&'static str>,
        bars: &[(Real, Real)],
    ) -> PaperWallet<&'static str> {
        let mut wallet = PaperWallet::new(1_000.0);
        for &(lp, rp) in bars {
            let s = snap(lp, rp);
            let l_candle = Candle::new(lp, lp, lp, lp, 0.0);
            let r_candle = Candle::new(rp, rp, rp, rp, 0.0);
            for fill in wallet.update("L", l_candle) {
                strat.on_fill(&fill);
            }
            for fill in wallet.update("R", r_candle) {
                strat.on_fill(&fill);
            }
            strat.update(s);
            strat.trade(&mut wallet);
        }
        wallet
    }

    #[test]
    fn enter_opens_both_legs_dollar_neutral() {
        // Constant-true enter, constant-false exit — the first trade bar
        // should open a long on L and a short on R.
        let mut strat = PairsStrategy::new("L", "R").on(
            ValueBool::<Snapshot<&'static str>>::new(true),
            ValueBool::<Snapshot<&'static str>>::new(false),
        );
        // Two bars: first bar signals, second bar fills at the open (both legs).
        let w = run(&mut strat, &[(100.0, 50.0), (100.0, 50.0)]);
        assert_eq!(w.orders().len(), 2, "one fill per leg");
        // Confirm the two legs opened with the intended sides.
        let mut have_long_l = false;
        let mut have_short_r = false;
        for order in w.orders() {
            match order.symbol {
                "L" => {
                    assert_eq!(order.side, Side::Buy);
                    have_long_l = true;
                }
                "R" => {
                    assert_eq!(order.side, Side::Sell);
                    have_short_r = true;
                }
                _ => unreachable!(),
            }
        }
        assert!(have_long_l && have_short_r);
    }

    #[test]
    fn exit_signal_flattens_both_legs() {
        // Enter on bar 0, exit on bar 2 (after the enter fills on bar 1).
        struct Trigger {
            bars: usize,
            at: usize,
            last: Option<bool>,
        }
        impl Indicator for Trigger {
            type Input = Snapshot<&'static str>;
            type Output = bool;
            fn update(&mut self, _snap: Snapshot<&'static str>) -> Option<bool> {
                let out = self.bars == self.at;
                self.bars += 1;
                self.last = Some(out);
                Some(out)
            }
            fn value(&self) -> Option<bool> {
                self.last
            }
            fn warm_up_period(&self) -> usize {
                0
            }
            fn reset(&mut self) {
                self.bars = 0;
                self.last = None;
            }
        }
        // enter fires on bar 0; exit fires on bar 2 (after the enter has filled).
        let mut strat = PairsStrategy::new("L", "R").on(
            Trigger {
                bars: 0,
                at: 0,
                last: None,
            },
            Trigger {
                bars: 0,
                at: 2,
                last: None,
            },
        );
        let w = run(
            &mut strat,
            &[
                (100.0, 50.0), // enter signals; queued
                (100.0, 50.0), // enter fills
                (100.0, 50.0), // exit signals; queued
                (100.0, 50.0), // exit fills
            ],
        );
        assert_eq!(w.orders().len(), 4, "2 open + 2 close fills");
        // Wallet should be flat at the end.
        assert!(w.positions().next().is_none());
    }

    #[test]
    fn spread_stop_loss_flattens_when_spread_drops_below() {
        // Enter → spread = 50 (100-50). Stop at 40; a bar where L=100, R=65
        // gives spread 35, hitting the stop.
        let strat = PairsStrategy::new("L", "R").on(
            ValueBool::<Snapshot<&'static str>>::new(true),
            ValueBool::<Snapshot<&'static str>>::new(false),
        );
        let stop = Value::<Snapshot<&'static str>>::new(40.0);
        let mut strat = strat.spread_stop_loss(stop);
        let w = run(
            &mut strat,
            &[
                (100.0, 50.0), // enter signals
                (100.0, 50.0), // enter fills; spread 50 > stop 40 -> hold
                (100.0, 65.0), // spread now 35, at/below stop -> close both next bar
                (100.0, 65.0), // close fills
            ],
        );
        // 2 opens + 2 closes = 4 orders; flat at the end.
        assert_eq!(w.orders().len(), 4);
        assert!(w.positions().next().is_none());
    }

    #[test]
    fn position_sizing_scales_each_leg() {
        // Half gross (m = 0.5): each leg entries at value_frac(0.25). Seed
        // equity 1000, entry fills at L=100 → L units = 0.25 * 1000 / 100 = 2.5;
        // R=50 → R units = 0.25 * 1000 / 50 = 5.0.
        let mut strat = PairsStrategy::new("L", "R")
            .on(
                ValueBool::<Snapshot<&'static str>>::new(true),
                ValueBool::<Snapshot<&'static str>>::new(false),
            )
            .position_sizing(Value::<Snapshot<&'static str>>::new(0.5));
        let w = run(&mut strat, &[(100.0, 50.0), (100.0, 50.0)]);
        assert_eq!(w.orders().len(), 2);
        for order in w.orders() {
            match order.symbol {
                "L" => {
                    assert_eq!(order.side, Side::Buy);
                    assert_eq!(order.units, 2.5);
                }
                "R" => {
                    assert_eq!(order.side, Side::Sell);
                    assert_eq!(order.units, 5.0);
                }
                _ => unreachable!(),
            }
        }
    }

    #[test]
    fn position_sizing_none_skips_trade() {
        use crate::indicators::Sma;
        // SMA-5 over a constant 0.5 — sizing indicator reads `None` for the
        // first four bars, then `Some(0.5)`. The buy signal is level-true from
        // bar 1, but the entry queues only on bar 5 (first `Some`) and fills at
        // bar 6's open. One entry pair, sized half-gross.
        let sizing = Sma::new(Value::<Snapshot<&'static str>>::new(0.5), 5);
        let mut strat = PairsStrategy::new("L", "R")
            .on(
                ValueBool::<Snapshot<&'static str>>::new(true),
                ValueBool::<Snapshot<&'static str>>::new(false),
            )
            .position_sizing(sizing);
        let w = run(
            &mut strat,
            &[
                (100.0, 50.0),
                (100.0, 50.0),
                (100.0, 50.0),
                (100.0, 50.0),
                (100.0, 50.0),
                (100.0, 50.0),
            ],
        );
        // Exactly 2 fills (L + R entry on bar 6). Every earlier `trade` call
        // returned early because sizing read `None`.
        assert_eq!(w.orders().len(), 2);
    }

    #[test]
    fn position_sizing_warm_up_gates_is_ready() {
        use crate::indicators::Sma;
        // Pair with an SMA-5 sizing indicator: readiness must hold until the
        // SMA has fed five samples.
        let sizing = Sma::new(Value::<Snapshot<&'static str>>::new(0.5), 5);
        let mut strat = PairsStrategy::new("L", "R")
            .on(
                ValueBool::<Snapshot<&'static str>>::new(true),
                ValueBool::<Snapshot<&'static str>>::new(false),
            )
            .position_sizing(sizing);
        assert!(!strat.is_ready());
        for _ in 0..4 {
            strat.update(snap(100.0, 50.0));
        }
        assert!(!strat.is_ready());
        strat.update(snap(100.0, 50.0));
        assert!(strat.is_ready());
    }

    #[test]
    fn spread_take_profit_flattens_when_spread_climbs_above() {
        let strat = PairsStrategy::new("L", "R").on(
            ValueBool::<Snapshot<&'static str>>::new(true),
            ValueBool::<Snapshot<&'static str>>::new(false),
        );
        let target = Value::<Snapshot<&'static str>>::new(60.0);
        let mut strat = strat.spread_take_profit(target);
        let w = run(
            &mut strat,
            &[
                (100.0, 50.0), // signal
                (100.0, 50.0), // fill; spread 50 < target 60 -> hold
                (110.0, 45.0), // spread 65, >= target -> close both next bar
                (110.0, 45.0), // close fills
            ],
        );
        assert_eq!(w.orders().len(), 4);
        assert!(w.positions().next().is_none());
    }

    /// Drive the pair with an explicit initial-equity book seed.
    fn run_with_capital(
        strat: &mut PairsStrategy<&'static str>,
        bars: &[(Real, Real)],
        initial_cash: Real,
    ) -> PaperWallet<&'static str> {
        let mut wallet = PaperWallet::new(initial_cash);
        for &(lp, rp) in bars {
            let s = snap(lp, rp);
            let l_candle = Candle::new(lp, lp, lp, lp, 0.0);
            let r_candle = Candle::new(rp, rp, rp, rp, 0.0);
            for fill in wallet.update("L", l_candle) {
                strat.on_fill(&fill);
            }
            for fill in wallet.update("R", r_candle) {
                strat.on_fill(&fill);
            }
            strat.update(s);
            strat.trade(&mut wallet);
        }
        wallet
    }

    #[test]
    fn book_marks_the_pair_equity_curve() {
        // Enter bar 0, hold. Book equity should track cash + L*close_L +
        // R_units*close_R (short right → negative units).
        let mut strat = PairsStrategy::with_initial_equity("L", "R", 10_000.0).on(
            ValueBool::<Snapshot<&'static str>>::new(true),
            ValueBool::<Snapshot<&'static str>>::new(false),
        );
        let book = strat.book();
        let _ = run_with_capital(
            &mut strat,
            &[
                (100.0, 50.0), // enter signals, queued
                (100.0, 50.0), // fill: L Buy, R Sell — 0.5*equity/price each
                (110.0, 50.0), // L up 10%, R unchanged
            ],
            10_000.0,
        );
        // At bar 2 fill: value_frac(0.5) at price 100 for L → 50 units.
        //                  value_frac(0.5) at price  50 for R → 100 units short.
        // Cash after fills = 10000 - 5000 (bought L) + 5000 (sold R) = 10000.
        // Equity at bar 2 close (fill bar) = 10000 + 50*100 + (-100)*50 = 10000. ✓
        // Equity at bar 3 close = 10000 + 50*110 + (-100)*50 = 10500.
        assert!(
            (book.equity_value() - 10_500.0).abs() < 1e-6,
            "book equity {}",
            book.equity_value()
        );
    }

    #[test]
    fn book_reports_pair_trade_close_pnl() {
        // Enter bar 0, exit bar 2. Bar 3 both legs close at the fill.
        // Aggregate trade P&L should sum both legs' contributions.
        struct At {
            bar: usize,
            fire: usize,
            last: Option<bool>,
        }
        impl Indicator for At {
            type Input = Snapshot<&'static str>;
            type Output = bool;
            fn update(&mut self, _s: Snapshot<&'static str>) -> Option<bool> {
                let out = self.bar == self.fire;
                self.bar += 1;
                self.last = Some(out);
                Some(out)
            }
            fn value(&self) -> Option<bool> {
                self.last
            }
            fn warm_up_period(&self) -> usize {
                0
            }
            fn reset(&mut self) {
                self.bar = 0;
                self.last = None;
            }
        }
        let mut strat = PairsStrategy::with_initial_equity("L", "R", 10_000.0).on(
            At {
                bar: 0,
                fire: 0,
                last: None,
            },
            At {
                bar: 0,
                fire: 2,
                last: None,
            },
        );
        let book = strat.book();
        let _ = run_with_capital(
            &mut strat,
            &[
                (100.0, 50.0), // enter signals; queued
                (100.0, 50.0), // fill open: L Buy 50u @100, R Sell 100u @50
                (110.0, 45.0), // exit signals; queued; L rose, R fell
                (110.0, 45.0), // fill close: L Sell 50u @110, R Buy 100u @45
            ],
            10_000.0,
        );
        // L P&L: 50 * (110 - 100) = 500.
        // R P&L: -100 * (45 - 50) = 500.
        // Aggregate = 1000.
        let pnl = book.trade_pnl::<Snapshot<&'static str>>().value();
        assert!(pnl.is_some(), "no trade close event booked");
        assert!(
            (pnl.unwrap() - 1_000.0).abs() < 1e-6,
            "expected 1000, got {}",
            pnl.unwrap()
        );
    }
}
