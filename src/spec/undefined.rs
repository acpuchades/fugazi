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
//! unset required `!param`, an `!slot` the driver has not bound yet, and an
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

/// The `!slot` twin of [`UNSET_PARAM_KEY`]. Kept distinct so the type observations
/// below can report on `!param` placeholders — the ones a user supplies from
/// `--params` and might get wrong — without mixing in `!slot`s, which a driver
/// supplies and a user never writes a value for.
pub const UNSET_SLOT_KEY: &str = "__fugazi_slot_hole__";

/// The `!undefined` twin of [`UNSET_PARAM_KEY`]. Distinct so the report can say
/// *where* an author-written hole is (its document path) rather than naming it
/// like a `--params` key the user is expected to recognise.
pub const UNDEFINED_KEY: &str = "__fugazi_undefined_hole__";

/// Build the sentinel [`Json`] node standing in for an unresolved required
/// `!param` with the given key.
pub fn sentinel(param_key: &str) -> Json {
    let mut map = Map::with_capacity(1);
    map.insert(
        UNSET_PARAM_KEY.to_string(),
        Json::String(param_key.to_string()),
    );
    Json::Object(map)
}

/// The `!undefined` twin of [`sentinel`], carrying the hole's document path.
pub fn undefined_sentinel(path: &str) -> Json {
    let mut map = Map::with_capacity(1);
    map.insert(UNDEFINED_KEY.to_string(), Json::String(path.to_string()));
    Json::Object(map)
}

/// The `!slot` twin of [`sentinel`], for
/// [`slots::substitute_for_check`](crate::spec::slots::substitute_for_check).
pub fn slot_sentinel(slot_key: &str) -> Json {
    let mut map = Map::with_capacity(1);
    map.insert(
        UNSET_SLOT_KEY.to_string(),
        Json::String(slot_key.to_string()),
    );
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
    for key in [UNSET_PARAM_KEY, UNSET_SLOT_KEY, UNDEFINED_KEY] {
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
/// `!slot` hole, which a driver supplies rather than a user.
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
    /// An **asset name** — a `String` slot that names a series, not arbitrary
    /// text. A [refinement](Self::refines) of [`Str`](Self::Str): every symbol
    /// is a string, so the two never contradict, but a placeholder demanded as
    /// a symbol is a ticker and a caller can be told so.
    ///
    /// Carried by the `SymbolName` newtype, not asserted alongside the parse —
    /// the field's *type* is what says it, which is also what makes
    /// `spec_grammar()` report `symbol` where it used to report `str`.
    Symbol,
    /// A **bar cadence token** — the `N<unit>` alphabet `--frequency` uses
    /// (`1m` / `4h` / `1d`). The other refinement of [`Str`](Self::Str), and
    /// the reason one was not enough: `!pick`'s `symbol:` and `freq:` are both
    /// `String` slots, sit side by side in the default `root:`, and want
    /// opposite things from a caller.
    ///
    /// Distinct from `!pick`'s `stream:`, which promises nothing and stays a
    /// plain [`Str`](Self::Str) — the same format contract the two spellings
    /// already had, now visible in the type.
    Frequency,
    List,
    Table,
    /// A whole *expression* — the hole stands in for a spec node (or a `!value`
    /// literal), which is parsed by a hand-rolled `TryFrom` rather than by
    /// serde, so no `deserialize_*` call named a type here.
    ///
    /// Any scalar a user can pass is a valid expression (a bare `20` is a
    /// constant source), so this one never *contradicts* another observation of
    /// the same name — see `reject_contradictory_params`.
    Expr,
}

/// The `deserialize_newtype_struct` name each refined-`String` spec field
/// carries, and the demand it stands for.
///
/// A newtype is how serde lets a `String` field say *which kind* of string it
/// is: `#[derive(Deserialize)] struct SymbolName(String)` calls
/// `deserialize_newtype_struct("SymbolName", …)`, and the name reaches the hole
/// deserializer as an ordinary part of the protocol. No side table, no
/// annotation — the field's type is the declaration, and the same type is what
/// the grammar derive reads to report `symbol` instead of `str`.
const NEWTYPE_DEMANDS: &[(&str, RequiredType)] = &[
    ("SymbolName", RequiredType::Symbol),
    ("FreqToken", RequiredType::Frequency),
];

/// What a hole in a refined-string slot answers with.
///
/// The same rule integers follow — they answer `1` rather than `0` so a
/// `NonZeroUsize` period still parses — applied to the refinements: a
/// placeholder has to satisfy the format its slot promises, or the very
/// documents `check` exists to validate fail on the stand-in rather than on
/// anything they wrote. A `""` for a `FreqToken` reached `resolve_stream` at
/// build and came back `invalid frequency ""`.
///
/// Neither value is ever run — a hole is counted and type-reported, and a
/// document holding one is not driven — so these only have to *parse*.
fn refined_placeholder(ty: RequiredType) -> &'static str {
    match ty {
        // Non-empty, and unmistakable in a message if one ever escapes.
        RequiredType::Symbol => "__fugazi_hole__",
        // A canonical token, so `resolve_stream` both accepts it and gets
        // something to canonicalize.
        RequiredType::Frequency => "1d",
        _ => "",
    }
}

/// The demand a newtype-struct name stands for, if it is one of the refined
/// string types. `None` for every other newtype, which deserializes unchanged.
fn newtype_demand(name: &str) -> Option<RequiredType> {
    NEWTYPE_DEMANDS
        .iter()
        .find(|(n, _)| *n == name)
        .map(|(_, ty)| *ty)
}

impl RequiredType {
    /// The coarser type this one is a **refinement** of, if any.
    ///
    /// A refinement is satisfied by a subset of what it refines, so the two can
    /// never contradict: a `!param` demanded as a `Symbol` in one place and a
    /// `Str` in another wants one value — a ticker — and that value is a
    /// string. [`HoleTypes::demanded`] collapses the pair to the finer of the
    /// two, which is the binding constraint and the one to show a caller.
    ///
    /// Two *different* refinements of the same type do contradict: `Symbol` and
    /// `Frequency` are both strings, but no string is both a ticker and a bar
    /// cadence.
    pub fn refines(self) -> Option<RequiredType> {
        match self {
            RequiredType::Symbol | RequiredType::Frequency => Some(RequiredType::Str),
            _ => None,
        }
    }

    /// Can one value satisfy both demands?
    ///
    /// True when they are equal, or when one [refines](Self::refines) the
    /// other. That second arm is what keeps a declaration and a refined slot
    /// from fighting: `type: string` on a `!pick { symbol: }` is a *coarser*
    /// claim than the slot's `Symbol`, not a conflicting one, and a document
    /// that has said `type: string` there since before the refined types
    /// existed must go on loading.
    pub fn compatible_with(self, other: RequiredType) -> bool {
        self == other || self.refines() == Some(other) || other.refines() == Some(self)
    }

    pub fn label(self) -> &'static str {
        match self {
            RequiredType::Bool => "bool",
            RequiredType::Number => "number",
            RequiredType::Str => "string",
            RequiredType::Symbol => "symbol",
            RequiredType::Frequency => "frequency",
            RequiredType::List => "list",
            RequiredType::Table => "table",
            RequiredType::Expr => "expression",
        }
    }
}

thread_local! {
    /// Every `(param name, required type)` a hole answered during the current
    /// check parse, in encounter order. Drained by
    /// [`take_observations`].
    static PARAM_USES: std::cell::RefCell<Vec<(UndefinedOrigin, String, RequiredType)>> =
        const { std::cell::RefCell::new(Vec::new()) };

    /// Every `type:` a hole's placeholder *declared*, recorded by the
    /// substitution pass rather than by the parse. Drained by the same
    /// [`take_observations`] call, so the two ledgers can never be read out of
    /// step. A name declared twice keeps the first declaration — the second is
    /// caught as a redeclaration by the substitution pass, which is the layer
    /// that can point at the offending node.
    static PARAM_DECLS: std::cell::RefCell<
        std::collections::BTreeMap<(UndefinedOrigin, String), crate::spec::ParamType>,
    > = const { std::cell::RefCell::new(std::collections::BTreeMap::new()) };
}

/// The placeholder name of a hole node, whatever its origin — the `!slot` one
/// included, which the *report* leaves out (a driver supplies it) but which a parse
/// still has to stand in for.
pub fn hole_name(value: &Yaml) -> Option<&str> {
    undefined_name(value)
}

/// Record a [`RequiredType`] for a hole a **hand-rolled** parse swallowed.
///
/// The derived path observes as it answers `deserialize_*`; the
/// `TryFrom<serde_norway::Value>` parses ([`NodeSpec`] and [`ValueLit`]) are
/// handed a raw tree with no type demand to answer, so they have to say so
/// themselves — the same gap [`observe_json`] fills for [`RootSpec`], and for
/// the same reason: a hole nobody observes is a `--params` value the report
/// never asks for.
///
/// [`NodeSpec`]: crate::spec::NodeSpec
/// [`ValueLit`]: crate::spec::expr::ValueLit
/// [`RootSpec`]: crate::spec::root::RootSpec
pub fn observe_hole(value: &Yaml, ty: RequiredType) {
    observe(value, ty);
}

fn observe(value: &Yaml, ty: RequiredType) {
    if let Some((origin, name)) = user_hole(value) {
        let entry = (origin, name.to_string(), ty);
        PARAM_USES.with(|u| u.borrow_mut().push(entry));
    }
}

/// Record the `type:` a placeholder declared, so a `check` report can print it
/// and a contradiction against it can be refused.
///
/// Called by the substitution passes ([`params::substitute_for_check`]) at the
/// moment they decide a placeholder stays a hole — a *resolved* placeholder has
/// a real value, and its declaration has already done its work by coercing it.
///
/// [`params::substitute_for_check`]: crate::spec::params::substitute_for_check
pub fn declare(origin: UndefinedOrigin, name: &str, ty: crate::spec::ParamType) {
    PARAM_DECLS.with(|d| {
        d.borrow_mut()
            .entry((origin, name.to_string()))
            .or_insert(ty);
    });
}

/// One placeholder's type story, as `fugazi check` sees it: what the author
/// declared (if anything) and what the document's positions demanded.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HoleTypes {
    /// Whether this is a named `--params` placeholder or an `!undefined`
    /// located by its document path.
    pub origin: UndefinedOrigin,
    /// The `--params` key, or the document path for an `!undefined`.
    pub name: String,
    /// The placeholder's own `type:`, when it carried one.
    pub declared: Option<crate::spec::ParamType>,
    /// Every distinct type the typed parse required of it, sorted.
    pub used: Vec<RequiredType>,
}

impl HoleTypes {
    /// The types a *position* actually demanded — [`RequiredType::Expr`]
    /// dropped, because it is not a demand: it says the hole stands where a
    /// whole expression goes, and every scalar a user can pass is one.
    pub fn demanded(&self) -> Vec<RequiredType> {
        let kept: Vec<RequiredType> = self
            .used
            .iter()
            .copied()
            .filter(|t| *t != RequiredType::Expr)
            .collect();
        kept.iter()
            .copied()
            // Drop a type some other demand *refines*: a slot typed `Symbol`
            // also answers `deserialize_str` on the way to its inner `String`,
            // so `Str` rides along with every refined demand and would read as
            // a second, contradictory one. The refinement is the binding
            // constraint — a value satisfying it satisfies what it refines.
            .filter(|t| !kept.iter().any(|other| other.refines() == Some(*t)))
            .collect()
    }
}

/// Drain the recorded placeholder types — the declarations the substitution
/// pass logged and the demands the typed parse observed — collapsed to one
/// entry per `!param` name.
///
/// A name mapping to more than one demanded type is a genuine contradiction: no
/// single `--params` value can satisfy both positions, so the document can
/// never run whatever the user supplies. So is a declaration the demands
/// disagree with; the caller
/// ([`reject_contradictory_params`](../../fugazi/cli/index.html)) decides how to
/// say so.
pub fn take_observations() -> Vec<HoleTypes> {
    let raw = PARAM_USES.with(|u| std::mem::take(&mut *u.borrow_mut()));
    let mut declared = PARAM_DECLS.with(|d| std::mem::take(&mut *d.borrow_mut()));
    let mut by_name: std::collections::BTreeMap<(UndefinedOrigin, String), Vec<RequiredType>> =
        std::collections::BTreeMap::new();
    // A declared placeholder is reportable even if nothing demanded a type of
    // it — the declaration *is* what the user has to satisfy.
    for key in declared.keys() {
        by_name.entry(key.clone()).or_default();
    }
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
        .map(|(key, used)| HoleTypes {
            declared: declared.remove(&key),
            origin: key.0,
            name: key.1,
            used,
        })
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
/// copy of its deferred body with every `!slot` marked as a hole, so a typo
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

/// Is `value` **itself** a placeholder sentinel — as opposed to a structure with
/// one somewhere inside it ([`contains_hole`])?
///
/// The distinction is what separates a hole with a slot to be typed from and one
/// without. `root: !param SYM` *is* a hole and no field ever demanded a type of
/// it; `root: !pick { symbol: !param SYM }` is a `!pick` whose `symbol:` slot
/// did. See [`crate::spec::root::RootSpec`]'s `Deserialize`.
pub fn is_hole(value: &Json) -> bool {
    match value {
        Json::Object(map) => {
            map.len() == 1
                && map
                    .keys()
                    .all(|k| [UNSET_PARAM_KEY, UNSET_SLOT_KEY, UNDEFINED_KEY].contains(&k.as_str()))
        }
        _ => false,
    }
}

/// Does `value` hold a placeholder sentinel anywhere inside it?
///
/// The structural twin of `names_a_hole`, for the callers that inspect a tree
/// rather than a parse error: a `check`-mode hole stands in for a value nobody
/// supplied, so a validation that would judge the *shape* of what it stands for
/// has to stand down. See [`crate::spec::root::RootSpec`]'s atom demand.
pub fn contains_hole(value: &Json) -> bool {
    match value {
        Json::Object(map) => {
            map.keys()
                .any(|k| [UNSET_PARAM_KEY, UNSET_SLOT_KEY, UNDEFINED_KEY].contains(&k.as_str()))
                || map.values().any(contains_hole)
        }
        Json::Array(items) => items.iter().any(contains_hole),
        _ => false,
    }
}

/// Record a [`RequiredType`] for every user-visible hole in a **`serde_json`**
/// tree, the way the hole-aware deserializer does for one it deserializes.
///
/// For the one parse that steps outside it: [`RootSpec`] buffers
/// its subtree to a `serde_json::Value` and re-parses it with plain `serde_json`
/// (it has to — it keeps the raw tree for the static analysers, and the two
/// formats' `Value` types both self-describe). A `root: !param SYM` used to
/// record no observation at all: `check` reported no unset placeholder and then
/// failed *building* a document whose only problem was a value nobody passed.
///
/// **Only for a root that is itself the hole.** Once `root:` became a whole
/// expression, a hole *inside* one sits in a field, and `NodeSpec`'s inner
/// payload parse routes back through the hole-aware deserializer — which types
/// it from the field that demanded it. Calling this over the whole tree on top
/// of that adds `Str` to every such hole, and a numeric slot then reads as
/// `number` *and* `string`: a contradiction, on a document that is fine. See
/// [`is_hole`].
///
/// [`RootSpec`]: crate::spec::root::RootSpec
pub fn observe_json(value: &Json, ty: RequiredType) {
    match value {
        Json::Object(map) => {
            for (k, v) in map {
                let origin = match k.as_str() {
                    UNSET_PARAM_KEY => Some(UndefinedOrigin::Param),
                    UNDEFINED_KEY => Some(UndefinedOrigin::Undefined),
                    // `!slot` holes are a driver's to fill, not a user's — the
                    // same exclusion `user_hole` makes.
                    _ => None,
                };
                match (origin, v.as_str()) {
                    (Some(origin), Some(name)) => {
                        PARAM_USES.with(|u| u.borrow_mut().push((origin, name.to_string(), ty)))
                    }
                    _ => observe_json(v, ty),
                }
            }
        }
        Json::Array(items) => items.iter().for_each(|v| observe_json(v, ty)),
        _ => {}
    }
}

/// Does this parse error mention one of the reserved sentinel keys — i.e. did a
/// placeholder, rather than the document, cause it? See [`parse_probe`].
fn names_a_hole(message: &str) -> bool {
    [UNSET_PARAM_KEY, UNSET_SLOT_KEY, UNDEFINED_KEY]
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
        name: &'static str,
        visitor: V,
    ) -> Result<V::Value, Self::Error> {
        // A refined string type (`SymbolName`, `FreqToken`) names itself here.
        // Record the finer demand; the inner `String` still records `Str` when
        // it asks, and `HoleTypes::demanded` collapses the pair — cheaper and
        // less fragile than threading a "don't record" flag down one level.
        if let Some(ty) = newtype_demand(name) {
            observe(&self.0, ty);
            if is_undefined(&self.0) {
                // Answer with a value the refinement accepts, not the generic
                // `""` the inner `String` would otherwise get. Handed on as a
                // *real* string, so nothing records `Str` on top of the finer
                // demand already observed above.
                let filled = Yaml::String(refined_placeholder(ty).to_string());
                return visitor.visit_newtype_struct(UndefinedDeserializer(filled));
            }
        }
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
            Yaml::String(tag) => visitor.visit_enum(UndefinedEnum { tag, content: None }),
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
            "period": slot_sentinel("SYM"),
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
        // document is fine — `!value !slot CHILD_GROUP` is how a portfolio
        // dispatches weights by group — so the probe must not report it.
        let json = serde_json::json!({"value": slot_sentinel("CHILD_GROUP")});
        parse_probe::<crate::spec::NodeSpec>(json)
            .expect("a placeholder-shaped failure is not a verdict");

        // The same node with a real typo *is* reported.
        let json = serde_json::json!({"valu": 1.0});
        assert!(parse_probe::<crate::spec::NodeSpec>(json).is_err());
    }

    #[test]
    fn a_probe_outside_check_leaves_no_observations() {
        let json = serde_json::json!({
            "period": slot_sentinel("SYM"),
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
        assert_eq!(seen[0].name, "PERIOD");
    }
}
