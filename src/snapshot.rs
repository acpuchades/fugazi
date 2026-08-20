//! Multi-asset framing: a keyed [`Selector`] naming which asset in a
//! [`Snapshot`] to read, and the [`Snapshot`] itself — a per-bar collection of
//! tagged [`Atom`]s that lets a strategy or an indicator reason about more
//! than one instrument at a time.

use std::hash::Hash;
use std::sync::{Arc, OnceLock};

use crate::hash::SymMap;

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
/// [`Snapshot::sole_atom_or_panic`] path (see [`Selector::is_empty`]) rather than a
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
    /// treats as a single-entry unpack ([`Snapshot::sole_atom_or_panic`]) rather than
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
    entries: Arc<Entries<Sym>>,
}

/// A snapshot's rows, plus a lazily-built `symbol → first row` index.
///
/// The rows stay **interleaved** — the tag beside the atom it describes, one
/// allocation. Splitting them was tried and is 14% slower (see [`Snapshot`]).
/// This is a *side table*, which is a different thing: it does not change how
/// the rows are scanned, it removes the need to scan them at all.
#[derive(Debug)]
struct Entries<Sym> {
    rows: Vec<Entry<Sym>>,
    /// `hash(symbol) → the earliest row whose symbol hashes to it`. Built on
    /// the first symbol-named [`Snapshot::find`] of a snapshot wide enough to be
    /// worth it, then shared by every other lookup that bar (a snapshot is built
    /// once per bar and cloned to every leaf, and a clone is a refcount bump, so
    /// the whole universe's lookups amortise one build).
    ///
    /// **Keyed by hash, not by symbol, and that is deliberate.** Keying by `Sym`
    /// would clone one into the map per row per bar — for `Symbol` that is an
    /// atomic refcount bump each, and it measured as most of the build cost. A
    /// `u64` key clones for free and compares in one instruction.
    ///
    /// It costs nothing in soundness because the value is a **lower bound, not
    /// an answer**. Two symbols that collide share the *earliest* of their rows,
    /// so the scan starts at or before the true first match and still lands on
    /// it; it can never start too late and miss one. A hash that is absent
    /// proves no symbol with that hash is present, so the lookup can answer
    /// `None` without touching a row.
    ///
    /// `OnceLock` rather than `RefCell`: a snapshot is handed to worker threads
    /// by the `optimize` sweep, so the cache has to be `Sync`.
    index: OnceLock<SymMap<u64, usize>>,
}

/// Hash `sym` to the `u64` the index is keyed by.
fn sym_hash<Sym: Hash>(sym: &Sym) -> u64 {
    use std::hash::Hasher;
    let mut h = crate::hash::FxHasher::default();
    sym.hash(&mut h);
    h.finish()
}

/// Rows below which [`Snapshot::find`] scans rather than indexing.
///
/// Building the index costs one hash and one insert per row; using it costs one
/// hash and one probe per lookup. Narrow snapshots do not recover the build,
/// because the scan they would replace is short. **Swept, not guessed** — the
/// same `MultiAssetStrategy` drive at four widths (`snapshot_scan --
/// drive_equal16` / `32` / `drive_equal`, each measured against a build with
/// this constant raised out of reach):
///
/// | universe | indexed vs scan |
/// |---|---:|
/// | 16 | **+6.5%** — the build does not pay for itself |
/// | 32 | −2.5% |
/// | 64 | −16.7% |
/// | 64, one leaf per symbol (basket) | −2.1% |
/// | 1 (single asset) | −0.0%, never indexes |
///
/// The crossover sits between 16 and 32, so 32 is the first width where every
/// measured shape is a win or neutral. A first draft used 8 and made a 16-symbol
/// run 6.5% *slower*.
const INDEX_THRESHOLD: usize = 32;

/// Cloning drops the index rather than copying it. The only thing that clones
/// `Entries` is [`Arc::make_mut`], which is always followed by a mutation that
/// would invalidate it anyway — so copying it would be work spent on a value
/// about to be thrown away.
impl<Sym: Clone> Clone for Entries<Sym> {
    fn clone(&self) -> Self {
        Self {
            rows: self.rows.clone(),
            index: OnceLock::new(),
        }
    }
}

impl<Sym> Entries<Sym> {
    fn new(rows: Vec<Entry<Sym>>) -> Self {
        Self {
            rows,
            index: OnceLock::new(),
        }
    }

    /// Drop the cached index. **Every mutation of `rows` must call this**, or a
    /// lookup could resolve to a row that has moved or gone.
    fn invalidate(&mut self) {
        self.index.take();
    }
}

impl<Sym: Hash> Entries<Sym> {
    /// The index, built on first use. Rows are walked in order and
    /// `or_insert` keeps the first writer, so each hash maps to the
    /// **earliest** row carrying it — which is what makes the indexed path
    /// agree with the first-match-wins scan it replaces.
    fn index(&self) -> &SymMap<u64, usize> {
        self.index.get_or_init(|| {
            let mut m = SymMap::with_capacity_and_hasher(self.rows.len(), Default::default());
            for (i, (sym, _, _)) in self.rows.iter().enumerate() {
                if let Some(sym) = sym {
                    m.entry(sym_hash(sym)).or_insert(i);
                }
            }
            m
        })
    }
}

/// One tagged atom inside a [`Snapshot`]: `(symbol, frequency, atom)`, with
/// both tags optional. Named so the shared storage type stays readable.
pub type Entry<Sym> = (Option<Sym>, Option<Frequency>, Atom);

/// The panic text for an ambiguous [`Snapshot::sole_atom_or_panic`] unpack.
///
/// Deliberately does **not** advise adding `!pick { symbol }` to every leaf.
/// That advice was measured to be wrong. The message was overwhelmingly
/// reached from `strategies::single_asset::extract_self_atom`, which fires
/// when a strategy's own *declared* symbol is absent from the bar — and a
/// document whose leaves already named their asset explicitly failed
/// identically, because the router, not the leaf, was the caller. That path
/// now falls back through [`Snapshot::sole_atom_or_none`] and reads `None`, so what
/// remains here is only the genuine case: a leaf in an **unrooted** context
/// that named no asset at all.
fn ambiguous_sole_atom(n: usize) -> String {
    format!(
        "Snapshot::sole_atom_or_panic: no leaf named an asset, and this bar carries {n} priceable \
         series — there is no single one to unpack.\n\
         \n\
         This is an *unrooted* context: one that blesses no series of its own, so every \
         leaf under it has to name its asset. Those are a pairs document (two legs, \
         neither privileged), a portfolio's `weights:` / `rebalance_on:`, and a \
         `!sharpe`-style embedded `strategy:`. A single-asset, basket or multi-asset \
         document blesses its own symbol and does not reach this.\n\
         \n\
         To fix, name the asset on the leaf:\n\
         \n\
         - YAML — `!close {{ source: !pick {{ symbol: BTCUSDT }} }}`\n\
         - Rust — `Pick::matching(Selector::by_symbol(..))` in place of `Pick::new()`\n\
         \n\
         Note this is *not* how an absent series is reported. A strategy whose declared \
         symbol does not quote on a given bar reads `None` and does not advance; a \
         declared symbol absent from the entire stream is refused up front, by name."
    )
}

impl<Sym> Snapshot<Sym> {
    /// An empty snapshot with no assets.
    pub fn new() -> Self {
        Self {
            entries: Arc::new(Entries::new(Vec::new())),
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
            entries: Arc::new(Entries::new(vec![(None, None, atom)])),
        }
    }

    /// A single-entry snapshot tagged with `symbol` and no `freq`. The
    /// single-series shortcut for building a driver-ready snapshot:
    /// [`fugazi::backtest::run`](crate::backtest::run) prices the wallet on
    /// this entry's `symbol` each bar.
    pub fn single(symbol: Sym, atom: Atom) -> Self {
        Self {
            entries: Arc::new(Entries::new(vec![(Some(symbol), None, atom)])),
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
        let e = Arc::make_mut(&mut self.entries);
        e.rows.push((symbol, freq, atom));
        e.invalidate();
    }

    /// Number of tagged atoms in this snapshot.
    pub fn len(&self) -> usize {
        self.entries.rows.len()
    }

    /// True if this snapshot carries no atoms.
    pub fn is_empty(&self) -> bool {
        self.entries.rows.is_empty()
    }

    /// Iterate over `(symbol, freq, atom)` triples in insertion order.
    pub fn iter(&self) -> impl Iterator<Item = (Option<&Sym>, Option<Frequency>, &Atom)> {
        self.entries
            .rows
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
    /// Contrast with [`sole_atom_or_panic`](Self::sole_atom_or_panic), the
    /// single-series safety net: it panics on 2+ entries because most price
    /// leaves (`!close`, `!high`, …) genuinely depend on *which* asset.
    pub fn any_atom(&self) -> Option<&Atom> {
        self.entries.rows.first().map(|(_, _, a)| a)
    }

    /// The sole priceable atom — **panicking** when the snapshot is
    /// ambiguous.
    ///
    /// One of three spellings of a single decision, differing *only* in how a
    /// 2+ entry snapshot is answered: this one panics,
    /// [`sole_atom_or_none`](Self::sole_atom_or_none) reads `None`, and
    /// [`sole_atom_or_err`](Self::sole_atom_or_err) returns `Err(count)`. All
    /// three agree on the other two cases — `None` when nothing priceable
    /// quoted, `Some` when exactly one did.
    ///
    /// This is the primitive [`Pick::new`](crate::indicators::Pick::new) uses
    /// for its "no query — this leaf named no asset" path, which is reached
    /// only from an *unrooted* context: one that blessed no series of its own,
    /// so every leaf under it has to name what it reads. A 2+ read there means
    /// the leaf cannot choose, and the loud failure beats silently returning an
    /// arbitrary asset.
    ///
    /// For sources that are symbol-agnostic (calendar accessors that only read
    /// `atom.time`), see [`any_atom`](Self::any_atom).
    pub fn sole_atom_or_panic(&self) -> Option<&Atom> {
        match self.sole_atom_or_err() {
            Ok(atom) => atom,
            Err(n) => panic!("{}", ambiguous_sole_atom(n)),
        }
    }

    /// The sole priceable atom, or `Err(count)` on ambiguity — the
    /// **fallible** spelling, and the one implementation all three share.
    ///
    /// Exists so a caller that has an error channel can use one — the FFI
    /// boundary in particular, where an unwinding panic becomes a
    /// `PanicException` that Python's `except Exception` does not catch.
    /// `sole_atom_or_panic` is the same call with the count rendered into a
    /// panic, for the `Indicator::update` path that has nowhere to return a
    /// `Result`.
    ///
    /// `Ok(None)` is an empty (or overlay-only) snapshot, which is not an
    /// error: it is the ordinary "nothing quoted this bar" reading.
    pub fn sole_atom_or_err(&self) -> Result<Option<&Atom>, usize> {
        // Only priceable entries count. An overlay-only series — a funding
        // rate, an open interest — is stacked into the snapshot beside the
        // price series and is reached deliberately, with `!pick`; it must stay
        // invisible to the implicit unpack, or attaching one would break every
        // bare `!close` in a single-asset strategy that never asked for it.
        let mut priceable = self.entries.rows.iter().filter(|(_, _, a)| a.is_priceable());
        let first = priceable.next();
        match (first, priceable.count()) {
            (None, _) => Ok(None),
            (Some(entry), 0) => Ok(Some(&entry.2)),
            (Some(_), extra) => Err(extra + 1),
        }
    }

    /// The sole priceable atom, or `None` on ambiguity — the
    /// **non-panicking** spelling.
    ///
    /// [`Pick::rooted`](crate::indicators::Pick::rooted) and
    /// `strategies::single_asset::extract_self_atom` use this as their fallback
    /// when the blessed symbol matches nothing on a bar. The distinction from
    /// [`sole_atom_or_panic`](Self::sole_atom_or_panic) is the whole point: in
    /// a *rooted* context a 2+ entry snapshot is ordinary — it just means the
    /// blessed leg is absent this bar (a listing gap, a different exchange
    /// calendar), which must read `None` and let the caller roll the symbol
    /// off, not abort the run. The panic is for the genuinely mis-wired case,
    /// where nothing named an asset at all.
    pub fn sole_atom_or_none(&self) -> Option<&Atom> {
        let mut priceable = self.entries.rows.iter().filter(|(_, _, a)| a.is_priceable());
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
    pub fn find(&self, query: &Selector<Sym>) -> Option<&Atom>
    where
        Sym: Clone + Eq + Hash,
    {
        let rows = &self.entries.rows;
        // A query naming a symbol can be answered from the index: no row before
        // that symbol's first can match (a row's tag must *equal* the named
        // symbol, and an untagged row never does), so the first match is at or
        // after it.
        //
        // The scan then resumes from there rather than stopping there, which is
        // what keeps this general. With `freq: None` — the common case, and what
        // every blessed root and basket leg builds — the very first row hits and
        // it is O(1). With a `freq` named too, a symbol may appear more than once
        // on different cadences, so the scan continues from the first candidate
        // and lands on the same row the full scan would have.
        //
        // A symbol absent from the index is absent from the snapshot, so that
        // case answers `None` without touching a row at all.
        let from = match query.symbol.as_ref() {
            Some(sym) if rows.len() >= INDEX_THRESHOLD => {
                match self.entries.index().get(&sym_hash(sym)) {
                    Some(&i) => i,
                    None => return None,
                }
            }
            _ => 0,
        };
        rows[from..].iter().find_map(|(s, f, a)| {
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
        let e = Arc::make_mut(&mut self.entries);
        e.rows.retain(|(s, f, _)| !query.matches(s.as_ref(), *f));
        e.invalidate();
    }
}

impl<Sym: PartialEq> PartialEq for Snapshot<Sym> {
    fn eq(&self, other: &Self) -> bool {
        self.entries.rows == other.entries.rows
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
            entries: Arc::new(Entries::new(iter.into_iter().collect())),
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
    fn sole_atom_or_panic_ignores_an_overlay_only_entry() {
        // The point of the whole change: stacking a funding series next to a
        // price series must not break a single-asset strategy's bare `!close`,
        // which reaches its bar through the implicit no-arg unpack.
        let mut snap: Snapshot<Symbol> = Snapshot::new();
        snap.push(Some("BTC".into()), None, Atom::new(Candle::new(1.0, 2.0, 0.5, 1.5, 10.0)));
        snap.push(Some("BTC.funding".into()), None, funding_atom(0.0003));

        let sole = snap.sole_atom_or_panic().expect("the one priceable entry");
        assert_eq!(sole.candle.unwrap().close, 1.5);
    }

    #[test]
    fn sole_atom_or_panic_still_panics_on_two_real_bars() {
        // The ambiguity it exists to catch is unchanged: two *priceable*
        // entries remain a programming error, not a silent arbitrary pick.
        let mut snap: Snapshot<Symbol> = Snapshot::new();
        snap.push(Some("BTC".into()), None, Atom::new(Candle::new(1.0, 2.0, 0.5, 1.5, 10.0)));
        snap.push(Some("ETH".into()), None, Atom::new(Candle::new(2.0, 3.0, 1.5, 2.5, 20.0)));
        assert!(std::panic::catch_unwind(|| snap.sole_atom_or_panic()).is_err());
    }

    #[test]
    fn sole_atom_or_err_reports_the_count_instead_of_panicking() {
        // The fallible twin the FFI boundary goes through: same decision, an
        // error value instead of an unwind. `Ok(None)` stays reserved for
        // "nothing quoted", which is not an error.
        let bar = |c: f64| Atom::new(Candle::new(c, c, c, c, 1.0));
        let mut three: Snapshot<Symbol> = Snapshot::new();
        three.push(Some("BTC".into()), None, bar(1.5));
        three.push(Some("ETH".into()), None, bar(2.5));
        three.push(Some("SOL".into()), None, bar(3.5));
        assert_eq!(three.sole_atom_or_err().err(), Some(3));

        let mut one: Snapshot<Symbol> = Snapshot::new();
        one.push(Some("BTC".into()), None, bar(1.5));
        assert_eq!(
            one.sole_atom_or_err().ok().flatten().and_then(|a| a.candle).map(|c| c.close),
            Some(1.5)
        );

        // An overlay-only entry is not priceable, so this is `Ok(None)` — the
        // "nothing quoted" reading, never an ambiguity.
        let mut overlay: Snapshot<Symbol> = Snapshot::new();
        overlay.push(Some("BTC.funding".into()), None, funding_atom(0.0003));
        assert!(matches!(overlay.sole_atom_or_err(), Ok(None)));
    }

    #[test]
    fn an_all_overlay_snapshot_has_no_sole_atom_or_panic() {
        let mut snap: Snapshot<Symbol> = Snapshot::new();
        snap.push(Some("BTC.funding".into()), None, funding_atom(0.0003));
        assert!(snap.sole_atom_or_panic().is_none());
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

#[cfg(test)]
mod index_tests {
    use super::*;
    use crate::market::Candle;
    use crate::time::Frequency;
    use crate::types::Real;

    fn atom(px: Real) -> Atom {
        Atom::new(Candle::new(px, px, px, px, 1.0))
    }

    /// The scan `find` used to be, kept verbatim as the oracle. Any divergence
    /// between this and the indexed path is a bug in the index.
    fn find_by_scan<'a>(snap: &'a Snapshot<Symbol>, q: &Selector<Symbol>) -> Option<&'a Atom> {
        snap.entries
            .rows
            .iter()
            .find_map(|(s, f, a)| q.matches(s.as_ref(), *f).then_some(a))
    }

    /// Wide enough to index, with duplicate symbols on different cadences and
    /// an untagged row mixed in — every shape the index has to answer the same
    /// way the scan does.
    fn awkward() -> (Snapshot<Symbol>, Vec<Selector<Symbol>>) {
        let mut snap: Snapshot<Symbol> = Snapshot::new();
        // Untagged first, so an index built from row 0 cannot accidentally
        // become the answer to a symbol-named query.
        snap.push(None, None, atom(1.0));
        for i in 0..40 {
            let s = symbol(format!("S{i:02}"));
            snap.push(Some(s.clone()), Some(Frequency::Hour(1)), atom(100.0 + i as Real));
            // Every third symbol appears twice — once hourly, once daily — so
            // first-match-wins and freq-qualified lookups are both exercised.
            if i % 3 == 0 {
                snap.push(Some(s), Some(Frequency::Day(1)), atom(900.0 + i as Real));
            }
        }
        assert!(snap.len() >= INDEX_THRESHOLD, "test must exercise the index");

        let mut queries = vec![Selector::default(), Selector::by_freq(Frequency::Day(1))];
        for i in 0..42 {
            let s = symbol(format!("S{i:02}")); // S40/S41 are absent
            queries.push(Selector::by_symbol(s.clone()));
            queries.push(Selector::exact(s.clone(), Frequency::Hour(1)));
            queries.push(Selector::exact(s, Frequency::Day(1)));
        }
        (snap, queries)
    }

    #[test]
    fn the_index_answers_exactly_what_the_scan_would() {
        let (snap, queries) = awkward();
        for q in &queries {
            let want = find_by_scan(&snap, q).map(|a| a.candle.unwrap().close);
            let got = snap.find(q).map(|a| a.candle.unwrap().close);
            assert_eq!(got, want, "indexed find disagreed with the scan for {q:?}");
        }
    }

    /// The property the index could most easily break: a duplicate tag must
    /// still resolve to the **earliest** row, because that is what the scan did
    /// and what `Snapshot`'s docs promise.
    #[test]
    fn a_duplicate_tag_still_resolves_first_match_wins() {
        let (snap, _) = awkward();
        // S00 appears hourly (100.0) then daily (900.0). A symbol-only query
        // must find the hourly one — the first pushed.
        let hit = snap.find(&Selector::by_symbol(symbol("S00"))).unwrap();
        assert_eq!(hit.candle.unwrap().close, 100.0);
        // Naming the later cadence explicitly must still reach the second row,
        // which is the case that forces the scan to *resume* from the index
        // rather than stop at it.
        let hit = snap
            .find(&Selector::exact(symbol("S00"), Frequency::Day(1)))
            .unwrap();
        assert_eq!(hit.candle.unwrap().close, 900.0);
    }

    /// A cached index that outlives a mutation would resolve to a row that has
    /// moved or gone. Both mutators must drop it.
    #[test]
    fn mutating_a_snapshot_invalidates_the_index() {
        let (mut snap, _) = awkward();
        // Force the index to exist.
        assert!(snap.find(&Selector::by_symbol(symbol("S01"))).is_some());

        // Push a symbol that was absent when the index was built.
        snap.push(Some(symbol("S99")), None, atom(42.0));
        let hit = snap.find(&Selector::by_symbol(symbol("S99")));
        assert_eq!(
            hit.map(|a| a.candle.unwrap().close),
            Some(42.0),
            "a row pushed after the index was built was not found",
        );

        // Removing shifts every later row's position.
        snap.find(&Selector::by_symbol(symbol("S02"))).unwrap();
        snap.remove_matching(&Selector::by_symbol(symbol("S00")));
        assert!(snap.find(&Selector::by_symbol(symbol("S00"))).is_none());
        let (s, q) = (&snap, Selector::by_symbol(symbol("S02")));
        assert_eq!(
            s.find(&q).map(|a| a.candle.unwrap().close),
            find_by_scan(s, &q).map(|a| a.candle.unwrap().close),
            "the index survived a removal and now points at the wrong row",
        );
    }

    /// A symbol type whose every value hashes the same, so **every** lookup
    /// takes the collision path. The index then maps one bucket to row 0 and
    /// answers are decided entirely by the scan that resumes from it — which is
    /// exactly the property the index's soundness rests on, and the one no
    /// realistic symbol set would exercise.
    #[derive(Clone, PartialEq, Eq, Debug)]
    struct Collide(&'static str);

    impl std::hash::Hash for Collide {
        fn hash<H: std::hash::Hasher>(&self, state: &mut H) {
            state.write_u8(0);
        }
    }

    #[test]
    fn total_hash_collision_still_answers_correctly() {
        let mut snap: Snapshot<Collide> = Snapshot::new();
        // Must clear INDEX_THRESHOLD, or this silently stops testing the
        // collision path it exists for — hence the assertion below.
        let names: Vec<String> = (0..40).map(|i| format!("N{i:02}")).collect();
        let names: Vec<&'static str> = names
            .into_iter()
            .map(|s| &*Box::leak(s.into_boxed_str()))
            .collect();
        for (i, n) in names.iter().enumerate() {
            snap.push(Some(Collide(n)), None, atom(100.0 + i as Real));
        }
        assert!(snap.len() >= INDEX_THRESHOLD);
        for (i, n) in names.iter().enumerate() {
            let q = Selector::by_symbol(Collide(n));
            assert_eq!(
                snap.find(&q).map(|a| a.candle.unwrap().close),
                Some(100.0 + i as Real),
                "collision path returned the wrong row for {n}",
            );
        }
        // An absent symbol collides with every present one, so the index cannot
        // short-circuit it — the scan has to reject it.
        assert!(snap.find(&Selector::by_symbol(Collide("ZZZ"))).is_none());
    }

    /// A snapshot below the threshold must never build an index, and must still
    /// answer identically — the single-asset path takes this branch.
    #[test]
    fn a_narrow_snapshot_skips_the_index_entirely() {
        let mut snap: Snapshot<Symbol> = Snapshot::new();
        snap.push(Some(symbol("BTC")), None, atom(1.0));
        snap.push(Some(symbol("ETH")), None, atom(2.0));
        assert!(snap.len() < INDEX_THRESHOLD);
        assert_eq!(
            snap.find(&Selector::by_symbol(symbol("ETH")))
                .map(|a| a.candle.unwrap().close),
            Some(2.0)
        );
        assert!(snap.find(&Selector::by_symbol(symbol("SOL"))).is_none());
        assert!(
            snap.entries.index.get().is_none(),
            "a narrow snapshot built an index it cannot pay for",
        );
    }
}
