//! [`SleeveWallet`]: a decorator that hides pre-existing positions from the
//! strategy trading through it, so a strategy sizes against *its own* slice of
//! an account rather than the whole balance.

use std::collections::HashMap;
use std::hash::Hash;

use crate::costs::TradingCosts;
use crate::types::{Candle, Real};

use super::Wallet;
use super::types::{
    Ack, Order, OrderId, POSITION_EPSILON, Reference, Rejection, Side, Size, Units, WalletError,
};

/// A **sleeve**: a [`Wallet`] view that runs a strategy against its own carve-out
/// of a shared account. The positions the wrapped wallet **already held when it
/// was wrapped** are treated as externally managed — the user's own book, or
/// another sleeve's — and the strategy sees only *its* share:
///
/// * [`position`](Wallet::position) / [`positions`](Wallet::positions) /
///   [`equity`](Wallet::equity) exclude the `baseline`, so sizing (`value_frac`
///   &c.) is against the sleeve's own capital (cash + only the positions it
///   opens);
/// * [`set_position`](Wallet::set_position) is **offset** by the baseline, so an
///   order moves only our share and never disturbs the external one;
/// * [`close`](Wallet::close) (the trait default, `set_position(0)`) flattens
///   *our* share to the baseline, not to zero;
/// * [`set`](Wallet::set) forwards to the inner wallet (preserving its fill-time
///   `shrink_buy_to_fit`) with the size re-expressed against the inner's
///   external-inclusive basis, so a `value_frac` buy still resolves to our own
///   amount without being refused when price ticks up;
/// * the protective legs are re-sized to **own** units via [`Size::resolve`], so a
///   whole-position stop closes our leg, not the user's.
///
/// Cross-symbol external positions (we trade symbols the user doesn't hold) are
/// exact; a same-symbol overlap is best-effort, since the venue reads one net
/// side per symbol.
///
/// Apply it at the wallet boundary — for a strategy, wrap the wallet before
/// [`backtest::run`](crate::backtest::run); for a
/// [`Portfolio`](crate::portfolio::Portfolio), it *is* the account the portfolio
/// trades, so the ledgers see own positions and equity uniformly.
pub struct SleeveWallet<Sym, W> {
    inner: W,
    baseline: HashMap<Sym, Real>,
}

impl<Sym: Clone + Eq + Hash, W: Wallet<Sym>> SleeveWallet<Sym, W> {
    /// Wrap `inner`, treating `baseline` (signed units per symbol) as externally
    /// held. Use [`external_baseline`] to snapshot it from the wallet's current
    /// positions.
    pub fn new(inner: W, baseline: HashMap<Sym, Real>) -> Self {
        Self { inner, baseline }
    }

    /// Reclaim the wrapped wallet after a run, to hand it back to its owner with
    /// the run's fills applied (the external baseline is untouched).
    pub fn into_inner(self) -> W {
        self.inner
    }

    fn base(&self, symbol: &Sym) -> Real {
        self.baseline.get(symbol).copied().unwrap_or(0.0)
    }

    /// Value of the external positions at current marks — subtracted from the
    /// account equity to leave the strategy's own equity. An unpriced external
    /// symbol contributes 0, matching how the underlying `equity()` skips it.
    fn external_value(&self) -> Real {
        self.baseline
            .iter()
            .map(|(sym, &amt)| amt * self.inner.price(sym).map_or(0.0, |p| p.0))
            .sum()
    }

    /// Resolve a protective/limit `size` against the strategy's **own** book, so
    /// a `position_frac(1.0)` leg takes off our units, not the account's.
    fn own_units(&self, symbol: &Sym, fallback_price: Real, size: Size) -> Real {
        let price = self.inner.price(symbol).map_or(fallback_price, |p| p.0);
        let own_position = self.inner.position(symbol).amount - self.base(symbol);
        size.resolve(price, own_position, self.inner.funds().0, self.equity().0)
    }
}

impl<Sym: Clone + Eq + Hash, W: Wallet<Sym>> Wallet<Sym> for SleeveWallet<Sym, W> {
    fn funds(&self) -> Reference {
        self.inner.funds()
    }
    fn position(&self, symbol: &Sym) -> Units<Sym> {
        Units {
            symbol: symbol.clone(),
            amount: self.inner.position(symbol).amount - self.base(symbol),
        }
    }
    fn positions(&self) -> Vec<Units<Sym>> {
        self.inner
            .positions()
            .into_iter()
            .filter_map(|u| {
                let amount = u.amount - self.base(&u.symbol);
                (amount.abs() > POSITION_EPSILON).then_some(Units {
                    symbol: u.symbol,
                    amount,
                })
            })
            .collect()
    }
    fn price(&self, symbol: &Sym) -> Option<Reference> {
        self.inner.price(symbol)
    }
    fn equity(&self) -> Reference {
        Reference(self.inner.equity().0 - self.external_value())
    }
    /// Delegates: shorting is a property of the account underneath, and the
    /// sleeve is only a view onto it. (A sleeve over a spot wallet can still
    /// *sell* down to its baseline — that reduces our share of a long, it
    /// doesn't open a short.)
    fn can_short(&self) -> bool {
        self.inner.can_short()
    }
    /// Delegates, for the same reason: a sleeve carves a share out of one
    /// account's cash, it does not redenominate it.
    fn quote_ccy(&self) -> Option<&str> {
        self.inner.quote_ccy()
    }
    /// Delegates, for the same reason again: a sleeve hides part of one
    /// account's position, it does not move it to another venue.
    fn data_sources(&self) -> &'static [&'static str] {
        self.inner.data_sources()
    }
    fn update(&mut self, symbol: Sym, candle: Candle) -> Vec<Order<Sym>> {
        self.inner.update(symbol, candle)
    }
    fn set_position(&mut self, target: Units<Sym>) -> Result<Ack<Sym>, WalletError> {
        let amount = self.base(&target.symbol) + target.amount;
        self.inner.set_position(Units {
            symbol: target.symbol,
            amount,
        })
    }
    fn set(&mut self, symbol: Sym, side: Side, size: Size) -> Result<Ack<Sym>, WalletError> {
        // Forward to the inner wallet rather than resolving to units here (the
        // trait default), so the inner's *fill-time* resolution and
        // shrink-to-fit survive — the netting layer relies on a `value_frac` buy
        // shrinking rather than being refused when price ticks up between bars.
        // Re-express the size against the inner's external-inclusive basis so the
        // amount that resolves is still the strategy's own. Absolute for the
        // symbol (no baseline delta): exact when the symbol isn't externally
        // held, best-effort on a same-symbol overlap.
        let translated = match size {
            Size::ValueFraction(f) => {
                let inner_eq = self.inner.equity().0;
                if inner_eq.abs() > POSITION_EPSILON {
                    Size::value_frac(f * self.equity().0 / inner_eq)
                } else {
                    size
                }
            }
            Size::PositionFraction(f) => {
                let own = self.inner.position(&symbol).amount - self.base(&symbol);
                Size::units(f.abs() * own.abs())
            }
            // `Units` is absolute; `FundsFraction` sizes against the shared cash
            // — both forward unchanged.
            other => other,
        };
        self.inner.set(symbol, side, translated)
    }
    fn set_stop(
        &mut self,
        symbol: Sym,
        trigger: Reference,
        size: Size,
    ) -> Result<Ack<Sym>, WalletError> {
        let units = self.own_units(&symbol, trigger.0, size);
        self.inner.set_stop(symbol, trigger, Size::units(units))
    }
    fn set_take_profit(
        &mut self,
        symbol: Sym,
        trigger: Reference,
        size: Size,
    ) -> Result<Ack<Sym>, WalletError> {
        let units = self.own_units(&symbol, trigger.0, size);
        self.inner
            .set_take_profit(symbol, trigger, Size::units(units))
    }
    fn cancel_protective(&mut self, symbol: &Sym) -> Result<(), WalletError> {
        self.inner.cancel_protective(symbol)
    }
    fn set_limit(
        &mut self,
        symbol: Sym,
        side: Side,
        size: Size,
        limit: Reference,
    ) -> Result<Ack<Sym>, WalletError> {
        // A limit sets an absolute side·size target on fill and so can't be
        // offset by a baseline the way market moves are; forwarded as-is. No
        // built-in strategy shape rests a limit entry, so this is a best-effort
        // passthrough.
        self.inner.set_limit(symbol, side, size, limit)
    }
    fn cancel_limit(&mut self, symbol: &Sym) -> Result<(), WalletError> {
        self.inner.cancel_limit(symbol)
    }
    fn cancel(&mut self, id: OrderId) -> Result<(), WalletError> {
        self.inner.cancel(id)
    }
    fn adjust_funds(&mut self, delta: Real) -> Result<(), WalletError> {
        self.inner.adjust_funds(delta)
    }
    fn set_costs_for(&mut self, symbol: Sym, costs: TradingCosts) -> Result<(), WalletError> {
        self.inner.set_costs_for(symbol, costs)
    }
    fn take_rejections(&mut self) -> Vec<Rejection<Sym>> {
        self.inner.take_rejections()
    }
    fn poll_fills(&mut self) -> Vec<Order<Sym>> {
        self.inner.poll_fills()
    }
    // Forwarded so a sleeve over a paper wallet still round-trips its book.
    // The baseline itself is not persisted: it is re-snapshotted from the
    // account each time a run is prepared, so it always reflects what the user
    // actually holds now rather than what they held when the state was written.
    fn snapshot_state(&self) -> serde_json::Value
    where
        Sym: serde::Serialize + serde::de::DeserializeOwned,
    {
        self.inner.snapshot_state()
    }
    fn restore_state(&mut self, state: &serde_json::Value) -> Result<(), String>
    where
        Sym: serde::Serialize + serde::de::DeserializeOwned,
    {
        self.inner.restore_state(state)
    }
}

/// Snapshot a wallet's current positions as an external baseline for
/// [`SleeveWallet`] — the positions to treat as the user's own and leave
/// untouched. Reads through the [`Wallet`] trait, so it needs
/// [`positions`](Wallet::positions) to enumerate.
pub fn external_baseline<Sym: Clone + Eq + Hash>(wallet: &dyn Wallet<Sym>) -> HashMap<Sym, Real> {
    wallet
        .positions()
        .into_iter()
        .filter(|u| u.amount.abs() > POSITION_EPSILON)
        .map(|u| (u.symbol, u.amount))
        .collect()
}

/// The strategy's **own** opening equity given an external `baseline`: the
/// account equity minus the value of the externally-held positions at current
/// marks. Seeds a strategy/portfolio book so book-anchored sizing reads our
/// capital, not the account's. An unpriced external symbol contributes 0,
/// matching how [`equity`](Wallet::equity) values it.
pub fn own_equity<Sym: Clone + Eq + Hash>(
    wallet: &dyn Wallet<Sym>,
    baseline: &HashMap<Sym, Real>,
) -> Real {
    let external: Real = baseline
        .iter()
        .map(|(sym, &amt)| amt * wallet.price(sym).map_or(0.0, |p| p.0))
        .sum();
    wallet.equity().0 - external
}
