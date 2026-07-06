//! Core scalar and market-data types shared across the crate.

use std::collections::HashMap;
use std::rc::Rc;
use std::sync::Arc;

/// The scalar type used throughout the crate for prices and indicator outputs.
///
/// Centralised as an alias so the whole library can be switched to another
/// floating-point width (or a fixed-point type) in one place.
pub type Real = f64;

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

/// Read-only column name → index registry for the values array of an
/// [`OverlayInfo`].
///
/// A schema is built with [`Schema::builder`] and frozen by
/// [`SchemaBuilder::finish`] into an `Arc<Schema>` shared across every bar of a
/// run. The `Arc` guarantees the schema can't change after any `OverlayInfo`
/// has bound values against it, so an index resolved once at indicator
/// construction stays valid for the whole run.
#[derive(Debug, Default)]
pub struct Schema {
    indexes: HashMap<String, usize>,
    len: usize,
}

impl Schema {
    /// Start building a new schema.
    pub fn builder() -> SchemaBuilder {
        SchemaBuilder {
            schema: Schema::default(),
        }
    }

    /// Number of registered columns — the required length of an
    /// [`OverlayInfo`] values array bound to this schema.
    pub fn len(&self) -> usize {
        self.len
    }

    /// Whether no columns are registered.
    pub fn is_empty(&self) -> bool {
        self.len == 0
    }

    /// Resolve a column name to its index, `None` if not registered.
    pub fn index_of(&self, key: &str) -> Option<usize> {
        self.indexes.get(key).copied()
    }

    /// Whether `key` is a registered column.
    pub fn contains(&self, key: &str) -> bool {
        self.indexes.contains_key(key)
    }

    /// Iterate the registered column names, in arbitrary order.
    pub fn keys(&self) -> impl Iterator<Item = &str> {
        self.indexes.keys().map(String::as_str)
    }
}

/// Mutable builder for a [`Schema`]. Register columns with [`add`](Self::add),
/// then freeze into an `Arc<Schema>` with [`finish`](Self::finish); the frozen
/// schema is read-only so the indexes it hands out are stable for the run.
#[derive(Debug, Default)]
pub struct SchemaBuilder {
    schema: Schema,
}

impl SchemaBuilder {
    /// Register a column and return its index. Idempotent — a repeated `key`
    /// returns the previously-assigned index without adding a new slot.
    pub fn add(&mut self, key: impl Into<String>) -> usize {
        let key = key.into();
        if let Some(&i) = self.schema.indexes.get(&key) {
            return i;
        }
        let i = self.schema.len;
        self.schema.indexes.insert(key, i);
        self.schema.len += 1;
        i
    }

    /// Number of columns registered so far.
    pub fn len(&self) -> usize {
        self.schema.len
    }

    /// Whether no columns have been registered yet.
    pub fn is_empty(&self) -> bool {
        self.schema.len == 0
    }

    /// Freeze into a shareable, read-only schema.
    pub fn finish(self) -> Arc<Schema> {
        Arc::new(self.schema)
    }
}

/// Per-bar overlay data attached to an [`Atom`]: a shared [`Schema`] plus that
/// bar's values in schema order.
///
/// Cheap to clone (one atomic bump for the shared `Arc<Schema>` and one
/// non-atomic bump for the per-atom `Rc<[Real]>`, no allocation), which is what
/// [`Combine`](crate::indicators::Combine) needs when it feeds the same
/// [`Atom`] to both sides.
#[derive(Debug, Clone)]
pub struct OverlayInfo {
    schema: Arc<Schema>,
    values: Rc<[Real]>,
}

impl OverlayInfo {
    /// Bind `values` to a fixed `schema`. `values.len()` must equal
    /// `schema.len()`.
    ///
    /// # Panics
    /// Panics if `values.len() != schema.len()`.
    pub fn new(schema: Arc<Schema>, values: impl Into<Rc<[Real]>>) -> Self {
        let values = values.into();
        assert_eq!(
            values.len(),
            schema.len(),
            "overlay values length ({}) must match schema length ({})",
            values.len(),
            schema.len(),
        );
        Self { schema, values }
    }

    /// The shared schema this overlay is bound to.
    pub fn schema(&self) -> &Arc<Schema> {
        &self.schema
    }

    /// The values array, in schema order.
    pub fn values(&self) -> &[Real] {
        &self.values
    }

    /// Read the value at a resolved column index, `None` if out of bounds.
    pub fn get(&self, index: usize) -> Option<Real> {
        self.values.get(index).copied()
    }

    /// Look up a column by name (a one-time convenience — hot-path readers
    /// resolve the index at construction and read [`get`](Self::get)).
    pub fn get_by_key(&self, key: &str) -> Option<Real> {
        self.schema.index_of(key).and_then(|i| self.get(i))
    }
}

/// A single bar's input to the indicator chain: an OHLCV [`Candle`] and,
/// optionally, per-bar overlay values keyed by a shared [`Schema`].
///
/// The `Input` associated type of every [`Indicator`](crate::Indicator) that
/// reads bar-shaped data. OHLCV access stays a direct field read
/// (`atom.candle.close`); overlay access goes through an index resolved once at
/// construction against the schema. Cheap to clone — a `Candle` memcpy plus,
/// when present, one atomic bump for the shared schema `Arc` and one
/// non-atomic bump for the per-atom `Rc<[Real]>` values.
#[derive(Debug, Clone)]
pub struct Atom {
    /// The OHLCV bar for this tick.
    pub candle: Candle,
    /// Optional per-bar overlay values (external series keyed by name).
    pub overlays: Option<OverlayInfo>,
}

impl Atom {
    /// An atom carrying only a candle (no overlays).
    pub fn new(candle: Candle) -> Self {
        Self {
            candle,
            overlays: None,
        }
    }

    /// An atom with both a candle and bound overlay values.
    pub fn with_overlays(candle: Candle, overlays: OverlayInfo) -> Self {
        Self {
            candle,
            overlays: Some(overlays),
        }
    }
}

impl From<Candle> for Atom {
    fn from(candle: Candle) -> Self {
        Self::new(candle)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn schema_indexes_are_stable_and_idempotent() {
        let mut b = Schema::builder();
        assert_eq!(b.add("a"), 0);
        assert_eq!(b.add("b"), 1);
        assert_eq!(b.add("a"), 0); // idempotent
        assert_eq!(b.len(), 2);
        let s = b.finish();
        assert_eq!(s.index_of("a"), Some(0));
        assert_eq!(s.index_of("b"), Some(1));
        assert_eq!(s.index_of("missing"), None);
    }

    #[test]
    fn overlay_info_binds_values_to_schema() {
        let mut b = Schema::builder();
        b.add("vol_20");
        b.add("regime");
        let s = b.finish();
        let overlays = OverlayInfo::new(s.clone(), vec![0.12, 1.0]);
        assert_eq!(overlays.get(0), Some(0.12));
        assert_eq!(overlays.get_by_key("regime"), Some(1.0));
        assert_eq!(overlays.get_by_key("missing"), None);
    }

    #[test]
    #[should_panic(expected = "overlay values length")]
    fn overlay_info_rejects_length_mismatch() {
        let mut b = Schema::builder();
        b.add("a");
        b.add("b");
        let s = b.finish();
        let _ = OverlayInfo::new(s, vec![0.0]); // 1 value for a 2-column schema
    }

    #[test]
    fn atom_from_candle_leaves_overlays_none() {
        let c = Candle::new(1.0, 2.0, 0.5, 1.5, 100.0);
        let a: Atom = c.into();
        assert!(a.overlays.is_none());
        assert_eq!(a.candle.close, 1.5);
    }
}
