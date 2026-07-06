//! [`SingleAssetStrategy`]: the generic, all-in skeleton every other strategy in
//! this catalogue specialises.

use crate::indicators::{Const, Position};
use crate::prelude::*;

/// A boxed price-level source — the value a stop-loss / take-profit compares
/// against. Built from the strategy's [`Position`] (see [`Position::entry`],
/// [`Position::peak`]).
type Level = Box<dyn Indicator<Input = Atom, Output = Real>>;

/// The latest value of an optional level, if it is present and warmed up.
fn level_value(level: &Option<Level>) -> Option<Real> {
    level.as_ref().and_then(|l| l.value())
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
/// Positions are always sized all-in against equity with
/// [`value_frac(1.0)`](crate::Size::value_frac), so an entry on the opposite side
/// *reverses* (equity survives a flip, unlike cash) — a single
/// [`set`](crate::Wallet::set) re-sizes all-in exactly. Each transition is guarded
/// by the current position, so an entry while already on that side is a no-op and
/// a level-valued signal (e.g. `roc > 0`) drives the same idempotent behaviour an
/// edge signal does.
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
/// use fugazi::indicators::{Current, Sma, Value};
/// use fugazi::strategies::SingleAssetStrategy;
///
/// // A golden/death-cross that reverses long↔short, with a 5% trailing stop
/// // on each side (long trails the peak, short trails the trough).
/// let cross_up = || Sma::new(Current::close(), 5).crosses_above(Sma::new(Current::close(), 20));
/// let cross_dn = || Sma::new(Current::close(), 5).crosses_below(Sma::new(Current::close(), 20));
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
/// the price stream) and decides in [`trade`](Strategy::trade). A signal reads
/// `false` and a level reads `None` until their sources warm up, and the position
/// guards keep that warm-up from firing a spurious trade.
pub struct SingleAssetStrategy<Sym> {
    symbol: Sym,
    long: Box<dyn Signal>,
    close_long: Box<dyn Signal>,
    short: Box<dyn Signal>,
    close_short: Box<dyn Signal>,
    long_stop: Option<Level>,
    long_target: Option<Level>,
    short_stop: Option<Level>,
    short_target: Option<Level>,
    position: Position,
}

impl<Sym> SingleAssetStrategy<Sym> {
    /// A strategy on `symbol` with no transitions wired — every slot a
    /// constant-`false` signal and no stops. Add sides with
    /// [`long_on`](Self::long_on) / [`short_on`](Self::short_on).
    pub fn new(symbol: Sym) -> Self {
        Self {
            symbol,
            long: Box::new(Const::new(false)),
            close_long: Box::new(Const::new(false)),
            short: Box::new(Const::new(false)),
            close_short: Box::new(Const::new(false)),
            long_stop: None,
            long_target: None,
            short_stop: None,
            short_target: None,
            position: Position::new(),
        }
    }

    /// Go all-in long on the first bar and hold — a long entry that never exits.
    pub fn buy_and_hold(symbol: Sym) -> Self {
        Self::new(symbol).long_on(Const::new(true), Const::new(false))
    }

    /// Enter (or reverse into) an all-in long on `enter`; flatten the long on
    /// `exit`.
    ///
    /// Chainable with [`short_on`](Self::short_on) for a long/short strategy:
    /// because opening a short closes an open long (and vice versa), an always-in
    /// reversal reads as `long_on(up, down).short_on(down, up)`, while a long/flat
    /// strategy uses `long_on` alone.
    pub fn long_on(mut self, enter: impl Signal + 'static, exit: impl Signal + 'static) -> Self {
        self.long = Box::new(enter);
        self.close_long = Box::new(exit);
        self
    }

    /// Enter (or reverse into) an all-in short on `enter`; flatten the short on
    /// `exit`. Opening the short closes any open long, and vice versa.
    pub fn short_on(mut self, enter: impl Signal + 'static, exit: impl Signal + 'static) -> Self {
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

    /// Set the long side's stop-loss level — the long flattens when the bar's
    /// `low` reaches it.
    pub fn long_stop_loss(
        mut self,
        level: impl Indicator<Input = Atom, Output = Real> + 'static,
    ) -> Self {
        self.long_stop = Some(Box::new(level));
        self
    }

    /// Set the long side's take-profit level — the long flattens when the bar's
    /// `high` reaches it.
    pub fn long_take_profit(
        mut self,
        level: impl Indicator<Input = Atom, Output = Real> + 'static,
    ) -> Self {
        self.long_target = Some(Box::new(level));
        self
    }

    /// Set the short side's stop-loss level — the short flattens when the bar's
    /// `high` reaches it.
    pub fn short_stop_loss(
        mut self,
        level: impl Indicator<Input = Atom, Output = Real> + 'static,
    ) -> Self {
        self.short_stop = Some(Box::new(level));
        self
    }

    /// Set the short side's take-profit level — the short flattens when the bar's
    /// `low` reaches it.
    pub fn short_take_profit(
        mut self,
        level: impl Indicator<Input = Atom, Output = Real> + 'static,
    ) -> Self {
        self.short_target = Some(Box::new(level));
        self
    }

}

impl<Sym: Clone + PartialEq> Strategy for SingleAssetStrategy<Sym> {
    type Input = Atom;
    type Symbol = Sym;

    fn update(&mut self, atom: Atom) {
        self.long.update(atom.clone());
        self.close_long.update(atom.clone());
        self.short.update(atom.clone());
        self.close_short.update(atom.clone());
        // Fold the bar into the position's extremes (a no-op while flat) before the
        // levels read it.
        self.position.update(atom.candle);
        if let Some(l) = self.long_stop.as_mut() {
            l.update(atom.clone());
        }
        if let Some(l) = self.long_target.as_mut() {
            l.update(atom.clone());
        }
        if let Some(l) = self.short_stop.as_mut() {
            l.update(atom.clone());
        }
        if let Some(l) = self.short_target.as_mut() {
            l.update(atom);
        }
    }

    fn on_fill(&mut self, order: &Order<Sym>) {
        // Track our own position from the wallet's fills — sets the entry price on
        // an open/reversal and clears it on a flatten (including a protective fill
        // booked inside the wallet).
        if order.symbol == self.symbol {
            self.position.apply(order.side, order.units, order.price);
        }
    }

    fn trade(&self, wallet: &mut dyn Wallet<Sym>) {
        let long = self.position.is_long();
        let short = self.position.is_short();
        // Entries first (all-in, reversal-capable). The fill lands next bar at the
        // open; cancel any resting bracket now (a reversal voids the old one).
        if self.long.is_true() && !long {
            let _ = wallet.set(self.symbol.clone(), Side::Buy, Size::value_frac(1.0));
            let _ = wallet.cancel_protective(&self.symbol);
            return;
        }
        if self.short.is_true() && !short {
            let _ = wallet.set(self.symbol.clone(), Side::Sell, Size::value_frac(1.0));
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
        self.position.reset();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::indicators::Value;
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
            strat.update(c.into());
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
}
