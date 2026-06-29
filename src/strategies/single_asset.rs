//! [`SingleAssetStrategy`]: the generic, all-in skeleton every other strategy in
//! this catalogue specialises.

use crate::indicators::{Const, Entry, EntryAnchor, PeakSinceEntry, TroughSinceEntry, Value};
use crate::prelude::*;

use super::{is_long, is_short};

/// A boxed price-level source — the value a stop-loss / take-profit compares
/// against. Built from the strategy's [`EntryAnchor`] (see [`Entry`],
/// [`PeakSinceEntry`]).
type Level = Box<dyn Indicator<Input = Candle, Output = Real>>;

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
/// ## Protective stops
///
/// A stop is a **price level** — an ordinary indicator expression over the
/// strategy's [`EntryAnchor`] (the entry price) and the bar. The percentage sugar
/// covers the common cases:
///
/// * [`stop_loss_pct(0.05)`](Self::stop_loss_pct) — exit 5% adverse to entry;
/// * [`take_profit_pct(0.10)`](Self::take_profit_pct) — exit 10% favourable;
/// * [`trailing_stop_pct(0.05)`](Self::trailing_stop_pct) — exit 5% off the best
///   price reached since entry.
///
/// Each is symmetric (applies to long and short alike). For a custom level —
/// e.g. an ATR stop — grab the [`anchor`](Self::anchor) and build the expression,
/// then attach it with [`long_stop_loss`](Self::long_stop_loss) /
/// [`short_stop_loss`](Self::short_stop_loss) (and the `take_profit` twins).
/// Stops are checked every bar against the candle's `high`/`low`, so they fire
/// intra-bar, and they fill at the level itself — clamped into the bar's
/// `[low, high]` when price gaps clean through it.
///
/// ```
/// use fugazi::prelude::*;
/// use fugazi::indicators::{Current, Sma};
/// use fugazi::strategies::SingleAssetStrategy;
///
/// // A golden/death-cross that reverses long↔short, with a 5% trailing stop.
/// let cross_up = || Sma::new(Current::close(), 5).crosses_above(Sma::new(Current::close(), 20));
/// let cross_dn = || Sma::new(Current::close(), 5).crosses_below(Sma::new(Current::close(), 20));
/// let strat = SingleAssetStrategy::new("BTC")
///     .long_on(cross_up(), cross_dn())
///     .short_on(cross_dn(), cross_up())
///     .trailing_stop_pct(0.05);
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
    anchor: EntryAnchor,
    last_candle: Option<Candle>,
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
            anchor: EntryAnchor::new(),
            last_candle: None,
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

    /// A clone of this strategy's [`EntryAnchor`], for building a custom stop
    /// level: `Entry::new(strat.anchor())` is the entry price, advanced and armed
    /// by the strategy as it trades.
    pub fn anchor(&self) -> EntryAnchor {
        self.anchor.clone()
    }

    /// Set the long side's stop-loss level — the long flattens when the bar's
    /// `low` reaches it.
    pub fn long_stop_loss(
        mut self,
        level: impl Indicator<Input = Candle, Output = Real> + 'static,
    ) -> Self {
        self.long_stop = Some(Box::new(level));
        self
    }

    /// Set the long side's take-profit level — the long flattens when the bar's
    /// `high` reaches it.
    pub fn long_take_profit(
        mut self,
        level: impl Indicator<Input = Candle, Output = Real> + 'static,
    ) -> Self {
        self.long_target = Some(Box::new(level));
        self
    }

    /// Set the short side's stop-loss level — the short flattens when the bar's
    /// `high` reaches it.
    pub fn short_stop_loss(
        mut self,
        level: impl Indicator<Input = Candle, Output = Real> + 'static,
    ) -> Self {
        self.short_stop = Some(Box::new(level));
        self
    }

    /// Set the short side's take-profit level — the short flattens when the bar's
    /// `low` reaches it.
    pub fn short_take_profit(
        mut self,
        level: impl Indicator<Input = Candle, Output = Real> + 'static,
    ) -> Self {
        self.short_target = Some(Box::new(level));
        self
    }

    /// A fixed stop-loss `frac` away from entry, both sides (long below entry,
    /// short above).
    pub fn stop_loss_pct(self, frac: Real) -> Self {
        let anchor = self.anchor();
        self.long_stop_loss(Entry::new(anchor.clone()).mul(Value::new(1.0 - frac)))
            .short_stop_loss(Entry::new(anchor).mul(Value::new(1.0 + frac)))
    }

    /// A fixed take-profit `frac` away from entry, both sides (long above entry,
    /// short below).
    pub fn take_profit_pct(self, frac: Real) -> Self {
        let anchor = self.anchor();
        self.long_take_profit(Entry::new(anchor.clone()).mul(Value::new(1.0 + frac)))
            .short_take_profit(Entry::new(anchor).mul(Value::new(1.0 - frac)))
    }

    /// A trailing stop `frac` off the best price reached since entry, both sides
    /// (off the running high for a long, the running low for a short). Replaces
    /// the side's stop-loss level.
    pub fn trailing_stop_pct(self, frac: Real) -> Self {
        let anchor = self.anchor();
        self.long_stop_loss(PeakSinceEntry::new(anchor.clone()).mul(Value::new(1.0 - frac)))
            .short_stop_loss(TroughSinceEntry::new(anchor).mul(Value::new(1.0 + frac)))
    }
}

impl<Sym: Clone> SingleAssetStrategy<Sym> {
    /// Flatten the position, filling at `price`, and disarm the entry anchor.
    fn exit_at(&self, wallet: &mut dyn Wallet<Sym>, price: Real) {
        if wallet.close_at(self.symbol.clone(), Reference(price)).is_ok() {
            self.anchor.clear();
        }
    }
}

impl<Sym: Clone> Strategy for SingleAssetStrategy<Sym> {
    type Input = Candle;
    type Symbol = Sym;

    fn update(&mut self, candle: Candle) {
        self.long.update(candle);
        self.close_long.update(candle);
        self.short.update(candle);
        self.close_short.update(candle);
        if let Some(l) = self.long_stop.as_mut() {
            l.update(candle);
        }
        if let Some(l) = self.long_target.as_mut() {
            l.update(candle);
        }
        if let Some(l) = self.short_stop.as_mut() {
            l.update(candle);
        }
        if let Some(l) = self.short_target.as_mut() {
            l.update(candle);
        }
        self.last_candle = Some(candle);
    }

    fn trade(&self, wallet: &mut dyn Wallet<Sym>) {
        let pos = wallet.position(&self.symbol).amount;
        // Entries first (all-in, reversal-capable); arm the anchor at the fill.
        if self.long.is_true() && !is_long(pos) {
            if let Ok(Some(order)) = wallet.set(self.symbol.clone(), Side::Buy, Size::value_frac(1.0))
            {
                self.anchor.arm(order.price);
            }
            return;
        }
        if self.short.is_true() && !is_short(pos) {
            if let Ok(Some(order)) =
                wallet.set(self.symbol.clone(), Side::Sell, Size::value_frac(1.0))
            {
                self.anchor.arm(order.price);
            }
            return;
        }
        // Signal-driven flatten-to-flat exits.
        if (self.close_long.is_true() && is_long(pos))
            || (self.close_short.is_true() && is_short(pos))
        {
            if wallet.close(self.symbol.clone()).is_ok() {
                self.anchor.clear();
            }
            return;
        }
        // Protective stops on the active side. The fill is the level itself,
        // clamped into the bar's `[low, high]` — so it is always a price the bar
        // traded at (and a bar that gaps clean through fills at the near edge).
        let Some(candle) = self.last_candle else {
            return;
        };
        let fill_at = |level: Real| level.clamp(candle.low, candle.high);
        // Stop-loss takes precedence over take-profit within a bar.
        if is_long(pos) {
            if let Some(level) = level_value(&self.long_stop)
                && candle.low <= level
            {
                self.exit_at(wallet, fill_at(level));
            } else if let Some(level) = level_value(&self.long_target)
                && candle.high >= level
            {
                self.exit_at(wallet, fill_at(level));
            }
        } else if is_short(pos) {
            if let Some(level) = level_value(&self.short_stop)
                && candle.high >= level
            {
                self.exit_at(wallet, fill_at(level));
            } else if let Some(level) = level_value(&self.short_target)
                && candle.low <= level
            {
                self.exit_at(wallet, fill_at(level));
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
        self.anchor.clear();
        self.last_candle = None;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::strategy::PaperWallet;

    /// Drive `strat` over `candles`, feeding each bar to the wallet first.
    fn run(strat: &mut SingleAssetStrategy<&'static str>, candles: &[Candle]) -> PaperWallet<&'static str> {
        let mut wallet = PaperWallet::new(1_000.0);
        for &c in candles {
            wallet.update("X", c);
            strat.update(c);
            strat.trade(&mut wallet);
        }
        wallet
    }

    fn flat_bar(price: Real) -> Candle {
        Candle::new(price, price, price, price, 0.0)
    }

    #[test]
    fn long_stop_loss_fills_at_the_level() {
        // Buy-and-hold the first bar at 100, with a 10% stop at 90.
        let mut strat = SingleAssetStrategy::buy_and_hold("X").stop_loss_pct(0.10);
        // Bar 2 trades down through 90 (low 88) but opens above it.
        let w = run(
            &mut strat,
            &[flat_bar(100.0), Candle::new(95.0, 96.0, 88.0, 89.0, 0.0)],
        );
        let exit = w.orders().last().unwrap();
        assert_eq!(exit.side, Side::Sell);
        assert_eq!(exit.price, 90.0); // filled exactly at the stop level
        assert!(w.is_flat());
    }

    #[test]
    fn long_stop_clamps_into_bar_on_gap() {
        // Same stop at 90, but bar 2 gaps clean through it: the whole bar trades
        // below 90, so the fill clamps to the bar's high (its near edge).
        let mut strat = SingleAssetStrategy::buy_and_hold("X").stop_loss_pct(0.10);
        let w = run(
            &mut strat,
            &[flat_bar(100.0), Candle::new(85.0, 86.0, 84.0, 84.0, 0.0)],
        );
        let exit = w.orders().last().unwrap();
        assert_eq!(exit.price, 86.0);
        assert!(w.is_flat());
    }

    #[test]
    fn long_trailing_stop_ratchets_up() {
        // 10% trailing stop. Enter at 100; the next bar rallies to a high of 130
        // (lifting the stop to 117) and then trades down to 109, crossing it.
        let mut strat = SingleAssetStrategy::buy_and_hold("X").trailing_stop_pct(0.10);
        let w = run(
            &mut strat,
            &[flat_bar(100.0), Candle::new(110.0, 130.0, 109.0, 128.0, 0.0)],
        );
        let exit = w.orders().last().unwrap();
        assert_eq!(exit.side, Side::Sell);
        assert_eq!(exit.price, 117.0); // 130 * 0.9, inside the bar's range
        assert!(w.is_flat());
    }

    #[test]
    fn no_stop_out_when_price_holds() {
        let mut strat = SingleAssetStrategy::buy_and_hold("X").stop_loss_pct(0.10);
        // Never trades below 90: stays long the whole way.
        let w = run(
            &mut strat,
            &[flat_bar(100.0), flat_bar(95.0), flat_bar(105.0)],
        );
        assert_eq!(w.orders().len(), 1); // only the entry
        assert!(!w.is_flat());
    }
}
