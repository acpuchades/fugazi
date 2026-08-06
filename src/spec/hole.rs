//! Type-directed *hole* deserialization for `fugazi check`.
//!
//! `fugazi check strategy` validates a document's **shape**, not its values —
//! unlike `run`/`optimize`, it never builds or drives the strategy. So a
//! required `!param` with no `--params` value and no `default` doesn't need the
//! user's real value; it just needs *something* of the right type for the typed
//! parse to succeed. But there is no single value that type-checks in every
//! position: a `usize` field (`period`) needs a number, a `String` field
//! (`symbol`) a string, a `bool` field a bool — and the type each hole needs
//! isn't visible on the untyped tree.
//!
//! It *is* visible to serde: at each leaf, the derived `Deserialize` calls the
//! matching `deserialize_*` method (`deserialize_u64` for a `usize`,
//! `deserialize_str` for a `String`, …). [`HoleDeserializer`] wraps the value
//! tree and, at a hole node, answers whichever method serde asks for with a
//! type-appropriate zero (`0` / `""` / `false`). No guessing, no search — the
//! type is dictated by the caller.
//!
//! ## Wiring
//!
//! `check` marks a required-but-unset `!param` with a [`sentinel`] node instead
//! of erroring (see [`crate::spec::params::substitute_for_check`]), then parses
//! under a [`check_mode`] guard. The spec enums re-buffer through
//! `serde_norway::Value` for tag normalization and then parse their inner
//! payload; those inner parses go through [`from_value`], which routes to the
//! hole-aware path **only** while the guard is held. Outside `check` the guard
//! is never set, so `run`/`optimize` take the plain `serde_norway::from_value`
//! path unchanged.

use std::cell::Cell;

use serde::de::{
    self, DeserializeOwned, DeserializeSeed, Deserializer, EnumAccess, MapAccess, SeqAccess,
    VariantAccess, Visitor,
};
use serde_json::{Map, Value as Json};
use serde_norway::Value as Yaml;

/// The reserved single-key object a check-mode hole is represented as. No real
/// spec node uses this key, so [`is_hole`] can recognize it unambiguously; the
/// value carries the original `!param` key (for diagnostics, not parsing).
pub const HOLE_KEY: &str = "__fugazi_param_hole__";

/// Build the sentinel [`Json`] node standing in for an unresolved required
/// `!param` with the given key.
pub fn sentinel(param_key: &str) -> Json {
    let mut map = Map::with_capacity(1);
    map.insert(HOLE_KEY.to_string(), Json::String(param_key.to_string()));
    Json::Object(map)
}

/// Is this buffered YAML node the hole sentinel [`sentinel`] produces?
pub fn is_hole(value: &Yaml) -> bool {
    matches!(value, Yaml::Mapping(m) if m.len() == 1 && m.contains_key(HOLE_KEY))
}

thread_local! {
    /// Set while a `check` parse is in flight (see [`check_mode`]). Read only by
    /// [`from_value`]; unset everywhere else, so non-check parses never touch
    /// the hole-aware path.
    static CHECK_MODE: Cell<bool> = const { Cell::new(false) };
}

/// RAII guard turning on hole-aware deserialization for the current thread.
/// Hold it across a `check` parse; dropping it restores the normal path.
pub struct CheckModeGuard(());

/// Enter check mode for the current thread until the returned guard drops.
pub fn check_mode() -> CheckModeGuard {
    CHECK_MODE.with(|c| c.set(true));
    CheckModeGuard(())
}

impl Drop for CheckModeGuard {
    fn drop(&mut self) {
        CHECK_MODE.with(|c| c.set(false));
    }
}

fn in_check_mode() -> bool {
    CHECK_MODE.with(Cell::get)
}

/// Deserialize `value` into `T`. Inside a [`check_mode`] guard this is
/// hole-aware (a [`sentinel`] node satisfies whatever type serde asks for);
/// otherwise it is exactly `serde_norway::from_value`, so `run`/`optimize` are
/// unaffected.
///
/// The spec enums' `TryFrom<serde_norway::Value>` impls call this for their
/// inner payload parse rather than `serde_norway::from_value` directly.
pub fn from_value<T: DeserializeOwned>(value: Yaml) -> Result<T, serde_norway::Error> {
    if in_check_mode() {
        T::deserialize(HoleDeserializer(value))
    } else {
        serde_norway::from_value(value)
    }
}

/// [`from_value`] from the `serde_json::Value` the substitution passes produce —
/// the top-level entry point `fugazi check` uses. Moves the tree into the
/// `serde_norway::Value` shape the bridges' inner parses buffer through, then
/// deserializes (hole-aware while a [`check_mode`] guard is held).
pub fn from_json_value<T: DeserializeOwned>(value: Json) -> Result<T, serde_norway::Error> {
    from_value(serde_norway::to_value(value)?)
}

/// A [`Deserializer`] over a `serde_norway::Value` that treats a [`sentinel`]
/// node as a wildcard, answering each leaf method with a type-appropriate zero.
/// Compound nodes recurse through wrappers that keep every child hole-aware.
struct HoleDeserializer(Yaml);

/// Answer scalar leaf methods: a hole visits the zero value for the requested
/// type; anything else delegates to the underlying `serde_norway::Value` (which
/// is a plain scalar, so no children are lost).
macro_rules! scalar_methods {
    ($($method:ident => $visit:ident ( $zero:expr )),* $(,)?) => {$(
        fn $method<V: Visitor<'de>>(self, visitor: V) -> Result<V::Value, Self::Error> {
            if is_hole(&self.0) {
                visitor.$visit($zero)
            } else {
                self.0.$method(visitor)
            }
        }
    )*};
}

impl<'de> Deserializer<'de> for HoleDeserializer {
    type Error = serde_norway::Error;

    // The self-describing path — used when a bridge re-buffers this subtree back
    // into a `serde_norway::Value`. A hole rebuilds faithfully as its sentinel
    // mapping (a plain, representable node), so the *next* inner parse's
    // hole-aware pass still sees and resolves it.
    fn deserialize_any<V: Visitor<'de>>(self, visitor: V) -> Result<V::Value, Self::Error> {
        self.0.deserialize_any(visitor)
    }

    scalar_methods! {
        deserialize_bool => visit_bool(false),
        deserialize_i8 => visit_i64(0),
        deserialize_i16 => visit_i64(0),
        deserialize_i32 => visit_i64(0),
        deserialize_i64 => visit_i64(0),
        deserialize_i128 => visit_i128(0),
        deserialize_u8 => visit_u64(0),
        deserialize_u16 => visit_u64(0),
        deserialize_u32 => visit_u64(0),
        deserialize_u64 => visit_u64(0),
        deserialize_u128 => visit_u128(0),
        deserialize_f32 => visit_f64(0.0),
        deserialize_f64 => visit_f64(0.0),
        deserialize_char => visit_char('\0'),
        deserialize_str => visit_str(""),
        deserialize_string => visit_str(""),
        deserialize_bytes => visit_bytes(&[]),
        deserialize_byte_buf => visit_bytes(&[]),
    }

    fn deserialize_option<V: Visitor<'de>>(self, visitor: V) -> Result<V::Value, Self::Error> {
        // A hole is a *present* value (the param would have been supplied), so
        // it deserializes as `Some(hole)` and the inner type resolves it.
        match self.0 {
            Yaml::Null => visitor.visit_none(),
            other => visitor.visit_some(HoleDeserializer(other)),
        }
    }

    fn deserialize_unit<V: Visitor<'de>>(self, visitor: V) -> Result<V::Value, Self::Error> {
        if is_hole(&self.0) {
            visitor.visit_unit()
        } else {
            self.0.deserialize_unit(visitor)
        }
    }

    fn deserialize_unit_struct<V: Visitor<'de>>(
        self,
        _name: &'static str,
        visitor: V,
    ) -> Result<V::Value, Self::Error> {
        self.deserialize_unit(visitor)
    }

    fn deserialize_newtype_struct<V: Visitor<'de>>(
        self,
        _name: &'static str,
        visitor: V,
    ) -> Result<V::Value, Self::Error> {
        visitor.visit_newtype_struct(self)
    }

    fn deserialize_seq<V: Visitor<'de>>(self, visitor: V) -> Result<V::Value, Self::Error> {
        if is_hole(&self.0) {
            return visitor.visit_seq(HoleSeq(Vec::new().into_iter()));
        }
        match self.0 {
            Yaml::Sequence(seq) => visitor.visit_seq(HoleSeq(seq.into_iter())),
            other => other.deserialize_seq(visitor),
        }
    }

    fn deserialize_tuple<V: Visitor<'de>>(
        self,
        _len: usize,
        visitor: V,
    ) -> Result<V::Value, Self::Error> {
        self.deserialize_seq(visitor)
    }

    fn deserialize_tuple_struct<V: Visitor<'de>>(
        self,
        _name: &'static str,
        _len: usize,
        visitor: V,
    ) -> Result<V::Value, Self::Error> {
        self.deserialize_seq(visitor)
    }

    fn deserialize_map<V: Visitor<'de>>(self, visitor: V) -> Result<V::Value, Self::Error> {
        if is_hole(&self.0) {
            return visitor.visit_map(HoleMap {
                entries: Vec::new().into_iter(),
                pending_value: None,
            });
        }
        match self.0 {
            Yaml::Mapping(map) => visitor.visit_map(HoleMap {
                entries: map.into_iter().collect::<Vec<_>>().into_iter(),
                pending_value: None,
            }),
            other => other.deserialize_map(visitor),
        }
    }

    fn deserialize_struct<V: Visitor<'de>>(
        self,
        _name: &'static str,
        _fields: &'static [&'static str],
        visitor: V,
    ) -> Result<V::Value, Self::Error> {
        self.deserialize_map(visitor)
    }

    fn deserialize_enum<V: Visitor<'de>>(
        self,
        name: &'static str,
        variants: &'static [&'static str],
        visitor: V,
    ) -> Result<V::Value, Self::Error> {
        match self.0 {
            // A bare string is a unit variant (`close`, `obv`).
            Yaml::String(tag) => visitor.visit_enum(HoleEnum {
                tag,
                content: None,
            }),
            // The externally-tagged map form: `{variant: content}`.
            Yaml::Mapping(map) if map.len() == 1 => {
                let (key, content) = map.into_iter().next().expect("len == 1");
                let Yaml::String(tag) = key else {
                    return Err(de::Error::custom("enum variant key must be a string"));
                };
                visitor.visit_enum(HoleEnum {
                    tag,
                    content: Some(content),
                })
            }
            // A YAML `!tag value` — strip the `!` and treat as the variant.
            Yaml::Tagged(tagged) => {
                let tag = tagged.tag.to_string();
                let tag = tag.strip_prefix('!').unwrap_or(&tag).to_string();
                visitor.visit_enum(HoleEnum {
                    tag,
                    content: Some(tagged.value),
                })
            }
            // Anything else — let the underlying value report the mismatch.
            other => other.deserialize_enum(name, variants, visitor),
        }
    }

    fn deserialize_identifier<V: Visitor<'de>>(self, visitor: V) -> Result<V::Value, Self::Error> {
        self.0.deserialize_identifier(visitor)
    }

    fn deserialize_ignored_any<V: Visitor<'de>>(self, visitor: V) -> Result<V::Value, Self::Error> {
        self.0.deserialize_ignored_any(visitor)
    }
}

/// [`SeqAccess`] keeping each element hole-aware.
struct HoleSeq(std::vec::IntoIter<Yaml>);

impl<'de> SeqAccess<'de> for HoleSeq {
    type Error = serde_norway::Error;

    fn next_element_seed<T: DeserializeSeed<'de>>(
        &mut self,
        seed: T,
    ) -> Result<Option<T::Value>, Self::Error> {
        match self.0.next() {
            Some(value) => seed.deserialize(HoleDeserializer(value)).map(Some),
            None => Ok(None),
        }
    }
}

/// [`MapAccess`] keeping each value hole-aware (keys are always plain strings).
struct HoleMap {
    entries: std::vec::IntoIter<(Yaml, Yaml)>,
    pending_value: Option<Yaml>,
}

impl<'de> MapAccess<'de> for HoleMap {
    type Error = serde_norway::Error;

    fn next_key_seed<K: DeserializeSeed<'de>>(
        &mut self,
        seed: K,
    ) -> Result<Option<K::Value>, Self::Error> {
        match self.entries.next() {
            Some((key, value)) => {
                self.pending_value = Some(value);
                seed.deserialize(HoleDeserializer(key)).map(Some)
            }
            None => Ok(None),
        }
    }

    fn next_value_seed<V: DeserializeSeed<'de>>(
        &mut self,
        seed: V,
    ) -> Result<V::Value, Self::Error> {
        let value = self
            .pending_value
            .take()
            .expect("next_value_seed called after next_key_seed");
        seed.deserialize(HoleDeserializer(value))
    }
}

/// [`EnumAccess`] for the variant name plus its (hole-aware) content.
struct HoleEnum {
    tag: String,
    content: Option<Yaml>,
}

impl<'de> EnumAccess<'de> for HoleEnum {
    type Error = serde_norway::Error;
    type Variant = HoleVariant;

    fn variant_seed<V: DeserializeSeed<'de>>(
        self,
        seed: V,
    ) -> Result<(V::Value, Self::Variant), Self::Error> {
        let tag = seed.deserialize(HoleDeserializer(Yaml::String(self.tag)))?;
        Ok((tag, HoleVariant(self.content)))
    }
}

struct HoleVariant(Option<Yaml>);

impl HoleVariant {
    fn content(self) -> Yaml {
        self.0.unwrap_or(Yaml::Null)
    }
}

impl<'de> VariantAccess<'de> for HoleVariant {
    type Error = serde_norway::Error;

    fn unit_variant(self) -> Result<(), Self::Error> {
        Ok(())
    }

    fn newtype_variant_seed<T: DeserializeSeed<'de>>(
        self,
        seed: T,
    ) -> Result<T::Value, Self::Error> {
        seed.deserialize(HoleDeserializer(self.content()))
    }

    fn tuple_variant<V: Visitor<'de>>(
        self,
        len: usize,
        visitor: V,
    ) -> Result<V::Value, Self::Error> {
        HoleDeserializer(self.content()).deserialize_tuple(len, visitor)
    }

    fn struct_variant<V: Visitor<'de>>(
        self,
        fields: &'static [&'static str],
        visitor: V,
    ) -> Result<V::Value, Self::Error> {
        HoleDeserializer(self.content()).deserialize_struct("", fields, visitor)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde::Deserialize;

    /// Convert a JSON tree (what substitution produces) into the buffered
    /// `serde_norway::Value` a bridge's inner parse sees.
    fn to_yaml(json: Json) -> Yaml {
        serde_norway::to_value(json).unwrap()
    }

    #[derive(Debug, Deserialize, PartialEq)]
    struct Leaf {
        period: usize,
        name: String,
        flag: bool,
        ratio: f64,
    }

    #[test]
    fn a_hole_satisfies_every_scalar_field_type() {
        // One sentinel per field; each field's deserialize_* method decides the
        // type it becomes — no value is guessed.
        let json = serde_json::json!({
            "period": sentinel("P"),
            "name": sentinel("N"),
            "flag": sentinel("F"),
            "ratio": sentinel("R"),
        });
        let _guard = check_mode();
        let leaf: Leaf = from_value(to_yaml(json)).unwrap();
        assert_eq!(
            leaf,
            Leaf {
                period: 0,
                name: String::new(),
                flag: false,
                ratio: 0.0,
            }
        );
    }

    #[test]
    fn non_hole_values_deserialize_normally_under_the_guard() {
        let json = serde_json::json!({
            "period": 14,
            "name": "sma",
            "flag": true,
            "ratio": 1.5,
        });
        let _guard = check_mode();
        let leaf: Leaf = from_value(to_yaml(json)).unwrap();
        assert_eq!(
            leaf,
            Leaf {
                period: 14,
                name: "sma".to_string(),
                flag: true,
                ratio: 1.5,
            }
        );
    }

    #[test]
    fn holes_inside_nested_and_sequence_positions_resolve() {
        #[derive(Debug, Deserialize, PartialEq)]
        struct Nested {
            inner: Leaf,
            periods: Vec<usize>,
        }
        let json = serde_json::json!({
            "inner": {
                "period": sentinel("P"),
                "name": "x",
                "flag": false,
                "ratio": sentinel("R"),
            },
            "periods": [sentinel("A"), 3, sentinel("B")],
        });
        let _guard = check_mode();
        let nested: Nested = from_value(to_yaml(json)).unwrap();
        assert_eq!(nested.inner.period, 0);
        assert_eq!(nested.inner.ratio, 0.0);
        assert_eq!(nested.periods, vec![0, 3, 0]);
    }

    #[test]
    fn outside_the_guard_the_path_is_plain_serde_norway() {
        // No guard: a sentinel is just an unexpected map where a usize is
        // wanted, so the parse fails exactly as a normal run would.
        let json = serde_json::json!({
            "period": sentinel("P"),
            "name": "x",
            "flag": false,
            "ratio": 1.0,
        });
        assert!(from_value::<Leaf>(to_yaml(json)).is_err());
    }
}
