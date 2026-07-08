//! Core scalar and market-data types shared across the crate.

use std::collections::HashMap;
use std::sync::Arc;

/// The scalar type used throughout the crate for prices and indicator outputs.
///
/// Centralised as an alias so the whole library can be switched to another
/// floating-point width (or a fixed-point type) in one place.
pub type Real = f64;

/// A UTC millisecond timestamp (Unix epoch).
///
/// Kept as a flat `i64` on purpose: it matches Binance's native representation,
/// stays `Copy`, and keeps `time::OffsetDateTime` out of the pure core's ABI —
/// callers that want a datetime go through [`Timestamp::to_datetime`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct Timestamp(pub i64);

impl Timestamp {
    /// The current UTC time, in milliseconds since the Unix epoch.
    pub fn now() -> Self {
        Self::from_datetime(::time::OffsetDateTime::now_utc())
    }

    /// Convert a `time::OffsetDateTime` to a millisecond epoch stamp.
    pub fn from_datetime(dt: ::time::OffsetDateTime) -> Self {
        let nanos = dt.unix_timestamp_nanos();
        Self((nanos / 1_000_000) as i64)
    }

    /// Reconstruct a `time::OffsetDateTime` at UTC from this millisecond stamp.
    pub fn to_datetime(self) -> ::time::OffsetDateTime {
        let nanos = (self.0 as i128) * 1_000_000;
        ::time::OffsetDateTime::from_unix_timestamp_nanos(nanos)
            .expect("i64 millis fits in OffsetDateTime range")
    }
}

/// A single OHLCV bar.
///
/// The numeric spine of every input [`Atom`]. Indicators that only need a price
/// stream take [`Real`] directly; those that need the full bar (true range,
/// typical price, volume-weighted values, …) take an [`Atom`] and read its
/// [`candle`](Atom::candle).
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Candle {
    pub open: Real,
    pub high: Real,
    pub low: Real,
    pub close: Real,
    pub volume: Real,
}

impl Candle {
    pub fn new(open: Real, high: Real, low: Real, close: Real, volume: Real) -> Self {
        Self {
            open,
            high,
            low,
            close,
            volume,
        }
    }

    /// Typical price: `(high + low + close) / 3`.
    pub fn typical(&self) -> Real {
        (self.high + self.low + self.close) / 3.0
    }

    /// Median price: `(high + low) / 2`.
    pub fn median(&self) -> Real {
        (self.high + self.low) / 2.0
    }
}

/// The declared type of one overlay column — recorded in the [`Schema`] so a
/// `!get` (or the library-side `GetReal`/`GetBool`/`GetStr`) dispatches to the
/// right typed leaf at build time. `Real` is a `f64`; `Bool` is a native bool
/// (a signal leaf); `Str` is a shared `Arc<str>` (used for categorical
/// side-channels like a regime label).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum OverlayType {
    Real,
    Bool,
    Str,
}

impl std::fmt::Display for OverlayType {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            OverlayType::Real => f.write_str("Real"),
            OverlayType::Bool => f.write_str("Bool"),
            OverlayType::Str => f.write_str("Str"),
        }
    }
}

/// One overlay column's per-bar value: a `Real`, a `Bool`, or a `Str`. The
/// runtime carrier for the schema's declared per-column [`OverlayType`].
///
/// `Str` shares its backing storage via `Arc<str>` so a column of repeated
/// labels (a regime tag, a session marker) doesn't allocate per bar for the
/// same value.
#[derive(Debug, Clone, PartialEq)]
pub enum OverlayValue {
    Real(Real),
    Bool(bool),
    Str(Arc<str>),
}

impl OverlayValue {
    /// The [`OverlayType`] this value carries.
    pub fn type_of(&self) -> OverlayType {
        match self {
            OverlayValue::Real(_) => OverlayType::Real,
            OverlayValue::Bool(_) => OverlayType::Bool,
            OverlayValue::Str(_) => OverlayType::Str,
        }
    }
}

impl From<Real> for OverlayValue {
    fn from(v: Real) -> Self {
        OverlayValue::Real(v)
    }
}
impl From<bool> for OverlayValue {
    fn from(v: bool) -> Self {
        OverlayValue::Bool(v)
    }
}
impl From<Arc<str>> for OverlayValue {
    fn from(v: Arc<str>) -> Self {
        OverlayValue::Str(v)
    }
}
impl From<&str> for OverlayValue {
    fn from(v: &str) -> Self {
        OverlayValue::Str(Arc::from(v))
    }
}
impl From<String> for OverlayValue {
    fn from(v: String) -> Self {
        OverlayValue::Str(Arc::from(v.as_str()))
    }
}

/// Read-only column name → (index, type) registry for the values array of an
/// [`OverlayInfo`].
///
/// A schema is built with [`Schema::builder`] and frozen by
/// [`SchemaBuilder::finish`] into an `Arc<Schema>` shared across every bar of a
/// run. The `Arc` guarantees the schema can't change after any `OverlayInfo`
/// has bound values against it, so an index resolved once at indicator
/// construction stays valid for the whole run.
///
/// Column type is recorded alongside the index so a `!get`-shaped builder can
/// dispatch to the right typed leaf without a `type:` tag on the caller side.
#[derive(Debug, Default)]
pub struct Schema {
    indexes: HashMap<String, usize>,
    /// One entry per registered column, in insertion order. `columns[i]` is
    /// the `(name, type)` pair at index `i`.
    columns: Vec<(String, OverlayType)>,
}

impl Schema {
    /// Start building a new schema.
    pub fn builder() -> SchemaBuilder {
        SchemaBuilder {
            schema: Schema::default(),
        }
    }

    /// A shared empty schema — the "no overlay side-channel" sentinel used by
    /// specs that build without an overlay context (`fugazi get`'s overlay
    /// pipeline, unit tests, doctests).
    pub fn empty() -> Arc<Schema> {
        Arc::new(Schema::default())
    }

    /// Number of registered columns — the required length of an
    /// [`OverlayInfo`] values array bound to this schema.
    pub fn len(&self) -> usize {
        self.columns.len()
    }

    /// Whether no columns are registered.
    pub fn is_empty(&self) -> bool {
        self.columns.is_empty()
    }

    /// Resolve a column name to its index, `None` if not registered.
    pub fn index_of(&self, key: &str) -> Option<usize> {
        self.indexes.get(key).copied()
    }

    /// Read a column's declared type by index. `None` if out of bounds.
    pub fn type_of(&self, index: usize) -> Option<OverlayType> {
        self.columns.get(index).map(|(_, t)| *t)
    }

    /// Read a column's declared type by name. `None` if not registered.
    pub fn type_of_key(&self, key: &str) -> Option<OverlayType> {
        self.index_of(key).and_then(|i| self.type_of(i))
    }

    /// Whether `key` is a registered column.
    pub fn contains(&self, key: &str) -> bool {
        self.indexes.contains_key(key)
    }

    /// Iterate the registered column names in insertion order.
    pub fn keys(&self) -> impl Iterator<Item = &str> {
        self.columns.iter().map(|(name, _)| name.as_str())
    }
}

/// Mutable builder for a [`Schema`]. Register columns with
/// [`add_real`](Self::add_real) / [`add_bool`](Self::add_bool) /
/// [`add_str`](Self::add_str), then freeze into an `Arc<Schema>` with
/// [`finish`](Self::finish); the frozen schema is read-only so the indexes it
/// hands out are stable for the run.
#[derive(Debug, Default)]
pub struct SchemaBuilder {
    schema: Schema,
}

impl SchemaBuilder {
    /// Register a `Real` column and return its index. Idempotent — a repeated
    /// `key` returns the previously-assigned index without adding a new slot,
    /// provided the type matches; a type mismatch panics.
    pub fn add_real(&mut self, key: impl Into<String>) -> usize {
        self.add_typed(key, OverlayType::Real)
    }

    /// Register a `Bool` column and return its index. See
    /// [`add_real`](Self::add_real) for the idempotency + type-mismatch rules.
    pub fn add_bool(&mut self, key: impl Into<String>) -> usize {
        self.add_typed(key, OverlayType::Bool)
    }

    /// Register a `Str` column and return its index. See
    /// [`add_real`](Self::add_real) for the idempotency + type-mismatch rules.
    pub fn add_str(&mut self, key: impl Into<String>) -> usize {
        self.add_typed(key, OverlayType::Str)
    }

    fn add_typed(&mut self, key: impl Into<String>, ty: OverlayType) -> usize {
        let key = key.into();
        if let Some(&i) = self.schema.indexes.get(&key) {
            let existing = self.schema.columns[i].1;
            assert_eq!(
                existing, ty,
                "column {key:?} was already registered as {existing}; \
                 cannot re-register as {ty}",
            );
            return i;
        }
        let i = self.schema.columns.len();
        self.schema.indexes.insert(key.clone(), i);
        self.schema.columns.push((key, ty));
        i
    }

    /// Number of columns registered so far.
    pub fn len(&self) -> usize {
        self.schema.len()
    }

    /// Whether no columns have been registered yet.
    pub fn is_empty(&self) -> bool {
        self.schema.is_empty()
    }

    /// Freeze into a shareable, read-only schema.
    pub fn finish(self) -> Arc<Schema> {
        Arc::new(self.schema)
    }
}

/// Per-bar overlay data attached to an [`Atom`]: a shared [`Schema`] plus that
/// bar's values in schema order.
///
/// Cheap to clone (two atomic bumps: the shared `Arc<Schema>` and the per-atom
/// `Arc<[OverlayValue]>`, no allocation), which is what
/// [`Combine`](crate::indicators::Combine) needs when it feeds the same
/// [`Atom`] to both sides. Both fields are `Arc` so an atom slice can also be
/// shared across worker threads (used by the CLI's `optimize` sweep).
#[derive(Debug, Clone)]
pub struct OverlayInfo {
    schema: Arc<Schema>,
    values: Arc<[OverlayValue]>,
}

impl OverlayInfo {
    /// Bind `values` to a fixed `schema`. `values.len()` must equal
    /// `schema.len()`, and each `values[i]`'s runtime type must match
    /// `schema.type_of(i)`.
    ///
    /// # Panics
    /// Panics on length mismatch or on any per-slot type mismatch.
    pub fn new(schema: Arc<Schema>, values: impl Into<Arc<[OverlayValue]>>) -> Self {
        let values = values.into();
        assert_eq!(
            values.len(),
            schema.len(),
            "overlay values length ({}) must match schema length ({})",
            values.len(),
            schema.len(),
        );
        for (i, v) in values.iter().enumerate() {
            let declared = schema.type_of(i).expect("index in range");
            let actual = v.type_of();
            assert_eq!(
                declared, actual,
                "overlay value at index {i} has type {actual}, \
                 schema declared {declared}",
            );
        }
        Self { schema, values }
    }

    /// The shared schema this overlay is bound to.
    pub fn schema(&self) -> &Arc<Schema> {
        &self.schema
    }

    /// The values array, in schema order.
    pub fn values(&self) -> &[OverlayValue] {
        &self.values
    }

    /// Read the value at a resolved column index, `None` if out of bounds.
    pub fn get(&self, index: usize) -> Option<&OverlayValue> {
        self.values.get(index)
    }

    /// Look up a value by column name (a one-time convenience — hot-path
    /// readers resolve the index at construction and read [`get`](Self::get)).
    pub fn get_by_key(&self, key: &str) -> Option<&OverlayValue> {
        self.schema.index_of(key).and_then(|i| self.get(i))
    }

    /// Read the `Real` at a resolved index. `None` if the index is out of
    /// bounds *or* the slot's runtime type isn't `Real`. Defensive — a
    /// well-constructed [`OverlayInfo`] never sees a type mismatch here, but
    /// the check keeps the typed leaves ([`GetReal`](crate::indicators::GetReal))
    /// honest.
    pub fn get_real(&self, index: usize) -> Option<Real> {
        match self.get(index)? {
            OverlayValue::Real(x) => Some(*x),
            _ => None,
        }
    }

    /// Read the `Bool` at a resolved index. `None` on out-of-bounds or type
    /// mismatch.
    pub fn get_bool(&self, index: usize) -> Option<bool> {
        match self.get(index)? {
            OverlayValue::Bool(b) => Some(*b),
            _ => None,
        }
    }

    /// Read the `Str` at a resolved index. `None` on out-of-bounds or type
    /// mismatch.
    pub fn get_str(&self, index: usize) -> Option<&Arc<str>> {
        match self.get(index)? {
            OverlayValue::Str(s) => Some(s),
            _ => None,
        }
    }
}

/// A single bar's input to the indicator chain: an OHLCV [`Candle`], an
/// optional bar-open [`Timestamp`], and, optionally, per-bar overlay values
/// keyed by a shared [`Schema`].
///
/// The `Input` associated type of every [`Indicator`](crate::Indicator) that
/// reads bar-shaped data. OHLCV access stays a direct field read
/// (`atom.candle.close`); overlay access goes through an index resolved once at
/// construction against the schema; time is read directly via
/// [`atom.time`](Atom::time) by calendar indicators
/// (`Year`/`Month`/`Day`/…, in [`crate::indicators::calendar`]). Cheap to
/// clone — a `Candle` memcpy, an `Option<Timestamp>` (also `Copy`), plus, when
/// overlays are present, two atomic bumps (the shared schema `Arc` and the
/// per-atom `Arc<[Real]>` values); the overlay pointers are `Arc`, so an atom
/// is `Send + Sync`.
///
/// `time` is deliberately `Option`: a bare backtest fed a raw candle stream
/// has no notion of wall-clock time, and the calendar indicators return `None`
/// on such input. A `Candle`-only construction path leaves it `None`; feed a
/// [`Timestamp`] via [`Atom::with_time`] (or
/// [`Atom::with_overlays_and_time`]) when the bar's open time is known.
#[derive(Debug, Clone)]
pub struct Atom {
    /// The OHLCV bar for this tick.
    pub candle: Candle,
    /// The bar's open time as a UTC millisecond epoch, if known. `None` for
    /// synthetic / time-agnostic bars; the calendar indicators read this and
    /// emit `None` when it is absent.
    pub time: Option<Timestamp>,
    /// Optional per-bar overlay values (external series keyed by name).
    pub overlays: Option<OverlayInfo>,
}

impl Atom {
    /// An atom carrying only a candle (no time, no overlays).
    pub fn new(candle: Candle) -> Self {
        Self {
            candle,
            time: None,
            overlays: None,
        }
    }

    /// An atom with a candle and a bar-open [`Timestamp`] (no overlays).
    pub fn with_time(candle: Candle, time: Timestamp) -> Self {
        Self {
            candle,
            time: Some(time),
            overlays: None,
        }
    }

    /// An atom with both a candle and bound overlay values.
    pub fn with_overlays(candle: Candle, overlays: OverlayInfo) -> Self {
        Self {
            candle,
            time: None,
            overlays: Some(overlays),
        }
    }

    /// An atom carrying a candle, a bar-open [`Timestamp`], and bound overlay
    /// values.
    pub fn with_overlays_and_time(
        candle: Candle,
        overlays: OverlayInfo,
        time: Timestamp,
    ) -> Self {
        Self {
            candle,
            time: Some(time),
            overlays: Some(overlays),
        }
    }
}

impl From<Candle> for Atom {
    fn from(candle: Candle) -> Self {
        Self::new(candle)
    }
}

/// Equality by bar-open [`Timestamp`]. An atom is identified by *when* it is,
/// not by the OHLCV numbers or overlays that decorate that instant, so two
/// atoms are equal iff their `time` fields match. Atoms with `time = None`
/// compare equal to each other (the `None`-until-timed convention).
impl PartialEq for Atom {
    fn eq(&self, other: &Self) -> bool {
        self.time == other.time
    }
}

impl Eq for Atom {}

/// Chronological ordering: atoms sort by their bar-open [`Timestamp`]. `None`
/// times sort *before* any `Some` (consistent with `Option`'s derived
/// ordering) so a batch of undated synthetic bars stays clustered at the head
/// of a sorted list.
impl PartialOrd for Atom {
    fn partial_cmp(&self, other: &Self) -> Option<std::cmp::Ordering> {
        Some(self.cmp(other))
    }
}

impl Ord for Atom {
    fn cmp(&self, other: &Self) -> std::cmp::Ordering {
        self.time.cmp(&other.time)
    }
}

/// A per-bar snapshot of several assets — a keyed collection of [`Atom`]s.
///
/// The multi-asset input frame that lets a strategy or an indicator reason
/// about more than one instrument at a time. Each key names one asset (e.g.
/// `"BTC"`, `"ETH"`), each value is that asset's [`Atom`] for the current
/// bar. The [`Pick`](crate::indicators::Pick) leaf projects one asset out of
/// the snapshot as an `Indicator<Output = Atom>`, so cross-asset expressions
/// compose from the same primitives as single-asset ones:
///
/// ```ignore
/// use fugazi::indicators::{Close, Pick};
/// use fugazi::prelude::*;
/// // BTC/ETH close spread as a first-class Real-output indicator.
/// let spread = Close::of(Pick::new("BTC"))
///     .sub(Close::of(Pick::new("ETH")));
/// ```
///
/// Generic over the key type `K`. `K: Eq + Hash` is required to look values
/// up; `K: Clone` is required to move the snapshot through the indicator
/// pipeline (each `update` clones the emitted snapshot into downstream
/// consumers). Semantics mirror the underlying [`HashMap`] — no order
/// guarantees, no automatic time alignment; the driver that builds each
/// bar's snapshot is responsible for feeding a consistent key set.
#[derive(Debug, Clone)]
pub struct Snapshot<K> {
    atoms: HashMap<K, Atom>,
}

impl<K: Eq + std::hash::Hash> PartialEq for Snapshot<K> {
    fn eq(&self, other: &Self) -> bool {
        self.atoms == other.atoms
    }
}

impl<K> Snapshot<K> {
    /// An empty snapshot with no assets.
    pub fn new() -> Self {
        Self {
            atoms: HashMap::new(),
        }
    }

    /// Number of assets in this snapshot.
    pub fn len(&self) -> usize {
        self.atoms.len()
    }

    /// True if this snapshot carries no assets.
    pub fn is_empty(&self) -> bool {
        self.atoms.is_empty()
    }

    /// Iterate over `(key, atom)` pairs.
    pub fn iter(&self) -> impl Iterator<Item = (&K, &Atom)> {
        self.atoms.iter()
    }

    /// Iterate over the keys.
    pub fn keys(&self) -> impl Iterator<Item = &K> {
        self.atoms.keys()
    }
}

impl<K: Eq + std::hash::Hash> Snapshot<K> {
    /// Look up an asset by key. `None` if the key is not present in this bar.
    pub fn get<Q>(&self, key: &Q) -> Option<&Atom>
    where
        K: std::borrow::Borrow<Q>,
        Q: Eq + std::hash::Hash + ?Sized,
    {
        self.atoms.get(key)
    }

    /// Insert or replace an asset's atom for this bar, returning the previous
    /// atom if any.
    pub fn insert(&mut self, key: K, atom: Atom) -> Option<Atom> {
        self.atoms.insert(key, atom)
    }

    /// True if this snapshot has an atom for the given key.
    pub fn contains_key<Q>(&self, key: &Q) -> bool
    where
        K: std::borrow::Borrow<Q>,
        Q: Eq + std::hash::Hash + ?Sized,
    {
        self.atoms.contains_key(key)
    }
}

impl<K> Default for Snapshot<K> {
    fn default() -> Self {
        Self::new()
    }
}

impl<K: Eq + std::hash::Hash> FromIterator<(K, Atom)> for Snapshot<K> {
    fn from_iter<I: IntoIterator<Item = (K, Atom)>>(iter: I) -> Self {
        Self {
            atoms: iter.into_iter().collect(),
        }
    }
}

impl<K: Eq + std::hash::Hash> From<HashMap<K, Atom>> for Snapshot<K> {
    fn from(atoms: HashMap<K, Atom>) -> Self {
        Self { atoms }
    }
}

impl<K> From<Snapshot<K>> for HashMap<K, Atom> {
    fn from(snap: Snapshot<K>) -> Self {
        snap.atoms
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn schema_indexes_are_stable_and_idempotent() {
        let mut b = Schema::builder();
        assert_eq!(b.add_real("a"), 0);
        assert_eq!(b.add_real("b"), 1);
        assert_eq!(b.add_real("a"), 0); // idempotent
        assert_eq!(b.len(), 2);
        let s = b.finish();
        assert_eq!(s.index_of("a"), Some(0));
        assert_eq!(s.index_of("b"), Some(1));
        assert_eq!(s.index_of("missing"), None);
    }

    #[test]
    fn schema_records_per_column_type() {
        let mut b = Schema::builder();
        b.add_real("vol_20");
        b.add_bool("risk_on");
        b.add_str("regime");
        let s = b.finish();
        assert_eq!(s.type_of_key("vol_20"), Some(OverlayType::Real));
        assert_eq!(s.type_of_key("risk_on"), Some(OverlayType::Bool));
        assert_eq!(s.type_of_key("regime"), Some(OverlayType::Str));
        assert_eq!(s.type_of_key("missing"), None);
        // Insertion order is preserved for keys().
        let keys: Vec<&str> = s.keys().collect();
        assert_eq!(keys, vec!["vol_20", "risk_on", "regime"]);
    }

    #[test]
    #[should_panic(expected = "cannot re-register")]
    fn schema_rejects_type_mismatch_on_reregister() {
        let mut b = Schema::builder();
        b.add_real("x");
        b.add_bool("x");
    }

    #[test]
    fn overlay_info_binds_heterogeneous_values() {
        let mut b = Schema::builder();
        b.add_real("vol_20");
        b.add_bool("risk_on");
        b.add_str("regime");
        let s = b.finish();
        let overlays = OverlayInfo::new(
            s.clone(),
            vec![
                OverlayValue::Real(0.12),
                OverlayValue::Bool(true),
                OverlayValue::Str(Arc::from("bull")),
            ],
        );
        assert_eq!(overlays.get_real(0), Some(0.12));
        assert_eq!(overlays.get_bool(1), Some(true));
        assert_eq!(overlays.get_str(2).map(|s| s.as_ref()), Some("bull"));

        // Typed accessors return None on a type mismatch (defensive).
        assert_eq!(overlays.get_bool(0), None);
        assert_eq!(overlays.get_real(1), None);
        assert_eq!(overlays.get_str(0), None);

        // get_by_key resolves the type-tagged value.
        assert!(matches!(
            overlays.get_by_key("regime"),
            Some(OverlayValue::Str(_))
        ));
        assert_eq!(overlays.get_by_key("missing"), None);
    }

    #[test]
    #[should_panic(expected = "overlay values length")]
    fn overlay_info_rejects_length_mismatch() {
        let mut b = Schema::builder();
        b.add_real("a");
        b.add_real("b");
        let s = b.finish();
        let _ = OverlayInfo::new(s, vec![OverlayValue::Real(0.0)]);
    }

    #[test]
    #[should_panic(expected = "has type")]
    fn overlay_info_rejects_type_mismatch_per_slot() {
        let mut b = Schema::builder();
        b.add_real("a");
        let s = b.finish();
        let _ = OverlayInfo::new(s, vec![OverlayValue::Bool(true)]);
    }

    #[test]
    fn schema_empty_is_zero_length() {
        let s = Schema::empty();
        assert!(s.is_empty());
        assert_eq!(s.len(), 0);
    }

    #[test]
    fn atom_from_candle_leaves_overlays_none() {
        let c = Candle::new(1.0, 2.0, 0.5, 1.5, 100.0);
        let a: Atom = c.into();
        assert!(a.overlays.is_none());
        assert_eq!(a.candle.close, 1.5);
    }
}
