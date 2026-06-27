//! [`SingleAssetStrategy`]: the generic, all-in skeleton every other strategy in
//! this catalogue specialises.

use crate::prelude::*;
use crate::indicators::Const;

use super::{is_long, is_short};

/// A single-asset, all-in strategy driven by boolean [`Signal`]s — one per state
/// transition of a long / flat / short position (open/close a long, open/close a
/// short). You don't set those four slots directly; you describe each side with a
/// builder:
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
/// ```
/// use fugazi::prelude::*;
/// use fugazi::indicators::{Current, Sma};
/// use fugazi::strategies::SingleAssetStrategy;
///
/// // A golden/death-cross that reverses long↔short — what `ma_crossover` builds.
/// let cross_up = || Sma::new(Current::close(), 5).crosses_above(Sma::new(Current::close(), 20));
/// let cross_dn = || Sma::new(Current::close(), 5).crosses_below(Sma::new(Current::close(), 20));
/// let strat = SingleAssetStrategy::new("BTC")
///     .long_on(cross_up(), cross_dn())
///     .short_on(cross_dn(), cross_up());
/// # let _ = strat;
/// ```
///
/// Like the rest of the catalogue it advances **all** of its signals every bar in
/// [`update`](Strategy::update) (a skipped signal would desync from the price
/// stream) and decides in [`trade`](Strategy::trade). A signal reads `false`
/// until its sources warm up, and the position guards keep that warm-up from
/// firing a spurious trade.
pub struct SingleAssetStrategy<Sym> {
    symbol: Sym,
    long: Box<dyn Signal>,
    close_long: Box<dyn Signal>,
    short: Box<dyn Signal>,
    close_short: Box<dyn Signal>,
}

impl<Sym> SingleAssetStrategy<Sym> {
    /// A strategy on `symbol` with no transitions wired — every slot a
    /// constant-`false` signal. Add sides with [`long_on`](Self::long_on) /
    /// [`short_on`](Self::short_on).
    pub fn new(symbol: Sym) -> Self {
        Self {
            symbol,
            long: Box::new(Const::new(false)),
            close_long: Box::new(Const::new(false)),
            short: Box::new(Const::new(false)),
            close_short: Box::new(Const::new(false)),
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
}

impl<Sym: Clone> Strategy for SingleAssetStrategy<Sym> {
    type Input = Candle;
    type Symbol = Sym;

    fn update(&mut self, candle: Candle) {
        self.long.update(candle);
        self.close_long.update(candle);
        self.short.update(candle);
        self.close_short.update(candle);
    }

    fn trade(&self, wallet: &mut dyn Wallet<Sym>) {
        let pos = wallet.position(&self.symbol).amount;
        // Entries first (all-in, reversal-capable), then flatten-to-flat exits.
        if self.long.is_true() && !is_long(pos) {
            let _ = wallet.set(self.symbol.clone(), Side::Buy, Size::value_frac(1.0));
        } else if self.short.is_true() && !is_short(pos) {
            let _ = wallet.set(self.symbol.clone(), Side::Sell, Size::value_frac(1.0));
        } else if (self.close_long.is_true() && is_long(pos))
            || (self.close_short.is_true() && is_short(pos))
        {
            let _ = wallet.close(self.symbol.clone());
        }
    }

    fn reset(&mut self) {
        self.long.reset();
        self.close_long.reset();
        self.short.reset();
        self.close_short.reset();
    }
}
