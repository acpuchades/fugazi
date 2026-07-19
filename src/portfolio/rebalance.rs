//! [`PositionRebalancer`]: the pluggable position-phase policy of a
//! [`Portfolio`](super::Portfolio) rebalance cycle.
//!
//! Each rebalance-fire runs two phases:
//! 1. **Cash phase** — free cash is shifted between sub-wallets to hit
//!    per-child equity targets. Handled by
//!    [`PortfolioInner::rebalance_cash_to`](super::wallet::PortfolioInner)
//!    and doesn't need customization (the algorithm is symmetric and
//!    respects `Wallet::adjust_funds` support).
//! 2. **Position phase** — for each contributor whose cash phase
//!    couldn't fully cover its donation, submit `set_position`
//!    scale-downs to raise cash for the following fire cycle. **This is
//!    the pluggable phase**: what to sell, in what order, and by how
//!    much varies by strategy. The default [`Proportional`] impl
//!    matches the original hardcoded behavior; [`LargestFirst`]
//!    liquidates full positions from largest first; a caller with a
//!    bespoke rule ("sell losers first", "sell most liquid first", "keep
//!    hedges intact") plugs in a custom impl via
//!    [`PortfolioBuilder::position_rebalancer`](super::PortfolioBuilder::position_rebalancer).
//!
//! The trait consumes per-position marks (unit count + current price)
//! plus the residual shortfall in reference currency, and returns new
//! target unit counts per position. The portfolio submits each target
//! via [`Wallet::set_position`](crate::Wallet::set_position); positions
//! omitted from the returned vec are left alone.

use crate::types::Real;
use crate::wallet::Units;

/// A snapshot of one held position, handed to a [`PositionRebalancer`]
/// so an impl can decide what to sell.
///
/// `units` is signed (positive long, negative short), and `price` is
/// the current mark from the sub-wallet. The value of this position in
/// reference currency is `units.abs() * price` — the "size" a
/// value-based rule (e.g. [`LargestFirst`]) sorts on.
#[derive(Debug, Clone)]
pub struct PositionInfo<Sym> {
    /// The instrument this position is in.
    pub symbol: Sym,
    /// Signed unit count — positive long, negative short.
    pub units: Real,
    /// Current mark for this instrument, in reference currency.
    pub price: Real,
}

/// The pluggable position-phase policy of a
/// [`Portfolio`](super::Portfolio) rebalance cycle. Impls decide *what
/// to sell* — which held positions to touch and by how much — to raise
/// `shortfall` reference-currency units of freed cash.
///
/// Called once per contributor per rebalance-fire, and only when its
/// cash-phase donation couldn't be fully covered (`shortfall > 0`).
/// The returned vector lists new target unit counts per position; the
/// portfolio submits each via
/// [`Wallet::set_position`](crate::Wallet::set_position). Positions
/// omitted from the returned vec are left alone (a "keep this one" hint
/// for value-preserving policies).
///
/// Two built-in impls ship: [`Proportional`] (the default — matches
/// the original hardcoded behavior) and [`LargestFirst`].
pub trait PositionRebalancer<Sym>: Send + Sync {
    /// Plan a set of scale-downs to raise `shortfall` freed cash from
    /// the given contributor's `positions`. Return an empty vec to skip
    /// this contributor (no orders emitted).
    ///
    /// Symbol / units in the returned vec are absolute targets, not
    /// deltas — passing a `Units { amount: 0.0 }` closes the leg fully,
    /// passing `amount: units * 0.75` keeps 75% of it. The strategy
    /// layer's `on_fill` sees each resulting fill and reacts per its
    /// own logic (book-anchored sizing recipes naturally respect the
    /// post-rebalance equity; hard-target strategies may re-enter on
    /// the next bar).
    fn plan_scaledowns(
        &self,
        positions: &[PositionInfo<Sym>],
        shortfall: Real,
    ) -> Vec<Units<Sym>>;
}

/// The **proportional** position-phase policy — scales every held
/// position down by the same fraction `(1 - shortfall/invested)`. Every
/// leg contributes to the shortfall in proportion to its current value.
///
/// This is the default installed on a fresh
/// [`Portfolio`](super::Portfolio) and matches the original hardcoded
/// rebalance behavior — a good baseline that doesn't privilege any
/// particular leg.
#[derive(Debug, Clone, Copy, Default)]
pub struct Proportional;

impl<Sym: Clone + Send + Sync> PositionRebalancer<Sym> for Proportional {
    fn plan_scaledowns(
        &self,
        positions: &[PositionInfo<Sym>],
        shortfall: Real,
    ) -> Vec<Units<Sym>> {
        // Total invested value = Σ |units_i| * price_i. A shortfall of
        // zero or a fully-cash contributor produces no orders.
        let invested: Real = positions.iter().map(|p| p.units.abs() * p.price).sum();
        if invested <= 0.0 || positions.is_empty() {
            return Vec::new();
        }
        let f = (shortfall / invested).clamp(0.0, 1.0);
        if f <= 0.0 {
            return Vec::new();
        }
        let scale = 1.0 - f;
        positions
            .iter()
            .map(|p| Units {
                symbol: p.symbol.clone(),
                amount: p.units * scale,
            })
            .collect()
    }
}

/// A **largest-first** position-phase policy — fully liquidates
/// positions from largest to smallest (by absolute value = `|units| *
/// price`) until the running total of freed cash covers the shortfall.
///
/// If the last position wouldn't need to be fully liquidated to cover
/// the shortfall, it's partially scaled instead (a target `units * (1 -
/// residual/value)`). Positions that don't need to be touched to cover
/// the shortfall are omitted from the returned vec — left alone.
///
/// Useful when the goal is to reduce complexity by closing whole legs
/// rather than proportionally trimming everything, or when the smallest
/// positions carry non-fungible value the caller wants to preserve
/// (a core holding, a hedge, etc.).
#[derive(Debug, Clone, Copy, Default)]
pub struct LargestFirst;

impl<Sym: Clone + Send + Sync> PositionRebalancer<Sym> for LargestFirst {
    fn plan_scaledowns(
        &self,
        positions: &[PositionInfo<Sym>],
        shortfall: Real,
    ) -> Vec<Units<Sym>> {
        if shortfall <= 0.0 || positions.is_empty() {
            return Vec::new();
        }
        let mut sorted: Vec<&PositionInfo<Sym>> = positions.iter().collect();
        // Largest absolute value first. NaN sorts to the end.
        sorted.sort_by(|a, b| {
            let av = a.units.abs() * a.price;
            let bv = b.units.abs() * b.price;
            bv.partial_cmp(&av).unwrap_or(std::cmp::Ordering::Equal)
        });
        let mut remaining = shortfall;
        let mut out = Vec::new();
        for p in sorted {
            if remaining <= 0.0 {
                break;
            }
            let value = p.units.abs() * p.price;
            if value <= 0.0 {
                continue;
            }
            if value <= remaining {
                // Fully liquidate this leg.
                out.push(Units {
                    symbol: p.symbol.clone(),
                    amount: 0.0,
                });
                remaining -= value;
            } else {
                // Partial scale-down: raise exactly `remaining` from
                // this position by keeping `(value - remaining)/value`
                // of it.
                let keep = (value - remaining) / value;
                out.push(Units {
                    symbol: p.symbol.clone(),
                    amount: p.units * keep,
                });
                remaining = 0.0;
            }
        }
        out
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn pos(sym: &'static str, units: Real, price: Real) -> PositionInfo<&'static str> {
        PositionInfo {
            symbol: sym,
            units,
            price,
        }
    }

    #[test]
    fn proportional_scales_every_leg_uniformly() {
        // Two legs, 100 + 50 = 150 invested. Shortfall of 30 = 20% —
        // both legs should shrink by 20%.
        let positions = [pos("A", 10.0, 10.0), pos("B", 5.0, 10.0)];
        let out = Proportional.plan_scaledowns(&positions, 30.0);
        assert_eq!(out.len(), 2);
        // 10 units of A at 20% scale-down → 8 units target.
        assert!((out[0].amount - 8.0).abs() < 1e-9);
        // 5 units of B at 20% scale-down → 4 units target.
        assert!((out[1].amount - 4.0).abs() < 1e-9);
    }

    #[test]
    fn proportional_full_shortfall_fully_liquidates() {
        // Shortfall == invested → fraction = 1.0 → scale = 0 → target 0.
        let positions = [pos("A", 10.0, 10.0)];
        let out = Proportional.plan_scaledowns(&positions, 100.0);
        assert_eq!(out.len(), 1);
        assert!(out[0].amount.abs() < 1e-9);
    }

    #[test]
    fn proportional_zero_shortfall_emits_nothing() {
        let positions = [pos("A", 10.0, 10.0)];
        assert!(Proportional.plan_scaledowns(&positions, 0.0).is_empty());
    }

    #[test]
    fn largest_first_fully_closes_the_biggest_leg_first() {
        // A = 200, B = 100. Shortfall = 150 → close A (raises 200,
        // overshoots by 50, so partially close instead — keep 25% of A
        // → 2.5 units target). B untouched.
        let positions = [pos("A", 10.0, 20.0), pos("B", 10.0, 10.0)];
        let out = LargestFirst.plan_scaledowns(&positions, 150.0);
        assert_eq!(out.len(), 1);
        assert_eq!(out[0].symbol, "A");
        // Keep (200 - 150)/200 = 25% of the 10 units = 2.5 target.
        assert!((out[0].amount - 2.5).abs() < 1e-9);
    }

    #[test]
    fn largest_first_walks_multiple_legs_when_biggest_is_too_small() {
        // A = 50, B = 30, C = 20. Shortfall = 70 → close A (raises 50,
        // remaining 20), close B (raises 30, overshoots by 10 — keep
        // 33.33% of B). C untouched.
        let positions = [pos("A", 5.0, 10.0), pos("B", 3.0, 10.0), pos("C", 2.0, 10.0)];
        let out = LargestFirst.plan_scaledowns(&positions, 70.0);
        assert_eq!(out.len(), 2);
        assert_eq!(out[0].symbol, "A");
        assert!(out[0].amount.abs() < 1e-9, "A fully closed");
        assert_eq!(out[1].symbol, "B");
        // (30 - 20)/30 = 1/3 kept → 3 * 1/3 = 1.0 target.
        assert!((out[1].amount - 1.0).abs() < 1e-9, "B partially closed to 1 unit");
    }

    #[test]
    fn largest_first_zero_shortfall_emits_nothing() {
        let positions = [pos("A", 5.0, 10.0)];
        assert!(LargestFirst.plan_scaledowns(&positions, 0.0).is_empty());
    }
}
