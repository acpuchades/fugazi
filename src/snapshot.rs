//! Multi-asset framing: a keyed [`Selector`] naming which asset in a
//! [`Snapshot`] to read, and the [`Snapshot`] itself — a per-bar collection of
//! tagged [`Atom`]s that lets a strategy or an indicator reason about more
//! than one instrument at a time.

use std::sync::Arc;

use crate::market::Atom;
use crate::time::Frequency;

/// The symbol type the spec, runtime, CLI and Python layers key assets by: a
/// shared string that **carries a precomputed hash of its own bytes**.
///
/// # Why a shared string
///
/// A symbol is **cloned constantly and mutated never**: the driver clones one
/// per symbol per bar to price the wallet, every `Snapshot` entry holds one, and
/// each spec-built leaf carries one in its `Selector`. With `String` each of
/// those is a heap allocation and a memcpy of the same handful of bytes; with a
/// shared `Arc<str>` inside it is a refcount bump, and a run interns one
/// allocation per *distinct* symbol rather than one per symbol per bar.
///
/// Measured (`docs/PERFORMANCE.md`): snapshot construction went from 3.00 to
/// 2.00 allocations per bar and 201 to 160 bytes per bar. The Python bindings
/// gain most, because they rebuild symbols across the FFI boundary on every
/// call — see `python/bench/bench_run.py`.
///
/// # Why it carries a hash
///
/// This was a bare `pub type Symbol = Arc<str>`, which compares by **content**.
/// The fat pointer carries the length inline, so a length mismatch is free, and
/// `std`'s `Arc` specialisation short-circuits pointer-equal operands — but two
/// *different* symbols of the *same* length force a deref and a `memcmp`, and
/// real universes are overwhelmingly same-length (`BTCUSDT`/`ETHUSDT`,
/// `AAPL`/`MSFT`).
///
/// [`Snapshot::find`] runs once per `!pick`-rooted leaf per symbol per bar and
/// rejects `N − 1` entries each time, so that `memcmp` measured at **76% of
/// `find` and 47% of a 64-symbol backtest** — a **1.89×** whole-run gap between
/// an equal-length universe and a ragged one (`docs/PERFORMANCE.md` Phase 13,
/// `cargo bench --bench snapshot_scan`).
///
/// [`eq`](Symbol::eq) therefore compares the hash first. Two symbols with
/// different hashes cannot be equal, so the bytes are never touched; equal
/// hashes fall through to the real comparison, so a collision costs one wasted
/// `memcmp` and **cannot produce a wrong answer**.
///
/// # Why not compare pointers
///
/// Cheaper still, and unsound. [`symbol`] is `Arc::from`, a fresh allocation per
/// call, so the selector's symbol (interned when the spec is built) and the
/// snapshot's (interned when data is loaded) are different allocations holding
/// the same bytes — `symbol("BTC") ` twice over already gives two pointers.
/// Pointer equality would match nothing and the run would report a plausible
/// **zero-fill backtest**, the failure this crate goes out of its way to make
/// impossible. Canonical interning could fix that, at the cost of a global
/// invariant no type enforces; hashing needs no invariant at all.
///
/// # What is preserved
///
/// The indicator and strategy layers stay **generic** over `Sym`; this type is
/// only what the runtime-typed layers pick, and nothing here adds a bound to
/// them. A pure-Rust caller can still use `&'static str` and pay nothing.
///
/// `Symbol` derefs to `str` and implements `Borrow<str>`, so a
/// `HashMap<Symbol, _>` is still queryable with a plain `&str` and comparisons
/// against string literals work unchanged. **[`Hash`] delegates to the bytes,
/// not to the cached hash** — it has to, or `Borrow<str>` would be unsound
/// (borrowed and owned forms must hash alike). So this buys cheap *equality*,
/// not cheap hashing.
#[derive(Clone)]
pub struct Symbol {
    /// FxHash of `name`. Derived, so it never participates in ordering or in
    /// [`Hash`] — only in the equality fast path.
    hash: u64,
    name: Arc<str>,
}

impl Symbol {
    /// The symbol's text.
    pub fn as_str(&self) -> &str {
        &self.name
    }

    /// The shared handle backing this symbol.
    pub fn as_arc(&self) -> &Arc<str> {
        &self.name
    }

    fn hash_of(s: &str) -> u64 {
        use std::hash::Hasher;
        let mut h = crate::hash::FxHasher::default();
        h.write(s.as_bytes());
        h.finish()
    }
}

/// Compares the cached hash before the bytes — see the type docs. Equivalent to
/// comparing the strings, because the hash is a pure function of them.
impl PartialEq for Symbol {
    #[inline]
    fn eq(&self, other: &Self) -> bool {
        self.hash == other.hash && self.name == other.name
    }
}

impl Eq for Symbol {}

/// Delegates to the **bytes**, so that `Borrow<str>` holds: a `HashMap<Symbol,
/// _>` must hash an owned symbol exactly as it hashes the `&str` it is looked up
/// with. Hashing the cached `hash` field instead would break every `&str` query.
impl std::hash::Hash for Symbol {
    #[inline]
    fn hash<H: std::hash::Hasher>(&self, state: &mut H) {
        self.name.hash(state);
    }
}

impl PartialOrd for Symbol {
    fn partial_cmp(&self, other: &Self) -> Option<std::cmp::Ordering> {
        Some(self.cmp(other))
    }
}

/// Lexicographic on the text. The cached hash takes no part — an ordering by
/// hash would be stable but meaningless, and `marked_equity` sorts symbols to
/// give its sum a canonical order that a reader can reason about.
impl Ord for Symbol {
    fn cmp(&self, other: &Self) -> std::cmp::Ordering {
        self.name.cmp(&other.name)
    }
}

impl std::ops::Deref for Symbol {
    type Target = str;
    #[inline]
    fn deref(&self) -> &str {
        &self.name
    }
}

impl AsRef<str> for Symbol {
    #[inline]
    fn as_ref(&self) -> &str {
        &self.name
    }
}

impl std::borrow::Borrow<str> for Symbol {
    #[inline]
    fn borrow(&self) -> &str {
        &self.name
    }
}

/// Prints as the bare string, like the `Arc<str>` this replaced — so error
/// messages and `{:?}` output are unchanged.
impl std::fmt::Debug for Symbol {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        std::fmt::Debug::fmt(&self.name, f)
    }
}

impl std::fmt::Display for Symbol {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        std::fmt::Display::fmt(&*self.name, f)
    }
}

impl PartialEq<str> for Symbol {
    fn eq(&self, other: &str) -> bool {
        &*self.name == other
    }
}

impl PartialEq<&str> for Symbol {
    fn eq(&self, other: &&str) -> bool {
        &*self.name == *other
    }
}

impl PartialEq<String> for Symbol {
    fn eq(&self, other: &String) -> bool {
        &*self.name == other.as_str()
    }
}

impl PartialEq<Symbol> for str {
    fn eq(&self, other: &Symbol) -> bool {
        self == &*other.name
    }
}

impl PartialEq<Symbol> for &str {
    fn eq(&self, other: &Symbol) -> bool {
        *self == &*other.name
    }
}

impl PartialEq<Symbol> for String {
    fn eq(&self, other: &Symbol) -> bool {
        self.as_str() == &*other.name
    }
}

impl From<&str> for Symbol {
    fn from(s: &str) -> Self {
        symbol(s)
    }
}

impl From<String> for Symbol {
    fn from(s: String) -> Self {
        symbol(s)
    }
}

impl From<&String> for Symbol {
    fn from(s: &String) -> Self {
        symbol(s)
    }
}

impl From<Arc<str>> for Symbol {
    fn from(name: Arc<str>) -> Self {
        Self {
            hash: Self::hash_of(&name),
            name,
        }
    }
}

impl From<Symbol> for Arc<str> {
    fn from(s: Symbol) -> Self {
        s.name
    }
}

impl std::str::FromStr for Symbol {
    type Err = std::convert::Infallible;
    fn from_str(s: &str) -> Result<Self, Self::Err> {
        Ok(symbol(s))
    }
}

/// Serializes as a bare string, exactly as the `Arc<str>` this replaced did —
/// so every run-state blob, spec document and report already written still
/// round-trips.
impl serde::Serialize for Symbol {
    fn serialize<S: serde::Serializer>(&self, s: S) -> Result<S::Ok, S::Error> {
        s.serialize_str(&self.name)
    }
}

impl<'de> serde::Deserialize<'de> for Symbol {
    fn deserialize<D: serde::Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        Ok(symbol(String::deserialize(d)?))
    }
}

/// Intern `name` into a [`Symbol`].
///
/// A free function rather than a `From` impl so call sites read as a deliberate
/// conversion — the point of [`Symbol`] is that you allocate once and clone
/// thereafter, and a conversion buried in an `.into()` inside a per-bar loop
/// defeats it. It is also where the cached hash is paid for, once per
/// interning rather than once per comparison.
pub fn symbol(name: impl AsRef<str>) -> Symbol {
    let name = name.as_ref();
    Symbol {
        hash: Symbol::hash_of(name),
        name: Arc::from(name),
    }
}

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
/// It is also deliberately **interleaved** — one `Vec` of
/// `(tag, tag, atom)` — rather than a tag vector beside an atom vector, and
/// that is the counter-intuitive half. [`find`](Self::find) reads only the
/// 24-byte tag out of a 112-byte entry, so a split scan would touch 4.7× less
/// cache; the ratio is real and the change is still a **14% instruction
/// regression**, because at every universe size this crate targets the whole
/// array is L1-resident anyway (7 KB at 64 symbols) and the split then pays
/// index bookkeeping per element for a miss that never happened. Implemented,
/// measured and reverted — `cargo bench --bench snapshot_scan`, and
/// `docs/PERFORMANCE.md` Phase 13. **Don't re-derive the 4.7× and try again.**
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
/// // Snapshot<Symbol>.
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
        let mut snap: Snapshot<Symbol> = Snapshot::new();
        snap.push(Some("BTC".into()), None, Atom::new(Candle::new(1.0, 2.0, 0.5, 1.5, 10.0)));
        snap.push(Some("BTC.funding".into()), None, funding_atom(0.0003));

        let sole = snap.sole_atom().expect("the one priceable entry");
        assert_eq!(sole.candle.unwrap().close, 1.5);
    }

    #[test]
    fn sole_atom_still_panics_on_two_real_bars() {
        // The ambiguity it exists to catch is unchanged: two *priceable*
        // entries remain a programming error, not a silent arbitrary pick.
        let mut snap: Snapshot<Symbol> = Snapshot::new();
        snap.push(Some("BTC".into()), None, Atom::new(Candle::new(1.0, 2.0, 0.5, 1.5, 10.0)));
        snap.push(Some("ETH".into()), None, Atom::new(Candle::new(2.0, 3.0, 1.5, 2.5, 20.0)));
        assert!(std::panic::catch_unwind(|| snap.sole_atom()).is_err());
    }

    #[test]
    fn an_all_overlay_snapshot_has_no_sole_atom() {
        let mut snap: Snapshot<Symbol> = Snapshot::new();
        snap.push(Some("BTC.funding".into()), None, funding_atom(0.0003));
        assert!(snap.sole_atom().is_none());
    }
}

#[cfg(test)]
mod symbol_tests {
    use super::*;
    use std::collections::HashMap;

    /// The case pointer equality would have got wrong, and the reason [`Symbol`]
    /// hashes instead: two symbols reach the same run through different
    /// interning sites — one when the spec is built, one when data is loaded —
    /// and must compare equal despite being separate allocations.
    #[test]
    fn symbols_from_different_interning_sites_are_equal() {
        let from_spec = symbol("BTCUSDT");
        let from_data = symbol("BTCUSDT");
        assert!(
            !std::ptr::eq(from_spec.as_arc().as_ptr(), from_data.as_arc().as_ptr()),
            "test is vacuous — these happen to share an allocation",
        );
        assert_eq!(from_spec, from_data);

        // Every construction path has to agree, since they are mixed freely.
        let built: [Symbol; 6] = [
            symbol("BTCUSDT"),
            Symbol::from("BTCUSDT"),
            Symbol::from(String::from("BTCUSDT")),
            Symbol::from(Arc::<str>::from("BTCUSDT")),
            serde_json::from_str(r#""BTCUSDT""#).unwrap(),
            "BTCUSDT".parse().unwrap(),
        ];
        for (i, a) in built.iter().enumerate() {
            assert_eq!(*a, from_spec, "construction path {i} disagrees");
        }
    }

    /// Hash-first equality must be *equivalent* to comparing the text, not
    /// merely correlated with it — in both directions.
    #[test]
    fn hash_first_equality_agrees_with_the_text() {
        let names = [
            "BTCUSDT", "ETHUSDT", "SOLUSDT", "AAPL", "MSFT", "", "A", "a",
            "BTC-USDT-SWAP", "BTC-USDT-PERP",
        ];
        for a in names {
            for b in names {
                assert_eq!(
                    symbol(a) == symbol(b),
                    a == b,
                    "`{a}` vs `{b}`: hash-first equality disagrees with the text",
                );
            }
        }
    }

    /// `Borrow<str>` is load-bearing — the crate documents that a
    /// `HashMap<Symbol, _>` stays queryable with a plain `&str`. That requires
    /// `Hash` to delegate to the bytes, not to the cached hash, so this is the
    /// guard against "optimising" `Hash` to use the field.
    #[test]
    fn a_symbol_keyed_map_is_still_queryable_with_a_str() {
        let mut m: HashMap<Symbol, i32> = HashMap::new();
        m.insert(symbol("BTCUSDT"), 1);
        m.insert(symbol("ETHUSDT"), 2);
        assert_eq!(m.get("BTCUSDT"), Some(&1));
        assert_eq!(m.get("ETHUSDT"), Some(&2));
        assert_eq!(m.get("SOLUSDT"), None);
        // And a symbol interned separately still finds its entry.
        assert_eq!(m.get(&symbol("BTCUSDT")), Some(&1));
    }

    /// Ordering is on the text, not on the hash: `marked_equity` sorts symbols
    /// to give its sum a canonical order, and a hash order would be stable but
    /// meaningless.
    #[test]
    fn ordering_is_lexicographic_on_the_text() {
        let mut v = [symbol("SOL"), symbol("BTC"), symbol("ETH")];
        v.sort();
        assert_eq!(
            v.iter().map(Symbol::as_str).collect::<Vec<_>>(),
            ["BTC", "ETH", "SOL"],
        );
    }

    /// The wire format is a bare string, exactly as the `Arc<str>` this replaced
    /// produced — so run-state blobs and spec documents already written still
    /// load. A round trip of the *new* code would pass against a moved format;
    /// the literal is the point.
    #[test]
    fn a_symbol_serializes_as_a_bare_string() {
        assert_eq!(serde_json::to_string(&symbol("BTCUSDT")).unwrap(), r#""BTCUSDT""#);
        let back: Symbol = serde_json::from_str(r#""BTCUSDT""#).unwrap();
        assert_eq!(back, symbol("BTCUSDT"));
        // Also as a map key, which is how the run-state blobs carry it.
        let m: HashMap<Symbol, i32> = HashMap::from([(symbol("X"), 7)]);
        assert_eq!(serde_json::to_string(&m).unwrap(), r#"{"X":7}"#);
    }

    /// String comparisons and `Deref` keep working unchanged, which is what
    /// lets the ~150 existing call sites stay as they were.
    #[test]
    fn a_symbol_still_behaves_like_a_str() {
        let s = symbol("BTCUSDT");
        assert_eq!(s, "BTCUSDT");
        assert_eq!("BTCUSDT", s);
        assert_eq!(s.len(), 7);
        assert!(s.starts_with("BTC"));
        assert_eq!(&*s, "BTCUSDT");
        assert_eq!(format!("{s}"), "BTCUSDT");
        assert_eq!(format!("{s:?}"), "\"BTCUSDT\"");
    }
}
