//! Type-directed *undefined-value* deserialization for `fugazi check`.
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
//! Three things deserialize as undefined, all through the same machinery: an
//! unset required `!param`, an `!arg` the driver has not bound yet, and an
//! author-written `!undefined`. They differ only in what is reported about
//! them afterwards — see [`UndefinedOrigin`].
//!
//! It *is* visible to serde: at each leaf, the derived `Deserialize` calls the
//! matching `deserialize_*` method (`deserialize_u64` for a `usize`,
//! `deserialize_str` for a `String`, …). `UndefinedDeserializer` wraps the value
//! tree and, at a hole node, answers whichever method serde asks for with a
//! type-appropriate placeholder (`1` / `0.0` / `""` / `false`). No guessing, no
//! search — the type is dictated by the caller. Integers answer `1` rather than
//! `0` so a `NonZeroUsize` period field still parses.
//!
//! ## Wiring
//!
//! `check` marks an undefined value with a [`sentinel`] node instead
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
/// spec node uses this key, so [`is_undefined`] can recognize it unambiguously; the
/// value carries the original `!param` key (for diagnostics, not parsing).
pub const UNSET_PARAM_KEY: &str = "__fugazi_param_hole__";

/// The `!arg` twin of [`UNSET_PARAM_KEY`]. Kept distinct so the type observations
/// below can report on `!param` placeholders — the ones a user supplies from
/// `--params` and might get wrong — without mixing in `!arg`s, which a driver
/// supplies and a user never writes a value for.
pub const UNSET_ARG_KEY: &str = "__fugazi_arg_hole__";

/// The `!undefined` twin of [`UNSET_PARAM_KEY`]. Distinct so the report can say
/// *where* an author-written hole is (its document path) rather than naming it
/// like a `--params` key the user is expected to recognise.
pub const UNDEFINED_KEY: &str = "__fugazi_undefined_hole__";

/// Build the sentinel [`Json`] node standing in for an unresolved required
/// `!param` with the given key.
pub fn sentinel(param_key: &str) -> Json {
    let mut map = Map::with_capacity(1);
    map.insert(UNSET_PARAM_KEY.to_string(), Json::String(param_key.to_string()));
    Json::Object(map)
}

/// The `!undefined` twin of [`sentinel`], carrying the hole's document path.
pub fn undefined_sentinel(path: &str) -> Json {
    let mut map = Map::with_capacity(1);
    map.insert(
        UNDEFINED_KEY.to_string(),
        Json::String(path.to_string()),
    );
    Json::Object(map)
}

/// The `!arg` twin of [`sentinel`], for
/// [`args::substitute_for_check`](crate::spec::args::substitute_for_check).
pub fn arg_sentinel(arg_key: &str) -> Json {
    let mut map = Map::with_capacity(1);
    map.insert(UNSET_ARG_KEY.to_string(), Json::String(arg_key.to_string()));
    Json::Object(map)
}

/// Is this buffered YAML node either hole sentinel?
pub fn is_undefined(value: &Yaml) -> bool {
    undefined_name(value).is_some()
}

/// The `(reserved-key, placeholder-name)` of a hole node, if it is one.
fn undefined_parts(value: &Yaml) -> Option<(&str, &str)> {
    let Yaml::Mapping(m) = value else {
        return None;
    };
    if m.len() != 1 {
        return None;
    }
    for key in [UNSET_PARAM_KEY, UNSET_ARG_KEY, UNDEFINED_KEY] {
        if let Some(Yaml::String(name)) = m.get(Yaml::String(key.to_string())) {
            return Some((key, name.as_str()));
        }
    }
    None
}

fn undefined_name(value: &Yaml) -> Option<&str> {
    undefined_parts(value).map(|(_, name)| name)
}

/// Where a user-facing hole came from — a named `--params` placeholder, or an
/// author-written `!undefined` located by its document path.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum UndefinedOrigin {
    /// An unset required `!param`; the key is its name.
    Param,
    /// An `!undefined`; the key is its path in the document.
    Undefined,
}

/// The user-facing identity of a hole node — `None` for a non-hole *and* for an
/// `!arg` hole, which a driver supplies rather than a user.
fn user_hole(value: &Yaml) -> Option<(UndefinedOrigin, &str)> {
    match undefined_parts(value) {
        Some((UNSET_PARAM_KEY, name)) => Some((UndefinedOrigin::Param, name)),
        Some((UNDEFINED_KEY, path)) => Some((UndefinedOrigin::Undefined, path)),
        _ => None,
    }
}

/// The coarse type a placeholder is required to have, inferred from which
/// `deserialize_*` method serde asked for at the hole.
///
/// Deliberately coarse: `u8` and `f64` are both `Number` because a user writes
/// `--params N=20` either way. What matters is catching a genuine
/// contradiction — the same `!param` used where a number is required *and*
/// where a string is.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum RequiredType {
    Bool,
    Number,
    Str,
    List,
    Table,
}

impl RequiredType {
    pub fn label(self) -> &'static str {
        match self {
            RequiredType::Bool => "bool",
            RequiredType::Number => "number",
            RequiredType::Str => "string",
            RequiredType::List => "list",
            RequiredType::Table => "table",
        }
    }
}

thread_local! {
    /// Every `(param name, required type)` a hole answered during the current
    /// check parse, in encounter order. Drained by
    /// [`take_observations`].
    static PARAM_USES: std::cell::RefCell<Vec<(UndefinedOrigin, String, RequiredType)>> =
        const { std::cell::RefCell::new(Vec::new()) };
}

fn observe(value: &Yaml, ty: RequiredType) {
    if let Some((origin, name)) = user_hole(value) {
        let entry = (origin, name.to_string(), ty);
        PARAM_USES.with(|u| u.borrow_mut().push(entry));
    }
}

/// Drain the recorded placeholder type observations, collapsed to one entry per
/// `!param` name with the distinct types it was required to have (sorted).
///
/// A name mapping to more than one type is a genuine contradiction: no single
/// `--params` value can satisfy both positions, so the document can never run
/// whatever the user supplies.
pub fn take_observations() -> Vec<(UndefinedOrigin, String, Vec<RequiredType>)> {
    let raw = PARAM_USES.with(|u| std::mem::take(&mut *u.borrow_mut()));
    let mut by_name: std::collections::BTreeMap<(UndefinedOrigin, String), Vec<RequiredType>> =
        std::collections::BTreeMap::new();
    for (origin, name, ty) in raw {
        let seen = by_name.entry((origin, name)).or_default();
        if !seen.contains(&ty) {
            seen.push(ty);
        }
    }
    for types in by_name.values_mut() {
        types.sort();
    }
    by_name
        .into_iter()
        .map(|((origin, name), types)| (origin, name, types))
        .collect()
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

/// Whether a [`check_mode`] guard is currently held on this thread.
///
/// Read by [`from_value`] to pick the hole-aware path, and by
/// [`parse_probe`] to decide whether the observations a probe parse records
/// belong to a `check` report or to nobody.
pub fn in_check_mode() -> bool {
    CHECK_MODE.with(Cell::get)
}

/// Hole-aware typed parse of a **probe** copy of a subtree, whether or not a
/// `check` run is in flight. The parsed value is discarded; only the verdict
/// matters.
///
/// [`SpecTemplate`](crate::spec::SpecTemplate)'s `Deserialize` calls this on a
/// copy of its deferred body with every `!arg` marked as a hole, so a typo
/// inside a basket's `score:` or a multi-asset side's `enter:` is a parse error
/// at *load*, exactly as it is for the eagerly-parsed single/pairs slots.
///
/// ## An error that names a hole is not a verdict
///
/// A hole is a wildcard for every serde-driven leaf — it answers whichever
/// `deserialize_*` method its position calls. The hand-rolled parses can't be
/// asked that: `!value`'s [`TryFrom`] reads a raw `serde_norway::Value` with no
/// type demand to answer, so a hole reaches it as the sentinel mapping and it
/// reports "expected a number, a bool, a string or a list". That is a statement
/// about the placeholder, not about the document, so it is **not** reported —
/// the probe returns `Ok`, the same skip rule [`crate::spec::typecheck`] applies
/// to what it cannot decide. Sentinel keys are reserved and can't occur in a
/// real document, so recognizing them in the message is unambiguous.
///
/// The cost of skipping is a template body that goes unvalidated past its first
/// placeholder-shaped failure — `run` still reports it, from the build. The cost
/// of *not* skipping would be refusing to load a document that runs fine.
///
/// ## Observations
///
/// Inside `check` the type observations this parse records are part of the
/// report — a `!param` used inside a template still needs a value — so they are
/// left for [`take_observations`]. Outside `check` nobody drains that ledger,
/// and an API consumer loading specs in a loop would grow it without bound, so a
/// self-entered probe drops what it recorded.
pub fn parse_probe<T: DeserializeOwned>(value: Json) -> Result<(), String> {
    let reporting = in_check_mode();
    let _guard = (!reporting).then(check_mode);
    let outcome = from_json_value::<T>(value)
        .map(|_| ())
        .map_err(|e| e.to_string());
    if !reporting {
        let _ = take_observations();
    }
    match outcome {
        Err(message) if names_a_hole(&message) => Ok(()),
        outcome => outcome,
    }
}

/// Does this parse error mention one of the reserved sentinel keys — i.e. did a
/// placeholder, rather than the document, cause it? See [`parse_probe`].
fn names_a_hole(message: &str) -> bool {
    [UNSET_PARAM_KEY, UNSET_ARG_KEY, UNDEFINED_KEY]
        .iter()
        .any(|key| message.contains(key))
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
        T::deserialize(UndefinedDeserializer(value))
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
struct UndefinedDeserializer(Yaml);

/// Answer scalar leaf methods: a hole visits the zero value for the requested
/// type; anything else delegates to the underlying `serde_norway::Value` (which
/// is a plain scalar, so no children are lost).
macro_rules! scalar_methods {
    ($($method:ident => $visit:ident ( $zero:expr ) as $ty:expr),* $(,)?) => {$(
        fn $method<V: Visitor<'de>>(self, visitor: V) -> Result<V::Value, Self::Error> {
            if is_undefined(&self.0) {
                // Record what type this position demanded before answering, so
                // `check` can report a placeholder's inferred type and catch a
                // name used at two incompatible ones.
                observe(&self.0, $ty);
                visitor.$visit($zero)
            } else {
                self.0.$method(visitor)
            }
        }
    )*};
}

impl<'de> Deserializer<'de> for UndefinedDeserializer {
    type Error = serde_norway::Error;

    // The self-describing path — used when a bridge re-buffers this subtree back
    // into a `serde_norway::Value`. A hole rebuilds faithfully as its sentinel
    // mapping (a plain, representable node), so the *next* inner parse's
    // hole-aware pass still sees and resolves it.
    fn deserialize_any<V: Visitor<'de>>(self, visitor: V) -> Result<V::Value, Self::Error> {
        self.0.deserialize_any(visitor)
    }

    // Integer holes answer `1`, not `0`. The value is arbitrary — a hole is
    // never built, only counted and type-reported — but it has to *parse*, and
    // the period family is `NonZeroUsize`, which rejects 0. `1` satisfies both
    // a plain `usize` and every `NonZero*`, so it's the placeholder that keeps
    // `fugazi check` working across the whole field vocabulary.
    scalar_methods! {
        deserialize_bool => visit_bool(false) as RequiredType::Bool,
        deserialize_i8 => visit_i64(1) as RequiredType::Number,
        deserialize_i16 => visit_i64(1) as RequiredType::Number,
        deserialize_i32 => visit_i64(1) as RequiredType::Number,
        deserialize_i64 => visit_i64(1) as RequiredType::Number,
        deserialize_i128 => visit_i128(1) as RequiredType::Number,
        deserialize_u8 => visit_u64(1) as RequiredType::Number,
        deserialize_u16 => visit_u64(1) as RequiredType::Number,
        deserialize_u32 => visit_u64(1) as RequiredType::Number,
        deserialize_u64 => visit_u64(1) as RequiredType::Number,
        deserialize_u128 => visit_u128(1) as RequiredType::Number,
        deserialize_f32 => visit_f64(0.0) as RequiredType::Number,
        deserialize_f64 => visit_f64(0.0) as RequiredType::Number,
        deserialize_char => visit_char('\0') as RequiredType::Str,
        deserialize_str => visit_str("") as RequiredType::Str,
        deserialize_string => visit_str("") as RequiredType::Str,
        deserialize_bytes => visit_bytes(&[]) as RequiredType::Str,
        deserialize_byte_buf => visit_bytes(&[]) as RequiredType::Str,
    }

    fn deserialize_option<V: Visitor<'de>>(self, visitor: V) -> Result<V::Value, Self::Error> {
        // A hole is a *present* value (the param would have been supplied), so
        // it deserializes as `Some(hole)` and the inner type resolves it.
        match self.0 {
            Yaml::Null => visitor.visit_none(),
            other => visitor.visit_some(UndefinedDeserializer(other)),
        }
    }

    fn deserialize_unit<V: Visitor<'de>>(self, visitor: V) -> Result<V::Value, Self::Error> {
        if is_undefined(&self.0) {
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
        if is_undefined(&self.0) {
            observe(&self.0, RequiredType::List);
            return visitor.visit_seq(UndefinedSeq(Vec::new().into_iter()));
        }
        match self.0 {
            Yaml::Sequence(seq) => visitor.visit_seq(UndefinedSeq(seq.into_iter())),
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
        if is_undefined(&self.0) {
            observe(&self.0, RequiredType::Table);
            return visitor.visit_map(UndefinedMap {
                entries: Vec::new().into_iter(),
                pending_value: None,
            });
        }
        match self.0 {
            Yaml::Mapping(map) => visitor.visit_map(UndefinedMap {
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
            Yaml::String(tag) => visitor.visit_enum(UndefinedEnum {
                tag,
                content: None,
            }),
            // The externally-tagged map form: `{variant: content}`.
            Yaml::Mapping(map) if map.len() == 1 => {
                let (key, content) = map.into_iter().next().expect("len == 1");
                let Yaml::String(tag) = key else {
                    return Err(de::Error::custom("enum variant key must be a string"));
                };
                visitor.visit_enum(UndefinedEnum {
                    tag,
                    content: Some(content),
                })
            }
            // A YAML `!tag value` — strip the `!` and treat as the variant.
            Yaml::Tagged(tagged) => {
                let tag = tagged.tag.to_string();
                let tag = tag.strip_prefix('!').unwrap_or(&tag).to_string();
                visitor.visit_enum(UndefinedEnum {
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
struct UndefinedSeq(std::vec::IntoIter<Yaml>);

impl<'de> SeqAccess<'de> for UndefinedSeq {
    type Error = serde_norway::Error;

    fn next_element_seed<T: DeserializeSeed<'de>>(
        &mut self,
        seed: T,
    ) -> Result<Option<T::Value>, Self::Error> {
        match self.0.next() {
            Some(value) => seed.deserialize(UndefinedDeserializer(value)).map(Some),
            None => Ok(None),
        }
    }
}

/// [`MapAccess`] keeping each value hole-aware (keys are always plain strings).
struct UndefinedMap {
    entries: std::vec::IntoIter<(Yaml, Yaml)>,
    pending_value: Option<Yaml>,
}

impl<'de> MapAccess<'de> for UndefinedMap {
    type Error = serde_norway::Error;

    fn next_key_seed<K: DeserializeSeed<'de>>(
        &mut self,
        seed: K,
    ) -> Result<Option<K::Value>, Self::Error> {
        match self.entries.next() {
            Some((key, value)) => {
                self.pending_value = Some(value);
                seed.deserialize(UndefinedDeserializer(key)).map(Some)
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
        seed.deserialize(UndefinedDeserializer(value))
    }
}

/// [`EnumAccess`] for the variant name plus its (hole-aware) content.
struct UndefinedEnum {
    tag: String,
    content: Option<Yaml>,
}

impl<'de> EnumAccess<'de> for UndefinedEnum {
    type Error = serde_norway::Error;
    type Variant = HoleVariant;

    fn variant_seed<V: DeserializeSeed<'de>>(
        self,
        seed: V,
    ) -> Result<(V::Value, Self::Variant), Self::Error> {
        let tag = seed.deserialize(UndefinedDeserializer(Yaml::String(self.tag)))?;
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
        seed.deserialize(UndefinedDeserializer(self.content()))
    }

    fn tuple_variant<V: Visitor<'de>>(
        self,
        len: usize,
        visitor: V,
    ) -> Result<V::Value, Self::Error> {
        UndefinedDeserializer(self.content()).deserialize_tuple(len, visitor)
    }

    fn struct_variant<V: Visitor<'de>>(
        self,
        fields: &'static [&'static str],
        visitor: V,
    ) -> Result<V::Value, Self::Error> {
        UndefinedDeserializer(self.content()).deserialize_struct("", fields, visitor)
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
        // type it becomes — no value is guessed. Integers answer `1` so that a
        // `NonZeroUsize` field (the whole period family) still parses; the
        // value is inert either way, since a hole is counted, not built.
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
                period: 1,
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
        assert_eq!(nested.inner.period, 1);
        assert_eq!(nested.inner.ratio, 0.0);
        assert_eq!(nested.periods, vec![1, 3, 1]);
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

    #[test]
    fn a_probe_is_hole_aware_without_a_guard_of_its_own() {
        // The load-time entry point: no `check` run in flight, holes still
        // answered. This is what lets a template body be validated at load.
        let json = serde_json::json!({
            "period": arg_sentinel("SYM"),
            "name": "x",
            "flag": false,
            "ratio": 1.0,
        });
        assert!(!in_check_mode());
        parse_probe::<Leaf>(json).expect("a hole must not fail the probe");
        // And the guard is released again, so an ordinary parse afterwards is
        // still strict.
        assert!(!in_check_mode());
    }

    #[test]
    fn a_probe_reports_a_real_shape_error() {
        let json = serde_json::json!({
            "period": 14,
            "name": "x",
            "flag": false,
            // `ratio` misspelled: nothing to do with placeholders, so the
            // probe's whole job is to report it.
            "ration": 1.0,
        });
        let err = parse_probe::<Leaf>(json).expect_err("a missing field is decidable");
        assert!(err.contains("ratio"), "{err}");
    }

    #[test]
    fn a_probe_skips_an_error_that_only_a_hole_could_have_caused() {
        // `!value`'s hand-rolled `TryFrom` reads the raw tree, so a hole
        // reaches it as the sentinel mapping and it reports a type error. The
        // document is fine — `!value !arg CHILD_GROUP` is how a portfolio
        // dispatches weights by group — so the probe must not report it.
        let json = serde_json::json!({"value": arg_sentinel("CHILD_GROUP")});
        parse_probe::<crate::spec::NodeSpec>(json)
            .expect("a placeholder-shaped failure is not a verdict");

        // The same node with a real typo *is* reported.
        let json = serde_json::json!({"valu": 1.0});
        assert!(parse_probe::<crate::spec::NodeSpec>(json).is_err());
    }

    #[test]
    fn a_probe_outside_check_leaves_no_observations() {
        let json = serde_json::json!({
            "period": arg_sentinel("SYM"),
            "name": sentinel("N"),
            "flag": false,
            "ratio": 1.0,
        });
        parse_probe::<Leaf>(json).unwrap();
        assert!(
            take_observations().is_empty(),
            "observations belong to a `check` report, and this was not one",
        );
    }

    #[test]
    fn a_probe_inside_check_keeps_its_observations() {
        // Inside `check` the same parse feeds the report: a `!param` used only
        // inside a template body is still a value the user has to supply.
        let json = serde_json::json!({
            "period": sentinel("PERIOD"),
            "name": "x",
            "flag": false,
            "ratio": 1.0,
        });
        let _guard = check_mode();
        parse_probe::<Leaf>(json).unwrap();
        let seen = take_observations();
        assert_eq!(seen.len(), 1, "{seen:?}");
        assert_eq!(seen[0].1, "PERIOD");
    }
}
