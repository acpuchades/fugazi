//! [`GetReal`], [`GetBool`], [`GetStr`]: three leaf sources that each pull one
//! overlay column out of each [`Atom`] by key. Complement the
//! [`Field`](super::Field) accessors, which project one of the built-in OHLCV
//! fields of the [`Candle`](crate::Candle) — the `Get*` leaves read the
//! optional side-channel data an [`OverlayInfo`] carries.
//!
//! One typed leaf per [`OverlayType`]: `GetReal` produces `Real`, `GetBool`
//! produces `bool` (a signal leaf), `GetStr` produces `Arc<str>`. The
//! constructor resolves both the column *index* and the column *type* against
//! the shared [`Schema`] at build time, so per-bar access is one array read
//! plus a variant match (which the compiler can hoist).

use std::sync::Arc;

use crate::indicator::Indicator;
use crate::indicators::Identity;
use crate::types::{Atom, OverlayInfo, OverlayType, Real, Schema};

/// Returned by [`GetReal::try_new`] (and the other typed constructors) when the
/// requested key is not registered in the schema.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct UnknownKey {
    pub key: String,
}

impl std::fmt::Display for UnknownKey {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "unknown overlay key: {}", self.key)
    }
}

impl std::error::Error for UnknownKey {}

/// Returned by [`GetReal::try_new`] / [`GetBool::try_new`] / [`GetStr::try_new`]
/// when the key exists but its declared [`OverlayType`] doesn't match the leaf
/// being constructed.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TypeMismatch {
    pub key: String,
    pub expected: OverlayType,
    pub actual: OverlayType,
}

impl std::fmt::Display for TypeMismatch {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "overlay column {:?} has type {}, expected {}",
            self.key, self.actual, self.expected,
        )
    }
}

impl std::error::Error for TypeMismatch {}

/// A `Get*` leaf's construction failure: either the key is missing or its
/// declared type doesn't match.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum GetError {
    UnknownKey(UnknownKey),
    TypeMismatch(TypeMismatch),
}

impl std::fmt::Display for GetError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            GetError::UnknownKey(e) => e.fmt(f),
            GetError::TypeMismatch(e) => e.fmt(f),
        }
    }
}

impl std::error::Error for GetError {}

impl From<UnknownKey> for GetError {
    fn from(e: UnknownKey) -> Self {
        GetError::UnknownKey(e)
    }
}

impl From<TypeMismatch> for GetError {
    fn from(e: TypeMismatch) -> Self {
        GetError::TypeMismatch(e)
    }
}

/// Resolve `key` against `schema`, checking the declared column type matches
/// `expected`. Shared by every `Get*::try_new`.
fn resolve(schema: &Arc<Schema>, key: &str, expected: OverlayType) -> Result<usize, GetError> {
    let index = schema.index_of(key).ok_or_else(|| UnknownKey {
        key: key.to_string(),
    })?;
    let actual = schema.type_of(index).expect("index in range");
    if actual != expected {
        return Err(TypeMismatch {
            key: key.to_string(),
            expected,
            actual,
        }
        .into());
    }
    Ok(index)
}

/// Read from `overlays` if it is bound to the same schema `Arc` this leaf was
/// resolved against — a cheap pointer-equality check that catches a strategy
/// fed atoms built from a different schema (indexes would silently mis-align).
fn read_slot<'a>(
    schema: &Arc<Schema>,
    overlays: Option<&'a OverlayInfo>,
) -> Option<&'a OverlayInfo> {
    let ov = overlays?;
    if !Arc::ptr_eq(schema, ov.schema()) {
        return None;
    }
    Some(ov)
}

// ---------------------------------------------------------------------------
// GetReal
// ---------------------------------------------------------------------------

/// Extracts one `Real` overlay column from each [`Atom`], selected by its
/// schema index.
///
/// Built with [`GetReal::new`] (panics on error) or [`GetReal::try_new`]
/// (returns [`GetError`]). The key + type are resolved against the shared
/// [`Schema`] *at construction* — the runtime object holds only the resolved
/// `usize` index, so per-bar access is one array read plus a variant match.
///
/// Reads `None` before the first bar and `None` on any atom whose
/// [`overlays`](Atom::overlays) is absent or bound to a different schema.
///
/// ```
/// use fugazi::prelude::*;
/// use fugazi::indicators::GetReal;
///
/// let mut b = Schema::builder();
/// b.add_real("vol_20");
/// let schema = b.finish();
///
/// let mut vol = GetReal::new(&schema, "vol_20");
/// let candle = Candle::new(100.0, 101.0, 99.0, 100.5, 1_000.0);
/// let overlays = OverlayInfo::new(schema.clone(), vec![OverlayValue::Real(0.12)]);
/// let atom = Atom::with_overlays(candle, overlays);
/// assert_eq!(vol.update(atom), Some(0.12));
/// ```
#[derive(Debug, Clone)]
pub struct GetReal<S = Identity<Atom>> {
    schema: Arc<Schema>,
    index: usize,
    source: S,
    /// Latest extracted value; `None` before the first bar (and `None` on any
    /// atom whose `overlays` is absent or bound to a different schema).
    pub value: Option<Real>,
}

impl GetReal<Identity<Atom>> {
    /// Build a `GetReal` for `key` rooted on the raw [`Atom`] input stream.
    /// Resolves the key + type against `schema` once at construction.
    ///
    /// # Panics
    /// Panics if `key` is not registered or its declared type is not `Real`.
    /// Use [`try_new`](Self::try_new) for a fallible constructor.
    pub fn new(schema: &Arc<Schema>, key: &str) -> Self {
        Self::try_new(schema, key).unwrap_or_else(|e| panic!("{e}"))
    }

    /// Fallible constructor for the default atom-rooted leaf.
    pub fn try_new(schema: &Arc<Schema>, key: &str) -> Result<Self, GetError> {
        Self::try_of(schema, key, Identity::new())
    }
}

impl<S> GetReal<S> {
    /// Build a `GetReal` for `key` rooted on a custom atom-emitting source.
    /// Panics on unknown key or type mismatch — use [`try_of`](Self::try_of)
    /// for the fallible form.
    pub fn of(schema: &Arc<Schema>, key: &str, source: S) -> Self {
        Self::try_of(schema, key, source).unwrap_or_else(|e| panic!("{e}"))
    }

    /// Fallible constructor rooted on a custom atom-emitting source.
    pub fn try_of(schema: &Arc<Schema>, key: &str, source: S) -> Result<Self, GetError> {
        let index = resolve(schema, key, OverlayType::Real)?;
        Ok(Self {
            schema: schema.clone(),
            index,
            source,
            value: None,
        })
    }

    /// The resolved column index this leaf reads.
    pub fn index(&self) -> usize {
        self.index
    }

    /// The schema this leaf was resolved against.
    pub fn schema(&self) -> &Arc<Schema> {
        &self.schema
    }
}

impl<S: Indicator<Output = Atom>> Indicator for GetReal<S> {
    type Input = S::Input;
    type Output = Real;

    fn update(&mut self, input: S::Input) -> Option<Real> {
        self.value = self.source.update(input).and_then(|atom| {
            read_slot(&self.schema, atom.overlays.as_ref())
                .and_then(|ov| ov.get_real(self.index))
        });
        self.value
    }

    fn value(&self) -> Option<Real> {
        self.value
    }

    /// `1` — one bar to receive the first overlay value (plus any warm-up
    /// the source contributes, per the standard source-generic pattern).
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

// ---------------------------------------------------------------------------
// GetBool
// ---------------------------------------------------------------------------

/// Extracts one `Bool` overlay column from each [`Atom`]. A signal leaf.
///
/// The bool twin of [`GetReal`]; see that type's docs for the construction and
/// error semantics.
#[derive(Debug, Clone)]
pub struct GetBool<S = Identity<Atom>> {
    schema: Arc<Schema>,
    index: usize,
    source: S,
    /// Latest extracted value; `None` before the first bar (and `None` on any
    /// atom whose `overlays` is absent or bound to a different schema).
    pub value: Option<bool>,
}

impl GetBool<Identity<Atom>> {
    /// Build a `GetBool` for `key` rooted on the raw [`Atom`] input stream.
    /// Panics on unknown key or type mismatch.
    pub fn new(schema: &Arc<Schema>, key: &str) -> Self {
        Self::try_new(schema, key).unwrap_or_else(|e| panic!("{e}"))
    }

    /// Fallible constructor for the default atom-rooted leaf.
    pub fn try_new(schema: &Arc<Schema>, key: &str) -> Result<Self, GetError> {
        Self::try_of(schema, key, Identity::new())
    }
}

impl<S> GetBool<S> {
    /// Build a `GetBool` for `key` rooted on a custom atom-emitting source.
    /// Panics on unknown key or type mismatch — use [`try_of`](Self::try_of).
    pub fn of(schema: &Arc<Schema>, key: &str, source: S) -> Self {
        Self::try_of(schema, key, source).unwrap_or_else(|e| panic!("{e}"))
    }

    /// Fallible constructor rooted on a custom atom-emitting source.
    pub fn try_of(schema: &Arc<Schema>, key: &str, source: S) -> Result<Self, GetError> {
        let index = resolve(schema, key, OverlayType::Bool)?;
        Ok(Self {
            schema: schema.clone(),
            index,
            source,
            value: None,
        })
    }

    pub fn index(&self) -> usize {
        self.index
    }

    pub fn schema(&self) -> &Arc<Schema> {
        &self.schema
    }
}

impl<S: Indicator<Output = Atom>> Indicator for GetBool<S> {
    type Input = S::Input;
    type Output = bool;

    fn update(&mut self, input: S::Input) -> Option<bool> {
        self.value = self.source.update(input).and_then(|atom| {
            read_slot(&self.schema, atom.overlays.as_ref())
                .and_then(|ov| ov.get_bool(self.index))
        });
        self.value
    }

    fn value(&self) -> Option<bool> {
        self.value
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

// ---------------------------------------------------------------------------
// GetStr
// ---------------------------------------------------------------------------

/// Extracts one `Str` overlay column from each [`Atom`].
///
/// The string twin of [`GetReal`]; see that type's docs for the construction
/// and error semantics. Outputs `Arc<str>` so the shared backing storage of
/// the underlying overlay value is preserved (no allocation per bar).
#[derive(Debug, Clone)]
pub struct GetStr<S = Identity<Atom>> {
    schema: Arc<Schema>,
    index: usize,
    source: S,
    /// Latest extracted value; `None` before the first bar (and `None` on any
    /// atom whose `overlays` is absent or bound to a different schema).
    pub value: Option<Arc<str>>,
}

impl GetStr<Identity<Atom>> {
    /// Build a `GetStr` for `key` rooted on the raw [`Atom`] input stream.
    /// Panics on unknown key or type mismatch.
    pub fn new(schema: &Arc<Schema>, key: &str) -> Self {
        Self::try_new(schema, key).unwrap_or_else(|e| panic!("{e}"))
    }

    /// Fallible constructor for the default atom-rooted leaf.
    pub fn try_new(schema: &Arc<Schema>, key: &str) -> Result<Self, GetError> {
        Self::try_of(schema, key, Identity::new())
    }
}

impl<S> GetStr<S> {
    /// Build a `GetStr` for `key` rooted on a custom atom-emitting source.
    /// Panics on unknown key or type mismatch — use [`try_of`](Self::try_of).
    pub fn of(schema: &Arc<Schema>, key: &str, source: S) -> Self {
        Self::try_of(schema, key, source).unwrap_or_else(|e| panic!("{e}"))
    }

    /// Fallible constructor rooted on a custom atom-emitting source.
    pub fn try_of(schema: &Arc<Schema>, key: &str, source: S) -> Result<Self, GetError> {
        let index = resolve(schema, key, OverlayType::Str)?;
        Ok(Self {
            schema: schema.clone(),
            index,
            source,
            value: None,
        })
    }

    pub fn index(&self) -> usize {
        self.index
    }

    pub fn schema(&self) -> &Arc<Schema> {
        &self.schema
    }
}

impl<S: Indicator<Output = Atom>> Indicator for GetStr<S> {
    type Input = S::Input;
    type Output = Arc<str>;

    fn update(&mut self, input: S::Input) -> Option<Arc<str>> {
        self.value = self.source.update(input).and_then(|atom| {
            read_slot(&self.schema, atom.overlays.as_ref())
                .and_then(|ov| ov.get_str(self.index).cloned())
        });
        self.value.clone()
    }

    fn value(&self) -> Option<Arc<str>> {
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
    use crate::types::{Candle, OverlayValue};

    fn schema_v3() -> Arc<Schema> {
        let mut b = Schema::builder();
        b.add_real("vol_20");
        b.add_bool("risk_on");
        b.add_str("regime");
        b.finish()
    }

    fn candle() -> Candle {
        Candle::new(100.0, 101.0, 99.0, 100.5, 1_000.0)
    }

    fn overlays(schema: &Arc<Schema>) -> OverlayInfo {
        OverlayInfo::new(
            schema.clone(),
            vec![
                OverlayValue::Real(0.12),
                OverlayValue::Bool(true),
                OverlayValue::Str(Arc::from("bull")),
            ],
        )
    }

    #[test]
    fn typed_leaves_read_their_own_column() {
        let schema = schema_v3();
        let atom = Atom::with_overlays(candle(), overlays(&schema));

        let mut r = GetReal::new(&schema, "vol_20");
        assert_eq!(r.update(atom.clone()), Some(0.12));

        let mut b = GetBool::new(&schema, "risk_on");
        assert_eq!(b.update(atom.clone()), Some(true));

        let mut s = GetStr::new(&schema, "regime");
        assert_eq!(s.update(atom).as_deref(), Some("bull"));
    }

    #[test]
    fn typed_try_new_rejects_type_mismatch() {
        let schema = schema_v3();
        let err = GetReal::try_new(&schema, "risk_on").unwrap_err();
        assert!(matches!(err, GetError::TypeMismatch(_)));
        let err = GetBool::try_new(&schema, "regime").unwrap_err();
        assert!(matches!(err, GetError::TypeMismatch(_)));
        let err = GetStr::try_new(&schema, "vol_20").unwrap_err();
        assert!(matches!(err, GetError::TypeMismatch(_)));
    }

    #[test]
    fn try_new_reports_unknown_key() {
        let schema = schema_v3();
        let err = GetReal::try_new(&schema, "missing").unwrap_err();
        assert!(matches!(err, GetError::UnknownKey(_)));
    }

    #[test]
    #[should_panic(expected = "unknown overlay key: missing")]
    fn new_panics_on_unknown_key() {
        let schema = schema_v3();
        let _ = GetReal::new(&schema, "missing");
    }

    #[test]
    fn none_when_atom_has_no_overlays() {
        let schema = schema_v3();
        let mut r = GetReal::new(&schema, "vol_20");
        assert_eq!(r.update(Atom::new(candle())), None);
        let mut b = GetBool::new(&schema, "risk_on");
        assert_eq!(b.update(Atom::new(candle())), None);
        let mut s = GetStr::new(&schema, "regime");
        assert_eq!(s.update(Atom::new(candle())), None);
    }

    #[test]
    fn none_when_atom_bound_to_different_schema() {
        // Two structurally-identical schemas — the Arc identity differs, so the
        // read must refuse rather than silently return the value at the same
        // index in the other schema.
        let a = schema_v3();
        let b = schema_v3();
        let mut r = GetReal::new(&a, "vol_20");
        let ov_b = OverlayInfo::new(
            b,
            vec![
                OverlayValue::Real(9.99),
                OverlayValue::Bool(false),
                OverlayValue::Str(Arc::from("bear")),
            ],
        );
        assert_eq!(r.update(Atom::with_overlays(candle(), ov_b)), None);
    }
}
