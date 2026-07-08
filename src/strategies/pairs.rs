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

use crate::indicators::{Close, Const, Pick, Position};
use crate::prelude::*;
use crate::types::{Selector, Snapshot};

/// Spread-level source (a real-valued indicator over the pair's `Snapshot`).
type Level<Sym> = Box<dyn Indicator<Input = Snapshot<Sym>, Output = Real>>;

/// The strategy's internal spread indicator: `close(left) - close(right)`.
type Spread<Sym> = Box<dyn Indicator<Input = Snapshot<Sym>, Output = Real>>;

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

/// A two-symbol, spread-driven pair-trading strategy. On **enter**, go long
/// `left` and short `right` at `value_frac(0.5)` each — 1.0 gross exposure,
/// dollar-neutral by construction. On **exit**, flatten both legs. Optional
/// **spread** stop-loss / take-profit levels are compared against the running
/// `close_left − close_right`; either firing flattens the whole pair.
///
/// Same shape as [`SingleAssetStrategy`](crate::strategies::SingleAssetStrategy)
/// at the trait boundary: `Input = Snapshot<Sym>`, `Symbol = Sym`, updated one
/// snapshot at a time, and levels compose off a shared [`Position`] anchor —
/// but with two signals instead of four, since "short the pair" is just
/// long/short swapped and can be obtained by swapping `left`/`right` at
/// construction rather than doubling the signal surface.
///
/// ## Readiness
///
/// [`is_ready`](Strategy::is_ready) returns `true` once `bars_seen` reaches the
/// largest `stable_period()` across the wired `enter`/`exit` signals and any
/// attached spread level. Because the internal spread is a raw close-of-left −
/// close-of-right expression, its own `stable_period()` is `0` and it never
/// dominates.
///
/// ```
/// use fugazi::prelude::*;
/// use fugazi::indicators::{Close, Pick, Sma, Value};
/// use fugazi::strategies::PairsStrategy;
/// use fugazi::types::Selector;
///
/// // Enter when a spread-vs-MA gap exceeds a threshold; exit when it closes.
/// let spread = || Close::of(Pick::matching(Selector::by_symbol("BTC")))
///     .sub(Close::of(Pick::matching(Selector::by_symbol("ETH"))));
/// let enter = spread().sub(Sma::new(spread(), 20)).below(-1.0);
/// let exit = spread().sub(Sma::new(spread(), 20)).above(0.0);
/// let _strat = PairsStrategy::new("BTC", "ETH").on(enter, exit);
/// ```
pub struct PairsStrategy<Sym> {
    left: Sym,
    right: Sym,
    enter: Box<dyn Signal<Snapshot<Sym>>>,
    exit: Box<dyn Signal<Snapshot<Sym>>>,
    spread: Spread<Sym>,
    stop: Option<Level<Sym>>,
    target: Option<Level<Sym>>,
    left_position: Position,
    right_position: Position,
    bars_seen: usize,
}

impl<Sym: Clone + PartialEq + 'static> PairsStrategy<Sym> {
    /// A pairs strategy over `left`/`right` with no transitions wired — both
    /// signal slots are constant-`false` and neither spread level is set. Add
    /// the signals with [`on`](Self::on); attach a level with
    /// [`spread_stop_loss`](Self::spread_stop_loss) /
    /// [`spread_take_profit`](Self::spread_take_profit).
    pub fn new(left: Sym, right: Sym) -> Self {
        let spread: Spread<Sym> = Box::new(
            Close::of(Pick::matching(Selector::by_symbol(left.clone())))
                .sub(Close::of(Pick::matching(Selector::by_symbol(right.clone())))),
        );
        Self {
            left,
            right,
            enter: Box::new(Const::<Snapshot<Sym>>::new(false)),
            exit: Box::new(Const::<Snapshot<Sym>>::new(false)),
            spread,
            stop: None,
            target: None,
            left_position: Position::new(),
            right_position: Position::new(),
            bars_seen: 0,
        }
    }

    /// Wire the pair's `enter` and `exit` signals. `enter` opens a long-left /
    /// short-right pair; `exit` flattens both legs. Both fire idempotently: a
    /// held `enter` reads as no-op while the pair is open, and an `exit` on a
    /// flat book is likewise silent.
    pub fn on(
        mut self,
        enter: impl Signal<Snapshot<Sym>> + 'static,
        exit: impl Signal<Snapshot<Sym>> + 'static,
    ) -> Self {
        self.enter = Box::new(enter);
        self.exit = Box::new(exit);
        self
    }

    /// Attach a **spread stop-loss**: the pair flattens when the running
    /// `close(left) − close(right)` reads **at or below** this level. Since the
    /// pair is long the spread by construction, this is the adverse side.
    pub fn spread_stop_loss(
        mut self,
        level: impl Indicator<Input = Snapshot<Sym>, Output = Real> + 'static,
    ) -> Self {
        self.stop = Some(Box::new(level));
        self
    }

    /// Attach a **spread take-profit**: the pair flattens when the running
    /// spread reads **at or above** this level (the favourable side).
    pub fn spread_take_profit(
        mut self,
        level: impl Indicator<Input = Snapshot<Sym>, Output = Real> + 'static,
    ) -> Self {
        self.target = Some(Box::new(level));
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

    /// The number of bars that must be fed before [`is_ready`](Strategy::is_ready)
    /// reports `true`: the largest `stable_period()` across `enter`, `exit`, and
    /// any attached spread level.
    fn readiness_threshold(&self) -> usize {
        let mut needed = self.enter.stable_period().max(self.exit.stable_period());
        for level in [&self.stop, &self.target].into_iter().flatten() {
            needed = needed.max(level.stable_period());
        }
        needed
    }

    /// Whether the pair is currently open (both legs held on the intended sides).
    fn is_open(&self) -> bool {
        self.left_position.is_long() && self.right_position.is_short()
    }
}

impl<Sym: Clone + PartialEq + 'static> Strategy for PairsStrategy<Sym> {
    type Input = Snapshot<Sym>;
    type Symbol = Sym;

    fn update(&mut self, snap: Snapshot<Sym>) {
        if let Some(atom) = find_atom(&snap, &self.left) {
            self.left_position.update(atom.candle);
        }
        if let Some(atom) = find_atom(&snap, &self.right) {
            self.right_position.update(atom.candle);
        }

        self.enter.update(snap.clone());
        self.exit.update(snap.clone());
        self.spread.update(snap.clone());
        if let Some(l) = self.stop.as_mut() {
            l.update(snap.clone());
        }
        if let Some(l) = self.target.as_mut() {
            l.update(snap);
        }
        self.bars_seen = self.bars_seen.saturating_add(1);
    }

    fn is_ready(&self) -> bool {
        self.bars_seen >= self.readiness_threshold()
    }

    fn on_fill(&mut self, order: &Order<Sym>) {
        // Track each leg's position independently from the fill stream.
        if order.symbol == self.left {
            self.left_position
                .apply(order.side, order.units, order.price);
        } else if order.symbol == self.right {
            self.right_position
                .apply(order.side, order.units, order.price);
        }
    }

    fn trade(&self, wallet: &mut dyn Wallet<Sym>) {
        let open = self.is_open();
        // Signal-driven exit takes precedence: an exit fires even on the same
        // bar as an entry re-fires (idempotent open → close → open would be
        // wasteful; the trade layer never opens what the exit just closed).
        if open && self.exit.is_true() {
            let _ = wallet.close(self.left.clone());
            let _ = wallet.close(self.right.clone());
            return;
        }
        // Spread-level exits (compared against the running spread indicator).
        if open {
            let spread = self.spread.value();
            let stop_hit = matches!(
                (spread, level_value(&self.stop)),
                (Some(s), Some(lvl)) if s <= lvl,
            );
            let target_hit = matches!(
                (spread, level_value(&self.target)),
                (Some(s), Some(lvl)) if s >= lvl,
            );
            if stop_hit || target_hit {
                let _ = wallet.close(self.left.clone());
                let _ = wallet.close(self.right.clone());
                return;
            }
        }
        // Entry: open both legs at `value_frac(0.5)` for a dollar-neutral
        // 1.0 gross exposure. Each leg's size resolves against the same
        // shared equity at the *next* bar's open (each leg is a queued
        // market order in `PaperWallet`), so the two legs' notionals match
        // at the fill.
        if !open && self.enter.is_true() {
            let _ = wallet.set(self.left.clone(), Side::Buy, Size::value_frac(0.5));
            let _ = wallet.set(self.right.clone(), Side::Sell, Size::value_frac(0.5));
        }
    }

    fn reset(&mut self) {
        self.enter.reset();
        self.exit.reset();
        self.spread.reset();
        if let Some(l) = self.stop.as_mut() {
            l.reset();
        }
        if let Some(l) = self.target.as_mut() {
            l.reset();
        }
        self.left_position.reset();
        self.right_position.reset();
        self.bars_seen = 0;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::indicators::Value;
    use crate::strategy::PaperWallet;
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
            Const::<Snapshot<&'static str>>::new(true),
            Const::<Snapshot<&'static str>>::new(false),
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
            Const::<Snapshot<&'static str>>::new(true),
            Const::<Snapshot<&'static str>>::new(false),
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
    fn spread_take_profit_flattens_when_spread_climbs_above() {
        let strat = PairsStrategy::new("L", "R").on(
            Const::<Snapshot<&'static str>>::new(true),
            Const::<Snapshot<&'static str>>::new(false),
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
}
