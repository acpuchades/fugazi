//! Runtime-typed indicator handle for spec-driven builders.
//!
//! The core `fugazi` indicator layer is statically composed: `Ema<S>`,
//! `Gt<L, R>`, `Combine<L, R, Op>` and so on are distinct types parameterised
//! by their sources, and the compiler enforces `Input`/`Output` compatibility
//! when they nest. A YAML- or Python-driven builder, by contrast, learns the
//! shape of the indicator tree only at runtime and needs one common return
//! type it can produce from every match arm and nest into the next.
//!
//! [`PayloadIndicator`] is that common type — a **runtime-typed** trait object
//! carrying its own [`input_type`](PayloadIndicator::input_type) /
//! [`output_type`](PayloadIndicator::output_type) descriptors, exchanging
//! [`PayloadValue`] payloads (`Real | Bool | Atom | Candle | Str | Time |
//! Snapshot`) on every `update`. Concrete library indicators are wrapped once
//! by [`Adapter`] to appear as `PayloadIndicator`s; the [`AsReal`] / [`AsBool`] /
//! [`AsCandle`] / [`AsAtom`] / [`AsStr`] typed views cross back the other way
//! so a boxed handle can drop into a library constructor.
//!
//! Gated behind the `runtime` Cargo feature (default-on; implied by `cli`).
//! A pure-lib user with no YAML/JSON/Python surface doesn't need it and can
//! disable it via `default-features = false`.

pub mod chain;

pub use chain::{
    any, erase, AnyChain, AtomChain, BoolChain, CandleChain, Chain, ChainDomain, DynIndicator,
    Erased, RealChain, StrChain, TimeChain,
};

use std::fmt;
use std::sync::Arc;

use crate::Indicator;
use crate::market::{Atom, Candle, Real};
use crate::snapshot::Snapshot;
use crate::time::Timestamp;
use crate::types::Symbol;

// ---------------------------------------------------------------------------
// Payload enum + type descriptor
// ---------------------------------------------------------------------------

/// The runtime-typed payload a [`PayloadIndicator`] exchanges. One variant per
/// concrete carrier the shared runtime-typed indicator vocabulary produces /
/// consumes.
///
/// `Real`, `Bool` and `Time` are `Copy`; `Atom`, `Candle`, `Str` and
/// `Snapshot` are not, so `PayloadValue` itself is only `Clone`.
///
/// The `Snapshot` variant is keyed by `String` — the symbol space YAML/JSON
/// specs and the Python bindings both produce is `String`-typed end-to-end.
#[derive(Debug, Clone)]
pub enum PayloadValue {
    Real(Real),
    Bool(bool),
    Atom(Atom),
    Candle(Candle),
    Str(Arc<str>),
    Time(Timestamp),
    Snapshot(Snapshot<Symbol>),
}

// `Atom` doesn't implement `PartialEq` (the overlay `Arc`s aren't compared by
// the library), but downstream test helpers still need to assert on
// `PayloadValue`. Compare the scalar variants exactly, reduce `Atom`/`Candle`
// payloads to their candle-field equality (dropping overlays for the atom
// case), and compare `Str` payloads by their string contents. Snapshots are
// compared by their `(sym, freq, atom.candle)` tuples — the same "atoms by
// candle-fields" reduction as the standalone Atom case.
impl PartialEq for PayloadValue {
    fn eq(&self, other: &Self) -> bool {
        match (self, other) {
            (PayloadValue::Real(a), PayloadValue::Real(b)) => a == b,
            (PayloadValue::Bool(a), PayloadValue::Bool(b)) => a == b,
            (PayloadValue::Candle(a), PayloadValue::Candle(b)) => a == b,
            (PayloadValue::Atom(a), PayloadValue::Atom(b)) => a.candle == b.candle,
            (PayloadValue::Str(a), PayloadValue::Str(b)) => a.as_ref() == b.as_ref(),
            (PayloadValue::Time(a), PayloadValue::Time(b)) => a == b,
            (PayloadValue::Snapshot(a), PayloadValue::Snapshot(b)) => {
                a.len() == b.len()
                    && a.iter().zip(b.iter()).all(|((sa, fa, aa), (sb, fb, ab))| {
                        sa == sb && fa == fb && aa.candle == ab.candle
                    })
            }
            _ => false,
        }
    }
}

/// The runtime tag on a [`PayloadValue`] — used to check
/// [`PayloadIndicator::input_type`] / [`output_type`](PayloadIndicator::output_type)
/// compatibility at spec-build time.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PayloadType {
    Real,
    Bool,
    Atom,
    Candle,
    Str,
    Time,
    Snapshot,
}

impl PayloadValue {
    /// The runtime [`PayloadType`] tag of the payload actually carried. The
    /// inverse of the compile-time `<T as TypeOf>::TYPE`; centralising it here
    /// means the [`TryFrom<PayloadValue>`] impls can spell their error arm as one
    /// catch-all instead of listing every non-matching variant.
    pub fn dyn_type(&self) -> PayloadType {
        match self {
            PayloadValue::Real(_) => PayloadType::Real,
            PayloadValue::Bool(_) => PayloadType::Bool,
            PayloadValue::Atom(_) => PayloadType::Atom,
            PayloadValue::Candle(_) => PayloadType::Candle,
            PayloadValue::Str(_) => PayloadType::Str,
            PayloadValue::Time(_) => PayloadType::Time,
            PayloadValue::Snapshot(_) => PayloadType::Snapshot,
        }
    }
}

impl fmt::Display for PayloadType {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            PayloadType::Real => f.write_str("Real"),
            PayloadType::Bool => f.write_str("Bool"),
            PayloadType::Atom => f.write_str("Atom"),
            PayloadType::Candle => f.write_str("Candle"),
            PayloadType::Str => f.write_str("Str"),
            PayloadType::Time => f.write_str("Time"),
            PayloadType::Snapshot => f.write_str("Snapshot"),
        }
    }
}

/// Report whether a [`PayloadValue`] tagged `from` can be consumed by a
/// [`PayloadIndicator`] with `input_type() == into`. Returns `true` when the tags
/// match exactly, or when a well-defined [`TryFrom<PayloadValue>`] lift bridges
/// them (`Candle → Atom`, `Atom → Snapshot`, `Candle → Snapshot`).
///
/// **Single source of truth for coercion compatibility.** Both this table
/// *and* the corresponding lift arms on the `TryFrom<PayloadValue>` impls (for
/// `Atom` and `Snapshot<Symbol>`) list the same three lifts, and a lift-parity
/// test in this module holds them in sync — adding a new lift on either side
/// without the other fails that test.
///
/// A probing implementation (build a sentinel `PayloadValue` of `from`'s variant
/// and check whether the appropriate `TryFrom` returns `Ok`) would be more
/// self-consistent, but that would require default constructors for `Atom`
/// and `Candle` that don't exist and shouldn't be added just for this.
pub fn can_lift(from: PayloadType, into: PayloadType) -> bool {
    from == into
        || matches!(
            (from, into),
            (PayloadType::Candle, PayloadType::Atom)
                | (PayloadType::Atom, PayloadType::Snapshot)
                | (PayloadType::Candle, PayloadType::Snapshot)
        )
}

impl From<Real> for PayloadValue {
    fn from(v: Real) -> Self {
        PayloadValue::Real(v)
    }
}
impl From<bool> for PayloadValue {
    fn from(v: bool) -> Self {
        PayloadValue::Bool(v)
    }
}
impl From<Atom> for PayloadValue {
    fn from(v: Atom) -> Self {
        PayloadValue::Atom(v)
    }
}
impl From<Candle> for PayloadValue {
    fn from(v: Candle) -> Self {
        PayloadValue::Candle(v)
    }
}
impl From<Arc<str>> for PayloadValue {
    fn from(v: Arc<str>) -> Self {
        PayloadValue::Str(v)
    }
}
impl From<Timestamp> for PayloadValue {
    fn from(v: Timestamp) -> Self {
        PayloadValue::Time(v)
    }
}
impl From<Snapshot<Symbol>> for PayloadValue {
    fn from(v: Snapshot<Symbol>) -> Self {
        PayloadValue::Snapshot(v)
    }
}

impl TryFrom<PayloadValue> for Real {
    type Error = PayloadType;
    fn try_from(v: PayloadValue) -> Result<Real, PayloadType> {
        match v {
            PayloadValue::Real(x) => Ok(x),
            other => Err(other.dyn_type()),
        }
    }
}
impl TryFrom<PayloadValue> for bool {
    type Error = PayloadType;
    fn try_from(v: PayloadValue) -> Result<bool, PayloadType> {
        match v {
            PayloadValue::Bool(x) => Ok(x),
            other => Err(other.dyn_type()),
        }
    }
}
impl TryFrom<PayloadValue> for Atom {
    type Error = PayloadType;
    fn try_from(v: PayloadValue) -> Result<Atom, PayloadType> {
        match v {
            PayloadValue::Atom(x) => Ok(x),
            // A raw Candle lifts trivially into an Atom with no overlays —
            // this is the key that lets a Resample's Candle output feed a
            // downstream Atom-input source without an explicit lift adapter.
            PayloadValue::Candle(c) => Ok(c.into()),
            other => Err(other.dyn_type()),
        }
    }
}
impl TryFrom<PayloadValue> for Candle {
    type Error = PayloadType;
    fn try_from(v: PayloadValue) -> Result<Candle, PayloadType> {
        match v {
            PayloadValue::Candle(x) => Ok(x),
            other => Err(other.dyn_type()),
        }
    }
}
impl TryFrom<PayloadValue> for Arc<str> {
    type Error = PayloadType;
    fn try_from(v: PayloadValue) -> Result<Arc<str>, PayloadType> {
        match v {
            PayloadValue::Str(s) => Ok(s),
            other => Err(other.dyn_type()),
        }
    }
}
impl TryFrom<PayloadValue> for Timestamp {
    type Error = PayloadType;
    fn try_from(v: PayloadValue) -> Result<Timestamp, PayloadType> {
        match v {
            PayloadValue::Time(t) => Ok(t),
            other => Err(other.dyn_type()),
        }
    }
}
impl TryFrom<PayloadValue> for Snapshot<Symbol> {
    type Error = PayloadType;
    fn try_from(v: PayloadValue) -> Result<Snapshot<Symbol>, PayloadType> {
        match v {
            PayloadValue::Snapshot(s) => Ok(s),
            // A Candle or Atom lifts into an untagged size-1 snapshot — the
            // key that lets a Resample's Candle output (or any Atom-emitting
            // source's output) feed a downstream Snapshot-rooted chain via
            // the sole-atom unpack that empty-selector `!pick` uses.
            PayloadValue::Candle(c) => Ok(Snapshot::<Symbol>::of_atom(c.into())),
            PayloadValue::Atom(a) => Ok(Snapshot::<Symbol>::of_atom(a)),
            other => Err(other.dyn_type()),
        }
    }
}

/// Maps a concrete carrier type (`Real`, `bool`, `Atom`, `Candle`, `Arc<str>`)
/// back to its [`PayloadType`] tag — the compile-time counterpart of the runtime
/// descriptor the [`Adapter`] blanket uses to fill in `input_type()` /
/// `output_type()`.
pub trait TypeOf {
    const TYPE: PayloadType;
}
impl TypeOf for Real {
    const TYPE: PayloadType = PayloadType::Real;
}
impl TypeOf for bool {
    const TYPE: PayloadType = PayloadType::Bool;
}
impl TypeOf for Atom {
    const TYPE: PayloadType = PayloadType::Atom;
}
impl TypeOf for Candle {
    const TYPE: PayloadType = PayloadType::Candle;
}
impl TypeOf for Arc<str> {
    const TYPE: PayloadType = PayloadType::Str;
}
impl TypeOf for Timestamp {
    const TYPE: PayloadType = PayloadType::Time;
}
impl TypeOf for Snapshot<Symbol> {
    const TYPE: PayloadType = PayloadType::Snapshot;
}

// ---------------------------------------------------------------------------
// The runtime-typed trait + boxed handle
// ---------------------------------------------------------------------------

/// A runtime-typed [`Indicator`]-like object exchanging [`PayloadValue`] payloads.
///
/// Any concrete library `Indicator<Input = X, Output = Y>` where `X` /
/// `Y ∈ { Real, bool, Candle, Atom, Arc<str>, Timestamp, Snapshot<Symbol> }`
/// becomes a `PayloadIndicator` via the [`Adapter`] blanket. To feed a
/// `Box<dyn PayloadIndicator>` back into a library constructor use the [`AsReal`] /
/// [`AsBool`] / [`AsCandle`] / [`AsAtom`] / [`AsStr`] typed views. Payload
/// projection at consumer sites is via `TryFrom<PayloadValue>` (the invariant is
/// checked at spec-build time, so the unwrap arm is unreachable).
pub trait PayloadIndicator: Send + Sync {
    fn input_type(&self) -> PayloadType;
    fn output_type(&self) -> PayloadType;
    fn update(&mut self, input: PayloadValue) -> Option<PayloadValue>;
    fn value(&self) -> Option<PayloadValue>;
    fn warm_up_bars(&self) -> usize;
    fn unstable_bars(&self) -> usize;
    fn stable_bars(&self) -> usize {
        self.warm_up_bars()
            .saturating_add(self.unstable_bars())
    }
    fn reset(&mut self);
    /// Serialize this node's mutable state for run resuming — the erased twin of
    /// [`Indicator::save_state`]. No default: each
    /// of the four carriers ([`Adapter`], [`As`], `Chain`, `UnstableWrap`)
    /// supplies it, threading the recursion across the `Indicator`/`PayloadIndicator`
    /// boundary so the whole runtime tree is covered.
    fn save_state(&self) -> serde_json::Value;
    /// Restore state produced by [`save_state`](PayloadIndicator::save_state) on an
    /// identically-constructed tree.
    fn load_state(&mut self, state: &serde_json::Value) -> Result<(), String>;
    /// Deep-clone the box. Threads `Clone` through the trait object the way the
    /// older `CloneableValue` supertrait did — needed because some concrete
    /// indicators internally clone their source (multi-output component
    /// accessors, `Hma`, `crosses_above`), so a `PayloadIndicator` must itself be
    /// clonable to slot into their construction.
    fn dyn_clone(&self) -> Box<dyn PayloadIndicator>;
}

impl Clone for Box<dyn PayloadIndicator> {
    fn clone(&self) -> Box<dyn PayloadIndicator> {
        (**self).dyn_clone()
    }
}

/// [`PayloadIndicator`] plus `Send + Sync` and a `Send + Sync`-preserving deep
/// clone. The base [`PayloadIndicator`] trait deliberately doesn't require these
/// autotraits, so a downstream impl holding thread-bound state is still a valid
/// `PayloadIndicator`. Callers that *do* need autotrait-preserving type erasure
/// (pyo3 pyclasses require `Send + Sync` on every field) reach for this subtrait
/// via [`wrap_sync`] instead of [`wrap`].
///
/// The library's own position- and book-anchored sources
/// ([`PositionField`](crate::indicators::PositionField), `BookField`) used to be
/// the reason for the split — they held `Rc<RefCell<…>>`. Both moved to
/// `Arc<Mutex<…>>` so the whole composition could cross thread boundaries, so
/// every indicator this crate ships now satisfies the subtrait.
///
/// The blanket impl fires for every `T: PayloadIndicator + Clone + Send + Sync +
/// 'static`, so `Adapter<I>` picks it up automatically when `I` is itself
/// `Send + Sync` — which every stateless indicator (`Ema`, `Sma`, `Rsi`,
/// `Combine`, …) is trivially.
pub trait PayloadIndicatorSync: PayloadIndicator + Send + Sync {
    fn dyn_clone_sync(&self) -> Box<dyn PayloadIndicatorSync>;
}

impl<T> PayloadIndicatorSync for T
where
    T: PayloadIndicator + Clone + Send + Sync + 'static,
{
    fn dyn_clone_sync(&self) -> Box<dyn PayloadIndicatorSync> {
        Box::new(self.clone())
    }
}

impl Clone for Box<dyn PayloadIndicatorSync> {
    fn clone(&self) -> Box<dyn PayloadIndicatorSync> {
        (**self).dyn_clone_sync()
    }
}

// ---------------------------------------------------------------------------
// Adapter: concrete Indicator → PayloadIndicator
// ---------------------------------------------------------------------------

/// Wraps a concrete library [`Indicator`] as a [`PayloadIndicator`].
///
/// One blanket impl over every `I: Indicator<Input = X, Output = Y>` where
/// `X: TryFrom<PayloadValue, Error = PayloadType> + TypeOf` and
/// `Y: Into<PayloadValue> + Clone + TypeOf`. `Y` is `Clone` (not `Copy`) because
/// `Atom` carries `Option<OverlayInfo>` and is not `Copy`.
#[derive(Debug, Clone)]
pub struct Adapter<I> {
    inner: I,
}

impl<I> Adapter<I> {
    pub fn new(inner: I) -> Self {
        Self { inner }
    }
}

impl<I, X, Y> PayloadIndicator for Adapter<I>
where
    I: Indicator<Input = X, Output = Y> + Clone + Send + Sync + 'static,
    X: TryFrom<PayloadValue, Error = PayloadType> + TypeOf,
    Y: Into<PayloadValue> + Clone + TypeOf,
{
    fn input_type(&self) -> PayloadType {
        X::TYPE
    }
    fn output_type(&self) -> PayloadType {
        Y::TYPE
    }
    fn update(&mut self, input: PayloadValue) -> Option<PayloadValue> {
        let x = X::try_from(input).unwrap_or_else(|got| {
            panic!(
                "PayloadIndicator input type mismatch: expected {}, got {}",
                X::TYPE,
                got
            )
        });
        self.inner.update(x).map(Into::into)
    }
    fn value(&self) -> Option<PayloadValue> {
        self.inner.value().map(Into::into)
    }
    fn warm_up_bars(&self) -> usize {
        self.inner.warm_up_bars()
    }
    fn unstable_bars(&self) -> usize {
        self.inner.unstable_bars()
    }
    fn reset(&mut self) {
        self.inner.reset();
    }
    fn save_state(&self) -> serde_json::Value {
        self.inner.save_state()
    }
    fn load_state(&mut self, state: &serde_json::Value) -> Result<(), String> {
        self.inner.load_state(state)
    }
    fn dyn_clone(&self) -> Box<dyn PayloadIndicator> {
        Box::new(self.clone())
    }
}

/// Wrap a concrete indicator into a boxed [`PayloadIndicator`].
pub fn wrap<I, X, Y>(inner: I) -> Box<dyn PayloadIndicator>
where
    I: Indicator<Input = X, Output = Y> + Clone + Send + Sync + 'static,
    X: TryFrom<PayloadValue, Error = PayloadType> + TypeOf,
    Y: Into<PayloadValue> + Clone + TypeOf,
{
    Box::new(Adapter::new(inner))
}

/// Wrap a concrete indicator into a boxed [`PayloadIndicatorSync`] — the
/// autotrait-preserving twin of [`wrap`] for callers that need `Send + Sync`
/// (pyo3 pyclasses, thread-crossing state).
pub fn wrap_sync<I, X, Y>(inner: I) -> Box<dyn PayloadIndicatorSync>
where
    I: Indicator<Input = X, Output = Y> + Clone + Send + Sync + 'static,
    X: TryFrom<PayloadValue, Error = PayloadType> + TypeOf,
    Y: Into<PayloadValue> + Clone + TypeOf,
{
    Box::new(Adapter::new(inner))
}

// ---------------------------------------------------------------------------
// chain: runtime-typed composition of two DynIndicators
// ---------------------------------------------------------------------------

/// Compose two [`PayloadIndicator`]s so that `outer`'s output feeds `inner`'s
/// input at runtime. The returned box has `input_type() =
/// outer.input_type()` and `output_type() = inner.output_type()`. `inner`
/// only advances on ticks where `outer` emits `Some`, so a slow `outer` (e.g.
/// a [`Resample`](crate::indicators::Resample) that emits every N base bars)
/// naturally sub-samples the `inner`.
///
/// The composed warm-up and unstable-period are the plain sum of the two —
/// the same arithmetic the library uses when composing statically, in
/// `outer`-emission units for `inner` — so `!stable { signal }` (or any
/// downstream reader of `stable_bars()`) is on the same convention as a
/// pure-library composition and doesn't get base-bar-scaled for free.
///
/// # Panics
/// If `outer.output_type() != inner.input_type()`, at construction. Prefer
/// [`try_chain`] where the types come from a user-authored document and the
/// mismatch should be reported rather than aborted.
pub fn chain(outer: Box<dyn PayloadIndicator>, inner: Box<dyn PayloadIndicator>) -> Box<dyn PayloadIndicator> {
    try_chain(outer, inner).unwrap_or_else(|e| panic!("{e}"))
}

/// The fallible twin of [`chain()`]: reports a type mismatch as an `Err` instead
/// of panicking.
///
/// This is what the spec builders call. A mismatch here is reachable from a
/// user-authored document, so it is an error to render, not an invariant to
/// abort on. The message follows the crate's diagnostic convention — a bare
/// sentence, to which each enclosing `try_build` arm prepends its own `!tag > `
/// breadcrumb (see [`crate::spec::diagnostics`]).
pub fn try_chain(
    outer: Box<dyn PayloadIndicator>,
    inner: Box<dyn PayloadIndicator>,
) -> Result<Box<dyn PayloadIndicator>, String> {
    if !can_lift(outer.output_type(), inner.input_type()) {
        return Err(format!(
            "cannot chain: outer output type ({}) doesn't match inner input type ({})",
            outer.output_type(),
            inner.input_type(),
        ));
    }
    Ok(Box::new(PayloadChain {
        outer,
        inner,
        value: None,
    }))
}

struct PayloadChain {
    outer: Box<dyn PayloadIndicator>,
    inner: Box<dyn PayloadIndicator>,
    value: Option<PayloadValue>,
}

impl PayloadIndicator for PayloadChain {
    fn input_type(&self) -> PayloadType {
        self.outer.input_type()
    }
    fn output_type(&self) -> PayloadType {
        self.inner.output_type()
    }
    fn update(&mut self, x: PayloadValue) -> Option<PayloadValue> {
        self.value = match self.outer.update(x) {
            Some(y) => self.inner.update(y),
            None => None,
        };
        self.value.clone()
    }
    fn value(&self) -> Option<PayloadValue> {
        self.value.clone()
    }
    fn warm_up_bars(&self) -> usize {
        // Plain library-style composition: outer needs its warm-up, then
        // inner needs `inner.warm_up_bars() - 1` more outer-emissions (one
        // coincides with outer's first emit). The unit is outer-samples for
        // outer's part and outer-emissions for inner's part, i.e. the same
        // undifferentiated arithmetic as `Ema::new(Resample.close(), P)` in
        // pure Rust.
        self.outer
            .warm_up_bars()
            .saturating_add(self.inner.warm_up_bars().saturating_sub(1))
    }
    fn unstable_bars(&self) -> usize {
        self.outer
            .unstable_bars()
            .saturating_add(self.inner.unstable_bars())
    }
    fn reset(&mut self) {
        self.outer.reset();
        self.inner.reset();
        self.value = None;
    }
    fn save_state(&self) -> serde_json::Value {
        // The cached `value` is deliberately not serialized: it is recomputed on
        // the next `update` (which the driver always calls before the next
        // `value()` read), so restoring it would only add a `PayloadValue`/`Atom`
        // serde surface for a field that is overwritten before it is read.
        serde_json::json!({
            "outer": self.outer.save_state(),
            "inner": self.inner.save_state(),
        })
    }
    fn load_state(&mut self, state: &serde_json::Value) -> Result<(), String> {
        let obj = state
            .as_object()
            .ok_or_else(|| format!("chain: expected a state object, got {state}"))?;
        self.outer
            .load_state(obj.get("outer").unwrap_or(&serde_json::Value::Null))
            .map_err(|e| format!("outer > {e}"))?;
        self.inner
            .load_state(obj.get("inner").unwrap_or(&serde_json::Value::Null))
            .map_err(|e| format!("inner > {e}"))?;
        self.value = None;
        Ok(())
    }
    fn dyn_clone(&self) -> Box<dyn PayloadIndicator> {
        Box::new(PayloadChain {
            outer: self.outer.dyn_clone(),
            inner: self.inner.dyn_clone(),
            value: self.value.clone(),
        })
    }
}

// ---------------------------------------------------------------------------
// unstable_wrap: runtime-typed passthrough that zeroes unstable_bars()
// (mirrors the library's Unstable)
// ---------------------------------------------------------------------------

/// A [`PayloadIndicator`] wrapper that forwards every method to `inner` *except*
/// [`unstable_bars`](PayloadIndicator::unstable_bars), which it forces to `0` —
/// the runtime twin of [`Unstable`](crate::indicators::Unstable). Use to opt a
/// subtree out of the strategy-readiness wait for its IIR settling tail.
pub fn unstable_wrap(inner: Box<dyn PayloadIndicator>) -> Box<dyn PayloadIndicator> {
    Box::new(UnstableWrap { inner })
}

struct UnstableWrap {
    inner: Box<dyn PayloadIndicator>,
}

impl PayloadIndicator for UnstableWrap {
    fn input_type(&self) -> PayloadType {
        self.inner.input_type()
    }
    fn output_type(&self) -> PayloadType {
        self.inner.output_type()
    }
    fn update(&mut self, x: PayloadValue) -> Option<PayloadValue> {
        self.inner.update(x)
    }
    fn value(&self) -> Option<PayloadValue> {
        self.inner.value()
    }
    fn warm_up_bars(&self) -> usize {
        self.inner.warm_up_bars()
    }
    fn unstable_bars(&self) -> usize {
        0
    }
    fn reset(&mut self) {
        self.inner.reset();
    }
    fn save_state(&self) -> serde_json::Value {
        // Transparent wrapper — its only effect is zeroing `unstable_bars`, so
        // state is entirely the inner's.
        self.inner.save_state()
    }
    fn load_state(&mut self, state: &serde_json::Value) -> Result<(), String> {
        self.inner.load_state(state)
    }
    fn dyn_clone(&self) -> Box<dyn PayloadIndicator> {
        Box::new(UnstableWrap {
            inner: self.inner.dyn_clone(),
        })
    }
}

// ---------------------------------------------------------------------------
// Typed views: reconstitute a Box<dyn PayloadIndicator> as a library-typed
// Indicator<Input=Snapshot<Symbol>, Output=Out> so it can drop into library
// constructors (Ema::new(source, period), IndicatorExt::gt(...),
// SingleAssetStrategy slots). Callers whose whole indicator chain is
// snapshot-rooted — every atom-input leaf is wrapped in a `!pick` on parse,
// so every PayloadIndicator in the tree consumes `Snapshot<Symbol>` — use these.
//
// One generic [`As<Out>`] carrier covers every supported output type; the
// per-type names ([`AsReal`], [`AsBool`], [`AsCandle`], [`AsAtom`], [`AsStr`])
// are type aliases over it.
// ---------------------------------------------------------------------------

/// Views a `Box<dyn PayloadIndicator>` with `output_type == Out::TYPE` as a
/// library-typed `Indicator<Input = Snapshot<Symbol>, Output = Out>` so it
/// drops into any source-wrapping library constructor (Ema, Sma, arithmetic
/// ops, comparisons, `SingleAssetStrategy` slots).
///
/// # Panics
/// [`new`](Self::new) panics if `inner.input_type() != Snapshot` or
/// `inner.output_type() != Out::TYPE`. Prefer [`try_new`](Self::try_new) where
/// the types come from a user-authored document. Once construction has been
/// checked either way, the unwrap arms in `update`/`value` are unreachable.
pub struct As<Out>(Box<dyn PayloadIndicator>, std::marker::PhantomData<fn() -> Out>);

impl<Out: TypeOf> As<Out> {
    pub fn new(inner: Box<dyn PayloadIndicator>) -> Self {
        Self::try_new(inner).unwrap_or_else(|e| panic!("{e}"))
    }

    /// The fallible twin of [`new`](Self::new): reports a type mismatch as an
    /// `Err` instead of panicking.
    ///
    /// This is the check that used to fire as `AsReal`'s `assert_eq!` in the
    /// middle of a run. Returning it lets the spec builders surface "this slot
    /// wanted a Real and got a Str" as a load-time diagnostic with the enclosing
    /// tag trail attached.
    pub fn try_new(inner: Box<dyn PayloadIndicator>) -> Result<Self, String> {
        if inner.input_type() != PayloadType::Snapshot {
            return Err(format!(
                "expected a Snapshot-input expression, got {}-input",
                inner.input_type(),
            ));
        }
        if inner.output_type() != Out::TYPE {
            return Err(format!(
                "expected a {} expression, got {}",
                Out::TYPE,
                inner.output_type(),
            ));
        }
        Ok(Self(inner, std::marker::PhantomData))
    }
}

impl<Out> Clone for As<Out> {
    fn clone(&self) -> Self {
        Self(self.0.clone(), std::marker::PhantomData)
    }
}

impl<Out> Indicator for As<Out>
where
    Out: TypeOf + TryFrom<PayloadValue, Error = PayloadType> + Clone,
{
    type Input = Snapshot<Symbol>;
    type Output = Out;
    fn update(&mut self, snap: Snapshot<Symbol>) -> Option<Out> {
        let payload = self.0.update(PayloadValue::Snapshot(snap))?;
        Some(Out::try_from(payload).unwrap_or_else(|got| {
            unreachable!(
                "As<{}> received {} but was built for {} output",
                Out::TYPE,
                got,
                Out::TYPE,
            )
        }))
    }
    fn value(&self) -> Option<Out> {
        let payload = self.0.value()?;
        Some(Out::try_from(payload).unwrap_or_else(|got| {
            unreachable!(
                "As<{}> held {} but was built for {} output",
                Out::TYPE,
                got,
                Out::TYPE,
            )
        }))
    }
    fn warm_up_bars(&self) -> usize {
        self.0.warm_up_bars()
    }
    fn unstable_bars(&self) -> usize {
        self.0.unstable_bars()
    }
    fn reset(&mut self) {
        self.0.reset();
    }
    fn save_state(&self) -> serde_json::Value {
        // The hop from the `Indicator` side back into the boxed `PayloadIndicator`,
        // so `Ema<As<Real>>` reaches the box under its typed source.
        self.0.save_state()
    }
    fn load_state(&mut self, state: &serde_json::Value) -> Result<(), String> {
        self.0.load_state(state)
    }
}

/// `Real`-output typed view — the shape every source-side library constructor
/// (Ema, Sma, arithmetic ops, comparisons, …) expects once the caller's
/// leaves have been rooted through `Pick`.
pub type AsReal = As<Real>;

/// `bool`-output typed view — i.e. a
/// [`Signal<Snapshot<Symbol>>`](crate::Signal).
pub type AsBool = As<bool>;

/// `Candle`-output typed view — the shape a bar indicator (`Atr`, `Adx`,
/// `Obv`, …) expects as its `source` after the source-generic refactor.
pub type AsCandle = As<Candle>;

/// `Atom`-output typed view — the atom-emitting bridge every source-generic
/// atom-input leaf (`Close::of(source)`, `Year::of(source)`,
/// `Atr::new(CurrentBar::of(source), period)`, …) uses. The typical concrete
/// source is `Pick::<Symbol>::new()` — the empty selector's
/// `Snapshot::sole_atom` unpack — but any snapshot-rooted atom-emitting
/// chain works.
///
/// Not currently constructed by the CLI spec builder — every leaf that would
/// want it (`!close`, `!year`, `!current`, …) already builds itself with
/// `Pick::<Symbol>::new()` baked in, so no intermediate `AsAtom` is
/// needed. Kept for completeness so a future `!pick { symbol, freq }`
/// NodeSpec variant can produce an atom-emitting PayloadIndicator and drop
/// it into a downstream atom-consuming source.
pub type AsAtom = As<Atom>;

/// `Arc<str>`-output typed view — the shape a
/// [`StrEq`](crate::indicators::StrEq) or any other string-consuming
/// combinator expects for its sources.
pub type AsStr = As<Arc<str>>;

#[cfg(test)]
mod tests {
    use super::*;
    use crate::indicators::{Current, Ema, Sma};

    fn bar(v: Real) -> Candle {
        Candle::new(v, v, v, v, 0.0)
    }

    #[test]
    fn payload_conversions_roundtrip() {
        assert_eq!(Real::try_from(PayloadValue::from(1.5_f64)).unwrap(), 1.5);
        assert!(bool::try_from(PayloadValue::from(true)).unwrap());
        let c = Candle::new(1.0, 2.0, 0.5, 1.5, 100.0);
        assert_eq!(Candle::try_from(PayloadValue::from(c)).unwrap(), c);

        // Type mismatch carries the actual variant tag for diagnostics.
        assert_eq!(
            Real::try_from(PayloadValue::from(true)).unwrap_err(),
            PayloadType::Bool
        );
    }

    #[test]
    fn adapter_reports_types_and_forwards_payload() {
        let mut sma = wrap(Sma::new(Current::close(), 3));
        assert_eq!(sma.input_type(), PayloadType::Atom);
        assert_eq!(sma.output_type(), PayloadType::Real);

        assert_eq!(sma.update(PayloadValue::Atom(bar(1.0).into())), None);
        assert_eq!(sma.update(PayloadValue::Atom(bar(2.0).into())), None);
        assert_eq!(
            sma.update(PayloadValue::Atom(bar(3.0).into())),
            Some(PayloadValue::Real(2.0))
        );
    }

    #[test]
    fn unstable_wrap_zeroes_unstable_but_forwards_output() {
        let raw = Ema::new(Current::close(), 3);
        let warm = raw.warm_up_bars();
        let settle = raw.unstable_bars();
        assert!(settle > 0, "Ema-3 should have a real unstable tail");

        let mut wrapped = unstable_wrap(wrap(Ema::new(Current::close(), 3)));
        let mut plain = wrap(Ema::new(Current::close(), 3));
        assert_eq!(wrapped.input_type(), PayloadType::Atom);
        assert_eq!(wrapped.output_type(), PayloadType::Real);
        assert_eq!(wrapped.warm_up_bars(), warm);
        assert_eq!(wrapped.unstable_bars(), 0);
        assert_eq!(wrapped.stable_bars(), warm);

        let bar = |v: Real| PayloadValue::Atom(Candle::new(v, v, v, v, 0.0).into());
        for i in 1..=5 {
            assert_eq!(wrapped.update(bar(i as Real)), plain.update(bar(i as Real)));
        }
    }

    #[test]
    fn stable_bars_defaults_to_warm_up_plus_unstable() {
        let ema = wrap(Ema::new(Current::close(), 3));
        assert_eq!(
            ema.stable_bars(),
            ema.warm_up_bars() + ema.unstable_bars()
        );
    }

    #[test]
    fn can_lift_matches_try_from_impls() {
        // For every (from, into) pair, verify `can_lift` agrees with what the
        // `TryFrom<PayloadValue>` impls actually accept. `can_lift` is the table
        // `chain()` consults at construction; a drift between the table and the
        // real lift semantics would either accept a chain that panics on the
        // first tick (if `can_lift` said yes but `TryFrom` says no) or refuse a
        // chain that would have worked (if the reverse).
        //
        // The sample values here are the sentinels the `TryFrom` impls actually
        // exercise. `Snapshot`, `Str`, `Time` are self-only, `Real`/`Bool` are
        // self-only, `Candle` lifts to `Atom` and `Snapshot`, `Atom` lifts to
        // `Snapshot`.
        let sample = |t: PayloadType| -> PayloadValue {
            match t {
                PayloadType::Real => PayloadValue::Real(1.0),
                PayloadType::Bool => PayloadValue::Bool(true),
                PayloadType::Candle => PayloadValue::Candle(bar(1.0)),
                PayloadType::Atom => PayloadValue::Atom(bar(1.0).into()),
                PayloadType::Str => PayloadValue::Str(Arc::from("x")),
                PayloadType::Time => PayloadValue::Time(Timestamp(0)),
                PayloadType::Snapshot => PayloadValue::Snapshot(crate::snapshot::Snapshot::new()),
            }
        };
        let try_into_ok = |v: PayloadValue, into: PayloadType| -> bool {
            match into {
                PayloadType::Real => Real::try_from(v).is_ok(),
                PayloadType::Bool => bool::try_from(v).is_ok(),
                PayloadType::Atom => Atom::try_from(v).is_ok(),
                PayloadType::Candle => Candle::try_from(v).is_ok(),
                PayloadType::Str => Arc::<str>::try_from(v).is_ok(),
                PayloadType::Time => Timestamp::try_from(v).is_ok(),
                PayloadType::Snapshot => crate::snapshot::Snapshot::<Symbol>::try_from(v).is_ok(),
            }
        };
        let all = [
            PayloadType::Real,
            PayloadType::Bool,
            PayloadType::Candle,
            PayloadType::Atom,
            PayloadType::Str,
            PayloadType::Time,
            PayloadType::Snapshot,
        ];
        for from in all {
            for into in all {
                let expected = try_into_ok(sample(from), into);
                assert_eq!(
                    can_lift(from, into),
                    expected,
                    "can_lift({from}, {into}) drift: TryFrom says {expected}",
                );
            }
        }
    }

    #[test]
    fn wrap_sync_yields_send_sync_handle_and_clones_deeply() {
        fn assert_send_sync<T: Send + Sync>(_: &T) {}
        let mut sma = wrap_sync(Sma::new(Current::close(), 2));
        assert_send_sync(&sma);
        // Clone survives with autotraits preserved (this is what pyo3 needs).
        let mut clone = sma.clone();
        assert_send_sync(&clone);

        // Both boxes advance independently after the clone.
        assert_eq!(sma.update(PayloadValue::Atom(bar(1.0).into())), None);
        assert_eq!(clone.update(PayloadValue::Atom(bar(10.0).into())), None);
        assert_eq!(
            sma.update(PayloadValue::Atom(bar(3.0).into())),
            Some(PayloadValue::Real(2.0))
        );
        assert_eq!(
            clone.update(PayloadValue::Atom(bar(20.0).into())),
            Some(PayloadValue::Real(15.0))
        );
    }

    #[test]
    fn save_restore_continues_identically_through_the_carriers() {
        // Exercises the alternating recursion end to end: a Chain over an
        // Adapter<Ema> — Chain::save_state → Adapter::save_state →
        // Ema::save_state (derive) → EmaState serde. Feed half a stream, snapshot
        // via the erased seam, rebuild an identical tree, restore, and verify the
        // tail matches an uninterrupted run.
        use crate::indicators::{Ema, Identity};
        let build = || -> Box<dyn PayloadIndicator> {
            chain(
                wrap(Identity::<Real>::new()),
                wrap(Ema::new(Identity::<Real>::new(), 3)),
            )
        };
        let mut paused = build();
        let mut whole = build();
        let feed = |d: &mut Box<dyn PayloadIndicator>, x: Real| d.update(PayloadValue::Real(x));
        for x in [10.0, 12.0, 11.0, 13.0] {
            feed(&mut paused, x);
            feed(&mut whole, x);
        }
        let saved = paused.save_state();
        let mut restored = build();
        restored.load_state(&saved).unwrap();
        for x in [14.0, 9.0, 15.0, 8.0] {
            assert_eq!(feed(&mut restored, x), feed(&mut whole, x));
        }
    }

    #[test]
    fn str_payload_roundtrips_through_dynvalue() {
        let s: Arc<str> = Arc::from("bull");
        let v: PayloadValue = s.clone().into();
        assert_eq!(v, PayloadValue::Str(Arc::from("bull")));
        let back: Arc<str> = v.try_into().unwrap();
        assert_eq!(back.as_ref(), "bull");
        // Mismatch surfaces the actual variant tag.
        assert_eq!(
            Arc::<str>::try_from(PayloadValue::from(1.0_f64)).unwrap_err(),
            PayloadType::Real,
        );
    }
}
