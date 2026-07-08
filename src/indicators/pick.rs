//! Cross-asset projection: pick one asset's [`Atom`] out of a
//! [`Snapshot<Sym>`] by a partial-key query.
//!
//! [`Pick`] holds a [`Selector<Sym>`] (the query) and reads from any
//! indicator whose output is a `Snapshot<Sym>`. Two paths:
//!
//! - **Empty selector** (`Selector::default()` / `Pick::new()`) — the
//!   *no-query* path: the snapshot must contain exactly one entry, whose atom
//!   is returned. This is the single-series ergonomic shortcut for strategies
//!   fed a multi-asset-shaped input by a driver that only ever populates one
//!   entry; a snapshot of size 2+ **panics** rather than silently picking an
//!   arbitrary asset (see [`Snapshot::sole_atom`]).
//! - **Non-empty selector** — the *structural-match* path: forwards to
//!   [`Snapshot::find`], which returns the first stored atom whose tag
//!   matches the query (`None` fields on the query are wildcards).
//!
//! Cross-asset expressions then compose from the same primitives as
//! single-asset ones — every source-generic candle leaf ([`Close`](super::Close),
//! [`Atr`](super::Atr), [`Year`](super::Year), …) drops on top:
//!
//! ```ignore
//! use fugazi::indicators::{Close, Pick};
//! use fugazi::prelude::*;
//! // BTC/ETH close spread as a plain `Real`-output indicator over Snapshot.
//! let spread = Close::of(Pick::matching(Selector::by_symbol("BTC")))
//!     .sub(Close::of(Pick::matching(Selector::by_symbol("ETH"))));
//! ```

use std::marker::PhantomData;

use crate::indicator::Indicator;
use crate::indicators::Identity;
use crate::types::{Atom, Selector, Snapshot};

/// Projects one asset's [`Atom`] out of a [`Snapshot<Sym>`], either by a
/// wildcard-aware structural [`Selector`] match or, when the selector is
/// empty, by the [`Snapshot::sole_atom`] single-entry unpack.
///
/// `Input = S::Input`, `Output = Atom`. The default source
/// `Identity<Snapshot<Sym>>` makes `Pick::new()` a leaf that consumes a
/// [`Snapshot`] directly; `Pick::of(selector, source)` re-roots it onto any
/// indicator that emits a `Snapshot<Sym>`.
///
/// Emits `None` on bars where the query matches no entry — the same
/// `None`-until-warm convention every other leaf uses, so a downstream
/// comparison stays `None` until the projected asset first appears.
#[derive(Debug, Clone)]
pub struct Pick<Sym, S = Identity<Snapshot<Sym>>> {
    selector: Selector<Sym>,
    source: S,
    /// The last atom projected out; `None` before the first bar or if the last
    /// snapshot had no matching entry.
    pub value: Option<Atom>,
    _phantom: PhantomData<fn() -> Sym>,
}

impl<Sym> Pick<Sym, Identity<Snapshot<Sym>>> {
    /// A [`Pick`] with an *empty* selector (both fields `None`) rooted on the
    /// raw [`Snapshot`] input stream. Every `update` runs the
    /// [`Snapshot::sole_atom`] single-entry unpack: the snapshot must contain
    /// exactly one atom, otherwise the call **panics**.
    pub fn new() -> Self {
        Self::of(Selector::default(), Identity::new())
    }

    /// A [`Pick`] with the given [`Selector`] rooted on the raw [`Snapshot`]
    /// input stream — the workhorse "structural query" constructor.
    ///
    /// `Selector::default()` (empty) is legal here too; if you know that's
    /// what you want, prefer the explicit [`Pick::new`] alias.
    pub fn matching(selector: Selector<Sym>) -> Self {
        Self::of(selector, Identity::new())
    }
}

impl<Sym, S> Pick<Sym, S> {
    /// A [`Pick`] with the given [`Selector`] rooted on a custom
    /// snapshot-emitting source. Empty selector still dispatches to
    /// [`Snapshot::sole_atom`].
    pub fn of(selector: Selector<Sym>, source: S) -> Self {
        Self {
            selector,
            source,
            value: None,
            _phantom: PhantomData,
        }
    }

    /// The [`Selector`] this pick queries with. See [`Selector::is_empty`]
    /// for the "no query" case.
    pub fn selector(&self) -> &Selector<Sym> {
        &self.selector
    }
}

impl<Sym> Default for Pick<Sym, Identity<Snapshot<Sym>>> {
    fn default() -> Self {
        Self::new()
    }
}

impl<Sym, S> Indicator for Pick<Sym, S>
where
    Sym: Clone + PartialEq,
    S: Indicator<Output = Snapshot<Sym>>,
{
    type Input = S::Input;
    type Output = Atom;

    fn update(&mut self, input: S::Input) -> Option<Atom> {
        self.value = self.source.update(input).and_then(|snap| {
            if self.selector.is_empty() {
                snap.sole_atom().cloned()
            } else {
                snap.find(&self.selector).cloned()
            }
        });
        self.value.clone()
    }

    fn value(&self) -> Option<Atom> {
        self.value.clone()
    }

    fn warm_up_period(&self) -> usize {
        self.source.warm_up_period().max(1)
    }

    fn unstable_period(&self) -> usize {
        self.source.unstable_period()
    }

    fn reset(&mut self) {
        self.source.reset();
        self.value = None;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::types::{Candle, Frequency, Real};

    fn snap<'a>(
        pairs: impl IntoIterator<Item = (Option<String>, Option<Frequency>, Real)>,
    ) -> Snapshot<String> {
        let mut s = Snapshot::new();
        for (sym, freq, close) in pairs {
            s.push(sym, freq, Atom::new(Candle::new(1.0, 1.0, 1.0, close, 1.0)));
        }
        s
    }

    #[test]
    fn matching_picks_by_symbol() {
        let mut p = Pick::<String>::matching(Selector::by_symbol("BTC"));
        let out = p.update(snap([
            (Some("BTC".into()), None, 10.0),
            (Some("ETH".into()), None, 20.0),
        ]));
        assert_eq!(out.map(|a| a.candle.close), Some(10.0));
    }

    #[test]
    fn matching_picks_wildcards_over_freq() {
        // Query on symbol only; entries carry an extra freq field.
        let mut p = Pick::<String>::matching(Selector::by_symbol("BTC"));
        let out = p.update(snap([
            (Some("BTC".into()), Some(Frequency::Hour(1)), 42.0),
            (Some("ETH".into()), Some(Frequency::Hour(1)), 100.0),
        ]));
        assert_eq!(out.map(|a| a.candle.close), Some(42.0));
    }

    #[test]
    fn matching_missing_yields_none() {
        let mut p = Pick::<String>::matching(Selector::by_symbol("SOL"));
        let out = p.update(snap([
            (Some("BTC".into()), None, 10.0),
            (Some("ETH".into()), None, 20.0),
        ]));
        assert_eq!(out, None);
        assert_eq!(p.value(), None);
    }

    #[test]
    fn new_no_query_unpacks_single_entry_snapshot() {
        let mut p = Pick::<String>::new();
        let out = p.update(snap([(Some("BTC".into()), None, 99.0)]));
        assert_eq!(out.map(|a| a.candle.close), Some(99.0));
    }

    #[test]
    fn new_no_query_unpacks_untagged_single_entry_snapshot() {
        // The single-series driver hot path: `Snapshot::of_atom(atom)` has no
        // symbol / freq tag, yet the empty-selector `Pick::new()` still
        // unpacks it via sole_atom().
        let mut p = Pick::<String>::new();
        let atom = Atom::new(Candle::new(1.0, 1.0, 1.0, 7.0, 1.0));
        let out = p.update(Snapshot::of_atom(atom));
        assert_eq!(out.map(|a| a.candle.close), Some(7.0));
    }

    #[test]
    fn new_no_query_returns_none_on_empty_snapshot() {
        let mut p = Pick::<String>::new();
        let out = p.update(Snapshot::<String>::new());
        assert_eq!(out, None);
    }

    #[test]
    #[should_panic(expected = "Snapshot::sole_atom: expected a single-entry snapshot")]
    fn new_no_query_panics_on_multi_entry_snapshot() {
        let mut p = Pick::<String>::new();
        p.update(snap([
            (Some("BTC".into()), None, 10.0),
            (Some("ETH".into()), None, 20.0),
        ]));
    }

    #[test]
    fn warm_up_delegates_to_source() {
        assert_eq!(Pick::<String>::new().warm_up_period(), 1);
        assert_eq!(
            Pick::<String>::matching(Selector::by_symbol("BTC")).warm_up_period(),
            1
        );
    }

    #[test]
    fn reset_clears_cached_value() {
        let mut p = Pick::<String>::matching(Selector::by_symbol("BTC"));
        p.update(snap([(Some("BTC".into()), None, 42.0)]));
        assert_eq!(p.value().map(|a| a.candle.close), Some(42.0));
        p.reset();
        assert_eq!(p.value(), None);
    }
}
