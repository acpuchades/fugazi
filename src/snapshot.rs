//! Multi-asset framing: a keyed [`Selector`] naming which asset in a
//! [`Snapshot`] to read, and the [`Snapshot`] itself — a per-bar collection of
//! tagged [`Atom`]s that lets a strategy or an indicator reason about more
//! than one instrument at a time.

use std::sync::Arc;

use crate::market::Atom;
use crate::time::Frequency;

/// A **selector**: a matching predicate naming *which* asset in a [`Snapshot`]
/// a [`Pick`](crate::indicators::Pick) should read.
///
/// Both fields are `Option` so a caller can specify only the ones they need:
/// `Selector::by_symbol("BTC")` matches every BTC entry regardless of
/// frequency, `Selector::by_freq(Frequency::Hour(1))` matches every hourly
/// entry regardless of symbol, `Selector::exact(sym, freq)` matches a single
/// tagged entry. A fully-empty selector (both fields `None`, the [`Default`])
/// is legal — it stands for "no query at all" and drives [`Pick`](crate::indicators::Pick) onto the
/// [`Snapshot::sole_atom`] path (see [`Selector::is_empty`]) rather than a
/// structural match.
///
/// # Matching semantics ([`Selector::matches`])
///
/// A query selector matches a snapshot entry when each field either has no
/// query (`None`, a wildcard) or agrees with the entry's tag. That means
/// `pick(symbol=BTC)` finds an entry tagged `{symbol=BTC, freq=Some(1h)}`
/// even though the query is silent on `freq`. Symmetric: a query
/// `pick(freq=1h)` matches `{symbol=Some(BTC), freq=1h}` without knowing
/// the symbol. An empty selector matches every entry; a *fully-empty*
/// query is semantically "no query" — the caller almost certainly meant
/// "single-entry unpack", so [`Pick`](crate::indicators::Pick) dispatches on [`is_empty`](Self::is_empty)
/// and never runs [`Snapshot::find`] on an empty query.
///
/// # Selector as a matcher, not a key
///
/// A selector is a **predicate**, not the [`Snapshot`] entry key. [`Snapshot`]
/// entries carry raw `(Option<Sym>, Option<Frequency>, Atom)` tuples; a
/// selector only decides whether it *matches* an entry. That means a
/// snapshot never needs `Sym: Eq + Hash` (just `PartialEq` for the match
/// predicate) and duplicates at push time are allowed — the first-match rule
/// on [`Snapshot::find`] disambiguates.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Selector<Sym> {
    pub symbol: Option<Sym>,
    pub freq: Option<Frequency>,
}

impl<Sym> Default for Selector<Sym> {
    fn default() -> Self {
        Self {
            symbol: None,
            freq: None,
        }
    }
}

impl<Sym> Selector<Sym> {
    /// Build a selector. Both fields may be `None` — the empty selector is
    /// legal and stands for the [`Pick`](crate::indicators::Pick) single-entry-unpack path (see
    /// [`Selector::is_empty`]).
    pub fn new(symbol: Option<Sym>, freq: Option<Frequency>) -> Self {
        Self { symbol, freq }
    }

    /// Selector matching every entry whose `symbol` equals `sym`, regardless
    /// of frequency.
    pub fn by_symbol(sym: impl Into<Sym>) -> Self {
        Self {
            symbol: Some(sym.into()),
            freq: None,
        }
    }

    /// Selector matching every entry whose `freq` equals `freq`, regardless
    /// of symbol.
    pub fn by_freq(freq: Frequency) -> Self {
        Self {
            symbol: None,
            freq: Some(freq),
        }
    }

    /// Selector matching a single `(symbol, freq)` pair exactly.
    pub fn exact(sym: impl Into<Sym>, freq: Frequency) -> Self {
        Self {
            symbol: Some(sym.into()),
            freq: Some(freq),
        }
    }

    /// True when both fields are `None` — the "no query" case that [`Pick`](crate::indicators::Pick)
    /// treats as a single-entry unpack ([`Snapshot::sole_atom`]) rather than
    /// a structural match.
    pub fn is_empty(&self) -> bool {
        self.symbol.is_none() && self.freq.is_none()
    }
}

impl<Sym: PartialEq> Selector<Sym> {
    /// Match this selector as a query against a snapshot entry's `(symbol,
    /// freq)` tags: each `None` field on the query is a wildcard (matches any
    /// entry value); a `Some` field must equal the entry's field.
    pub fn matches(&self, symbol: Option<&Sym>, freq: Option<Frequency>) -> bool {
        (self.symbol.is_none() || self.symbol.as_ref() == symbol)
            && (self.freq.is_none() || self.freq == freq)
    }
}

/// A per-bar snapshot of several assets — a **series** of tagged [`Atom`]s.
///
/// The multi-asset input frame that lets a strategy or an indicator reason
/// about more than one instrument at a time. Each entry is a
/// `(Option<Sym>, Option<Frequency>, Atom)` tuple: the tag is what a
/// [`Selector`] matches against; the atom is what a [`Pick`](crate::indicators::Pick)
/// projects out.
///
/// The storage is deliberately a sequence rather than a hashmap: [`Selector`]
/// is a predicate, not a key, so entries never dedup by tag (`Sym: PartialEq`
/// is enough — no `Eq + Hash` bound) and duplicates at push time are legal
/// with first-match-wins on [`Snapshot::find`]. Iteration order is insertion
/// order, so a driver that pushes entries deterministically gets a
/// deterministic scan for free.
///
/// # Cloning is a refcount bump
///
/// The entries live behind an [`Arc`], so `clone` costs one atomic increment
/// and no allocation. That is load-bearing rather than an optimisation: a
/// snapshot is fed to *every* signal slot of *every* symbol each bar (see
/// [`MultiAssetStrategy::update`](crate::strategies::MultiAssetStrategy)), and
/// every binary node in an expression tree clones its input again because
/// [`Combine`](crate::indicators::Combine) feeds both sides. With a plain `Vec`
/// the per-bar cost grew with the *square* of the universe — an N-symbol run
/// deep-copied an N-entry vector N × slots times per bar.
///
/// The mutators ([`push`](Self::push), [`remove_matching`](Self::remove_matching))
/// are copy-on-write via [`Arc::make_mut`]: while a snapshot is being built by
/// the driver it is uniquely owned, so they mutate in place and the shared case
/// never arises on the hot path. This is the same treatment
/// [`OverlayInfo`](crate::market::OverlayInfo) already gets, for the same
/// reason.
///
/// Cross-asset expressions compose from the same primitives as single-asset
/// ones:
///
/// ```ignore
/// use fugazi::indicators::{Close, Pick};
/// use fugazi::prelude::*;
/// // BTC/ETH close spread as a first-class Real-output indicator over a
/// // Snapshot<String>.
/// let spread = Close::of(Pick::matching(Selector::by_symbol("BTC")))
///     .sub(Close::of(Pick::matching(Selector::by_symbol("ETH"))));
/// ```
#[derive(Debug, Clone)]
pub struct Snapshot<Sym> {
    entries: Arc<Vec<Entry<Sym>>>,
}

/// One tagged atom inside a [`Snapshot`]: `(symbol, frequency, atom)`, with
/// both tags optional. Named so the shared storage type stays readable.
pub type Entry<Sym> = (Option<Sym>, Option<Frequency>, Atom);

impl<Sym> Snapshot<Sym> {
    /// An empty snapshot with no assets.
    pub fn new() -> Self {
        Self {
            entries: Arc::new(Vec::new()),
        }
    }

    /// A single-entry snapshot carrying just this atom with no `(symbol,
    /// freq)` tag. Convenient for the single-series driver hot path — an
    /// empty [`Selector`] on [`Pick::new`](crate::indicators::Pick::new) will
    /// unpack the sole atom without inspecting the tag.
    ///
    /// Note: an untagged entry is skipped by
    /// [`fugazi::backtest::run`](crate::backtest::run)'s wallet-pricing
    /// loop — there's no symbol to price against — so a single-series run
    /// that expects the wallet to be marked to market and book fills should
    /// use [`single`](Self::single) instead.
    pub fn of_atom(atom: Atom) -> Self {
        Self {
            entries: Arc::new(vec![(None, None, atom)]),
        }
    }

    /// A single-entry snapshot tagged with `symbol` and no `freq`. The
    /// single-series shortcut for building a driver-ready snapshot:
    /// [`fugazi::backtest::run`](crate::backtest::run) prices the wallet on
    /// this entry's `symbol` each bar.
    pub fn single(symbol: Sym, atom: Atom) -> Self {
        Self {
            entries: Arc::new(vec![(Some(symbol), None, atom)]),
        }
    }

    /// Append a tagged atom to the snapshot. Duplicates are allowed —
    /// [`Snapshot::find`] returns the first match on a query, so
    /// insertion order determines precedence.
    ///
    /// Copy-on-write: mutates in place while this snapshot is the sole owner of
    /// its entries (the case during driver construction), and clones them first
    /// if it is not.
    pub fn push(&mut self, symbol: Option<Sym>, freq: Option<Frequency>, atom: Atom)
    where
        Sym: Clone,
    {
        Arc::make_mut(&mut self.entries).push((symbol, freq, atom));
    }

    /// Number of tagged atoms in this snapshot.
    pub fn len(&self) -> usize {
        self.entries.len()
    }

    /// True if this snapshot carries no atoms.
    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }

    /// Iterate over `(symbol, freq, atom)` triples in insertion order.
    pub fn iter(&self) -> impl Iterator<Item = (Option<&Sym>, Option<Frequency>, &Atom)> {
        self.entries
            .iter()
            .map(|(s, f, a)| (s.as_ref(), *f, a))
    }

    /// The first atom in the snapshot, or `None` if empty. Never panics,
    /// even on multi-entry snapshots — this is the primitive
    /// [`PickAny::new`](crate::indicators::PickAny::new) uses for
    /// symbol-agnostic reads (calendar accessors like
    /// [`Year`](crate::indicators::Year) / [`Hour`](crate::indicators::Hour)
    /// only inspect [`Atom::time`], which every entry in a well-formed
    /// snapshot shares, so "any one" is defined and stable).
    ///
    /// Contrast with [`sole_atom`](Self::sole_atom), which is the
    /// single-series safety net: it panics on 2+ entries because most price
    /// leaves (`!close`, `!high`, …) genuinely depend on *which* asset.
    pub fn any_atom(&self) -> Option<&Atom> {
        self.entries.first().map(|(_, _, a)| a)
    }

    /// The sole atom in a single-entry snapshot, if there is exactly one.
    /// Returns `None` for empty snapshots; **panics** with a diagnostic
    /// message when the snapshot has 2+ entries. This is the primitive
    /// [`Pick::new`](crate::indicators::Pick::new) uses for its "no query —
    /// this is a single-series run" path: a single-series driver always
    /// feeds a size-1 snapshot, so a 2+ read means the run was accidentally
    /// hooked up to multi-asset input and the loud failure is preferable to
    /// silently returning an arbitrary asset.
    ///
    /// For sources that are symbol-agnostic (calendar accessors that only
    /// read `atom.time`), see [`any_atom`](Self::any_atom); for the
    /// non-panicking twin, [`lone_atom`](Self::lone_atom).
    pub fn sole_atom(&self) -> Option<&Atom> {
        // Only priceable entries count. An overlay-only series — a funding
        // rate, an open interest — is stacked into the snapshot beside the
        // price series and is reached deliberately, with `!pick`; it must stay
        // invisible to the implicit unpack, or attaching one would break every
        // bare `!close` in a single-asset strategy that never asked for it.
        let mut priceable = self.entries.iter().filter(|(_, _, a)| a.is_priceable());
        let first = priceable.next();
        match (first, priceable.count()) {
            (None, _) => None,
            (Some(entry), 0) => Some(&entry.2),
            (Some(_), extra) => {
                let n = extra + 1;
                panic!(
                "Snapshot::sole_atom: expected a single-entry snapshot, got {n} entries. \
                 This usually means a strategy authored for single-series input was fed a \
                 multi-asset snapshot, and the implicit no-arg `Pick::new()` on one of its \
                 leaves could not choose an asset. \n\
                 \n\
                 To fix: pick which asset each leaf reads.\n\
                 \n\
                 - In YAML, add a `!pick {{ symbol, freq }}` source to each affected \
                 leaf — e.g. `!close {{ source: !pick {{ symbol: BTC }} }}`. \n\
                 - In Rust, replace `Pick::new()` with `Pick::matching(Selector::by_symbol(...))` \
                 (or `by_freq(...)` / `exact(...)`)."
                )
            }
        }
    }

    /// The sole priceable atom, or `None` when there isn't exactly one — the
    /// **non-panicking** twin of [`sole_atom`](Self::sole_atom).
    ///
    /// [`Pick::rooted`](crate::indicators::Pick::rooted) uses this as its
    /// fallback when the blessed symbol doesn't match. The distinction from
    /// `sole_atom` is the whole point: in a rooted context a 2+ entry snapshot
    /// is *normal* — it just means the blessed leg is absent this bar (a
    /// listing gap, a different exchange calendar), which should read `None`
    /// and let the caller roll the symbol off, not abort the run. The panic in
    /// `sole_atom` is for the genuinely mis-wired case, where nothing named an
    /// asset at all.
    pub fn lone_atom(&self) -> Option<&Atom> {
        let mut priceable = self.entries.iter().filter(|(_, _, a)| a.is_priceable());
        match (priceable.next(), priceable.next()) {
            (Some(entry), None) => Some(&entry.2),
            _ => None,
        }
    }
}

impl<Sym: PartialEq> Snapshot<Sym> {
    /// Structural lookup: return the first stored atom whose tag matches
    /// `query` under [`Selector::matches`] (each `None` field on the query
    /// is a wildcard). Scans entries in insertion order — the caller's push
    /// sequence is the precedence when a query could match more than one
    /// entry; disambiguate by supplying both `symbol` and `freq`.
    pub fn find(&self, query: &Selector<Sym>) -> Option<&Atom> {
        self.entries.iter().find_map(|(s, f, a)| {
            if query.matches(s.as_ref(), *f) {
                Some(a)
            } else {
                None
            }
        })
    }

    /// Remove every entry whose tag matches `query`. Used by the Python
    /// bindings' `__setitem__` to implement "assignment overwrites" — Rust
    /// callers who want raw list semantics should use [`push`](Self::push)
    /// directly.
    pub fn remove_matching(&mut self, query: &Selector<Sym>)
    where
        Sym: Clone,
    {
        Arc::make_mut(&mut self.entries).retain(|(s, f, _)| !query.matches(s.as_ref(), *f));
    }
}

impl<Sym: PartialEq> PartialEq for Snapshot<Sym> {
    fn eq(&self, other: &Self) -> bool {
        self.entries == other.entries
    }
}

impl<Sym> Default for Snapshot<Sym> {
    fn default() -> Self {
        Self::new()
    }
}

impl<Sym> FromIterator<Entry<Sym>> for Snapshot<Sym> {
    fn from_iter<I: IntoIterator<Item = Entry<Sym>>>(iter: I) -> Self {
        Self {
            entries: Arc::new(iter.into_iter().collect()),
        }
    }
}

impl<Sym> From<Atom> for Snapshot<Sym> {
    fn from(atom: Atom) -> Self {
        Self::of_atom(atom)
    }
}

impl<Sym> From<crate::market::Candle> for Snapshot<Sym> {
    fn from(candle: crate::market::Candle) -> Self {
        Self::of_atom(Atom::new(candle))
    }
}

#[cfg(test)]
mod overlay_only_tests {
    use super::*;
    use crate::market::{Candle, OverlayInfo, OverlayValue, Schema};
    use crate::time::Timestamp;

    fn funding_atom(rate: f64) -> Atom {
        let mut b = Schema::builder();
        b.add_real("funding_rate");
        let schema = b.finish();
        Atom::overlay_only(
            OverlayInfo::new(schema, vec![OverlayValue::Real(rate)]),
            Timestamp(0),
        )
    }

    #[test]
    fn sole_atom_ignores_an_overlay_only_entry() {
        // The point of the whole change: stacking a funding series next to a
        // price series must not break a single-asset strategy's bare `!close`,
        // which reaches its bar through the implicit no-arg unpack.
        let mut snap: Snapshot<String> = Snapshot::new();
        snap.push(Some("BTC".into()), None, Atom::new(Candle::new(1.0, 2.0, 0.5, 1.5, 10.0)));
        snap.push(Some("BTC.funding".into()), None, funding_atom(0.0003));

        let sole = snap.sole_atom().expect("the one priceable entry");
        assert_eq!(sole.candle.unwrap().close, 1.5);
    }

    #[test]
    fn sole_atom_still_panics_on_two_real_bars() {
        // The ambiguity it exists to catch is unchanged: two *priceable*
        // entries remain a programming error, not a silent arbitrary pick.
        let mut snap: Snapshot<String> = Snapshot::new();
        snap.push(Some("BTC".into()), None, Atom::new(Candle::new(1.0, 2.0, 0.5, 1.5, 10.0)));
        snap.push(Some("ETH".into()), None, Atom::new(Candle::new(2.0, 3.0, 1.5, 2.5, 20.0)));
        assert!(std::panic::catch_unwind(|| snap.sole_atom()).is_err());
    }

    #[test]
    fn an_all_overlay_snapshot_has_no_sole_atom() {
        let mut snap: Snapshot<String> = Snapshot::new();
        snap.push(Some("BTC.funding".into()), None, funding_atom(0.0003));
        assert!(snap.sole_atom().is_none());
    }
}
