//! [`Get`]: a leaf source that pulls one overlay column out of each [`Atom`] by
//! key. Complements the [`Field`](super::Field) accessors, which project one of
//! the built-in OHLCV fields of the [`Candle`](crate::Candle) — `Get` reads the
//! optional side-channel data an [`OverlayInfo`] carries.

use std::sync::Arc;

use crate::indicator::Indicator;
use crate::types::{Atom, OverlayInfo, Real, Schema};

/// Returned by [`Get::try_new`] when the requested key is not registered in the
/// schema.
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

/// Extracts one overlay column from each [`Atom`], selected by its schema
/// index.
///
/// Built with [`Get::new`] (panics on an unknown key) or [`Get::try_new`]
/// (returns [`UnknownKey`]). The key is resolved against the shared
/// [`Schema`] *at construction* — the runtime object holds only the resolved
/// `usize` index, so per-bar access is one array read.
///
/// Reads `None` before the first bar. Once fed, reads `Some(v)` when the
/// atom's [`overlays`](Atom::overlays) is bound to the schema this `Get` was
/// built against and carries a value at the resolved index, `None` on any
/// atom whose `overlays` is `None` (so a signal composed with a `Get` reads
/// `false` on overlay-free bars via [`is_true`](crate::indicators::BoolIndicatorExt::is_true)).
///
/// ```
/// use fugazi::prelude::*;
/// use fugazi::indicators::Get;
///
/// let mut b = Schema::builder();
/// b.add("vol_20");
/// let schema = b.finish();
///
/// let mut vol = Get::new(&schema, "vol_20");
/// let candle = Candle::new(100.0, 101.0, 99.0, 100.5, 1_000.0);
/// let overlays = OverlayInfo::new(schema.clone(), vec![0.12]);
/// let atom = Atom::with_overlays(candle, overlays);
/// assert_eq!(vol.update(atom), Some(0.12));
/// ```
#[derive(Debug, Clone)]
pub struct Get {
    /// Kept for a debug-time schema-mismatch check and for diagnostic use.
    schema: Arc<Schema>,
    index: usize,
    /// Latest extracted value; `None` before the first bar (and `None` on any
    /// atom whose `overlays` is absent or bound to a different schema).
    pub value: Option<Real>,
}

impl Get {
    /// Build a `Get` that reads the overlay column `key` from every input
    /// [`Atom`]. Resolves the key against `schema` once at construction.
    ///
    /// # Panics
    /// Panics if `key` is not registered in `schema`. Use [`try_new`](Self::try_new)
    /// for a fallible constructor.
    pub fn new(schema: &Arc<Schema>, key: &str) -> Self {
        Self::try_new(schema, key).unwrap_or_else(|e| panic!("{e}"))
    }

    /// Fallible constructor: returns [`UnknownKey`] if `key` is not registered
    /// in `schema`.
    pub fn try_new(schema: &Arc<Schema>, key: &str) -> Result<Self, UnknownKey> {
        let index = schema.index_of(key).ok_or_else(|| UnknownKey {
            key: key.to_string(),
        })?;
        Ok(Self {
            schema: schema.clone(),
            index,
            value: None,
        })
    }

    /// The resolved column index this `Get` reads.
    pub fn index(&self) -> usize {
        self.index
    }

    /// The schema this `Get` was resolved against.
    pub fn schema(&self) -> &Arc<Schema> {
        &self.schema
    }
}

/// Read from `overlays` if it is bound to the same schema `Arc` we resolved
/// against — a cheap pointer-equality check that catches a strategy fed
/// atoms built from a different schema (indexes would silently mis-align).
fn read(schema: &Arc<Schema>, index: usize, overlays: Option<&OverlayInfo>) -> Option<Real> {
    let ov = overlays?;
    if !Arc::ptr_eq(schema, ov.schema()) {
        // Schema mismatch: index would refer to a different column. Prefer
        // `None` over a silent wrong read.
        return None;
    }
    ov.get(index)
}

impl Indicator for Get {
    type Input = Atom;
    type Output = Real;

    fn update(&mut self, atom: Atom) -> Option<Real> {
        self.value = read(&self.schema, self.index, atom.overlays.as_ref());
        self.value
    }

    fn value(&self) -> Option<Real> {
        self.value
    }

    /// `1` — one bar to receive the first overlay value. Ready as soon as an
    /// [`Atom`] with matching-schema overlays arrives.
    fn warm_up_period(&self) -> usize {
        1
    }

    fn reset(&mut self) {
        self.value = None;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::types::Candle;

    fn schema_with(keys: &[&str]) -> Arc<Schema> {
        let mut b = Schema::builder();
        for k in keys {
            b.add(*k);
        }
        b.finish()
    }

    fn candle() -> Candle {
        Candle::new(100.0, 101.0, 99.0, 100.5, 1_000.0)
    }

    #[test]
    fn resolves_index_at_construction_and_reads_values() {
        let schema = schema_with(&["vol_20", "regime"]);
        let mut vol = Get::new(&schema, "vol_20");
        assert_eq!(vol.index(), 0);

        let overlays = OverlayInfo::new(schema.clone(), vec![0.12, 1.0]);
        let atom = Atom::with_overlays(candle(), overlays);
        assert_eq!(vol.update(atom), Some(0.12));
    }

    #[test]
    fn none_when_atom_has_no_overlays() {
        let schema = schema_with(&["vol_20"]);
        let mut vol = Get::new(&schema, "vol_20");
        assert_eq!(vol.update(Atom::new(candle())), None);
    }

    #[test]
    fn none_when_atom_bound_to_different_schema() {
        let schema_a = schema_with(&["vol_20", "regime"]);
        let schema_b = schema_with(&["regime", "vol_20"]);
        let mut vol = Get::new(&schema_a, "vol_20"); // index 0 in A
        let overlays_b = OverlayInfo::new(schema_b, vec![1.0, 0.12]); // 0.12 lives at index 1 here
        let atom = Atom::with_overlays(candle(), overlays_b);
        // Mismatched schema Arc: refuse the read rather than return 1.0.
        assert_eq!(vol.update(atom), None);
    }

    #[test]
    fn try_new_reports_unknown_key() {
        let schema = schema_with(&["vol_20"]);
        let err = Get::try_new(&schema, "missing").unwrap_err();
        assert_eq!(err.key, "missing");
    }

    #[test]
    #[should_panic(expected = "unknown overlay key: missing")]
    fn new_panics_on_unknown_key() {
        let schema = schema_with(&["vol_20"]);
        let _ = Get::new(&schema, "missing");
    }
}
