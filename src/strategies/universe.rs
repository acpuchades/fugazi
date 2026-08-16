//! The declared-vs-floating symbol **universe**, as a pluggable trait.
//!
//! Shared by [`BasketStrategy`](super::BasketStrategy) and
//! [`MultiAssetStrategy`](super::MultiAssetStrategy). It used to live inside
//! `basket.rs`, so the multi-asset shape reached across into its sibling's
//! module for a concept neither owns.

// ---------------------------------------------------------------------------
// Universe — declared vs. floating symbol scope, as a pluggable trait.
// ---------------------------------------------------------------------------

/// The set of symbols a [`BasketStrategy`](super::BasketStrategy) (and
/// [`MultiAssetStrategy`](crate::strategies::MultiAssetStrategy)) is willing
/// to trade. Consumed as a `Box<dyn Universe<Sym>>` so a caller can plug
/// in their own scoping rule (a sector filter, a cap-weighted screen, a
/// dynamic universe rebuilt from an indicator feed, …) without touching
/// the strategy.
///
/// Two questions the strategy asks per bar:
///
/// - [`admits`](Self::admits) — should this symbol enter the strategy?
///   Called at symbol discovery; `false` filters the symbol out (no chain
///   built for it). All impls answer this on every seen symbol.
/// - [`required`](Self::required) — which symbols *must* be present on
///   every snapshot? Absence panics. Return an empty slice for lax /
///   floating universes; strict impls return the full list.
///
/// Three built-in impls ship:
///
/// - [`Floating`] — the default. Admits every symbol, requires none.
/// - [`AllOf<Sym>`] — strict declared universe: admits only listed
///   symbols, requires all of them present on every bar. Readiness gates
///   on every listed symbol scoring *and* sizing (enforced by the
///   strategy, which iterates [`required`](Self::required) itself).
/// - [`AnyOf<Sym>`] — lax declared universe: admits only listed symbols
///   but silently skips absent / unready members. No `required` list.
///
/// Installed via [`BasketStrategy::universe`](super::BasketStrategy::universe) (or the convenience
/// [`all_of`](super::BasketStrategy::all_of) / [`any_of`](super::BasketStrategy::any_of)
/// shortcuts, which wrap the built-in impls); the floating default is
/// installed for a fresh basket.
pub trait Universe<Sym>: Send + Sync {
    /// Whether `sym` is allowed into the strategy. Called at symbol
    /// discovery; `false` filters the symbol out (no chain built).
    fn admits(&self, sym: &Sym) -> bool;

    /// Symbols that must be present on every snapshot. Absence panics
    /// from `Strategy::update`. Return `&[]` for lax / floating universes.
    ///
    /// Strategies iterate this list themselves for readiness gating
    /// (each listed symbol must have both scored and sized before
    /// `is_ready()` returns `true`), so a `!required` implementation
    /// doesn't need any state — just report the required set.
    fn required(&self) -> &[Sym];
}

/// [`Universe`] impl that admits every symbol, requires none. The
/// default installed by a fresh [`BasketStrategy`](super::BasketStrategy) /
/// [`MultiAssetStrategy`](crate::strategies::MultiAssetStrategy).
#[derive(Debug, Clone, Copy, Default)]
pub struct Floating;

impl<Sym> Universe<Sym> for Floating {
    fn admits(&self, _sym: &Sym) -> bool {
        true
    }
    fn required(&self) -> &[Sym] {
        &[]
    }
}

/// Strict declared [`Universe`]: admits only listed symbols, requires
/// every listed symbol on every bar. Readiness gates on every listed
/// symbol scoring *and* sizing this bar. See [`Universe`] for the
/// contract.
#[derive(Debug, Clone)]
pub struct AllOf<Sym>(pub Vec<Sym>);

impl<Sym: PartialEq + Send + Sync> Universe<Sym> for AllOf<Sym> {
    fn admits(&self, sym: &Sym) -> bool {
        self.0.contains(sym)
    }
    fn required(&self) -> &[Sym] {
        &self.0
    }
}

/// Lax declared [`Universe`]: admits only listed symbols but silently
/// skips absent / unready members. Same per-bar filtering the floating
/// universe does, just narrowed to a fixed list. See [`Universe`].
#[derive(Debug, Clone)]
pub struct AnyOf<Sym>(pub Vec<Sym>);

impl<Sym: PartialEq + Send + Sync> Universe<Sym> for AnyOf<Sym> {
    fn admits(&self, sym: &Sym) -> bool {
        self.0.contains(sym)
    }
    fn required(&self) -> &[Sym] {
        &[]
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::prelude::*;
    use crate::types::Snapshot;
    // The universe rules are only observable through a strategy that consults
    // them, so these drive a `BasketStrategy`.
    use crate::indicators::sizing::equal_weight;
    use crate::indicators::{Close, Pick};
    use crate::strategies::BasketStrategy;

    fn snap(entries: &[(&'static str, Real)]) -> Snapshot<&'static str> {
        let mut s = Snapshot::new();
        for &(sym, close) in entries {
            let atom = Atom::new(Candle::new(close, close, close, close, 0.0));
            s.push(Some(sym), None, atom);
        }
        s
    }

    // ---------------- Selection functions --------------------------------

    // ---------------- Universe (all_of / any_of) -------------------------

    #[test]
    fn all_of_restricts_discovery_to_listed_symbols() {
        // Universe = {A, B}. Snapshot carries A, B, C — C should never get
        // a chain built.
        let mut strat: BasketStrategy<&'static str> =
            BasketStrategy::with_initial_equity(1_000.0)
                .scored_by(|sym: &&'static str| {
                    Close::of(Pick::matching(Selector::by_symbol(*sym)))
                })
                .sized_by(|_| equal_weight::<&'static str>(2))
                .top_bottom(1, 1)
                .all_of(["A", "B"]);
        strat.update(snap(&[("A", 100.0), ("B", 50.0), ("C", 200.0)]));
        assert!(strat.position(&"A").is_some());
        assert!(strat.position(&"B").is_some());
        assert!(
            strat.position(&"C").is_none(),
            "C is not in the declared universe; no chain / position should be built for it"
        );
    }

    #[test]
    #[should_panic(expected = "strict universe requires")]
    fn all_of_panics_when_listed_symbol_absent() {
        let mut strat: BasketStrategy<&'static str> =
            BasketStrategy::with_initial_equity(1_000.0)
                .scored_by(|sym: &&'static str| {
                    Close::of(Pick::matching(Selector::by_symbol(*sym)))
                })
                .sized_by(|_| equal_weight::<&'static str>(2))
                .top_bottom(1, 1)
                .all_of(["A", "B"]);
        // B is missing from the snapshot — strict-erroring convention.
        strat.update(snap(&[("A", 100.0)]));
    }

    #[test]
    fn all_of_is_ready_gates_on_every_listed_symbol_scoring() {
        // Score = SMA-3 so the first two bars score None for every symbol.
        // Under !all_of, is_ready must stay false until every listed
        // symbol has settled.
        let mut strat: BasketStrategy<&'static str> =
            BasketStrategy::with_initial_equity(1_000.0)
                .scored_by(|sym: &&'static str| {
                    crate::indicators::Sma::new(
                        Close::of(Pick::matching(Selector::by_symbol(*sym))),
                        3,
                    )
                })
                .sized_by(|_| equal_weight::<&'static str>(2))
                .top_bottom(1, 1)
                .all_of(["A", "B"]);
        assert!(!strat.is_ready(), "empty basket cannot be ready under all_of");
        strat.update(snap(&[("A", 100.0), ("B", 50.0)]));
        assert!(!strat.is_ready(), "first bar: SMA-3 not warmed for either");
        strat.update(snap(&[("A", 101.0), ("B", 51.0)]));
        assert!(!strat.is_ready(), "second bar: SMA-3 still not warmed");
        strat.update(snap(&[("A", 102.0), ("B", 52.0)]));
        assert!(
            strat.is_ready(),
            "third bar: both listed symbols have scored — ready"
        );
    }

    #[test]
    fn any_of_ignores_absent_symbols() {
        // Universe = {A, B} lax. B is missing on this bar — must not panic.
        let mut strat: BasketStrategy<&'static str> =
            BasketStrategy::with_initial_equity(1_000.0)
                .scored_by(|sym: &&'static str| {
                    Close::of(Pick::matching(Selector::by_symbol(*sym)))
                })
                .sized_by(|_| equal_weight::<&'static str>(2))
                .top_bottom(1, 1)
                .any_of(["A", "B"]);
        strat.update(snap(&[("A", 100.0)]));
        assert!(strat.position(&"A").is_some());
        // No B in the snapshot: no chain built yet (it hasn't been seen).
        assert!(strat.position(&"B").is_none());
        // any_of doesn't gate readiness on absence.
        assert!(strat.is_ready());
    }

    #[test]
    fn any_of_restricts_discovery_to_listed_symbols() {
        // Same shape as the all_of restriction test, but without the
        // presence-required panic. C in the snapshot must still be
        // filtered out at discovery.
        let mut strat: BasketStrategy<&'static str> =
            BasketStrategy::with_initial_equity(1_000.0)
                .scored_by(|sym: &&'static str| {
                    Close::of(Pick::matching(Selector::by_symbol(*sym)))
                })
                .sized_by(|_| equal_weight::<&'static str>(2))
                .top_bottom(1, 1)
                .any_of(["A", "B"]);
        strat.update(snap(&[("A", 100.0), ("B", 50.0), ("C", 200.0)]));
        assert!(strat.position(&"A").is_some());
        assert!(strat.position(&"B").is_some());
        assert!(strat.position(&"C").is_none());
    }

    #[test]
    fn floating_universe_is_ready_by_default() {
        // Sanity: the default (no all_of / no any_of) leaves is_ready as
        // the trait default.
        let strat: BasketStrategy<&'static str> =
            BasketStrategy::with_initial_equity(1_000.0);
        assert!(strat.is_ready());
    }

    #[test]
    fn custom_universe_impl_plugs_in_without_touching_the_strategy() {
        // A caller can install any `Universe` impl via `.universe(...)`
        // and BasketStrategy consumes it through the trait alone — the
        // demonstration here is a `PrefixUniverse` that admits any
        // symbol starting with a given letter. No strategy code
        // recognizes "prefix universes"; it's all trait-driven.
        struct PrefixUniverse(char);
        impl Universe<&'static str> for PrefixUniverse {
            fn admits(&self, sym: &&'static str) -> bool {
                sym.starts_with(self.0)
            }
            fn required(&self) -> &[&'static str] {
                &[]
            }
        }

        let mut strat: BasketStrategy<&'static str> =
            BasketStrategy::with_initial_equity(1_000.0)
                .scored_by(|sym: &&'static str| {
                    Close::of(Pick::matching(Selector::by_symbol(*sym)))
                })
                .sized_by(|_| equal_weight::<&'static str>(2))
                .top_bottom(1, 1)
                .universe(PrefixUniverse('B'));
        // Only 'B'-prefixed symbols get chains built.
        strat.update(snap(&[("BTC", 100.0), ("ETH", 200.0), ("BNB", 300.0)]));
        assert!(strat.position(&"BTC").is_some());
        assert!(strat.position(&"BNB").is_some());
        assert!(
            strat.position(&"ETH").is_none(),
            "ETH doesn't start with 'B' so PrefixUniverse rejects it"
        );
        // No required list → is_ready stays true (lax semantics).
        assert!(strat.is_ready());
    }
}
