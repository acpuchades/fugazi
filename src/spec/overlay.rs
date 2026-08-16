//! Reusable overlay core: a `name → NodeSpec` column set that builds a live
//! [`DynIndicator`] per column and computes derived overlay values over a
//! series, merging the results onto an existing [`Schema`].
//!
//! This is the shape the CLI's `fugazi get -x` overlays and the Python
//! `compute_overlays` binding both need. The CLI keeps its own scope grammar
//! (`SYMBOL[FREQ]:`, `@file`, inline `col=expr`, reserved-name rejection) in
//! `src/cli/overlay.rs` and delegates the *build + compute* here; because this
//! module lives under the `spec` feature (not `cli`), the Python wheel can
//! reach it too.
//!
//! ## Compute model
//!
//! Overlay indicators built from an [`NodeSpec`] are **snapshot-rooted** — a
//! [`Pick`](crate::indicators::Pick) reads the atom out of a `Snapshot`. An
//! overlay column is written once and instantiated once per `(symbol, freq)`
//! series, so each instantiation has a **blessed series**: the group key. It's
//! installed as that instantiation's [`Pick::rooted`](crate::indicators::Pick::rooted)
//! root, which is what lets the two readings coexist on one bar —
//!
//! * a bare `!close` (or any `source:`-omitted leaf) reads **its own** series;
//! * `!close { source: !pick { symbol: SPY } }` reaches **across** to SPY.
//!
//! [`compute_snapshots`] is therefore driven by the *whole* multi-symbol
//! snapshot, with one indicator set per series. [`compute_series`] remains for
//! the genuinely single-series case (an untagged `Sequence[Atom]` from the
//! Python binding), where there is nothing to reach across to.
//!
//! Cross-symbol `!get` carries one caveat:
//! [`GetReal`](crate::indicators::GetReal) guards on `Arc::ptr_eq` against the
//! schema it was built with, so reading another series' overlay column only
//! resolves when both series share one schema `Arc` — true within a single
//! `file:` source or one provider's fetch, not across two providers.
//!
//! The output schema is the input schema's columns (same order, same indexes)
//! with the new columns **appended**. Every output atom is bound to that one
//! output-schema `Arc`, so a downstream [`GetReal`](crate::indicators::GetReal)
//! resolved against the returned schema reads both pre-existing and new columns
//! (its `Arc::ptr_eq` guard requires the identical `Arc`). A new column still in
//! its warm-up produces a `None` slot (via [`OverlayInfo::sparse`]) — it reads
//! as absent, exactly like a source before its first sample.

use std::collections::{HashMap, HashSet};
use std::sync::Arc;

use anyhow::{Result, anyhow, bail};
use serde_json::Value as Json;

use crate::indicators::{Book, Position};
use crate::market::{Atom, OverlayInfo, OverlayType, OverlayValue, Schema};
use crate::runtime::{DynIndicator, DynType, DynValue};
use crate::snapshot::Selector;
use crate::time::Frequency;
use crate::types::Snapshot;

use super::expr::NodeSpec;

/// One named overlay column: its output column name, its source expression,
/// and where it was written (a file path or `(inline overlay)`) so a build
/// failure can say *which* column of *which* document is at fault.
#[derive(Debug, Clone)]
pub struct OverlayColumn {
    pub name: String,
    pub spec: NodeSpec,
    pub origin: String,
}

impl OverlayColumn {
    /// Build a fresh, live indicator for this overlay against `schema` — the
    /// overlay side channel visible to `!get { key }` references in the spec.
    /// Overlays never run inside a strategy, so position-anchored leaves
    /// (`entry`, `peak`, `trough`) read from a stub [`Position`] that never
    /// updates, and book-reading leaves from a stub [`Book`]; both stay `None`.
    ///
    /// `root` is the blessed series this instantiation reads for any
    /// `source:`-omitted leaf — see the module docs.
    pub fn build(
        &self,
        schema: &Arc<Schema>,
        root: Option<&Selector<String>>,
    ) -> Result<Box<dyn DynIndicator>> {
        build_overlay(&self.spec, schema, root)
            .map_err(|e| anyhow!("overlay {:?} in {}: {e}", self.name, self.origin))
    }
}

/// Build a live overlay indicator for a bare [`NodeSpec`] against `schema`,
/// with the stub anchors overlays always use (no live `Position` / `Book`).
/// Shared by [`OverlayColumn::build`] and the CLI's scoped `Overlay::build`.
///
/// An overlay expression is ordinary user input, so a `!get` naming a column
/// the stream doesn't carry, a `!pick` with a malformed frequency, or a
/// mixed-type `!match` is bad input rather than a broken invariant: it belongs
/// on the normal error path. [`NodeSpec::try_build`] returns the whole family
/// as values, and the caller prefixes the column and source document.
pub fn build_overlay(
    spec: &NodeSpec,
    schema: &Arc<Schema>,
    root: Option<&Selector<String>>,
) -> Result<Box<dyn DynIndicator>> {
    spec.try_build(&Position::new(), &Book::new(1.0), None, schema, root)
        .map_err(|e| anyhow!("{e}"))
}

/// Parse a flat `name: NodeSpec` JSON map (already `!import`/`!param`-resolved
/// by the caller) into columns, in JSON-map iteration order. Rejects empty
/// names. Vocabulary-neutral — reserving OHLCV column names is a CLI policy and
/// stays in `cli::overlay`.
pub fn columns_from_value(value: Json, label: &str) -> Result<Vec<OverlayColumn>> {
    let Json::Object(map) = value else {
        bail!("overlay {label} must be a mapping of column names to source expressions");
    };
    let mut out = Vec::with_capacity(map.len());
    for (name, expr_value) in map {
        if name.is_empty() {
            bail!("overlay {label}: empty column name");
        }
        let spec: NodeSpec = serde_json::from_value(expr_value)
            .map_err(|e| anyhow!("overlay {name:?} in {label}: {e}"))?;
        out.push(OverlayColumn {
            name,
            spec,
            origin: label.to_string(),
        });
    }
    Ok(out)
}

/// Full YAML entry point: parse `text` → resolve `!import` → substitute
/// `!param` → [`columns_from_value`]. Mirrors the strategy loader's pipeline
/// ([`super::load_value`]).
pub fn columns_from_yaml(
    text: &str,
    params: &HashMap<String, Json>,
    base: &std::path::Path,
    label: &str,
) -> Result<Vec<OverlayColumn>> {
    let value = super::load_value(text, params, base, label)?;
    columns_from_value(value, label)
}

/// A prepared, stateful overlay column: its live indicator plus the resolved
/// output [`OverlayType`] (fixed once from the indicator's `output_type()`).
///
/// [`Clone`] deep-clones the (never-yet-fed) indicator — used to hand each
/// symbol its own fresh indicator set in snapshot mode while every symbol's
/// output binds to the one shared output schema.
#[derive(Clone)]
pub struct PreparedColumn {
    name: String,
    ind: Box<dyn DynIndicator>,
    ty: OverlayType,
}

impl PreparedColumn {
    /// This column's output type.
    pub fn ty(&self) -> OverlayType {
        self.ty
    }

    /// This column's name.
    pub fn name(&self) -> &str {
        &self.name
    }
}

/// Resolve an indicator's scalar overlay type, erroring on a non-scalar output.
///
/// Public because the CLI's `get -x` path builds overlays through
/// [`crate::spec::overlay::build_overlay`] directly rather than through
/// [`prepare`], and needs the same guard — an overlay that emits an `Atom`
/// or a `Candle` cannot be a CSV cell.
pub fn scalar_type(ind: &dyn DynIndicator, name: &str) -> Result<OverlayType> {
    Ok(match ind.output_type() {
        DynType::Real => OverlayType::Real,
        DynType::Bool => OverlayType::Bool,
        DynType::Str => OverlayType::Str,
        other => bail!(
            "overlay column {name:?} produces {other}, \
             not a scalar (Real / Bool / Str) column"
        ),
    })
}

/// Build the output schema (existing columns + appended new columns) and the
/// prepared indicators for a `name: NodeSpec` column set. Each column is built
/// against `existing`, so a `!get { key }` inside an overlay resolves against
/// the input schema (and cannot forward-reference a sibling new column).
pub fn prepare(
    existing: &Arc<Schema>,
    columns: &[OverlayColumn],
) -> Result<(Arc<Schema>, Vec<PreparedColumn>)> {
    prepare_for(existing, columns, None)
}

/// [`prepare`] for one blessed series: every `source:`-omitted leaf in every
/// column reads `root`'s entry out of the snapshot, while an explicit
/// `!pick { symbol: … }` still reaches across. This is the per-series
/// instantiation [`compute_snapshots`] builds once per `(symbol, freq)` key.
pub fn prepare_for(
    existing: &Arc<Schema>,
    columns: &[OverlayColumn],
    root: Option<&Selector<String>>,
) -> Result<(Arc<Schema>, Vec<PreparedColumn>)> {
    let named: Vec<(String, Box<dyn DynIndicator>)> = columns
        .iter()
        .map(|c| Ok((c.name.clone(), c.build(existing, root)?)))
        .collect::<Result<_>>()?;
    prepare_built(existing, named)
}

/// [`prepare`] for pre-built indicators (the Python carrier path) — no
/// `NodeSpec` build step. Infers each column's type from `output_type()`,
/// appends the new columns after the `existing` schema (preserving existing
/// indexes), and errors on a non-scalar output, a new name colliding with an
/// existing column, or a duplicate new name.
pub fn prepare_built(
    existing: &Arc<Schema>,
    named: Vec<(String, Box<dyn DynIndicator>)>,
) -> Result<(Arc<Schema>, Vec<PreparedColumn>)> {
    let mut builder = Schema::builder();
    let existing_names: HashSet<&str> = existing.keys().collect();
    for key in existing.keys() {
        let ty = existing.type_of_key(key).expect("existing key registered");
        add_typed(&mut builder, key, ty);
    }

    let mut seen_new: HashSet<String> = HashSet::new();
    let mut prepared = Vec::with_capacity(named.len());
    for (name, ind) in named {
        let ty = scalar_type(&*ind, &name)?;
        if existing_names.contains(name.as_str()) {
            bail!(
                "overlay column {name:?} collides with an existing overlay column \
                 already on the input series"
            );
        }
        if !seen_new.insert(name.clone()) {
            bail!("overlay column {name:?} is defined more than once");
        }
        add_typed(&mut builder, name.as_str(), ty);
        prepared.push(PreparedColumn { name, ind, ty });
    }
    Ok((builder.finish(), prepared))
}

fn add_typed(builder: &mut crate::market::SchemaBuilder, key: &str, ty: OverlayType) {
    match ty {
        OverlayType::Real => builder.add_real(key),
        OverlayType::Bool => builder.add_bool(key),
        OverlayType::Str => builder.add_str(key),
    };
}

/// Drive one symbol's atom series through the prepared column set, returning the
/// augmented atoms — each bound to `out_schema`, with the first `existing_len`
/// slots carried through from the input atom's overlays and the tail holding the
/// new columns' values (`None` while a column warms up).
///
/// `symbol` seeds the size-1 [`Snapshot`] used to drive snapshot-rooted overlay
/// indicators (`None` → a symbol-less snapshot via [`Snapshot::of_atom`]).
/// Atom-/candle-rooted indicators (from a pre-built Python carrier) are fed the
/// atom / candle directly, chosen per each indicator's `input_type()`.
pub fn compute_series(
    symbol: Option<&str>,
    atoms: &[Atom],
    out_schema: &Arc<Schema>,
    existing_len: usize,
    prepared: &mut [PreparedColumn],
) -> Vec<Atom> {
    atoms
        .iter()
        .map(|atom| {
            let snap: Snapshot<String> = match symbol {
                Some(s) => Snapshot::single(s.to_string(), atom.clone()),
                None => Snapshot::of_atom(atom.clone()),
            };
            drive(prepared, &snap, atom, out_schema, existing_len)
        })
        .collect()
}

/// The **snapshot-aware** entry point: compute every column for every series
/// in a multi-symbol stream, driving each series' indicator set with the whole
/// per-bar snapshot.
///
/// One prepared set is built per `(symbol, freq)` key, in first-appearance
/// order, each rooted on its own key — so within one bar a column reads its
/// own series through a bare `!close` and another series through
/// `!pick { symbol: … }`. A series' set advances only on bars where that
/// series has an entry, exactly as a per-series drive would.
///
/// Returns the output schema alongside the rebuilt snapshots; every augmented
/// atom is bound to that one schema `Arc`.
pub fn compute_snapshots(
    existing: &Arc<Schema>,
    columns: &[OverlayColumn],
    snaps: &[Snapshot<String>],
) -> Result<(Arc<Schema>, Vec<Snapshot<String>>)> {
    // The output schema doesn't depend on which series drives a column — every
    // instantiation has the same shape — so resolve it once, unrooted.
    let (out_schema, _) = prepare_for(existing, columns, None)?;
    let existing_len = existing.len();

    type Key = (Option<String>, Option<Frequency>);
    let mut sets: HashMap<Key, Vec<PreparedColumn>> = HashMap::new();
    let mut out = Vec::with_capacity(snaps.len());

    for snap in snaps {
        let mut rebuilt = Snapshot::<String>::new();
        for (sym, freq, atom) in snap.iter() {
            let key: Key = (sym.cloned(), freq);
            if !sets.contains_key(&key) {
                let root = Selector::<String> {
                    symbol: key.0.clone(),
                    freq: key.1,
                };
                // An untagged entry has nothing to be rooted on; it falls back
                // to the sole-atom unpack, same as the single-series path.
                let root = (!root.is_empty()).then_some(root);
                let (_, prepared) = prepare_for(existing, columns, root.as_ref())?;
                sets.insert(key.clone(), prepared);
            }
            let prepared = sets.get_mut(&key).expect("just inserted");
            let augmented = drive(prepared, snap, atom, &out_schema, existing_len);
            rebuilt.push(key.0, key.1, augmented);
        }
        out.push(rebuilt);
    }
    Ok((out_schema, out))
}

/// Advance one series' prepared columns by a single bar and assemble the
/// augmented atom: the input atom's existing overlay slots carried through
/// verbatim, then one new slot per column (`None` while it warms up).
///
/// `snap` is what snapshot-rooted columns read — the whole multi-symbol
/// snapshot under [`compute_snapshots`], a size-1 one under
/// [`compute_series`]. `atom` is *this* series' bar, used for the carried-over
/// slots and for the atom-/candle-rooted carriers the Python binding can
/// supply.
fn drive(
    prepared: &mut [PreparedColumn],
    snap: &Snapshot<String>,
    atom: &Atom,
    out_schema: &Arc<Schema>,
    existing_len: usize,
) -> Atom {
    let mut slots: Vec<Option<OverlayValue>> = Vec::with_capacity(out_schema.len());
    match &atom.overlays {
        Some(ov) => {
            let vals = ov.values();
            for i in 0..existing_len {
                slots.push(vals.get(i).and_then(Clone::clone));
            }
        }
        None => slots.resize(existing_len, None),
    }

    for pc in prepared.iter_mut() {
        let input = match pc.ind.input_type() {
            DynType::Snapshot => DynValue::Snapshot(snap.clone()),
            DynType::Atom => DynValue::Atom(atom.clone()),
            // A candle-rooted overlay over an overlay-only series has
            // nothing to read; hand it the atom so the column warms up as
            // absent rather than fabricating a bar.
            DynType::Candle => match atom.candle {
                Some(candle) => DynValue::Candle(candle),
                None => DynValue::Atom(atom.clone()),
            },
            // Overlay roots are Snapshot/Atom/Candle; anything else (a
            // scalar-input carrier) gets the atom as a best effort.
            _ => DynValue::Atom(atom.clone()),
        };
        let produced = pc.ind.update(input).and_then(|dv| to_overlay_value(pc.ty, dv));
        slots.push(produced);
    }

    Atom {
        candle: atom.candle,
        time: atom.time,
        overlays: Some(OverlayInfo::sparse(out_schema.clone(), slots)),
    }
}

fn to_overlay_value(ty: OverlayType, dv: DynValue) -> Option<OverlayValue> {
    match (ty, dv) {
        (OverlayType::Real, DynValue::Real(x)) => Some(OverlayValue::Real(x)),
        (OverlayType::Bool, DynValue::Bool(b)) => Some(OverlayValue::Bool(b)),
        (OverlayType::Str, DynValue::Str(s)) => Some(OverlayValue::Str(s)),
        _ => None,
    }
}

/// Build a [`PreparedColumn`] set from pre-built indicators, returning it
/// alongside the output schema — the Python-carrier entry point. Thin alias for
/// [`prepare_built`] kept for symmetry with [`prepare`].
pub fn prepare_from_indicators(
    existing: &Arc<Schema>,
    named: Vec<(String, Box<dyn DynIndicator>)>,
) -> Result<(Arc<Schema>, Vec<PreparedColumn>)> {
    prepare_built(existing, named)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::market::{Candle, OverlayValue};

    fn candle(close: Real) -> Candle {
        Candle::new(close, close, close, close, 1_000.0)
    }

    // Bring `Real` into scope for the helper above.
    use crate::types::Real;

    fn bare_atoms(closes: &[Real]) -> Vec<Atom> {
        closes.iter().map(|&c| Atom::new(candle(c))).collect()
    }

    fn cols(yaml: &str) -> Vec<OverlayColumn> {
        columns_from_yaml(yaml, &HashMap::new(), std::path::Path::new("."), "(test)").unwrap()
    }

    #[test]
    fn columns_from_value_preserves_order_and_rejects_empty_name() {
        let c = cols("a: !sma { period: 2 }\nz: !ema { period: 3 }\n");
        assert_eq!(c.iter().map(|c| c.name.as_str()).collect::<Vec<_>>(), ["a", "z"]);

        let err = columns_from_value(
            serde_json::json!({ "": 1 }),
            "(test)",
        )
        .unwrap_err();
        assert!(format!("{err:#}").contains("empty column name"));
    }

    #[test]
    fn columns_from_yaml_resolves_params() {
        let params = HashMap::from([("P".to_string(), serde_json::json!(4))]);
        let c = columns_from_yaml(
            "r: !sma { period: !param P }",
            &params,
            std::path::Path::new("."),
            "(test)",
        )
        .unwrap();
        assert!(matches!(&c[0].spec, NodeSpec::Sma { period, .. } if period.get() == 4));
    }

    #[test]
    fn prepare_infers_types_and_appends_after_existing() {
        // Seed an existing schema with one Real column.
        let mut b = Schema::builder();
        b.add_real("vol");
        let existing = b.finish();

        // NodeSpec is value-producing: Real (`!sma`) and Str (`!value bull`);
        // Bool overlays come from the pre-built carrier path (see below).
        let c = cols("sma3: !sma { period: 3 }\nlabel: !value bull\n");
        let (out_schema, prepared) = prepare(&existing, &c).unwrap();

        // Existing column keeps index 0; new columns are appended after it.
        // (Column order among *new* columns follows JSON-map iteration, which is
        // alphabetical here — indexes stay stable, `get` resolves by name.)
        assert_eq!(out_schema.index_of("vol"), Some(0));
        assert!(out_schema.index_of("sma3").unwrap() >= 1);
        assert!(out_schema.index_of("label").unwrap() >= 1);
        assert_eq!(out_schema.type_of_key("sma3"), Some(OverlayType::Real));
        assert_eq!(out_schema.type_of_key("label"), Some(OverlayType::Str));
        assert_eq!(prepared.len(), 2);
    }

    #[test]
    fn prepare_built_infers_bool_and_computes() {
        use crate::indicators::ValueBool;
        use crate::runtime::wrap;

        let ind = wrap(ValueBool::<Atom>::new(true));
        let (out_schema, mut prepared) =
            prepare_built(&Schema::empty(), vec![("flag".to_string(), ind)]).unwrap();
        assert_eq!(out_schema.type_of_key("flag"), Some(OverlayType::Bool));

        let atoms = bare_atoms(&[1.0, 2.0]);
        let aug = compute_series(None, &atoms, &out_schema, 0, &mut prepared);
        let i = out_schema.index_of("flag").unwrap();
        assert_eq!(aug[0].overlays.as_ref().unwrap().get_bool(i), Some(true));
        assert_eq!(aug[1].overlays.as_ref().unwrap().get_bool(i), Some(true));
    }

    #[test]
    fn prepare_rejects_name_colliding_with_existing() {
        let mut b = Schema::builder();
        b.add_real("vol");
        let existing = b.finish();
        let c = cols("vol: !sma { period: 3 }");
        let err = match prepare(&existing, &c) {
            Ok(_) => panic!("expected a collision error"),
            Err(e) => e,
        };
        assert!(format!("{err:#}").contains("collides"));
    }

    #[test]
    fn compute_series_sma_reads_none_during_warmup_then_the_value() {
        let atoms = bare_atoms(&[10.0, 20.0, 30.0, 40.0]);
        let empty = Schema::empty();
        let c = cols("sma3: !sma { period: 3 }");
        let (out_schema, mut prepared) = prepare(&empty, &c).unwrap();
        let augmented = compute_series(None, &atoms, &out_schema, 0, &mut prepared);

        let idx = out_schema.index_of("sma3").unwrap();
        // First two bars warming → None.
        assert_eq!(augmented[0].overlays.as_ref().unwrap().get_real(idx), None);
        assert_eq!(augmented[1].overlays.as_ref().unwrap().get_real(idx), None);
        // Third bar: SMA(10,20,30) = 20.
        assert_eq!(augmented[2].overlays.as_ref().unwrap().get_real(idx), Some(20.0));
        // Fourth bar: SMA(20,30,40) = 30.
        assert_eq!(augmented[3].overlays.as_ref().unwrap().get_real(idx), Some(30.0));
    }

    #[test]
    fn compute_series_merges_and_binds_one_schema_arc() {
        // Input atoms carry a pre-existing `vol` overlay bound to `existing`.
        let mut b = Schema::builder();
        b.add_real("vol");
        let existing = b.finish();
        let closes = [10.0, 20.0, 30.0, 40.0];
        let atoms: Vec<Atom> = closes
            .iter()
            .enumerate()
            .map(|(i, &c)| {
                let ov = OverlayInfo::new(existing.clone(), vec![OverlayValue::Real(i as Real)]);
                Atom::with_overlays(candle(c), ov)
            })
            .collect();

        let c = cols("sma3: !sma { period: 3 }");
        let (out_schema, mut prepared) = prepare(&existing, &c).unwrap();
        let augmented = compute_series(Some("BTC"), &atoms, &out_schema, existing.len(), &mut prepared);

        let vol_i = out_schema.index_of("vol").unwrap();
        let sma_i = out_schema.index_of("sma3").unwrap();
        for (i, a) in augmented.iter().enumerate() {
            let ov = a.overlays.as_ref().unwrap();
            // Every augmented atom is bound to the one out_schema Arc.
            assert!(Arc::ptr_eq(ov.schema(), &out_schema));
            // Pre-existing column carried verbatim on every bar (incl. warm-up).
            assert_eq!(ov.get_real(vol_i), Some(i as Real));
        }
        // New column: None until warm, then the SMA.
        assert_eq!(augmented[0].overlays.as_ref().unwrap().get_real(sma_i), None);
        assert_eq!(augmented[2].overlays.as_ref().unwrap().get_real(sma_i), Some(20.0));
    }

    #[test]
    fn compute_series_round_trips_through_get_real() {
        use crate::indicators::GetReal;
        use crate::Indicator;

        let atoms = bare_atoms(&[10.0, 20.0, 30.0, 40.0]);
        let c = cols("sma3: !sma { period: 3 }");
        let (out_schema, mut prepared) = prepare(&Schema::empty(), &c).unwrap();
        let augmented = compute_series(None, &atoms, &out_schema, 0, &mut prepared);

        // A GetReal resolved against the returned schema reads the values back.
        let mut get = GetReal::new(&out_schema, "sma3");
        let read: Vec<Option<Real>> = augmented.iter().map(|a| get.update(a.clone())).collect();
        assert_eq!(read, vec![None, None, Some(20.0), Some(30.0)]);
    }
}
