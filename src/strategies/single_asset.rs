//! [`SingleAssetStrategy`]: the generic, all-in skeleton every other strategy in
//! this catalogue specialises.

use crate::indicators::{Book, Const, Position, Value};
use crate::prelude::*;
use crate::types::{Selector, Snapshot};

/// A boxed price-level source — the value a stop-loss / take-profit compares
/// against. Built from the strategy's [`Position`] (see [`Position::entry`],
/// [`Position::peak`]).
///
/// Levels consume the strategy's `Input = Snapshot<Sym>`; a level built from
/// `position.entry()` (etc.) is already
/// [`Input = Snapshot<Sym>`](crate::types::Snapshot) via the [`Position`]
/// carriers.
type Level<Sym> = Box<dyn Indicator<Input = Snapshot<Sym>, Output = Real>>;

/// The latest value of an optional level, if it is present and warmed up.
fn level_value<Sym>(level: &Option<Level<Sym>>) -> Option<Real> {
    level.as_ref().and_then(|l| l.value())
}

/// Route the strategy's *own* asset out of a per-bar [`Snapshot`] for the
/// [`Position`] tracker. Prefers a symbol-matching entry (any frequency); if
/// none match — the common single-series case where the driver pushes an
/// untagged size-1 snapshot via [`Snapshot::of_atom`] — falls back to the
/// [`Snapshot::sole_atom`] unpack. Returns `None` on an empty snapshot; the
/// caller (`SingleAssetStrategy::update`) then simply skips the position
/// fold for that bar.
///
/// Panics with the [`Snapshot::sole_atom`] message if the fallback is
/// triggered on a multi-entry untagged snapshot — that's the same loud
/// failure the empty-selector [`Pick`](crate::indicators::Pick) uses,
/// preserved end-to-end.
fn extract_self_atom<Sym: PartialEq + Clone>(snap: &Snapshot<Sym>, symbol: &Sym) -> Option<Atom> {
    let by_symbol = Selector::by_symbol(symbol.clone());
    snap.find(&by_symbol)
        .cloned()
        .or_else(|| snap.sole_atom().cloned())
}

/// A single-asset, all-in strategy driven by boolean [`Signal`]s — one per state
/// transition of a long / flat / short position (open/close a long, open/close a
/// short) — plus optional **protective stops** (stop-loss / take-profit, fixed
/// or trailing) on each side. You don't set the four signal slots directly; you
/// describe each side with a builder:
///
/// * [`long_on(enter, exit)`](Self::long_on) — go long on `enter`, flatten on `exit`;
/// * [`short_on(enter, exit)`](Self::short_on) — go short on `enter`, flatten on `exit`;
/// * [`buy_and_hold(symbol)`](Self::buy_and_hold) — long the first bar and hold.
///
/// `long_on` and `short_on` chain, and because opening one side closes the other,
/// the three classic shapes fall out:
///
/// * **long/flat** — `new(symbol).long_on(enter, exit)` (no short side);
/// * **always-in long/short** — `new(symbol).long_on(up, down).short_on(down, up)`:
///   the death cross both exits the long and enters the short, so the position
///   flips with no flat state;
/// * **long/short with a flat rest** — give each side a distinct `exit`.
///
/// Positions are sized against equity with
/// [`value_frac(m)`](crate::Size::value_frac), where `m` is the current value of
/// the strategy's **position-sizing indicator** — a magnitude multiplier that
/// defaults to a constant `1.0` (all-in). Set it with
/// [`position_sizing`](Self::position_sizing) to plug in Kelly-scaled,
/// vol-targeted, or fixed-fractional sizing (e.g.
/// `.position_sizing(Value::new(0.5))` for half-position, or a
/// `target_vol.div(realized_vol)` expression for vol targeting). The multiplier
/// is a *magnitude only* — direction still comes from the entry's [`Side`], so a
/// negative reading has no meaning; if the sizing indicator emits `None`
/// (still warming, or a division-by-zero), the strategy simply skips the whole
/// [`trade`](Strategy::trade) call for that bar (safe default; opt-out is an
/// explicit `.or(Value::new(1.0))` at the composition level). Entries and
/// reversals use whatever the sizing indicator reads at that bar — a single
/// [`set`](crate::Wallet::set) at the resulting `value_frac` re-sizes exactly
/// (equity survives a flip, unlike cash). Each transition is guarded by the
/// current position, so an entry while already on that side is a no-op and a
/// level-valued signal (e.g. `roc > 0`) drives the same idempotent behaviour an
/// edge signal does — the sizing multiplier only takes effect on the *next*
/// transition, not mid-position.
///
/// Entries and signal exits are **market orders**: they fill a bar *after* the
/// signal, at the next bar's `open` (the [`Wallet`](crate::Wallet) queues them —
/// see [`PaperWallet`](crate::PaperWallet)), so the bar whose `close` triggered
/// the signal is never also the bar it fills on. The strategy tracks its own
/// [`Position`] from the wallet's fill stream (via [`on_fill`](Strategy::on_fill)),
/// so its side, size and entry price are always the *actual* fills — it never
/// polls the wallet, which is left as the pure execution venue.
///
/// ## Protective stops
///
/// A stop is a **price level** — an ordinary indicator expression over the
/// strategy's [`Position`] (its entry price and the extremes since entry). Grab
/// the [`position`](Self::position) and build the expression
/// (`position.entry()` / `position.peak()` / `position.trough()`), then attach
/// it with [`long_stop_loss`](Self::long_stop_loss) /
/// [`short_stop_loss`](Self::short_stop_loss) (and the `take_profit` twins). A
/// fixed 5% long stop is `position.entry().mul(Value::new(0.95))`; a 5%
/// trailing long stop is `position.peak().mul(Value::new(0.95))`; an ATR stop
/// is `position.entry().sub(Atr::new(14).mul(Value::new(2.0)))`.
///
/// These aren't intra-bar fills the strategy computes: each bar the strategy
/// **rests the level as a stop / take-profit order** on the wallet
/// ([`set_stop`](crate::Wallet::set_stop) / [`set_take_profit`](crate::Wallet::set_take_profit),
/// re-submitted so a trailing level cancel/replaces), and the *wallet* triggers
/// and prices it — filling at the level, or the bar's `open` on a gap. A
/// **trailing** stop tracks completed bars (see [`Position::peak`]) and rests one
/// bar after a new extreme, so it fills a bar later than a fixed stop would.
///
/// ```
/// use fugazi::prelude::*;
/// use fugazi::indicators::{Close, Pick, Sma, Value};
/// use fugazi::strategies::SingleAssetStrategy;
///
/// // A golden/death-cross that reverses long↔short, with a 5% trailing stop
/// // on each side (long trails the peak, short trails the trough). The
/// // strategy's `Input` is `Snapshot<&'static str>`; every atom-input leaf
/// // (`Close`, `Sma`, …) sits on `Pick::<Sym>::new()` — an empty-selector
/// // single-entry unpack — so a single-series driver still Just Works.
/// let close = || Close::of(Pick::<&'static str>::new());
/// let cross_up = || Sma::new(close(), 5).crosses_above(Sma::new(close(), 20));
/// let cross_dn = || Sma::new(close(), 5).crosses_below(Sma::new(close(), 20));
/// let strat = SingleAssetStrategy::new("BTC")
///     .long_on(cross_up(), cross_dn())
///     .short_on(cross_dn(), cross_up());
/// let long_stop = strat.position().peak().mul(Value::new(0.95));
/// let short_stop = strat.position().trough().mul(Value::new(1.05));
/// let strat = strat.long_stop_loss(long_stop).short_stop_loss(short_stop);
/// # let _ = strat;
/// ```
///
/// Like the rest of the catalogue it advances **all** of its signals and levels
/// every bar in [`update`](Strategy::update) (a skipped source would desync from
/// the price stream) and decides in [`trade`](Strategy::trade).
///
/// ## Readiness
///
/// [`is_ready`](Strategy::is_ready) returns `true` only once `bars_seen` reaches
/// the largest `stable_period()` across every wired signal (`long`,
/// `close_long`, `short`, `close_short`), every attached protective level, and
/// the sizing indicator — so the driver skips [`trade`](Strategy::trade) until
/// every source a decision could consult is past both its warm-up **and** its
/// IIR settling tail. This
/// is a safe default: a strategy built from EMAs, RSIs or ATRs won't trade off
/// their seed-dependent early output. A caller who is happy to trade through a
/// subtree's unstable tail opts out explicitly by wrapping it in
/// [`Unstable`](crate::indicators::Unstable) (which zeroes that subtree's
/// `unstable_period()`).
// The protective-level fields use the terse `_stop`/`_target` names, while
// their builder methods use the longer `_stop_loss`/`_take_profit` forms for
// discoverability at construction. The pair is intentional — keep them
// asymmetric.
pub struct SingleAssetStrategy<Sym> {
    symbol: Sym,
    long: Box<dyn Signal<Snapshot<Sym>>>,
    close_long: Box<dyn Signal<Snapshot<Sym>>>,
    short: Box<dyn Signal<Snapshot<Sym>>>,
    close_short: Box<dyn Signal<Snapshot<Sym>>>,
    long_stop: Option<Level<Sym>>,
    long_target: Option<Level<Sym>>,
    short_stop: Option<Level<Sym>>,
    short_target: Option<Level<Sym>>,
    sizing: Level<Sym>,
    position: Position,
    book: Book,
    bars_seen: usize,
}

impl<Sym: Clone + 'static> SingleAssetStrategy<Sym> {
    /// A strategy on `symbol` with no transitions wired — every slot a
    /// constant-`false` signal and no stops. Add sides with
    /// [`long_on`](Self::long_on) / [`short_on`](Self::short_on).
    ///
    /// The strategy's [`Book`] is seeded at `1.0`. This is fine for a toy
    /// [`PaperWallet::new(1.0)`](crate::PaperWallet::new), but any real
    /// backtest needs to match the wallet's initial capital via
    /// [`with_initial_equity`](Self::with_initial_equity) for the
    /// book-anchored sizing recipes (drawdown throttle, realized-vol
    /// target, fractional Kelly) to read meaningful numbers.
    pub fn new(symbol: Sym) -> Self {
        Self::with_initial_equity(symbol, 1.0)
    }

    /// A strategy on `symbol` whose [`Book`] is seeded at `initial_equity`
    /// — the assumed starting capital, which should match the wallet's
    /// seed for equity / drawdown numbers to be meaningful. Otherwise
    /// identical to [`new`](Self::new).
    ///
    /// # Panics
    /// Panics if `initial_equity` is not strictly positive.
    pub fn with_initial_equity(symbol: Sym, initial_equity: Real) -> Self {
        Self {
            symbol,
            long: Box::new(Const::<Snapshot<Sym>>::new(false)),
            close_long: Box::new(Const::<Snapshot<Sym>>::new(false)),
            short: Box::new(Const::<Snapshot<Sym>>::new(false)),
            close_short: Box::new(Const::<Snapshot<Sym>>::new(false)),
            long_stop: None,
            long_target: None,
            short_stop: None,
            short_target: None,
            sizing: Box::new(Value::<Snapshot<Sym>>::new(1.0)),
            position: Position::new(),
            book: Book::new(initial_equity),
            bars_seen: 0,
        }
    }

    /// Go all-in long on the first bar and hold — a long entry that never exits.
    pub fn buy_and_hold(symbol: Sym) -> Self {
        Self::new(symbol).long_on(
            Const::<Snapshot<Sym>>::new(true),
            Const::<Snapshot<Sym>>::new(false),
        )
    }

    /// Enter (or reverse into) an all-in long on `enter`; flatten the long on
    /// `exit`.
    ///
    /// Chainable with [`short_on`](Self::short_on) for a long/short strategy:
    /// because opening a short closes an open long (and vice versa), an always-in
    /// reversal reads as `long_on(up, down).short_on(down, up)`, while a long/flat
    /// strategy uses `long_on` alone.
    pub fn long_on(
        mut self,
        enter: impl Signal<Snapshot<Sym>> + 'static,
        exit: impl Signal<Snapshot<Sym>> + 'static,
    ) -> Self {
        self.long = Box::new(enter);
        self.close_long = Box::new(exit);
        self
    }

    /// Enter (or reverse into) an all-in short on `enter`; flatten the short on
    /// `exit`. Opening the short closes any open long, and vice versa.
    pub fn short_on(
        mut self,
        enter: impl Signal<Snapshot<Sym>> + 'static,
        exit: impl Signal<Snapshot<Sym>> + 'static,
    ) -> Self {
        self.short = Box::new(enter);
        self.close_short = Box::new(exit);
        self
    }

    /// A clone of this strategy's [`Position`], for building a custom stop level:
    /// `strat.position().entry()` is the entry price, tracked by the strategy as it
    /// fills.
    pub fn position(&self) -> Position {
        self.position.clone()
    }

    /// A clone of this strategy's [`Book`], for building a custom sizing
    /// expression against the equity curve or closed-trade history:
    /// `strat.book().drawdown()` for a drawdown-throttled multiplier,
    /// `strat.book().return_per_bar()` for realized-vol targeting,
    /// `strat.book().trade_return()` for a Kelly-style fractional sizer.
    /// See [`Book`] for the full accessor set and the initial-equity
    /// requirement.
    pub fn book(&self) -> Book {
        self.book.clone()
    }

    /// Set the long side's stop-loss level — the long flattens when the bar's
    /// `low` reaches it.
    pub fn long_stop_loss(
        mut self,
        level: impl Indicator<Input = Snapshot<Sym>, Output = Real> + 'static,
    ) -> Self {
        self.long_stop = Some(Box::new(level));
        self
    }

    /// Set the long side's take-profit level — the long flattens when the bar's
    /// `high` reaches it.
    pub fn long_take_profit(
        mut self,
        level: impl Indicator<Input = Snapshot<Sym>, Output = Real> + 'static,
    ) -> Self {
        self.long_target = Some(Box::new(level));
        self
    }

    /// Set the short side's stop-loss level — the short flattens when the bar's
    /// `high` reaches it.
    pub fn short_stop_loss(
        mut self,
        level: impl Indicator<Input = Snapshot<Sym>, Output = Real> + 'static,
    ) -> Self {
        self.short_stop = Some(Box::new(level));
        self
    }

    /// Set the short side's take-profit level — the short flattens when the bar's
    /// `low` reaches it.
    pub fn short_take_profit(
        mut self,
        level: impl Indicator<Input = Snapshot<Sym>, Output = Real> + 'static,
    ) -> Self {
        self.short_target = Some(Box::new(level));
        self
    }

    /// Set the **position-sizing multiplier** — a real-valued source read on
    /// every entry (or reversal) and multiplied into the
    /// [`value_frac`](crate::Size::value_frac) magnitude. The default is a
    /// constant `1.0` (all-in). A negative reading is not meaningful — direction
    /// comes from the entry [`Side`] — and a `None` reading (still warming, or a
    /// division by zero on a vol denominator, say) causes the strategy to *skip*
    /// [`trade`](Strategy::trade) that bar entirely. This is the safe default;
    /// wrap with `.or(Value::new(1.0))` at the composition level to opt out.
    ///
    /// Typical usage: `Value::new(0.5)` for a fixed half-position, a Kelly
    /// fraction indicator for Kelly sizing, or `target_vol.div(realized_vol)` for
    /// vol targeting. The multiplier is read on transitions only — a change
    /// mid-position does not resize.
    pub fn position_sizing(
        mut self,
        sizing: impl Indicator<Input = Snapshot<Sym>, Output = Real> + 'static,
    ) -> Self {
        self.sizing = Box::new(sizing);
        self
    }

    /// The number of bars that must be fed before [`is_ready`](Strategy::is_ready)
    /// reports `true`: the largest `stable_period()` across every wired signal
    /// (`long`, `close_long`, `short`, `close_short`) and every attached
    /// protective level. Unwired slots contribute `0`
    /// ([`Const::<Atom>::new(false)`](crate::indicators::Const) has zero
    /// stable-period). Wrap a subtree in [`Unstable`](crate::indicators::Unstable)
    /// to zero out its IIR settling contribution and only wait for the
    /// warm-up.
    fn readiness_threshold(&self) -> usize {
        let mut needed = self.long.stable_period();
        needed = needed.max(self.close_long.stable_period());
        needed = needed.max(self.short.stable_period());
        needed = needed.max(self.close_short.stable_period());
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
}

impl<Sym: Clone + PartialEq + 'static> Strategy for SingleAssetStrategy<Sym> {
    type Input = Snapshot<Sym>;
    type Symbol = Sym;

    fn update(&mut self, snap: Snapshot<Sym>) {
        // Fold this bar into the position's extremes (a no-op while flat).
        // We route the strategy's *own* asset out of the snapshot for the
        // position — the leaves themselves already know how to project via
        // `!pick`, but Position tracks a plain Candle, so we need to extract
        // one asset's atom here.
        let self_atom = extract_self_atom(&snap, &self.symbol);
        if let Some(atom) = &self_atom {
            self.position.update(atom.candle);
            self.book.update(atom.candle);
        }

        self.long.update(snap.clone());
        self.close_long.update(snap.clone());
        self.short.update(snap.clone());
        self.close_short.update(snap.clone());
        if let Some(l) = self.long_stop.as_mut() {
            l.update(snap.clone());
        }
        if let Some(l) = self.long_target.as_mut() {
            l.update(snap.clone());
        }
        if let Some(l) = self.short_stop.as_mut() {
            l.update(snap.clone());
        }
        if let Some(l) = self.short_target.as_mut() {
            l.update(snap.clone());
        }
        self.sizing.update(snap);
        self.bars_seen = self.bars_seen.saturating_add(1);
    }

    fn is_ready(&self) -> bool {
        self.bars_seen >= self.readiness_threshold()
    }

    fn on_fill(&mut self, order: &Order<Sym>) {
        // Track our own position + book from the wallet's fills — sets the entry
        // price on an open/reversal, clears it on a flatten (including a
        // protective fill booked inside the wallet), and updates the book's
        // cash / units / trade lifecycle in lockstep.
        if order.symbol == self.symbol {
            self.position.apply(order.side, order.units, order.price);
            self.book.apply_fill(order.side, order.units, order.price);
        }
    }

    fn trade(&self, wallet: &mut dyn Wallet<Sym>) {
        // Sizing is read once per bar and skips the whole `trade` call if the
        // indicator can't produce a number — the safe default per the
        // "unsettled data ⇒ wait" convention.
        let Some(size) = self.sizing.value() else {
            return;
        };
        let long = self.position.is_long();
        let short = self.position.is_short();
        // Entries first (magnitude = sizing, reversal-capable). The fill lands next
        // bar at the open; cancel any resting bracket now (a reversal voids the old
        // one).
        if self.long.is_true() && !long {
            let _ = wallet.set(self.symbol.clone(), Side::Buy, Size::value_frac(size));
            let _ = wallet.cancel_protective(&self.symbol);
            return;
        }
        if self.short.is_true() && !short {
            let _ = wallet.set(self.symbol.clone(), Side::Sell, Size::value_frac(size));
            let _ = wallet.cancel_protective(&self.symbol);
            return;
        }
        // Signal-driven flatten-to-flat exits (also fill next bar at the open).
        if (self.close_long.is_true() && long) || (self.close_short.is_true() && short) {
            let _ = wallet.close(self.symbol.clone());
            let _ = wallet.cancel_protective(&self.symbol);
            return;
        }
        // Rest the protective levels on the active side. Re-submitted every bar so
        // a moving (trailing) level cancel/replaces; the wallet triggers and prices
        // them. The wallet reads the side from the position, so a stop is always the
        // adverse level and a take-profit the favourable one.
        if long {
            if let Some(level) = level_value(&self.long_stop) {
                let _ = wallet.set_stop(self.symbol.clone(), Reference(level));
            }
            if let Some(level) = level_value(&self.long_target) {
                let _ = wallet.set_take_profit(self.symbol.clone(), Reference(level));
            }
        } else if short {
            if let Some(level) = level_value(&self.short_stop) {
                let _ = wallet.set_stop(self.symbol.clone(), Reference(level));
            }
            if let Some(level) = level_value(&self.short_target) {
                let _ = wallet.set_take_profit(self.symbol.clone(), Reference(level));
            }
        }
    }

    fn reset(&mut self) {
        self.long.reset();
        self.close_long.reset();
        self.short.reset();
        self.close_short.reset();
        if let Some(l) = self.long_stop.as_mut() {
            l.reset();
        }
        if let Some(l) = self.long_target.as_mut() {
            l.reset();
        }
        if let Some(l) = self.short_stop.as_mut() {
            l.reset();
        }
        if let Some(l) = self.short_target.as_mut() {
            l.reset();
        }
        self.sizing.reset();
        self.position.reset();
        self.book.reset();
        self.bars_seen = 0;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::indicators::{Sma, Value};
    use crate::strategy::PaperWallet;

    /// Drive `strat` over `candles`, feeding each bar to the wallet first and
    /// delivering its fills to the strategy before it updates and trades.
    fn run(
        strat: &mut SingleAssetStrategy<&'static str>,
        candles: &[Candle],
    ) -> PaperWallet<&'static str> {
        let mut wallet = PaperWallet::new(1_000.0);
        for &c in candles {
            for fill in wallet.update("X", c) {
                strat.on_fill(&fill);
            }
            strat.update(Snapshot::of_atom(c.into()));
            strat.trade(&mut wallet);
        }
        wallet
    }

    fn flat_bar(price: Real) -> Candle {
        Candle::new(price, price, price, price, 0.0)
    }

    /// Buy-and-hold with a fixed long stop at `1 - frac` of the entry price —
    /// the general-form equivalent of the removed `stop_loss_pct` sugar.
    fn buy_and_hold_with_stop(frac: Real) -> SingleAssetStrategy<&'static str> {
        let strat = SingleAssetStrategy::buy_and_hold("X");
        let level = strat.position().entry().mul(Value::new(1.0 - frac));
        strat.long_stop_loss(level)
    }

    #[test]
    fn long_stop_loss_fills_at_the_level() {
        // Buy-and-hold, 10% stop. The first bar signals; the entry fills at the
        // *next* bar's open (100), anchoring the stop at 90.
        let mut strat = buy_and_hold_with_stop(0.10);
        // The third bar trades down through 90 (low 88) but opens above it.
        let w = run(
            &mut strat,
            &[
                flat_bar(100.0),                          // signal: queue the entry
                flat_bar(100.0),                          // entry fills at 100; stop = 90
                Candle::new(95.0, 96.0, 88.0, 89.0, 0.0), // down through 90, opens above
            ],
        );
        let exit = w.orders().last().unwrap();
        assert_eq!(exit.side, Side::Sell);
        assert_eq!(exit.price, 90.0); // filled exactly at the stop level
        assert!(w.positions().next().is_none());
    }

    #[test]
    fn long_stop_gaps_to_the_open() {
        // Same stop at 90, but the bar gaps down opening at 85, already below it,
        // so the fill is the open, not the (unreachable) stop level.
        let mut strat = buy_and_hold_with_stop(0.10);
        let w = run(
            &mut strat,
            &[
                flat_bar(100.0),                          // queue the entry
                flat_bar(100.0),                          // entry fills at 100; stop = 90
                Candle::new(85.0, 86.0, 84.0, 84.0, 0.0), // gaps down below the stop
            ],
        );
        let exit = w.orders().last().unwrap();
        assert_eq!(exit.price, 85.0);
        assert!(w.positions().next().is_none());
    }

    #[test]
    fn long_trailing_stop_ratchets_up() {
        // 10% trailing stop, built off the position's running peak. The entry
        // fills at the second bar's open (100); the third bar rallies to a high
        // of 130, so the peak reflects 130 from the fourth bar's update, lifting
        // the resting stop to 117. Because a resting order can only be
        // *submitted* that bar and matched the *next* one, the stop fills a bar
        // later (the accepted trailing lag): the fifth bar opens above 117 and
        // trades down through it, filling at the level.
        let strat = SingleAssetStrategy::buy_and_hold("X");
        let level = strat.position().peak().mul(Value::new(0.90));
        let mut strat = strat.long_stop_loss(level);
        let w = run(
            &mut strat,
            &[
                flat_bar(100.0),                              // queue the entry
                flat_bar(100.0),                              // entry fills at 100
                Candle::new(110.0, 130.0, 109.0, 128.0, 0.0), // sets the peak (130)
                Candle::new(126.0, 127.0, 115.0, 116.0, 0.0), // stop rests at 117 here
                Candle::new(120.0, 121.0, 115.0, 116.0, 0.0), // opens above 117, hits it
            ],
        );
        let exit = w.orders().last().unwrap();
        assert_eq!(exit.side, Side::Sell);
        assert_eq!(exit.price, 117.0); // 130 * 0.9, opened above so filled at the level
        assert_eq!(exit.kind, OrderKind::Stop);
        assert!(w.positions().next().is_none());
    }

    #[test]
    fn no_stop_out_when_price_holds() {
        let mut strat = buy_and_hold_with_stop(0.10);
        // Entry fills at the second bar's open (95); a 10% stop sits at 85.5, and
        // price never reaches it, so it stays long the whole way.
        let w = run(
            &mut strat,
            &[flat_bar(100.0), flat_bar(95.0), flat_bar(105.0)],
        );
        assert_eq!(w.orders().len(), 1); // only the entry
        assert!(w.positions().next().is_some());
    }

    #[test]
    fn position_sizing_scales_entry_magnitude() {
        // Half-position: `value_frac(0.5) * equity / price` at entry. Seed
        // equity is 1000 and the entry fills at 100, so units = 5 (vs. 10 for
        // the default all-in).
        let mut strat =
            SingleAssetStrategy::buy_and_hold("X").position_sizing(Value::new(0.5));
        let w = run(&mut strat, &[flat_bar(100.0), flat_bar(100.0)]);
        let entry = w.orders().last().unwrap();
        assert_eq!(entry.side, Side::Buy);
        assert_eq!(entry.price, 100.0);
        assert_eq!(entry.units, 5.0);
    }

    #[test]
    fn position_sizing_none_skips_trade() {
        // A sizing indicator with a 5-bar warm-up (SMA-5 over a constant 0.5) —
        // until it produces a number, `trade()` must be a no-op even though the
        // entry signal is level-true from bar 1. The SMA emits `Some(0.5)` on
        // bar 5, queueing the entry; it fills at bar 6's open (100), sized
        // half: `0.5 * 1000 / 100 = 5` units.
        let sizing = Sma::new(Value::<Snapshot<&'static str>>::new(0.5), 5);
        let mut strat = SingleAssetStrategy::buy_and_hold("X").position_sizing(sizing);
        let w = run(
            &mut strat,
            &[
                flat_bar(100.0),
                flat_bar(100.0),
                flat_bar(100.0),
                flat_bar(100.0),
                flat_bar(100.0),
                flat_bar(100.0),
            ],
        );
        // Exactly one fill (the bar-6 entry) — every earlier bar's `trade`
        // returned early because the sizing indicator read `None`.
        assert_eq!(w.orders().len(), 1);
        let entry = w.orders().last().unwrap();
        assert_eq!(entry.side, Side::Buy);
        assert_eq!(entry.units, 5.0);
    }

    #[test]
    fn position_sizing_warm_up_gates_is_ready() {
        // With no other warming sources on a buy-and-hold, the strategy is
        // ready from bar 0. Attaching an SMA-5 sizing indicator must push
        // readiness to bar 5.
        let sizing = Sma::new(crate::strategies::self_close::<&'static str>(), 5);
        let mut strat = SingleAssetStrategy::buy_and_hold("X").position_sizing(sizing);
        assert!(!strat.is_ready());
        for _ in 0..4 {
            strat.update(Snapshot::of_atom(flat_bar(100.0).into()));
        }
        assert!(!strat.is_ready());
        strat.update(Snapshot::of_atom(flat_bar(100.0).into()));
        assert!(strat.is_ready());
    }

    /// Drive `strat` over `candles` with a wallet seeded at `initial_cash`
    /// (so the book's seed can be matched).
    fn run_with_capital(
        strat: &mut SingleAssetStrategy<&'static str>,
        candles: &[Candle],
        initial_cash: Real,
    ) -> PaperWallet<&'static str> {
        let mut wallet = PaperWallet::new(initial_cash);
        for &c in candles {
            for fill in wallet.update("X", c) {
                strat.on_fill(&fill);
            }
            strat.update(Snapshot::of_atom(c.into()));
            strat.trade(&mut wallet);
        }
        wallet
    }

    #[test]
    fn book_tracks_buy_and_hold_equity_curve() {
        // Seed both wallet and book at 1000. Buy-and-hold at 100 fills on bar
        // 2 (queued bar 1). Bar 3: mark to 120 → equity should be 1200.
        let mut strat = SingleAssetStrategy::with_initial_equity("X", 1_000.0)
            .long_on(
                Const::<Snapshot<&'static str>>::new(true),
                Const::<Snapshot<&'static str>>::new(false),
            );
        let book = strat.book();
        let _ = run_with_capital(
            &mut strat,
            &[flat_bar(100.0), flat_bar(100.0), flat_bar(120.0)],
            1_000.0,
        );
        assert!(
            (book.equity_value() - 1_200.0).abs() < 1e-9,
            "book equity {}",
            book.equity_value()
        );
        assert!((book.equity_peak_value() - 1_200.0).abs() < 1e-9);
    }

    #[test]
    fn book_records_trade_close_on_exit() {
        // Long/flat: enter bar 0, exit bar 2. Entry queues bar 0, fills bar 1
        // at 100. Exit queues bar 2, fills bar 3 at 120 — trade P&L =
        // 10 * (120 − 100) = 200, return 0.20 relative to seed equity 1000.
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
        let mut strat = SingleAssetStrategy::with_initial_equity("X", 1_000.0).long_on(
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
                flat_bar(100.0), // bar 0: enter signals; queue Buy
                flat_bar(100.0), // bar 1: entry fills at 100 (10 units)
                flat_bar(110.0), // bar 2: exit signals; queue close
                flat_bar(120.0), // bar 3: exit fills at 120; book records close
            ],
            1_000.0,
        );
        // After the run, the book's active_trade_close should reflect the
        // close (it was set on bar 3's update, no bar after to drain it).
        let ret = book.trade_return::<Snapshot<&'static str>>().value();
        let pnl = book.trade_pnl::<Snapshot<&'static str>>().value();
        assert!(ret.is_some() && pnl.is_some(), "no trade close recorded");
        assert!((pnl.unwrap() - 200.0).abs() < 1e-9);
        assert!((ret.unwrap() - 0.20).abs() < 1e-9);
    }
}
